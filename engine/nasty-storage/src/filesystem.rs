use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::cmd;
use crate::io_scheduler::IoSchedulerState;

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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateFilesystemRequest {
    /// Name for the new filesystem; becomes the mount point directory under `/fs/`.
    pub name: String,
    /// Devices to include in the filesystem.
    pub devices: Vec<DeviceSpec>,
    /// Explicitly confirmed whole disks to prepare. Each snapshot must still
    /// match the live disk and all its children when the operation starts.
    #[serde(default)]
    pub prepare_disks: Vec<DiskPreparation>,
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
    /// Default per-device tiering label when targets are set and no device label is provided.
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

/// A filesystem registration whose persisted UUID is not currently visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnavailableFilesystem {
    pub name: String,
    /// UUID persisted in the host-side registration.
    pub uuid: String,
    #[serde(default)]
    pub devices: Vec<String>,
    pub auto_mount: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mount_error: Option<MountFailure>,
}

/// Remove an unavailable filesystem from host-side tracking only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForgetUnavailableRequest {
    pub name: String,
    /// Must exactly match the UUID currently persisted for `name`.
    pub expected_uuid: String,
    /// Must exactly match `name`.
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

/// Outcome of the most recent scrub attempt. Detailed corrected versus
/// uncorrected status lives in [`ScrubErrorKind`] so these established
/// serialized values remain readable after a NixOS generation rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrubOutcome {
    /// Completed with no reported errors.
    Ok,
    /// Completed with corrected, uncorrected, or legacy unstructured errors.
    Errors,
    /// Interrupted, signalled, returned an unknown status, failed to
    /// spawn/wait, or the engine restarted mid-scrub.
    Failed,
    /// The operator cancelled the scrub (process terminated via
    /// `scrub_cancel`); not an error condition (#553).
    Cancelled,
}

/// Error detail decoded from bcachefs-tools exit bits and final counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrubErrorKind {
    Corrected,
    Uncorrected,
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
    /// Stable ID for the active attempt, retained after it completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Exact child exit code for the most recently finished attempt.
    /// `None` when no code was available (spawn failure, signal,
    /// engine interruption).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    /// Approximate aggregate bytes repaired during the most recently
    /// finished attempt, parsed from bcachefs's rounded per-device values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_corrected_bytes: Option<u64>,
    /// Approximate aggregate bytes still unreadable after the most
    /// recently finished attempt's recovery work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_uncorrected_bytes: Option<u64>,
    /// Whether reported read errors were repaired. May accompany a
    /// `Failed` outcome when the scrub also reported interruption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_kind: Option<ScrubErrorKind>,
    /// Output of `bcachefs version` captured when the attempt started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bcachefs_tools_version: Option<String>,
    /// Running kernel release captured when the attempt started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    /// Loaded bcachefs module version captured when the attempt started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bcachefs_module_version: Option<String>,
    /// A cancellation signal was accepted for this active run. Persisted so
    /// an additional engine restart still records the eventual stop correctly.
    #[serde(default)]
    pub cancel_requested: bool,
    /// Human-readable summary string — kept for backward compatibility
    /// with the existing Diagnostics tab renderer (which reads `raw`).
    /// New WebUI surfaces should prefer the typed fields above.
    pub raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScrubCancelRequest {
    pub name: String,
    /// Run observed by the caller. Legacy callers may omit it, but current
    /// clients send it so a delayed confirmation cannot cancel a replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ScrubRunMetadata {
    run_id: String,
    bcachefs_tools_version: Option<String>,
    kernel_version: Option<String>,
    bcachefs_module_version: Option<String>,
}

#[derive(Debug)]
struct ScrubProcessResult {
    outcome: ScrubOutcome,
    error_kind: Option<ScrubErrorKind>,
    output: String,
    exit_code: Option<i32>,
    counts: Option<ScrubErrorBytes>,
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
    /// The discovered or mounted filesystem UUID differs from persisted state.
    IdentityMismatch,
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
type LocalOperationSet = Arc<std::sync::Mutex<std::collections::HashSet<String>>>;
type ScrubControls = Arc<Mutex<ScrubControlState>>;
type ScrubPersistLock = Arc<Mutex<()>>;

#[derive(Default)]
struct ScrubControlState {
    cancellations: HashMap<String, String>,
    local_runs: HashMap<String, LocalScrubRun>,
}

struct LocalScrubRun {
    run_id: String,
    spawn_attempted: bool,
}

struct LocalOperationReservation {
    operations: LocalOperationSet,
    name: String,
}

impl LocalOperationReservation {
    fn acquire(operations: &LocalOperationSet, name: String) -> Option<Self> {
        let mut active = operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.insert(name.clone()).then(|| Self {
            operations: operations.clone(),
            name,
        })
    }
}

impl Drop for LocalOperationReservation {
    fn drop(&mut self) {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.name);
    }
}

fn operation_is_owned_here(operations: &LocalOperationSet, name: &str) -> bool {
    operations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(name)
}

/// A missing child only proves interruption when the operation came from a
/// previous engine process and the state has not changed since status polling
/// took its snapshot. The latter check prevents a late poll from overwriting a
/// completion result recorded while `pgrep` was running.
fn should_record_interrupted_operation(
    observed_started_at: Option<i64>,
    current_running: bool,
    current_started_at: Option<i64>,
    owned_here: bool,
    child_alive: bool,
) -> bool {
    !owned_here && !child_alive && current_running && current_started_at == observed_started_at
}

fn scrub_process_pattern(mount: &str) -> String {
    let mut escaped = String::with_capacity(mount.len());
    for ch in mount.chars() {
        if matches!(
            ch,
            '.' | '[' | ']' | '\\' | '*' | '^' | '$' | '(' | ')' | '+' | '?' | '{' | '}' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    format!(r"(^|.*/)bcachefs scrub {escaped}$")
}

async fn scrub_process_is_alive(mount: &str) -> bool {
    scrub_process_running_known(mount).await.unwrap_or(false)
}

async fn scrub_process_running_known(mount: &str) -> Result<bool, FilesystemError> {
    let pattern = scrub_process_pattern(mount);
    let output = cmd::run("pgrep", &["-f", &pattern]).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(FilesystemError::CommandFailed(format!(
            "cannot determine whether a scrub process is running ({})",
            output.status
        ))),
    }
}

fn scrub_status_run_id(status: &ScrubStatus, name: &str) -> String {
    status
        .run_id
        .clone()
        .unwrap_or_else(|| format!("legacy:{name}"))
}

fn scrub_cancel_requested(controls: &ScrubControlState, name: &str, status: &ScrubStatus) -> bool {
    controls
        .cancellations
        .get(name)
        .is_some_and(|run_id| run_id == &scrub_status_run_id(status, name))
}

fn scrub_cancel_targets_run(expected_run_id: Option<&str>, current_run_id: &str) -> bool {
    expected_run_id.is_none_or(|expected| expected == current_run_id)
}

async fn rollback_scrub_cancel_state(
    controls: &mut ScrubControlState,
    state: &mut HashMap<String, ScrubStatus>,
    name: &str,
    run_id: &str,
) -> Result<(), String> {
    controls.cancellations.remove(name);
    if let Some(entry) = state.get_mut(name) {
        entry.cancel_requested = false;
    }
    if let Err(error) = write_scrub_state_snapshot(state).await {
        controls
            .cancellations
            .insert(name.to_string(), run_id.to_string());
        if let Some(entry) = state.get_mut(name) {
            entry.cancel_requested = true;
        }
        return Err(error);
    }
    Ok(())
}

fn loop_devices_backed_by(output: &str, mount_point: &str) -> Vec<String> {
    let prefix = format!("{}/", mount_point.trim_end_matches('/'));
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            let backing_file = fields.next()?;
            backing_file
                .starts_with(&prefix)
                .then(|| device.to_string())
        })
        .collect()
}

async fn detach_filesystem_loop_devices(mount_point: &str) -> Result<(), FilesystemError> {
    let output = cmd::run_ok(
        "losetup",
        &[
            "--list",
            "--noheadings",
            "--output",
            "NAME,BACK-FILE",
            "--raw",
        ],
    )
    .await
    .map_err(FilesystemError::CommandFailed)?;

    for device in loop_devices_backed_by(&output, mount_point) {
        info!("Detaching filesystem-backed loop device {device} before unmount");
        cmd::run_ok("losetup", &["-d", &device])
            .await
            .map_err(|e| {
                FilesystemError::CommandFailed(format!(
                    "failed to detach {device} before unmounting {mount_point}: {e}"
                ))
            })?;
    }
    Ok(())
}

const MIN_CREATE_FREE_BYTES: u64 = 1_073_741_824;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlockIdentity {
    devno: String,
    size_bytes: u64,
    dev_type: String,
    parent_devno: Option<String>,
    /// Partition start as reported by lsblk, in 512-byte sectors.
    start_512_sector: Option<u64>,
    partition_number: Option<u32>,
    partition_uuid: Option<String>,
    partition_table_uuid: Option<String>,
    disk_sequence: Option<u64>,
    serial: Option<String>,
    wwn: Option<String>,
    logical_sector_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiskPreparation {
    pub path: String,
    pub identity: BlockIdentity,
    pub partition_table_type: Option<String>,
    pub fs_type: Option<String>,
    pub children: Vec<(String, BlockIdentity, Option<String>)>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceWipeRequest {
    pub path: String,
    /// Required for whole disks; obtained from `device.prepare.inspect`.
    pub expected: Option<DiskPreparation>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiskInspectRequest {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct PreflightBlockDevice {
    path: String,
    parent_path: Option<String>,
    identity: BlockIdentity,
    fs_type: Option<String>,
    partition_table_type: Option<String>,
    read_only: bool,
    mount_points: Vec<String>,
    children: HashSet<String>,
    holders: Vec<String>,
}

#[derive(Debug)]
struct BlockInventory {
    devices: HashMap<String, PreflightBlockDevice>,
    paths: HashMap<String, String>,
}

impl BlockInventory {
    fn get_path(&self, path: &str) -> Option<&PreflightBlockDevice> {
        self.paths
            .get(path)
            .and_then(|devno| self.devices.get(devno))
    }

    fn get_identity(&self, identity: &BlockIdentity) -> Option<&PreflightBlockDevice> {
        self.devices.get(&identity.devno)
    }

    fn descendants<'a>(&'a self, root: &'a PreflightBlockDevice) -> Vec<&'a PreflightBlockDevice> {
        let mut found = Vec::new();
        let mut pending: Vec<&str> = root.children.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        while let Some(devno) = pending.pop() {
            if !seen.insert(devno) {
                continue;
            }
            if let Some(node) = self.devices.get(devno) {
                pending.extend(node.children.iter().map(String::as_str));
                found.push(node);
            }
        }
        found
    }
}

fn disk_preparation_snapshot(
    inventory: &BlockInventory,
    disk: &PreflightBlockDevice,
) -> Result<DiskPreparation, FilesystemError> {
    if disk.identity.dev_type != "disk" {
        return Err(FilesystemError::InvalidInput(format!(
            "{} is not a whole disk",
            disk.path
        )));
    }
    let mut children: Vec<_> = inventory
        .descendants(disk)
        .into_iter()
        .map(|child| {
            (
                child.path.clone(),
                child.identity.clone(),
                child.fs_type.clone(),
            )
        })
        .collect();
    children.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(DiskPreparation {
        path: disk.path.clone(),
        identity: disk.identity.clone(),
        partition_table_type: disk.partition_table_type.clone(),
        fs_type: disk.fs_type.clone(),
        children,
    })
}

fn checked_preparation<'a>(
    inventory: &'a BlockInventory,
    expected: &DiskPreparation,
) -> Result<&'a PreflightBlockDevice, FilesystemError> {
    let path = canonical_block_path(&expected.path)?;
    let disk = inventory
        .get_path(&path)
        .ok_or_else(|| FilesystemError::DeviceNotFound(expected.path.clone()))?;
    if !preparation_matches(inventory, disk, expected)? {
        return Err(FilesystemError::InvalidInput(format!(
            "{} or its partitions changed since confirmation; refresh and try again",
            expected.path
        )));
    }
    Ok(disk)
}

fn preparation_matches(
    inventory: &BlockInventory,
    disk: &PreflightBlockDevice,
    expected: &DiskPreparation,
) -> Result<bool, FilesystemError> {
    Ok(disk_preparation_snapshot(inventory, disk)? == *expected)
}

fn validate_preparation_usage(
    inventory: &BlockInventory,
    disk: &PreflightBlockDevice,
    swaps: &HashSet<String>,
    registered: &HashSet<String>,
) -> Result<(), FilesystemError> {
    if let Some(reason) = node_usage_error(inventory, disk, swaps, true, false) {
        return Err(FilesystemError::DeviceInUse(format!(
            "{} ({reason})",
            disk.path
        )));
    }
    for node in std::iter::once(disk).chain(inventory.descendants(disk)) {
        if node.identity.dev_type != "disk" && node.identity.dev_type != "part" {
            return Err(FilesystemError::DeviceInUse(format!(
                "{} has a non-partition child {}",
                disk.path, node.path
            )));
        }
        if registered.contains(&node.path) {
            return Err(FilesystemError::DeviceInUse(format!(
                "{} is referenced by a registered filesystem",
                node.path
            )));
        }
    }
    Ok(())
}

fn report_preparation_failure(
    error: FilesystemError,
    confirmed: &[String],
    started: &[String],
    prepared: &[String],
) -> FilesystemError {
    if started.is_empty() {
        return error;
    }
    let incomplete: Vec<_> = started
        .iter()
        .filter(|path| !prepared.contains(path))
        .collect();
    let unprocessed: Vec<_> = confirmed
        .iter()
        .filter(|path| !started.contains(path))
        .collect();
    FilesystemError::CommandFailed(format!(
        "{error}; prepared disks: {prepared:?}; preparation may be partial on: {incomplete:?}; not processed: {unprocessed:?}. Inspect all selected disks before retrying"
    ))
}

#[derive(Debug, Clone)]
enum CreateTargetPlan {
    Existing {
        spec: DeviceSpec,
        identity: BlockIdentity,
        prepare: Option<Box<DiskPreparation>>,
    },
    FreeSpace {
        spec: DeviceSpec,
        parent: BlockIdentity,
        partition_number: u32,
        start_sector: u64,
        end_sector: u64,
        path: String,
    },
}

#[derive(Debug, Clone)]
struct CreateFilesystemPlan {
    request: CreateFilesystemRequest,
    targets: Vec<CreateTargetPlan>,
    mount_point: String,
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn json_bool(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(|v| {
            v.as_bool()
                .or_else(|| v.as_u64().map(|n| n != 0))
                .or_else(|| v.as_str().map(|s| s == "1" || s == "true"))
        })
        .unwrap_or(false)
}

fn normalize_device_path(path: String) -> String {
    if path.starts_with('/') {
        path
    } else {
        format!("/dev/{path}")
    }
}

fn parse_mount_points(value: &serde_json::Value) -> Vec<String> {
    match value.get("mountpoints") {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::String(value)) => value
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => json_string(value, "mountpoint").into_iter().collect(),
    }
}

fn parse_lsblk_inventory(output: &str) -> Result<BlockInventory, FilesystemError> {
    let parsed: serde_json::Value = serde_json::from_str(output).map_err(|e| {
        FilesystemError::CommandFailed(format!("failed to parse lsblk device inventory: {e}"))
    })?;
    let roots = parsed
        .get("blockdevices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| FilesystemError::CommandFailed("lsblk returned no blockdevices".into()))?;

    fn collect(
        values: &[serde_json::Value],
        tree_parent: Option<&str>,
        devices: &mut HashMap<String, PreflightBlockDevice>,
        paths: &mut HashMap<String, String>,
        tree_edges: &mut Vec<(String, String)>,
    ) -> Result<(), FilesystemError> {
        for value in values {
            let devno = json_string(value, "maj:min").ok_or_else(|| {
                FilesystemError::CommandFailed("lsblk device is missing MAJ:MIN".into())
            })?;
            let path = json_string(value, "path")
                .or_else(|| json_string(value, "name"))
                .map(normalize_device_path)
                .ok_or_else(|| {
                    FilesystemError::CommandFailed("lsblk device is missing PATH".into())
                })?;
            let parent_path = json_string(value, "pkname").map(normalize_device_path);
            let dev_type = json_string(value, "type").unwrap_or_default();
            let identity = BlockIdentity {
                devno: devno.clone(),
                size_bytes: json_u64(value, "size").unwrap_or(0),
                dev_type,
                parent_devno: None,
                start_512_sector: json_u64(value, "start"),
                partition_number: json_u64(value, "partn").and_then(|n| u32::try_from(n).ok()),
                partition_uuid: json_string(value, "partuuid"),
                partition_table_uuid: json_string(value, "ptuuid"),
                disk_sequence: json_u64(value, "disk-seq"),
                serial: json_string(value, "serial"),
                wwn: json_string(value, "wwn"),
                logical_sector_bytes: json_u64(value, "log-sec").unwrap_or(512),
            };

            devices
                .entry(devno.clone())
                .or_insert_with(|| PreflightBlockDevice {
                    path: path.clone(),
                    parent_path,
                    identity,
                    fs_type: json_string(value, "fstype"),
                    partition_table_type: json_string(value, "pttype"),
                    read_only: json_bool(value, "ro"),
                    mount_points: parse_mount_points(value),
                    children: HashSet::new(),
                    holders: Vec::new(),
                });
            paths.insert(path, devno.clone());
            if let Some(name) = json_string(value, "name") {
                paths.insert(normalize_device_path(name), devno.clone());
            }
            if let Some(kname) = json_string(value, "kname") {
                paths.insert(normalize_device_path(kname), devno.clone());
            }
            if let Some(parent) = tree_parent {
                tree_edges.push((devno.clone(), parent.to_string()));
            }
            if let Some(children) = value.get("children").and_then(|v| v.as_array()) {
                collect(children, Some(&devno), devices, paths, tree_edges)?;
            }
        }
        Ok(())
    }

    let mut devices = HashMap::new();
    let mut paths = HashMap::new();
    let mut tree_edges = Vec::new();
    collect(roots, None, &mut devices, &mut paths, &mut tree_edges)?;

    let path_parents: Vec<(String, String)> = devices
        .iter()
        .filter_map(|(devno, node)| {
            let parent = node.parent_path.as_ref()?;
            Some((devno.clone(), paths.get(parent)?.clone()))
        })
        .collect();
    for (child, parent) in tree_edges.into_iter().chain(path_parents) {
        if let Some(node) = devices.get_mut(&child) {
            node.identity.parent_devno = Some(parent.clone());
        }
        if let Some(node) = devices.get_mut(&parent) {
            node.children.insert(child);
        }
    }

    Ok(BlockInventory { devices, paths })
}

async fn read_block_inventory() -> Result<BlockInventory, FilesystemError> {
    let output = cmd::run_ok(
        "lsblk",
        &[
            "--json",
            "--bytes",
            "--paths",
            "--output",
            "NAME,KNAME,PATH,MAJ:MIN,SIZE,TYPE,PKNAME,FSTYPE,PTTYPE,PTUUID,PARTUUID,PARTN,START,RO,MOUNTPOINTS,LOG-SEC,DISK-SEQ,SERIAL,WWN",
        ],
    )
    .await
    .map_err(FilesystemError::CommandFailed)?;
    let mut inventory = parse_lsblk_inventory(&output)?;

    for node in inventory.devices.values_mut() {
        let holders_path = format!("/sys/dev/block/{}/holders", node.identity.devno);
        let mut entries = tokio::fs::read_dir(&holders_path).await.map_err(|e| {
            FilesystemError::CommandFailed(format!("failed to inspect {holders_path}: {e}"))
        })?;
        while let Some(entry) = entries.next_entry().await? {
            node.holders
                .push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(inventory)
}

async fn read_active_swaps(inventory: &BlockInventory) -> Result<HashSet<String>, FilesystemError> {
    let output = cmd::run_ok(
        "swapon",
        &["--show", "--noheadings", "--raw", "--output", "NAME"],
    )
    .await
    .map_err(FilesystemError::CommandFailed)?;
    let mut swaps = HashSet::new();
    for path in output.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        if let Some(node) = inventory.get_path(&canonical.to_string_lossy()) {
            swaps.insert(node.identity.devno.clone());
        }
    }
    Ok(swaps)
}

fn validate_create_name(name: &str) -> Result<(), FilesystemError> {
    if name.is_empty() || name.len() > 32 {
        return Err(FilesystemError::InvalidInput(
            "filesystem name must be between 1 and 32 bytes".into(),
        ));
    }
    if name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err(FilesystemError::InvalidInput(
            "filesystem name may contain only ASCII letters, digits, '.', '-', and '_'".into(),
        ));
    }
    Ok(())
}

fn validate_create_label(field: &str, label: &str) -> Result<(), FilesystemError> {
    if label.is_empty()
        || label.len() > 32
        || !label
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err(FilesystemError::InvalidInput(format!(
            "{field} must be 1-32 ASCII letters, digits, '.', '-', or '_'"
        )));
    }
    Ok(())
}

fn validate_create_request(req: &CreateFilesystemRequest) -> Result<(), FilesystemError> {
    validate_create_name(&req.name)?;
    if req.devices.is_empty() {
        return Err(FilesystemError::NoDevices);
    }
    if let Some(comp) = &req.compression {
        validate_compression(comp).map_err(FilesystemError::InvalidInput)?;
    }
    if req.encryption == Some(true) && req.passphrase.as_deref().is_none_or(str::is_empty) {
        return Err(FilesystemError::InvalidInput(
            "passphrase is required when encryption=true".into(),
        ));
    }
    if req.replicas == 0 {
        return Err(FilesystemError::InvalidInput(
            "replicas must be at least 1".into(),
        ));
    }

    let mut total_durability = 0u32;
    for dev in &req.devices {
        if dev.path.is_empty() {
            return Err(FilesystemError::InvalidInput(
                "device path must not be empty".into(),
            ));
        }
        let durability = dev.durability.unwrap_or(1);
        if durability > 2 {
            return Err(FilesystemError::InvalidInput(format!(
                "device {} has invalid durability {durability}; expected 0, 1, or 2",
                dev.path
            )));
        }
        total_durability = total_durability.saturating_add(durability);
        if let Some(label) = &dev.label {
            validate_create_label("device label", label)?;
        }
    }
    if req.replicas > total_durability {
        return Err(FilesystemError::InvalidInput(format!(
            "{} replicas require at least that much total device durability (got {total_durability})",
            req.replicas
        )));
    }
    if req.erasure_code == Some(true) {
        if req.replicas < 2 {
            return Err(FilesystemError::InvalidInput(
                "erasure coding requires replicas >= 2".into(),
            ));
        }
        if req.devices.len() < (req.replicas as usize) + 1 {
            return Err(FilesystemError::InvalidInput(format!(
                "erasure coding with {} replicas requires at least {} devices (got {})",
                req.replicas,
                req.replicas + 1,
                req.devices.len()
            )));
        }
    }

    for (field, value) in [
        ("filesystem label", req.label.as_deref()),
        ("foreground target", req.foreground_target.as_deref()),
        ("metadata target", req.metadata_target.as_deref()),
        ("background target", req.background_target.as_deref()),
        ("promote target", req.promote_target.as_deref()),
    ] {
        if let Some(value) = value {
            validate_create_label(field, value)?;
        }
    }
    let targets: Vec<&str> = [
        req.foreground_target.as_deref(),
        req.metadata_target.as_deref(),
        req.background_target.as_deref(),
        req.promote_target.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !targets.is_empty() {
        let default_label = req.label.as_deref().unwrap_or(&req.name);
        let labels: Vec<&str> = req
            .devices
            .iter()
            .map(|device| device.label.as_deref().unwrap_or(default_label))
            .collect();
        for target in targets {
            let target_prefix = format!("{target}.");
            if !labels
                .iter()
                .any(|label| *label == target || label.starts_with(&target_prefix))
            {
                return Err(FilesystemError::InvalidInput(format!(
                    "tiering target '{target}' does not match any selected device label"
                )));
            }
        }
    }
    for (field, value) in [
        ("data_checksum", req.data_checksum.as_deref()),
        ("metadata_checksum", req.metadata_checksum.as_deref()),
    ] {
        if let Some(value) = value
            && !matches!(value, "none" | "crc32c" | "crc64" | "xxhash")
        {
            return Err(FilesystemError::InvalidInput(format!(
                "invalid {field} value '{value}'"
            )));
        }
    }
    for (field, value) in [
        ("bucket_size", req.bucket_size.as_deref()),
        ("encoded_extent_max", req.encoded_extent_max.as_deref()),
    ] {
        if let Some(value) = value {
            let bytes = parse_human_bytes(value).ok_or_else(|| {
                FilesystemError::InvalidInput(format!("invalid {field} value '{value}'"))
            })?;
            if bytes == 0 || !bytes.is_power_of_two() {
                return Err(FilesystemError::InvalidInput(format!(
                    "{field} must be a non-zero power-of-two size"
                )));
            }
        }
    }
    if let Some(value) = req.version_upgrade.as_deref()
        && !matches!(value, "compatible" | "incompatible" | "none")
    {
        return Err(FilesystemError::InvalidInput(format!(
            "invalid version_upgrade value '{value}'"
        )));
    }
    Ok(())
}

fn node_usage_error(
    inventory: &BlockInventory,
    node: &PreflightBlockDevice,
    swaps: &HashSet<String>,
    include_descendants: bool,
    reject_signatures: bool,
) -> Option<String> {
    let mut nodes = vec![node];
    if include_descendants {
        nodes.extend(inventory.descendants(node));
    }
    for candidate in nodes {
        if candidate.read_only {
            return Some(format!("{} is read-only", candidate.path));
        }
        if let Some(mount) = candidate.mount_points.first() {
            return Some(format!("{} is mounted at {mount}", candidate.path));
        }
        if swaps.contains(&candidate.identity.devno) {
            return Some(format!("{} is active swap", candidate.path));
        }
        if !candidate.holders.is_empty() {
            return Some(format!(
                "{} is held by {}",
                candidate.path,
                candidate.holders.join(", ")
            ));
        }
        if reject_signatures && let Some(fs_type) = candidate.fs_type.as_deref() {
            return Some(format!("{} contains a {fs_type} signature", candidate.path));
        }
    }
    None
}

fn validate_existing_create_target(
    inventory: &BlockInventory,
    node: &PreflightBlockDevice,
    swaps: &HashSet<String>,
) -> Result<(), FilesystemError> {
    if !matches!(node.identity.dev_type.as_str(), "disk" | "part") {
        return Err(FilesystemError::InvalidInput(format!(
            "{} is a {} device, not a disk or partition",
            node.path, node.identity.dev_type
        )));
    }
    if let Some(reason) = node_usage_error(inventory, node, swaps, true, true) {
        return Err(FilesystemError::DeviceInUse(format!(
            "{} ({reason})",
            node.path
        )));
    }
    if node.identity.dev_type == "disk"
        && (!node.children.is_empty() || node.partition_table_type.is_some())
    {
        return Err(FilesystemError::DeviceInUse(format!(
            "{} (whole disk has a partition table or child devices)",
            node.path
        )));
    }
    if node.identity.size_bytes == 0 {
        return Err(FilesystemError::InvalidInput(format!(
            "{} reports zero capacity",
            node.path
        )));
    }
    Ok(())
}

fn validate_free_space_parent(
    inventory: &BlockInventory,
    node: &PreflightBlockDevice,
    swaps: &HashSet<String>,
) -> Result<(), FilesystemError> {
    if node.identity.dev_type != "disk" {
        return Err(FilesystemError::InvalidInput(format!(
            "{}:free requires a whole disk, got {}",
            node.path, node.identity.dev_type
        )));
    }
    if node.partition_table_type.as_deref() != Some("gpt") {
        return Err(FilesystemError::InvalidInput(format!(
            "{}:free requires an existing GPT partition table",
            node.path
        )));
    }
    if let Some(reason) = node_usage_error(inventory, node, swaps, false, true) {
        return Err(FilesystemError::DeviceInUse(format!(
            "{} ({reason})",
            node.path
        )));
    }
    Ok(())
}

async fn has_block_signatures(path: &str) -> Result<bool, FilesystemError> {
    let output = cmd::run_ok(
        "wipefs",
        &["--no-act", "--noheadings", "--output", "TYPE", path],
    )
    .await
    .map_err(FilesystemError::CommandFailed)?;
    Ok(output.lines().any(|line| !line.trim().is_empty()))
}

fn canonical_block_path(path: &str) -> Result<String, FilesystemError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| FilesystemError::DeviceNotFound(path.to_string()))?;
    let canonical = canonical.to_string_lossy().into_owned();
    if !canonical.starts_with("/dev/") {
        return Err(FilesystemError::InvalidInput(format!(
            "{path} does not resolve to a /dev block device"
        )));
    }
    Ok(canonical)
}

fn parse_sgdisk_sector(output: &str, what: &str) -> Result<u64, FilesystemError> {
    output
        .lines()
        .find_map(|line| line.trim().parse().ok())
        .ok_or_else(|| FilesystemError::CommandFailed(format!("sgdisk returned no {what} sector")))
}

fn parse_sgdisk_partition_numbers(output: &str) -> HashSet<u32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let number = fields.next()?.parse::<u32>().ok()?;
            fields.next()?.parse::<u64>().ok()?;
            fields.next()?.parse::<u64>().ok()?;
            Some(number)
        })
        .collect()
}

fn partition_device_path(disk_path: &str, number: u32) -> String {
    if disk_path.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        format!("{disk_path}p{number}")
    } else {
        format!("{disk_path}{number}")
    }
}

async fn plan_free_partition(
    disk: &PreflightBlockDevice,
) -> Result<(u32, u64, u64, String), FilesystemError> {
    if disk.identity.logical_sector_bytes < 512
        || !disk.identity.logical_sector_bytes.is_multiple_of(512)
    {
        return Err(FilesystemError::InvalidInput(format!(
            "{} reports unsupported logical sector size {}",
            disk.path, disk.identity.logical_sector_bytes
        )));
    }
    let first = cmd::run_ok("sgdisk", &["--first-aligned-in-largest", &disk.path])
        .await
        .map_err(FilesystemError::CommandFailed)?;
    let end = cmd::run_ok("sgdisk", &["--end-of-largest", &disk.path])
        .await
        .map_err(FilesystemError::CommandFailed)?;
    let table = cmd::run_ok("sgdisk", &["--print", &disk.path])
        .await
        .map_err(FilesystemError::CommandFailed)?;
    let start_sector = parse_sgdisk_sector(&first, "first free")?;
    let end_sector = parse_sgdisk_sector(&end, "last free")?;
    if end_sector < start_sector {
        return Err(FilesystemError::InvalidInput(format!(
            "{} has no usable contiguous free space",
            disk.path
        )));
    }
    let sectors = end_sector - start_sector + 1;
    let bytes = sectors
        .checked_mul(disk.identity.logical_sector_bytes)
        .ok_or_else(|| FilesystemError::InvalidInput("free-space size overflow".into()))?;
    if bytes < MIN_CREATE_FREE_BYTES {
        return Err(FilesystemError::InvalidInput(format!(
            "{} has only {bytes} bytes in its largest aligned free extent; at least {MIN_CREATE_FREE_BYTES} are required",
            disk.path
        )));
    }
    let used = parse_sgdisk_partition_numbers(&table);
    let partition_number = (1..=128).find(|n| !used.contains(n)).ok_or_else(|| {
        FilesystemError::InvalidInput(format!("{} has no free GPT partition slots", disk.path))
    })?;
    let path = partition_device_path(&disk.path, partition_number);
    Ok((partition_number, start_sector, end_sector, path))
}

fn target_containing_disk<'a>(
    inventory: &'a BlockInventory,
    node: &'a PreflightBlockDevice,
) -> Option<&'a str> {
    if node.identity.dev_type == "disk" {
        Some(&node.identity.devno)
    } else {
        node.identity.parent_devno.as_deref().and_then(|parent| {
            inventory
                .devices
                .get(parent)
                .filter(|p| p.identity.dev_type == "disk")
                .map(|_| parent)
        })
    }
}

async fn build_create_plan(
    req: CreateFilesystemRequest,
    registered: &HashSet<String>,
) -> Result<CreateFilesystemPlan, FilesystemError> {
    validate_create_request(&req)?;
    if req.bind_to_tpm == Some(true) {
        if req.encryption != Some(true) {
            return Err(FilesystemError::InvalidInput(
                "bind_to_tpm requires encryption=true".into(),
            ));
        }
        if req.store_key == Some(false) {
            return Err(FilesystemError::InvalidInput(
                "bind_to_tpm requires store_key=true".into(),
            ));
        }
        if !nasty_common::tpm::is_available().await {
            return Err(FilesystemError::InvalidInput(
                "bind_to_tpm requested but no TPM2 is available on this host".into(),
            ));
        }
    }

    let mount_point = format!("{NASTY_MOUNT_BASE}/{}", req.name);

    let inventory = read_block_inventory().await?;
    let swaps = read_active_swaps(&inventory).await?;
    let mut confirmations = HashMap::new();
    for expected in &req.prepare_disks {
        if confirmations
            .insert(expected.path.clone(), expected)
            .is_some()
        {
            return Err(FilesystemError::InvalidInput(format!(
                "{} was confirmed more than once",
                expected.path
            )));
        }
    }
    let mut resolved = Vec::with_capacity(req.devices.len());
    for spec in &req.devices {
        let (raw_path, free) = match spec.path.strip_suffix(":free") {
            Some(path) => (path, true),
            None => (spec.path.as_str(), false),
        };
        let path = canonical_block_path(raw_path)?;
        let node = inventory
            .get_path(&path)
            .ok_or_else(|| FilesystemError::DeviceNotFound(spec.path.clone()))?;
        let prepare = confirmations.remove(&path);
        if free {
            if prepare.is_some() {
                return Err(FilesystemError::InvalidInput(format!(
                    "{}:free cannot be erased as a whole disk",
                    path
                )));
            }
            validate_free_space_parent(&inventory, node, &swaps)?;
        } else if let Some(expected) = prepare {
            checked_preparation(&inventory, expected)?;
            validate_preparation_usage(&inventory, node, &swaps, registered)?;
        } else {
            validate_existing_create_target(&inventory, node, &swaps)?;
            if has_block_signatures(&node.path).await? {
                return Err(FilesystemError::DeviceInUse(format!(
                    "{} (existing disk, partition-table, RAID, LVM, or filesystem signature)",
                    node.path
                )));
            }
        }
        resolved.push((spec.clone(), free, node, prepare.cloned()));
    }
    if let Some(path) = confirmations.keys().next() {
        return Err(FilesystemError::InvalidInput(format!(
            "{path} was confirmed for erasure but not selected as a whole disk"
        )));
    }

    let mut selected_devnos = HashSet::new();
    for (_, free, node, _) in &resolved {
        if !selected_devnos.insert(node.identity.devno.clone()) {
            return Err(FilesystemError::InvalidInput(format!(
                "{} is selected more than once, possibly through aliases",
                node.path
            )));
        }
        let Some(disk) = target_containing_disk(&inventory, node) else {
            return Err(FilesystemError::InvalidInput(format!(
                "could not determine the parent disk for {}",
                node.path
            )));
        };
        for (_, other_free, other, _) in &resolved {
            if node.identity.devno == other.identity.devno {
                continue;
            }
            if target_containing_disk(&inventory, other) == Some(disk)
                && ((!free && node.identity.dev_type == "disk")
                    || (!other_free && other.identity.dev_type == "disk"))
            {
                return Err(FilesystemError::InvalidInput(format!(
                    "{} overlaps selected device {}",
                    node.path, other.path
                )));
            }
        }
    }

    let mut targets = Vec::with_capacity(resolved.len());
    for (spec, free, node, prepare) in resolved {
        if free {
            let (partition_number, start_sector, end_sector, path) =
                plan_free_partition(node).await?;
            targets.push(CreateTargetPlan::FreeSpace {
                spec,
                parent: node.identity.clone(),
                partition_number,
                start_sector,
                end_sector,
                path,
            });
        } else {
            targets.push(CreateTargetPlan::Existing {
                spec,
                identity: node.identity.clone(),
                prepare: prepare.map(Box::new),
            });
        }
    }

    Ok(CreateFilesystemPlan {
        request: req,
        targets,
        mount_point,
    })
}

async fn reserve_create_mount_point(plan: &CreateFilesystemPlan) -> Result<(), FilesystemError> {
    tokio::fs::create_dir_all(NASTY_MOUNT_BASE).await?;
    reserve_create_mount_point_at(&plan.mount_point, &plan.request.name).await
}

async fn reserve_create_mount_point_at(
    mount_point: &str,
    filesystem_name: &str,
) -> Result<(), FilesystemError> {
    match tokio::fs::create_dir(mount_point).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = tokio::fs::symlink_metadata(mount_point).await?;
            if !metadata.is_dir() {
                return Err(FilesystemError::InvalidInput(format!(
                    "mount point {mount_point} already exists and is not a plain directory"
                )));
            }
            if is_mountpoint(mount_point).await {
                return Err(FilesystemError::InvalidInput(format!(
                    "mount point {mount_point} is occupied"
                )));
            }

            // Forget deliberately leaves the canonical directory in place.
            // Reclaim it only when remove_dir proves atomically that it is
            // empty; any raced write or operator-owned content fails closed.
            match tokio::fs::remove_dir(mount_point).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    return Err(FilesystemError::InvalidInput(format!(
                        "mount point {mount_point} already exists and is not empty"
                    )));
                }
                Err(error) => return Err(FilesystemError::Io(error)),
            }

            match tokio::fs::create_dir(mount_point).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(FilesystemError::AlreadyExists(filesystem_name.to_string()))
                }
                Err(error) => Err(FilesystemError::Io(error)),
            }
        }
        Err(e) => Err(FilesystemError::Io(e)),
    }
}

async fn verify_create_mount_point_reserved(mount_point: &str) -> Result<(), FilesystemError> {
    let metadata = tokio::fs::symlink_metadata(mount_point).await?;
    if !metadata.is_dir() || is_mountpoint(mount_point).await {
        return Err(FilesystemError::InvalidInput(format!(
            "mount point {mount_point} changed after preflight; refusing to format devices"
        )));
    }
    let mut entries = tokio::fs::read_dir(mount_point).await?;
    if entries.next_entry().await?.is_some() {
        return Err(FilesystemError::InvalidInput(format!(
            "mount point {mount_point} is no longer empty; refusing to format devices"
        )));
    }
    Ok(())
}

fn identity_changed(
    expected: &BlockIdentity,
    current: Option<&PreflightBlockDevice>,
    path: &str,
) -> Result<(), FilesystemError> {
    let current = current.ok_or_else(|| {
        FilesystemError::InvalidInput(format!(
            "device {path} disappeared after preflight; no changes were made"
        ))
    })?;
    if &current.identity != expected {
        return Err(FilesystemError::InvalidInput(format!(
            "device {path} changed after preflight; refusing destructive operation"
        )));
    }
    Ok(())
}

async fn revalidate_create_sources(plan: &CreateFilesystemPlan) -> Result<(), FilesystemError> {
    let inventory = read_block_inventory().await?;
    let swaps = read_active_swaps(&inventory).await?;
    for target in &plan.targets {
        match target {
            CreateTargetPlan::Existing {
                identity, prepare, ..
            } => {
                let current = inventory.get_identity(identity);
                identity_changed(
                    identity,
                    current,
                    current.map(|n| n.path.as_str()).unwrap_or(&identity.devno),
                )?;
                let current = current.expect("identity_changed checked presence");
                if let Some(expected) = prepare {
                    checked_preparation(&inventory, expected)?;
                    // Registered members are checked again by the caller
                    // immediately before any wipe.
                    if let Some(reason) = node_usage_error(&inventory, current, &swaps, true, false)
                    {
                        return Err(FilesystemError::DeviceInUse(format!(
                            "{} ({reason})",
                            current.path
                        )));
                    }
                } else {
                    validate_existing_create_target(&inventory, current, &swaps)?;
                    if has_block_signatures(&current.path).await? {
                        return Err(FilesystemError::DeviceInUse(format!(
                            "{} (a signature appeared after preflight)",
                            current.path
                        )));
                    }
                }
            }
            CreateTargetPlan::FreeSpace { parent, .. } => {
                let current = inventory.get_identity(parent);
                identity_changed(
                    parent,
                    current,
                    current.map(|n| n.path.as_str()).unwrap_or(&parent.devno),
                )?;
                validate_free_space_parent(
                    &inventory,
                    current.expect("identity_changed checked presence"),
                    &swaps,
                )?;
            }
        }
    }
    Ok(())
}

fn verify_created_partition(
    inventory: &BlockInventory,
    parent: &BlockIdentity,
    partition_number: u32,
    start_sector: u64,
    end_sector: u64,
    path: &str,
) -> Result<BlockIdentity, FilesystemError> {
    let node = inventory.get_path(path).ok_or_else(|| {
        FilesystemError::CommandFailed(format!(
            "created partition {path} did not appear in the kernel device inventory"
        ))
    })?;
    let expected_size = (end_sector - start_sector + 1)
        .checked_mul(parent.logical_sector_bytes)
        .ok_or_else(|| FilesystemError::InvalidInput("partition size overflow".into()))?;
    // sgdisk uses the disk's logical LBAs; lsblk START remains in 512-byte
    // sectors even with --bytes, so normalize before comparing geometry.
    let expected_start_512 = start_sector
        .checked_mul(parent.logical_sector_bytes / 512)
        .ok_or_else(|| FilesystemError::InvalidInput("partition start overflow".into()))?;
    if node.identity.dev_type != "part"
        || node.identity.parent_devno.as_deref() != Some(parent.devno.as_str())
        || node.identity.partition_number != Some(partition_number)
        || node.identity.start_512_sector != Some(expected_start_512)
        || node.identity.size_bytes != expected_size
    {
        return Err(FilesystemError::InvalidInput(format!(
            "created node {path} does not match planned partition {partition_number} ({start_sector}-{end_sector}); refusing to format it"
        )));
    }
    Ok(node.identity.clone())
}

async fn execute_partition_plan(
    plan: &CreateFilesystemPlan,
) -> Result<Vec<DeviceSpec>, FilesystemError> {
    revalidate_create_sources(plan).await?;
    let mut devices = Vec::with_capacity(plan.targets.len());
    let mut created_partitions = Vec::new();

    for target in &plan.targets {
        match target {
            CreateTargetPlan::Existing { spec, identity, .. } => {
                let mut resolved = spec.clone();
                let inventory = read_block_inventory().await?;
                let node = inventory.get_identity(identity).ok_or_else(|| {
                    FilesystemError::InvalidInput(format!(
                        "device {} disappeared after preflight",
                        spec.path
                    ))
                })?;
                resolved.path = node.path.clone();
                devices.push(resolved);
            }
            CreateTargetPlan::FreeSpace {
                spec,
                parent,
                partition_number,
                start_sector,
                end_sector,
                path,
            } => {
                // Recheck the parent immediately before changing its partition table.
                let inventory = read_block_inventory().await?;
                let swaps = read_active_swaps(&inventory).await?;
                let disk = inventory.get_identity(parent);
                identity_changed(
                    parent,
                    disk,
                    disk.map(|n| n.path.as_str()).unwrap_or(&parent.devno),
                )?;
                let disk = disk.expect("identity_changed checked presence");
                validate_free_space_parent(&inventory, disk, &swaps)?;

                let new_arg = format!("--new={partition_number}:{start_sector}:{end_sector}");
                cmd::run_ok("sgdisk", &[&new_arg, &disk.path])
                    .await
                    .map_err(FilesystemError::CommandFailed)?;
                if let Err(e) = cmd::run_ok("partprobe", &[&disk.path]).await {
                    warn!(
                        "partprobe failed after creating {path}: {e}; waiting for the exact node"
                    );
                }
                let _ = cmd::run_ok("udevadm", &["settle"]).await;

                let mut created_identity = None;
                for _ in 0..10 {
                    let current = read_block_inventory().await?;
                    if current.get_path(path).is_some() {
                        created_identity = Some(verify_created_partition(
                            &current,
                            parent,
                            *partition_number,
                            *start_sector,
                            *end_sector,
                            path,
                        )?);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let created_identity = created_identity.ok_or_else(|| {
                    FilesystemError::CommandFailed(format!(
                        "created partition {path} did not appear after partprobe"
                    ))
                })?;

                created_partitions.push((path.clone(), created_identity));
                let mut resolved = spec.clone();
                resolved.path = path.clone();
                devices.push(resolved);
            }
        }
    }

    // Do not wipe any selected free extent until every requested partition has
    // been created and matched to its exact plan. This limits partial failure
    // to GPT entries instead of erasing data before a later disk fails.
    for (path, expected) in created_partitions {
        let inventory = read_block_inventory().await?;
        let swaps = read_active_swaps(&inventory).await?;
        let current = inventory.get_path(&path);
        identity_changed(&expected, current, &path)?;
        if let Some(reason) = node_usage_error(
            &inventory,
            current.expect("identity_changed checked presence"),
            &swaps,
            true,
            false,
        ) {
            return Err(FilesystemError::DeviceInUse(format!("{path} ({reason})")));
        }
        cmd::run_ok("wipefs", &["--all", "--force", &path])
            .await
            .map_err(FilesystemError::CommandFailed)?;
    }
    Ok(devices)
}

async fn revalidate_create_targets(
    plan: &CreateFilesystemPlan,
    devices: &[DeviceSpec],
) -> Result<(), FilesystemError> {
    let inventory = read_block_inventory().await?;
    let swaps = read_active_swaps(&inventory).await?;
    for (target, device) in plan.targets.iter().zip(devices) {
        let node = inventory
            .get_path(&device.path)
            .ok_or_else(|| FilesystemError::DeviceNotFound(device.path.clone()))?;
        match target {
            CreateTargetPlan::Existing { identity, .. } => {
                identity_changed(identity, Some(node), &device.path)?;
            }
            CreateTargetPlan::FreeSpace {
                parent,
                partition_number,
                start_sector,
                end_sector,
                path,
                ..
            } => {
                if path != &device.path {
                    return Err(FilesystemError::InvalidInput(
                        "resolved partition path changed after preflight".into(),
                    ));
                }
                verify_created_partition(
                    &inventory,
                    parent,
                    *partition_number,
                    *start_sector,
                    *end_sector,
                    path,
                )?;
            }
        }
        validate_existing_create_target(&inventory, node, &swaps)?;
        if has_block_signatures(&node.path).await? {
            return Err(FilesystemError::DeviceInUse(format!(
                "{} (a signature appeared before format)",
                node.path
            )));
        }
    }
    Ok(())
}

fn build_create_format_args(req: &CreateFilesystemRequest, devices: &[DeviceSpec]) -> Vec<String> {
    let mut args = vec!["format".to_string(), format!("--fs_label={}", req.name)];
    if req.replicas > 1 {
        args.push(format!("--replicas={}", req.replicas));
    }
    if let Some(comp) = &req.compression {
        args.push(format!("--compression={comp}"));
    }
    if req.encryption == Some(true) {
        args.push("--encrypted".to_string());
    }
    for (name, target) in [
        ("foreground_target", req.foreground_target.as_deref()),
        ("metadata_target", req.metadata_target.as_deref()),
        ("background_target", req.background_target.as_deref()),
        ("promote_target", req.promote_target.as_deref()),
    ] {
        if let Some(target) = target {
            args.push(format!("--{name}={target}"));
        }
    }
    if req.erasure_code == Some(true) {
        args.push("--erasure_code".to_string());
    }
    for (name, value) in [
        ("data_checksum", req.data_checksum.as_deref()),
        ("metadata_checksum", req.metadata_checksum.as_deref()),
        ("bucket", req.bucket_size.as_deref()),
        ("encoded_extent_max", req.encoded_extent_max.as_deref()),
    ] {
        if let Some(value) = value {
            args.push(format!("--{name}={value}"));
        }
    }

    let has_targets = req.foreground_target.is_some()
        || req.metadata_target.is_some()
        || req.background_target.is_some()
        || req.promote_target.is_some();
    for dev in devices {
        if let Some(label) = &dev.label {
            args.push(format!("--label={label}"));
        } else if has_targets {
            args.push(format!(
                "--label={}",
                req.label.as_deref().unwrap_or(&req.name)
            ));
        }
        if let Some(durability) = dev.durability {
            args.push(format!("--durability={durability}"));
        }
        args.push(dev.path.clone());
    }
    args
}

#[derive(Clone)]
pub struct FilesystemService {
    list_cache: ListCache,
    /// Serializes operations that can claim, repartition, wipe, mount, or
    /// unmount block devices. Identity checks still protect against changes
    /// made by other processes, but this closes in-process lifecycle races.
    block_mutations: Arc<Mutex<()>>,
    /// Serializes manual and scheduled scrub admission. Scheduled admission
    /// performs a global recheck while holding this lock; manual admission
    /// retains its existing per-filesystem policy.
    scrub_admission: Arc<Mutex<()>>,
    /// Per-filesystem scrub state, loaded from `SCRUB_STATE_PATH` on
    /// construction. Mutated by `scrub_start` (sets `running` /
    /// `started_at`) and the spawned scrub task (records completion);
    /// read by `scrub_status` and `scrub_status_all`. The mutex is
    /// held only briefly for read/write — the actual `bcachefs scrub`
    /// child runs detached.
    scrub_state: ScrubStateMap,
    /// Serializes atomic scrub-state snapshots so an older write cannot land
    /// after a newer completion or cancellation update.
    scrub_persist: ScrubPersistLock,
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
    /// Run-scoped cancellation requests and whether this process has attempted
    /// each child spawn. Keeping both under one lock closes the cancel-before-
    /// spawn window while preserving a child result that already completed.
    scrub_controls: ScrubControls,
    /// Scrubs with completion tasks owned by this engine process. A child
    /// exits before its task drains output and records the result, so status
    /// polling must not mistake that normal window for an engine restart.
    local_scrubs: LocalOperationSet,
    /// Same ownership guard for detached fsck completion tasks.
    local_fscks: LocalOperationSet,
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
            block_mutations: Arc::new(Mutex::new(())),
            scrub_admission: Arc::new(Mutex::new(())),
            scrub_state: Arc::new(Mutex::new(scrub)),
            scrub_persist: Arc::new(Mutex::new(())),
            mount_state: Arc::new(Mutex::new(mount)),
            fsck_state: Arc::new(Mutex::new(fsck)),
            evacuating: Arc::new(Mutex::new(std::collections::HashSet::new())),
            scrub_controls: Arc::new(Mutex::new(ScrubControlState::default())),
            local_scrubs: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            local_fscks: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
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

            if opts.uuid.as_deref().is_none_or(str::is_empty) {
                let failure = missing_persisted_identity_failure(name);
                error!("{}", failure.message);
                self.record_mount_failure(name, failure).await;
                failed_names.push(name.to_string());
                continue;
            }

            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");

            if is_mountpoint(&mount_point).await {
                let expected_uuid = opts.uuid.as_deref().filter(|uuid| !uuid.is_empty());
                let actual_uuid = mounted_fs_uuid_at(&mount_point).await.ok().flatten();
                if !mounted_identity_matches(expected_uuid, actual_uuid.as_deref()) {
                    let failure = identity_mismatch_failure(
                        name,
                        expected_uuid.unwrap_or("unknown"),
                        actual_uuid.as_deref(),
                    );
                    error!("{}", failure.message);
                    self.record_mount_failure(name, failure).await;
                    failed_names.push(name.to_string());
                    continue;
                }
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
            let _mutation_guard = self.block_mutations.lock().await;
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
            let mut timing = nasty_common::diagnostics::Stage::new("filesystem.cache_lock_wait");
            let cache = self.list_cache.lock().await;
            timing.finish(true);
            drop(timing);
            if let Some((ts, ref data)) = *cache
                && ts.elapsed() < FS_LIST_CACHE_TTL
            {
                nasty_common::diagnostics::observe_pools(
                    data.iter()
                        .map(|fs| (fs.total_bytes, fs.devices.len() as u64)),
                );
                return Ok(data.clone());
            }
        }

        let mut timing = nasty_common::diagnostics::Stage::new("filesystem.discovery");
        let result = self.list_uncached().await;
        timing.finish(result.is_ok());
        drop(timing);
        let result = result?;
        nasty_common::diagnostics::observe_pools(
            result
                .iter()
                .map(|fs| (fs.total_bytes, fs.devices.len() as u64)),
        );

        {
            let mut cache = self.list_cache.lock().await;
            *cache = Some((Instant::now(), result.clone()));
        }

        Ok(result)
    }

    /// List UUID-bound registrations that are absent from live discovery.
    /// Legacy name-only registrations are intentionally omitted because they
    /// cannot be forgotten with an identity-safe request.
    pub async fn list_unavailable(&self) -> Result<Vec<UnavailableFilesystem>, FilesystemError> {
        let state = load_fs_state_strict().await?;
        let live = self.list().await?;
        let failures = self.mount_state.lock().await;
        Ok(project_unavailable_filesystems(&state, &live, &failures))
    }

    /// Uncached implementation of filesystem listing.
    async fn list_uncached(&self) -> Result<Vec<Filesystem>, FilesystemError> {
        let mounts = read_bcachefs_mounts().await?;
        let state = load_fs_state().await;

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

            let mount_name = Path::new(mount_point)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = mounted_filesystem_name(&state, &mount_name, uuid);

            seen_uuids.insert(uuid.clone());
            let uuid = uuid.clone();

            // Read per-device labels and fs options for mounted filesystems
            let fs_devices = read_fs_devices(&uuid, devices).await;
            let options = read_fs_options_sysfs(&uuid).await;

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
        let unmounted = discover_unmounted_bcachefs(&seen_uuids).await;
        for (uuid, _label, devices) in unmounted {
            // Infer filesystem name from existing mount directory or fs-state.json.
            // Note: blkid's LABEL_SUB is the bcachefs per-device tiering label
            // (e.g. "fast", "slow"), NOT the filesystem name — don't use it.
            let name = filesystem_name_for_uuid(&state, &uuid);

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
        for fs in &mut filesystems {
            if let Some(opts) = state.get(&fs.name).filter(|opts| {
                opts.uuid
                    .as_deref()
                    .filter(|uuid| !uuid.is_empty())
                    .is_some_and(|uuid| uuid == fs.uuid)
            }) {
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

        // Deterministic order so the WebUI doesn't shuffle rows on every
        // poll: the mounted set comes from HashMap iteration (unordered)
        // and the unmounted set is appended after, so without this the
        // pool list — and each pool's device table — "bounces" between
        // refreshes (#554). Sort pools by name, members by slot then path.
        filesystems.sort_by(|a, b| a.name.cmp(&b.name));
        for fs in &mut filesystems {
            fs.devices.sort_by(|a, b| {
                a.member_index
                    .cmp(&b.member_index)
                    .then(a.path.cmp(&b.path))
            });
        }

        Ok(filesystems)
    }

    /// Get a single filesystem by name
    pub async fn get(&self, name: &str) -> Result<Filesystem, FilesystemError> {
        let state = load_fs_state().await;
        let expected_uuid = match state.get(name) {
            Some(opts) => Some(
                opts.uuid
                    .as_deref()
                    .filter(|uuid| !uuid.is_empty())
                    .ok_or_else(|| {
                        FilesystemError::CommandFailed(format!(
                            "filesystem '{name}' has legacy state without a UUID; refusing a name-only operation"
                        ))
                    })?,
            ),
            None => None,
        };
        select_filesystem_for_mount(self.list().await?, name, expected_uuid)
    }

    /// Create a new bcachefs filesystem: format devices, create mount point, mount
    pub async fn create(
        &self,
        req: CreateFilesystemRequest,
    ) -> Result<Filesystem, FilesystemError> {
        let _mutation_guard = self.block_mutations.lock().await;
        let confirmed: Vec<String> = req
            .prepare_disks
            .iter()
            .map(|disk| disk.path.clone())
            .collect();
        let mut preparation_started = Vec::new();
        let mut prepared = Vec::new();
        let result = self
            .create_locked(req, &mut preparation_started, &mut prepared)
            .await;
        result.map_err(|error| {
            report_preparation_failure(error, &confirmed, &preparation_started, &prepared)
        })
    }

    async fn create_locked(
        &self,
        request: CreateFilesystemRequest,
        preparation_started: &mut Vec<String>,
        prepared: &mut Vec<String>,
    ) -> Result<Filesystem, FilesystemError> {
        let registered = self.registered_block_paths().await?;
        let mut plan = build_create_plan(request, &registered).await?;
        let mut req = plan.request.clone();
        if load_fs_state().await.contains_key(&req.name) {
            return Err(FilesystemError::AlreadyExists(req.name.clone()));
        }
        let mount_point = plan.mount_point.clone();
        reserve_create_mount_point(&plan).await?;
        // Verify every selected device before the first irreversible write.
        if let Err(error) = revalidate_create_sources(&plan).await {
            let _ = tokio::fs::remove_dir(&mount_point).await;
            return Err(error);
        }
        for target in &mut plan.targets {
            let expected = match target {
                CreateTargetPlan::Existing { prepare, .. } => prepare.clone(),
                CreateTargetPlan::FreeSpace { .. } => None,
            };
            let Some(expected) = expected else { continue };
            preparation_started.push(expected.path.clone());
            if let Err(error) = self.prepare_whole_disk(&expected).await {
                let _ = tokio::fs::remove_dir(&mount_point).await;
                return Err(error);
            }
            prepared.push(expected.path.clone());
            let inventory = read_block_inventory().await?;
            let node = inventory
                .get_path(&expected.path)
                .ok_or_else(|| FilesystemError::DeviceNotFound(expected.path.clone()))?;
            if let CreateTargetPlan::Existing {
                identity, prepare, ..
            } = target
            {
                *identity = node.identity.clone();
                *prepare = None;
            }
        }
        req.devices = match execute_partition_plan(&plan).await {
            Ok(devices) => devices,
            Err(e) => {
                let _ = tokio::fs::remove_dir(&mount_point).await;
                return Err(e);
            }
        };

        // This is the last operation before `bcachefs format`: every target
        // must still be the exact device planned above and remain unused.
        if let Err(e) = revalidate_create_targets(&plan, &req.devices).await {
            let _ = tokio::fs::remove_dir(&mount_point).await;
            return Err(e);
        }
        verify_create_mount_point_reserved(&mount_point).await?;
        let args = build_create_format_args(&req, &req.devices);

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
            let passphrase = req
                .passphrase
                .as_deref()
                .expect("encrypted request was validated before execution");
            // bcachefs format --encrypted reads passphrase twice from stdin (passphrase + confirm)
            let stdin = format!("{passphrase}\n{passphrase}\n");
            let output = match cmd::run_stdin("bcachefs", &arg_refs, stdin.as_bytes()).await {
                Ok(output) => output,
                Err(e) => {
                    let _ = tokio::fs::remove_dir(&mount_point).await;
                    return Err(FilesystemError::CommandFailed(format!(
                        "failed to execute bcachefs: {e}"
                    )));
                }
            };

            if !output.status.success() {
                // bcachefs format writes superblocks then does a trial open that
                // can race with udev, causing EBUSY on exit even though format
                // succeeded.  Check if superblocks were actually written.
                if !is_device_bcachefs(&req.devices[0].path).await {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let _ = tokio::fs::remove_dir(&mount_point).await;
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
            let output = match cmd::run("bcachefs", &arg_refs).await {
                Ok(output) => output,
                Err(e) => {
                    let _ = tokio::fs::remove_dir(&mount_point).await;
                    return Err(FilesystemError::CommandFailed(format!(
                        "failed to execute bcachefs: {e}"
                    )));
                }
            };

            if !output.status.success() {
                if !is_device_bcachefs(&req.devices[0].path).await {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let _ = tokio::fs::remove_dir(&mount_point).await;
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

        let uuid = get_fs_uuid(&req.devices[0].path)
            .await
            .filter(|uuid| !uuid.is_empty())
            .ok_or_else(|| {
                FilesystemError::CommandFailed(format!(
                    "filesystem '{}' was formatted, but its UUID could not be verified; refusing to mount it",
                    req.name
                ))
            })?;
        verify_device_paths_uuid(
            req.devices
                .iter()
                .map(|device| device.path.clone())
                .collect(),
            &uuid,
        )
        .await?;

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

        let mounted_uuid = mounted_fs_uuid_after_mount(&mount_point).await;
        if mounted_uuid.as_deref() != Some(uuid.as_str()) {
            let rollback = cmd::run_ok("umount", &[&mount_point]).await;
            let suffix = rollback
                .err()
                .map(|error| format!("; rollback unmount also failed: {error}"))
                .unwrap_or_default();
            return Err(FilesystemError::CommandFailed(format!(
                "mounted filesystem UUID did not match expected UUID {uuid}{suffix}"
            )));
        }

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
        let _mutation_guard = self.block_mutations.lock().await;

        let fs = self.get(&req.name).await?;
        if self.scrub_running_known(&req.name).await? {
            return Err(FilesystemError::CommandFailed(format!(
                "cannot destroy filesystem '{}' while a scrub is running",
                req.name
            )));
        }
        verify_filesystem_device_identity(&fs).await?;

        let mount_dir = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        if is_mountpoint(&mount_dir).await {
            verify_mountpoint_identity(&mount_dir, &fs.uuid).await?;
        }

        // Unmount if mounted
        if fs.mounted
            && let Some(ref mp) = fs.mount_point
        {
            verify_mountpoint_identity(mp, &fs.uuid).await?;
            detach_filesystem_loop_devices(mp).await?;
            info!("Unmounting filesystem '{}' from {}", req.name, mp);
            cmd::run_ok("umount", &[mp.as_str()])
                .await
                .map_err(FilesystemError::CommandFailed)?;
        }

        // Also try unmounting by UUID — catches kernel-auto-assembled filesystems
        // that the engine doesn't know are mounted (e.g. after a reboot).
        let uuid_mount = format!("UUID={}", fs.uuid);
        let _ = cmd::run_ok("umount", &[&uuid_mount]).await;

        // Remove mount point directory if it exists
        if is_mountpoint(&mount_dir).await {
            return Err(FilesystemError::CommandFailed(format!(
                "mount point {mount_dir} is still occupied; refusing to remove it"
            )));
        }
        let _ = tokio::fs::remove_dir(&mount_dir).await;

        // Wipe bcachefs superblocks from all member devices
        for dev in &fs.devices {
            info!("Wiping bcachefs superblock on {}", dev.path);
            cmd::run_ok("wipefs", &["-a", &dev.path])
                .await
                .map_err(|e| {
                    FilesystemError::CommandFailed(format!("failed to wipe {}: {e}", dev.path))
                })?;
        }

        // Forget only after every destructive step succeeded. A failed
        // destroy must retain the UUID binding so a retry cannot target a
        // different filesystem that later appears under the same name.
        forget_fs(&req.name).await;

        // Flush the kernel's blkid cache so the ghost filesystem disappears
        let _ = cmd::run_ok("udevadm", &["trigger"]).await;
        let _ = cmd::run_ok("udevadm", &["settle"]).await;

        self.invalidate_list_cache().await;
        Ok(())
    }

    /// Forget only the host-side registration for an unavailable filesystem.
    /// This deliberately leaves device contents, encryption keys, and the
    /// canonical mount directory untouched.
    pub async fn forget_unavailable(
        &self,
        req: ForgetUnavailableRequest,
    ) -> Result<(), FilesystemError> {
        let _mutation_guard = self.block_mutations.lock().await;
        let mut state = load_fs_state_strict().await?;
        validate_forget_unavailable(&state, &req)?;

        let mount_point = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        if is_mountpoint(&mount_point).await {
            return Err(FilesystemError::CommandFailed(format!(
                "mount point {mount_point} is occupied; refusing to forget the filesystem"
            )));
        }

        // Safety must distinguish a genuinely absent UUID from a failed
        // discovery command. `blkid -U` exits 2 for no match; every other
        // failure aborts instead of being treated as evidence of absence.
        if filesystem_uuid_is_visible(&req.expected_uuid).await? {
            return Err(FilesystemError::CommandFailed(format!(
                "filesystem UUID {} is visible or mounted; refusing to forget it",
                req.expected_uuid
            )));
        }

        state.remove(&req.name);
        save_fs_state(&state).await?;

        // Registration removal is authoritative. History cleanup is
        // best-effort and must not turn a completed forget into a failure.
        if self.mount_state.lock().await.remove(&req.name).is_some() {
            persist_mount_state(&self.mount_state).await;
        }
        if self.scrub_state.lock().await.remove(&req.name).is_some() {
            persist_scrub_state(&self.scrub_state, &self.scrub_persist).await;
        }
        if self.fsck_state.lock().await.remove(&req.name).is_some() {
            persist_fsck_state(&self.fsck_state).await;
        }
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
        let _mutation_guard = self.block_mutations.lock().await;
        let state = load_fs_state().await;
        if state
            .get(name)
            .is_some_and(|opts| opts.uuid.as_deref().is_none_or(str::is_empty))
        {
            return Err(FilesystemError::CommandFailed(format!(
                "filesystem '{name}' has legacy state without a UUID; refusing a name-only mount"
            )));
        }
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
        let expected_uuid = opts.uuid.as_deref().filter(|uuid| !uuid.is_empty());
        let filesystems = self.list().await?;
        let visible_uuid = filesystems
            .iter()
            .find(|filesystem| filesystem.name == name)
            .map(|filesystem| filesystem.uuid.clone());
        let fs = match select_filesystem_for_mount(filesystems, name, expected_uuid) {
            Ok(filesystem) => filesystem,
            Err(error) => {
                if let Some(expected_uuid) = expected_uuid {
                    let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
                    let mount_point_occupied = is_mountpoint(&mount_point).await;
                    let persisted_path_has_expected_uuid =
                        if visible_uuid.is_none() && !mount_point_occupied {
                            persisted_path_has_uuid(&opts.devices, expected_uuid).await
                        } else {
                            false
                        };
                    let reason = classify_unavailable_selection(
                        visible_uuid.is_some(),
                        mount_point_occupied,
                        persisted_path_has_expected_uuid,
                    );
                    let failure = if reason == MountFailureReason::MissingDevice {
                        unavailable_filesystem_failure(name, &opts.devices)
                    } else {
                        let actual_uuid = if mount_point_occupied {
                            mounted_fs_uuid_at(&mount_point).await.ok().flatten()
                        } else {
                            visible_uuid
                        };
                        identity_mismatch_failure(name, expected_uuid, actual_uuid.as_deref())
                    };
                    self.record_mount_failure(name, failure).await;
                }
                return Err(error);
            }
        };
        if fs.mounted {
            info!("Filesystem '{}' is already mounted", name);
            return Ok(fs);
        }

        let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
        if is_mountpoint(&mount_point).await {
            let actual_uuid = mounted_fs_uuid_at(&mount_point).await.ok().flatten();
            if actual_uuid.as_deref() != Some(fs.uuid.as_str()) {
                let failure = identity_mismatch_failure(name, &fs.uuid, actual_uuid.as_deref());
                let message = failure.message.clone();
                self.record_mount_failure(name, failure).await;
                return Err(FilesystemError::CommandFailed(message));
            }
            self.invalidate_list_cache().await;
            return select_filesystem_for_mount(self.list().await?, name, Some(&fs.uuid));
        }
        tokio::fs::create_dir_all(&mount_point).await?;

        if let Err(error) = verify_filesystem_device_identity(&fs).await {
            self.record_mount_failure(name, identity_mismatch_failure(name, &fs.uuid, None))
                .await;
            return Err(error);
        }

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

        let mounted_uuid = mounted_fs_uuid_after_mount(&mount_point).await;
        if mounted_uuid.as_deref() != Some(fs.uuid.as_str()) {
            let rollback = cmd::run_ok("umount", &[&mount_point]).await;
            let failure = identity_mismatch_failure(name, &fs.uuid, mounted_uuid.as_deref());
            let mut message = failure.message.clone();
            if let Err(error) = rollback {
                message.push_str(&format!(" Rollback unmount also failed: {error}"));
            }
            self.record_mount_failure(name, failure).await;
            return Err(FilesystemError::CommandFailed(message));
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
        select_filesystem_for_mount(self.list().await?, name, Some(&fs.uuid))
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

        if mount_changed {
            let mut state = load_fs_state().await;
            {
                let opts = state.entry(req.name.clone()).or_default();
                opts.uuid = Some(fs.uuid.clone());
                opts.devices = fs
                    .devices
                    .iter()
                    .map(|device| device.path.clone())
                    .collect();
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
            let mount_point = fs.mount_point.as_deref().ok_or_else(|| {
                FilesystemError::CommandFailed(format!(
                    "filesystem '{}' has no active mount point to remount",
                    req.name
                ))
            })?;
            verify_mountpoint_identity(mount_point, &fs.uuid).await?;
            let state = load_fs_state().await;
            let mount_opt_str =
                build_mount_opts(state.get(&req.name).unwrap_or(&FsMountOptions::default()));
            cmd::run_ok(
                "mount",
                &["-o", &format!("remount,{mount_opt_str}"), mount_point],
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
        let _mutation_guard = self.block_mutations.lock().await;
        info!("Unmounting filesystem '{}'", name);
        let fs = self.get(name).await?;
        if self.scrub_running_known(name).await? {
            return Err(FilesystemError::CommandFailed(format!(
                "cannot unmount filesystem '{name}' while a scrub is running"
            )));
        }
        if fs.mounted
            && let Some(ref mp) = fs.mount_point
        {
            verify_mountpoint_identity(mp, &fs.uuid).await?;
            info!("Running umount on {}", mp);
            cmd::run_ok("umount", &[mp.as_str()])
                .await
                .map_err(FilesystemError::CommandFailed)?;
            info!("Filesystem '{}' unmounted successfully", name);
        } else {
            info!("Filesystem '{}' has no mount point, skipping umount", name);
        }

        // Track mount state
        save_fs_unmounted(name, &fs).await;

        self.invalidate_list_cache().await;
        Ok(())
    }

    /// List block devices available for filesystem creation.
    pub async fn list_devices(&self) -> Result<Vec<BlockDevice>, FilesystemError> {
        self.list_devices_with_health(&[]).await
    }

    /// Merge the metrics service's cached SMART identity, without probing
    /// every drive during each device inventory request.
    pub async fn list_devices_with_health(
        &self,
        health: &[nasty_common::metrics_types::DiskHealth],
    ) -> Result<Vec<BlockDevice>, FilesystemError> {
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
                "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,PTTYPE,ROTA,MODEL,SERIAL,VENDOR,TRAN,UUID",
            ],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or(serde_json::Value::Null);

        let mut devices = Vec::new();
        let mut partition_parents = std::collections::HashMap::new();
        if let Some(blockdevices) = parsed.get("blockdevices").and_then(|v| v.as_array()) {
            let smart_by_path: HashMap<&str, &nasty_common::metrics_types::DiskHealth> = health
                .iter()
                // A megaraid endpoint may share a block path with its
                // virtual volume; never ascribe that physical drive's
                // identity to the controller's block device.
                .filter(|disk| disk.transport.is_none() && disk.smart_status != "UNAVAILABLE")
                .map(|disk| (disk.device.as_str(), disk))
                .collect();

            // Read /proc/mounts to know which devices are *actually* mounted.
            // lsblk's mountpoint field can be stale after bcachefs device removal/wipe.
            // bcachefs sources come in three shapes across module versions:
            // a plain device, a colon-joined member list (< 1.38.8), or a
            // single /dev/disk/by-uuid/<fs-uuid> (≥ 1.38.8 multi-device) that
            // must be expanded to real members via sysfs.
            let mounted_devices: std::collections::HashSet<String> =
                tokio::fs::read_to_string("/proc/mounts")
                    .await
                    .unwrap_or_default()
                    .lines()
                    .flat_map(|line| {
                        let dev_field = line.split_whitespace().next().unwrap_or("");
                        resolve_mount_devices(
                            dev_field.split(':').map(String::from).collect::<Vec<_>>(),
                            std::path::Path::new(SYSFS_BCACHEFS),
                        )
                    })
                    .collect();

            #[allow(clippy::too_many_arguments)]
            fn collect_devices(
                devs: &[serde_json::Value],
                fs_devices: &std::collections::HashSet<String>,
                mounted_devices: &std::collections::HashSet<String>,
                out: &mut Vec<BlockDevice>,
                resolver: &crate::disk_type::IdentityResolver,
                smart_by_path: &HashMap<&str, &nasty_common::metrics_types::DiskHealth>,
                overrides: &std::collections::HashMap<String, String>,
                scheduler_states: &std::collections::HashMap<String, IoSchedulerState>,
                parent_disk: Option<&str>,
                partition_parents: &mut std::collections::HashMap<String, String>,
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
                    let rota = dev.get("rota").and_then(|v| {
                        v.as_bool()
                            .or_else(|| v.as_str().map(|s| s == "1"))
                            .or_else(|| v.as_u64().map(|n| n == 1))
                    });
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

                    let smart = smart_by_path
                        .get(format!("/dev/{name}").as_str())
                        .copied()
                        .filter(|disk| smart_matches_disk(disk, serial.as_deref(), size));
                    let mut classification = classify_disk(name, rota, smart, None);

                    if dev_type == "disk" || dev_type == "part" {
                        let path = format!("/dev/{name}");
                        if dev_type == "part"
                            && let Some(parent) = parent_disk
                        {
                            partition_parents.insert(path.clone(), format!("/dev/{parent}"));
                        }
                        let in_fs = fs_devices.contains(&path);
                        let actually_mounted = mounted_devices.contains(&path);

                        // Manual disk-type override (#552) applies to whole
                        // disks only — partitions inherit nothing here.
                        let (mut stable_id, mut id_kind) = (None, None);
                        let mut type_source = "detected".to_string();
                        let mut io_scheduler = None;
                        if dev_type == "disk" {
                            let (key, kind) = resolver.resolve(name);
                            if let Some(class) = overrides.get(&key)
                                && crate::disk_type::class_to_fields(class).is_some()
                            {
                                classification = classify_disk(name, rota, smart, Some(class));
                                type_source = "manual".to_string();
                            }
                            stable_id = Some(key);
                            id_kind = Some(kind.to_string());
                            io_scheduler = scheduler_states.get(name).cloned();
                        }

                        out.push(BlockDevice {
                            path,
                            size_bytes: size,
                            dev_type: dev_type.to_string(),
                            mount_point: mountpoint,
                            fs_type: fstype,
                            partition_table_type: pick("pttype"),
                            parent_path: parent_disk.map(|name| format!("/dev/{name}")),
                            fs_uuid,
                            in_use: in_fs || actually_mounted,
                            rotational: classification.rotational,
                            device_class: classification.device_class,
                            media: classification.media,
                            media_source: classification.media_source,
                            native_interface: classification.native_interface,
                            interface_source: classification.interface_source,
                            model,
                            serial,
                            vendor,
                            transport,
                            stable_id,
                            id_kind,
                            type_source,
                            io_scheduler,
                        });
                    }

                    if let Some(children) = dev.get("children").and_then(|v| v.as_array()) {
                        let child_parent = if dev_type == "disk" {
                            Some(name)
                        } else {
                            parent_disk
                        };
                        collect_devices(
                            children,
                            fs_devices,
                            mounted_devices,
                            out,
                            resolver,
                            smart_by_path,
                            overrides,
                            scheduler_states,
                            child_parent,
                            partition_parents,
                        );
                    }
                }
            }
            // Identity (by-id/by-path) + persisted type overrides are read
            // once per listing, then applied per whole-disk inside the walk.
            let resolver = crate::disk_type::IdentityResolver::new().await;
            fn disk_names(devs: &[serde_json::Value], names: &mut Vec<String>) {
                for dev in devs {
                    if dev.get("type").and_then(|value| value.as_str()) == Some("disk")
                        && let Some(name) = dev.get("name").and_then(|value| value.as_str())
                    {
                        names.push(name.to_string());
                    }
                    if let Some(children) = dev.get("children").and_then(|value| value.as_array()) {
                        disk_names(children, names);
                    }
                }
            }
            let mut whole_disk_names = Vec::new();
            disk_names(blockdevices, &mut whole_disk_names);
            let scheduler_states =
                crate::io_scheduler::states_for_device_names(&resolver, &whole_disk_names).await;
            let overrides = crate::disk_type::load().await;
            collect_devices(
                blockdevices,
                &used_devices,
                &mounted_devices,
                &mut devices,
                &resolver,
                &smart_by_path,
                &overrides,
                &scheduler_states,
                None,
                &mut partition_parents,
            );

            // Parentage comes from lsblk's device tree. Device names may end
            // in digits, so prefix trimming is not a valid partition parser.
            let in_use_parents: std::collections::HashSet<&str> = devices
                .iter()
                .filter(|device| device.in_use && device.dev_type == "part")
                .filter_map(|device| partition_parents.get(&device.path).map(String::as_str))
                .collect();
            for device in &mut devices {
                if device.dev_type == "disk" && in_use_parents.contains(device.path.as_str()) {
                    device.in_use = true;
                }
            }
            let parent_media: HashMap<String, BlockDevice> = devices
                .iter()
                .filter(|device| device.dev_type == "disk")
                .map(|disk| (disk.path.clone(), disk.clone()))
                .collect();
            for device in &mut devices {
                if device.dev_type != "part" {
                    continue;
                }
                if let Some(parent) = device
                    .parent_path
                    .as_ref()
                    .and_then(|p| parent_media.get(p))
                {
                    device.rotational = parent.rotational;
                    device.device_class.clone_from(&parent.device_class);
                    device.media.clone_from(&parent.media);
                    device.media_source.clone_from(&parent.media_source);
                    device.native_interface.clone_from(&parent.native_interface);
                    device.interface_source.clone_from(&parent.interface_source);
                    device.transport.clone_from(&parent.transport);
                    device.type_source.clone_from(&parent.type_source);
                }
            }
        }

        // Detect unpartitioned free space on disks with existing partitions.
        // Use sgdisk to find the largest free gap; if > 1 GiB, add a virtual "free" entry.
        // Skip boot devices (mmcblk/eMMC) — they should never be offered as storage.
        let partitioned_disks: Vec<String> = partition_parents
            .values()
            .filter(|path| !path.contains("mmcblk"))
            // A disk that is itself a filesystem member (whole-disk
            // bcachefs) can't host a new partition, and probing it with
            // sgdisk only produces GPT lectures — the member superblock
            // sits where a partition table would be. Partition nodes on
            // such a disk are stale kernel state from a pre-wipe table
            // (#488).
            .filter(|parent| !used_devices.contains(*parent))
            .cloned()
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
                        let (
                            rotational,
                            device_class,
                            media,
                            media_source,
                            native_interface,
                            interface_source,
                            model,
                            serial,
                            vendor,
                            transport,
                            type_source,
                        ) = disk
                            .map(|d| {
                                (
                                    d.rotational,
                                    d.device_class.clone(),
                                    d.media.clone(),
                                    d.media_source.clone(),
                                    d.native_interface.clone(),
                                    d.interface_source.clone(),
                                    d.model.clone(),
                                    d.serial.clone(),
                                    d.vendor.clone(),
                                    d.transport.clone(),
                                    d.type_source.clone(),
                                )
                            })
                            .unwrap_or((
                                false,
                                "unknown".to_string(),
                                None,
                                "unknown".to_string(),
                                None,
                                "unknown".to_string(),
                                None,
                                None,
                                None,
                                None,
                                "detected".to_string(),
                            ));
                        devices.push(BlockDevice {
                            path: format!("{disk_path}:free"),
                            size_bytes: free_bytes,
                            dev_type: "free".to_string(),
                            mount_point: None,
                            fs_type: None,
                            partition_table_type: None,
                            parent_path: Some(disk_path.clone()),
                            fs_uuid: None,
                            in_use: false,
                            rotational,
                            device_class,
                            media,
                            media_source,
                            native_interface,
                            interface_source,
                            model,
                            serial,
                            vendor,
                            transport,
                            // Free-space pseudo-entries carry no identity of
                            // their own; they mirror the parent disk's class.
                            stable_id: None,
                            id_kind: None,
                            type_source,
                            io_scheduler: None,
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

    async fn registered_block_paths(&self) -> Result<HashSet<String>, FilesystemError> {
        // Fail closed if either persisted state or live filesystem discovery
        // cannot be read: unmounted members still belong to their pool.
        let mut paths: HashSet<String> = load_fs_state_strict()
            .await?
            .values()
            .flat_map(|state| state.devices.iter().cloned())
            .collect();
        paths.extend(
            self.list()
                .await?
                .into_iter()
                .flat_map(|fs| fs.devices.into_iter().map(|device| device.path)),
        );
        Ok(paths
            .into_iter()
            .map(|path| canonical_block_path(&path).unwrap_or(path))
            .collect())
    }

    pub async fn inspect_disks(
        &self,
        paths: &[String],
    ) -> Result<Vec<DiskPreparation>, FilesystemError> {
        let inventory = read_block_inventory().await?;
        let swaps = read_active_swaps(&inventory).await?;
        let registered = self.registered_block_paths().await?;
        paths
            .iter()
            .map(|path| {
                let path = canonical_block_path(path)?;
                let disk = inventory
                    .get_path(&path)
                    .ok_or_else(|| FilesystemError::DeviceNotFound(path.clone()))?;
                validate_preparation_usage(&inventory, disk, &swaps, &registered)?;
                disk_preparation_snapshot(&inventory, disk)
            })
            .collect()
    }

    async fn prepare_whole_disk(&self, expected: &DiskPreparation) -> Result<(), FilesystemError> {
        let inventory = read_block_inventory().await?;
        let swaps = read_active_swaps(&inventory).await?;
        let registered = self.registered_block_paths().await?;
        let disk = checked_preparation(&inventory, expected)?;
        validate_preparation_usage(&inventory, disk, &swaps, &registered)?;
        info!(
            "Preparing whole disk {} ({} child devices)",
            expected.path,
            expected.children.len()
        );

        // Clear signatures on the partitions *before* removing their table:
        // otherwise old superblocks resurface if the disk is repartitioned.
        for (path, identity, fs_type) in &expected.children {
            let current = read_block_inventory().await?;
            let swaps = read_active_swaps(&current).await?;
            let registered = self.registered_block_paths().await?;
            let parent = current.get_path(&expected.path);
            identity_changed(&expected.identity, parent, &expected.path)?;
            validate_preparation_usage(&current, parent.unwrap(), &swaps, &registered)?;
            identity_changed(identity, current.get_path(path), path)?;
            if current
                .get_path(path)
                .and_then(|node| node.fs_type.as_ref())
                != fs_type.as_ref()
            {
                return Err(FilesystemError::InvalidInput(format!(
                    "{path} signature changed since confirmation; refusing to erase it"
                )));
            }
            cmd::run_ok("wipefs", &["--all", "--force", path])
                .await
                .map_err(FilesystemError::CommandFailed)?;
        }
        let current = read_block_inventory().await?;
        let swaps = read_active_swaps(&current).await?;
        let registered = self.registered_block_paths().await?;
        let parent = current.get_path(&expected.path);
        identity_changed(&expected.identity, parent, &expected.path)?;
        validate_preparation_usage(&current, parent.unwrap(), &swaps, &registered)?;
        if parent.and_then(|node| node.fs_type.as_ref()) != expected.fs_type.as_ref() {
            return Err(FilesystemError::InvalidInput(format!(
                "{} signature changed since confirmation; refusing to erase it",
                expected.path
            )));
        }
        cmd::run_ok("wipefs", &["--all", "--force", &expected.path])
            .await
            .map_err(FilesystemError::CommandFailed)?;
        // Zap the backup GPT too; sgdisk can exit nonzero for an already
        // damaged table, so the postcondition below is authoritative.
        if let Err(error) = cmd::run_ok("sgdisk", &["--zap-all", &expected.path]).await {
            debug!("sgdisk --zap-all {}: {error}", expected.path);
        }
        cmd::run_ok("partprobe", &[&expected.path])
            .await
            .map_err(FilesystemError::CommandFailed)?;
        let _ = cmd::run_ok("udevadm", &["settle"]).await;
        for _ in 0..10 {
            let current = read_block_inventory().await?;
            if let Some(disk) = current.get_path(&expected.path)
                && disk.identity.devno == expected.identity.devno
                && disk.identity.disk_sequence == expected.identity.disk_sequence
                && disk.identity.size_bytes == expected.identity.size_bytes
                && disk.identity.serial == expected.identity.serial
                && disk.identity.wwn == expected.identity.wwn
                && disk.children.is_empty()
                && disk.partition_table_type.is_none()
                && !has_block_signatures(&disk.path).await?
            {
                self.invalidate_list_cache().await;
                info!("Whole disk {} prepared", expected.path);
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        self.invalidate_list_cache().await;
        Err(FilesystemError::CommandFailed(format!(
            "{} was wiped but partitions or signatures remain visible; inspect the device before retrying",
            expected.path
        )))
    }

    /// Clear a partition's signatures, or prepare a confirmed whole disk.
    pub async fn device_wipe(&self, request: DeviceWipeRequest) -> Result<(), FilesystemError> {
        let _mutation_guard = self.block_mutations.lock().await;
        let path = canonical_block_path(&request.path)?;
        let inventory = read_block_inventory().await?;
        let node = inventory
            .get_path(&path)
            .ok_or_else(|| FilesystemError::DeviceNotFound(path.clone()))?;
        if node.identity.dev_type == "disk" {
            let expected = request.expected.as_ref().ok_or_else(|| {
                FilesystemError::InvalidInput("whole-disk wipe requires a fresh inspection".into())
            })?;
            if expected.path != path {
                return Err(FilesystemError::InvalidInput(
                    "confirmed disk path differs".into(),
                ));
            }
            self.prepare_whole_disk(expected).await.map_err(|error| {
                FilesystemError::CommandFailed(format!(
                    "{error}; if preparation started, {} may be partly erased. Refresh the device list before retrying",
                    expected.path
                ))
            })
        } else if node.identity.dev_type == "part" {
            let identity = node.identity.clone();
            let swaps = read_active_swaps(&inventory).await?;
            let registered = self.registered_block_paths().await?;
            validate_preparation_usage(&inventory, node, &swaps, &registered)?;
            let current = read_block_inventory().await?;
            let swaps = read_active_swaps(&current).await?;
            let registered = self.registered_block_paths().await?;
            let node = current.get_path(&path);
            identity_changed(&identity, node, &path)?;
            validate_preparation_usage(&current, node.unwrap(), &swaps, &registered)?;
            cmd::run_ok("wipefs", &["--all", "--force", &path])
                .await
                .map_err(FilesystemError::CommandFailed)?;
            self.invalidate_list_cache().await;
            Ok(())
        } else {
            Err(FilesystemError::InvalidInput(format!(
                "{path} is not a disk or partition"
            )))
        }
    }

    /// Add a device to an existing mounted filesystem.
    /// bcachefs device add [--label=X] [--durability=X] <mountpoint> <device>
    pub async fn device_add(&self, req: DeviceAddRequest) -> Result<Filesystem, FilesystemError> {
        let _mutation_guard = self.block_mutations.lock().await;
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
        cmd::run_ok_bulk("bcachefs", &args)
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
            match cmd::run_ok_bulk("bcachefs", &["device", "evacuate", &device]).await {
                Ok(_) => info!("Evacuation of {} in '{}' completed", device, fs_name),
                Err(e) => warn!("Evacuation of {} in '{}' failed: {}", device, fs_name, e),
            }
            evacuating.lock().await.remove(&device);
        });

        self.invalidate_list_cache().await;
        Ok(())
    }

    /// Cancel a running device evacuation: terminate the
    /// `bcachefs device evacuate <device>` process and return the device
    /// to `rw` so it rejoins normal operation. Data already migrated off
    /// stays migrated; the device simply keeps what's left. Both steps
    /// are best-effort (`pkill` exits 1 when the child already finished).
    /// (#553)
    pub async fn device_evacuate_cancel(
        &self,
        name: &str,
        device: &str,
    ) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.devices.iter().any(|d| d.path == device) {
            return Err(FilesystemError::CommandFailed(format!(
                "{device} is not a member of filesystem '{name}'"
            )));
        }
        let pattern = format!("bcachefs device evacuate {device}");
        info!("Cancelling evacuation of {device} on '{name}' via pkill -TERM -f '{pattern}'");
        nasty_common::cmd::try_run("pkill", &["-TERM", "-f", &pattern]).await;
        self.evacuating.lock().await.remove(device);
        // Return the device to read-write so it's usable again; the
        // partial drain is harmless (replicas were preserved throughout).
        nasty_common::cmd::try_run("bcachefs", &["device", "set-state", "rw", device]).await;
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
        let _admission = self.scrub_admission.lock().await;
        let _mutation = self.block_mutations.lock().await;
        let fs = self.get(name).await?;
        self.scrub_start_admitted(name, fs).await
    }

    /// Admit a scheduled scrub only if the name still resolves to the UUID
    /// selected by the scheduler and no scrub is running (or uncertain)
    /// anywhere. Manual starts use [`Self::scrub_start`] and remain subject only
    /// to their established per-filesystem exclusion.
    pub async fn scrub_start_scheduled(
        &self,
        name: &str,
        expected_uuid: &str,
    ) -> Result<(), FilesystemError> {
        let _admission = self.scrub_admission.lock().await;
        let _mutation = self.block_mutations.lock().await;
        let fs = self.get(name).await?;
        validate_expected_scrub_uuid(name, expected_uuid, &fs.uuid)?;

        let filesystems = self.list().await.map_err(|error| {
            FilesystemError::CommandFailed(format!(
                "cannot establish global scrub state for scheduled admission: {error}"
            ))
        })?;
        for candidate in filesystems {
            let running = self
                .scrub_running_known(&candidate.name)
                .await
                .map_err(|error| {
                    FilesystemError::CommandFailed(format!(
                        "cannot establish scrub state for filesystem '{}': {error}",
                        candidate.name
                    ))
                })?;
            if running {
                return Err(FilesystemError::CommandFailed(format!(
                    "a scrub is already running on filesystem '{}'",
                    candidate.name
                )));
            }
        }

        self.scrub_start_admitted(name, fs).await
    }

    pub(crate) async fn scrub_running_known(&self, name: &str) -> Result<bool, FilesystemError> {
        let filesystem = self.get(name).await?;
        let status = self.scrub_status(name).await?;
        if status.running {
            return Ok(true);
        }
        match filesystem.mount_point.as_deref() {
            Some(mount_point) => scrub_process_running_known(mount_point).await,
            None => Ok(false),
        }
    }

    async fn scrub_start_admitted(
        &self,
        name: &str,
        fs: Filesystem,
    ) -> Result<(), FilesystemError> {
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to start scrub".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap().clone();
        let fs_name = name.to_string();

        // Reconcile persisted state first. After an engine restart, the
        // process-local reservation is empty even if the orphaned child is
        // still running.
        if self.scrub_status(name).await?.running || scrub_process_is_alive(&mount_point).await {
            return Err(FilesystemError::CommandFailed(
                "a scrub is already running on this filesystem".to_string(),
            ));
        }
        let now = unix_now_secs();
        let run = scrub_run_metadata().await;

        let ownership = LocalOperationReservation::acquire(&self.local_scrubs, fs_name.clone())
            .ok_or_else(|| {
                FilesystemError::CommandFailed(
                    "a scrub is already running on this filesystem".to_string(),
                )
            })?;
        {
            let mut controls = self.scrub_controls.lock().await;
            controls.local_runs.insert(
                fs_name.clone(),
                LocalScrubRun {
                    run_id: run.run_id.clone(),
                    spawn_attempted: false,
                },
            );
        }

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
                run_id: None,
                last_exit_code: None,
                last_corrected_bytes: None,
                last_uncorrected_bytes: None,
                last_error_kind: None,
                bcachefs_tools_version: None,
                kernel_version: None,
                bcachefs_module_version: None,
                cancel_requested: false,
                raw: "No scrub running".into(),
            });
            entry.running = true;
            entry.started_at = Some(now);
            entry.run_id = Some(run.run_id.clone());
            entry.bcachefs_tools_version = run.bcachefs_tools_version.clone();
            entry.kernel_version = run.kernel_version.clone();
            entry.bcachefs_module_version = run.bcachefs_module_version.clone();
            entry.cancel_requested = false;
            entry.raw = "Scrub in progress...".into();
        }
        persist_scrub_state(&self.scrub_state, &self.scrub_persist).await;

        let store = self.scrub_state.clone();
        let persist = self.scrub_persist.clone();
        let controls = self.scrub_controls.clone();
        info!(
            run_id = %run.run_id,
            bcachefs_tools_version = ?run.bcachefs_tools_version,
            kernel_version = ?run.kernel_version,
            bcachefs_module_version = ?run.bcachefs_module_version,
            "Starting scrub on filesystem '{name}'"
        );
        tokio::spawn(async move {
            let mount = mount_point;
            // Stream stdout+stderr line-by-line so we can pick the
            // most recent `XX%` token out of bcachefs's progress
            // updates as it runs. Falls back gracefully when the
            // binary doesn't print percent at all — the chip just
            // shows "scrubbing (Nh ago)" via the elapsed timestamp.
            let result =
                stream_scrub_and_collect(&mount, &fs_name, &store, &controls, &run.run_id).await;
            // Only turn an interruption/signal into Cancelled. If cancel
            // raced with a completed error result, preserve the errors.
            let outcome = {
                let mut control = controls.lock().await;
                let cancelled = control
                    .cancellations
                    .remove(&fs_name)
                    .is_some_and(|cancelled_run_id| cancelled_run_id == run.run_id);
                if control
                    .local_runs
                    .get(&fs_name)
                    .is_some_and(|local| local.run_id == run.run_id)
                {
                    control.local_runs.remove(&fs_name);
                }
                scrub_outcome_after_cancel(
                    result.outcome,
                    result.error_kind,
                    result.exit_code,
                    cancelled,
                )
            };
            let end = unix_now_secs();
            let duration = (end - now).max(0) as u64;
            let corrected_bytes = result.counts.map(|counts| counts.corrected_bytes);
            let uncorrected_bytes = result.counts.map(|counts| counts.uncorrected_bytes);

            match (outcome, result.error_kind) {
                (ScrubOutcome::Ok, _) => {
                    info!(run_id = %run.run_id, exit_code = ?result.exit_code, "Scrub on '{fs_name}' completed in {duration}s: ok")
                }
                (ScrubOutcome::Errors, Some(ScrubErrorKind::Corrected)) => {
                    warn!(run_id = %run.run_id, exit_code = ?result.exit_code, corrected_bytes = ?corrected_bytes, "Scrub on '{fs_name}' completed in {duration}s with corrected errors")
                }
                (ScrubOutcome::Errors, Some(ScrubErrorKind::Uncorrected)) => {
                    warn!(run_id = %run.run_id, exit_code = ?result.exit_code, corrected_bytes = ?corrected_bytes, uncorrected_bytes = ?uncorrected_bytes, "Scrub on '{fs_name}' completed in {duration}s with uncorrected errors")
                }
                (ScrubOutcome::Errors, None) => {
                    warn!(run_id = %run.run_id, exit_code = ?result.exit_code, "Scrub on '{fs_name}' completed in {duration}s: errors detected (see WebUI for full output)")
                }
                (ScrubOutcome::Failed, Some(error_kind)) => {
                    warn!(run_id = %run.run_id, exit_code = ?result.exit_code, ?error_kind, corrected_bytes = ?corrected_bytes, uncorrected_bytes = ?uncorrected_bytes, "Scrub on '{fs_name}' failed after {duration}s with reported errors: {}", result.output)
                }
                (ScrubOutcome::Failed, None) => {
                    warn!(run_id = %run.run_id, exit_code = ?result.exit_code, "Scrub on '{fs_name}' failed after {duration}s: {}", result.output)
                }
                (ScrubOutcome::Cancelled, _) => {
                    info!(run_id = %run.run_id, exit_code = ?result.exit_code, "Scrub on '{fs_name}' cancelled after {duration}s")
                }
            }

            let truncated = truncate_tail(&result.output, SCRUB_OUTPUT_KEEP_BYTES);
            let summary = match (outcome, result.error_kind) {
                (ScrubOutcome::Ok, _) => "Last scrub: ok".to_string(),
                (ScrubOutcome::Errors, Some(ScrubErrorKind::Corrected)) => {
                    "Last scrub: completed with corrected errors".to_string()
                }
                (ScrubOutcome::Errors, Some(ScrubErrorKind::Uncorrected)) => {
                    "Last scrub: completed with uncorrected errors".to_string()
                }
                (ScrubOutcome::Errors, None) => "Last scrub: errors detected".to_string(),
                (ScrubOutcome::Failed, Some(ScrubErrorKind::Corrected)) => {
                    "Last scrub: failed with corrected errors".to_string()
                }
                (ScrubOutcome::Failed, Some(ScrubErrorKind::Uncorrected)) => {
                    "Last scrub: failed with uncorrected errors".to_string()
                }
                (ScrubOutcome::Failed, None) => "Last scrub: failed".to_string(),
                (ScrubOutcome::Cancelled, _) => "Last scrub: cancelled".to_string(),
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
                    run_id: None,
                    last_exit_code: None,
                    last_corrected_bytes: None,
                    last_uncorrected_bytes: None,
                    last_error_kind: None,
                    bcachefs_tools_version: None,
                    kernel_version: None,
                    bcachefs_module_version: None,
                    cancel_requested: false,
                    raw: summary.clone(),
                });
                entry.running = false;
                entry.started_at = None;
                entry.progress_percent = None;
                entry.last_run_at = Some(end);
                entry.last_duration_secs = Some(duration);
                entry.last_outcome = Some(outcome);
                entry.last_output = Some(truncated);
                entry.run_id = Some(run.run_id.clone());
                entry.last_exit_code = result.exit_code;
                entry.last_corrected_bytes = corrected_bytes;
                entry.last_uncorrected_bytes = uncorrected_bytes;
                entry.last_error_kind = result.error_kind;
                entry.bcachefs_tools_version = run.bcachefs_tools_version.clone();
                entry.kernel_version = run.kernel_version.clone();
                entry.bcachefs_module_version = run.bcachefs_module_version.clone();
                entry.cancel_requested = false;
                entry.raw = summary;
            }
            persist_scrub_state(&store, &persist).await;
            drop(ownership);
        });

        Ok(())
    }

    /// Cancel a running scrub by terminating its `bcachefs scrub <mount>`
    /// process. Pattern-based (`pkill -f`) rather than a stored PID, for
    /// the same reason `scrub_status` uses a `pgrep` cross-check: an
    /// engine restart orphans the child, and a pattern still finds it.
    /// Flags the cancel first so the completion path records a
    /// `Cancelled` outcome instead of `Failed` (#553).
    pub async fn scrub_cancel(
        &self,
        name: &str,
        expected_run_id: Option<&str>,
    ) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        let mount = fs.mount_point.clone().ok_or_else(|| {
            FilesystemError::CommandFailed("filesystem is not mounted".to_string())
        })?;

        // Refuse if nothing is running, so the UI button can't fire a
        // stray pkill against an unrelated future scrub.
        // Hold control, persistence, and state locks through signaling.
        // Completion cannot overtake the durable cancellation marker, and a
        // replacement start cannot make `pkill` drift onto a newer run.
        let mut controls = self.scrub_controls.lock().await;
        let _persist = self.scrub_persist.lock().await;
        let mut state = self.scrub_state.lock().await;
        let run_id = state
            .get(name)
            .filter(|status| status.running)
            .map(|status| scrub_status_run_id(status, name))
            .ok_or_else(|| {
                FilesystemError::CommandFailed("no scrub is running on this filesystem".to_string())
            })?;
        if !scrub_cancel_targets_run(expected_run_id, &run_id) {
            return Err(FilesystemError::CommandFailed(
                "the scrub run changed before cancellation; refresh and try again".to_string(),
            ));
        }

        let already_requested = controls
            .cancellations
            .get(name)
            .is_some_and(|cancelled_run_id| cancelled_run_id == &run_id);
        if !already_requested {
            controls
                .cancellations
                .insert(name.to_string(), run_id.clone());
        }
        let spawn_pending = controls
            .local_runs
            .get(name)
            .is_some_and(|local| local.run_id == run_id && !local.spawn_attempted);
        if let Some(entry) = state.get_mut(name) {
            entry.cancel_requested = true;
        }
        if let Err(error) = write_scrub_state_snapshot(&state).await {
            if !already_requested {
                controls.cancellations.remove(name);
                if let Some(entry) = state.get_mut(name) {
                    entry.cancel_requested = false;
                }
            }
            return Err(FilesystemError::CommandFailed(format!(
                "failed to persist scrub cancellation before signaling: {error}"
            )));
        }

        let pattern = scrub_process_pattern(&mount);
        info!(%run_id, "Cancelling scrub on '{name}' via pkill -TERM -f '{pattern}'");
        match cmd::run("pkill", &["-TERM", "-f", &pattern]).await {
            Ok(output) if output.status.success() => {}
            // Intent is already durable, so no-match is an accepted race. A
            // local completion still preserves clean or error-bearing results;
            // a pending spawn observes the marker and skips child creation.
            Ok(output) if output.status.code() == Some(1) => {
                debug!(%run_id, status = %output.status, %already_requested, %spawn_pending, "Scrub process was absent when cancellation signal was sent");
            }
            Ok(output) => {
                let command_error = format!(
                    "failed to cancel scrub ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                if !already_requested
                    && let Err(rollback_error) =
                        rollback_scrub_cancel_state(&mut controls, &mut state, name, &run_id).await
                {
                    return Err(FilesystemError::CommandFailed(format!(
                        "{command_error}; cancellation state rollback also failed: {rollback_error}"
                    )));
                }
                return Err(FilesystemError::CommandFailed(command_error));
            }
            Err(error) => {
                let command_error = format!("failed to cancel scrub: {error}");
                if !already_requested
                    && let Err(rollback_error) =
                        rollback_scrub_cancel_state(&mut controls, &mut state, name, &run_id).await
                {
                    return Err(FilesystemError::CommandFailed(format!(
                        "{command_error}; cancellation state rollback also failed: {rollback_error}"
                    )));
                }
                return Err(FilesystemError::CommandFailed(command_error));
            }
        }
        Ok(())
    }

    /// Get scrub status for a filesystem. Merges the persisted state
    /// (last completion + the engine's view of "running") with a
    /// `pgrep` cross-check so that an engine restart during a scrub
    /// (which orphans the bcachefs child to init) is recorded as
    /// `Failed` rather than leaving the FS forever stuck in "running".
    /// If this engine cancelled an orphaned child, records `Cancelled`.
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
                run_id: None,
                last_exit_code: None,
                last_corrected_bytes: None,
                last_uncorrected_bytes: None,
                last_error_kind: None,
                bcachefs_tools_version: None,
                kernel_version: None,
                bcachefs_module_version: None,
                cancel_requested: false,
                raw: "Never scrubbed".into(),
            })
        };

        let owned_here = operation_is_owned_here(&self.local_scrubs, name);
        if status.running && !owned_here {
            // State says running — verify the child still exists. If
            // the engine restarted mid-scrub the child may have died
            // or been re-parented; either way the recorded `running`
            // is stale and we shouldn't lie about it. Cross-check uses
            // the FS's current mount point — when the FS isn't mounted
            // at all there's nothing for bcachefs scrub to be running
            // against, so we treat that as "definitely not alive".
            let alive = if let Some(mp) = fs.mount_point.as_deref() {
                scrub_process_is_alive(mp).await
            } else {
                false
            };
            if !alive {
                let end = unix_now_secs();
                let duration = status
                    .started_at
                    .map(|s| (end - s).max(0) as u64)
                    .unwrap_or(0);
                let mut persist = false;
                let mut controls = self.scrub_controls.lock().await;
                let mut state = self.scrub_state.lock().await;
                // Check ownership while the state entry is locked. A new run
                // reserves ownership before it can replace this entry.
                let owned_now = operation_is_owned_here(&self.local_scrubs, name);
                let entry = state
                    .entry(name.to_string())
                    .or_insert_with(|| status.clone());
                if should_record_interrupted_operation(
                    status.started_at,
                    entry.running,
                    entry.started_at,
                    owned_now,
                    alive,
                ) {
                    let cancelled =
                        status.cancel_requested || scrub_cancel_requested(&controls, name, &status);
                    entry.running = false;
                    entry.started_at = None;
                    entry.progress_percent = None;
                    entry.last_run_at = Some(end);
                    entry.last_duration_secs = Some(duration);
                    entry.last_outcome = Some(if cancelled {
                        ScrubOutcome::Cancelled
                    } else {
                        ScrubOutcome::Failed
                    });
                    entry.last_exit_code = None;
                    entry.last_corrected_bytes = None;
                    entry.last_uncorrected_bytes = None;
                    entry.last_error_kind = None;
                    entry.cancel_requested = false;
                    if cancelled {
                        entry.last_output = Some(
                            "The scrub process stopped after cancellation was requested.".into(),
                        );
                        entry.raw = "Last scrub: cancelled".into();
                        controls.cancellations.remove(name);
                    } else {
                        entry.last_output = Some(
                            "engine restarted while scrub was running — the bcachefs child \
                             was lost; restart the scrub if you want a fresh full pass."
                                .into(),
                        );
                        entry.raw = "Last scrub: failed (engine restart)".into();
                    }
                    persist = true;
                }
                status = entry.clone();
                drop(state);
                drop(controls);
                if persist {
                    persist_scrub_state(&self.scrub_state, &self.scrub_persist).await;
                }
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
        let ownership = LocalOperationReservation::acquire(&self.local_fscks, fs_name.clone())
            .ok_or_else(|| {
                FilesystemError::CommandFailed(
                    "an fsck is already running on this filesystem".to_string(),
                )
            })?;
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
            drop(ownership);
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

        let owned_here = operation_is_owned_here(&self.local_fscks, name);
        if status.running && !owned_here {
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
                let mut persist = false;
                let mut state = self.fsck_state.lock().await;
                let owned_now = operation_is_owned_here(&self.local_fscks, name);
                let entry = state
                    .entry(name.to_string())
                    .or_insert_with(|| status.clone());
                if should_record_interrupted_operation(
                    status.started_at,
                    entry.running,
                    entry.started_at,
                    owned_now,
                    alive,
                ) {
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
                    persist = true;
                }
                status = entry.clone();
                drop(state);
                if persist {
                    persist_fsck_state(&self.fsck_state).await;
                }
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

    /// Whether copygc (copy garbage collection) is enabled for a mounted
    /// filesystem. `None` when the kernel doesn't expose the option, so
    /// callers can hide the control rather than guess (forward-compat —
    /// e.g. `rebalance_enabled` was dropped upstream in favour of
    /// `reconcile_enabled`). (#553)
    pub async fn copygc_status(&self, name: &str) -> Result<Option<bool>, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Ok(None);
        }
        let path = format!("/sys/fs/bcachefs/{}/options/copygc_enabled", fs.uuid);
        Ok(match tokio::fs::read_to_string(&path).await {
            Ok(s) => Some(s.trim() != "0"),
            Err(_) => None,
        })
    }

    /// Enable or disable copygc on a mounted filesystem via sysfs.
    /// Pausing copygc (`enabled=false`) is the same lever nasty-top's
    /// advisor pulls on write-stalls. (#553)
    pub async fn set_copygc_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to toggle copygc".to_string(),
            ));
        }
        let path = format!("/sys/fs/bcachefs/{}/options/copygc_enabled", fs.uuid);
        let val = if enabled { "1" } else { "0" };
        info!("Setting copygc_enabled={val} on filesystem '{name}'");
        tokio::fs::write(&path, val)
            .await
            .map_err(|e| FilesystemError::CommandFailed(format!("failed to write {path}: {e}")))
    }

    /// Active data-move operations from `internal/moving_ctxts` — live
    /// progress for scrub / reconcile / copygc / evacuate (#540). Empty on
    /// an unmounted fs or when the debug file is absent/unreadable.
    pub async fn moving_ctxts(&self, name: &str) -> Vec<MoveCtx> {
        let Ok(fs) = self.get(name).await else {
            return Vec::new();
        };
        if !fs.mounted {
            return Vec::new();
        }
        let path = format!("/sys/fs/bcachefs/{}/internal/moving_ctxts", fs.uuid);
        let raw = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        parse_moving_ctxts(&raw)
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

/// One active data-move operation parsed from a bcachefs filesystem's
/// `internal/moving_ctxts` (#540). bcachefs runs scrub, reconcile,
/// copygc, and device evacuation on a shared data-move framework; each
/// registers a context here with live byte counters. This is the closest
/// thing bcachefs exposes to a `/proc/mdstat` for those operations.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveCtx {
    /// Normalized kind: `scrub` | `reconcile` | `copygc` | `evacuate` | `other`.
    pub kind: String,
    /// Keys the operation has relocated so far.
    pub keys_moved: u64,
    /// Bytes the operation has scanned/considered so far.
    pub bytes_seen: u64,
    /// Bytes it has actually relocated so far.
    pub bytes_moved: u64,
}

/// Parse `internal/moving_ctxts`. Each context is a non-indented header
/// (`scrub: ...`, `reconcile_work: ...`) followed by indented counter
/// lines (`bytes seen:`, `bytes moved:`). This is a debug interface with
/// no stability guarantee, so parse defensively: unrecognized headers
/// become `other`, missing counters stay 0, and unknown lines are ignored.
pub fn parse_moving_ctxts(raw: &str) -> Vec<MoveCtx> {
    let mut out = Vec::new();
    let mut cur: Option<MoveCtx> = None;
    for line in raw.lines() {
        let is_header =
            !line.is_empty() && !line.starts_with(char::is_whitespace) && line.contains(':');
        if is_header {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            let label = line.split(':').next().unwrap_or("").trim().to_lowercase();
            let kind = if label.contains("scrub") {
                "scrub"
            } else if label.contains("reconcile") || label.contains("rebalance") {
                "reconcile"
            } else if label.contains("copygc") || label.contains("copy_gc") {
                "copygc"
            } else if label.contains("evac") || label.contains("migrate") {
                "evacuate"
            } else {
                "other"
            };
            cur = Some(MoveCtx {
                kind: kind.to_string(),
                keys_moved: 0,
                bytes_seen: 0,
                bytes_moved: 0,
            });
        } else if let Some(c) = cur.as_mut() {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("keys moved:") {
                c.keys_moved = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = t.strip_prefix("bytes seen:") {
                c.bytes_seen = parse_human_bytes(v.trim()).unwrap_or(0);
            } else if let Some(v) = t.strip_prefix("bytes moved:") {
                c.bytes_moved = parse_human_bytes(v.trim()).unwrap_or(0);
            }
        }
    }
    if let Some(c) = cur.take() {
        out.push(c);
    }
    out
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

struct DiskClassification {
    rotational: bool,
    device_class: String,
    media: Option<String>,
    media_source: String,
    native_interface: Option<String>,
    interface_source: String,
}

fn smart_matches_disk(
    smart: &nasty_common::metrics_types::DiskHealth,
    serial: Option<&str>,
    size: u64,
) -> bool {
    (smart.capacity_bytes == 0 || smart.capacity_bytes == size)
        && (smart.serial == "Unknown" || serial.is_none_or(|serial| serial == smart.serial))
}

fn classify_disk(
    name: &str,
    sysfs_rotational: Option<bool>,
    smart: Option<&nasty_common::metrics_types::DiskHealth>,
    media_override: Option<&str>,
) -> DiskClassification {
    let (native_interface, interface_source) = if let Some(interface) = smart
        .and_then(|disk| disk.native_interface.as_deref())
        .filter(|value| matches!(*value, "sata" | "sas" | "nvme"))
    {
        (Some(interface.to_string()), "smart")
    } else if name.starts_with("nvme") {
        (Some("nvme".to_string()), "kernel")
    } else {
        (None, "unknown")
    };
    let (media, media_source) = if let Some(value) = media_override {
        // Legacy "nvme" overrides meant "fast". Keep them readable as
        // SSD media without manufacturing an NVMe interface on a VM disk.
        (Some(if value == "hdd" { "hdd" } else { "ssd" }), "manual")
    } else if let Some(rotational) = smart.and_then(|disk| disk.rotational) {
        (Some(if rotational { "hdd" } else { "ssd" }), "smart")
    } else if native_interface.as_deref() == Some("nvme") {
        (Some("ssd"), "kernel")
    } else if let Some(rotational) = sysfs_rotational {
        (Some(if rotational { "hdd" } else { "ssd" }), "sysfs")
    } else {
        (None, "unknown")
    };
    let device_class = match (media, native_interface.as_deref()) {
        (Some("ssd"), Some("nvme")) => "nvme",
        (Some(media), _) => media,
        _ => "unknown",
    };
    DiskClassification {
        rotational: media == Some("hdd"),
        device_class: device_class.to_string(),
        media: media.map(str::to_string),
        media_source: media_source.to_string(),
        native_interface,
        interface_source: interface_source.to_string(),
    }
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
    /// Partition table on whole disks, even if no filesystem signature is present.
    #[serde(default)]
    pub partition_table_type: Option<String>,
    /// Parent whole disk for partition and free-space rows.
    #[serde(default)]
    pub parent_path: Option<String>,
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
    /// Legacy class used by existing clients: "nvme", "ssd", "hdd", or "unknown".
    pub device_class: String,
    /// Physical media (hdd/ssd), independent of drive interface and path.
    #[serde(default)]
    pub media: Option<String>,
    #[serde(default)]
    pub media_source: String,
    /// Native drive interface from SMART; connection path remains `transport`.
    #[serde(default)]
    pub native_interface: Option<String>,
    #[serde(default)]
    pub interface_source: String,
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
    /// Connection path reported by lsblk (e.g. "sas" for a SATA drive
    /// through a SAS shelf). Not necessarily the native drive interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Stable identity key this disk's type override is anchored to —
    /// a unique by-id link, a by-path link, or (last resort) the /dev
    /// name. None for partitions and synthetic "free" entries. (#552)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    /// How durable `stable_id` is: `hardware` (by-id, survives re-slot),
    /// `slot` (by-path, reboot-stable but tied to the VM disk slot), or
    /// `volatile` (/dev name only — won't survive re-lettering).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_kind: Option<String>,
    /// `detected` when `device_class` came from lsblk/sysfs, `manual`
    /// when an operator override is in effect (#552).
    #[serde(default = "default_type_source")]
    pub type_source: String,
    /// Kernel I/O scheduler state for physical whole disks. Partitions and
    /// synthetic free-space rows do not own a queue and return `None`.
    #[serde(default)]
    pub io_scheduler: Option<IoSchedulerState>,
}

fn default_type_source() -> String {
    "detected".to_string()
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
        if let Some(sy) = find_sysfs_member(dev_path, &sysfs_by_path).await {
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

        // On a mounted pool every *attached* member is in sysfs_by_path
        // after resolving mount-source aliases,
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

/// Match persistent mount-source aliases to sysfs's live kernel paths.
/// Keep the original path on the returned filesystem row; an unresolved
/// alias must not match a different member by label or basename.
async fn find_sysfs_member<'a>(
    path: &str,
    members: &HashMap<&str, &'a DeviceSysfs>,
) -> Option<&'a DeviceSysfs> {
    if let Some(member) = members.get(path) {
        return Some(*member);
    }
    let resolved = tokio::fs::canonicalize(path).await.ok()?;
    members.get(resolved.to_str()?).copied()
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

/// If the parsed mount source is the bcachefs ≥1.38.8 multi-device
/// form — a single `/dev/disk/by-uuid/<fs-uuid>` entry — return the
/// UUID. Older modules report the colon-joined member list instead,
/// and single-device mounts a plain device path; both return None.
fn by_uuid_source(devices: &[String]) -> Option<&str> {
    match devices {
        [only] => only.strip_prefix("/dev/disk/by-uuid/"),
        _ => None,
    }
}

/// Resolve a mounted filesystem's member devices from sysfs:
/// `<base>/<uuid>/dev-N/block` symlinks point at the members' block
/// device sysfs nodes, whose basename is the kernel device name.
/// Returns None when the filesystem has no sysfs presence (not
/// mounted, or gone by the time we look). Ordered by member index so
/// output is stable across udev enumeration order.
fn sysfs_fs_members(base: &std::path::Path, uuid: &str) -> Option<Vec<String>> {
    let fs_dir = base.join(uuid);
    let entries = std::fs::read_dir(&fs_dir).ok()?;
    let mut members: Vec<(u32, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(idx) = name
            .to_str()
            .and_then(|n| n.strip_prefix("dev-"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(target) = std::fs::read_link(entry.path().join("block")) else {
            continue;
        };
        if let Some(dev_name) = target.file_name().and_then(|n| n.to_str()) {
            members.push((idx, format!("/dev/{dev_name}")));
        }
    }
    if members.is_empty() {
        return None;
    }
    members.sort_by_key(|(idx, _)| *idx);
    Some(members.into_iter().map(|(_, path)| path).collect())
}

/// Replace a by-uuid mount source with the real member list from
/// sysfs. Colon-list and plain-device sources pass through untouched;
/// a by-uuid source whose sysfs entry is missing (unmount race) also
/// passes through — a stale-looking path beats claiming the
/// filesystem has no mounted devices at all.
fn resolve_mount_devices(devices: Vec<String>, sysfs_base: &std::path::Path) -> Vec<String> {
    match by_uuid_source(&devices).and_then(|uuid| sysfs_fs_members(sysfs_base, uuid)) {
        Some(members) => members,
        None => devices,
    }
}

/// Where the kernel exposes mounted bcachefs filesystems.
const SYSFS_BCACHEFS: &str = "/sys/fs/bcachefs";

/// Parse /proc/mounts for bcachefs entries.
/// Returns map of mount_point -> list of devices.
async fn read_bcachefs_mounts() -> Result<HashMap<String, Vec<String>>, FilesystemError> {
    let content = tokio::fs::read_to_string("/proc/mounts")
        .await
        .unwrap_or_default();
    let mounts = content
        .lines()
        .filter_map(parse_bcachefs_mount_line)
        .map(|m| {
            (
                m.mount_point,
                resolve_mount_devices(m.devices, std::path::Path::new(SYSFS_BCACHEFS)),
            )
        })
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

fn filesystem_name_for_uuid(state: &FsState, uuid: &str) -> String {
    find_fs_name_by_uuid(state, uuid).unwrap_or_else(|| uuid.chars().take(8).collect::<String>())
}

fn mounted_filesystem_name(state: &FsState, mount_name: &str, uuid: &str) -> String {
    match state
        .get(mount_name)
        .and_then(|opts| opts.uuid.as_deref())
        .filter(|expected_uuid| *expected_uuid != uuid)
    {
        Some(_) => filesystem_name_for_uuid(state, uuid),
        None => mount_name.to_string(),
    }
}

fn select_filesystem_by_name(
    filesystems: Vec<Filesystem>,
    name: &str,
) -> Result<Filesystem, FilesystemError> {
    let mut matches = filesystems.into_iter().filter(|fs| fs.name == name);
    let filesystem = matches
        .next()
        .ok_or_else(|| FilesystemError::NotFound(name.to_string()))?;
    if matches.next().is_some() {
        return Err(FilesystemError::InvalidInput(format!(
            "filesystem name '{name}' is ambiguous; identify the filesystem by UUID"
        )));
    }
    Ok(filesystem)
}

fn select_filesystem_for_mount(
    filesystems: Vec<Filesystem>,
    name: &str,
    expected_uuid: Option<&str>,
) -> Result<Filesystem, FilesystemError> {
    if let Some(expected_uuid) = expected_uuid {
        return filesystems
            .into_iter()
            .find(|fs| fs.uuid == expected_uuid)
            .ok_or_else(|| {
                FilesystemError::CommandFailed(format!(
                    "filesystem '{name}' with expected UUID {expected_uuid} is not available; refusing to mount a different filesystem"
                ))
            });
    }
    select_filesystem_by_name(filesystems, name)
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
    /// Retained only so unresolved scheduler migration data survives an
    /// unrelated filesystem-state rewrite.
    #[serde(
        default,
        rename = "io_scheduler",
        skip_serializing_if = "Option::is_none"
    )]
    legacy_io_scheduler: Option<String>,
}

/// Filesystem state: maps fs name → mount options.
type FsState = HashMap<String, FsMountOptions>;

fn project_unavailable_filesystems(
    state: &FsState,
    live: &[Filesystem],
    failures: &HashMap<String, MountFailure>,
) -> Vec<UnavailableFilesystem> {
    let live_uuids: HashSet<&str> = live
        .iter()
        .map(|filesystem| filesystem.uuid.as_str())
        .collect();
    let mut unavailable = state
        .iter()
        .filter_map(|(name, options)| {
            let uuid = options.uuid.as_deref().filter(|uuid| !uuid.is_empty())?;
            (!live_uuids.contains(uuid)).then(|| UnavailableFilesystem {
                name: name.clone(),
                uuid: uuid.to_string(),
                devices: options.devices.clone(),
                auto_mount: options.mounted != Some(false),
                last_mount_error: failures.get(name).cloned(),
            })
        })
        .collect::<Vec<_>>();
    unavailable.sort_by(|a, b| a.name.cmp(&b.name).then(a.uuid.cmp(&b.uuid)));
    unavailable
}

fn validate_forget_unavailable(
    state: &FsState,
    req: &ForgetUnavailableRequest,
) -> Result<(), FilesystemError> {
    if req.confirm_name != req.name {
        return Err(FilesystemError::InvalidInput(
            "confirmation name does not match filesystem name".into(),
        ));
    }
    let options = state
        .get(&req.name)
        .ok_or_else(|| FilesystemError::NotFound(req.name.clone()))?;
    let persisted_uuid = options
        .uuid
        .as_deref()
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| {
            FilesystemError::InvalidInput(format!(
                "filesystem '{}' has legacy state without a UUID and cannot be safely forgotten",
                req.name
            ))
        })?;
    if persisted_uuid != req.expected_uuid {
        return Err(FilesystemError::InvalidInput(format!(
            "expected UUID does not match the persisted UUID for filesystem '{}'",
            req.name
        )));
    }
    Ok(())
}

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
/// etc. so the next manual `mount` doesn't start
/// from defaults. Pre-fix this function removed the entry entirely,
/// which silently wiped the operator's config (and produced the
/// "encrypted=None at boot → systemd-ask-password deadlock" reported
/// on 10.10.10.71 after a passing unmount/mount cycle).
async fn save_fs_unmounted(fs_name: &str, fs: &Filesystem) {
    let mut state = load_fs_state().await;
    let opts = state.entry(fs_name.to_string()).or_default();
    opts.uuid = Some(fs.uuid.clone());
    opts.devices = fs
        .devices
        .iter()
        .map(|device| device.path.clone())
        .collect();
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
    match load_fs_state_strict().await {
        Ok(state) => state,
        Err(error) => {
            warn!("{error} — using empty state");
            FsState::new()
        }
    }
}

async fn load_fs_state_strict() -> Result<FsState, FilesystemError> {
    let content = match tokio::fs::read_to_string(FS_STATE_PATH).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(FsState::new()),
        Err(error) => {
            return Err(FilesystemError::CommandFailed(format!(
                "read {FS_STATE_PATH} failed: {error}"
            )));
        }
    };
    serde_json::from_str(&content).map_err(|error| {
        FilesystemError::CommandFailed(format!("parse {FS_STATE_PATH} failed: {error}"))
    })
}

async fn save_fs_state(state: &FsState) -> Result<(), FilesystemError> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| FilesystemError::CommandFailed(e.to_string()))?;
    tokio::fs::write(FS_STATE_PATH, json).await?;
    Ok(())
}

/// Persist the in-memory scrub state map with serialized atomic replacement.
/// Best-effort for ordinary status updates; cancellation uses the strict
/// snapshot helper directly because its intent must land before signaling.
async fn persist_scrub_state(store: &ScrubStateMap, persist: &ScrubPersistLock) {
    let _persist = persist.lock().await;
    let snapshot = store.lock().await.clone();
    if let Err(error) = write_scrub_state_snapshot(&snapshot).await {
        warn!("{error}");
    }
}

async fn write_scrub_state_snapshot(snapshot: &HashMap<String, ScrubStatus>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|error| format!("serialize scrub state failed: {error}"))?;
    let temp_path = format!("{SCRUB_STATE_PATH}.tmp");
    tokio::fs::write(&temp_path, json)
        .await
        .map_err(|error| format!("write {SCRUB_STATE_PATH} failed: {error}"))?;
    tokio::fs::rename(&temp_path, SCRUB_STATE_PATH)
        .await
        .map_err(|error| format!("replace {SCRUB_STATE_PATH} failed: {error}"))?;
    Ok(())
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

fn mounted_identity_matches(expected_uuid: Option<&str>, actual_uuid: Option<&str>) -> bool {
    expected_uuid.is_some_and(|expected| actual_uuid == Some(expected))
}

fn missing_persisted_identity_failure(name: &str) -> MountFailure {
    let message = format!(
        "Filesystem '{name}' has legacy state without a UUID. NASty refused to restore it by name because another pool could now use the same device paths."
    );
    MountFailure {
        attempted_at: unix_now_secs(),
        reason: MountFailureReason::IdentityMismatch,
        message: message.clone(),
        missing_devices: Vec::new(),
        raw: message,
    }
}

fn identity_mismatch_failure(
    name: &str,
    expected_uuid: &str,
    actual_uuid: Option<&str>,
) -> MountFailure {
    let actual = actual_uuid.unwrap_or("an unknown or non-bcachefs filesystem");
    let message = format!(
        "Mount point {NASTY_MOUNT_BASE}/{name} belongs to {actual}, but '{name}' is tracked as UUID {expected_uuid}. NASty refused to use this mount."
    );
    MountFailure {
        attempted_at: unix_now_secs(),
        reason: MountFailureReason::IdentityMismatch,
        message: message.clone(),
        missing_devices: Vec::new(),
        raw: message,
    }
}

fn classify_unavailable_selection(
    conflicting_visible_identity: bool,
    canonical_mountpoint_occupied: bool,
    persisted_path_has_expected_uuid: bool,
) -> MountFailureReason {
    if conflicting_visible_identity
        || canonical_mountpoint_occupied
        || persisted_path_has_expected_uuid
    {
        MountFailureReason::IdentityMismatch
    } else {
        MountFailureReason::MissingDevice
    }
}

fn unavailable_filesystem_failure(name: &str, paths: &[String]) -> MountFailure {
    let missing_devices = paths
        .iter()
        .map(|path| MissingDevice {
            path: path.clone(),
            member_index: None,
            label: None,
        })
        .collect::<Vec<_>>();
    let message = if missing_devices.is_empty() {
        format!(
            "Filesystem '{name}' is not visible and has no available last-known member devices."
        )
    } else {
        classify_mount_failure("", &missing_devices).1
    };
    MountFailure {
        attempted_at: unix_now_secs(),
        reason: MountFailureReason::MissingDevice,
        message: message.clone(),
        missing_devices,
        raw: message,
    }
}

async fn persisted_path_has_uuid(paths: &[String], expected_uuid: &str) -> bool {
    for path in paths {
        if Path::new(path).exists() && get_fs_uuid(path).await.as_deref() == Some(expected_uuid) {
            return true;
        }
    }
    false
}

async fn filesystem_uuid_is_visible(uuid: &str) -> Result<bool, FilesystemError> {
    let output = tokio::process::Command::new("blkid")
        .args(["-U", uuid])
        .output()
        .await
        .map_err(FilesystemError::Io)?;
    if classify_blkid_uuid_probe(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )? {
        return Ok(true);
    }

    // bcachefs 1.38+ can make blkid miss mounted devices. A strict lsblk
    // fallback prevents an alternate mount from being mistaken for absence.
    let output = tokio::process::Command::new("lsblk")
        .args(["--json", "--output", "UUID"])
        .output()
        .await
        .map_err(FilesystemError::Io)?;
    if !output.status.success() {
        return Err(FilesystemError::CommandFailed(format!(
            "lsblk UUID probe failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    lsblk_output_has_uuid(&String::from_utf8_lossy(&output.stdout), uuid)
}

fn classify_blkid_uuid_probe(
    status_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<bool, FilesystemError> {
    match status_code {
        Some(0) if !stdout.trim().is_empty() => Ok(true),
        Some(2) if stderr.trim().is_empty() => Ok(false),
        code => Err(FilesystemError::CommandFailed(format!(
            "blkid UUID probe failed with status {code:?}: {}",
            stderr.trim()
        ))),
    }
}

fn lsblk_output_has_uuid(output: &str, expected_uuid: &str) -> Result<bool, FilesystemError> {
    let parsed: serde_json::Value = serde_json::from_str(output).map_err(|error| {
        FilesystemError::CommandFailed(format!("parse lsblk UUID probe: {error}"))
    })?;
    let roots = parsed
        .get("blockdevices")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            FilesystemError::CommandFailed("lsblk UUID probe returned no blockdevices".into())
        })?;

    fn contains_uuid(nodes: &[serde_json::Value], expected_uuid: &str) -> bool {
        nodes.iter().any(|node| {
            node.get("uuid").and_then(serde_json::Value::as_str) == Some(expected_uuid)
                || node
                    .get("children")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|children| contains_uuid(children, expected_uuid))
        })
    }

    Ok(contains_uuid(roots, expected_uuid))
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

fn first_nonempty_line(value: String) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

async fn read_trimmed(path: &str) -> Option<String> {
    tokio::fs::read_to_string(path)
        .await
        .ok()
        .and_then(first_nonempty_line)
}

async fn scrub_run_metadata() -> ScrubRunMetadata {
    let tools = async {
        cmd::run_ok("bcachefs", &["version"])
            .await
            .ok()
            .and_then(first_nonempty_line)
    };
    let module = async {
        if let Some(version) = read_trimmed("/sys/module/bcachefs/version").await {
            return Some(version);
        }
        cmd::run_ok("modinfo", &["bcachefs", "--field", "version"])
            .await
            .ok()
            .and_then(first_nonempty_line)
    };
    let (bcachefs_tools_version, kernel_version, bcachefs_module_version) =
        tokio::join!(tools, read_trimmed("/proc/sys/kernel/osrelease"), module,);
    ScrubRunMetadata {
        run_id: uuid::Uuid::new_v4().to_string(),
        bcachefs_tools_version,
        kernel_version,
        bcachefs_module_version,
    }
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

const SCRUB_EXIT_INTERRUPTED: i32 = 1;
const SCRUB_EXIT_CORRECTED: i32 = 2;
const SCRUB_EXIT_UNCORRECTED: i32 = 4;
const SCRUB_EXIT_KNOWN_MASK: i32 =
    SCRUB_EXIT_INTERRUPTED | SCRUB_EXIT_CORRECTED | SCRUB_EXIT_UNCORRECTED;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ScrubErrorBytes {
    corrected_bytes: u64,
    uncorrected_bytes: u64,
    device_offline: bool,
}

fn parse_scrub_error_bytes(output: &str) -> Option<ScrubErrorBytes> {
    let mut columns = None;
    let mut totals = ScrubErrorBytes::default();
    let mut rows = 0;

    for line in output.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if columns.is_none() {
            let corrected = fields.iter().position(|field| *field == "corrected");
            let uncorrected = fields.iter().position(|field| *field == "uncorrected");
            if let (Some(corrected), Some(uncorrected)) = (corrected, uncorrected) {
                columns = Some((corrected, uncorrected));
            }
            continue;
        }

        let (corrected_column, uncorrected_column) = columns.unwrap();
        let complete = fields.contains(&"complete");
        let offline = fields.contains(&"offline");
        if !complete && !offline {
            continue;
        }
        let Some(corrected) = fields
            .get(corrected_column)
            .and_then(|value| parse_human_bytes(value))
        else {
            continue;
        };
        let Some(uncorrected) = fields
            .get(uncorrected_column)
            .and_then(|value| parse_human_bytes(value))
        else {
            continue;
        };
        totals.corrected_bytes = totals.corrected_bytes.saturating_add(corrected);
        totals.uncorrected_bytes = totals.uncorrected_bytes.saturating_add(uncorrected);
        totals.device_offline |= offline;
        rows += 1;
    }

    (rows > 0).then_some(totals)
}

fn classify_scrub_result(
    exit_code: Option<i32>,
    counts: Option<ScrubErrorBytes>,
    output: &str,
) -> (ScrubOutcome, Option<ScrubErrorKind>) {
    let exit_uncorrected = exit_code.is_some_and(|code| code & SCRUB_EXIT_UNCORRECTED != 0);
    let exit_corrected = exit_code.is_some_and(|code| code & SCRUB_EXIT_CORRECTED != 0);
    let error_kind = if exit_uncorrected || counts.is_some_and(|value| value.uncorrected_bytes > 0)
    {
        Some(ScrubErrorKind::Uncorrected)
    } else if exit_corrected || counts.is_some_and(|value| value.corrected_bytes > 0) {
        Some(ScrubErrorKind::Corrected)
    } else {
        None
    };
    let interrupted = exit_code.is_none_or(|code| {
        code < 0 || code & SCRUB_EXIT_INTERRUPTED != 0 || code & !SCRUB_EXIT_KNOWN_MASK != 0
    }) || counts.is_some_and(|value| value.device_offline);
    let outcome = if interrupted {
        ScrubOutcome::Failed
    } else if error_kind.is_some() || combined_indicates_errors(output) {
        ScrubOutcome::Errors
    } else {
        ScrubOutcome::Ok
    };
    (outcome, error_kind)
}

fn scrub_outcome_after_cancel(
    outcome: ScrubOutcome,
    error_kind: Option<ScrubErrorKind>,
    exit_code: Option<i32>,
    cancel_requested: bool,
) -> ScrubOutcome {
    let interrupted = exit_code.is_none_or(|code| code & SCRUB_EXIT_INTERRUPTED != 0);
    if cancel_requested && interrupted && error_kind.is_none() && outcome == ScrubOutcome::Failed {
        ScrubOutcome::Cancelled
    } else {
        outcome
    }
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
/// return the classified result and full captured transcript on exit.
async fn stream_scrub_and_collect(
    mount: &str,
    fs_name: &str,
    store: &ScrubStateMap,
    controls: &ScrubControls,
    run_id: &str,
) -> ScrubProcessResult {
    use tokio::io::AsyncReadExt;

    // Serialize the spawn decision with cancellation. If cancel won the
    // lock first, do not create a child after pkill already reported no match.
    let mut control = controls.lock().await;
    let cancelled_before_spawn = control
        .cancellations
        .get(fs_name)
        .is_some_and(|cancelled_run_id| cancelled_run_id == run_id);
    if let Some(local) = control
        .local_runs
        .get_mut(fs_name)
        .filter(|local| local.run_id == run_id)
    {
        local.spawn_attempted = true;
    }
    if cancelled_before_spawn {
        drop(control);
        return ScrubProcessResult {
            outcome: ScrubOutcome::Failed,
            error_kind: None,
            output: "scrub cancelled before the bcachefs process started".to_string(),
            exit_code: None,
            counts: None,
        };
    }
    let child = nasty_common::priority::bulk_command("bcachefs")
        .args(["scrub", mount])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    drop(control);
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return ScrubProcessResult {
                outcome: ScrubOutcome::Failed,
                error_kind: None,
                output: format!("failed to spawn bcachefs scrub: {e}"),
                exit_code: None,
                counts: None,
            };
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
            return ScrubProcessResult {
                outcome: ScrubOutcome::Failed,
                error_kind: None,
                output: format!("bcachefs scrub child wait failed: {e}"),
                exit_code: None,
                counts: None,
            };
        }
    };
    let _ = drain_handle.await;

    let captured = capture.lock().map(|g| g.render()).unwrap_or_default();
    let exit_code = status.code();
    let counts = parse_scrub_error_bytes(&captured);
    let (outcome, error_kind) = classify_scrub_result(exit_code, counts, &captured);
    ScrubProcessResult {
        outcome,
        error_kind,
        output: captured,
        exit_code,
        counts,
    }
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
    // `-n` = dry run (report only, change nothing); `-y` = assume yes
    // (auto-repair). `-f` forces a full check even if the superblock
    // looks clean — without it bcachefs may skip a clean-marked fs.
    let mode = if repair { "-y" } else { "-n" };
    let mut args: Vec<&str> = vec!["fsck", mode, "-f"];
    args.extend(devices.iter().map(|d| d.as_str()));

    let mut child = match nasty_common::priority::bulk_command("bcachefs")
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

async fn is_mountpoint(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    if tokio::fs::read_to_string("/proc/self/mountinfo")
        .await
        .is_ok_and(|contents| mountinfo_has_mountpoint(&contents, path))
    {
        return true;
    }
    // A path is a mount point when its device ID differs from its parent's,
    // or when it is the filesystem root. The mountinfo check above also
    // catches bind mounts that share their parent's device ID.
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

fn mountinfo_has_mountpoint(contents: &str, path: &str) -> bool {
    contents.lines().any(|line| {
        line.split_whitespace()
            .nth(4)
            .is_some_and(|mount_point| mount_point == path)
    })
}

async fn mounted_fs_uuid_at(mount_point: &str) -> Result<Option<String>, FilesystemError> {
    let mounts = read_bcachefs_mounts().await?;
    let Some(devices) = mounts.get(mount_point) else {
        return Ok(None);
    };
    if let Some(uuid) = by_uuid_source(devices) {
        return Ok(Some(uuid.to_string()));
    }
    let Some(first_device) = devices.first() else {
        return Ok(None);
    };
    Ok(get_fs_uuid(first_device)
        .await
        .filter(|uuid| !uuid.is_empty()))
}

/// A successful mount can become visible before blkid/lsblk can resolve its UUID.
/// Retry only the unavailable case; a reported mismatch must fail immediately.
async fn mounted_fs_uuid_after_mount(mount_point: &str) -> Option<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let uuid = mounted_fs_uuid_at(mount_point).await.ok().flatten();
        if uuid.is_some() || tokio::time::Instant::now() >= deadline {
            return uuid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn verify_mountpoint_identity(
    mount_point: &str,
    expected_uuid: &str,
) -> Result<(), FilesystemError> {
    let actual_uuid = mounted_fs_uuid_at(mount_point).await?;
    if actual_uuid.as_deref() != Some(expected_uuid) {
        return Err(FilesystemError::CommandFailed(format!(
            "mount point {mount_point} does not contain expected filesystem UUID {expected_uuid}; refusing the operation"
        )));
    }
    Ok(())
}

fn validate_expected_scrub_uuid(
    name: &str,
    expected_uuid: &str,
    actual_uuid: &str,
) -> Result<(), FilesystemError> {
    if actual_uuid != expected_uuid {
        return Err(FilesystemError::CommandFailed(format!(
            "filesystem '{name}' now resolves to UUID {actual_uuid}, not scheduled UUID {expected_uuid}; refusing scrub"
        )));
    }
    Ok(())
}

async fn verify_device_paths_uuid(
    paths: Vec<String>,
    expected_uuid: &str,
) -> Result<(), FilesystemError> {
    for path in paths {
        let actual_uuid = get_fs_uuid(&path).await;
        if actual_uuid.as_deref() != Some(expected_uuid) {
            return Err(FilesystemError::CommandFailed(format!(
                "device {path} no longer belongs to filesystem UUID {expected_uuid}; refusing the operation"
            )));
        }
    }
    Ok(())
}

async fn verify_filesystem_device_identity(fs: &Filesystem) -> Result<(), FilesystemError> {
    verify_device_paths_uuid(
        fs.devices
            .iter()
            .map(|device| device.path.clone())
            .collect(),
        &fs.uuid,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sysfs_members_match_device_aliases_without_guessing_missing_members() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let kernel_path = dir.path().join("sda1");
        std::fs::write(&kernel_path, []).unwrap();
        let kernel_path = std::fs::canonicalize(kernel_path).unwrap();
        let by_id = dir.path().join("by-id");
        std::fs::create_dir(&by_id).unwrap();
        let alias = by_id.join("wwn-disk-a-part1");
        symlink("../sda1", &alias).unwrap();
        let dangling = by_id.join("wwn-disk-b-part1");
        symlink("../sdb1", &dangling).unwrap();
        let unrelated = dir.path().join("sdc1");
        std::fs::write(&unrelated, []).unwrap();

        let member = DeviceSysfs {
            path: Some(kernel_path.to_str().unwrap().to_owned()),
            member_index: Some(0),
            state: Some("rw".into()),
            label: Some("hdd.disk-a".into()),
            ..Default::default()
        };
        let members = HashMap::from([(member.path.as_deref().unwrap(), &member)]);
        for path in [&kernel_path, &alias] {
            let found = find_sysfs_member(path.to_str().unwrap(), &members)
                .await
                .unwrap();
            assert!(std::ptr::eq(found, &member));
            assert_eq!(found.member_index, Some(0));
            assert_eq!(found.state.as_deref(), Some("rw"));
        }
        for path in [&dangling, &unrelated] {
            assert!(
                find_sysfs_member(path.to_str().unwrap(), &members)
                    .await
                    .is_none()
            );
        }

        // Detaching the device makes the previously valid alias dangling.
        std::fs::remove_file(&kernel_path).unwrap();
        assert!(
            find_sysfs_member(alias.to_str().unwrap(), &members)
                .await
                .is_none()
        );
        assert!(
            find_sysfs_member(alias.to_str().unwrap(), &HashMap::new())
                .await
                .is_none()
        );
    }

    fn smart_identity(
        rotational: Option<bool>,
        interface: Option<&str>,
    ) -> nasty_common::metrics_types::DiskHealth {
        serde_json::from_value(serde_json::json!({
            "device": "/dev/sdb", "model": "test", "serial": "serial", "firmware": "test",
            "capacity_bytes": 1000, "temperature_c": null, "power_on_hours": null,
            "health_passed": true, "smart_status": "PASSED", "rotational": rotational,
            "native_interface": interface, "attributes": []
        }))
        .unwrap()
    }

    #[test]
    fn media_is_independent_of_sas_connection_and_native_interface() {
        let sas_ssd = smart_identity(Some(false), Some("sas"));
        let detected = classify_disk("sdb", Some(true), Some(&sas_ssd), None);
        assert_eq!(detected.media.as_deref(), Some("ssd"));
        assert_eq!(detected.native_interface.as_deref(), Some("sas"));
        assert_eq!(detected.device_class, "ssd");
        assert_eq!(detected.media_source, "smart");

        let sata_hdd = smart_identity(Some(true), Some("sata"));
        let detected = classify_disk("sdc", Some(false), Some(&sata_hdd), None);
        assert_eq!(detected.media.as_deref(), Some("hdd"));
        assert_eq!(detected.native_interface.as_deref(), Some("sata"));
        assert!(detected.rotational);

        let sas_hdd = smart_identity(Some(true), Some("sas"));
        assert_eq!(
            classify_disk("sdd", Some(true), Some(&sas_hdd), None)
                .media
                .as_deref(),
            Some("hdd")
        );
        let nvme = classify_disk("nvme0n1", Some(false), None, None);
        assert_eq!(nvme.media.as_deref(), Some("ssd"));
        assert_eq!(nvme.native_interface.as_deref(), Some("nvme"));
        assert_eq!(nvme.device_class, "nvme");
    }

    #[test]
    fn missing_identity_stays_unknown_and_media_override_keeps_interface() {
        let unknown = classify_disk("sdb", None, None, None);
        assert_eq!(unknown.media, None);
        assert_eq!(unknown.native_interface, None);
        assert_eq!(unknown.device_class, "unknown");
        let fallback = classify_disk("sdb", Some(true), None, None);
        assert_eq!(fallback.media.as_deref(), Some("hdd"));
        assert_eq!(fallback.media_source, "sysfs");
        assert_eq!(fallback.native_interface, None);

        let sas_ssd = smart_identity(Some(false), Some("sas"));
        let overridden = classify_disk("sdb", Some(false), Some(&sas_ssd), Some("hdd"));
        assert_eq!(overridden.media.as_deref(), Some("hdd"));
        assert_eq!(overridden.media_source, "manual");
        assert_eq!(overridden.native_interface.as_deref(), Some("sas"));
    }

    #[test]
    fn cached_smart_identity_is_not_reused_after_a_disk_changes() {
        let smart = smart_identity(Some(false), Some("sas"));
        assert!(smart_matches_disk(&smart, Some("serial"), 1000));
        assert!(!smart_matches_disk(&smart, Some("replacement"), 1000));
        assert!(!smart_matches_disk(&smart, Some("serial"), 2000));
    }

    fn unique_tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nasty-filesystem-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn scheduled_scrub_uuid_must_still_match_name() {
        assert!(validate_expected_scrub_uuid("tank", "uuid-a", "uuid-a").is_ok());
        let error = validate_expected_scrub_uuid("tank", "uuid-a", "uuid-b").unwrap_err();
        assert!(error.to_string().contains("not scheduled UUID uuid-a"));
    }

    fn create_request(paths: &[&str]) -> CreateFilesystemRequest {
        CreateFilesystemRequest {
            name: "tank".into(),
            devices: paths
                .iter()
                .map(|path| DeviceSpec {
                    path: (*path).into(),
                    label: None,
                    durability: None,
                })
                .collect(),
            prepare_disks: vec![],
            replicas: 1,
            compression: None,
            encryption: None,
            passphrase: None,
            store_key: Some(true),
            bind_to_tpm: None,
            label: None,
            foreground_target: None,
            metadata_target: None,
            background_target: None,
            promote_target: None,
            erasure_code: None,
            data_checksum: None,
            metadata_checksum: None,
            bucket_size: None,
            encoded_extent_max: None,
            version_upgrade: None,
            journal_flush_delay: None,
        }
    }

    #[tokio::test]
    async fn create_mount_point_reclaims_only_an_empty_plain_directory() {
        let base = unique_tmp("mount-reservation");
        std::fs::create_dir_all(&base).unwrap();
        let mount_point = base.join("first");
        std::fs::create_dir(&mount_point).unwrap();

        reserve_create_mount_point_at(mount_point.to_str().unwrap(), "first")
            .await
            .unwrap();
        assert!(mount_point.is_dir());

        let marker = mount_point.join("operator-data");
        std::fs::write(&marker, b"keep").unwrap();
        let error = verify_create_mount_point_reserved(mount_point.to_str().unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is no longer empty"));
        let error = reserve_create_mount_point_at(mount_point.to_str().unwrap(), "first")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is not empty"));
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn create_mount_point_refuses_a_symlink() {
        let base = unique_tmp("mount-symlink");
        let target = base.join("target");
        let mount_point = base.join("first");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &mount_point).unwrap();

        let error = reserve_create_mount_point_at(mount_point.to_str().unwrap(), "first")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is not a plain directory"));
        assert!(
            mount_point
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.is_dir());

        std::fs::remove_dir_all(&base).unwrap();
    }

    fn filesystem_fixture(name: &str, uuid: &str) -> Filesystem {
        Filesystem {
            name: name.to_string(),
            uuid: uuid.to_string(),
            devices: Vec::new(),
            mount_point: None,
            mounted: false,
            total_bytes: 0,
            used_bytes: 0,
            available_bytes: 0,
            options: FilesystemOptions::default(),
            last_mount_error: None,
        }
    }

    #[test]
    fn unknown_filesystem_uses_uuid_prefix_instead_of_managed_mount_directory() {
        let state = HashMap::from([(
            "first".to_string(),
            FsMountOptions {
                uuid: Some("aaaaaaaa-main".to_string()),
                ..Default::default()
            },
        )]);

        assert_eq!(filesystem_name_for_uuid(&state, "aaaaaaaa-main"), "first");
        assert_eq!(filesystem_name_for_uuid(&state, "bbbbbbbb-usb"), "bbbbbbbb");
        assert_eq!(
            mounted_filesystem_name(&state, "first", "bbbbbbbb-usb"),
            "bbbbbbbb"
        );
        assert_eq!(
            mounted_filesystem_name(&state, "first", "aaaaaaaa-main"),
            "first"
        );
    }

    #[test]
    fn mount_selection_uses_persisted_uuid_even_when_names_collide() {
        let filesystems = vec![
            filesystem_fixture("first", "bbbbbbbb-usb"),
            filesystem_fixture("first", "aaaaaaaa-main"),
        ];

        let selected =
            select_filesystem_for_mount(filesystems.clone(), "first", Some("aaaaaaaa-main"))
                .unwrap();
        assert_eq!(selected.uuid, "aaaaaaaa-main");
        assert!(select_filesystem_by_name(filesystems, "first").is_err());

        let only_usb = vec![filesystem_fixture("first", "bbbbbbbb-usb")];
        let error =
            select_filesystem_for_mount(only_usb, "first", Some("aaaaaaaa-main")).unwrap_err();
        assert!(error.to_string().contains("refusing to mount"));
    }

    #[test]
    fn mounted_identity_requires_the_persisted_uuid() {
        assert!(mounted_identity_matches(
            Some("aaaaaaaa-main"),
            Some("aaaaaaaa-main")
        ));
        assert!(!mounted_identity_matches(
            Some("aaaaaaaa-main"),
            Some("bbbbbbbb-usb")
        ));
        assert!(!mounted_identity_matches(Some("aaaaaaaa-main"), None));
        assert!(!mounted_identity_matches(None, Some("bbbbbbbb-usb")));
    }

    #[test]
    fn unavailable_projection_is_uuid_based_actionable_and_sorted() {
        let state = HashMap::from([
            (
                "zeta".to_string(),
                FsMountOptions {
                    uuid: Some("uuid-z".to_string()),
                    devices: vec!["/dev/z-last".to_string()],
                    mounted: Some(false),
                    ..Default::default()
                },
            ),
            (
                "alpha".to_string(),
                FsMountOptions {
                    uuid: Some("uuid-a".to_string()),
                    devices: vec!["/dev/a-last".to_string()],
                    ..Default::default()
                },
            ),
            (
                "live-under-another-name".to_string(),
                FsMountOptions {
                    uuid: Some("uuid-live".to_string()),
                    ..Default::default()
                },
            ),
            ("legacy".to_string(), FsMountOptions::default()),
        ]);
        let failure = unavailable_filesystem_failure("alpha", &["/dev/a-last".to_string()]);
        let failures = HashMap::from([("alpha".to_string(), failure.clone())]);
        let live = vec![filesystem_fixture("renamed", "uuid-live")];

        let unavailable = project_unavailable_filesystems(&state, &live, &failures);

        assert_eq!(unavailable.len(), 2);
        assert_eq!(unavailable[0].name, "alpha");
        assert_eq!(unavailable[0].uuid, "uuid-a");
        assert_eq!(unavailable[0].devices, vec!["/dev/a-last"]);
        assert!(unavailable[0].auto_mount);
        assert_eq!(unavailable[0].last_mount_error, Some(failure));
        assert_eq!(unavailable[1].name, "zeta");
        assert!(!unavailable[1].auto_mount);
    }

    #[test]
    fn forget_validation_requires_confirmation_uuid_and_actionable_state() {
        let state = HashMap::from([
            (
                "tank".to_string(),
                FsMountOptions {
                    uuid: Some("uuid-tank".to_string()),
                    ..Default::default()
                },
            ),
            ("legacy".to_string(), FsMountOptions::default()),
        ]);
        let mut request = ForgetUnavailableRequest {
            name: "tank".to_string(),
            expected_uuid: "uuid-tank".to_string(),
            confirm_name: "tank".to_string(),
        };
        assert!(validate_forget_unavailable(&state, &request).is_ok());

        request.confirm_name = "other".to_string();
        assert!(matches!(
            validate_forget_unavailable(&state, &request),
            Err(FilesystemError::InvalidInput(_))
        ));
        request.confirm_name = "tank".to_string();
        request.expected_uuid = "different".to_string();
        assert!(matches!(
            validate_forget_unavailable(&state, &request),
            Err(FilesystemError::InvalidInput(_))
        ));
        request.name = "legacy".to_string();
        request.confirm_name = "legacy".to_string();
        assert!(matches!(
            validate_forget_unavailable(&state, &request),
            Err(FilesystemError::InvalidInput(_))
        ));
    }

    #[test]
    fn unavailable_selection_classifies_only_total_absence_as_missing() {
        assert_eq!(
            classify_unavailable_selection(false, false, false),
            MountFailureReason::MissingDevice
        );
        assert_eq!(
            classify_unavailable_selection(true, false, false),
            MountFailureReason::IdentityMismatch
        );
        assert_eq!(
            classify_unavailable_selection(false, true, false),
            MountFailureReason::IdentityMismatch
        );
        assert_eq!(
            classify_unavailable_selection(false, false, true),
            MountFailureReason::IdentityMismatch
        );

        let paths = vec!["/dev/old-a".to_string(), "/dev/old-b".to_string()];
        let failure = unavailable_filesystem_failure("tank", &paths);
        assert_eq!(failure.reason, MountFailureReason::MissingDevice);
        assert_eq!(
            failure
                .missing_devices
                .iter()
                .map(|device| device.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/dev/old-a", "/dev/old-b"]
        );
    }

    #[test]
    fn blkid_uuid_probe_distinguishes_absence_from_failure() {
        assert!(classify_blkid_uuid_probe(Some(0), "/dev/sda\n", "").unwrap());
        assert!(!classify_blkid_uuid_probe(Some(2), "", "").unwrap());
        assert!(classify_blkid_uuid_probe(Some(2), "", "probe error").is_err());
        assert!(classify_blkid_uuid_probe(Some(0), "", "").is_err());
        assert!(classify_blkid_uuid_probe(Some(1), "", "permission denied").is_err());
        assert!(classify_blkid_uuid_probe(None, "", "terminated").is_err());
    }

    #[test]
    fn lsblk_uuid_probe_walks_nested_devices() {
        let output = r#"{
            "blockdevices": [
                {"uuid": null, "children": [{"uuid": "root"}]},
                {"uuid": "pool", "children": []}
            ]
        }"#;
        assert!(lsblk_output_has_uuid(output, "root").unwrap());
        assert!(lsblk_output_has_uuid(output, "pool").unwrap());
        assert!(!lsblk_output_has_uuid(output, "missing").unwrap());
        assert!(lsblk_output_has_uuid("{}", "pool").is_err());
    }

    #[test]
    fn mountinfo_detects_bind_mounts_at_the_exact_target() {
        let mountinfo = "36 25 0:32 / /fs/first rw,relatime - bcachefs /dev/sdb rw\n\
                         37 25 0:32 /subvol /fs/second rw,relatime - bcachefs /dev/sdb rw\n";
        assert!(mountinfo_has_mountpoint(mountinfo, "/fs/first"));
        assert!(mountinfo_has_mountpoint(mountinfo, "/fs/second"));
        assert!(!mountinfo_has_mountpoint(mountinfo, "/fs/third"));
    }

    #[test]
    fn create_format_uses_filesystem_label_without_implicit_device_labels() {
        let req = create_request(&["/dev/sdb", "/dev/sdc"]);
        let args = build_create_format_args(&req, &req.devices);

        assert!(args.contains(&"--fs_label=tank".to_string()));
        assert!(!args.contains(&"--label=tank".to_string()));
    }

    #[test]
    fn create_format_keeps_device_labels_for_tiering() {
        let mut req = create_request(&["/dev/sdb", "/dev/sdc"]);
        req.foreground_target = Some("fast".to_string());
        req.devices[0].label = Some("fast".to_string());
        let args = build_create_format_args(&req, &req.devices);

        assert!(args.contains(&"--fs_label=tank".to_string()));
        assert!(args.contains(&"--label=fast".to_string()));
        assert!(args.contains(&"--label=tank".to_string()));
    }

    fn create_inventory_fixture() -> BlockInventory {
        parse_lsblk_inventory(
            r#"{
                "blockdevices": [
                    {
                        "name": "/dev/sda", "kname": "/dev/sda", "path": "/dev/sda",
                        "maj:min": "8:0", "size": 107374182400, "type": "disk",
                        "pkname": null, "fstype": null, "pttype": "gpt",
                        "ptuuid": "disk-a", "partuuid": null, "partn": null,
                        "start": null, "ro": false, "mountpoints": [null],
                        "log-sec": 512, "disk-seq": 10,
                        "children": [
                            {
                                "name": "/dev/sda1", "kname": "/dev/sda1", "path": "/dev/sda1",
                                "maj:min": "8:1", "size": 1073741824, "type": "part",
                                "pkname": "/dev/sda", "fstype": "vfat", "pttype": null,
                                "ptuuid": null, "partuuid": "boot", "partn": 1,
                                "start": 2048, "ro": false, "mountpoints": ["/boot"],
                                "log-sec": 512, "disk-seq": 10
                            },
                            {
                                "name": "/dev/sda2", "kname": "/dev/sda2", "path": "/dev/sda2",
                                "maj:min": "8:2", "size": 96636764160, "type": "part",
                                "pkname": "/dev/sda", "fstype": "bcachefs", "pttype": null,
                                "ptuuid": null, "partuuid": "root", "partn": 2,
                                "start": 1050624, "ro": false, "mountpoints": ["/"],
                                "log-sec": 512, "disk-seq": 10
                            },
                            {
                                "name": "/dev/sda3", "kname": "/dev/sda3", "path": "/dev/sda3",
                                "maj:min": "8:3", "size": 1048576, "type": "part",
                                "pkname": "/dev/sda", "fstype": null, "pttype": null,
                                "ptuuid": null, "partuuid": "data", "partn": 3,
                                "start": 4096, "ro": false, "mountpoints": [null],
                                "log-sec": 512, "disk-seq": 10
                            }
                        ]
                    },
                    {
                        "name": "/dev/sdb", "kname": "/dev/sdb", "path": "/dev/sdb",
                        "maj:min": "8:16", "size": 10737418240, "type": "disk",
                        "pkname": null, "fstype": null, "pttype": null,
                        "ptuuid": null, "partuuid": null, "partn": null,
                        "start": null, "ro": false, "mountpoints": [null],
                        "log-sec": 512, "disk-seq": 11
                    }
                ]
            }"#,
        )
        .expect("fixture parses")
    }

    // ── Filesystem creation preflight (DATA-2) ─────────────────────

    #[test]
    fn create_preflight_accepts_unused_sibling_of_boot_partition() {
        let inventory = create_inventory_fixture();
        let data = inventory.get_path("/dev/sda3").unwrap();
        let result = validate_existing_create_target(&inventory, data, &HashSet::new());
        assert!(
            result.is_ok(),
            "the installer's reserved data partition must remain eligible: {result:?}"
        );
    }

    #[test]
    fn create_preflight_rejects_whole_boot_disk_but_allows_its_free_extent() {
        let inventory = create_inventory_fixture();
        let disk = inventory.get_path("/dev/sda").unwrap();
        let err = validate_existing_create_target(&inventory, disk, &HashSet::new()).unwrap_err();
        assert!(err.to_string().contains("mounted"), "{err}");

        assert!(
            validate_free_space_parent(&inventory, disk, &HashSet::new()).is_ok(),
            "an exact unallocated extent may coexist with mounted sibling partitions"
        );
    }

    #[test]
    fn whole_disk_preparation_checks_descendants_and_snapshot_identity() {
        let mut inventory = create_inventory_fixture();
        for node in inventory.devices.values_mut() {
            node.mount_points.clear();
        }
        let disk = inventory.get_path("/dev/sda").unwrap();
        // Normal create must not erase a partitioned disk implicitly.
        assert!(validate_existing_create_target(&inventory, disk, &HashSet::new()).is_err());
        validate_preparation_usage(&inventory, disk, &HashSet::new(), &HashSet::new()).unwrap();
        let snapshot = disk_preparation_snapshot(&inventory, disk).unwrap();
        assert_eq!(snapshot.children.len(), 3);
        assert!(preparation_matches(&inventory, disk, &snapshot).unwrap());
        assert!(
            disk_preparation_snapshot(&inventory, inventory.get_path("/dev/sda3").unwrap())
                .is_err()
        );

        let child = inventory
            .get_path("/dev/sda3")
            .unwrap()
            .identity
            .devno
            .clone();
        inventory.devices.get_mut(&child).unwrap().fs_type = Some("ext4".into());
        assert!(
            !preparation_matches(
                &inventory,
                inventory.get_path("/dev/sda").unwrap(),
                &snapshot
            )
            .unwrap()
        );

        let disk = inventory.get_path("/dev/sda").unwrap();
        assert!(
            validate_preparation_usage(
                &inventory,
                disk,
                &HashSet::from([child.clone()]),
                &HashSet::new()
            )
            .unwrap_err()
            .to_string()
            .contains("active swap")
        );
        assert!(
            validate_preparation_usage(
                &inventory,
                disk,
                &HashSet::new(),
                &HashSet::from(["/dev/sda3".into()])
            )
            .unwrap_err()
            .to_string()
            .contains("registered filesystem")
        );
    }

    #[test]
    fn preparation_failure_reports_completed_partial_and_unprocessed_disks() {
        let error = report_preparation_failure(
            FilesystemError::CommandFailed("partprobe failed".into()),
            &["/dev/sdb".into(), "/dev/sdc".into(), "/dev/sdd".into()],
            &["/dev/sdb".into(), "/dev/sdc".into()],
            &["/dev/sdb".into()],
        )
        .to_string();
        assert!(error.contains("prepared disks: [\"/dev/sdb\"]"));
        assert!(error.contains("partial on: [\"/dev/sdc\"]"));
        assert!(error.contains("not processed: [\"/dev/sdd\"]"));
    }

    #[test]
    fn create_preflight_rejects_swap_holders_signatures_and_read_only_devices() {
        let mut inventory = create_inventory_fixture();
        let data_devno = inventory
            .get_path("/dev/sda3")
            .unwrap()
            .identity
            .devno
            .clone();

        let swaps = HashSet::from([data_devno.clone()]);
        assert!(
            validate_existing_create_target(
                &inventory,
                inventory.devices.get(&data_devno).unwrap(),
                &swaps
            )
            .unwrap_err()
            .to_string()
            .contains("active swap")
        );

        inventory.devices.get_mut(&data_devno).unwrap().holders = vec!["md0".into()];
        assert!(
            validate_existing_create_target(
                &inventory,
                inventory.devices.get(&data_devno).unwrap(),
                &HashSet::new()
            )
            .unwrap_err()
            .to_string()
            .contains("md0")
        );

        let node = inventory.devices.get_mut(&data_devno).unwrap();
        node.holders.clear();
        node.fs_type = Some("LVM2_member".into());
        assert!(
            validate_existing_create_target(
                &inventory,
                inventory.devices.get(&data_devno).unwrap(),
                &HashSet::new()
            )
            .unwrap_err()
            .to_string()
            .contains("LVM2_member")
        );

        let node = inventory.devices.get_mut(&data_devno).unwrap();
        node.fs_type = None;
        node.read_only = true;
        assert!(
            validate_existing_create_target(
                &inventory,
                inventory.devices.get(&data_devno).unwrap(),
                &HashSet::new()
            )
            .unwrap_err()
            .to_string()
            .contains("read-only")
        );
    }

    #[test]
    fn create_request_validation_finishes_before_any_execution_plan() {
        let mut encrypted = create_request(&["/dev/sdb"]);
        encrypted.encryption = Some(true);
        assert!(
            validate_create_request(&encrypted)
                .unwrap_err()
                .to_string()
                .contains("passphrase")
        );

        let mut erasure = create_request(&["/dev/sdb:free", "/dev/sdc:free"]);
        erasure.erasure_code = Some(true);
        erasure.replicas = 2;
        assert!(
            validate_create_request(&erasure)
                .unwrap_err()
                .to_string()
                .contains("at least 3 devices")
        );
    }

    #[test]
    fn create_request_rejects_unmatched_tiering_target() {
        let mut req = create_request(&["/dev/sdb"]);
        req.devices[0].label = Some("slow".into());
        req.foreground_target = Some("fast".into());
        assert!(
            validate_create_request(&req)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );

        req.devices[0].label = Some("fast.nvme".into());
        assert!(validate_create_request(&req).is_ok());
    }

    #[test]
    fn created_partition_must_match_exact_parent_number_and_geometry() {
        let inventory = create_inventory_fixture();
        let parent = &inventory.get_path("/dev/sda").unwrap().identity;
        assert_eq!(parent.disk_sequence, Some(10));
        let identity = verify_created_partition(&inventory, parent, 3, 4096, 6143, "/dev/sda3")
            .expect("exact planned partition accepted");
        assert_eq!(identity.devno, "8:3");
        assert!(verify_created_partition(&inventory, parent, 2, 4096, 6143, "/dev/sda3").is_err());
        assert!(verify_created_partition(&inventory, parent, 3, 4097, 6144, "/dev/sda3").is_err());
    }

    #[test]
    fn device_identity_detects_preflight_to_format_changes() {
        let inventory = create_inventory_fixture();
        let expected = inventory.get_path("/dev/sdb").unwrap().identity.clone();
        assert!(identity_changed(&expected, inventory.get_path("/dev/sdb"), "/dev/sdb").is_ok());

        let mut changed = create_inventory_fixture();
        changed
            .devices
            .get_mut("8:16")
            .unwrap()
            .identity
            .disk_sequence = Some(99);
        assert!(
            identity_changed(&expected, changed.get_path("/dev/sdb"), "/dev/sdb")
                .unwrap_err()
                .to_string()
                .contains("changed after preflight")
        );
    }

    #[test]
    fn created_partition_normalizes_4kn_lbas_to_lsblk_start_units() {
        let mut inventory = create_inventory_fixture();
        inventory
            .devices
            .get_mut("8:0")
            .unwrap()
            .identity
            .logical_sector_bytes = 4096;
        let partition = &mut inventory.devices.get_mut("8:3").unwrap().identity;
        partition.logical_sector_bytes = 4096;
        partition.start_512_sector = Some(4096 * 8);
        partition.size_bytes = 2048 * 4096;

        let parent = &inventory.get_path("/dev/sda").unwrap().identity;
        assert!(verify_created_partition(&inventory, parent, 3, 4096, 6143, "/dev/sda3").is_ok());
    }

    #[test]
    fn free_partition_helpers_choose_exact_slot_and_device_path() {
        let table = "Number  Start (sector)    End (sector)  Size       Code  Name\n\
                         1            2048         1050623   512.0 MiB   EF00  boot\n\
                         3         2099200         4196351   1024.0 MiB  8300  data\n";
        assert_eq!(parse_sgdisk_partition_numbers(table), HashSet::from([1, 3]));
        assert_eq!(partition_device_path("/dev/sda", 2), "/dev/sda2");
        assert_eq!(partition_device_path("/dev/nvme0n1", 2), "/dev/nvme0n1p2");
    }

    #[test]
    fn loop_device_filter_matches_only_mount_descendants() {
        let output = "/dev/loop1 /fs/first/vol-a/vol.img\n\
                      /dev/loop2 /fs/second/vol-b/vol.img\n\
                      /dev/loop3 /fs/firstish/vol-c/vol.img\n\
                      /dev/loop4 /fs/first/nested/vol.img\n";
        assert_eq!(
            loop_devices_backed_by(output, "/fs/first"),
            vec!["/dev/loop1", "/dev/loop4"]
        );
        assert_eq!(
            loop_devices_backed_by(output, "/fs/first/"),
            vec!["/dev/loop1", "/dev/loop4"]
        );
    }

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

    #[test]
    fn unresolved_legacy_scheduler_survives_state_roundtrip() {
        let legacy = r#"{
            "devices": ["/dev/missing"],
            "io_scheduler": "bfq"
        }"#;
        let opts: FsMountOptions = serde_json::from_str(legacy).expect("legacy state parses");
        assert_eq!(opts.legacy_io_scheduler.as_deref(), Some("bfq"));

        let json = serde_json::to_string(&opts).expect("serialize");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["io_scheduler"],
            "bfq"
        );
    }

    /// Round-trip an unmount→mount cycle in pure data form: an FS
    /// with tuned options must keep them across a `mounted = false`
    /// pause and survive serde re-serialization. Regression for
    /// "unmount silently wiped my compression/journal_flush_delay config" —
    /// pre-fix `save_fs_unmounted` did a
    /// flat `state.remove(name)`.
    #[test]
    fn unmount_preserves_tuned_options_across_state_roundtrip() {
        let mounted = FsMountOptions {
            uuid: Some("uuid-1".into()),
            devices: vec!["/dev/sda".into(), "/dev/sdb".into()],
            mounted: Some(true),
            encrypted: Some(true),
            journal_flush_delay: Some(1000),
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

    // ── parse_moving_ctxts (#540) ──────────────────────────────────
    // Samples are verbatim from a real bcachefs 6.18.35 pool.

    #[test]
    fn moving_ctxts_parses_active_scrub() {
        // Trimmed from a mid-pass scrub captured on .38.
        let raw = "\
scrub: data type==(unknown data_type 254) pos=extents:5764607:9038848:U32_MAX
  keys moved:                  5886
  keys raced:                  0
  bytes seen:                  1.44G
  bytes moved:                 1.43G
  bytes raced:                 0
  reads: ios 64/64 sectors 32768/131072
";
        let ctxs = parse_moving_ctxts(raw);
        assert_eq!(ctxs.len(), 1);
        assert_eq!(ctxs[0].kind, "scrub");
        assert_eq!(ctxs[0].keys_moved, 5886);
        assert_eq!(ctxs[0].bytes_seen, (1.44 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(
            ctxs[0].bytes_moved,
            (1.43 * 1024.0 * 1024.0 * 1024.0) as u64
        );
    }

    #[test]
    fn moving_ctxts_parses_idle_reconcile_baseline() {
        let raw = "\
reconcile_work: data type==user pos=extents:POS_MIN
  keys moved:                  0
  bytes seen:                  0
  bytes moved:                 0
";
        let ctxs = parse_moving_ctxts(raw);
        assert_eq!(ctxs.len(), 1);
        assert_eq!(ctxs[0].kind, "reconcile");
        assert_eq!(ctxs[0].keys_moved, 0);
        assert_eq!(ctxs[0].bytes_seen, 0);
        assert_eq!(ctxs[0].bytes_moved, 0);
    }

    #[test]
    fn moving_ctxts_handles_multiple_and_unknown_kinds() {
        let raw = "\
scrub: ...
  bytes seen:                  512M
copygc: ...
  bytes moved:                 8.0M
weird_future_op: ...
  bytes moved:                 1.0M
";
        let ctxs = parse_moving_ctxts(raw);
        assert_eq!(ctxs.len(), 3);
        assert_eq!(ctxs[0].kind, "scrub");
        assert_eq!(ctxs[0].bytes_seen, 512 * 1024 * 1024);
        assert_eq!(ctxs[1].kind, "copygc");
        assert_eq!(ctxs[1].bytes_moved, 8 * 1024 * 1024);
        // Unrecognized header → "other", still parsed (forward-compat).
        assert_eq!(ctxs[2].kind, "other");
    }

    #[test]
    fn moving_ctxts_empty_when_idle() {
        assert!(parse_moving_ctxts("").is_empty());
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

    // ── by-uuid mount source resolution (bcachefs ≥ 1.38.8) ───────
    //
    // The 1.38.8 kernel module's show_devname reports multi-device
    // filesystems as `/dev/disk/by-uuid/<fs-uuid>` in /proc/mounts
    // instead of the colon-joined member list. Splitting that on ':'
    // yields a path that matches no member device, so the engine
    // believed the filesystem wasn't mounted at all (the ".41 says
    // filesystem is missing" report). Members must come from sysfs.

    #[test]
    fn by_uuid_source_detected_only_for_single_by_uuid_entry() {
        assert_eq!(
            by_uuid_source(&["/dev/disk/by-uuid/cf4eabd3-14a5-4bbb-87a6-d6f217cdfb47".into()]),
            Some("cf4eabd3-14a5-4bbb-87a6-d6f217cdfb47")
        );
        assert_eq!(by_uuid_source(&["/dev/sda".into()]), None);
        assert_eq!(
            by_uuid_source(&["/dev/sdb".into(), "/dev/sdd".into()]),
            None
        );
        assert_eq!(by_uuid_source(&[]), None);
    }

    /// Build a fake `/sys/fs/bcachefs/<uuid>/dev-N/block` tree under a
    /// unique temp dir. Returns (base, uuid); caller removes base.
    fn sysfs_fixture(members: &[(&str, &str)]) -> (std::path::PathBuf, String) {
        let uuid = uuid::Uuid::new_v4().to_string();
        let base = std::env::temp_dir().join(format!("nasty-sysfs-{}", uuid::Uuid::new_v4()));
        let fs_dir = base.join(&uuid);
        for (devdir, target) in members {
            let d = fs_dir.join(devdir);
            std::fs::create_dir_all(&d).unwrap();
            std::os::unix::fs::symlink(target, d.join("block")).unwrap();
        }
        (base, uuid)
    }

    #[test]
    fn sysfs_members_resolve_block_symlink_basenames_in_index_order() {
        let (base, uuid) = sysfs_fixture(&[
            ("dev-1", "../../../devices/pci0000:00/host1/block/sdd"),
            ("dev-0", "../../../devices/pci0000:00/host0/block/sdb"),
            ("dev-10", "../../../devices/pci0000:00/host2/block/sde"),
        ]);
        let members = sysfs_fs_members(&base, &uuid).expect("members resolve");
        assert_eq!(
            members,
            vec![
                "/dev/sdb".to_string(),
                "/dev/sdd".to_string(),
                "/dev/sde".to_string()
            ],
            "sorted by member index, numerically (dev-10 after dev-1)"
        );
        assert!(
            sysfs_fs_members(&base, "00000000-0000-0000-0000-000000000000").is_none(),
            "unknown uuid (unmounted fs) yields None"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_mount_devices_swaps_by_uuid_source_for_sysfs_members() {
        let (base, uuid) = sysfs_fixture(&[("dev-0", "../sdb"), ("dev-1", "../sdd")]);

        let resolved = resolve_mount_devices(vec![format!("/dev/disk/by-uuid/{uuid}")], &base);
        assert_eq!(
            resolved,
            vec!["/dev/sdb".to_string(), "/dev/sdd".to_string()]
        );

        // Old-format colon lists pass through untouched.
        let raw = vec!["/dev/sdb".to_string(), "/dev/sdd".to_string()];
        assert_eq!(resolve_mount_devices(raw.clone(), &base), raw);

        // A by-uuid source whose sysfs entry is gone (fs racing an
        // unmount) keeps the raw source rather than reporting nothing.
        let ghost = vec!["/dev/disk/by-uuid/dead-beef".to_string()];
        assert_eq!(resolve_mount_devices(ghost.clone(), &base), ghost);

        std::fs::remove_dir_all(&base).ok();
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
    fn local_operation_reservation_releases_on_drop() {
        let operations = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        {
            let _reservation =
                LocalOperationReservation::acquire(&operations, "tank".to_string()).unwrap();
            assert!(operation_is_owned_here(&operations, "tank"));
            assert!(LocalOperationReservation::acquire(&operations, "tank".to_string()).is_none());
        }
        assert!(!operation_is_owned_here(&operations, "tank"));
    }

    #[test]
    fn scrub_completion_window_is_not_misclassified_as_restart() {
        let started_at = Some(1_700_000_000);

        assert!(!should_record_interrupted_operation(
            started_at, true, started_at, true, false,
        ));
        assert!(!should_record_interrupted_operation(
            started_at, false, None, false, false,
        ));
        assert!(!should_record_interrupted_operation(
            started_at,
            true,
            Some(1_700_000_001),
            false,
            false,
        ));
    }

    #[test]
    fn stale_operation_from_previous_engine_is_marked_interrupted() {
        let started_at = Some(1_700_000_000);
        assert!(should_record_interrupted_operation(
            started_at, true, started_at, false, false,
        ));
        assert!(!should_record_interrupted_operation(
            started_at, true, started_at, false, true,
        ));
    }

    #[test]
    fn scrub_process_pattern_is_exact_and_escapes_mount_metacharacters() {
        assert_eq!(
            scrub_process_pattern("/fs/pool.1+[copy]"),
            r"(^|.*/)bcachefs scrub /fs/pool\.1\+\[copy\]$"
        );
    }

    #[test]
    fn orphaned_scrub_cancel_marker_must_match_the_run() {
        let status: ScrubStatus = serde_json::from_str(
            r#"{
                "running": true,
                "started_at": 1700000000,
                "run_id": "current-run",
                "raw": "Scrub in progress..."
            }"#,
        )
        .unwrap();
        let mut controls = ScrubControlState {
            cancellations: HashMap::from([("tank".to_string(), "older-run".to_string())]),
            local_runs: HashMap::new(),
        };

        assert!(!scrub_cancel_requested(&controls, "tank", &status));
        controls
            .cancellations
            .insert("tank".to_string(), "current-run".to_string());
        assert!(scrub_cancel_requested(&controls, "tank", &status));
    }

    #[test]
    fn scrub_cancel_rejects_a_replacement_run() {
        assert!(scrub_cancel_targets_run(None, "current-run"));
        assert!(scrub_cancel_targets_run(Some("current-run"), "current-run"));
        assert!(!scrub_cancel_targets_run(Some("older-run"), "current-run"));
    }

    #[tokio::test]
    async fn scrub_cancel_before_spawn_prevents_child_launch() {
        let store = Arc::new(Mutex::new(HashMap::new()));
        let controls = Arc::new(Mutex::new(ScrubControlState {
            cancellations: HashMap::from([("tank".to_string(), "run-1".to_string())]),
            local_runs: HashMap::from([(
                "tank".to_string(),
                LocalScrubRun {
                    run_id: "run-1".to_string(),
                    spawn_attempted: false,
                },
            )]),
        }));

        let result = stream_scrub_and_collect("/fs/tank", "tank", &store, &controls, "run-1").await;

        assert_eq!(result.exit_code, None);
        assert_eq!(
            result.output,
            "scrub cancelled before the bcachefs process started"
        );
        assert!(
            controls
                .lock()
                .await
                .local_runs
                .get("tank")
                .unwrap()
                .spawn_attempted
        );
        assert_eq!(
            scrub_outcome_after_cancel(result.outcome, result.error_kind, result.exit_code, true,),
            ScrubOutcome::Cancelled
        );
    }

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
    fn scrub_exit_bitmask_distinguishes_completed_errors_from_failure() {
        assert_eq!(
            classify_scrub_result(Some(0), None, ""),
            (ScrubOutcome::Ok, None)
        );
        assert_eq!(
            classify_scrub_result(Some(2), None, ""),
            (ScrubOutcome::Errors, Some(ScrubErrorKind::Corrected))
        );
        assert_eq!(
            classify_scrub_result(Some(4), None, ""),
            (ScrubOutcome::Errors, Some(ScrubErrorKind::Uncorrected))
        );
        assert_eq!(
            classify_scrub_result(Some(6), None, ""),
            (ScrubOutcome::Errors, Some(ScrubErrorKind::Uncorrected))
        );
        assert_eq!(
            classify_scrub_result(Some(1), None, ""),
            (ScrubOutcome::Failed, None)
        );
        assert_eq!(
            classify_scrub_result(Some(3), None, ""),
            (ScrubOutcome::Failed, Some(ScrubErrorKind::Corrected))
        );
        assert_eq!(
            classify_scrub_result(Some(5), None, ""),
            (ScrubOutcome::Failed, Some(ScrubErrorKind::Uncorrected))
        );
        assert_eq!(
            classify_scrub_result(Some(7), None, ""),
            (ScrubOutcome::Failed, Some(ScrubErrorKind::Uncorrected))
        );
        assert_eq!(
            classify_scrub_result(Some(8), None, ""),
            (ScrubOutcome::Failed, None)
        );
        assert_eq!(
            classify_scrub_result(None, None, ""),
            (ScrubOutcome::Failed, None)
        );
    }

    #[test]
    fn scrub_error_bytes_are_summed_from_device_table() {
        let out = "\
Starting scrub on 3 devices: sda sdb sdc
device                checked    corrected  uncorrected        total
sda                     72.4G           0B        14.1M        70.5G   102%  complete
sdb                     70.5G         4.0K           0B        70.5G   100%  complete
sdc                     72.3G         1.5M        21.8M        70.5G   102%  complete
";
        assert_eq!(
            parse_scrub_error_bytes(out),
            Some(ScrubErrorBytes {
                corrected_bytes: parse_human_bytes("4.0K").unwrap()
                    + parse_human_bytes("1.5M").unwrap(),
                uncorrected_bytes: parse_human_bytes("14.1M").unwrap()
                    + parse_human_bytes("21.8M").unwrap(),
                device_offline: false,
            })
        );
    }

    #[test]
    fn scrub_offline_device_counts_errors_but_marks_run_failed() {
        let out = "\
device                checked    corrected  uncorrected        total
sda                     70.5G         4.0K        14.1M        70.5G   100%  offline
";
        let counts = parse_scrub_error_bytes(out).unwrap();
        assert!(counts.device_offline);
        assert_eq!(counts.corrected_bytes, 4096);
        assert_eq!(
            counts.uncorrected_bytes,
            parse_human_bytes("14.1M").unwrap()
        );
        assert_eq!(
            classify_scrub_result(Some(4), Some(counts), out),
            (ScrubOutcome::Failed, Some(ScrubErrorKind::Uncorrected))
        );
    }

    #[test]
    fn scrub_counts_backstop_zero_exit_from_older_tools() {
        let corrected = ScrubErrorBytes {
            corrected_bytes: 4096,
            uncorrected_bytes: 0,
            device_offline: false,
        };
        let uncorrected = ScrubErrorBytes {
            corrected_bytes: 0,
            uncorrected_bytes: 4096,
            device_offline: false,
        };
        assert_eq!(
            classify_scrub_result(Some(0), Some(corrected), ""),
            (ScrubOutcome::Errors, Some(ScrubErrorKind::Corrected))
        );
        assert_eq!(
            classify_scrub_result(Some(0), Some(uncorrected), ""),
            (ScrubOutcome::Errors, Some(ScrubErrorKind::Uncorrected))
        );
    }

    #[test]
    fn scrub_cancel_race_preserves_completed_or_reported_errors() {
        assert_eq!(
            scrub_outcome_after_cancel(ScrubOutcome::Failed, None, None, true),
            ScrubOutcome::Cancelled
        );
        assert_eq!(
            scrub_outcome_after_cancel(ScrubOutcome::Failed, None, Some(1), true),
            ScrubOutcome::Cancelled
        );
        assert_eq!(
            scrub_outcome_after_cancel(
                ScrubOutcome::Errors,
                Some(ScrubErrorKind::Uncorrected),
                Some(4),
                true,
            ),
            ScrubOutcome::Errors
        );
        assert_eq!(
            scrub_outcome_after_cancel(
                ScrubOutcome::Failed,
                Some(ScrubErrorKind::Uncorrected),
                Some(5),
                true,
            ),
            ScrubOutcome::Failed
        );
        assert_eq!(
            scrub_outcome_after_cancel(ScrubOutcome::Ok, None, Some(0), true),
            ScrubOutcome::Ok
        );
    }

    #[test]
    fn scrub_status_deserializes_legacy_state_without_run_details() {
        let status: ScrubStatus = serde_json::from_str(
            r#"{
                "running": false,
                "last_run_at": 1700000000,
                "last_outcome": "errors",
                "raw": "Last scrub: errors detected"
            }"#,
        )
        .unwrap();

        assert_eq!(status.last_outcome, Some(ScrubOutcome::Errors));
        assert!(status.run_id.is_none());
        assert!(status.last_exit_code.is_none());
        assert!(status.last_corrected_bytes.is_none());
        assert!(status.last_uncorrected_bytes.is_none());
        assert!(status.last_error_kind.is_none());
        assert!(status.bcachefs_tools_version.is_none());
        assert!(status.kernel_version.is_none());
        assert!(status.bcachefs_module_version.is_none());
        assert!(!status.cancel_requested);
    }

    #[test]
    fn scrub_cancel_intent_survives_state_roundtrip() {
        let status: ScrubStatus = serde_json::from_str(
            r#"{
                "running": true,
                "started_at": 1700000000,
                "run_id": "current-run",
                "cancel_requested": true,
                "raw": "Scrub in progress..."
            }"#,
        )
        .unwrap();

        let restored: ScrubStatus =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert!(restored.cancel_requested);
    }

    #[test]
    fn scrub_cancel_request_keeps_legacy_name_only_shape() {
        let legacy: ScrubCancelRequest = serde_json::from_str(r#"{"name":"tank"}"#).unwrap();
        assert_eq!(legacy.name, "tank");
        assert!(legacy.run_id.is_none());

        let scoped: ScrubCancelRequest =
            serde_json::from_str(r#"{"name":"tank","run_id":"run-1"}"#).unwrap();
        assert_eq!(scoped.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn scrub_status_persists_exit_counts_versions_and_run_id() {
        let status = ScrubStatus {
            running: false,
            started_at: None,
            progress_percent: None,
            last_run_at: Some(1_700_000_000),
            last_duration_secs: Some(3600),
            last_outcome: Some(ScrubOutcome::Errors),
            last_output: Some("device table".into()),
            run_id: Some("27d5ac0d-f877-48aa-89eb-83ebfbaee17f".into()),
            last_exit_code: Some(4),
            last_corrected_bytes: Some(4096),
            last_uncorrected_bytes: Some(8192),
            last_error_kind: Some(ScrubErrorKind::Uncorrected),
            bcachefs_tools_version: Some("1.39.5".into()),
            kernel_version: Some("6.18.47".into()),
            bcachefs_module_version: Some("1.39.5".into()),
            cancel_requested: false,
            raw: "Last scrub: completed with uncorrected errors".into(),
        };

        let json = serde_json::to_string(&status).unwrap();
        let restored: ScrubStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.run_id, status.run_id);
        assert_eq!(restored.last_exit_code, Some(4));
        assert_eq!(restored.last_corrected_bytes, Some(4096));
        assert_eq!(restored.last_uncorrected_bytes, Some(8192));
        assert_eq!(restored.last_error_kind, Some(ScrubErrorKind::Uncorrected));
        assert_eq!(restored.bcachefs_tools_version.as_deref(), Some("1.39.5"));
        assert_eq!(restored.kernel_version.as_deref(), Some("6.18.47"));
        assert_eq!(restored.bcachefs_module_version.as_deref(), Some("1.39.5"));
        assert!(json.contains("\"last_outcome\":\"errors\""));
        assert!(json.contains("\"last_error_kind\":\"uncorrected\""));
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
