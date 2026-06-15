use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::cmd;

const NASTY_MOUNT_BASE: &str = "/fs";
const FS_STATE_PATH: &str = "/var/lib/nasty/fs-state.json";
const SCRUB_STATE_PATH: &str = "/var/lib/nasty/scrub-state.json";
const MOUNT_STATE_PATH: &str = "/var/lib/nasty/mount-state.json";
const FSCK_STATE_PATH: &str = "/var/lib/nasty/fsck-state.json";
/// Trim the captured scrub output to this many trailing bytes before
/// persisting. Long scrubs print per-shard counters every few seconds —
/// keeping the full transcript would bloat the state file without
/// adding operator value over "what was the final summary".
const SCRUB_OUTPUT_KEEP_BYTES: usize = 8 * 1024;
const KEYS_DIR: &str = "/var/lib/nasty/keys";
const PROC_KEYS_PATH: &str = "/proc/keys";

/// Suffix for a TPM2-sealed copy of the encryption key (#102). Lives
/// alongside the plaintext `.key` in [`KEYS_DIR`]; `read_unlock_key`
/// prefers the sealed copy when both are present and falls back to
/// the plaintext on unseal failure so a cleared TPM or Secure-Boot
/// toggle can't strand the user.
const TPM_SEALED_SUFFIX: &str = "tpm";

/// Parse /proc/keys output and return true if the session keyring (or any
/// keyring visible to this process) holds a `bcachefs:<uuid>` key.
/// `bcachefs unlock -k session` lands the FS encryption key here; the kernel
/// reads it from there at mount time. So "key present" === "FS unlocked",
/// regardless of whether it's currently mounted.
fn proc_keys_has_bcachefs_uuid(contents: &str, uuid: &str) -> bool {
    let needle = format!("bcachefs:{uuid}");
    contents
        .lines()
        .any(|line| line_has_key_description(line, &needle))
}

/// Token-level membership check against a `/proc/keys` line.
///
/// The format on a real running kernel is:
///   `<id-hex> <flags> <uses> <perm> <uid> <gid> <type> <description>: <data>`
///
/// e.g. `1de1938e I--Q--- 1 perm 3f010000 0 0 user bcachefs:<uuid>: 32`
///
/// — the description column ends with `:` followed by a type-specific
/// data column. A naive `tok == needle` fails because the token in the
/// real output is `bcachefs:<uuid>:`, not `bcachefs:<uuid>`. Strip the
/// trailing colon before comparing. (This bug was masked in earlier
/// tests that hand-wrote /proc/keys content without the trailing colon
/// — those tests were wrong about the kernel format.)
fn line_has_key_description(line: &str, needle: &str) -> bool {
    line.split_whitespace().any(|tok| {
        let stripped = tok.strip_suffix(':').unwrap_or(tok);
        stripped == needle
    })
}

async fn is_bcachefs_key_loaded(uuid: &str) -> bool {
    let contents = match tokio::fs::read_to_string(PROC_KEYS_PATH).await {
        Ok(s) => s,
        Err(_) => return false,
    };
    proc_keys_has_bcachefs_uuid(&contents, uuid)
}

/// Parse `/proc/keys` and return the decimal key id of the
/// `bcachefs:<uuid>` key, or `None` if it isn't loaded. The id
/// is what `keyctl unlink <id> @s` expects to revoke the key from
/// the engine's session keyring.
///
/// Pure function — `find_bcachefs_key_id` is the async I/O wrapper.
fn parse_bcachefs_key_id(contents: &str, uuid: &str) -> Option<String> {
    let needle = format!("bcachefs:{uuid}");
    contents.lines().find_map(|line| {
        if !line_has_key_description(line, &needle) {
            return None;
        }
        let id_hex = line.split_whitespace().next()?;
        // /proc/keys writes ids as 8-hex-digit zero-padded; keyctl
        // accepts decimal — stick with decimal so the command line
        // is unambiguous regardless of leading-zero handling.
        u32::from_str_radix(id_hex, 16).ok().map(|n| n.to_string())
    })
}

async fn find_bcachefs_key_id(uuid: &str) -> Option<String> {
    let contents = tokio::fs::read_to_string(PROC_KEYS_PATH).await.ok()?;
    parse_bcachefs_key_id(&contents, uuid)
}

/// Resolve the auto-unlock material for filesystem `name`. Returns
/// the raw passphrase bytes that should be fed to `bcachefs unlock`
/// via stdin.
///
/// Resolution order (#102 — TPM2 sealing):
///   1. `<KEYS_DIR>/<name>.tpm` — TPM-sealed blob, PCR-7 bound. We
///      unseal it and use the result.
///   2. `<KEYS_DIR>/<name>.key` — plaintext fallback. Kept around as
///      the designated recovery path when binding to a TPM, and the
///      only on-disk material for systems without TPM2.
///   3. None — the caller must prompt for a passphrase.
///
/// A sealed blob that fails to unseal (TPM cleared, Secure Boot
/// disabled, blob corrupt) is logged and treated as missing so the
/// `.key` fallback kicks in. We do not surface the unseal error to
/// the caller — a stale `.tpm` shouldn't block a mount that the
/// `.key` would otherwise handle.
async fn read_unlock_key(name: &str) -> Result<Option<Vec<u8>>, FilesystemError> {
    let sealed_path = format!("{KEYS_DIR}/{name}.{TPM_SEALED_SUFFIX}");
    if Path::new(&sealed_path).exists() {
        match unseal_key_file(&sealed_path).await {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(e) => warn!(
                "TPM unseal for '{name}' at {sealed_path} failed ({e}); falling back to plaintext .key"
            ),
        }
    }
    let key_path = format!("{KEYS_DIR}/{name}.key");
    if Path::new(&key_path).exists() {
        let bytes = tokio::fs::read(&key_path).await.map_err(|e| {
            FilesystemError::CommandFailed(format!("read key for '{name}' at {key_path}: {e}"))
        })?;
        return Ok(Some(bytes));
    }
    Ok(None)
}

async fn unseal_key_file(path: &str) -> Result<Vec<u8>, String> {
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read {path}: {e}"))?;
    let blob: nasty_common::tpm::SealedBlob =
        serde_json::from_slice(&data).map_err(|e| format!("parse {path}: {e}"))?;
    nasty_common::tpm::unseal(&blob)
        .await
        .map_err(|e| e.to_string())
}

/// What `bcachefs show-super` told us about a member device — used
/// to decide whether to run `bcachefs unlock` before mounting.
///
/// The decision must be driven by what bcachefs itself says, NOT by
/// the presence of a key file in `KEYS_DIR`. Stale `.key` files do
/// exist in the wild — older NASty install paths wrote them
/// regardless of whether the operator selected encryption (observed
/// live on `.0f.ee` and `10.10.20.100`, March/April installs). If
/// we always treat "file exists ⇒ FS is encrypted" the engine then
/// invokes `bcachefs unlock` against an unencrypted device,
/// bcachefs prints `Error: <dev> is not encrypted`, the mount path
/// bails out, and the filesystem stays unmounted at boot. Storage
/// offline. The whole point of having `S` in NAS.
#[derive(Debug, PartialEq, Eq)]
enum NeedsUnlock {
    /// `show-super` succeeded — either the FS isn't encrypted at
    /// all, or it is encrypted and a key is already loaded in the
    /// keyring (e.g. from an earlier unlock in this boot). Either
    /// way the kernel has what it needs for `bcachefs mount`.
    No,
    /// `show-super` failed with "error reading passphrase" — the FS
    /// is encrypted and no usable key is reachable. Caller should
    /// load one from `KEYS_DIR` (or surface the locked state to the
    /// operator if no key file exists).
    Yes,
    /// `show-super` failed for some unrelated reason (device gone,
    /// permission denied, bcachefs binary missing, …). Don't try
    /// unlock; let `bcachefs mount` produce the canonical error.
    Unknown,
}

/// Pure classifier for `bcachefs show-super` output. Extracted so
/// the encryption-detection logic can be pinned with unit tests
/// — the live wrapper just runs the command and feeds its results
/// through here.
fn classify_show_super(exit_success: bool, stderr: &str) -> NeedsUnlock {
    if exit_success {
        return NeedsUnlock::No;
    }
    // bcachefs prints things like "error reading passphrase" or
    // "Error: reading superblock: error reading passphrase". Match
    // the discriminating phrase rather than full strings so we don't
    // break across bcachefs-tools versions that tweak the prefix.
    if stderr.contains("error reading passphrase") {
        return NeedsUnlock::Yes;
    }
    NeedsUnlock::Unknown
}

/// Ask bcachefs whether this device's superblock is currently
/// readable without an unlock step. Wraps the pure classifier so the
/// rest of the engine has one place to call.
async fn probe_needs_unlock(device: &str) -> NeedsUnlock {
    match cmd::run("bcachefs", &["show-super", device]).await {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            classify_show_super(out.status.success(), &stderr)
        }
        Err(e) => {
            warn!(
                "bcachefs show-super {device} could not spawn ({e}); \
                 skipping unlock probe, letting mount produce the canonical error"
            );
            NeedsUnlock::Unknown
        }
    }
}

/// Pipe `key_bytes` into `bcachefs unlock` via stdin. The trailing
/// newline matches the existing passphrase-stdin form — bcachefs
/// reads up to the first newline as the passphrase.
async fn bcachefs_unlock_with_key(device: &str, key_bytes: &[u8]) -> Result<(), FilesystemError> {
    let mut stdin = Vec::with_capacity(key_bytes.len() + 1);
    stdin.extend_from_slice(key_bytes);
    if !key_bytes.ends_with(b"\n") {
        stdin.push(b'\n');
    }
    cmd::run_ok_stdin("bcachefs", &["unlock", "-k", "session", device], &stdin)
        .await
        .map_err(FilesystemError::CommandFailed)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum FilesystemError {
    #[error("bcachefs command failed: {0}")]
    CommandFailed(String),
    #[error("filesystem not found: {0}")]
    NotFound(String),
    #[error("filesystem already exists: {0}")]
    AlreadyExists(String),
    #[error("device {0} is already in use")]
    DeviceInUse(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("no devices specified")]
    NoDevices,
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Filesystem {
    /// Human-readable filesystem name, derived from the mount point directory.
    pub name: String,
    /// bcachefs filesystem UUID.
    pub uuid: String,
    /// Member devices of the filesystem.
    pub devices: Vec<FilesystemDevice>,
    /// Absolute path where the filesystem is mounted (e.g. `/fs/tank`).
    pub mount_point: Option<String>,
    /// Whether the filesystem is currently mounted.
    pub mounted: bool,
    /// Total usable capacity in bytes.
    pub total_bytes: u64,
    /// Bytes currently in use.
    pub used_bytes: u64,
    /// Bytes available for writing.
    pub available_bytes: u64,
    /// Filesystem-level options read from sysfs or show-super.
    pub options: FilesystemOptions,
    /// Details of the most recent failed mount attempt, surfaced while
    /// the filesystem is not mounted. `None` when it's mounted or has
    /// no recorded failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mount_error: Option<MountFailure>,
}

/// Filesystem-level bcachefs options.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FilesystemOptions {
    /// Foreground (inline) compression algorithm (e.g. `lz4`, `zstd`, `none`).
    pub compression: Option<String>,
    /// Background recompression algorithm applied by the background worker.
    pub background_compression: Option<String>,
    /// Number of replicas for data extents.
    pub data_replicas: Option<u32>,
    /// Number of replicas for metadata (btree) extents.
    pub metadata_replicas: Option<u32>,
    /// Checksum algorithm for data (e.g. `crc32c`, `xxhash`).
    pub data_checksum: Option<String>,
    /// Checksum algorithm for metadata.
    pub metadata_checksum: Option<String>,
    /// Target label for foreground (new) writes.
    pub foreground_target: Option<String>,
    /// Target label for background migration writes.
    pub background_target: Option<String>,
    /// Target label for data promotion (cache tier).
    pub promote_target: Option<String>,
    /// Target label for metadata placement.
    pub metadata_target: Option<String>,
    /// Whether erasure coding (EC) is enabled on the filesystem.
    pub erasure_code: Option<bool>,
    /// Whether the filesystem is encrypted at rest.
    pub encrypted: Option<bool>,
    /// Whether the encrypted filesystem is currently locked (needs unlock before mount).
    pub locked: Option<bool>,
    /// Whether a stored key exists for auto-unlock on boot.
    pub key_stored: Option<bool>,
    /// Action on unrecoverable read errors (`continue`, `ro`, `panic`).
    pub error_action: Option<String>,
    /// Version upgrade behavior at mount: `compatible`, `incompatible`, or `none`.
    pub version_upgrade: Option<String>,
    /// Whether mounted in degraded mode (missing devices).
    pub degraded: Option<bool>,
    /// Whether verbose mount logging is enabled.
    pub verbose: Option<bool>,
    /// Whether fsck runs at mount time.
    pub fsck: Option<bool>,
    /// Whether journal flushing is disabled.
    pub journal_flush_disabled: Option<bool>,
    /// Journal flush delay in microseconds. Higher values batch more journal writes,
    /// improving throughput under sync-heavy workloads (e.g. NFS commits).
    pub journal_flush_delay: Option<u32>,
    /// I/O scheduler for member block devices (e.g. `none`, `mq-deadline`, `kyber`).
    /// `none` is recommended for SSDs; `mq-deadline` is the kernel default.
    pub io_scheduler: Option<String>,
    /// Maximum concurrent background mover IOs.
    pub move_ios_in_flight: Option<u32>,
    /// Maximum bytes in flight for background mover (e.g. `"8.0M"`).
    pub move_bytes_in_flight: Option<String>,
}

/// A device within a filesystem, with its per-device bcachefs configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesystemDevice {
    pub path: String,
    /// Hierarchical label (e.g. "ssd.fast", "hdd.archive").
    /// Used for target-based tiering.
    pub label: Option<String>,
    /// How many replicas a copy on this device counts for.
    /// 0 = cache only, 1 = normal (default), 2 = hardware RAID.
    pub durability: Option<u32>,
    /// Persistent device state: rw, ro, evacuating, spare.
    pub state: Option<String>,
    /// Which data types are allowed on this device (e.g. "journal,btree,user").
    pub data_allowed: Option<String>,
    /// Which data types are currently stored on this device (e.g. "btree,user").
    pub has_data: Option<String>,
    /// Whether TRIM/discard is enabled on this device.
    pub discard: Option<bool>,
    /// bcachefs's own per-member `Rotational` flag from the superblock
    /// (`show-super -f members_v2`). This is what bcachefs uses for its
    /// SSD-vs-HDD optimization decisions — NOT the live hardware type
    /// (that's `BlockDevice.rotational`, derived from sysfs/lsblk). The
    /// two can disagree: bcachefs latches this on first mount and can
    /// get it wrong (an SSD stuck at `Rotational: 1`), so surfacing it
    /// lets the operator spot the mis-latch (#501, upstream
    /// koverstreet/bcachefs-tools#594). Sourced from show-super (not
    /// sysfs) keyed by member index so it means the same persisted thing
    /// whether or not the pool is mounted.
    pub rotational: Option<bool>,
    /// Cumulative read IO errors (since filesystem creation), from
    /// `/sys/fs/bcachefs/<uuid>/dev-N/io_errors`. Only populated while
    /// the filesystem is mounted (sysfs is absent otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_errors: Option<u64>,
    /// Cumulative write IO errors (since filesystem creation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_errors: Option<u64>,
    /// Cumulative checksum errors (since filesystem creation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_errors: Option<u64>,
    /// bcachefs member index (the `Device N` slot). Stable across
    /// reboots and independent of the kernel device name, so it
    /// disambiguates "is this the same member?" when a disk is removed
    /// and re-added — possibly in a different physical slot. From
    /// show-super, so available mounted or not. See #452.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_index: Option<u32>,
    /// Stable per-device bcachefs UUID (distinct from the filesystem
    /// UUID). From `/sys/fs/bcachefs/<fs>/dev-N/uuid`, so populated only
    /// while mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// True when this is a *missing* member: the bcachefs superblock still
    /// lists it (phantom `dev-N` in sysfs) but its block device is gone
    /// (pulled/dead). `path` then carries a synthetic placeholder, not a
    /// real `/dev` node — remove it by `member_index` with force. See #466.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<bool>,
}

/// Specifies a device and its per-device options for filesystem creation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeviceSpec {
    /// Absolute block device path (e.g. `/dev/sda`).
    pub path: String,
    /// Hierarchical label (e.g. "ssd.fast", "hdd.archive").
    pub label: Option<String>,
    /// Durability: 0 = cache, 1 = normal, 2 = hardware RAID.
    pub durability: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateFilesystemRequest {
    /// Name for the new filesystem; becomes the mount point directory under `/fs/`.
    pub name: String,
    /// Devices to include in the filesystem.
    pub devices: Vec<DeviceSpec>,
    /// Number of data replicas (default 1).
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    /// Inline compression algorithm (e.g. `lz4`, `zstd`, `none`).
    pub compression: Option<String>,
    /// Whether to enable encryption at format time.
    pub encryption: Option<bool>,
    /// Passphrase for encryption (required when encryption is true).
    pub passphrase: Option<String>,
    /// Whether to store the key for auto-unlock on boot (default true).
    /// When false, user must enter passphrase via WebUI after every reboot.
    #[serde(default = "default_store_key")]
    pub store_key: Option<bool>,
    /// Whether to seal the stored key with the host's TPM2 immediately
    /// after creation (PCR-7 bound, same shape as `fs.tpm.bind`). Saves
    /// the operator the WebUI "Bind to TPM" round-trip and avoids the
    /// brief window between FS creation and binding when the plaintext
    /// `.key` exists alone on disk. Requires `encryption == true`,
    /// `store_key != false`, and a usable TPM2 on the host — request
    /// is rejected upfront when any are missing.
    pub bind_to_tpm: Option<bool>,
    /// Filesystem-wide label (used as default when no per-device labels set).
    pub label: Option<String>,
    /// Tiering targets set at format time.
    pub foreground_target: Option<String>,
    /// Target label for metadata placement.
    pub metadata_target: Option<String>,
    /// Target label for background migration.
    pub background_target: Option<String>,
    /// Target label for data promotion (cache tier).
    pub promote_target: Option<String>,
    /// Whether to enable erasure coding.
    pub erasure_code: Option<bool>,
    /// Data checksum algorithm (e.g. `crc32c`, `crc64`, `xxhash`, `none`).
    pub data_checksum: Option<String>,
    /// Metadata checksum algorithm.
    pub metadata_checksum: Option<String>,
    /// Bucket size in bytes (e.g. `"512k"`, `"1M"`). Affects allocation granularity.
    pub bucket_size: Option<String>,
    /// Maximum encoded extent size (e.g. `"64k"`, `"128k"`).
    pub encoded_extent_max: Option<String>,
    /// Version upgrade behavior at mount time: `compatible`, `incompatible`, or `none`.
    pub version_upgrade: Option<String>,
    /// Journal flush delay in microseconds (default: 1000). Higher values batch
    /// more journal writes, improving throughput under sync-heavy workloads.
    pub journal_flush_delay: Option<u32>,
    /// I/O scheduler for member block devices (`none`, `mq-deadline`, `kyber`).
    /// `none` is recommended for SSDs.
    pub io_scheduler: Option<String>,
}

fn default_replicas() -> u32 {
    1
}
fn default_store_key() -> Option<bool> {
    Some(true)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DestroyFilesystemRequest {
    /// Name of the filesystem to destroy.
    pub name: String,
    /// Must match `name` exactly — guards against accidental destruction.
    pub confirm_name: String,
}

/// Update runtime-mutable filesystem options on a mounted filesystem.
/// Options are written directly to sysfs (/sys/fs/bcachefs/<uuid>/options/).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateFilesystemOptionsRequest {
    /// Name of the filesystem to update.
    pub name: String,
    /// Inline compression algorithm (e.g. `lz4`, `zstd`, `none`).
    pub compression: Option<String>,
    /// Background recompression algorithm.
    pub background_compression: Option<String>,
    /// Target label for foreground (new) writes.
    pub foreground_target: Option<String>,
    /// Target label for background migration.
    pub background_target: Option<String>,
    /// Target label for data promotion (cache tier).
    pub promote_target: Option<String>,
    /// Target label for metadata placement.
    pub metadata_target: Option<String>,
    /// Action on unrecoverable read errors (`continue`, `ro`, `panic`).
    pub error_action: Option<String>,
    /// Whether to enable erasure coding.
    pub erasure_code: Option<bool>,
    /// Version upgrade behavior at mount time: `compatible`, `incompatible`, or `none`.
    /// Changing mount options requires a remount.
    pub version_upgrade: Option<String>,
    /// Mount in degraded mode (allow mounting with missing devices).
    pub degraded: Option<bool>,
    /// Enable verbose mount logging.
    pub verbose: Option<bool>,
    /// Run fsck at mount time.
    pub fsck: Option<bool>,
    /// Disable journal flushing (unsafe, for benchmarking).
    pub journal_flush_disabled: Option<bool>,
    /// Journal flush delay in microseconds. Higher values batch more journal writes.
    pub journal_flush_delay: Option<u32>,
    /// I/O scheduler for member block devices (`none`, `mq-deadline`, `kyber`).
    pub io_scheduler: Option<String>,
    /// Data checksum algorithm (`none`, `crc32c`, `crc64`, `xxhash`).
    pub data_checksum: Option<String>,
    /// Metadata checksum algorithm (`none`, `crc32c`, `crc64`, `xxhash`).
    pub metadata_checksum: Option<String>,
    /// Number of data replicas.
    pub data_replicas: Option<u32>,
    /// Number of metadata replicas.
    pub metadata_replicas: Option<u32>,
    /// Maximum concurrent background mover IOs.
    pub move_ios_in_flight: Option<u32>,
    /// Maximum bytes in flight for background mover (e.g. `"8.0M"`).
    pub move_bytes_in_flight: Option<String>,
}

/// Add a device to an existing filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceAddRequest {
    /// Name of the filesystem to add the device to.
    pub filesystem: String,
    /// Device to add, with optional label and durability settings.
    pub device: DeviceSpec,
}

/// Remove/evacuate/online/offline a device in a filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceActionRequest {
    /// Name of the filesystem containing the device.
    pub filesystem: String,
    /// The device to act on: an absolute block-device path (e.g. `/dev/sdb`)
    /// or, for a missing/dead member with no current path, its numeric
    /// bcachefs member index.
    pub device: String,
    /// Force removal even when data/metadata can't be migrated off first —
    /// required for a *missing* member (the disk is gone, nothing to
    /// evacuate; safe while enough replicas remain on surviving devices).
    /// Ignored by non-remove actions.
    #[serde(default)]
    pub force: bool,
}

/// Set a label on a device in a filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceSetLabelRequest {
    /// Name of the filesystem containing the device.
    pub filesystem: String,
    /// Absolute path of the block device (e.g. `/dev/sdb`).
    pub device: String,
    /// New hierarchical label (e.g. `ssd.fast`, `hdd.archive`).
    pub label: String,
}

/// Change the persistent state of a device within a filesystem.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeviceSetStateRequest {
    /// Name of the filesystem containing the device.
    pub filesystem: String,
    /// Absolute path of the block device (e.g. `/dev/sdb`).
    pub device: String,
    /// One of: rw, ro, failed, spare
    pub state: String,
}

/// Host + per-FS TPM2 bind state returned from `fs.tpm.status`,
/// `fs.tpm.bind`, and `fs.tpm.unbind`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TpmBindStatus {
    /// Host has a usable TPM 2.0 resource manager (`/dev/tpmrm0`).
    pub tpm_available: bool,
    /// A `<KEYS_DIR>/<name>.tpm` sealed blob exists for this filesystem.
    pub bound: bool,
}

/// Detailed filesystem usage from `bcachefs fs usage`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FsUsage {
    /// Raw output from `bcachefs fs usage`, structured where possible.
    pub raw: String,
    /// Per-device usage breakdown.
    pub devices: Vec<DeviceUsage>,
    /// Total data stored (before replication).
    pub data_bytes: u64,
    /// Total metadata stored.
    pub metadata_bytes: u64,
    /// Reserved bytes.
    pub reserved_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeviceUsage {
    /// Block device path.
    pub path: String,
    /// Bytes currently used on this device.
    pub used_bytes: u64,
    /// Bytes available on this device.
    pub free_bytes: u64,
    /// Total capacity of this device in bytes.
    pub total_bytes: u64,
}

/// Outcome of the most recent completed scrub. Classified from the
/// child process's exit status + a scan of its combined output for
/// `error`-shaped lines (bcachefs reports counter increments inline
/// during the scan). `Failed` is used for non-zero exits *and* for
/// the engine-restart-during-scrub case where we lost track of the
/// running child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrubOutcome {
    /// Exited 0 and no error markers in output.
    Ok,
    /// Exited 0 but the scrub reported one or more errors.
    Errors,
    /// Non-zero exit, spawn failure, or the engine restarted mid-scrub.
    Failed,
}

/// Scrub operation status — both live state ("am I running, since when")
/// and the last-completed-run summary ("when, how long, outcome,
/// captured output"). Persisted across engine restarts via
/// `/var/lib/nasty/scrub-state.json` so the operator's view doesn't
/// reset every time the engine cycles.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScrubStatus {
    /// Whether a scrub is currently in progress.
    pub running: bool,
    /// Unix seconds when the current run started. `Some` while
    /// `running`; cleared on completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// 0-100 progress of the in-flight scrub, parsed from the
    /// most recent `XX%` token in bcachefs's streaming output. Only
    /// populated while `running`; deliberately NOT persisted so an
    /// engine restart while a scrub is in flight doesn't surface
    /// a stale percent from a child that's no longer being read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<f32>,
    /// Unix seconds when the most recent completed scrub finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    /// Duration of the most recent completed scrub, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_secs: Option<u64>,
    /// Outcome of the most recent completed scrub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<ScrubOutcome>,
    /// Captured stdout+stderr from the most recent completed scrub.
    /// Truncated to the last `SCRUB_OUTPUT_KEEP_BYTES` so a chatty
    /// long-running scrub doesn't bloat the state file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
    /// Human-readable summary string — kept for backward compatibility
    /// with the existing Diagnostics tab renderer (which reads `raw`).
    /// New WebUI surfaces should prefer the typed fields above.
    pub raw: String,
}

/// Reconcile (background work) status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReconcileStatus {
    /// Raw text output from the bcachefs reconcile status command.
    pub raw: String,
    /// Whether reconcile is currently enabled on this filesystem.
    pub enabled: bool,
}

/// Outcome of an offline `bcachefs fsck` run. Mirrors [`ScrubOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsckOutcome {
    /// Exited 0 with no error markers — the filesystem is consistent.
    Clean,
    /// Errors were reported (a dry run found problems, or a repair run
    /// couldn't fix everything). The captured output carries detail.
    Errors,
    /// Spawn failure, abnormal exit, or the engine restarted mid-check.
    Failed,
}

/// fsck operation status — live state plus the last-completed-run
/// summary. Persisted to `/var/lib/nasty/fsck-state.json` so the
/// operator's view survives engine restarts, exactly like scrub.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FsckStatus {
    /// Whether an fsck is currently in progress.
    pub running: bool,
    /// Whether the in-flight (or most recent) run was a repair (`-y`)
    /// vs a read-only dry run (`-n`).
    #[serde(default)]
    pub repair: bool,
    /// Unix seconds when the current run started. `Some` while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// 0-100 progress of the in-flight run, when bcachefs emits a
    /// parseable `XX%` token. Not persisted (a restart shouldn't surface
    /// a stale percent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<f32>,
    /// Unix seconds when the most recent completed run finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    /// Duration of the most recent completed run, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_secs: Option<u64>,
    /// Whether the most recent completed run was a repair vs a dry run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_repair: Option<bool>,
    /// Outcome of the most recent completed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<FsckOutcome>,
    /// Captured stdout+stderr from the most recent completed run,
    /// truncated to the last `SCRUB_OUTPUT_KEEP_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
}

/// Why a filesystem's most recent mount attempt failed, classified from
/// the bcachefs mount stderr plus the set of expected-but-absent member
/// devices. Drives the WebUI's mount-failure banner and its suggested
/// next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MountFailureReason {
    /// One or more member devices are absent — the pool can't assemble.
    /// A degraded mount may bring it up if enough replicas remain.
    MissingDevice,
    /// Encrypted and locked; needs an unlock before mounting.
    NeedsUnlock,
    /// bcachefs reported recovery/consistency errors — a check (fsck) is warranted.
    NeedsCheck,
    /// The mount point or a member device is busy / already in use.
    Busy,
    /// Couldn't be classified; the raw stderr carries the detail.
    Unknown,
}

/// An expected member device that wasn't present at mount time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MissingDevice {
    /// The device path NASty expected (from the persisted member list).
    pub path: String,
    /// bcachefs member index, when derivable from show-super.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_index: Option<u32>,
    /// Hierarchical tiering label (e.g. "hdd.archive"), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Record of the most recent failed mount attempt for a filesystem.
/// Persisted to `/var/lib/nasty/mount-state.json` so the WebUI can
/// explain *why* a pool isn't mounted after a boot-time failure instead
/// of just showing "Unmounted". Cleared on the next successful mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MountFailure {
    /// Unix seconds when the failed attempt happened.
    pub attempted_at: i64,
    /// Classified reason, driving the UI's suggested next step.
    pub reason: MountFailureReason,
    /// Short, operator-facing explanation.
    pub message: String,
    /// Expected member devices that were absent at attempt time.
    #[serde(default)]
    pub missing_devices: Vec<MissingDevice>,
    /// Raw bcachefs stderr, kept verbatim for the details expander.
    pub raw: String,
}

/// One member device parsed from `bcachefs show-super -f members_v2`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberInfo {
    index: Option<u32>,
    path: Option<String>,
    label: Option<String>,
}

/// How long a cached `list()` result stays valid.
const FS_LIST_CACHE_TTL: Duration = Duration::from_secs(3);

type ListCache = Arc<Mutex<Option<(Instant, Vec<Filesystem>)>>>;
type ScrubStateMap = Arc<Mutex<HashMap<String, ScrubStatus>>>;
type MountStateMap = Arc<Mutex<HashMap<String, MountFailure>>>;
type FsckStateMap = Arc<Mutex<HashMap<String, FsckStatus>>>;

#[derive(Clone)]
pub struct FilesystemService {
    list_cache: ListCache,
    /// Per-filesystem scrub state, loaded from `SCRUB_STATE_PATH` on
    /// construction. Mutated by `scrub_start` (sets `running` /
    /// `started_at`) and the spawned scrub task (records completion);
    /// read by `scrub_status` and `scrub_status_all`. The mutex is
    /// held only briefly for read/write — the actual `bcachefs scrub`
    /// child runs detached.
    scrub_state: ScrubStateMap,
    /// Per-filesystem record of the most recent *failed* mount attempt,
    /// loaded from `MOUNT_STATE_PATH` on construction. Written by
    /// `mount_with_opts` on failure and cleared on success; read by
    /// `list()` to surface `Filesystem.last_mount_error`.
    mount_state: MountStateMap,
    /// Per-filesystem fsck state, loaded from `FSCK_STATE_PATH`. Same
    /// shape as `scrub_state`: live "running" + last-run summary.
    fsck_state: FsckStateMap,
    /// Device paths with a `bcachefs device evacuate` currently running
    /// (spawned detached — it can take hours). Guards against a second
    /// evacuation of the same device being spawned in the window before
    /// bcachefs persists the `evacuating` state (#479). Not persisted:
    /// an engine restart orphans the child anyway, and the state-based
    /// check in `device_evacuate` covers re-submission after that.
    evacuating: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl Default for FilesystemService {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesystemService {
    pub fn new() -> Self {
        // Best-effort load; a missing or corrupt file means no
        // history is surfaced for previously-run scrubs but doesn't
        // block the engine from accepting new ones.
        let scrub = match std::fs::read_to_string(SCRUB_STATE_PATH) {
            Ok(s) => serde_json::from_str::<HashMap<String, ScrubStatus>>(&s).unwrap_or_else(|e| {
                warn!("parse {SCRUB_STATE_PATH} failed: {e} — starting with empty scrub history");
                HashMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!("read {SCRUB_STATE_PATH} failed: {e} — starting with empty scrub history");
                HashMap::new()
            }
        };
        // Same best-effort load for the last-mount-failure history.
        let mount = match std::fs::read_to_string(MOUNT_STATE_PATH) {
            Ok(s) => {
                serde_json::from_str::<HashMap<String, MountFailure>>(&s).unwrap_or_else(|e| {
                    warn!(
                        "parse {MOUNT_STATE_PATH} failed: {e} — starting with empty mount history"
                    );
                    HashMap::new()
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!("read {MOUNT_STATE_PATH} failed: {e} — starting with empty mount history");
                HashMap::new()
            }
        };
        // ...and for the fsck history.
        let fsck = match std::fs::read_to_string(FSCK_STATE_PATH) {
            Ok(s) => serde_json::from_str::<HashMap<String, FsckStatus>>(&s).unwrap_or_else(|e| {
                warn!("parse {FSCK_STATE_PATH} failed: {e} — starting with empty fsck history");
                HashMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!("read {FSCK_STATE_PATH} failed: {e} — starting with empty fsck history");
                HashMap::new()
            }
        };
        Self {
            list_cache: Arc::new(Mutex::new(None)),
            scrub_state: Arc::new(Mutex::new(scrub)),
            mount_state: Arc::new(Mutex::new(mount)),
            fsck_state: Arc::new(Mutex::new(fsck)),
            evacuating: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Invalidate the cached `list()` result.
    /// Call this after any mutation (create, mount, unmount, destroy, etc.).
    pub async fn invalidate_list_cache(&self) {
        *self.list_cache.lock().await = None;
    }

    /// Record (and persist) the most recent failed mount for `name`.
    async fn record_mount_failure(&self, name: &str, failure: MountFailure) {
        self.mount_state
            .lock()
            .await
            .insert(name.to_string(), failure);
        persist_mount_state(&self.mount_state).await;
        // Drop the cached list so the next fetch surfaces the failure
        // immediately rather than after the 3s TTL.
        self.invalidate_list_cache().await;
    }

    /// Clear any recorded mount failure for `name` (on a successful mount).
    async fn clear_mount_failure(&self, name: &str) {
        if self.mount_state.lock().await.remove(name).is_some() {
            persist_mount_state(&self.mount_state).await;
        }
    }

    /// Mount filesystems that were previously tracked as mounted.
    /// Called at startup to restore filesystem state across reboots.
    /// Restore filesystem mounts from saved state. Returns names of filesystems
    /// that failed to mount.
    pub async fn restore_mounts(&self) -> Vec<String> {
        let state = load_fs_state().await;
        if state.is_empty() {
            info!("No filesystems to restore");
            return vec![];
        }

        // Wait for udev to finish device enumeration before any mount attempts.
        info!("Waiting for block devices to settle...");
        match tokio::process::Command::new("udevadm")
            .args(["settle", "--timeout=30"])
            .status()
            .await
        {
            Ok(s) if s.success() => {}
            Ok(s) => warn!(
                "udevadm settle exited {s} — proceeding to mount with possibly-incomplete device enumeration"
            ),
            Err(e) => warn!(
                "udevadm settle failed to spawn: {e} — proceeding to mount blind, expect mount failures if devices haven't enumerated"
            ),
        }

        let mut failed_names = Vec::new();

        for (name, opts) in &state {
            // Honor the operator's "I unmounted this on purpose"
            // signal. Without this, every boot would auto-mount FSes
            // the operator deliberately took down. Missing field
            // (None) and Some(true) both keep auto-mount on — that's
            // the historical default for entries written before the
            // `mounted` flag existed.
            if opts.mounted == Some(false) {
                info!("Filesystem '{name}' was unmounted by the operator — skipping auto-mount");
                continue;
            }

            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");

            if is_mountpoint(&mount_point).await {
                info!("Filesystem '{name}' already mounted at {mount_point}");
                continue;
            }

            // If we know the expected devices, wait for them to appear
            if !opts.devices.is_empty() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                loop {
                    let missing: Vec<&String> = opts
                        .devices
                        .iter()
                        .filter(|d| !std::path::Path::new(d).exists())
                        .collect();
                    if missing.is_empty() {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        error!(
                            "Filesystem '{name}': devices still missing after 60s: {}",
                            missing
                                .iter()
                                .map(|d| d.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        break;
                    }
                    info!(
                        "Filesystem '{name}': waiting for {} device(s): {}",
                        missing.len(),
                        missing
                            .iter()
                            .map(|d| d.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                // Refresh blkid cache after devices appear
                match tokio::process::Command::new("blkid")
                    .arg("-g")
                    .output()
                    .await
                {
                    Ok(o) if o.status.success() => {}
                    Ok(o) => warn!(
                        "blkid -g cache refresh exited {}: {} — mount probe may use a stale cache",
                        o.status,
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Err(e) => {
                        warn!("blkid -g failed to spawn: {e} — mount probe may use a stale cache")
                    }
                }
            }

            info!("Mounting filesystem '{name}'...");
            match self.mount_with_opts(name, opts).await {
                Ok(_) => info!("Filesystem '{name}' mounted at {mount_point}"),
                Err(e) => {
                    error!("Failed to mount filesystem '{name}': {e}");
                    failed_names.push(name.to_string());
                }
            }
        }
        failed_names
    }

    /// List all bcachefs filesystems (mounted and known via blkid).
    /// Results are cached for up to 3 seconds to avoid redundant subprocess calls.
    pub async fn list(&self) -> Result<Vec<Filesystem>, FilesystemError> {
        {
            let cache = self.list_cache.lock().await;
            if let Some((ts, ref data)) = *cache
                && ts.elapsed() < FS_LIST_CACHE_TTL
            {
                return Ok(data.clone());
            }
        }

        let result = self.list_uncached().await?;

        {
            let mut cache = self.list_cache.lock().await;
            *cache = Some((Instant::now(), result.clone()));
        }

        Ok(result)
    }

    /// Uncached implementation of filesystem listing.
    async fn list_uncached(&self) -> Result<Vec<Filesystem>, FilesystemError> {
        let mounts = read_bcachefs_mounts().await?;

        // A single bcachefs filesystem can have multiple mount points — e.g. kubelet
        // bind-mounts a subvolume under /var/lib/kubelet/... while the canonical
        // mount lives at /fs/<name>. Deduplicate by UUID, preferring the /fs/ mount.
        let mut primary_mount: HashMap<String, String> = HashMap::new();
        for (mount_point, devices) in &mounts {
            let uuid = get_fs_uuid(devices.first().map(|s| s.as_str()).unwrap_or(""))
                .await
                .unwrap_or_default();
            if uuid.is_empty() {
                continue;
            }
            let existing = primary_mount.get(&uuid);
            let is_nasty = mount_point.starts_with(&format!("{NASTY_MOUNT_BASE}/"));
            let existing_is_nasty = existing
                .map(|m| m.starts_with(&format!("{NASTY_MOUNT_BASE}/")))
                .unwrap_or(false);
            if existing.is_none() || (is_nasty && !existing_is_nasty) {
                primary_mount.insert(uuid, mount_point.clone());
            }
        }

        let mut filesystems = Vec::new();
        let mut seen_uuids = std::collections::HashSet::new();

        for (uuid, mount_point) in &primary_mount {
            let devices = match mounts.get(mount_point) {
                Some(d) => d,
                None => continue,
            };

            // None falls through to (0, 0, 0). The cause is logged inside
            // get_mount_usage itself so we don't need to match here.
            let (total, used, available) = get_mount_usage(mount_point).await.unwrap_or((0, 0, 0));

            let name = Path::new(mount_point)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            seen_uuids.insert(uuid.clone());
            let uuid = uuid.clone();

            // Read per-device labels and fs options for mounted filesystems
            let fs_devices = read_fs_devices(&uuid, devices).await;
            let mut options = read_fs_options_sysfs(&uuid).await;
            options.io_scheduler = read_io_scheduler(&fs_devices).await;

            filesystems.push(Filesystem {
                name,
                uuid,
                devices: fs_devices,
                mount_point: Some(mount_point.clone()),
                mounted: true,
                total_bytes: total,
                used_bytes: used,
                available_bytes: available,
                options,
                last_mount_error: None,
            });
        }

        // Discover unmounted bcachefs filesystems via blkid
        let state = load_fs_state().await;
        let unmounted = discover_unmounted_bcachefs(&seen_uuids).await;
        for (uuid, _label, devices) in unmounted {
            // Infer filesystem name from existing mount directory or fs-state.json.
            // Note: blkid's LABEL_SUB is the bcachefs per-device tiering label
            // (e.g. "fast", "slow"), NOT the filesystem name — don't use it.
            let name = find_fs_name_by_uuid(&state, &uuid)
                .or_else(|| find_fs_name_by_devices(&devices))
                .unwrap_or_else(|| uuid[..8].to_string());

            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
            let has_mount_dir = Path::new(&mount_point).is_dir();

            let fs_devices = devices
                .iter()
                .map(|d| FilesystemDevice {
                    path: d.clone(),
                    label: None,
                    durability: None,
                    state: None,
                    data_allowed: None,
                    has_data: None,
                    discard: None,
                    rotational: None,
                    read_errors: None,
                    write_errors: None,
                    checksum_errors: None,
                    member_index: None,
                    uuid: None,
                    missing: None,
                })
                .collect();

            // For unmounted filesystems, try reading options from show-super
            let options = read_fs_options_show_super(devices.first().map(|s| s.as_str())).await;

            filesystems.push(Filesystem {
                name,
                uuid,
                devices: fs_devices,
                mount_point: if has_mount_dir {
                    Some(mount_point)
                } else {
                    None
                },
                mounted: false,
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
                options,
                last_mount_error: None,
            });
        }

        // Overlay persisted mount options onto sysfs options
        let state = load_fs_state().await;
        for fs in &mut filesystems {
            if let Some(opts) = state.get(&fs.name) {
                if fs.options.version_upgrade.is_none() {
                    fs.options.version_upgrade = opts.version_upgrade.clone();
                }
                if fs.options.degraded.is_none() {
                    fs.options.degraded = opts.degraded;
                }
                if fs.options.verbose.is_none() {
                    fs.options.verbose = opts.verbose;
                }
                if fs.options.fsck.is_none() {
                    fs.options.fsck = opts.fsck;
                }
                if fs.options.journal_flush_disabled.is_none() {
                    fs.options.journal_flush_disabled = opts.journal_flush_disabled;
                }

                // Encryption state
                if opts.encrypted == Some(true) {
                    if fs.options.encrypted.is_none() {
                        fs.options.encrypted = Some(true);
                    }
                    let key_path = format!("{KEYS_DIR}/{}.key", fs.name);
                    fs.options.key_stored = Some(Path::new(&key_path).exists());
                    // Locked = encrypted, not mounted, AND no key in the keyring.
                    // After `bcachefs unlock -k session` the key is available
                    // but the FS isn't mounted yet — it's "unlocked, ready to
                    // mount", not "locked".
                    let unlocked_in_keyring = is_bcachefs_key_loaded(&fs.uuid).await;
                    fs.options.locked = Some(!fs.mounted && !unlocked_in_keyring);
                }
            }
        }

        // Attach the most recent failed-mount record to any pool that
        // isn't currently mounted, so the UI can explain *why* it's down
        // rather than just showing "Unmounted" (#451).
        {
            let failures = self.mount_state.lock().await;
            for fs in &mut filesystems {
                if !fs.mounted {
                    fs.last_mount_error = failures.get(&fs.name).cloned();
                }
            }
        }

        Ok(filesystems)
    }

    /// Get a single filesystem by name
    pub async fn get(&self, name: &str) -> Result<Filesystem, FilesystemError> {
        let filesystems = self.list().await?;
        filesystems
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| FilesystemError::NotFound(name.to_string()))
    }

    /// Create a new bcachefs filesystem: format devices, create mount point, mount
    pub async fn create(
        &self,
        mut req: CreateFilesystemRequest,
    ) -> Result<Filesystem, FilesystemError> {
        if req.devices.is_empty() {
            return Err(FilesystemError::NoDevices);
        }

        // Reject a malformed compression spec before formatting — the
        // string is interpolated straight into `bcachefs format
        // --compression=…`, and a bad level there fails the format with
        // an opaque error after we've already started touching disk.
        if let Some(ref comp) = req.compression {
            validate_compression(comp).map_err(FilesystemError::InvalidInput)?;
        }

        // Upfront validation of `bind_to_tpm`. Fails the request before
        // touching disk when prerequisites aren't met, so the operator
        // doesn't end up with a half-baked FS (formatted but never
        // sealed) after picking an inconsistent option set in the
        // WebUI. The post-format tpm_bind call near the bottom of
        // create() can still fail at runtime (e.g. tpm2-tools missing
        // unexpectedly) — in that case the FS exists and the operator
        // can retry via the WebUI's "Bind to TPM" affordance.
        if req.bind_to_tpm == Some(true) {
            if req.encryption != Some(true) {
                return Err(FilesystemError::InvalidInput(
                    "bind_to_tpm requires encryption=true (there's nothing to seal otherwise)"
                        .into(),
                ));
            }
            if req.store_key == Some(false) {
                return Err(FilesystemError::InvalidInput(
                    "bind_to_tpm requires store_key=true (the .key file is the input to the TPM seal)"
                        .into(),
                ));
            }
            if !nasty_common::tpm::is_available().await {
                return Err(FilesystemError::InvalidInput(
                    "bind_to_tpm requested but no TPM2 is available on this host (/dev/tpmrm0 missing)"
                        .into(),
                ));
            }
        }

        // Resolve ":free" virtual devices — create a new partition in free space
        for dev in &mut req.devices {
            if let Some(disk_path) = dev.path.strip_suffix(":free") {
                let new_part = create_partition_on_free_space(disk_path).await?;
                info!("Resolved {}:free -> {}", disk_path, new_part);
                dev.path = new_part;
            }
        }

        // Validate devices exist
        for dev in &req.devices {
            if !Path::new(&dev.path).exists() {
                return Err(FilesystemError::DeviceNotFound(dev.path.clone()));
            }
        }

        // Check devices aren't already in use by a bcachefs filesystem
        for dev in &req.devices {
            if is_device_bcachefs(&dev.path).await {
                return Err(FilesystemError::DeviceInUse(dev.path.clone()));
            }
        }

        // Check mount point doesn't already exist with content
        let mount_point = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        if Path::new(&mount_point).exists() {
            return Err(FilesystemError::AlreadyExists(req.name.clone()));
        }

        // Build bcachefs format command
        // Global options first, then per-device options + device path pairs
        let mut args: Vec<String> = vec!["format".to_string()];

        args.push(format!("--label={}", req.name));

        if req.replicas > 1 {
            args.push(format!("--replicas={}", req.replicas));
        }

        if let Some(ref comp) = req.compression {
            args.push(format!("--compression={comp}"));
        }

        if req.encryption == Some(true) {
            args.push("--encrypted".to_string());
        }

        if let Some(ref t) = req.foreground_target {
            args.push(format!("--foreground_target={t}"));
        }
        if let Some(ref t) = req.metadata_target {
            args.push(format!("--metadata_target={t}"));
        }
        if let Some(ref t) = req.background_target {
            args.push(format!("--background_target={t}"));
        }
        if let Some(ref t) = req.promote_target {
            args.push(format!("--promote_target={t}"));
        }

        if req.erasure_code == Some(true) {
            if req.replicas < 2 {
                return Err(FilesystemError::InvalidInput(
                    "Erasure coding requires replicas >= 2 (data is written as replicas first, then converted to parity stripes)".to_string(),
                ));
            }
            if req.devices.len() < (req.replicas as usize) + 1 {
                return Err(FilesystemError::InvalidInput(format!(
                    "Erasure coding with {} replicas requires at least {} devices (got {})",
                    req.replicas,
                    req.replicas + 1,
                    req.devices.len(),
                )));
            }
            args.push("--erasure_code".to_string());
        }

        if let Some(ref v) = req.data_checksum {
            args.push(format!("--data_checksum={v}"));
        }
        if let Some(ref v) = req.metadata_checksum {
            args.push(format!("--metadata_checksum={v}"));
        }
        if let Some(ref v) = req.bucket_size {
            args.push(format!("--bucket={v}"));
        }
        if let Some(ref v) = req.encoded_extent_max {
            args.push(format!("--encoded_extent_max={v}"));
        }

        // Per-device options go immediately before each device path
        let has_targets = req.foreground_target.is_some()
            || req.metadata_target.is_some()
            || req.background_target.is_some()
            || req.promote_target.is_some();

        for dev in &req.devices {
            // Only add labels when tiering targets are configured or device has an explicit label
            if let Some(ref label) = dev.label {
                args.push(format!("--label={label}"));
            } else if has_targets {
                // Fall back to filesystem-level label or name when targets need labels to route to
                let default_label = req.label.as_deref().unwrap_or(&req.name);
                args.push(format!("--label={default_label}"));
            }

            if let Some(durability) = dev.durability {
                args.push(format!("--durability={durability}"));
            }

            args.push(dev.path.clone());
        }

        // Format
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let dev_paths: Vec<&str> = req.devices.iter().map(|d| d.path.as_str()).collect();
        let is_encrypted = req.encryption == Some(true);
        info!(
            "Formatting bcachefs filesystem '{}' on {:?}{}",
            req.name,
            dev_paths,
            if is_encrypted { " (encrypted)" } else { "" }
        );

        if is_encrypted {
            let passphrase = req.passphrase.as_deref().ok_or_else(|| {
                FilesystemError::CommandFailed(
                    "passphrase required for encrypted filesystem".to_string(),
                )
            })?;
            // bcachefs format --encrypted reads passphrase twice from stdin (passphrase + confirm)
            let stdin = format!("{passphrase}\n{passphrase}\n");
            let output = cmd::run_stdin("bcachefs", &arg_refs, stdin.as_bytes())
                .await
                .map_err(|e| {
                    FilesystemError::CommandFailed(format!("failed to execute bcachefs: {e}"))
                })?;

            if !output.status.success() {
                // bcachefs format writes superblocks then does a trial open that
                // can race with udev, causing EBUSY on exit even though format
                // succeeded.  Check if superblocks were actually written.
                if !is_device_bcachefs(&req.devices[0].path).await {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(FilesystemError::CommandFailed(format!(
                        "bcachefs exited with {}: {stderr}",
                        output.status
                    )));
                }
                warn!(
                    "bcachefs format exited with {} but superblocks are present, continuing",
                    output.status
                );
            }

            // Store key for auto-unlock (default: yes)
            if req.store_key != Some(false) {
                tokio::fs::create_dir_all(KEYS_DIR).await?;
                let key_path = format!("{KEYS_DIR}/{}.key", req.name);
                tokio::fs::write(&key_path, passphrase.as_bytes()).await?;
                info!("Encryption key stored at {key_path}");
            }
        } else {
            let output = cmd::run("bcachefs", &arg_refs).await.map_err(|e| {
                FilesystemError::CommandFailed(format!("failed to execute bcachefs: {e}"))
            })?;

            if !output.status.success() {
                if !is_device_bcachefs(&req.devices[0].path).await {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(FilesystemError::CommandFailed(format!(
                        "bcachefs exited with {}: {stderr}",
                        output.status
                    )));
                }
                warn!(
                    "bcachefs format exited with {} but superblocks are present, continuing",
                    output.status
                );
            }
        }

        // Create mount point
        tokio::fs::create_dir_all(&mount_point).await?;

        let device_arg = req
            .devices
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join(":");

        // Unlock encrypted filesystem before mounting
        if is_encrypted {
            if let Some(bytes) = read_unlock_key(&req.name).await? {
                bcachefs_unlock_with_key(&req.devices[0].path, &bytes).await?;
            } else if let Some(ref passphrase) = req.passphrase {
                let stdin = format!("{passphrase}\n");
                cmd::run_ok_stdin(
                    "bcachefs",
                    &["unlock", "-k", "session", &req.devices[0].path],
                    stdin.as_bytes(),
                )
                .await
                .map_err(FilesystemError::CommandFailed)?;
            }
        }

        // Mount
        let mount_opts = FsMountOptions {
            encrypted: if is_encrypted { Some(true) } else { None },
            version_upgrade: req.version_upgrade.clone(),
            journal_flush_delay: req.journal_flush_delay,
            io_scheduler: req.io_scheduler.clone(),
            ..FsMountOptions::default()
        };
        let mount_opt_str = build_mount_opts(&mount_opts);
        info!(
            "Mounting filesystem '{}' at {} with options: {}",
            req.name, mount_point, mount_opt_str
        );
        cmd::run_ok(
            "bcachefs",
            &["mount", "-o", &mount_opt_str, &device_arg, &mount_point],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        // Apply I/O scheduler to member block devices
        let dev_list: Vec<FilesystemDevice> = req
            .devices
            .iter()
            .map(|d| FilesystemDevice {
                path: d.path.clone(),
                label: d.label.clone(),
                durability: d.durability,
                state: None,
                data_allowed: None,
                has_data: None,
                discard: None,
                rotational: None,
                read_errors: None,
                write_errors: None,
                checksum_errors: None,
                member_index: None,
                uuid: None,
                missing: None,
            })
            .collect();
        if let Some(ref sched) = req.io_scheduler
            && let Err(e) = apply_io_scheduler(&dev_list, sched).await
        {
            warn!("Failed to set I/O scheduler: {e}");
        }

        // Read back the filesystem info
        let uuid = get_fs_uuid(&req.devices[0].path).await.unwrap_or_default();

        // Track mount state with identity info for boot reconciliation
        let mut saved_opts = mount_opts;
        saved_opts.uuid = Some(uuid.clone());
        saved_opts.devices = req.devices.iter().map(|d| d.path.clone()).collect();
        save_fs_mounted_with_opts(&req.name, saved_opts).await;
        // Logged inside get_mount_usage on failure.
        let (total, used, available) = get_mount_usage(&mount_point).await.unwrap_or((0, 0, 0));

        let fs_devices = req
            .devices
            .iter()
            .map(|d| FilesystemDevice {
                path: d.path.clone(),
                label: d.label.clone(),
                durability: d.durability,
                state: Some("rw".to_string()),
                data_allowed: None,
                has_data: None,
                discard: None,
                rotational: None,
                read_errors: None,
                write_errors: None,
                checksum_errors: None,
                member_index: None,
                uuid: None,
                missing: None,
            })
            .collect();

        self.invalidate_list_cache().await;

        // Bind the freshly-stored key to the host TPM2 when the
        // operator asked for it. Prerequisites (encryption,
        // store_key, TPM availability) were verified upfront so a
        // failure here is unexpected — log + return error with a
        // hint to the WebUI's manual Bind affordance rather than
        // rolling back the format. The FS exists on disk with valid
        // data either way; the operator just needs to retry the
        // bind step.
        if req.bind_to_tpm == Some(true) {
            if let Err(e) = self.tpm_bind(&req.name).await {
                warn!(
                    "Filesystem '{}' was created but TPM bind failed: {e}. \
                     The plaintext .key remains on disk; retry via the WebUI's \
                     'Bind to TPM' button on the Filesystems page.",
                    req.name
                );
                return Err(FilesystemError::CommandFailed(format!(
                    "filesystem '{}' created but TPM seal failed: {e}",
                    req.name
                )));
            }
            info!(
                "Filesystem '{}' created with key sealed to TPM2 (PCR-7 bound)",
                req.name
            );
        }

        Ok(Filesystem {
            name: req.name.clone(),
            uuid: uuid.clone(),
            devices: fs_devices,
            mount_point: Some(mount_point),
            mounted: true,
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            options: read_fs_options_sysfs(&uuid).await,
            last_mount_error: None,
        })
    }

    /// Unmount and destroy a filesystem, wiping superblocks from all member devices.
    pub async fn destroy(&self, req: DestroyFilesystemRequest) -> Result<(), FilesystemError> {
        if req.confirm_name != req.name {
            return Err(FilesystemError::InvalidInput(
                "confirmation name does not match filesystem name".into(),
            ));
        }

        let fs = self.get(&req.name).await?;

        // Unmount if mounted
        if fs.mounted
            && let Some(ref mp) = fs.mount_point
        {
            info!("Unmounting filesystem '{}' from {}", req.name, mp);
            cmd::run_ok("umount", &[mp.as_str()])
                .await
                .map_err(FilesystemError::CommandFailed)?;
        }

        // Also try unmounting by UUID — catches kernel-auto-assembled filesystems
        // that the engine doesn't know are mounted (e.g. after a reboot).
        let uuid_mount = format!("UUID={}", fs.uuid);
        let _ = cmd::run_ok("umount", &[&uuid_mount]).await;

        // Forget the filesystem entirely — destroy wipes the
        // superblocks below, so keeping a stale state entry would
        // have `restore_mounts` waiting 60 s for the now-gone devices
        // on every boot. Distinct from plain `unmount`, which uses
        // `save_fs_unmounted` to preserve tuned options.
        forget_fs(&req.name).await;

        // Remove mount point directory if it exists
        let mount_dir = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        let _ = tokio::fs::remove_dir_all(&mount_dir).await;

        // Wipe bcachefs superblocks from all member devices
        for dev in &fs.devices {
            info!("Wiping bcachefs superblock on {}", dev.path);
            cmd::run_ok("wipefs", &["-a", &dev.path])
                .await
                .map_err(|e| {
                    FilesystemError::CommandFailed(format!("failed to wipe {}: {e}", dev.path))
                })?;
        }

        // Flush the kernel's blkid cache so the ghost filesystem disappears
        let _ = cmd::run_ok("udevadm", &["trigger"]).await;
        let _ = cmd::run_ok("udevadm", &["settle"]).await;

        self.invalidate_list_cache().await;
        Ok(())
    }

    /// Mount an existing unmounted filesystem
    pub async fn mount(&self, name: &str) -> Result<Filesystem, FilesystemError> {
        self.mount_maybe_degraded(name, false).await
    }

    /// Mount, optionally forcing the `degraded` option on to bring a pool
    /// up without a missing member. When `force_degraded` is set the flag
    /// is persisted via the normal mount-options save, so the pool keeps
    /// mounting degraded across reboots until the operator restores the
    /// device and turns it back off. See #451.
    pub async fn mount_maybe_degraded(
        &self,
        name: &str,
        force_degraded: bool,
    ) -> Result<Filesystem, FilesystemError> {
        let state = load_fs_state().await;
        let mut opts = get_fs_mount_options(&state, name);
        if force_degraded {
            opts.degraded = Some(true);
        }
        self.mount_with_opts(name, &opts).await
    }

    /// Mount with explicit mount options
    async fn mount_with_opts(
        &self,
        name: &str,
        opts: &FsMountOptions,
    ) -> Result<Filesystem, FilesystemError> {
        info!("Mounting filesystem '{}'", name);
        let fs = self.get(name).await?;
        if fs.mounted {
            info!("Filesystem '{}' is already mounted", name);
            return Ok(fs);
        }

        let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
        tokio::fs::create_dir_all(&mount_point).await?;

        let first_device = fs.devices.first().map(|d| d.path.as_str()).unwrap_or("");

        // Unlock decision: ASK BCACHEFS, don't infer.
        //
        // `bcachefs show-super` is the only thing that can tell us
        // authoritatively whether this filesystem needs an unlock
        // before mount. Three branches:
        //
        // 1. show-super succeeds → either unencrypted, or encrypted
        //    with a key already loaded. Nothing to do; the kernel
        //    has what it needs for `bcachefs mount`.
        // 2. show-super fails with "error reading passphrase" →
        //    encrypted, no usable key reachable. Read a key from
        //    KEYS_DIR (`.tpm` then `.key`) and `bcachefs unlock`.
        //    If no key file is present and the kernel keyring is
        //    empty, the FS is genuinely locked — fail with a clear
        //    message so the operator unlocks via the WebUI.
        // 3. show-super fails for some other reason → don't second-
        //    guess, let `bcachefs mount` produce the canonical error.
        //
        // History of getting here wrong (don't undo any of this):
        // - Originally we gated on `opts.encrypted == Some(true)`,
        //   but opts.encrypted is derived from show-super output —
        //   so on encrypted-but-locked FSes it was None (show-super
        //   failed at boot) and the auto-unlock branch never fired.
        //   `bcachefs mount` then prompted via systemd-ask-password
        //   and the engine timed out. Fixed in PR #297.
        // - PR #297 switched the gate to "if a key file exists,
        //   unlock unconditionally." That broke the inverse: stale
        //   `.key` files on unencrypted filesystems (older install
        //   paths wrote them anyway) caused `bcachefs unlock` to
        //   reply "device is not encrypted" → the mount path
        //   propagated the error → filesystems unmounted at boot
        //   on .0f.ee and 10.10.20.100. The fix below is to probe
        //   bcachefs FIRST and only attempt unlock when bcachefs
        //   itself reports the FS as needing one.
        let unlocked_via_key_file = match probe_needs_unlock(first_device).await {
            NeedsUnlock::No => false,
            NeedsUnlock::Yes => {
                if let Some(bytes) = read_unlock_key(name).await? {
                    bcachefs_unlock_with_key(first_device, &bytes).await?;
                    true
                } else if is_bcachefs_key_loaded(&fs.uuid).await {
                    // Probe said "needs unlock" but show-super might
                    // have just raced our key-loading; the keyring
                    // has it, so let mount try.
                    false
                } else {
                    return Err(FilesystemError::CommandFailed(format!(
                        "encrypted filesystem '{name}' is locked — unlock it first, then mount."
                    )));
                }
            }
            NeedsUnlock::Unknown => false,
        };

        let device_arg = fs
            .devices
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join(":");
        let mount_opt_str = build_mount_opts(opts);
        if let Err(e) = cmd::run_ok(
            "bcachefs",
            &["mount", "-o", &mount_opt_str, &device_arg, &mount_point],
        )
        .await
        {
            // Persist *why* it failed (named missing devices + classified
            // reason) so the WebUI can explain an unmounted pool instead
            // of just showing "Unmounted" — boot-time failures otherwise
            // vanish into the log. See #451.
            let failure = build_mount_failure(opts, &fs, e.clone()).await;
            self.record_mount_failure(name, failure).await;
            return Err(FilesystemError::CommandFailed(e));
        }

        // Apply I/O scheduler to member block devices
        if let Some(ref sched) = opts.io_scheduler
            && let Err(e) = apply_io_scheduler(&fs.devices, sched).await
        {
            warn!("Failed to set I/O scheduler: {e}");
        }

        // Track mount state with identity info for boot reconciliation.
        // If we successfully unlocked via a stored key, force the
        // persisted `encrypted` flag to true: opts.encrypted may have
        // been None (e.g. older NASty versions didn't always persist
        // it, or a previous boot's show-super failed and the recorded
        // value got cleared), and we want next boot's auto-unlock
        // branch to have the right signal without depending on a
        // possibly-failing show-super.
        let mut saved_opts = opts.clone();
        saved_opts.uuid = Some(fs.uuid.clone());
        saved_opts.devices = fs.devices.iter().map(|d| d.path.clone()).collect();
        if unlocked_via_key_file {
            saved_opts.encrypted = Some(true);
        }
        save_fs_mounted_with_opts(name, saved_opts).await;

        // Mounted cleanly — drop any stale failure record so the banner
        // clears on the operator's next view.
        self.clear_mount_failure(name).await;

        self.invalidate_list_cache().await;
        self.get(name).await
    }

    /// Unlock an encrypted filesystem with a passphrase (does not mount).
    pub async fn unlock(
        &self,
        name: &str,
        passphrase: &str,
    ) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(name).await?;

        let first_device = fs
            .devices
            .first()
            .map(|d| d.path.clone())
            .ok_or_else(|| FilesystemError::CommandFailed("no devices".to_string()))?;

        let stdin = format!("{passphrase}\n");
        cmd::run_ok_stdin(
            "bcachefs",
            &["unlock", "-k", "session", &first_device],
            stdin.as_bytes(),
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        info!("Filesystem '{name}' unlocked");
        self.invalidate_list_cache().await;
        self.get(name).await
    }

    /// Lock an encrypted filesystem: unmount it (if mounted) and revoke
    /// its key from the kernel keyring. Mirror of `unlock`. After this,
    /// remounting requires re-entering the passphrase via `unlock`
    /// (or the stored auto-unlock key, if one is on disk — those two
    /// concepts are independent; "lock" doesn't delete the stored key,
    /// `delete_key` does).
    ///
    /// No-op (success) if the FS is already locked. Errors out if the
    /// FS isn't encrypted at all — calling lock on a plain FS is a
    /// programming bug worth surfacing.
    pub async fn lock(&self, name: &str) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(name).await?;
        if fs.options.encrypted != Some(true) {
            return Err(FilesystemError::InvalidInput(format!(
                "filesystem '{name}' is not encrypted"
            )));
        }
        if fs.mounted {
            self.unmount(name).await?;
        }
        match find_bcachefs_key_id(&fs.uuid).await {
            Some(key_id) => {
                cmd::run_ok("keyctl", &["unlink", &key_id, "@s"])
                    .await
                    .map_err(FilesystemError::CommandFailed)?;
                info!("Filesystem '{name}' locked (key {key_id} unlinked from session keyring)");
            }
            None => {
                info!("Filesystem '{name}' was already locked (no key in keyring)");
            }
        }
        self.invalidate_list_cache().await;
        self.get(name).await
    }

    /// Export the stored encryption key for a filesystem.
    pub async fn export_key(&self, name: &str) -> Result<String, FilesystemError> {
        let key_path = format!("{KEYS_DIR}/{name}.key");
        tokio::fs::read_to_string(&key_path).await.map_err(|e| {
            // Keep the io::Error kind in the message — "permission denied"
            // vs "not found" is the difference between a real bug and a
            // user with no stored key.
            FilesystemError::CommandFailed(format!("read key for '{name}' at {key_path}: {e}"))
        })
    }

    /// Delete the stored encryption key (switch to passphrase-only mode).
    pub async fn delete_key(&self, name: &str) -> Result<(), FilesystemError> {
        let key_path = format!("{KEYS_DIR}/{name}.key");
        tokio::fs::remove_file(&key_path).await.map_err(|e| {
            FilesystemError::CommandFailed(format!("delete key for '{name}' at {key_path}: {e}"))
        })
    }

    /// TPM2 bind status for filesystem `name`.
    ///
    /// `tpm_available` is the host capability (`/dev/tpmrm0` present);
    /// `bound` is per-FS (a `<name>.tpm` sealed blob exists). The two
    /// are independent — a host can lose its TPM (firmware downgrade,
    /// chip swap) and still have a stale `.tpm` file from before.
    pub async fn tpm_status(&self, name: &str) -> TpmBindStatus {
        let sealed_path = format!("{KEYS_DIR}/{name}.{TPM_SEALED_SUFFIX}");
        TpmBindStatus {
            tpm_available: nasty_common::tpm::is_available().await,
            bound: Path::new(&sealed_path).exists(),
        }
    }

    /// Seal the stored plaintext key with the host TPM and write it
    /// next to the existing `<name>.key` as `<name>.tpm`. The plaintext
    /// `.key` is **kept** as a recovery path — wiping it is a separate
    /// explicit step the user invokes via `delete_key` after they've
    /// satisfied themselves the bind works.
    ///
    /// Errors when:
    ///   - the host has no usable TPM2 (`/dev/tpmrm0` missing);
    ///   - no plaintext `.key` exists to seal (caller must create the
    ///     FS with `store_key=true` first);
    ///   - the FS isn't encrypted at all (programming bug — surfaced
    ///     so it doesn't silently no-op).
    pub async fn tpm_bind(&self, name: &str) -> Result<TpmBindStatus, FilesystemError> {
        let fs = self.get(name).await?;
        if fs.options.encrypted != Some(true) {
            return Err(FilesystemError::InvalidInput(format!(
                "filesystem '{name}' is not encrypted"
            )));
        }
        if !nasty_common::tpm::is_available().await {
            return Err(FilesystemError::CommandFailed(
                "TPM2 not available on this host".into(),
            ));
        }

        let key_path = format!("{KEYS_DIR}/{name}.key");
        let plaintext = tokio::fs::read(&key_path).await.map_err(|e| {
            FilesystemError::CommandFailed(format!(
                "no stored key for '{name}' at {key_path}: {e} — bind requires an existing .key"
            ))
        })?;

        let blob = nasty_common::tpm::seal_with_pcr7(&plaintext)
            .await
            .map_err(|e| FilesystemError::CommandFailed(format!("tpm seal: {e}")))?;
        let json = serde_json::to_vec_pretty(&blob)
            .map_err(|e| FilesystemError::CommandFailed(format!("serialize sealed blob: {e}")))?;

        let sealed_path = format!("{KEYS_DIR}/{name}.{TPM_SEALED_SUFFIX}");
        tokio::fs::write(&sealed_path, &json)
            .await
            .map_err(|e| FilesystemError::CommandFailed(format!("write {sealed_path}: {e}")))?;
        info!("Filesystem '{name}' key sealed to TPM at {sealed_path}");

        Ok(self.tpm_status(name).await)
    }

    /// Remove the TPM-sealed copy of the key. The plaintext `.key`
    /// (if present) is unaffected — auto-unlock continues working off
    /// it. No-op (success) when no sealed blob exists.
    pub async fn tpm_unbind(&self, name: &str) -> Result<TpmBindStatus, FilesystemError> {
        let sealed_path = format!("{KEYS_DIR}/{name}.{TPM_SEALED_SUFFIX}");
        match tokio::fs::remove_file(&sealed_path).await {
            Ok(()) => info!("Filesystem '{name}' TPM seal removed ({sealed_path})"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(FilesystemError::CommandFailed(format!(
                    "remove {sealed_path}: {e}"
                )));
            }
        }
        Ok(self.tpm_status(name).await)
    }

    /// Update runtime-mutable options on a mounted filesystem via sysfs.
    pub async fn update_options(
        &self,
        req: UpdateFilesystemOptionsRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to update options".to_string(),
            ));
        }
        // Validate compression specs up front so a typo in the level
        // can't leave foreground set and background rejected halfway
        // through the sysfs writes below.
        for spec in [&req.compression, &req.background_compression]
            .into_iter()
            .flatten()
        {
            validate_compression(spec).map_err(FilesystemError::InvalidInput)?;
        }

        let uuid = &fs.uuid;
        let base = format!("/sys/fs/bcachefs/{uuid}/options");

        async fn write_opt(base: &str, name: &str, value: &str) -> Result<(), FilesystemError> {
            let path = format!("{base}/{name}");
            let v = if value.is_empty() { "none" } else { value };
            tokio::fs::write(&path, v)
                .await
                .map_err(|e| FilesystemError::CommandFailed(format!("failed to set {name}: {e}")))
        }

        if let Some(ref v) = req.compression {
            write_opt(&base, "compression", v).await?;
        }
        if let Some(ref v) = req.background_compression {
            write_opt(&base, "background_compression", v).await?;
        }
        if let Some(ref v) = req.foreground_target {
            write_opt(&base, "foreground_target", v).await?;
        }
        if let Some(ref v) = req.background_target {
            write_opt(&base, "background_target", v).await?;
        }
        if let Some(ref v) = req.promote_target {
            write_opt(&base, "promote_target", v).await?;
        }
        if let Some(ref v) = req.metadata_target {
            write_opt(&base, "metadata_target", v).await?;
        }
        if let Some(ref v) = req.error_action {
            write_opt(&base, "errors", v).await?;
        }
        if let Some(ec) = req.erasure_code {
            write_opt(&base, "erasure_code", if ec { "1" } else { "0" }).await?;
        }
        if let Some(ref v) = req.data_checksum {
            write_opt(&base, "data_checksum", v).await?;
        }
        if let Some(ref v) = req.metadata_checksum {
            write_opt(&base, "metadata_checksum", v).await?;
        }
        if let Some(v) = req.data_replicas {
            write_opt(&base, "data_replicas", &v.to_string()).await?;
        }
        if let Some(v) = req.metadata_replicas {
            write_opt(&base, "metadata_replicas", &v.to_string()).await?;
        }
        if let Some(v) = req.move_ios_in_flight {
            write_opt(&base, "move_ios_in_flight", &v.to_string()).await?;
        }
        if let Some(ref v) = req.move_bytes_in_flight {
            write_opt(&base, "move_bytes_in_flight", v).await?;
        }
        if let Some(v) = req.journal_flush_delay {
            write_opt(&base, "journal_flush_delay", &v.to_string()).await?;
        }

        // Apply I/O scheduler to member block devices
        if let Some(ref sched) = req.io_scheduler {
            apply_io_scheduler(&fs.devices, sched).await?;
        }

        // Mount options require a remount to take effect — but only if they actually changed.
        let state = load_fs_state().await;
        let current = state.get(&req.name).cloned().unwrap_or_default();
        let mount_changed = (req.version_upgrade.is_some()
            && req.version_upgrade != current.version_upgrade)
            || (req.degraded.is_some() && req.degraded != current.degraded)
            || (req.verbose.is_some() && req.verbose != current.verbose)
            || (req.fsck.is_some() && req.fsck != current.fsck)
            || (req.journal_flush_disabled.is_some()
                && req.journal_flush_disabled != current.journal_flush_disabled)
            || (req.journal_flush_delay.is_some()
                && req.journal_flush_delay != current.journal_flush_delay);
        drop(state);

        // Persist state changes (mount opts + io_scheduler)
        let state_changed = mount_changed
            || (req.io_scheduler.is_some() && req.io_scheduler != current.io_scheduler);

        if state_changed {
            let mut state = load_fs_state().await;
            {
                let opts = state.entry(req.name.clone()).or_default();
                if let Some(ref v) = req.version_upgrade {
                    opts.version_upgrade = Some(v.clone());
                }
                if let Some(v) = req.degraded {
                    opts.degraded = Some(v);
                }
                if let Some(v) = req.verbose {
                    opts.verbose = Some(v);
                }
                if let Some(v) = req.fsck {
                    opts.fsck = Some(v);
                }
                if let Some(v) = req.journal_flush_disabled {
                    opts.journal_flush_disabled = Some(v);
                }
                if let Some(v) = req.journal_flush_delay {
                    opts.journal_flush_delay = Some(v);
                }
                if let Some(ref v) = req.io_scheduler {
                    opts.io_scheduler = Some(v.clone());
                }
            }
            if let Err(e) = save_fs_state(&state).await {
                // The runtime FS state is updated in memory, but at next
                // boot we'll fall back to whatever was last persisted —
                // so user-tweaked mount options silently revert. Log so
                // the user can match a "my settings keep resetting"
                // bug to the persistence failure that caused it.
                warn!("save_fs_state after option update failed: {e}");
            }
        }

        if mount_changed {
            // Remount in-place (no unmount needed, works even when busy)
            let mount_point = format!("{NASTY_MOUNT_BASE}/{}", req.name);
            let state = load_fs_state().await;
            let mount_opt_str =
                build_mount_opts(state.get(&req.name).unwrap_or(&FsMountOptions::default()));
            cmd::run_ok(
                "mount",
                &["-o", &format!("remount,{mount_opt_str}"), &mount_point],
            )
            .await
            .map_err(FilesystemError::CommandFailed)?;
            self.invalidate_list_cache().await;
            return self.get(&req.name).await;
        }

        self.invalidate_list_cache().await;
        self.get(&req.name).await
    }

    /// Unmount a filesystem
    pub async fn unmount(&self, name: &str) -> Result<(), FilesystemError> {
        info!("Unmounting filesystem '{}'", name);
        let fs = self.get(name).await?;
        if let Some(ref mp) = fs.mount_point {
            info!("Running umount on {}", mp);
            cmd::run_ok("umount", &[mp.as_str()])
                .await
                .map_err(FilesystemError::CommandFailed)?;
            info!("Filesystem '{}' unmounted successfully", name);
        } else {
            info!("Filesystem '{}' has no mount point, skipping umount", name);
        }

        // Track mount state
        save_fs_unmounted(name).await;

        self.invalidate_list_cache().await;
        Ok(())
    }

    /// List block devices available for filesystem creation
    pub async fn list_devices(&self) -> Result<Vec<BlockDevice>, FilesystemError> {
        // Collect all device paths already used by filesystems. If list()
        // fails (corrupt state file, permissions, …) we fall back to an
        // empty set so the caller still gets *some* answer, but we log
        // the failure so the operator can see why "available devices"
        // suddenly includes ones that are actually in use.
        let filesystems = match self.list().await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "list_devices: failed to enumerate existing filesystems ({e}) — \
                     falling back to empty set; some devices may appear available \
                     even though they're actually in use"
                );
                Vec::new()
            }
        };
        let used_devices: std::collections::HashSet<String> = filesystems
            .iter()
            .flat_map(|f| f.devices.iter().map(|d| d.path.clone()))
            .collect();

        let output = cmd::run_ok(
            "lsblk",
            &[
                "-Jbno",
                "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,ROTA,MODEL,SERIAL,VENDOR,TRAN,UUID",
            ],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or(serde_json::Value::Null);

        let mut devices = Vec::new();
        if let Some(blockdevices) = parsed.get("blockdevices").and_then(|v| v.as_array()) {
            fn classify(name: &str, rota: bool, transport: Option<&str>) -> (bool, String) {
                if name.starts_with("nvme") {
                    return (false, "nvme".to_string());
                }
                if name.starts_with("mmcblk") {
                    return (false, "mmc".to_string());
                }
                // SAS drives report `tran == "sas"` from lsblk. Both SAS HDDs
                // and SAS SSDs get the `sas` class so the WebUI Devices tab
                // can badge them as the enterprise drives they are rather
                // than disguising them as SATA hdd/ssd (issue #365). The
                // rotational bit is still set correctly so callers that care
                // about spinning vs solid-state get the right answer.
                if matches!(transport, Some("sas")) {
                    return (rota, "sas".to_string());
                }
                if rota {
                    (true, "hdd".to_string())
                } else {
                    (false, "ssd".to_string())
                }
            }

            // Read /proc/mounts to know which devices are *actually* mounted.
            // lsblk's mountpoint field can be stale after bcachefs device removal/wipe.
            let mounted_devices: std::collections::HashSet<String> =
                tokio::fs::read_to_string("/proc/mounts")
                    .await
                    .unwrap_or_default()
                    .lines()
                    .flat_map(|line| {
                        // Each line: "device mountpoint fstype options ..."
                        // bcachefs uses colon-separated multi-device: "/dev/sdb:/dev/sdc /fs/first ..."
                        let dev_field = line.split_whitespace().next().unwrap_or("");
                        dev_field.split(':').map(String::from).collect::<Vec<_>>()
                    })
                    .collect();

            fn collect_devices(
                devs: &[serde_json::Value],
                fs_devices: &std::collections::HashSet<String>,
                mounted_devices: &std::collections::HashSet<String>,
                out: &mut Vec<BlockDevice>,
            ) {
                for dev in devs {
                    let name = dev.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let dev_type = dev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let size = dev
                        .get("size")
                        .and_then(|v| {
                            v.as_u64()
                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        })
                        .unwrap_or(0);
                    let mountpoint = dev
                        .get("mountpoint")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let fstype = dev.get("fstype").and_then(|v| v.as_str()).map(String::from);
                    let rota = dev
                        .get("rota")
                        .and_then(|v| {
                            v.as_bool()
                                .or_else(|| v.as_str().map(|s| s == "1"))
                                .or_else(|| v.as_u64().map(|n| n == 1))
                        })
                        .unwrap_or(false);
                    // lsblk surfaces these only on whole disks; on partitions
                    // they're empty/null. Treat empty-after-trim as None so
                    // the WebUI can hide the field entirely instead of
                    // rendering blanks.
                    let pick = |key: &str| -> Option<String> {
                        dev.get(key)
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                    };
                    let model = pick("model");
                    let serial = pick("serial");
                    let vendor = pick("vendor");
                    let transport = pick("tran");
                    let fs_uuid = pick("uuid");

                    // Transport needs to be resolved before classify so the
                    // SAS path can use it.
                    let (rotational, device_class) = classify(name, rota, transport.as_deref());

                    if dev_type == "disk" || dev_type == "part" {
                        let path = format!("/dev/{name}");
                        let in_fs = fs_devices.contains(&path);
                        let actually_mounted = mounted_devices.contains(&path);
                        out.push(BlockDevice {
                            path,
                            size_bytes: size,
                            dev_type: dev_type.to_string(),
                            mount_point: mountpoint,
                            fs_type: fstype,
                            fs_uuid,
                            in_use: in_fs || actually_mounted,
                            rotational,
                            device_class,
                            model,
                            serial,
                            vendor,
                            transport,
                        });
                    }

                    if let Some(children) = dev.get("children").and_then(|v| v.as_array()) {
                        collect_devices(children, fs_devices, mounted_devices, out);
                    }
                }
            }
            collect_devices(blockdevices, &used_devices, &mounted_devices, &mut devices);
        }

        // Mark parent disks as in_use if any of their partitions are in_use.
        let in_use_paths: std::collections::HashSet<String> = devices
            .iter()
            .filter(|d| d.in_use && d.dev_type == "part")
            .map(|d| d.path.clone())
            .collect();
        for dev in &mut devices {
            if dev.dev_type == "disk"
                && !dev.in_use
                && in_use_paths.iter().any(|p| p.starts_with(&dev.path))
            {
                dev.in_use = true;
            }
        }

        // Detect unpartitioned free space on disks with existing partitions.
        // Use sgdisk to find the largest free gap; if > 1 GiB, add a virtual "free" entry.
        // Skip boot devices (mmcblk/eMMC) — they should never be offered as storage.
        let partitioned_disks: Vec<String> = devices
            .iter()
            .filter(|d| d.dev_type == "part")
            .filter(|d| !d.path.contains("mmcblk"))
            .filter_map(|d| {
                // /dev/sda1 -> /dev/sda, /dev/nvme0n1p1 -> /dev/nvme0n1
                let name = d.path.trim_start_matches("/dev/");
                // Strip trailing partition number
                if name.contains("nvme") || name.contains("loop") || name.contains("mmcblk") {
                    name.rsplit_once('p')
                        .map(|(base, _)| format!("/dev/{base}"))
                } else {
                    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
                    Some(format!("/dev/{base}"))
                }
            })
            // A disk that is itself a filesystem member (whole-disk
            // bcachefs) can't host a new partition, and probing it with
            // sgdisk only produces GPT lectures — the member superblock
            // sits where a partition table would be. Partition nodes on
            // such a disk are stale kernel state from a pre-wipe table
            // (#488).
            .filter(|parent| !used_devices.contains(parent))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        info!(
            "Free-space detection: found {} partitioned disks: {:?}",
            partitioned_disks.len(),
            partitioned_disks
        );
        const MIN_FREE_BYTES: u64 = 1_073_741_824; // 1 GiB
        for disk_path in &partitioned_disks {
            match get_disk_free_space(disk_path).await {
                Ok(free_bytes) => {
                    info!("Free space on {disk_path}: {free_bytes} bytes");
                    if free_bytes >= MIN_FREE_BYTES {
                        let disk = devices.iter().find(|d| &d.path == disk_path);
                        let (rotational, device_class, model, serial, vendor, transport) = disk
                            .map(|d| {
                                (
                                    d.rotational,
                                    d.device_class.clone(),
                                    d.model.clone(),
                                    d.serial.clone(),
                                    d.vendor.clone(),
                                    d.transport.clone(),
                                )
                            })
                            .unwrap_or((false, "ssd".to_string(), None, None, None, None));
                        devices.push(BlockDevice {
                            path: format!("{disk_path}:free"),
                            size_bytes: free_bytes,
                            dev_type: "free".to_string(),
                            mount_point: None,
                            fs_type: None,
                            fs_uuid: None,
                            in_use: false,
                            rotational,
                            device_class,
                            model,
                            serial,
                            vendor,
                            transport,
                        });
                    }
                }
                // Routine for foreign or half-wiped partition tables —
                // the disk simply gets no free-space entry. sgdisk's
                // multi-line GPT lecture at warn level flooded the
                // journal on every device.list refresh (#488).
                Err(e) => debug!("Failed to get free space for {disk_path}: {e}"),
            }
        }

        Ok(devices)
    }

    /// Wipe all filesystem signatures from a device.
    /// Only allowed if the device is not currently in use by any filesystem.
    pub async fn device_wipe(&self, path: &str) -> Result<(), FilesystemError> {
        let devices = self.list_devices().await?;
        let dev = devices
            .iter()
            .find(|d| d.path == path)
            .ok_or_else(|| FilesystemError::CommandFailed(format!("device not found: {path}")))?;
        if dev.in_use {
            return Err(FilesystemError::CommandFailed(format!(
                "device {path} is currently in use"
            )));
        }
        info!("Wiping device {path}");
        cmd::run_ok("wipefs", &["-a", path])
            .await
            .map_err(FilesystemError::CommandFailed)?;
        if dev.dev_type == "disk" {
            // wipefs erases the signatures libblkid probes for (primary
            // GPT, protective MBR, filesystem superblocks) but NOT the
            // backup GPT at the end of the disk. Leaving it behind makes
            // every GPT-aware tool from then on lecture about "invalid
            // main header, valid backup — you should repair the disk!"
            // (#488). Zap both tables explicitly; best-effort because
            // sgdisk exits non-zero while cleaning up exactly the
            // half-wiped state we're fixing.
            if let Err(e) = cmd::run_ok("sgdisk", &["--zap-all", path]).await {
                debug!("sgdisk --zap-all {path}: {e} (expected on a half-wiped GPT)");
            }
            // Drop stale kernel partition nodes from the pre-wipe table
            // so the device list (and the free-space scan) stop seeing
            // partitions that no longer exist on disk.
            let _ = cmd::run_ok("partprobe", &[path]).await;
        }
        self.invalidate_list_cache().await;
        Ok(())
    }

    /// Add a device to an existing mounted filesystem.
    /// bcachefs device add [--label=X] [--durability=X] <mountpoint> <device>
    pub async fn device_add(&self, req: DeviceAddRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to add a device".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap().clone();

        if !Path::new(&req.device.path).exists() {
            return Err(FilesystemError::DeviceNotFound(req.device.path.clone()));
        }

        // Reject if the device is actively in use (mounted or member of a live filesystem).
        let known_devices = self.list_devices().await?;
        if known_devices
            .iter()
            .any(|d| d.path == req.device.path && d.in_use)
        {
            return Err(FilesystemError::DeviceInUse(req.device.path.clone()));
        }
        // Reject if the device has a filesystem signature (including stale bcachefs superblocks
        // left over after removal). The user must explicitly wipe it via Disks → Wipe first —
        // unless the superblock belongs to *this* filesystem and a member slot is offline, in
        // which case the right move is a re-attach, not a wipe (#472).
        if is_device_bcachefs(&req.device.path).await {
            let same_fs = get_fs_uuid(&req.device.path).await.as_deref() == Some(fs.uuid.as_str());
            let has_missing_member = fs.devices.iter().any(|d| d.missing == Some(true));
            return Err(FilesystemError::CommandFailed(
                if same_fs && has_missing_member {
                    format!(
                        "{} is an offline member of this filesystem. Use \"Bring online\" to re-attach it with its data intact instead of adding it as a new device.",
                        req.device.path
                    )
                } else if same_fs {
                    format!(
                        "{} is a former member of this filesystem. Go to Disks → Wipe to erase its old superblock before re-adding it as a new device.",
                        req.device.path
                    )
                } else {
                    format!(
                        "{} has an existing bcachefs superblock. Go to Disks → Wipe to erase it before adding it to a filesystem.",
                        req.device.path
                    )
                },
            ));
        }

        let mut args: Vec<String> = vec!["device".into(), "add".into()];
        if let Some(ref label) = req.device.label {
            args.push(format!("--label={label}"));
        }
        if let Some(durability) = req.device.durability {
            args.push(format!("--durability={durability}"));
        }
        args.push(mount_point.clone());
        args.push(req.device.path.clone());

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        info!(
            "Adding device {} to filesystem '{}'",
            req.device.path, req.filesystem
        );
        cmd::run_ok("bcachefs", &arg_refs)
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.invalidate_list_cache().await;
        self.get(&req.filesystem).await
    }

    /// Remove a device from a mounted filesystem.
    /// This evacuates data first, then removes the device.
    /// bcachefs device remove <device> <mountpoint>
    pub async fn device_remove(
        &self,
        req: DeviceActionRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to remove a device".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();

        info!(
            "Removing device {} from filesystem '{}'{}",
            req.device,
            req.filesystem,
            if req.force { " (forced)" } else { "" }
        );
        // `req.device` is a path for present devices, or a numeric member
        // index for a missing/dead member (no /dev node). `bcachefs device
        // remove` accepts both, with the mount point as the trailing PATH
        // arg. For a missing member nothing can be migrated off, so force
        // both data and metadata — safe while enough replicas survive.
        let mut args = vec!["device", "remove"];
        if req.force {
            args.push("--force");
            args.push("--force-metadata");
        }
        args.push(&req.device);
        args.push(mount_point);
        cmd::run_ok("bcachefs", &args)
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.invalidate_list_cache().await;
        self.get(&req.filesystem).await
    }

    /// Evacuate all data off a device (move to other devices in the filesystem).
    /// This is a prerequisite for safe device removal.
    /// bcachefs device evacuate <device>
    pub async fn device_evacuate(&self, req: DeviceActionRequest) -> Result<(), FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to evacuate a device".to_string(),
            ));
        }

        // Refuse a second evacuation of the same device: either the
        // spawned `bcachefs device evacuate` is still running (tracked
        // in-process — bcachefs takes a moment to persist `evacuating`,
        // and hammering the button in that window must not spawn
        // parallel migrations, #479), or the device state already says
        // so.
        {
            let mut inflight = self.evacuating.lock().await;
            let state_says_evacuating = fs
                .devices
                .iter()
                .any(|d| d.path == req.device && d.state.as_deref() == Some("evacuating"));
            if state_says_evacuating || inflight.contains(&req.device) {
                return Err(FilesystemError::CommandFailed(format!(
                    "evacuation of {} is already in progress",
                    req.device
                )));
            }
            inflight.insert(req.device.clone());
        }

        let device = req.device.clone();
        let fs_name = req.filesystem.clone();
        let evacuating = self.evacuating.clone();
        info!(
            "Starting evacuation of device {} in filesystem '{}'",
            device, fs_name
        );

        // Spawn evacuation in background — this can take hours for large devices.
        // bcachefs sets the device state to "evacuating" automatically.
        tokio::spawn(async move {
            match cmd::run_ok("bcachefs", &["device", "evacuate", &device]).await {
                Ok(_) => info!("Evacuation of {} in '{}' completed", device, fs_name),
                Err(e) => warn!("Evacuation of {} in '{}' failed: {}", device, fs_name, e),
            }
            evacuating.lock().await.remove(&device);
        });

        self.invalidate_list_cache().await;
        Ok(())
    }

    /// Change the persistent state of a device (rw, ro, failed, spare).
    /// bcachefs device set-state <new_state> <device> [path]
    pub async fn device_set_state(
        &self,
        req: DeviceSetStateRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let valid_states = ["rw", "ro", "failed", "spare"];
        if !valid_states.contains(&req.state.as_str()) {
            return Err(FilesystemError::CommandFailed(format!(
                "invalid device state '{}', must be one of: {}",
                req.state,
                valid_states.join(", ")
            )));
        }

        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to change device state".to_string(),
            ));
        }
        info!(
            "Setting device {} state to '{}' in filesystem '{}'",
            req.device, req.state, req.filesystem
        );
        cmd::run_ok(
            "bcachefs",
            &["device", "set-state", &req.state, &req.device],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        self.invalidate_list_cache().await;
        self.get(&req.filesystem).await
    }

    /// Bring a device online (temporary, no membership change).
    /// bcachefs device online <device>
    pub async fn device_online(
        &self,
        req: DeviceActionRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to online a device".to_string(),
            ));
        }

        info!(
            "Onlining device {} in filesystem '{}'",
            req.device, req.filesystem
        );
        cmd::run_ok("bcachefs", &["device", "online", &req.device])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.invalidate_list_cache().await;
        self.get(&req.filesystem).await
    }

    /// Take a device offline (temporary, no membership change).
    /// bcachefs device offline <device>
    pub async fn device_offline(
        &self,
        req: DeviceActionRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to offline a device".to_string(),
            ));
        }
        info!(
            "Offlining device {} in filesystem '{}'",
            req.device, req.filesystem
        );
        cmd::run_ok("bcachefs", &["device", "offline", &req.device])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.invalidate_list_cache().await;
        self.get(&req.filesystem).await
    }

    /// Set the label on a device of a mounted filesystem via the bcachefs sysfs interface.
    ///
    /// Labels drive tiering target selection (e.g. "ssd.fast", "hdd.archive").
    /// The sysfs entry `/sys/fs/bcachefs/<uuid>/dev-<N>/label` is writable on a
    /// live filesystem; we find the right dev-N by matching the `block` symlink.
    pub async fn device_set_label(
        &self,
        req: DeviceSetLabelRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to set a device label".to_string(),
            ));
        }

        // Validate: device must be a member of the filesystem
        if !fs.devices.iter().any(|d| d.path == req.device) {
            return Err(FilesystemError::CommandFailed(format!(
                "{} is not a member of filesystem '{}'",
                req.device, req.filesystem
            )));
        }

        // Find the sysfs dev-N directory whose `block` symlink resolves to our device.
        // The symlink target ends with the kernel device name (e.g. "sdc").
        let dev_name = req.device.trim_start_matches("/dev/");
        let sysfs_base = format!("/sys/fs/bcachefs/{}", fs.uuid);
        let mut label_path: Option<std::path::PathBuf> = None;

        let mut rd = tokio::fs::read_dir(&sysfs_base).await.map_err(|e| {
            FilesystemError::CommandFailed(format!("failed to read sysfs {sysfs_base}: {e}"))
        })?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("dev-") {
                continue;
            }
            let block_link = entry.path().join("block");
            if let Ok(target) = tokio::fs::read_link(&block_link).await
                && target.file_name().map(|n| n == dev_name).unwrap_or(false)
            {
                label_path = Some(entry.path().join("label"));
                break;
            }
        }

        let label_path = label_path.ok_or_else(|| {
            FilesystemError::CommandFailed(format!(
                "could not find sysfs entry for {} in filesystem '{}'",
                req.device, req.filesystem
            ))
        })?;

        info!(
            "Setting label '{}' on {} in filesystem '{}'",
            req.label, req.device, req.filesystem
        );
        tokio::fs::write(&label_path, &req.label)
            .await
            .map_err(|e| {
                FilesystemError::CommandFailed(format!("failed to write sysfs label: {e}"))
            })?;

        self.invalidate_list_cache().await;
        self.get(&req.filesystem).await
    }

    // ── Filesystem health & monitoring ────────────────────────────────

    /// Get detailed filesystem usage from `bcachefs fs usage`.
    pub async fn usage(&self, name: &str) -> Result<FsUsage, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to read usage".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();

        let raw = cmd::run_ok("bcachefs", &["fs", "usage", mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        // Also get -a output for per-device btree/user breakdown
        let raw_all = cmd::run_ok("bcachefs", &["fs", "usage", "-a", mount_point])
            .await
            .unwrap_or_default();

        let mut dev_usages = Vec::new();
        let mut data_bytes: u64 = 0;
        let mut metadata_bytes: u64 = 0;
        let mut reserved_bytes: u64 = 0;

        // Parse default output for summary: "Used:", "Online reserved:"
        // and device table: "label (device N):  devname  state  size  used  use%"
        for line in raw.lines() {
            let trimmed = line.trim();
            let lower = trimmed.to_lowercase();

            if lower.starts_with("used:") {
                if let Some(bytes) = extract_first_bytes(trimmed) {
                    data_bytes = bytes; // "Used" is total used (data + metadata)
                }
            } else if lower.starts_with("online reserved:")
                && let Some(bytes) = extract_first_bytes(trimmed)
            {
                reserved_bytes = bytes;
            }

            // Device table row: "label (device N):  sdb  rw  53264510976  8912896  0%"
            if trimmed.contains("(device")
                && trimmed.contains("):")
                && let Some(du) = parse_device_table_line(trimmed)
            {
                dev_usages.push(du);
            }
        }

        // Parse -a output to sum btree (metadata) vs user (data) across devices.
        // Per-device sections start with "label (device N):" and contain indented rows:
        //   btree:  8912896  ...
        //   user:   0        ...
        let mut total_btree: u64 = 0;
        let mut total_user: u64 = 0;
        for line in raw_all.lines() {
            let trimmed = line.trim();
            // Indented rows inside per-device sections
            if trimmed.starts_with("btree:") {
                if let Some(bytes) = extract_first_bytes(trimmed) {
                    total_btree += bytes;
                }
            } else if trimmed.starts_with("user:")
                && let Some(bytes) = extract_first_bytes(trimmed)
            {
                total_user += bytes;
            }
        }

        // Use the per-type breakdown if available
        if total_btree > 0 || total_user > 0 {
            metadata_bytes = total_btree;
            data_bytes = total_user;
        }

        Ok(FsUsage {
            raw,
            devices: dev_usages,
            data_bytes,
            metadata_bytes,
            reserved_bytes,
        })
    }

    /// Start a data scrub on a filesystem.
    /// `bcachefs scrub <mountpoint>`. The bcachefs binary blocks for
    /// the entire scrub duration (potentially hours on a multi-TB
    /// pool), so the actual run lives in a detached `tokio::spawn`.
    /// State (start time + completion result) is persisted to
    /// `SCRUB_STATE_PATH` so a `scrub_status` call after engine
    /// restart still surfaces "last scrub finished N hours ago,
    /// found X errors" rather than the previous "no scrub running".
    pub async fn scrub_start(&self, name: &str) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to start scrub".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap().clone();
        let fs_name = name.to_string();
        let now = unix_now_secs();

        // Stamp the in-memory state with started_at *before* we spawn,
        // so a `scrub_status` call landing 50ms later sees `running`.
        // The completion path below clears started_at and fills the
        // last_* fields.
        {
            let mut state = self.scrub_state.lock().await;
            let entry = state.entry(fs_name.clone()).or_insert_with(|| ScrubStatus {
                running: false,
                started_at: None,
                progress_percent: None,
                last_run_at: None,
                last_duration_secs: None,
                last_outcome: None,
                last_output: None,
                raw: "No scrub running".into(),
            });
            entry.running = true;
            entry.started_at = Some(now);
            entry.raw = "Scrub in progress...".into();
        }
        persist_scrub_state(&self.scrub_state).await;

        let store = self.scrub_state.clone();
        info!("Starting scrub on filesystem '{}'", name);
        tokio::spawn(async move {
            let mount = mount_point;
            // Stream stdout+stderr line-by-line so we can pick the
            // most recent `XX%` token out of bcachefs's progress
            // updates as it runs. Falls back gracefully when the
            // binary doesn't print percent at all — the chip just
            // shows "scrubbing (Nh ago)" via the elapsed timestamp.
            let (outcome, captured) = stream_scrub_and_collect(&mount, &fs_name, &store).await;
            let end = unix_now_secs();
            let duration = (end - now).max(0) as u64;

            match outcome {
                ScrubOutcome::Ok => info!("Scrub on '{fs_name}' completed in {duration}s: ok",),
                ScrubOutcome::Errors => warn!(
                    "Scrub on '{fs_name}' completed in {duration}s: errors detected (see WebUI for full output)",
                ),
                ScrubOutcome::Failed => {
                    warn!("Scrub on '{fs_name}' failed after {duration}s: {captured}",)
                }
            }

            let truncated = truncate_tail(&captured, SCRUB_OUTPUT_KEEP_BYTES);
            let summary = match outcome {
                ScrubOutcome::Ok => "Last scrub: ok".to_string(),
                ScrubOutcome::Errors => "Last scrub: errors detected".to_string(),
                ScrubOutcome::Failed => "Last scrub: failed".to_string(),
            };
            {
                let mut state = store.lock().await;
                let entry = state.entry(fs_name.clone()).or_insert_with(|| ScrubStatus {
                    running: false,
                    started_at: None,
                    progress_percent: None,
                    last_run_at: None,
                    last_duration_secs: None,
                    last_outcome: None,
                    last_output: None,
                    raw: summary.clone(),
                });
                entry.running = false;
                entry.started_at = None;
                entry.progress_percent = None;
                entry.last_run_at = Some(end);
                entry.last_duration_secs = Some(duration);
                entry.last_outcome = Some(outcome);
                entry.last_output = Some(truncated);
                entry.raw = summary;
            }
            persist_scrub_state(&store).await;
        });

        Ok(())
    }

    /// Get scrub status for a filesystem. Merges the persisted state
    /// (last completion + the engine's view of "running") with a
    /// `pgrep` cross-check so that an engine restart during a scrub
    /// (which orphans the bcachefs child to init) is recorded as
    /// `Failed` rather than leaving the FS forever stuck in "running".
    pub async fn scrub_status(&self, name: &str) -> Result<ScrubStatus, FilesystemError> {
        // Confirm the FS exists in the catalog (this is the only
        // input validation we need — historical scrub state is useful
        // regardless of current mount state, so an operator who
        // temporarily unmounted can still see "Last scrub: ok, 4d ago"
        // rather than an error). The pgrep cross-check below requires
        // a mount point, so we only run it for currently-mounted FSes.
        let fs = self.get(name).await?;

        // Snapshot whatever's persisted. Default = never-scrubbed.
        let mut status = {
            let state = self.scrub_state.lock().await;
            state.get(name).cloned().unwrap_or_else(|| ScrubStatus {
                running: false,
                started_at: None,
                progress_percent: None,
                last_run_at: None,
                last_duration_secs: None,
                last_outcome: None,
                last_output: None,
                raw: "Never scrubbed".into(),
            })
        };

        if status.running {
            // State says running — verify the child still exists. If
            // the engine restarted mid-scrub the child may have died
            // or been re-parented; either way the recorded `running`
            // is stale and we shouldn't lie about it. Cross-check uses
            // the FS's current mount point — when the FS isn't mounted
            // at all there's nothing for bcachefs scrub to be running
            // against, so we treat that as "definitely not alive".
            let alive = if let Some(mp) = fs.mount_point.as_deref() {
                cmd::run_ok("pgrep", &["-fa", "bcachefs scrub"])
                    .await
                    .map(|out| out.lines().any(|l| l.contains(mp)))
                    .unwrap_or(false)
            } else {
                false
            };
            if !alive {
                let end = unix_now_secs();
                let duration = status
                    .started_at
                    .map(|s| (end - s).max(0) as u64)
                    .unwrap_or(0);
                let mut state = self.scrub_state.lock().await;
                let entry = state
                    .entry(name.to_string())
                    .or_insert_with(|| status.clone());
                entry.running = false;
                entry.started_at = None;
                entry.progress_percent = None;
                entry.last_run_at = Some(end);
                entry.last_duration_secs = Some(duration);
                entry.last_outcome = Some(ScrubOutcome::Failed);
                entry.last_output = Some(
                    "engine restarted while scrub was running — the bcachefs child \
                     was lost; restart the scrub if you want a fresh full pass."
                        .into(),
                );
                entry.raw = "Last scrub: failed (engine restart)".into();
                status = entry.clone();
                drop(state);
                persist_scrub_state(&self.scrub_state).await;
            }
        }

        Ok(status)
    }

    /// Start an offline `bcachefs fsck` on a filesystem. `repair=false`
    /// is a read-only dry run (`-n`); `repair=true` auto-repairs (`-y`).
    /// Refuses while mounted — offline fsck needs exclusive access, and
    /// the won't-mount case (#451) is already unmounted. Runs detached
    /// and streams output, mirroring `scrub_start`.
    pub async fn fsck_start(&self, name: &str, repair: bool) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "unmount the filesystem before running fsck (an offline check needs exclusive \
                 access to the member devices)"
                    .to_string(),
            ));
        }
        let devices: Vec<String> = fs.devices.iter().map(|d| d.path.clone()).collect();
        if devices.is_empty() {
            return Err(FilesystemError::CommandFailed(
                "no member devices found to check".to_string(),
            ));
        }
        // Refuse a second concurrent run.
        if self
            .fsck_state
            .lock()
            .await
            .get(name)
            .is_some_and(|s| s.running)
        {
            return Err(FilesystemError::CommandFailed(
                "an fsck is already running on this filesystem".to_string(),
            ));
        }

        let fs_name = name.to_string();
        let now = unix_now_secs();
        {
            let mut state = self.fsck_state.lock().await;
            let entry = state.entry(fs_name.clone()).or_default();
            entry.running = true;
            entry.repair = repair;
            entry.started_at = Some(now);
            entry.progress_percent = None;
        }
        persist_fsck_state(&self.fsck_state).await;

        let store = self.fsck_state.clone();
        info!(
            "Starting fsck ({}) on filesystem '{name}'",
            if repair { "repair" } else { "dry run" }
        );
        tokio::spawn(async move {
            let (outcome, captured) =
                stream_fsck_and_collect(&devices, &fs_name, &store, repair).await;
            let end = unix_now_secs();
            let duration = (end - now).max(0) as u64;
            match outcome {
                FsckOutcome::Clean => info!("fsck on '{fs_name}' completed in {duration}s: clean"),
                FsckOutcome::Errors => warn!(
                    "fsck on '{fs_name}' completed in {duration}s: errors reported (see WebUI for full output)"
                ),
                FsckOutcome::Failed => warn!("fsck on '{fs_name}' failed after {duration}s"),
            }
            let truncated = truncate_tail(&captured, SCRUB_OUTPUT_KEEP_BYTES);
            {
                let mut state = store.lock().await;
                let entry = state.entry(fs_name.clone()).or_default();
                entry.running = false;
                entry.started_at = None;
                entry.progress_percent = None;
                entry.last_run_at = Some(end);
                entry.last_duration_secs = Some(duration);
                entry.last_repair = Some(repair);
                entry.last_outcome = Some(outcome);
                entry.last_output = Some(truncated);
            }
            persist_fsck_state(&store).await;
        });

        Ok(())
    }

    /// Get fsck status for a filesystem, with a `pgrep` cross-check that
    /// records an engine-restart-mid-fsck as `Failed` instead of leaving
    /// it stuck "running" (mirrors `scrub_status`).
    pub async fn fsck_status(&self, name: &str) -> Result<FsckStatus, FilesystemError> {
        // Validate the FS exists; history is useful regardless of mount
        // state, so don't require it mounted.
        let _ = self.get(name).await?;

        let mut status = self
            .fsck_state
            .lock()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();

        if status.running {
            // fsck runs against the member devices (the FS is unmounted),
            // so the cross-check matches on the filesystem name in the
            // command line isn't reliable; match the bcachefs fsck process
            // against any of this FS's device paths instead.
            let fs = self.get(name).await?;
            let devices: Vec<String> = fs.devices.iter().map(|d| d.path.clone()).collect();
            let alive = cmd::run_ok("pgrep", &["-fa", "bcachefs fsck"])
                .await
                .map(|out| {
                    out.lines()
                        .any(|l| devices.iter().any(|d| l.contains(d.as_str())))
                })
                .unwrap_or(false);
            if !alive {
                let end = unix_now_secs();
                let duration = status
                    .started_at
                    .map(|s| (end - s).max(0) as u64)
                    .unwrap_or(0);
                let mut state = self.fsck_state.lock().await;
                let entry = state
                    .entry(name.to_string())
                    .or_insert_with(|| status.clone());
                entry.running = false;
                entry.started_at = None;
                entry.progress_percent = None;
                entry.last_run_at = Some(end);
                entry.last_duration_secs = Some(duration);
                entry.last_repair = Some(status.repair);
                entry.last_outcome = Some(FsckOutcome::Failed);
                entry.last_output = Some(
                    "engine restarted while fsck was running — the bcachefs child was lost; \
                     start the check again."
                        .into(),
                );
                status = entry.clone();
                drop(state);
                persist_fsck_state(&self.fsck_state).await;
            }
        }

        Ok(status)
    }

    /// Get reconcile (background work) status for a filesystem.
    /// `bcachefs reconcile status <mountpoint>`
    pub async fn reconcile_status(&self, name: &str) -> Result<ReconcileStatus, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to check reconcile status".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();

        let raw = cmd::run_ok("bcachefs", &["reconcile", "status", mount_point])
            .await
            .unwrap_or_else(|_| "No reconcile data available".to_string());

        let enabled = self.reconcile_enabled(&fs.uuid).await;

        Ok(ReconcileStatus { raw, enabled })
    }

    /// Read reconcile_enabled from sysfs for a mounted filesystem.
    async fn reconcile_enabled(&self, uuid: &str) -> bool {
        let path = format!("/sys/fs/bcachefs/{uuid}/options/reconcile_enabled");
        tokio::fs::read_to_string(&path)
            .await
            .map(|s| s.trim() != "0")
            .unwrap_or(true)
    }

    /// Enable or disable reconcile on a mounted filesystem via sysfs.
    pub async fn set_reconcile_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to toggle reconcile".to_string(),
            ));
        }
        let path = format!("/sys/fs/bcachefs/{}/options/reconcile_enabled", fs.uuid);
        let val = if enabled { "1" } else { "0" };
        info!("Setting reconcile_enabled={val} on filesystem '{name}'");
        tokio::fs::write(&path, val)
            .await
            .map_err(|e| FilesystemError::CommandFailed(format!("failed to write {path}: {e}")))
    }

    /// Raw output of `bcachefs fs usage <mount>` — space breakdown by data type and device.
    pub async fn bcachefs_usage(&self, name: &str) -> Result<String, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();
        let raw = cmd::run_ok("bcachefs", &["fs", "usage", "-a", "-h", mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;
        Ok(raw)
    }

    pub async fn bcachefs_top(&self, name: &str) -> Result<String, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();
        // Use `script` to provide a PTY so fs top doesn't fail with "No such device"
        // Capture 2 seconds of output to get at least one full frame
        let raw = cmd::run_ok(
            "script",
            &[
                "-qc",
                &format!("timeout 2 bcachefs fs top -h {mount_point}"),
                "/dev/null",
            ],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        // Strip ANSI escapes and extract the last complete frame
        let clean = strip_ansi(&raw);
        // Split on clear-screen artifacts and take the last substantial frame
        let clean_ref = clean.as_str();
        let frames: Vec<&str> = clean_ref.split("\x1b[?1049h").collect();
        let frame = frames.last().unwrap_or(&clean_ref);
        // Clean up: remove carriage returns, control chars, and the header/help lines
        let lines: Vec<&str> = frame
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.is_empty())
            .filter(|l| !l.starts_with("All counters"))
            .filter(|l| !l.starts_with("  perf trace"))
            .filter(|l| !l.starts_with("  q:quit"))
            .collect();
        Ok(lines.join("\n"))
    }

    pub async fn bcachefs_timestats(
        &self,
        name: &str,
    ) -> Result<serde_json::Value, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();
        let raw = cmd::run_ok(
            "bcachefs",
            &["fs", "timestats", "--json", "--once", mount_point],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;
        serde_json::from_str(&raw).map_err(|e| {
            FilesystemError::CommandFailed(format!("failed to parse timestats JSON: {e}"))
        })
    }
}

/// Strip ANSI escape sequences (used for bcachefs raw text output).
#[allow(dead_code)]
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a device usage line like "/dev/sda: 123 used  456 free  789 total"
/// Parse device table row from `bcachefs fs usage` default output.
/// Format: "label (device N):  sdb     rw     53264510976   8912896    0%"
///    or:  "label (device N):  sdb     rw     49.6G         8.50M      0%"  (with -h)
fn parse_device_table_line(line: &str) -> Option<DeviceUsage> {
    let after = line.split("):").nth(1)?.trim();
    let parts: Vec<&str> = after.split_whitespace().collect();
    // parts: [devname, state, size, used, use%]
    if parts.len() < 4 {
        return None;
    }
    let dev_name = parts[0];
    let path = if dev_name.starts_with('/') {
        dev_name.to_string()
    } else {
        format!("/dev/{dev_name}")
    };
    let total = parse_human_bytes(parts[2]).unwrap_or(0);
    let used = parse_human_bytes(parts[3]).unwrap_or(0);
    let free = total.saturating_sub(used);

    Some(DeviceUsage {
        path,
        used_bytes: used,
        free_bytes: free,
        total_bytes: total,
    })
}

/// Extract the first number (byte count) from a summary line.
fn extract_first_bytes(line: &str) -> Option<u64> {
    let after_colon = line.split_once(':')?.1.trim();
    let token = after_colon.split_whitespace().next()?;
    parse_human_bytes(token)
}

/// Validate a bcachefs compression spec before it's interpolated into
/// `bcachefs format --compression=…` or written to the sysfs
/// `compression` / `background_compression` option (#491).
///
/// Accepts `none`, or `lz4` / `zstd` / `gzip` optionally followed by
/// `:<level>`. Levels are bounded to the algorithm's real range so a
/// typo gets a clear message instead of an opaque format/sysfs error:
/// zstd 1–22, gzip 1–9. lz4 has no tunable level in bcachefs, so a
/// level on lz4 (or none) is rejected. Pure; unit-tested.
fn validate_compression(spec: &str) -> Result<(), String> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "none" {
        return Ok(());
    }
    let (algo, level) = match spec.split_once(':') {
        Some((a, l)) => (a, Some(l)),
        None => (spec, None),
    };
    let max_level = match algo {
        "lz4" => None, // valid algorithm, but no level knob
        "zstd" => Some(22u32),
        "gzip" => Some(9u32),
        other => return Err(format!("unknown compression algorithm '{other}'")),
    };
    if let Some(level) = level {
        let Some(max) = max_level else {
            return Err(format!("{algo} does not take a compression level"));
        };
        let n: u32 = level
            .parse()
            .map_err(|_| format!("compression level '{level}' is not a number"))?;
        if n < 1 || n > max {
            return Err(format!(
                "{algo} compression level must be between 1 and {max} (got {n})"
            ));
        }
    }
    Ok(())
}

/// Parse human-readable byte strings like "109.8M", "2.3G", "512K", "1024".
fn parse_human_bytes(s: &str) -> Option<u64> {
    // Try plain integer first
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Split into numeric part and suffix
    let (num_str, suffix) = match s.find(|c: char| c.is_alphabetic()) {
        Some(i) => (&s[..i], &s[i..]),
        None => return s.parse::<f64>().ok().map(|n| n as u64),
    };
    let num: f64 = num_str.parse().ok()?;
    let multiplier: f64 = match suffix.to_uppercase().as_str() {
        "B" => 1.0,
        "K" | "KIB" | "KB" => 1024.0,
        "M" | "MIB" | "MB" => 1024.0 * 1024.0,
        "G" | "GIB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TIB" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * multiplier) as u64)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BlockDevice {
    /// Absolute path of the block device (e.g. `/dev/sda`).
    pub path: String,
    /// Total capacity in bytes.
    pub size_bytes: u64,
    /// lsblk device type: `disk` or `part`.
    pub dev_type: String,
    /// Current mount point, if mounted.
    pub mount_point: Option<String>,
    /// Filesystem type detected on the device (e.g. `bcachefs`, `ext4`).
    pub fs_type: Option<String>,
    /// Filesystem UUID from lsblk — for bcachefs members this is the
    /// *external* (whole-filesystem) UUID, so a candidate disk can be
    /// matched against an existing pool's `Filesystem.uuid` to tell an
    /// offline/former member apart from a foreign disk (#472).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_uuid: Option<String>,
    /// Whether the device is currently in use (mounted, in a filesystem, or has partitions in use).
    pub in_use: bool,
    /// Whether the underlying disk spins (false for NVMe/SSD, true for HDD).
    pub rotational: bool,
    /// Device speed class: "nvme", "ssd", or "hdd".
    pub device_class: String,
    /// Drive model from lsblk (e.g. "Samsung SSD 970 EVO Plus 1TB"). None
    /// for partitions and for virtual disks that don't expose a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Drive serial from lsblk. None for partitions and virtual disks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// Drive vendor from lsblk (e.g. "ATA", "NVMe").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Transport bus from lsblk (e.g. "sata", "nvme", "usb").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

/// Get the largest contiguous free space on a partitioned disk using sgdisk.
async fn get_disk_free_space(disk_path: &str) -> Result<u64, String> {
    // sgdisk --print outputs a table with partition info and a summary line:
    // "Total free space is X sectors (Y GiB)"
    // Alternatively, use parted for a cleaner parse.
    let output = cmd::run_ok("sgdisk", &["--print", disk_path])
        .await
        .map_err(|e| format!("sgdisk failed: {e}"))?;

    // Parse "Total free space is NNNN sectors" line
    for line in output.lines() {
        let trimmed = line.trim().to_lowercase();
        if trimmed.starts_with("total free space is") {
            // "Total free space is 195126272 sectors (93.0 GiB)"
            let sectors_str = trimmed
                .strip_prefix("total free space is ")
                .and_then(|s| s.split_whitespace().next());
            if let Some(s) = sectors_str
                && let Ok(sectors) = s.parse::<u64>()
            {
                // Sectors are typically 512 bytes; sgdisk uses logical sector size.
                // Parse sector size from sgdisk output: "Sector size (logical): NNN bytes"
                let sector_size = output
                    .lines()
                    .find(|l| l.to_lowercase().contains("sector size (logical)"))
                    .and_then(|l| {
                        l.split_whitespace()
                            .filter_map(|w| w.parse::<u64>().ok())
                            .next_back()
                    })
                    .unwrap_or(512);
                return Ok(sectors * sector_size);
            }
        }
    }
    Ok(0)
}

/// Create a new GPT partition in the free space of a disk.
/// Returns the path of the new partition (e.g. `/dev/sda3`).
pub async fn create_partition_on_free_space(disk_path: &str) -> Result<String, FilesystemError> {
    // Use sgdisk to create a new partition using the largest available block
    cmd::run_ok("sgdisk", &["--largest-new=0", disk_path])
        .await
        .map_err(FilesystemError::CommandFailed)?;

    // Re-read partition table
    let _ = cmd::run_ok("partprobe", &[disk_path]).await;
    // Brief settle time for the kernel to create the device node
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Find the new partition — it's the highest-numbered one
    let output = cmd::run_ok("lsblk", &["-Jno", "NAME,TYPE", disk_path])
        .await
        .map_err(FilesystemError::CommandFailed)?;
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap_or_default();
    let mut last_part = String::new();
    if let Some(devs) = parsed.get("blockdevices").and_then(|v| v.as_array()) {
        for dev in devs {
            if let Some(children) = dev.get("children").and_then(|v| v.as_array()) {
                for child in children {
                    if child.get("type").and_then(|v| v.as_str()) == Some("part")
                        && let Some(name) = child.get("name").and_then(|v| v.as_str())
                    {
                        last_part = format!("/dev/{name}");
                    }
                }
            }
        }
    }

    if last_part.is_empty() {
        return Err(FilesystemError::CommandFailed(
            "failed to find new partition after creation".to_string(),
        ));
    }

    // Wipe any stale filesystem signatures inherited from the disk's
    // previously unpartitioned space (e.g. old ZFS/LVM metadata).
    let _ = cmd::run_ok("wipefs", &["-a", &last_part]).await;

    info!("Created partition {last_part} on {disk_path}");
    Ok(last_part)
}

/// Read per-device info (labels, durability) for a mounted bcachefs filesystem.
/// Uses `bcachefs show-super` on the first device to extract member info.
async fn read_fs_devices(uuid: &str, device_paths: &[String]) -> Vec<FilesystemDevice> {
    let first_dev = match device_paths.first() {
        Some(d) => d.as_str(),
        None => return Vec::new(),
    };

    let member_info = cmd::run_ok("bcachefs", &["show-super", "-f", "members_v2", first_dev])
        .await
        .unwrap_or_default();

    // The authoritative member set for a mounted pool (incl. missing
    // members — phantom dev-N with no block device). Empty when unmounted.
    let sysfs_members = read_device_sysfs(uuid).await;
    let sysfs_by_path: HashMap<&str, &DeviceSysfs> = sysfs_members
        .iter()
        .filter_map(|m| m.path.as_deref().map(|p| (p, m)))
        .collect();

    // show-super -f members_v2 output comes in two formats:
    //
    // Single-line (older):
    //   Device 0 (label ssd.fast):  /dev/sda  ...  durability: 1  state: rw
    //
    // Multi-line (newer):
    //   Device 0:       /dev/sda
    //           Label:          ssd.fast
    //           State:          rw
    //           Durability:     1
    //
    // Split output into per-device blocks by "Device N:" markers, then scan
    // each block for the info we need regardless of which format is used.

    // Build blocks: each block is all lines from one "Device N:" until the next.
    let lines: Vec<&str> = member_info.lines().collect();
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in &lines {
        let trimmed = line.trim();
        // A new device block starts when a line begins with "Device " followed by a digit.
        if trimmed.starts_with("Device ")
            && trimmed.chars().nth(7).is_some_and(|c| c.is_ascii_digit())
            && !current.is_empty()
        {
            blocks.push(current.clone());
            current.clear();
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let extract_value = |block: &[&str], key: &str| -> Option<String> {
        for line in block {
            let lower = line.to_lowercase();
            if let Some(pos) = lower.find(key) {
                let rest = &line[pos + key.len()..];
                let rest = rest.trim_start_matches([':', ' ', '\t']);
                // Take first token, strip surrounding punctuation
                if let Some(tok) = rest.split_whitespace().next() {
                    let tok =
                        tok.trim_matches(|c: char| c == '(' || c == ')' || c == ',' || c == ';');
                    if !tok.is_empty() && tok != "none" {
                        return Some(tok.to_string());
                    }
                }
            }
        }
        None
    };

    // bcachefs's own `Rotational` flag per member slot, from the
    // superblock (#501). Keyed by member index, not device path, so it
    // stays correct across a remove/re-add reshuffle and means the same
    // persisted thing whether or not the pool is mounted — and so the
    // value is consistent with the sysfs-vs-show-super divergence the
    // latch bug (#594) can cause: we always report the persisted
    // superblock value here.
    let rotational_by_slot: std::collections::HashMap<u32, bool> = blocks
        .iter()
        .filter_map(|b| {
            let idx = b.first().and_then(|h| parse_device_index(h))?;
            let rot = extract_value(b, "rotational")?;
            Some((idx, rot == "1" || rot == "true"))
        })
        .collect();
    let rotational_of = |slot: Option<u32>| slot.and_then(|i| rotational_by_slot.get(&i).copied());

    let mut devices: Vec<FilesystemDevice> = Vec::new();
    // Phantom slots already represented by a bound /proc/mounts row,
    // skipped by the missing-member loop below.
    let mut bound_slots: std::collections::HashSet<Option<u32>> = std::collections::HashSet::new();

    for dev_path in device_paths {
        // Mounted pool: sysfs is the authoritative, correctly-mapped
        // source. It's keyed by the kernel's live `block` symlink, so it
        // stays correct after a remove/re-add reshuffle, and it doesn't
        // need the passphrase on encrypted pools. show-super, by contrast,
        // reports the device PATHS stored in the superblock — which go
        // stale on reshuffle and made labels/slots land on the wrong row
        // (#455). So prefer sysfs; only fall back to show-super when the
        // filesystem isn't mounted (no sysfs tree).
        if let Some(sy) = sysfs_by_path.get(dev_path.as_str()) {
            devices.push(FilesystemDevice {
                path: dev_path.clone(),
                label: sy.label.clone(),
                durability: sy.durability,
                state: sy.state.clone(),
                data_allowed: sy.data_allowed.clone(),
                has_data: sy.has_data.clone(),
                discard: sy.discard,
                rotational: rotational_of(sy.member_index),
                read_errors: sy.read_errors,
                write_errors: sy.write_errors,
                checksum_errors: sy.checksum_errors,
                member_index: sy.member_index,
                uuid: sy.uuid.clone(),
                missing: None,
            });
            continue;
        }

        // Unmounted: fall back to show-super's per-device blocks, matched
        // by device path (best-effort; sysfs is absent here).
        let dev_short = dev_path.trim_start_matches("/dev/");
        let block = blocks.iter().find(|b| {
            b.iter()
                .any(|l| l.contains(dev_path.as_str()) || l.contains(dev_short))
        });
        let (label, durability, state, data_allowed, has_data, discard) = if let Some(block) = block
        {
            let label = extract_value(block, "label");
            let durability = extract_value(block, "durability").and_then(|s| s.parse().ok());
            let state = extract_value(block, "state");
            let data_allowed = extract_value(block, "data allowed");
            let has_data = extract_value(block, "has data");
            let discard = extract_value(block, "discard").map(|s| s == "1" || s == "true");
            (label, durability, state, data_allowed, has_data, discard)
        } else {
            (None, None, None, None, None, None)
        };
        let member_index = block
            .and_then(|b| b.first())
            .and_then(|hdr| parse_device_index(hdr));

        // On a mounted pool every *attached* member is in sysfs_by_path,
        // so reaching here with a non-empty sysfs tree means this
        // /proc/mounts path dropped out after mount. Its slot lives on as
        // a phantom dev-N — bind the two into one row carrying the real
        // path (so the re-attach affordance has a device to act on) and
        // the phantom's live sysfs fields, flagged missing, instead of a
        // stale-`rw` superblock row plus a separate "(missing dev-N)"
        // placeholder for the same member (#472).
        if !sysfs_members.is_empty() {
            let phantom = member_index.and_then(|idx| {
                sysfs_members
                    .iter()
                    .find(|m| m.path.is_none() && m.member_index == Some(idx))
            });
            if let Some(m) = phantom {
                bound_slots.insert(m.member_index);
                devices.push(FilesystemDevice {
                    path: dev_path.clone(),
                    label: m.label.clone(),
                    durability: m.durability,
                    state: m.state.clone(),
                    data_allowed: m.data_allowed.clone(),
                    has_data: m.has_data.clone(),
                    discard: m.discard,
                    rotational: rotational_of(m.member_index),
                    read_errors: m.read_errors,
                    write_errors: m.write_errors,
                    checksum_errors: m.checksum_errors,
                    member_index: m.member_index,
                    uuid: m.uuid.clone(),
                    missing: Some(true),
                });
                continue;
            }
        }

        devices.push(FilesystemDevice {
            path: dev_path.clone(),
            label,
            durability,
            state,
            data_allowed,
            has_data,
            discard,
            rotational: rotational_of(member_index),
            read_errors: None,
            write_errors: None,
            checksum_errors: None,
            member_index,
            uuid: None,
            // No phantom slot matched, but on a mounted pool this device
            // is still detached — don't pretend it's a healthy member.
            missing: if sysfs_members.is_empty() {
                None
            } else {
                Some(true)
            },
        });
    }

    // Missing members (#466): superblock still lists them but their block
    // device is gone (pulled/dead) — surfaced as phantom dev-N in sysfs
    // with no resolvable `block` symlink. They're not in `device_paths`
    // (which comes from /proc/mounts = present devices), so add them here
    // so the operator can see the dead member and force-remove it.
    for m in &sysfs_members {
        if m.path.is_some() || bound_slots.contains(&m.member_index) {
            continue;
        }
        let slot = m
            .member_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "?".to_string());
        devices.push(FilesystemDevice {
            // Synthetic, stable per-slot key (the row has no real /dev node).
            path: format!("(missing dev-{slot})"),
            label: m.label.clone(),
            durability: m.durability,
            state: m.state.clone(),
            data_allowed: m.data_allowed.clone(),
            has_data: m.has_data.clone(),
            discard: m.discard,
            rotational: rotational_of(m.member_index),
            read_errors: m.read_errors,
            write_errors: m.write_errors,
            checksum_errors: m.checksum_errors,
            member_index: m.member_index,
            uuid: m.uuid.clone(),
            missing: Some(true),
        });
    }

    devices
}

/// Parse `/sys/fs/bcachefs/<uuid>/dev-N/io_errors`, returning the
/// cumulative `(read, write, checksum)` counts from the "since
/// filesystem creation" block. A later "since … ago" block reports
/// counts since the last reset and is ignored. Pure; unit-tested.
fn parse_io_errors(s: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let (mut read, mut write, mut checksum) = (None, None, None);
    for line in s.lines() {
        let l = line.trim();
        // Once we've started filling the first block, a second
        // "IO errors since …" header marks the reset block — stop.
        if l.starts_with("IO errors since")
            && (read.is_some() || write.is_some() || checksum.is_some())
        {
            break;
        }
        let val = |key: &str| -> Option<u64> {
            l.strip_prefix(key)
                .map(|r| r.trim_start_matches([':', ' ', '\t']))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|t| t.parse().ok())
        };
        // "checksum:0" has no space; "read:    0" does — `val` handles both.
        if read.is_none() && l.starts_with("read") {
            read = val("read");
        } else if write.is_none() && l.starts_with("write") {
            write = val("write");
        } else if checksum.is_none() && l.starts_with("checksum") {
            checksum = val("checksum");
        }
    }
    (read, write, checksum)
}

/// Map each member device path → its cumulative `(read, write, checksum)`
/// IO error counts, by scanning `/sys/fs/bcachefs/<uuid>/dev-*/`. Empty
/// when the filesystem isn't mounted (the sysfs tree is absent).
/// Parse the bcachefs member index out of a show-super device block
/// header like `Device 0:   /dev/sda` → `Some(0)`. Tolerates the
/// `Device 0 (label ...):` single-line variant too.
fn parse_device_index(header: &str) -> Option<u32> {
    header
        .trim()
        .strip_prefix("Device ")
        .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
        .filter(|d| !d.is_empty())
        .and_then(|d| d.parse::<u32>().ok())
}

/// One member of a mounted bcachefs filesystem, read from
/// `/sys/fs/bcachefs/<fs-uuid>/dev-N/`. Includes *missing* members
/// (phantom `dev-N` whose `block` symlink no longer resolves), which is
/// how a pulled/dead disk is surfaced — `path` is `None` for those.
#[derive(Default, Clone)]
struct DeviceSysfs {
    /// `/dev/<name>` from the live `block` symlink; `None` for a missing
    /// (pulled/dead) member whose block device is gone.
    path: Option<String>,
    read_errors: Option<u64>,
    write_errors: Option<u64>,
    checksum_errors: Option<u64>,
    /// Stable per-device bcachefs UUID (`dev-N/uuid`).
    uuid: Option<String>,
    /// Member slot — the `N` in `dev-N`.
    member_index: Option<u32>,
    label: Option<String>,
    state: Option<String>,
    durability: Option<u32>,
    data_allowed: Option<String>,
    has_data: Option<String>,
    discard: Option<bool>,
}

/// Read one sysfs attribute file, trimmed; `None` if absent/empty or the
/// bcachefs "unset" sentinel `(none)`.
async fn read_sysfs_attr(dir: &str, attr: &str) -> Option<String> {
    tokio::fs::read_to_string(format!("{dir}/{attr}"))
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "(none)" && s != "none")
}

async fn read_device_sysfs(uuid: &str) -> Vec<DeviceSysfs> {
    let base = format!("/sys/fs/bcachefs/{uuid}");
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(&base).await {
        Ok(r) => r,
        Err(_) => return out,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(idx) = name.strip_prefix("dev-") else {
            continue;
        };
        let member_index = idx.parse::<u32>().ok();
        let dir = format!("{base}/{name}");
        // dev-N/block is a symlink whose basename is the kernel device name.
        // This is the kernel's *live* mapping, so it stays correct across a
        // remove/re-add reshuffle — unlike the device paths recorded in the
        // superblock that `show-super` reports. A *missing* member (pulled
        // or dead disk) still has its dev-N dir but the block symlink no
        // longer resolves — we keep it with `path: None` so the UI can show
        // it and offer a force-remove (#466).
        let path = match tokio::fs::read_link(format!("{dir}/block")).await {
            Ok(t) => t
                .file_name()
                .map(|s| format!("/dev/{}", s.to_string_lossy())),
            Err(_) => None,
        };
        // A pulled disk can leave the symlink *dangling* (it reads OK but
        // its target /dev node is gone) — treat that as missing too, so a
        // pulled drive is flagged even when the kernel didn't drop the link.
        let path = match path {
            Some(p) if tokio::fs::metadata(&p).await.is_ok() => Some(p),
            _ => None,
        };
        let (read_errors, write_errors, checksum_errors) =
            match tokio::fs::read_to_string(format!("{dir}/io_errors")).await {
                Ok(s) => parse_io_errors(&s),
                Err(_) => (None, None, None),
            };
        // `state` is the bracketed-enum form: `[rw] ro evacuating spare`.
        let state = read_sysfs_attr(&dir, "state")
            .await
            .map(|s| parse_bcachefs_opt(&s));
        out.push(DeviceSysfs {
            path,
            read_errors,
            write_errors,
            checksum_errors,
            uuid: read_sysfs_attr(&dir, "uuid").await,
            member_index,
            label: read_sysfs_attr(&dir, "label").await,
            state,
            durability: read_sysfs_attr(&dir, "durability")
                .await
                .and_then(|s| s.parse().ok()),
            data_allowed: read_sysfs_attr(&dir, "data_allowed").await,
            has_data: read_sysfs_attr(&dir, "has_data").await,
            discard: read_sysfs_attr(&dir, "discard")
                .await
                .map(|s| s == "1" || s == "true"),
        });
    }
    out
}

/// Read filesystem options from sysfs for a mounted bcachefs filesystem.
/// Options live at /sys/fs/bcachefs/<uuid>/options/<option_name>
/// Extract the selected value from bcachefs option strings.
/// bcachefs sysfs/show-super use `[selected] opt1 opt2` for enum options.
/// For plain values (e.g. `zstd`), returns the value as-is.
fn parse_bcachefs_opt(val: &str) -> String {
    if val.contains('[') {
        val.split('[')
            .nth(1)
            .and_then(|s| s.split(']').next())
            .unwrap_or(val)
            .trim()
            .to_string()
    } else {
        val.to_string()
    }
}

async fn read_fs_options_sysfs(uuid: &str) -> FilesystemOptions {
    if uuid.is_empty() {
        return FilesystemOptions::default();
    }

    let base = format!("/sys/fs/bcachefs/{uuid}/options");

    async fn read_opt(base: &str, name: &str) -> Option<String> {
        let path = format!("{base}/{name}");
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => {
                let v = parse_bcachefs_opt(s.trim());
                if v.is_empty() || v == "none" || v == "(none)" {
                    None
                } else {
                    Some(v)
                }
            }
            Err(_) => None,
        }
    }

    async fn read_opt_u32(base: &str, name: &str) -> Option<u32> {
        read_opt(base, name).await.and_then(|s| s.parse().ok())
    }

    async fn read_opt_bool(base: &str, name: &str) -> Option<bool> {
        read_opt(base, name).await.map(|s| s == "1" || s == "true")
    }

    FilesystemOptions {
        compression: read_opt(&base, "compression").await,
        background_compression: read_opt(&base, "background_compression").await,
        data_replicas: read_opt_u32(&base, "data_replicas").await,
        metadata_replicas: read_opt_u32(&base, "metadata_replicas").await,
        data_checksum: read_opt(&base, "data_checksum").await,
        metadata_checksum: read_opt(&base, "metadata_checksum").await,
        foreground_target: read_opt(&base, "foreground_target").await,
        background_target: read_opt(&base, "background_target").await,
        promote_target: read_opt(&base, "promote_target").await,
        metadata_target: read_opt(&base, "metadata_target").await,
        erasure_code: read_opt_bool(&base, "erasure_code").await,
        encrypted: read_opt_bool(&base, "encrypted").await,
        error_action: read_opt(&base, "errors").await,
        version_upgrade: read_opt(&base, "version_upgrade").await,
        locked: None,
        key_stored: None,
        degraded: None,
        verbose: None,
        fsck: None,
        journal_flush_disabled: None,
        journal_flush_delay: read_opt_u32(&base, "journal_flush_delay").await,
        io_scheduler: None, // read per-device, not from bcachefs sysfs
        move_ios_in_flight: read_opt_u32(&base, "move_ios_in_flight").await,
        move_bytes_in_flight: read_opt(&base, "move_bytes_in_flight").await,
    }
}

/// Read filesystem options from `bcachefs show-super` for an unmounted filesystem.
async fn read_fs_options_show_super(device: Option<&str>) -> FilesystemOptions {
    let dev = match device {
        Some(d) => d,
        None => return FilesystemOptions::default(),
    };

    let output = match cmd::run_ok("bcachefs", &["show-super", dev]).await {
        Ok(o) => o,
        Err(e) => {
            // `show-super` failure means the WebUI's "Options" panel
            // for this filesystem will display all defaults — masking
            // whatever the real on-disk options are. Worth logging
            // so the operator can correlate the missing data with a
            // bcachefs tools / permission issue.
            warn!("bcachefs show-super {dev} failed: {e}; reporting defaults");
            return FilesystemOptions::default();
        }
    };

    let mut opts = FilesystemOptions::default();

    for line in output.lines() {
        let line = line.trim();
        // show-super outputs lines like "Option:  value" or "Option          value"
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let val = parse_bcachefs_opt(val.trim());
            if val.is_empty() || val == "none" || val == "(none)" {
                continue;
            }
            match key.as_str() {
                "compression" => opts.compression = Some(val),
                "background_compression" => opts.background_compression = Some(val),
                "data_replicas" => opts.data_replicas = val.parse().ok(),
                "metadata_replicas" => opts.metadata_replicas = val.parse().ok(),
                "data_checksum" => opts.data_checksum = Some(val),
                "metadata_checksum" => opts.metadata_checksum = Some(val),
                "foreground_target" => opts.foreground_target = Some(val),
                "background_target" => opts.background_target = Some(val),
                "promote_target" => opts.promote_target = Some(val),
                "metadata_target" => opts.metadata_target = Some(val),
                "erasure_code" => opts.erasure_code = Some(val == "1" || val == "true"),
                "encrypted" => opts.encrypted = Some(val == "1" || val == "true" || val == "yes"),
                "errors" => opts.error_action = Some(val),
                "version_upgrade" => opts.version_upgrade = Some(val),
                _ => {}
            }
        }
    }

    opts
}

/// One row pulled from `/proc/mounts` for a bcachefs filesystem.
/// `devices` is the colon-separated source split into individual
/// device paths (`/dev/sda:/dev/sdb` → `["/dev/sda", "/dev/sdb"]`),
/// since multi-device bcachefs filesystems are first-class.
#[derive(Debug, PartialEq, Eq)]
struct ProcMountsBcachefs {
    devices: Vec<String>,
    mount_point: String,
}

/// Parse one `/proc/mounts` line into a bcachefs mount entry, or
/// `None` for non-bcachefs or malformed rows. The kernel format is
/// fixed (man proc(5): `device mount_point fstype options dump pass`),
/// so this stays simple — but naming the fields keeps the call site
/// readable and gives us a test seam for any future regression.
fn parse_bcachefs_mount_line(line: &str) -> Option<ProcMountsBcachefs> {
    let mut fields = line.split_whitespace();
    let device = fields.next()?;
    let mount_point = fields.next()?;
    let fstype = fields.next()?;
    if fstype != "bcachefs" {
        return None;
    }
    Some(ProcMountsBcachefs {
        devices: device.split(':').map(String::from).collect(),
        mount_point: mount_point.to_string(),
    })
}

/// Parse /proc/mounts for bcachefs entries.
/// Returns map of mount_point -> list of devices.
async fn read_bcachefs_mounts() -> Result<HashMap<String, Vec<String>>, FilesystemError> {
    let content = tokio::fs::read_to_string("/proc/mounts")
        .await
        .unwrap_or_default();
    let mounts = content
        .lines()
        .filter_map(parse_bcachefs_mount_line)
        .map(|m| (m.mount_point, m.devices))
        .collect();
    Ok(mounts)
}

/// Get the bcachefs UUID for a device.
/// Tries blkid first (works when unmounted), falls back to lsblk (works when mounted).
/// bcachefs 1.38+ can make blkid fail on mounted devices.
async fn get_fs_uuid(device: &str) -> Option<String> {
    // Try blkid first
    if let Ok(output) = cmd::run_ok("blkid", &["-s", "UUID", "-o", "value", device]).await {
        let uuid = output.trim().to_string();
        if !uuid.is_empty() {
            return Some(uuid);
        }
    }

    // Fallback: lsblk (works on mounted bcachefs 1.38+)
    if let Ok(output) = cmd::run_ok("lsblk", &["-no", "UUID", device]).await {
        let uuid = output.trim().to_string();
        if !uuid.is_empty() {
            return Some(uuid);
        }
    }

    None
}

/// Get filesystem usage via statvfs-style info from `df`
async fn get_mount_usage(mount_point: &str) -> Option<(u64, u64, u64)> {
    let output = match cmd::run_ok("df", &["-B1", "--output=size,used,avail", mount_point]).await {
        Ok(o) => o,
        Err(e) => {
            // Reporting all-zeros to capacity dashboards on a real query
            // failure makes the UI lie ("disk empty!" when it's actually
            // inaccessible). Log so a "0/0/0" reading can be matched to
            // the underlying df failure.
            warn!("df --output=size,used,avail {mount_point} failed: {e}");
            return None;
        }
    };

    // Skip header line, parse second line.
    let Some(line) = output.lines().nth(1) else {
        warn!(
            "df output for {mount_point} had no second line — got: {:?}",
            output
        );
        return None;
    };
    let nums: Vec<u64> = line
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() == 3 {
        Some((nums[0], nums[1], nums[2]))
    } else {
        warn!(
            "df output for {mount_point} didn't parse as 3 u64s — got: {:?}",
            line
        );
        None
    }
}

/// Check if a device already has a bcachefs filesystem
async fn is_device_bcachefs(device: &str) -> bool {
    cmd::run_ok("blkid", &["-s", "TYPE", "-o", "value", device])
        .await
        .map(|s| s.trim() == "bcachefs")
        .unwrap_or(false)
}

/// Discover unmounted bcachefs filesystems via blkid.
/// Returns Vec of (uuid, label, devices) for filesystems not in seen_uuids.
/// Look up a filesystem name by UUID in the persisted fs-state.json.
fn find_fs_name_by_uuid(state: &FsState, uuid: &str) -> Option<String> {
    for (name, opts) in state {
        if opts.uuid.as_deref() == Some(uuid) {
            return Some(name.clone());
        }
    }
    None
}

async fn discover_unmounted_bcachefs(
    seen_uuids: &std::collections::HashSet<String>,
) -> Vec<(String, String, Vec<String>)> {
    let output = match cmd::run_ok("blkid", &["-t", "TYPE=bcachefs", "-o", "export"]).await {
        Ok(o) => o,
        Err(e) => {
            // blkid failure means we'll silently miss every unmounted
            // bcachefs filesystem on the box. The WebUI's "import"
            // flow won't see them. Loud log so the operator notices.
            warn!("blkid failed: {e}; unmounted bcachefs filesystems will not be discovered");
            return Vec::new();
        }
    };

    // Parse blkid export format: blocks separated by blank lines
    // Each block has KEY=VALUE lines
    let mut results: HashMap<String, (String, Vec<String>)> = HashMap::new(); // uuid -> (label, devices)

    for block in output.split("\n\n") {
        let mut devname = String::new();
        let mut uuid = String::new();
        let mut label = String::new();

        for line in block.lines() {
            if let Some(val) = line.strip_prefix("DEVNAME=") {
                devname = val.to_string();
            } else if let Some(val) = line.strip_prefix("UUID=") {
                uuid = val.to_string();
            } else if let Some(val) = line.strip_prefix("LABEL_SUB=") {
                label = val.to_string();
            }
        }

        if uuid.is_empty() || devname.is_empty() || seen_uuids.contains(&uuid) {
            continue;
        }

        let entry = results
            .entry(uuid.clone())
            .or_insert_with(|| (label.clone(), Vec::new()));
        if !label.is_empty() && entry.0.is_empty() {
            entry.0 = label;
        }
        entry.1.push(devname);
    }

    results
        .into_iter()
        .map(|(uuid, (label, devices))| (uuid, label, devices))
        .collect()
}

// ── Filesystem mount state persistence ────────────────────────────────

/// Track which filesystems should be mounted across reboots
/// Per-filesystem mount state, persisted across reboots.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FsMountOptions {
    /// bcachefs filesystem UUID — used to verify identity on restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
    /// Device paths that were part of the filesystem at last mount.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    devices: Vec<String>,
    /// Whether the filesystem should be auto-mounted at next boot.
    /// `Some(true)` / missing = auto-mount (existing behaviour preserved
    /// for state files that pre-date this field). `Some(false)` = the
    /// operator unmounted it deliberately and `restore_mounts` should
    /// leave it alone. Previously the unmount path removed the entry
    /// entirely, which also wiped every tuned mount option
    /// (`encrypted`, `compression`, `journal_flush_delay`, …) — so
    /// the next mount started from `FsMountOptions::default()` and
    /// silently lost the user's config. Tracking unmount as a flag
    /// instead lets us preserve those options across an unmount cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mounted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version_upgrade: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    degraded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verbose: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fsck: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_flush_disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_flush_delay: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    io_scheduler: Option<String>,
}

/// Filesystem state: maps fs name → mount options.
type FsState = HashMap<String, FsMountOptions>;

async fn save_fs_mounted_with_opts(fs_name: &str, mut opts: FsMountOptions) {
    // Always flip mounted=true here so callers can't forget — the
    // function name promises "mounted state recorded," and the only
    // way next boot's `restore_mounts` knows to mount this FS is the
    // flag being true (or absent).
    opts.mounted = Some(true);
    let mut state = load_fs_state().await;
    state.insert(fs_name.to_string(), opts);
    if let Err(e) = save_fs_state(&state).await {
        // The mount itself worked — Linux has the mount in its table —
        // but next boot won't know to remount this fs. Log so the user
        // can match a "filesystem isn't mounted after reboot" report
        // to the persistence error.
        warn!("save_fs_state(mounted: {fs_name}) failed: {e}");
    }
}

/// Mark a filesystem as unmounted without losing its tuned mount
/// options. Sets `mounted: Some(false)` so `restore_mounts` skips it
/// at next boot; preserves `encrypted`, `compression`, `journal_*`,
/// `io_scheduler`, etc. so the next manual `mount` doesn't start
/// from defaults. Pre-fix this function removed the entry entirely,
/// which silently wiped the operator's config (and produced the
/// "encrypted=None at boot → systemd-ask-password deadlock" reported
/// on 10.10.10.71 after a passing unmount/mount cycle).
async fn save_fs_unmounted(fs_name: &str) {
    let mut state = load_fs_state().await;
    let opts = state.entry(fs_name.to_string()).or_default();
    opts.mounted = Some(false);
    if let Err(e) = save_fs_state(&state).await {
        warn!("save_fs_state(unmounted: {fs_name}) failed: {e}");
    }
}

/// Forget a filesystem entirely — remove its entry from the state
/// file. Used by `destroy`, where the underlying bcachefs is being
/// wiped: keeping a stale entry would have `restore_mounts` waiting
/// 60 s for the now-gone devices to reappear on every boot.
async fn forget_fs(fs_name: &str) {
    let mut state = load_fs_state().await;
    if state.remove(fs_name).is_some()
        && let Err(e) = save_fs_state(&state).await
    {
        warn!("save_fs_state(forget: {fs_name}) failed: {e}");
    }
}

async fn load_fs_state() -> FsState {
    let content = match tokio::fs::read_to_string(FS_STATE_PATH).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FsState::new(),
        Err(e) => {
            // A non-NotFound read error means we *might* be silently
            // resetting persisted mount state — log so a "filesystems
            // didn't auto-mount" report can be matched to the real
            // cause (permissions, IO error, etc.).
            warn!("read {FS_STATE_PATH} failed: {e} — using empty state");
            return FsState::new();
        }
    };
    match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "parse {FS_STATE_PATH} failed: {e} — file may be corrupt; \
                 using empty state (mount-option overrides will be lost)"
            );
            FsState::new()
        }
    }
}

async fn save_fs_state(state: &FsState) -> Result<(), FilesystemError> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| FilesystemError::CommandFailed(e.to_string()))?;
    tokio::fs::write(FS_STATE_PATH, json).await?;
    Ok(())
}

/// Persist the in-memory scrub state map to disk. Best-effort: a
/// write failure is logged but doesn't abort the caller (the in-memory
/// state is still authoritative for the current engine lifetime).
async fn persist_scrub_state(store: &ScrubStateMap) {
    let snapshot = store.lock().await.clone();
    let json = match serde_json::to_string_pretty(&snapshot) {
        Ok(s) => s,
        Err(e) => {
            warn!("serialize scrub state failed: {e}");
            return;
        }
    };
    if let Err(e) = tokio::fs::write(SCRUB_STATE_PATH, json).await {
        warn!("write {SCRUB_STATE_PATH} failed: {e}");
    }
}

async fn persist_mount_state(store: &MountStateMap) {
    let snapshot = store.lock().await.clone();
    let json = match serde_json::to_string_pretty(&snapshot) {
        Ok(s) => s,
        Err(e) => {
            warn!("serialize mount state failed: {e}");
            return;
        }
    };
    if let Err(e) = tokio::fs::write(MOUNT_STATE_PATH, json).await {
        warn!("write {MOUNT_STATE_PATH} failed: {e}");
    }
}

async fn persist_fsck_state(store: &FsckStateMap) {
    let snapshot = store.lock().await.clone();
    let json = match serde_json::to_string_pretty(&snapshot) {
        Ok(s) => s,
        Err(e) => {
            warn!("serialize fsck state failed: {e}");
            return;
        }
    };
    if let Err(e) = tokio::fs::write(FSCK_STATE_PATH, json).await {
        warn!("write {FSCK_STATE_PATH} failed: {e}");
    }
}

/// Pull `key:`'s first value token out of a `show-super` device block.
/// Mirrors the local extractor in `read_fs_devices` (kept separate so
/// `parse_members` doesn't depend on that function's internals).
fn extract_member_value(block: &[&str], key: &str) -> Option<String> {
    for line in block {
        let lower = line.to_lowercase();
        if let Some(pos) = lower.find(key) {
            let rest = &line[pos + key.len()..];
            let rest = rest.trim_start_matches([':', ' ', '\t']);
            if let Some(tok) = rest.split_whitespace().next() {
                let tok = tok.trim_matches(|c: char| c == '(' || c == ')' || c == ',' || c == ';');
                if !tok.is_empty() && tok != "none" {
                    return Some(tok.to_string());
                }
            }
        }
    }
    None
}

/// Parse `bcachefs show-super -f members_v2` into one [`MemberInfo`] per
/// `Device N:` block. Tolerant of both the single-line and multi-line
/// formats (the same shapes `read_fs_devices` handles).
fn parse_members(show_super: &str) -> Vec<MemberInfo> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in show_super.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Device ")
            && trimmed.chars().nth(7).is_some_and(|c| c.is_ascii_digit())
            && !current.is_empty()
        {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    blocks
        .iter()
        .filter_map(|block| {
            let header = block.first()?.trim();
            // "Device 3:" or "Device 3 (label ...):" → 3
            let index = header
                .strip_prefix("Device ")
                .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|d| d.parse::<u32>().ok());
            let path = block.iter().find_map(|l| {
                l.split_whitespace()
                    .find(|t| t.starts_with("/dev/"))
                    .map(|t| t.trim_end_matches([',', ';']).to_string())
            });
            let label = extract_member_value(block, "label");
            Some(MemberInfo { index, path, label })
        })
        .collect()
}

/// Of the `expected` member paths, return the ones not in `present`,
/// enriched with member index/label from `members` when show-super
/// knew about them. Pure so it's unit-testable without touching disk.
fn build_missing(
    expected: &[String],
    present: &std::collections::HashSet<String>,
    members: &[MemberInfo],
) -> Vec<MissingDevice> {
    expected
        .iter()
        .filter(|p| !present.contains(*p))
        .map(|path| {
            let m = members
                .iter()
                .find(|m| m.path.as_deref() == Some(path.as_str()));
            MissingDevice {
                path: path.clone(),
                member_index: m.and_then(|m| m.index),
                label: m.and_then(|m| m.label.clone()),
            }
        })
        .collect()
}

fn describe_missing(d: &MissingDevice) -> String {
    match (&d.label, d.member_index) {
        (Some(l), Some(i)) => format!("{} (member {i}, {l})", d.path),
        (Some(l), None) => format!("{} ({l})", d.path),
        (None, Some(i)) => format!("{} (member {i})", d.path),
        (None, None) => d.path.clone(),
    }
}

fn join_human(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{}, and {}", rest.join(", "), last)
        }
    }
}

/// Classify a bcachefs mount stderr (plus the set of absent members)
/// into an operator-facing reason + message. Pure; substring-heuristic
/// over bcachefs's error text, always conservative (Unknown keeps the
/// raw stderr for the details expander).
fn classify_mount_failure(raw: &str, missing: &[MissingDevice]) -> (MountFailureReason, String) {
    let lc = raw.to_lowercase();
    let missing_hit = !missing.is_empty()
        || lc.contains("insufficient devices")
        || lc.contains("not enough devices")
        || lc.contains("required member")
        || lc.contains("no such device")
        || lc.contains("unable to read device")
        || lc.contains("missing device");
    if missing_hit {
        let msg = if missing.is_empty() {
            "A required member device is missing, so the pool can't assemble. If enough \
             replicas remain, mount degraded to bring it up without the missing device."
                .to_string()
        } else {
            let names: Vec<String> = missing.iter().map(describe_missing).collect();
            let (noun, verb, pronoun) = if missing.len() > 1 {
                ("Member devices", "are", "them")
            } else {
                ("Member device", "is", "it")
            };
            format!(
                "{noun} {} {verb} missing — the pool can't assemble. If enough replicas \
                 remain, mount degraded to bring it up without {pronoun}.",
                join_human(&names)
            )
        };
        return (MountFailureReason::MissingDevice, msg);
    }
    if lc.contains("passphrase")
        || lc.contains("encrypt")
        || lc.contains("unlock")
        || lc.contains("locked")
    {
        return (
            MountFailureReason::NeedsUnlock,
            "The filesystem is encrypted and locked — unlock it before mounting.".to_string(),
        );
    }
    if lc.contains("fsck")
        || lc.contains("recovery")
        || lc.contains("checksum")
        || lc.contains("btree")
        || lc.contains("corrupt")
        || lc.contains("journal")
    {
        return (
            MountFailureReason::NeedsCheck,
            "bcachefs reported consistency errors — run a check (fsck) before mounting."
                .to_string(),
        );
    }
    if lc.contains("already mounted") || lc.contains("busy") {
        return (
            MountFailureReason::Busy,
            "The filesystem or its mount point is busy or already in use.".to_string(),
        );
    }
    (
        MountFailureReason::Unknown,
        "The filesystem couldn't be mounted. See the details below for the bcachefs error."
            .to_string(),
    )
}

/// Assemble a [`MountFailure`] from a failed mount: figure out which
/// expected members are absent (enriched via show-super on a present
/// member), then classify. `opts.devices` is the authoritative expected
/// set; fall back to the live device list if it hasn't been recorded.
async fn build_mount_failure(opts: &FsMountOptions, fs: &Filesystem, raw: String) -> MountFailure {
    let expected: Vec<String> = if !opts.devices.is_empty() {
        opts.devices.clone()
    } else {
        fs.devices.iter().map(|d| d.path.clone()).collect()
    };
    let present: std::collections::HashSet<String> = expected
        .iter()
        .filter(|p| std::path::Path::new(p).exists())
        .cloned()
        .collect();
    let members = match present.iter().next() {
        Some(dev) => {
            let out = cmd::run_ok("bcachefs", &["show-super", "-f", "members_v2", dev])
                .await
                .unwrap_or_default();
            parse_members(&out)
        }
        None => Vec::new(),
    };
    let missing = build_missing(&expected, &present, &members);
    let (reason, message) = classify_mount_failure(&raw, &missing);
    MountFailure {
        attempted_at: unix_now_secs(),
        reason,
        message,
        missing_devices: missing,
        raw,
    }
}

fn unix_now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Heuristic: does the captured bcachefs scrub output contain
/// lines that look like reported errors? We default to "no" because
/// bcachefs prints "errors: 0" on a clean run and we don't want to
/// misclassify that as Errors. Matches `errors: N` where N > 0 and
/// also `error:` / `ERROR` (case-insensitive) as a backup signal.
fn combined_indicates_errors(s: &str) -> bool {
    for line in s.lines() {
        let lower = line.to_ascii_lowercase();
        // "errors: 0" → false; "errors: 3" → true.
        if let Some(rest) = lower.strip_prefix("errors:") {
            let count: u64 = rest
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            if count > 0 {
                return true;
            }
            continue;
        }
        if lower.contains("errors: ")
            && let Some(idx) = lower.find("errors: ")
            && let Some(token) = lower[idx + "errors: ".len()..].split_whitespace().next()
            && let Ok(count) = token.parse::<u64>()
            && count > 0
        {
            return true;
        }
        // Fallback: literal "error:" or "ERROR" tokens. Skip the
        // standard "errors: 0" line which we already handled above.
        if (lower.contains("error:") || lower.contains(" error ")) && !lower.contains("errors: 0") {
            return true;
        }
    }
    false
}

/// Keep at most the last `max` bytes of `s`, preserving the trailing
/// content (where bcachefs prints its final summary). Operates on
/// the byte length but trims to the next char boundary so we never
/// emit invalid UTF-8.
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let mut out = String::with_capacity(s.len() - start + 32);
    out.push_str("[output truncated]\n");
    out.push_str(&s[start..]);
    out
}

/// Extract the last `XX%` / `XX.X%` token in a string. Walks back
/// from the rightmost `%`, skips optional whitespace, then collects
/// digits + an optional dot. Returns `None` when no parseable
/// percent is present, or when the parsed value is out of [0, 100].
/// Used by the scrub output streamer — bcachefs emits "32.5%" or
/// similar inside its progress lines.
fn parse_percent(s: &str) -> Option<f32> {
    let bytes = s.as_bytes();
    let percent_pos = bytes.iter().rposition(|&b| b == b'%')?;
    let mut end = percent_pos;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
        start -= 1;
    }
    if start == end {
        return None;
    }
    std::str::from_utf8(&bytes[start..end])
        .ok()?
        .parse::<f32>()
        .ok()
        .filter(|p| (0.0..=100.0).contains(p))
}

/// Upper bound on retained screen rows. Normal `bcachefs scrub` output
/// redraws ~10 rows in place so this is never approached; the cap only
/// matters if a degraded pool spews many distinct error lines over a
/// multi-hour run, where it keeps the engine's RSS bounded.
const SCRUB_SCREEN_MAX_LINES: usize = 4096;

/// Minimal terminal-screen model that reconstructs the *current* frame
/// from `bcachefs scrub`'s in-place progress output.
///
/// bcachefs redraws its per-device table every tick: it erases each row
/// (`ESC[2K`), returns the cursor (`\r`) and walks it up (`ESC[1A`),
/// then reprints. Captured as a raw byte stream, that yields a
/// transcript full of escape litter (`[2K[1A…`) and dozens of
/// duplicated device rows. Replaying the control codes onto a virtual
/// screen collapses it back to exactly what a terminal would show — the
/// latest frame only.
#[derive(Default)]
struct ScrubScreen {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl ScrubScreen {
    fn ensure_row(&mut self) {
        while self.lines.len() <= self.row {
            self.lines.push(Vec::new());
        }
    }

    fn write_char(&mut self, c: char) {
        self.ensure_row();
        let line = &mut self.lines[self.row];
        if self.col < line.len() {
            line[self.col] = c;
        } else {
            while line.len() < self.col {
                line.push(' ');
            }
            line.push(c);
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
        self.ensure_row();
        if self.lines.len() > SCRUB_SCREEN_MAX_LINES {
            let drop = self.lines.len() - SCRUB_SCREEN_MAX_LINES;
            self.lines.drain(0..drop);
            self.row = self.row.saturating_sub(drop);
        }
    }

    fn apply_csi(&mut self, params: &str, final_byte: u8) {
        // First numeric parameter; `max(1)` is applied where the spec
        // treats an absent/zero count as 1 (cursor moves).
        let n = params
            .split(';')
            .next()
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0);
        match final_byte {
            b'A' => self.row = self.row.saturating_sub(n.max(1)),
            b'B' => {
                self.row += n.max(1);
                self.ensure_row();
            }
            b'C' => self.col += n.max(1),
            b'D' => self.col = self.col.saturating_sub(n.max(1)),
            b'K' => {
                // Erase in line: 0/absent → to EOL, 1 → to cursor, 2 → all.
                self.ensure_row();
                let line = &mut self.lines[self.row];
                match n {
                    0 => line.truncate(self.col.min(line.len())),
                    1 => {
                        for ch in line.iter_mut().take(self.col) {
                            *ch = ' ';
                        }
                    }
                    _ => line.clear(),
                }
            }
            b'J' if n >= 2 => {
                self.lines.clear();
                self.row = 0;
                self.col = 0;
            }
            // Cursor home. scrub uses relative moves, not absolute
            // addressing, so the row;col form never appears; treat any
            // form as a move to the origin.
            b'H' | b'f' => {
                self.row = 0;
                self.col = 0;
            }
            // SGR colours ('m') and anything else: no screen effect.
            _ => {}
        }
    }

    /// Apply a chunk of raw output. Returns the trailing slice that
    /// forms an incomplete escape sequence, so a caller streaming in
    /// fixed-size reads can prepend it to the next chunk and still parse
    /// sequences split across a read boundary.
    fn feed<'a>(&mut self, text: &'a str) -> &'a str {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                0x1b => {
                    if i + 1 >= bytes.len() {
                        return &text[i..]; // lone ESC: wait for more
                    }
                    if bytes[i + 1] == b'[' {
                        // CSI: parameter bytes then a final byte 0x40..=0x7e.
                        let mut j = i + 2;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        if j >= bytes.len() {
                            return &text[i..]; // incomplete CSI
                        }
                        self.apply_csi(&text[i + 2..j], bytes[j]);
                        i = j + 1;
                    } else {
                        // Two-byte escape (e.g. charset select): skip both.
                        i += 2;
                    }
                }
                b'\n' => {
                    self.newline();
                    i += 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                // Drop other C0 control bytes (tab, bell, backspace…).
                b if b < 0x20 => i += 1,
                _ => {
                    let c = text[i..].chars().next().unwrap();
                    self.write_char(c);
                    i += c.len_utf8();
                }
            }
        }
        ""
    }

    /// Render to text: trailing spaces trimmed per line, trailing blank
    /// lines dropped.
    fn render(&self) -> String {
        let mut out: Vec<String> = self
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>().trim_end().to_string())
            .collect();
        while matches!(out.last(), Some(l) if l.is_empty()) {
            out.pop();
        }
        out.join("\n")
    }
}

/// Spawn `bcachefs scrub <mount>` with piped stdout+stderr, stream
/// every line (and every `\r`-separated progress update — bcachefs
/// uses carriage returns for in-place percent updates), feed the
/// most-recent `XX%` token back into the in-memory scrub state, and
/// return the (outcome, full captured transcript) on process exit.
async fn stream_scrub_and_collect(
    mount: &str,
    fs_name: &str,
    store: &ScrubStateMap,
) -> (ScrubOutcome, String) {
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    let mut child = match Command::new("bcachefs")
        .args(["scrub", mount])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                ScrubOutcome::Failed,
                format!("failed to spawn bcachefs scrub: {e}"),
            );
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let store_for_progress = store.clone();
    let fs_name_for_progress = fs_name.to_string();
    let capture = std::sync::Arc::new(std::sync::Mutex::new(ScrubScreen::default()));
    let capture_for_task = capture.clone();

    // One reader task per stream. Each task accumulates into the
    // shared capture buffer and updates the in-memory progress
    // percent whenever it sees a parseable `XX%` token. Streams
    // run concurrently because stdout (final summary) and stderr
    // (progress) come on different file descriptors.
    let drain = async move |handle: Option<tokio::process::ChildStdout>,
                            err_handle: Option<tokio::process::ChildStderr>| {
        let store = store_for_progress;
        let fs_name = fs_name_for_progress;
        let cap = capture_for_task;
        let mut stdout_buf = [0u8; 1024];
        let mut stderr_buf = [0u8; 1024];
        // Two pending buffers per stream: `*_line` re-splits on \n/\r for
        // the live percent token (unchanged behaviour); `*_screen` holds
        // an escape sequence split across a read so the frame model can
        // reassemble it.
        let mut stdout_line = String::new();
        let mut stderr_line = String::new();
        let mut stdout_screen = String::new();
        let mut stderr_screen = String::new();

        let mut stdout = handle;
        let mut stderr = err_handle;

        loop {
            tokio::select! {
                read = async {
                    match stdout.as_mut() {
                        Some(s) => s.read(&mut stdout_buf).await,
                        None => Ok(0),
                    }
                }, if stdout.is_some() => {
                    match read {
                        Ok(0) => { stdout = None; }
                        Ok(n) => process_chunk(
                            &stdout_buf[..n],
                            &mut stdout_line,
                            &mut stdout_screen,
                            &cap,
                            &store,
                            &fs_name,
                        ).await,
                        Err(_) => { stdout = None; }
                    }
                }
                read = async {
                    match stderr.as_mut() {
                        Some(s) => s.read(&mut stderr_buf).await,
                        None => Ok(0),
                    }
                }, if stderr.is_some() => {
                    match read {
                        Ok(0) => { stderr = None; }
                        Ok(n) => process_chunk(
                            &stderr_buf[..n],
                            &mut stderr_line,
                            &mut stderr_screen,
                            &cap,
                            &store,
                            &fs_name,
                        ).await,
                        Err(_) => { stderr = None; }
                    }
                }
                else => break,
            }
        }
        // Flush trailing un-terminated content: feed any remaining raw
        // bytes to the frame model and parse a final percent from each
        // stream's leftover line.
        for (line_pending, screen_pending) in [
            (&mut stdout_line, &mut stdout_screen),
            (&mut stderr_line, &mut stderr_screen),
        ] {
            if !screen_pending.is_empty()
                && let Ok(mut screen) = cap.lock()
            {
                screen.feed(screen_pending);
            }
            if !line_pending.is_empty()
                && let Some(pct) = parse_percent(&strip_ansi(line_pending))
            {
                let mut state = store.lock().await;
                if let Some(entry) = state.get_mut(&fs_name) {
                    entry.progress_percent = Some(pct);
                }
            }
        }
    };

    let drain_handle = tokio::spawn(drain(stdout, stderr));

    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            let _ = drain_handle.await;
            return (
                ScrubOutcome::Failed,
                format!("bcachefs scrub child wait failed: {e}"),
            );
        }
    };
    let _ = drain_handle.await;

    let captured = capture.lock().map(|g| g.render()).unwrap_or_default();
    let outcome = if !status.success() {
        ScrubOutcome::Failed
    } else if combined_indicates_errors(&captured) {
        ScrubOutcome::Errors
    } else {
        ScrubOutcome::Ok
    };
    (outcome, captured)
}

/// Apply a freshly-read chunk to both consumers of the scrub stream:
///
/// 1. The frame model (`ScrubScreen`) — fed the *raw* bytes so it can
///    replay bcachefs's in-place redraws into a clean current frame.
///    An escape sequence straddling a read boundary is returned by
///    `feed` and carried over in `screen_pending`.
/// 2. The live progress percent — unchanged: re-split on `\n`/`\r` and
///    take the most recent `XX%` token.
async fn process_chunk(
    chunk: &[u8],
    line_pending: &mut String,
    screen_pending: &mut String,
    cap: &std::sync::Arc<std::sync::Mutex<ScrubScreen>>,
    store: &ScrubStateMap,
    fs_name: &str,
) {
    let text = String::from_utf8_lossy(chunk);

    // 1. Reconstruct the terminal frame for the transcript.
    {
        let mut combined = std::mem::take(screen_pending);
        combined.push_str(&text);
        if let Ok(mut screen) = cap.lock() {
            *screen_pending = screen.feed(&combined).to_string();
        } else {
            *screen_pending = combined;
        }
    }

    // 2. Update the live progress percent (most recent token wins).
    line_pending.push_str(&text);
    while let Some(boundary) = line_pending.find(['\n', '\r']) {
        let line: String = line_pending.drain(..=boundary).collect();
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        if let Some(pct) = parse_percent(&strip_ansi(line)) {
            let mut state = store.lock().await;
            if let Some(entry) = state.get_mut(fs_name) {
                entry.progress_percent = Some(pct);
            }
        }
    }
}

/// Classify an fsck run from its exit status and captured output.
/// Pure + unit-tested. A non-zero exit (errors found, possibly
/// corrected) or error markers in the output ⇒ `Errors`; the captured
/// transcript carries whether they were corrected. Spawn/wait failures
/// are mapped to `Failed` by the caller.
fn classify_fsck(success: bool, output: &str) -> FsckOutcome {
    if !success || combined_indicates_errors(output) {
        FsckOutcome::Errors
    } else {
        FsckOutcome::Clean
    }
}

/// Run an offline `bcachefs fsck` on `devices`, streaming output into a
/// reconstructed terminal frame and surfacing live progress percent.
/// Mirrors [`stream_scrub_and_collect`]; reuses the same `ScrubScreen`
/// frame model and percent parser.
async fn stream_fsck_and_collect(
    devices: &[String],
    fs_name: &str,
    store: &FsckStateMap,
    repair: bool,
) -> (FsckOutcome, String) {
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    // `-n` = dry run (report only, change nothing); `-y` = assume yes
    // (auto-repair). `-f` forces a full check even if the superblock
    // looks clean — without it bcachefs may skip a clean-marked fs.
    let mode = if repair { "-y" } else { "-n" };
    let mut args: Vec<&str> = vec!["fsck", mode, "-f"];
    args.extend(devices.iter().map(|d| d.as_str()));

    let mut child = match Command::new("bcachefs")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                FsckOutcome::Failed,
                format!("failed to spawn bcachefs fsck: {e}"),
            );
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let store_for_progress = store.clone();
    let fs_name_for_progress = fs_name.to_string();
    let capture = std::sync::Arc::new(std::sync::Mutex::new(ScrubScreen::default()));
    let capture_for_task = capture.clone();

    let drain = async move |handle: Option<tokio::process::ChildStdout>,
                            err_handle: Option<tokio::process::ChildStderr>| {
        let store = store_for_progress;
        let fs_name = fs_name_for_progress;
        let cap = capture_for_task;
        let mut stdout_buf = [0u8; 1024];
        let mut stderr_buf = [0u8; 1024];
        let mut stdout_line = String::new();
        let mut stderr_line = String::new();
        let mut stdout_screen = String::new();
        let mut stderr_screen = String::new();
        let mut stdout = handle;
        let mut stderr = err_handle;

        loop {
            tokio::select! {
                read = async {
                    match stdout.as_mut() {
                        Some(s) => s.read(&mut stdout_buf).await,
                        None => Ok(0),
                    }
                }, if stdout.is_some() => {
                    match read {
                        Ok(0) => { stdout = None; }
                        Ok(n) => process_fsck_chunk(
                            &stdout_buf[..n],
                            &mut stdout_line,
                            &mut stdout_screen,
                            &cap,
                            &store,
                            &fs_name,
                        ).await,
                        Err(_) => { stdout = None; }
                    }
                }
                read = async {
                    match stderr.as_mut() {
                        Some(s) => s.read(&mut stderr_buf).await,
                        None => Ok(0),
                    }
                }, if stderr.is_some() => {
                    match read {
                        Ok(0) => { stderr = None; }
                        Ok(n) => process_fsck_chunk(
                            &stderr_buf[..n],
                            &mut stderr_line,
                            &mut stderr_screen,
                            &cap,
                            &store,
                            &fs_name,
                        ).await,
                        Err(_) => { stderr = None; }
                    }
                }
                else => break,
            }
        }
        for (line_pending, screen_pending) in [
            (&mut stdout_line, &mut stdout_screen),
            (&mut stderr_line, &mut stderr_screen),
        ] {
            if !screen_pending.is_empty()
                && let Ok(mut screen) = cap.lock()
            {
                screen.feed(screen_pending);
            }
            if !line_pending.is_empty()
                && let Some(pct) = parse_percent(&strip_ansi(line_pending))
            {
                let mut state = store.lock().await;
                if let Some(entry) = state.get_mut(&fs_name) {
                    entry.progress_percent = Some(pct);
                }
            }
        }
    };

    let drain_handle = tokio::spawn(drain(stdout, stderr));

    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            let _ = drain_handle.await;
            return (
                FsckOutcome::Failed,
                format!("bcachefs fsck child wait failed: {e}"),
            );
        }
    };
    let _ = drain_handle.await;

    let captured = capture.lock().map(|g| g.render()).unwrap_or_default();
    (classify_fsck(status.success(), &captured), captured)
}

/// fsck analogue of [`process_chunk`]: feed raw bytes to the frame model
/// and update the live percent on the [`FsckStateMap`].
async fn process_fsck_chunk(
    chunk: &[u8],
    line_pending: &mut String,
    screen_pending: &mut String,
    cap: &std::sync::Arc<std::sync::Mutex<ScrubScreen>>,
    store: &FsckStateMap,
    fs_name: &str,
) {
    let text = String::from_utf8_lossy(chunk);
    {
        let mut combined = std::mem::take(screen_pending);
        combined.push_str(&text);
        if let Ok(mut screen) = cap.lock() {
            *screen_pending = screen.feed(&combined).to_string();
        } else {
            *screen_pending = combined;
        }
    }
    line_pending.push_str(&text);
    while let Some(boundary) = line_pending.find(['\n', '\r']) {
        let line: String = line_pending.drain(..=boundary).collect();
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        if let Some(pct) = parse_percent(&strip_ansi(line)) {
            let mut state = store.lock().await;
            if let Some(entry) = state.get_mut(fs_name) {
                entry.progress_percent = Some(pct);
            }
        }
    }
}

fn get_fs_mount_options(state: &FsState, name: &str) -> FsMountOptions {
    state.get(name).cloned().unwrap_or_default()
}

fn build_mount_opts(opts: &FsMountOptions) -> String {
    let mut parts = vec!["prjquota".to_string()];
    if let Some(ref vu) = opts.version_upgrade
        && !vu.is_empty()
        && vu != "none"
    {
        parts.push(format!("version_upgrade={vu}"));
    }
    if opts.degraded == Some(true) {
        parts.push("degraded".to_string());
    }
    if opts.verbose == Some(true) {
        parts.push("verbose".to_string());
    }
    if opts.fsck == Some(true) {
        parts.push("fsck".to_string());
    }
    if opts.journal_flush_disabled == Some(true) {
        parts.push("journal_flush_disabled".to_string());
    }
    if let Some(delay) = opts.journal_flush_delay {
        parts.push(format!("journal_flush_delay={delay}"));
    }
    parts.join(",")
}

/// Extract the short device name from a path (e.g. `/dev/sda` → `sda`).
fn block_dev_name(path: &str) -> Option<&str> {
    path.rsplit('/').next()
}

/// Apply an I/O scheduler to all member block devices of a filesystem.
async fn apply_io_scheduler(
    devices: &[FilesystemDevice],
    scheduler: &str,
) -> Result<(), FilesystemError> {
    for dev in devices {
        let Some(name) = block_dev_name(&dev.path) else {
            continue;
        };
        let sysfs_path = format!("/sys/block/{name}/queue/scheduler");
        if let Err(e) = tokio::fs::write(&sysfs_path, scheduler).await {
            // Not fatal — device may be a partition or missing sysfs entry
            warn!("Failed to set I/O scheduler on {name}: {e}");
        } else {
            info!("Set I/O scheduler to '{scheduler}' on {name}");
        }
    }
    Ok(())
}

/// Read the active I/O scheduler from the first member device.
/// Returns the bracketed value from `/sys/block/{dev}/queue/scheduler`.
async fn read_io_scheduler(devices: &[FilesystemDevice]) -> Option<String> {
    let dev = devices.first()?;
    let name = block_dev_name(&dev.path)?;
    let sysfs_path = format!("/sys/block/{name}/queue/scheduler");
    let content = tokio::fs::read_to_string(&sysfs_path).await.ok()?;
    // Format: "none [mq-deadline] kyber" — extract the bracketed one
    content
        .split_whitespace()
        .find(|s| s.starts_with('['))
        .map(|s| s.trim_matches(|c| c == '[' || c == ']').to_string())
}

async fn is_mountpoint(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    // A path is a mount point when its device ID differs from its parent's,
    // or when it is the filesystem root (path == parent, same inode).
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return false;
    };
    let parent = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("/"));
    let Ok(parent_meta) = tokio::fs::metadata(parent).await else {
        return false;
    };
    meta.dev() != parent_meta.dev() || meta.ino() == parent_meta.ino()
}

/// Try to find filesystem name from existing mount point directories
fn find_fs_name_by_devices(_devices: &[String]) -> Option<String> {
    // Check if any directory exists under the mount base
    let base = Path::new(NASTY_MOUNT_BASE);
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                return Some(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── show-super classification (encryption probe) ──────────────

    /// Happy path: show-super returned 0. Whatever the FS is, we
    /// don't need to `bcachefs unlock` before mounting it.
    #[test]
    fn classify_show_super_success_means_no_unlock_needed() {
        assert_eq!(super::classify_show_super(true, ""), super::NeedsUnlock::No);
        // Some bcachefs versions still print noise on stderr even on
        // success. We trust the exit code.
        assert_eq!(
            super::classify_show_super(true, "Note: some advisory message"),
            super::NeedsUnlock::No
        );
    }

    /// Encrypted-and-locked: bcachefs reports it cannot read the
    /// passphrase, exit non-zero. We need to unlock before mount.
    #[test]
    fn classify_show_super_passphrase_failure_means_unlock_needed() {
        // Exact stderr captured on .0f.ee / .100 boot logs before
        // the fix:
        let stderr = "bcachefs exited with exit status: 1: \
                      Error: reading superblock: error reading passphrase";
        assert_eq!(
            super::classify_show_super(false, stderr),
            super::NeedsUnlock::Yes
        );
        // Bare phrase without the wrapping context should also match
        // — guard against bcachefs-tools tweaking the prefix.
        assert_eq!(
            super::classify_show_super(false, "error reading passphrase"),
            super::NeedsUnlock::Yes
        );
    }

    /// **Regression test for the .0f.ee / .100 incident** —
    /// previously the engine treated "key file present" as proof
    /// the FS was encrypted, ran `bcachefs unlock`, and got back
    /// "Error: /dev/sdd is not encrypted". With the show-super
    /// probe in place, that error path is unreachable: the probe
    /// would already have classified the device as
    /// `NeedsUnlock::No` (show-super succeeds on unencrypted FSes)
    /// and the unlock step would have been skipped. So the
    /// regression manifests as a probe-classifier-level mistake,
    /// which this test fences.
    #[test]
    fn classify_show_super_does_not_treat_other_errors_as_unlock_needed() {
        // The exact stderr `bcachefs unlock` produces against an
        // unencrypted device — used to convince us we needed an
        // unlock when we really didn't.
        let stderr = "Error: /dev/sdd is not encrypted";
        assert_eq!(
            super::classify_show_super(false, stderr),
            super::NeedsUnlock::Unknown,
            "unrelated bcachefs errors must NOT trigger an unlock attempt"
        );
        // Common other failure shapes — all map to Unknown (don't
        // attempt unlock; let mount produce the real error).
        assert_eq!(
            super::classify_show_super(false, "Error: opening /dev/sdd: No such file or directory"),
            super::NeedsUnlock::Unknown
        );
        assert_eq!(
            super::classify_show_super(false, "Error: opening /dev/sdd: Permission denied"),
            super::NeedsUnlock::Unknown
        );
        assert_eq!(
            super::classify_show_super(false, "Error: not a bcachefs filesystem"),
            super::NeedsUnlock::Unknown
        );
        // Empty stderr on a non-zero exit also stays Unknown.
        assert_eq!(
            super::classify_show_super(false, ""),
            super::NeedsUnlock::Unknown
        );
    }

    // ── FsMountOptions / state serialisation ───────────────────────

    /// Existing on-disk state files (written before the `mounted`
    /// flag was added) parse cleanly, with `mounted` left as `None`.
    /// `restore_mounts` treats `None == auto-mount`, so legacy
    /// installs keep their existing behaviour after an upgrade.
    #[test]
    fn fs_mount_options_deserializes_pre_mounted_flag_state() {
        let legacy = r#"{
            "uuid": "1936f811-8b77-4931-822e-3f9454f93162",
            "devices": ["/dev/sdd", "/dev/sdb", "/dev/sdc"],
            "encrypted": true,
            "compression": null
        }"#;
        let opts: FsMountOptions = serde_json::from_str(legacy).expect("legacy state parses");
        assert_eq!(opts.mounted, None);
        assert_eq!(opts.encrypted, Some(true));
        assert_eq!(opts.devices.len(), 3);
    }

    /// Round-trip an unmount→mount cycle in pure data form: an FS
    /// with tuned options must keep them across a `mounted = false`
    /// pause and survive serde re-serialization. Regression for
    /// "unmount silently wiped my compression/journal_flush_delay/
    /// io_scheduler config" — pre-fix `save_fs_unmounted` did a
    /// flat `state.remove(name)`.
    #[test]
    fn unmount_preserves_tuned_options_across_state_roundtrip() {
        let mounted = FsMountOptions {
            uuid: Some("uuid-1".into()),
            devices: vec!["/dev/sda".into(), "/dev/sdb".into()],
            mounted: Some(true),
            encrypted: Some(true),
            journal_flush_delay: Some(1000),
            io_scheduler: Some("none".into()),
            ..FsMountOptions::default()
        };

        // Simulate `save_fs_unmounted`: preserve the entry, flip
        // mounted=false, leave everything else alone.
        let mut after_unmount = mounted.clone();
        after_unmount.mounted = Some(false);

        // Round-trip through JSON the way fs-state.json does, so
        // skip_serializing_if / default decorators are exercised.
        let json = serde_json::to_string(&after_unmount).expect("serialize");
        let restored: FsMountOptions = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.mounted, Some(false));
        assert_eq!(restored.encrypted, Some(true)); // the regression we're fencing
        assert_eq!(restored.journal_flush_delay, Some(1000));
        assert_eq!(restored.io_scheduler.as_deref(), Some("none"));
        assert_eq!(restored.devices, vec!["/dev/sda", "/dev/sdb"]);
    }

    // ── parse_human_bytes ──────────────────────────────────────────

    #[test]
    fn parse_human_bytes_plain_integer() {
        assert_eq!(parse_human_bytes("0"), Some(0));
        assert_eq!(parse_human_bytes("123"), Some(123));
        // From `bcachefs fs usage` "Size:" line captured in the smoke test.
        assert_eq!(parse_human_bytes("975783936"), Some(975_783_936));
    }

    #[test]
    fn parse_human_bytes_with_units() {
        // Values lifted from `bcachefs show-super` Options and per-device blocks.
        assert_eq!(parse_human_bytes("4.00k"), Some(4096));
        assert_eq!(parse_human_bytes("256k"), Some(256 * 1024));
        assert_eq!(parse_human_bytes("512k"), Some(512 * 1024));
        assert_eq!(parse_human_bytes("2.00M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_human_bytes("1.00G"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn parse_human_bytes_unit_aliases_are_case_insensitive() {
        assert_eq!(parse_human_bytes("1024B"), Some(1024));
        assert_eq!(parse_human_bytes("1k"), Some(1024));
        assert_eq!(parse_human_bytes("1KB"), Some(1024));
        assert_eq!(parse_human_bytes("1KiB"), Some(1024));
        assert_eq!(parse_human_bytes("1MiB"), Some(1024 * 1024));
        assert_eq!(parse_human_bytes("1TB"), Some(1024u64.pow(4)));
    }

    #[test]
    fn parse_human_bytes_invalid_returns_none() {
        assert_eq!(parse_human_bytes(""), None);
        assert_eq!(parse_human_bytes("abc"), None);
        assert_eq!(parse_human_bytes("1.5XYZ"), None);
        // `bcachefs show-super` prints "Superblock size: 7.85k/1.00M" — the
        // slash form isn't a unit and shouldn't parse.
        assert_eq!(parse_human_bytes("7.85k/1.00M"), None);
    }

    // ── validate_compression (#491) ────────────────────────────────

    #[test]
    fn validate_compression_accepts_bare_algorithms_and_none() {
        for s in ["none", "", "lz4", "zstd", "gzip", "  zstd  "] {
            assert!(validate_compression(s).is_ok(), "should accept {s:?}");
        }
    }

    #[test]
    fn validate_compression_accepts_levels_in_range() {
        for s in ["zstd:1", "zstd:15", "zstd:22", "gzip:1", "gzip:9"] {
            assert!(validate_compression(s).is_ok(), "should accept {s:?}");
        }
    }

    #[test]
    fn validate_compression_rejects_out_of_range_levels() {
        assert!(validate_compression("zstd:0").is_err());
        assert!(validate_compression("zstd:23").is_err());
        assert!(validate_compression("gzip:10").is_err());
    }

    #[test]
    fn validate_compression_rejects_level_on_lz4() {
        // bcachefs lz4 has no tunable level.
        assert!(validate_compression("lz4:5").is_err());
    }

    #[test]
    fn validate_compression_rejects_unknown_algo_and_garbage() {
        assert!(validate_compression("snappy").is_err());
        assert!(validate_compression("zstd:high").is_err());
        assert!(validate_compression("zstd:").is_err());
    }

    // ── parse_device_table_line ────────────────────────────────────

    #[test]
    fn parse_device_table_line_real_fixture() {
        // Real row captured from `bcachefs fs usage /mnt/test` in the smoke test.
        let line = "(no label) (device 0):  vdb     rw     1062203392  2883584    0%";
        let dev = parse_device_table_line(line).expect("should parse");
        assert_eq!(dev.path, "/dev/vdb");
        assert_eq!(dev.total_bytes, 1_062_203_392);
        assert_eq!(dev.used_bytes, 2_883_584);
        assert_eq!(dev.free_bytes, 1_062_203_392 - 2_883_584);
    }

    #[test]
    fn parse_device_table_line_human_readable() {
        let line = "label (device 0):  sdb     rw     49.6G         8.50M      0%";
        let dev = parse_device_table_line(line).expect("should parse");
        assert_eq!(dev.path, "/dev/sdb");
        assert_eq!(dev.total_bytes, (49.6 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(dev.used_bytes, (8.50 * 1024.0 * 1024.0) as u64);
    }

    #[test]
    fn parse_device_table_line_keeps_absolute_path() {
        let line = "(no label) (device 0):  /dev/sda     rw     100G  10G  10%";
        let dev = parse_device_table_line(line).expect("should parse");
        assert_eq!(dev.path, "/dev/sda");
    }

    #[test]
    fn parse_device_table_line_skips_header_and_garbage() {
        assert!(
            parse_device_table_line("Device label  Device  State  Size     Used  Use%").is_none()
        );
        assert!(parse_device_table_line("").is_none());
        assert!(parse_device_table_line("nonsense without colon-paren").is_none());
    }

    // ── parse_bcachefs_mount_line ──────────────────────────────────

    #[test]
    fn parse_bcachefs_mount_single_device() {
        let m = parse_bcachefs_mount_line("/dev/sda /mnt/tank bcachefs rw,relatime 0 0")
            .expect("should parse");
        assert_eq!(m.mount_point, "/mnt/tank");
        assert_eq!(m.devices, vec!["/dev/sda".to_string()]);
    }

    #[test]
    fn parse_bcachefs_mount_multi_device() {
        let m = parse_bcachefs_mount_line(
            "/dev/sda:/dev/sdb:/dev/sdc /mnt/pool bcachefs rw,compression=zstd 0 0",
        )
        .expect("should parse");
        assert_eq!(m.mount_point, "/mnt/pool");
        assert_eq!(
            m.devices,
            vec![
                "/dev/sda".to_string(),
                "/dev/sdb".to_string(),
                "/dev/sdc".to_string(),
            ],
        );
    }

    #[test]
    fn parse_bcachefs_mount_skips_other_fstypes() {
        assert!(parse_bcachefs_mount_line("/dev/sda /mnt ext4 rw 0 0").is_none());
        assert!(parse_bcachefs_mount_line("tmpfs /run tmpfs rw 0 0").is_none());
    }

    #[test]
    fn parse_bcachefs_mount_skips_short_lines() {
        assert!(parse_bcachefs_mount_line("").is_none());
        assert!(parse_bcachefs_mount_line("/dev/sda").is_none());
        assert!(parse_bcachefs_mount_line("/dev/sda /mnt").is_none());
    }

    // ── parse_bcachefs_opt ─────────────────────────────────────────

    #[test]
    fn parse_bcachefs_opt_extracts_bracketed_default() {
        // All values lifted from the Options block of `bcachefs show-super`
        // captured in the smoke test.
        assert_eq!(
            parse_bcachefs_opt("continue [fix_safe] panic ro"),
            "fix_safe"
        );
        assert_eq!(parse_bcachefs_opt("none [crc32c] crc64 xxhash"), "crc32c");
        assert_eq!(parse_bcachefs_opt("crc32c crc64 [siphash]"), "siphash");
        assert_eq!(parse_bcachefs_opt("[ask] yes very no"), "ask");
        assert_eq!(parse_bcachefs_opt("[unclean] no always"), "unclean");
        assert_eq!(
            parse_bcachefs_opt("[compatible] incompatible none"),
            "compatible"
        );
    }

    #[test]
    fn parse_bcachefs_opt_returns_plain_value_unchanged() {
        // Non-enum options in the same Options block are scalar values.
        assert_eq!(parse_bcachefs_opt("none"), "none");
        assert_eq!(parse_bcachefs_opt("zstd"), "zstd");
        assert_eq!(parse_bcachefs_opt("4.00k"), "4.00k");
        assert_eq!(parse_bcachefs_opt(""), "");
    }

    // ── proc_keys_has_bcachefs_uuid ────────────────────────────────

    // Real /proc/keys lines have format:
    //   `<id-hex> <flags> <uses> perm <perm-hex> <uid> <gid> <type>  <description>: <data>`
    // i.e. the description column ends with `:` followed by a type-specific
    // data column (for `user`/`logon` keys: data length in bytes). Earlier
    // tests handcrafted lines without that trailing `:`, which masked a
    // real bug in the parser — see `line_has_key_description`.

    #[test]
    fn proc_keys_finds_bcachefs_user_key() {
        // Verbatim from a running NASty after `bcachefs unlock -k session`
        // (issue: filesystem 'first' showed Locked despite successful unlock,
        // because the parser's `tok == needle` check missed the trailing colon).
        let contents = "\
1de1938e I--Q---     1 perm 3f010000     0     0 user      bcachefs:a56458ab-a24c-45b6-9052-299ae1e3da43: 32
";
        assert!(proc_keys_has_bcachefs_uuid(
            contents,
            "a56458ab-a24c-45b6-9052-299ae1e3da43",
        ));
    }

    #[test]
    fn proc_keys_finds_bcachefs_logon_key() {
        // bcachefs has shipped variants that use the `logon` keytype too —
        // make sure the parser doesn't over-fit to one keytype. The trailing
        // `:` and data column are still present.
        let contents = "\
2c93e9b4 I--Q---     1 perm 3f010000     0     0 keyring   _ses: 1
3a821c8e I------     1 perm 3f010000     0     0 logon     bcachefs:abcd1234-1111-2222-3333-444455556666: 32
";
        assert!(proc_keys_has_bcachefs_uuid(
            contents,
            "abcd1234-1111-2222-3333-444455556666",
        ));
    }

    #[test]
    fn proc_keys_misses_when_uuid_absent() {
        let contents = "\
2c93e9b4 I--Q---     1 perm 3f010000     0     0 keyring   _ses: 1
3a821c8e I------     1 perm 3f010000     0     0 logon     bcachefs:other-uuid-here: 32
";
        assert!(!proc_keys_has_bcachefs_uuid(
            contents,
            "abcd1234-1111-2222-3333-444455556666",
        ));
    }

    #[test]
    fn proc_keys_misses_on_empty_keyring() {
        assert!(!proc_keys_has_bcachefs_uuid("", "abcd1234"));
    }

    #[test]
    fn proc_keys_does_not_substring_match_other_uuids() {
        // A UUID that's a *prefix* of an entry must not match — bcachefs UUIDs
        // are full strings, not prefixes.
        let contents = "\
3a821c8e I------     1 perm 3f010000     0     0 logon     bcachefs:abcd1234-extra-suffix: 32
";
        assert!(!proc_keys_has_bcachefs_uuid(contents, "abcd1234"));
    }

    // ── parse_bcachefs_key_id ─────────────────────────────────────

    #[test]
    fn parse_key_id_returns_decimal_id_for_real_kernel_format() {
        // Verbatim live-box format including the trailing `:` and `<data>`
        // column. 1de1938e hex == 501322638 decimal — that's what
        // `keyctl unlink` needs to revoke the key.
        let contents = "\
1de1938e I--Q---     1 perm 3f010000     0     0 user      bcachefs:a56458ab-a24c-45b6-9052-299ae1e3da43: 32
";
        let id = parse_bcachefs_key_id(contents, "a56458ab-a24c-45b6-9052-299ae1e3da43");
        assert_eq!(id.as_deref(), Some("501322638"));
    }

    #[test]
    fn parse_key_id_returns_decimal_id_for_matching_uuid() {
        // Synthetic but format-faithful: 3a821c8e hex == 981605518 decimal.
        let contents = "\
2c93e9b4 I--Q---     1 perm 3f010000     0     0 keyring   _ses: 1
3a821c8e I------     1 perm 3f010000     0     0 logon     bcachefs:abcd1234-1111-2222-3333-444455556666: 32
";
        let id = parse_bcachefs_key_id(contents, "abcd1234-1111-2222-3333-444455556666");
        assert_eq!(id.as_deref(), Some("981605518"));
    }

    #[test]
    fn parse_key_id_returns_none_when_uuid_absent() {
        let contents = "\
3a821c8e I------     1 perm 3f010000     0     0 logon     bcachefs:other-uuid: 32
";
        assert!(parse_bcachefs_key_id(contents, "abcd1234-1111-2222-3333-444455556666").is_none());
    }

    #[test]
    fn parse_key_id_returns_none_for_empty_keyring() {
        assert!(parse_bcachefs_key_id("", "any-uuid").is_none());
    }

    #[test]
    fn parse_key_id_does_not_match_uuid_prefix() {
        // Same prefix-safety as proc_keys_has_bcachefs_uuid — don't
        // unlink someone else's key just because the uuids share a
        // common prefix.
        let contents = "\
3a821c8e I------     1 perm 3f010000     0     0 logon     bcachefs:abcd1234-extra: 32
";
        assert!(parse_bcachefs_key_id(contents, "abcd1234").is_none());
    }

    #[test]
    fn parse_key_id_picks_first_matching_line() {
        // Defensive: a stale revoked key + a fresh one with the same
        // uuid would be unusual but possible if a previous lock was
        // interrupted. Take the first id; if unlink fails on it, the
        // operator can re-run.
        let contents = "\
00000010 I------     1 perm 3f010000     0     0 logon     bcachefs:dup-uuid: 32
00000020 I------     1 perm 3f010000     0     0 logon     bcachefs:dup-uuid: 32
";
        let id = parse_bcachefs_key_id(contents, "dup-uuid");
        assert_eq!(id.as_deref(), Some("16")); // 0x10
    }

    // ── Scrub output classifier ───────────────────────────────

    #[test]
    fn scrub_clean_run_classifies_as_ok() {
        // bcachefs prints a final summary line; a clean run reports
        // zero errors. We must NOT misclassify "errors: 0" as Errors —
        // that's exactly what every successful scrub reports.
        let out = "scrubbing /fs/tank ...\nscrub complete\nerrors: 0\n";
        assert!(!combined_indicates_errors(out));
    }

    #[test]
    fn scrub_nonzero_error_count_classifies_as_errors() {
        // Any non-zero error count in the summary flips the bullet.
        let out = "scrubbing /fs/tank ...\nscrub complete\nerrors: 3\n";
        assert!(combined_indicates_errors(out));
    }

    #[test]
    fn scrub_inline_error_token_classifies_as_errors() {
        // Fallback signal — bcachefs may emit per-shard "error:"
        // lines during the scan even before the final summary.
        // Operators looking at the captured output expect Errors.
        let out = "scrubbing /fs/tank ...\ndev 0: error: io_error reading block 0xabc\nerrors: 1\n";
        assert!(combined_indicates_errors(out));
    }

    #[test]
    fn scrub_truncate_tail_keeps_summary_at_end() {
        // The summary lives at the end of a chatty scrub. Truncating
        // from the front (keeping the tail) preserves what the
        // operator actually needs to see, plus a marker that more
        // existed.
        let chatty = "noise\n".repeat(2000) + "scrub complete\nerrors: 0\n";
        let trimmed = truncate_tail(&chatty, 256);
        assert!(trimmed.starts_with("[output truncated]"));
        assert!(trimmed.ends_with("errors: 0\n"));
        assert!(trimmed.len() < chatty.len());
    }

    #[test]
    fn scrub_parse_percent_extracts_typical_progress_line() {
        // bcachefs prints something like "data scrub: 32.5% complete"
        // (and similar — exact format may vary across tools versions).
        // The parser walks back from the rightmost `%` so trailing
        // descriptive text doesn't trip us up.
        assert_eq!(parse_percent("data scrub: 32.5% complete"), Some(32.5));
        assert_eq!(parse_percent("47%"), Some(47.0));
        assert_eq!(parse_percent("scrubbing 100%"), Some(100.0));
        assert_eq!(parse_percent("0%"), Some(0.0));
        // Space between number and `%` should still parse — some
        // tools print "47 %".
        assert_eq!(parse_percent("47 %"), Some(47.0));
    }

    #[test]
    fn scrub_parse_percent_rejects_invalid_or_out_of_range() {
        assert_eq!(parse_percent("no percent here"), None);
        assert_eq!(parse_percent(""), None);
        // 150% is nonsense — clamp by rejecting rather than
        // displaying a > 100 progress bar that looks broken.
        assert_eq!(parse_percent("150%"), None);
        // Bare `%` with no number.
        assert_eq!(parse_percent("xxx%"), None);
    }

    #[test]
    fn scrub_parse_percent_picks_rightmost_when_multiple() {
        // When a line contains multiple percent tokens, the most
        // recent one is the operator-meaningful current progress.
        // (rationale: bcachefs may print "errors: 0%, scrubbing: 47%"
        // or similar composites in future versions.)
        assert_eq!(parse_percent("errors: 0%, scrub: 47.5%"), Some(47.5));
    }

    #[test]
    fn scrub_truncate_tail_short_input_passthrough() {
        // Below the cap, no truncation marker — operator sees the
        // exact, untouched output.
        let s = "scrub complete\nerrors: 0\n";
        assert_eq!(truncate_tail(s, 1024), s);
    }

    #[test]
    fn scrub_screen_collapses_redraw_to_latest_frame() {
        // Mirrors real `bcachefs scrub`: a static header, the device
        // rows (last row not newline-terminated), then the in-place
        // redraw — per row `ESC[2K \r ESC[1A`, a final `ESC[2K \r`, and
        // a reprint with updated values.
        let mut s = ScrubScreen::default();
        s.feed("Starting scrub on 2 devices: sda sdb\n");
        s.feed("device   total   %\n");
        s.feed("sda  100M  10%\nsdb  200M  20%");
        s.feed("\x1b[2K\r\x1b[1A\x1b[2K\r");
        s.feed("sda  100M  60%\nsdb  200M  55%");
        let out = s.render();

        assert_eq!(
            out,
            "Starting scrub on 2 devices: sda sdb\ndevice   total   %\nsda  100M  60%\nsdb  200M  55%"
        );
        // No escape litter, no duplicated frames, and only the latest
        // values survive (the redraw replaced the stale ones).
        assert!(!out.contains('\x1b'), "escape codes leaked: {out:?}");
        assert!(!out.contains("[2K"), "erase-line litter leaked: {out:?}");
        assert_eq!(out.lines().count(), 4, "rows duplicated: {out:?}");
        assert!(out.contains("60%") && out.contains("55%"));
        assert!(
            !out.contains("10%") && !out.contains("20%"),
            "stale frame: {out:?}"
        );
    }

    #[test]
    fn scrub_screen_reassembles_escape_split_across_reads() {
        // A CSI can land on a 1 KiB read boundary; `feed` returns the
        // incomplete tail so it can be prepended to the next chunk.
        let mut s = ScrubScreen::default();
        s.feed("header\n");
        s.feed("sda 10%\nsdb 20%");

        let leftover = s.feed("\x1b[2");
        assert_eq!(leftover, "\x1b[2"); // incomplete CSI, nothing applied yet

        s.feed(&format!("{leftover}K\r\x1b[1A\x1b[2K\r"));
        s.feed("sda 60%\nsdb 55%");

        assert_eq!(s.render(), "header\nsda 60%\nsdb 55%");
    }

    #[test]
    fn scrub_screen_strips_color_codes_without_moving_cursor() {
        // SGR colour sequences must not affect layout or leak into text.
        let mut s = ScrubScreen::default();
        s.feed("\x1b[32msda ok\x1b[0m\n");
        assert_eq!(s.render(), "sda ok");
    }

    // ── Mount-failure diagnostics (#451) ──────────────────────

    fn md(path: &str, idx: Option<u32>, label: Option<&str>) -> MissingDevice {
        MissingDevice {
            path: path.into(),
            member_index: idx,
            label: label.map(str::to_string),
        }
    }

    #[test]
    fn classify_uses_missing_devices_even_when_stderr_is_opaque() {
        let missing = vec![md("/dev/sdb", Some(1), Some("hdd.archive"))];
        let (reason, msg) = classify_mount_failure("bcachefs: mount failed", &missing);
        assert_eq!(reason, MountFailureReason::MissingDevice);
        assert!(msg.contains("/dev/sdb"));
        assert!(msg.contains("member 1"));
        assert!(msg.contains("hdd.archive"));
        assert!(msg.contains("degraded"));
        assert!(msg.contains("is missing"), "singular phrasing: {msg}");
    }

    #[test]
    fn classify_detects_missing_from_stderr_text_alone() {
        let (reason, _) = classify_mount_failure("error: insufficient devices to mount", &[]);
        assert_eq!(reason, MountFailureReason::MissingDevice);
    }

    #[test]
    fn classify_plural_missing_uses_plural_phrasing() {
        let missing = vec![md("/dev/sdb", None, None), md("/dev/sdc", None, None)];
        let (_, msg) = classify_mount_failure("", &missing);
        assert!(msg.contains("/dev/sdb and /dev/sdc"), "{msg}");
        assert!(msg.contains("are missing"), "{msg}");
    }

    #[test]
    fn classify_recognizes_lock_check_and_busy() {
        assert_eq!(
            classify_mount_failure("error reading passphrase", &[]).0,
            MountFailureReason::NeedsUnlock
        );
        assert_eq!(
            classify_mount_failure("filesystem needs recovery, run fsck", &[]).0,
            MountFailureReason::NeedsCheck
        );
        assert_eq!(
            classify_mount_failure("mount: /fs/tank: device is busy", &[]).0,
            MountFailureReason::Busy
        );
    }

    #[test]
    fn classify_unknown_keeps_generic_message() {
        let (reason, msg) = classify_mount_failure("some novel bcachefs error", &[]);
        assert_eq!(reason, MountFailureReason::Unknown);
        assert!(msg.to_lowercase().contains("details"));
    }

    #[test]
    fn missing_device_classification_wins_over_other_keywords() {
        // A missing-device failure whose stderr also mentions "journal"
        // must still classify as MissingDevice (the actionable cause).
        let missing = vec![md("/dev/sdb", None, None)];
        let (reason, _) = classify_mount_failure("error: journal: insufficient devices", &missing);
        assert_eq!(reason, MountFailureReason::MissingDevice);
    }

    #[test]
    fn parse_members_multiline_format() {
        let out = "\
Device 0:\t/dev/sda
\tLabel:\t\tssd.fast
\tState:\t\trw
Device 1:\t/dev/sdb
\tLabel:\t\thdd.archive
\tState:\t\trw
";
        let members = parse_members(out);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].index, Some(0));
        assert_eq!(members[0].path.as_deref(), Some("/dev/sda"));
        assert_eq!(members[0].label.as_deref(), Some("ssd.fast"));
        assert_eq!(members[1].index, Some(1));
        assert_eq!(members[1].path.as_deref(), Some("/dev/sdb"));
    }

    #[test]
    fn build_missing_enriches_absent_members_from_show_super() {
        let expected = vec!["/dev/sda".to_string(), "/dev/sdb".to_string()];
        let present: std::collections::HashSet<String> =
            ["/dev/sda".to_string()].into_iter().collect();
        let members = vec![
            MemberInfo {
                index: Some(0),
                path: Some("/dev/sda".into()),
                label: Some("ssd.fast".into()),
            },
            MemberInfo {
                index: Some(1),
                path: Some("/dev/sdb".into()),
                label: Some("hdd.archive".into()),
            },
        ];
        let missing = build_missing(&expected, &present, &members);
        assert_eq!(missing, vec![md("/dev/sdb", Some(1), Some("hdd.archive"))]);
    }

    #[test]
    fn build_missing_handles_no_show_super_info() {
        let expected = vec!["/dev/sda".to_string(), "/dev/sdb".to_string()];
        let present: std::collections::HashSet<String> =
            ["/dev/sda".to_string()].into_iter().collect();
        let missing = build_missing(&expected, &present, &[]);
        assert_eq!(missing, vec![md("/dev/sdb", None, None)]);
    }

    #[test]
    fn join_human_oxford_comma() {
        assert_eq!(join_human(&["a".into()]), "a");
        assert_eq!(join_human(&["a".into(), "b".into()]), "a and b");
        assert_eq!(
            join_human(&["a".into(), "b".into(), "c".into()]),
            "a, b, and c"
        );
    }

    // ── fsck outcome classification (#440) ────────────────────

    #[test]
    fn fsck_clean_on_zero_exit_and_no_errors() {
        let out = "checking allocations\nchecking extents\ndone\n";
        assert_eq!(classify_fsck(true, out), FsckOutcome::Clean);
    }

    #[test]
    fn fsck_nonzero_exit_is_errors() {
        // bcachefs fsck exits non-zero when it found (and/or corrected)
        // problems; the captured transcript carries the detail.
        let out = "checking extents\n";
        assert_eq!(classify_fsck(false, out), FsckOutcome::Errors);
    }

    #[test]
    fn fsck_error_markers_flag_errors_even_on_zero_exit() {
        let out = "checking extents\nerrors: 2\n";
        assert_eq!(classify_fsck(true, out), FsckOutcome::Errors);
    }

    // ── Per-device IO error counters (#457) ───────────────────

    #[test]
    fn parse_io_errors_reads_creation_block_only() {
        // The sysfs file has two blocks; only the cumulative
        // "since filesystem creation" one should be parsed. Note the
        // "checksum:0" form has no space after the colon.
        let s = "\
IO errors since filesystem creation
  read:    3
  write:   1
  checksum:2
IO errors since 8 y ago
  read:    99
  write:   99
  checksum:99
";
        assert_eq!(parse_io_errors(s), (Some(3), Some(1), Some(2)));
    }

    #[test]
    fn parse_io_errors_clean_device_is_all_zero() {
        let s = "IO errors since filesystem creation\n  read:    0\n  write:   0\n  checksum:0\n";
        assert_eq!(parse_io_errors(s), (Some(0), Some(0), Some(0)));
    }

    #[test]
    fn parse_io_errors_empty_input_is_none() {
        assert_eq!(parse_io_errors(""), (None, None, None));
    }

    #[test]
    fn parse_device_index_handles_both_formats() {
        assert_eq!(parse_device_index("Device 0:    /dev/sda"), Some(0));
        assert_eq!(parse_device_index("\tDevice 7:\t/dev/sdh"), Some(7));
        assert_eq!(
            parse_device_index("Device 2 (label ssd.fast):  /dev/sdc"),
            Some(2)
        );
        assert_eq!(parse_device_index("Label:  ssd.fast"), None);
        assert_eq!(parse_device_index("Device index: 0"), None); // not a member header
    }
}
