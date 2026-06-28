use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::cmd;
use crate::filesystem::FilesystemService;

const BLOCK_FILE_NAME: &str = "vol.img";

fn subvol_path(mount_point: &str, name: &str) -> String {
    format!("{mount_point}/{name}")
}

fn snap_path(mount_point: &str, subvol: &str, snap: &str) -> String {
    format!("{mount_point}/{subvol}@{snap}")
}

/// POSIX xattr namespace prefix for all user properties.
const XATTR_NS: &str = "user.";

/// Reserved xattr keys for NASty-internal subvolume metadata.
const XATTR_NASTY_TYPE: &str = "user.nasty.type";
const XATTR_NASTY_VOLSIZE: &str = "user.nasty.volsize";
const XATTR_NASTY_COMPRESSION: &str = "user.nasty.compression";
const XATTR_NASTY_COMMENT: &str = "user.nasty.comment";
const XATTR_NASTY_OWNER: &str = "user.nasty.owner";
const XATTR_NASTY_DIRECT_IO: &str = "user.nasty.direct_io";

/// Logical key prefix that maps to the reserved nasty.* xattrs.
/// Excluded from the user-visible `properties` map.
const NASTY_KEY_PREFIX: &str = "nasty.";

#[derive(Debug, Error)]
pub enum SubvolumeError {
    #[error("filesystem not found: {0}")]
    FilesystemNotFound(String),
    #[error("filesystem not mounted: {0}")]
    FilesystemNotMounted(String),
    #[error("subvolume already exists: {0}")]
    AlreadyExists(String),
    #[error("subvolume not found: {0}")]
    NotFound(String),
    #[error("access denied")]
    AccessDenied,
    #[error("volsize is required for block subvolumes")]
    VolsizeRequired,
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("invalid volsize: {0}")]
    InvalidVolsize(String),
    #[error("could not delete child subvolume(s): {0}")]
    ChildrenStuck(String),
    #[error("could not detach loop device {device}: {reason}")]
    LoopDetachFailed { device: String, reason: String },
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Hard ceiling on block-subvolume size requests. A NAS volume has no
/// legitimate reason to need >256 TiB — bcachefs would happily accept
/// the truncate but the resulting sparse file confuses every downstream
/// (df, NFS clients, du, backup tools that try to estimate transfer
/// size). The cap rejects obvious typos (extra zero) and protects
/// against an Operator-role caller trying to ENOSPC the filesystem by
/// requesting `u64::MAX`.
const MAX_VOLSIZE_BYTES: u64 = 256 * 1024 * 1024 * 1024 * 1024; // 256 TiB

/// Validate a subvolume name. Names may contain `/` (intentional —
/// nested subvolumes like `projects/web` are a supported pattern), but
/// must not escape the filesystem mount via `..`, must not start with
/// `/` (we always prefix the mount point), and must not contain `@`
/// (bcachefs uses `@` as the snapshot separator — colliding here
/// corrupts snapshot lookups) or control characters (would smuggle
/// newlines into log lines and config files downstream).
fn validate_subvolume_name(name: &str) -> Result<(), SubvolumeError> {
    if name.is_empty() {
        return Err(SubvolumeError::InvalidName("name is empty".to_string()));
    }
    if name.len() > 200 {
        return Err(SubvolumeError::InvalidName(
            "name exceeds 200 chars".to_string(),
        ));
    }
    if name.starts_with('/') {
        return Err(SubvolumeError::InvalidName(
            "name must not start with '/'".to_string(),
        ));
    }
    if name.starts_with('.') {
        // Leading dot would create a hidden directory that the WebUI
        // doesn't surface and that's hard to clean up via shell.
        return Err(SubvolumeError::InvalidName(
            "name must not start with '.'".to_string(),
        ));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err(SubvolumeError::InvalidName(
                "name must not contain empty path components ('//')".to_string(),
            ));
        }
        if component == ".." || component == "." {
            return Err(SubvolumeError::InvalidName(
                "name must not contain '.' or '..' components".to_string(),
            ));
        }
        if component.contains('@') {
            return Err(SubvolumeError::InvalidName(
                "name must not contain '@' (reserved for snapshot syntax)".to_string(),
            ));
        }
        if component.chars().any(|c| c.is_control()) {
            return Err(SubvolumeError::InvalidName(
                "name must not contain control characters".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_volsize_bytes(bytes: u64) -> Result<(), SubvolumeError> {
    if bytes == 0 {
        return Err(SubvolumeError::InvalidVolsize(
            "volsize must be greater than zero".to_string(),
        ));
    }
    if bytes > MAX_VOLSIZE_BYTES {
        return Err(SubvolumeError::InvalidVolsize(format!(
            "volsize {bytes} exceeds maximum {MAX_VOLSIZE_BYTES} (256 TiB)"
        )));
    }
    Ok(())
}

/// `losetup -d` returns one of a small set of "device wasn't attached
/// in the first place" errors depending on kernel + util-linux version.
/// They all mean the same thing to us — there's nothing to detach, so
/// the cleanup step is already satisfied. Returning `true` lets the
/// caller treat the failure as success and proceed with the rest of
/// the delete instead of erroring out on an idempotent retry.
fn is_already_detached(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("no such device") || lower.contains("not in use")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubvolumeType {
    Filesystem,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Subvolume {
    /// Subvolume name (unique within the filesystem).
    pub name: String,
    /// Name of the filesystem that contains this subvolume.
    pub filesystem: String,
    /// Whether this is a filesystem or block-backed subvolume.
    pub subvolume_type: SubvolumeType,
    /// Absolute filesystem path to the subvolume directory.
    pub path: String,
    /// Disk usage in bytes. For filesystem subvolumes, comes from the
    /// per-project quota (set on every create, so tracking is always
    /// on); `None` only on legacy subvolumes created before the
    /// always-track change. For block subvolumes, comes from the
    /// backing image's allocated size.
    pub used_bytes: Option<u64>,
    /// Hard quota limit in bytes for filesystem subvolumes. `None`
    /// means no limit set (the subvolume can grow to fill the
    /// filesystem). Always `None` for block subvolumes — their
    /// ceiling is `volsize_bytes`, not a quota.
    pub quota_bytes: Option<u64>,
    /// Compression algorithm applied to this subvolume (e.g. `lz4`, `zstd`).
    pub compression: Option<String>,
    /// Free-text description or notes for this subvolume.
    pub comments: Option<String>,
    // Block-specific
    /// Size of the backing sparse image in bytes (block subvolumes only).
    pub volsize_bytes: Option<u64>,
    /// Loop device path currently attached to the backing image (block subvolumes only).
    pub block_device: Option<String>,
    /// Names of snapshots belonging to this subvolume.
    pub snapshots: Vec<String>,
    /// Token name that created this subvolume; None for subvolumes created by human users.
    pub owner: Option<String>,
    /// Arbitrary key-value metadata stored as POSIX xattrs (user.* namespace).
    /// Used by nasty-csi to track CSI volume metadata without sidecar files.
    #[serde(default)]
    pub properties: HashMap<String, String>,
    /// Parent subvolume name if this is a clone (from bcachefs snapshot_parent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Whether O_DIRECT is enabled on the loop device (block subvolumes only).
    #[serde(default)]
    pub direct_io: bool,
    /// Effective bcachefs options set on this subvolume (from bcachefs_effective.* xattrs).
    /// Only includes options that differ from the filesystem default.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub bcachefs_options: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// Snapshot name (unique within the parent subvolume).
    pub name: String,
    /// Name of the parent subvolume.
    pub subvolume: String,
    /// Name of the filesystem that contains this snapshot.
    pub filesystem: String,
    /// Absolute filesystem path to the snapshot directory.
    pub path: String,
    /// Whether this snapshot is read-only.
    pub read_only: bool,
    /// Parent subvolume path as tracked by bcachefs (from snapshot_parent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Loop device path if this snapshot's vol.img is currently attached (block snapshots only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_device: Option<String>,
}

/// In-memory metadata read from xattrs on the subvolume directory.
struct SubvolumeMeta {
    subvolume_type: SubvolumeType,
    volsize_bytes: Option<u64>,
    compression: Option<String>,
    comments: Option<String>,
    owner: Option<String>,
    direct_io: bool,
}

/// All xattr data for a subvolume — both internal metadata and user-visible properties.
/// Read in a single `xattr::list()` + N `xattr::get()` pass.
struct SubvolumeAttrs {
    meta: SubvolumeMeta,
    properties: HashMap<String, String>,
    /// Effective bcachefs options (from bcachefs_effective.* xattrs).
    bcachefs_options: HashMap<String, String>,
}

/// Read all xattrs from a subvolume in one pass.
/// Splits results into internal metadata (`user.nasty.*`) and user-visible properties
/// (`user.nasty-csi:*` etc), avoiding duplicate enumeration.
const BCACHEFS_EFFECTIVE_NS: &str = "bcachefs_effective.";

fn read_all_xattrs(path: &Path) -> SubvolumeAttrs {
    let mut meta_raw: HashMap<String, String> = HashMap::new();
    let mut properties: HashMap<String, String> = HashMap::new();
    let mut bcachefs_options: HashMap<String, String> = HashMap::new();

    if let Ok(attrs) = xattr::list(path) {
        for name in attrs {
            let name_str = name.to_string_lossy();
            let value = match xattr::get(path, &*name_str) {
                Ok(Some(bytes)) => match String::from_utf8(bytes) {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                _ => continue,
            };
            if let Some(key) = name_str.strip_prefix(BCACHEFS_EFFECTIVE_NS) {
                // bcachefs effective options (bcachefs_effective.*)
                bcachefs_options.insert(key.to_string(), value);
            } else if let Some(key) = name_str.strip_prefix(XATTR_NS) {
                if key.starts_with(NASTY_KEY_PREFIX) {
                    meta_raw.insert(key.to_string(), value);
                } else {
                    properties.insert(key.to_string(), value);
                }
            }
        }
    }

    let subvolume_type = match meta_raw.get("nasty.type").map(|s| s.as_str()) {
        Some("block") => SubvolumeType::Block,
        Some("filesystem") => SubvolumeType::Filesystem,
        _ => {
            if path.join(BLOCK_FILE_NAME).exists() {
                SubvolumeType::Block
            } else {
                SubvolumeType::Filesystem
            }
        }
    };

    SubvolumeAttrs {
        meta: SubvolumeMeta {
            subvolume_type,
            volsize_bytes: meta_raw.get("nasty.volsize").and_then(|s| s.parse().ok()),
            compression: meta_raw.remove("nasty.compression"),
            comments: meta_raw.remove("nasty.comment"),
            owner: meta_raw.remove("nasty.owner"),
            direct_io: meta_raw
                .get("nasty.direct_io")
                .map(|s| s == "true")
                .unwrap_or(false),
        },
        properties,
        bcachefs_options,
    }
}

/// Read only NASty-internal metadata from the reserved `user.nasty.*` xattrs.
/// Used by code paths that don't need user-visible properties.
fn read_meta_xattrs(path: &Path) -> SubvolumeMeta {
    let get = |key: &str| -> Option<String> {
        xattr::get(path, key)
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok())
    };

    let subvolume_type = match get(XATTR_NASTY_TYPE).as_deref() {
        Some("block") => SubvolumeType::Block,
        Some("filesystem") => SubvolumeType::Filesystem,
        _ => {
            if path.join(BLOCK_FILE_NAME).exists() {
                SubvolumeType::Block
            } else {
                SubvolumeType::Filesystem
            }
        }
    };

    SubvolumeMeta {
        subvolume_type,
        volsize_bytes: get(XATTR_NASTY_VOLSIZE).and_then(|s| s.parse().ok()),
        compression: get(XATTR_NASTY_COMPRESSION),
        comments: get(XATTR_NASTY_COMMENT),
        owner: get(XATTR_NASTY_OWNER),
        direct_io: get(XATTR_NASTY_DIRECT_IO).as_deref() == Some("true"),
    }
}

/// Write NASty-internal metadata as reserved `user.nasty.*` xattrs.
fn write_meta_xattrs(
    path: &str,
    subvolume_type: &SubvolumeType,
    volsize_bytes: Option<u64>,
    compression: Option<&str>,
    comments: Option<&str>,
    owner: Option<&str>,
    direct_io: bool,
) -> Result<(), SubvolumeError> {
    let type_str = match subvolume_type {
        SubvolumeType::Filesystem => "filesystem",
        SubvolumeType::Block => "block",
    };
    xattr::set(path, XATTR_NASTY_TYPE, type_str.as_bytes())
        .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr type: {e}")))?;

    if let Some(v) = volsize_bytes {
        xattr::set(path, XATTR_NASTY_VOLSIZE, v.to_string().as_bytes())
            .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr volsize: {e}")))?;
    }
    if let Some(c) = compression {
        xattr::set(path, XATTR_NASTY_COMPRESSION, c.as_bytes())
            .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr compression: {e}")))?;
    }
    if let Some(c) = comments {
        xattr::set(path, XATTR_NASTY_COMMENT, c.as_bytes())
            .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr comment: {e}")))?;
    }
    if let Some(o) = owner {
        xattr::set(path, XATTR_NASTY_OWNER, o.as_bytes())
            .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr owner: {e}")))?;
    }
    if direct_io {
        xattr::set(path, XATTR_NASTY_DIRECT_IO, b"true")
            .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr direct_io: {e}")))?;
    }
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSubvolumeRequest {
    /// Name of the filesystem to create the subvolume in.
    pub filesystem: String,
    /// Name for the new subvolume.
    pub name: String,
    /// Whether to create a filesystem or block-backed subvolume (default: filesystem).
    #[serde(default = "default_type")]
    pub subvolume_type: SubvolumeType,
    /// Size of the block backing image in bytes (required for block subvolumes).
    pub volsize_bytes: Option<u64>,
    /// Compression algorithm to set on the subvolume (e.g. `lz4`, `zstd`).
    pub compression: Option<String>,
    /// Optional description for the subvolume.
    pub comments: Option<String>,
    /// Enable O_DIRECT on the loop device (bypasses host page cache for the backing file).
    #[serde(default)]
    pub direct_io: Option<bool>,
    /// Device or label for foreground writes (overrides filesystem default).
    pub foreground_target: Option<String>,
    /// Device or label for background moves/recompression (overrides filesystem default).
    pub background_target: Option<String>,
    /// Device or label to promote data to on read (cache tier, overrides filesystem default).
    pub promote_target: Option<String>,
    /// Device or label for metadata/btree writes (overrides filesystem default).
    pub metadata_target: Option<String>,
    /// Number of data replicas for this subvolume (overrides filesystem default).
    pub data_replicas: Option<u32>,
}

fn default_type() -> SubvolumeType {
    SubvolumeType::Filesystem
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSubvolumeRequest {
    /// Name of the filesystem containing the subvolume.
    pub filesystem: String,
    /// Name of the subvolume to delete.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSnapshotRequest {
    /// Name of the filesystem containing the subvolume.
    pub filesystem: String,
    /// Name of the subvolume to snapshot.
    pub subvolume: String,
    /// Name for the new snapshot.
    pub name: String,
    /// Whether to create a read-only snapshot (default: true).
    pub read_only: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSnapshotRequest {
    /// Name of the filesystem containing the snapshot.
    pub filesystem: String,
    /// Name of the parent subvolume.
    pub subvolume: String,
    /// Name of the snapshot to delete.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CloneSnapshotRequest {
    /// Name of the filesystem containing the snapshot.
    pub filesystem: String,
    /// Name of the parent subvolume.
    pub subvolume: String,
    /// Name of the snapshot to clone.
    pub snapshot: String,
    /// Name for the new writable subvolume created from the snapshot.
    pub new_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RollbackSnapshotRequest {
    /// Name of the filesystem containing the snapshot.
    pub filesystem: String,
    /// Name of the parent subvolume to roll back.
    pub subvolume: String,
    /// Name of the snapshot to roll the subvolume back to.
    pub snapshot: String,
}

/// Outcome of a rollback: the recreated subvolume plus the name of the
/// read-only safety snapshot taken of the pre-rollback state (the undo
/// point — roll back to it to get the prior state back).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RollbackResult {
    pub subvolume: Subvolume,
    pub safety_snapshot: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CloneSubvolumeRequest {
    /// Name of the filesystem containing the source subvolume.
    pub filesystem: String,
    /// Name of the subvolume to clone.
    pub name: String,
    /// Name for the new writable subvolume.
    pub new_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResizeSubvolumeRequest {
    /// Name of the filesystem containing the subvolume.
    pub filesystem: String,
    /// Name of the subvolume to resize.
    pub name: String,
    /// New size in bytes. For block subvolumes: sparse image size. For filesystem subvolumes: bcachefs quota limit.
    pub volsize_bytes: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateSubvolumeRequest {
    /// Name of the filesystem containing the subvolume.
    pub filesystem: String,
    /// Name of the subvolume to update.
    pub name: String,
    /// New compression algorithm (e.g. `lz4`, `zstd`, `none`). `none` clears compression.
    pub compression: Option<String>,
    /// New description for the subvolume. Empty string clears the comment.
    pub comments: Option<String>,
    /// Device or label for foreground writes. Use `-` to remove.
    pub foreground_target: Option<String>,
    /// Device or label for background moves/recompression. Use `-` to remove.
    pub background_target: Option<String>,
    /// Device or label to promote data to on read. Use `-` to remove.
    pub promote_target: Option<String>,
    /// Device or label for metadata/btree writes. Use `-` to remove.
    pub metadata_target: Option<String>,
    /// Number of data replicas. Use `0` to reset to filesystem default.
    pub data_replicas: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetPropertiesRequest {
    /// Name of the filesystem containing the subvolume.
    pub filesystem: String,
    /// Name of the subvolume to update.
    pub name: String,
    /// Key-value pairs to set (merged with existing properties).
    pub properties: HashMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemovePropertiesRequest {
    /// Name of the filesystem containing the subvolume.
    pub filesystem: String,
    /// Name of the subvolume to update.
    pub name: String,
    /// Property keys to remove.
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindByPropertyRequest {
    /// Optional filesystem to restrict the search to.
    pub filesystem: Option<String>,
    /// xattr property key to match against.
    pub key: String,
    /// Value that the property key must equal.
    pub value: String,
}

pub struct SubvolumeService {
    filesystems: FilesystemService,
}

impl SubvolumeService {
    pub fn new(filesystems: FilesystemService) -> Self {
        Self { filesystems }
    }

    /// Re-attach loop devices for block subvolumes after filesystems are mounted.
    /// Returns a map of subvolume_name → current loop device path so callers
    /// can patch NVMe-oF / iSCSI state files before those services start.
    pub async fn restore_block_devices(&self) -> std::collections::HashMap<String, String> {
        let all = match self.list_all(None, None).await {
            Ok(v) => v,
            Err(e) => {
                warn!("restore_block_devices: failed to list subvolumes: {e}");
                return std::collections::HashMap::new();
            }
        };

        let block_subvols: Vec<_> = all
            .into_iter()
            .filter(|s| s.subvolume_type == SubvolumeType::Block)
            .collect();

        let mut dev_map = std::collections::HashMap::new();

        if block_subvols.is_empty() {
            info!("No block subvolumes to restore");
            return dev_map;
        }

        for subvol in block_subvols {
            let img_path = format!("{}/{BLOCK_FILE_NAME}", subvol.path);
            if !Path::new(&img_path).exists() {
                warn!(
                    "Block image {img_path} not found for {}/{}",
                    subvol.filesystem, subvol.name
                );
                continue;
            }

            // Use existing loop device if already attached (engine restart, not reboot)
            let loop_dev = if let Some(existing) = find_loop_device(&img_path).await {
                info!(
                    "Loop device already attached for {}/{}",
                    subvol.filesystem, subvol.name
                );
                existing
            } else {
                let mut args = vec!["--find", "--show"];
                if subvol.direct_io {
                    args.push("--direct-io=on");
                }
                args.push(&img_path);
                match cmd::run_ok("losetup", &args).await {
                    Ok(dev) => {
                        let dev = dev.trim().to_string();
                        info!(
                            "Attached {} for block subvolume {}/{}",
                            dev, subvol.filesystem, subvol.name
                        );
                        dev
                    }
                    Err(e) => {
                        warn!(
                            "Failed to attach loop device for {}/{}: {e}",
                            subvol.filesystem, subvol.name
                        );
                        continue;
                    }
                }
            };

            dev_map.insert(subvol.name.clone(), loop_dev);
        }

        dev_map
    }

    /// One-shot migration: assign a project quota ID to every
    /// filesystem subvolume that doesn't have one yet, AND rewrite any
    /// hard limits that match the pre-fix 1024× bug pattern. Idempotent
    /// — safe to run on every engine startup.
    ///
    /// - Missing project IDs: legacy subvolumes created before #176 had
    ///   no quota row at all, so `repquota` couldn't report their usage
    ///   and the WebUI's size cell showed `—`.
    /// - Inflated quotas: until the setquota-units fix, every requested
    ///   quota was stored 1024× larger than intended (bytes passed where
    ///   KiB blocks were expected). If `hard_bytes == volsize_xattr *
    ///   1024`, that's the exact bug signature — rewrite with the
    ///   corrected value. Anything else (manual edits, intentional
    ///   over/under-commit) is left untouched.
    pub async fn reconcile_project_ids(&self) {
        let filesystems = match self.filesystems.list().await {
            Ok(v) => v,
            Err(e) => {
                warn!("reconcile_project_ids: filesystems.list failed: {e}");
                return;
            }
        };

        for fs in filesystems.into_iter().filter(|f| f.mounted) {
            let Some(mount_point) = fs.mount_point.clone() else {
                continue;
            };
            let project_usages = query_project_usages(&mount_point).await;

            let subvols = match self.list(&fs.name, None).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("reconcile_project_ids: list({}) failed: {e}", fs.name);
                    continue;
                }
            };

            let mut assigned = 0usize;
            let mut rewritten = 0usize;
            for sv in subvols
                .into_iter()
                .filter(|s| s.subvolume_type == SubvolumeType::Filesystem)
            {
                let projid = project_id_for(&fs.name, &sv.name);
                match project_usages.get(&projid) {
                    None => {
                        info!(
                            "reconcile_project_ids: assigning project {projid} to {} ({}@{})",
                            sv.path, sv.name, fs.name
                        );
                        // hard=0 means tracked-but-unlimited; matches the
                        // semantics applied at create time for subvolumes
                        // without an explicit quota.
                        set_project_quota(&mount_point, &sv.path, projid, 0).await;
                        assigned += 1;
                    }
                    Some(info) => {
                        // Detect the 1024× setquota-units bug: an
                        // intended volsize of N bytes was stored as N
                        // KiB blocks, which we read back as N * 1024
                        // bytes. Rewrite only when the on-disk value
                        // matches that exact pattern.
                        if let Some(volsize) = sv.volsize_bytes
                            && volsize > 0
                            && info.hard_bytes == volsize.saturating_mul(1024)
                        {
                            info!(
                                "reconcile_project_ids: rewriting inflated quota for {} \
                                 (was {} bytes, intended {} bytes)",
                                sv.path, info.hard_bytes, volsize
                            );
                            set_project_quota(&mount_point, &sv.path, projid, volsize).await;
                            rewritten += 1;
                        }
                    }
                }
            }

            if assigned > 0 || rewritten > 0 {
                info!(
                    "reconcile_project_ids: {assigned} assigned, {rewritten} rewritten on filesystem '{}'",
                    fs.name
                );
            }
        }
    }

    /// Get the mount point for a filesystem, or error if not mounted
    async fn fs_mount_point(&self, fs_name: &str) -> Result<String, SubvolumeError> {
        let fs = self
            .filesystems
            .get(fs_name)
            .await
            .map_err(|_| SubvolumeError::FilesystemNotFound(fs_name.to_string()))?;

        fs.mount_point
            .ok_or_else(|| SubvolumeError::FilesystemNotMounted(fs_name.to_string()))
    }

    /// List subvolumes in a filesystem.
    /// `owner_filter`: if Some, only return subvolumes owned by that token.
    pub async fn list(
        &self,
        fs_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Subvolume>, SubvolumeError> {
        let mount_point = self.fs_mount_point(fs_name).await?;
        let mut subvolumes = Vec::new();

        // Ask bcachefs which paths are real subvolumes (filters out plain dirs)
        let info = bcachefs_list_all(&mount_point).await;

        // Batch queries: run repquota + losetup once instead of du/losetup per subvolume
        let (project_usages, losetup_map) =
            tokio::join!(query_project_usages(&mount_point), build_losetup_map());

        // List all subvolumes except snapshots (@) and internal .nasty/* ones.
        for name in info.subvol_paths.iter().filter(|p| {
            !p.is_empty() && !p.contains('@') && !p.starts_with(".nasty/") && *p != ".nasty"
        }) {
            let path_str = subvol_path(&mount_point, name);
            let path = Path::new(&path_str);

            // Single-pass xattr read: meta + properties in one list+get sweep
            let attrs = read_all_xattrs(path);

            // Apply owner filter: operators only see their own subvolumes
            if let Some(filter) = owner_filter
                && attrs.meta.owner.as_deref() != Some(filter)
            {
                continue;
            }

            // Build snapshot list from the already-fetched bcachefs data
            let snap_prefix = format!("{name}@");
            let snapshots: Vec<Snapshot> = info
                .snapshot_flags
                .iter()
                .filter(|(p, _)| p.starts_with(&snap_prefix) && !p.contains('/'))
                .map(|(p, &read_only)| {
                    let snap_name = p[snap_prefix.len()..].to_string();
                    let parent = info.snapshot_parents.get(p).cloned();
                    Snapshot {
                        name: snap_name.clone(),
                        subvolume: name.to_string(),
                        filesystem: fs_name.to_string(),
                        path: snap_path(&mount_point, name, &snap_name),
                        read_only,
                        parent,
                        block_device: None,
                    }
                })
                .collect();

            // Get size + quota: project quota for filesystem subvols, stat for block subvols.
            // hard_bytes == 0 in repquota means "no limit" → translate to None.
            let (size, quota_bytes) = match attrs.meta.subvolume_type {
                SubvolumeType::Block => (block_image_size(&path_str), None),
                SubvolumeType::Filesystem => {
                    let projid = project_id_for(fs_name, name);
                    match project_usages.get(&projid) {
                        Some(info) => (
                            Some(info.used_bytes),
                            (info.hard_bytes > 0).then_some(info.hard_bytes),
                        ),
                        None => (None, None),
                    }
                }
            };

            // Look up loop device from pre-built map instead of spawning losetup per subvol
            let block_device = if attrs.meta.subvolume_type == SubvolumeType::Block {
                let img_path = format!("{path_str}/{BLOCK_FILE_NAME}");
                find_loop_device_from_map(&losetup_map, &img_path)
            } else {
                None
            };

            let parent = info.snapshot_parents.get(name.as_str()).cloned();

            subvolumes.push(Subvolume {
                name: name.to_string(),
                filesystem: fs_name.to_string(),
                subvolume_type: attrs.meta.subvolume_type,
                path: path_str.clone(),
                used_bytes: size,
                quota_bytes,
                compression: attrs.meta.compression,
                comments: attrs.meta.comments,
                volsize_bytes: attrs.meta.volsize_bytes,
                block_device,
                snapshots: snapshots.iter().map(|s| s.name.clone()).collect(),
                owner: attrs.meta.owner,
                properties: attrs.properties,
                parent,
                direct_io: attrs.meta.direct_io,
                bcachefs_options: attrs.bcachefs_options,
            });
        }

        Ok(subvolumes)
    }

    /// List subvolumes across all mounted filesystems.
    /// `fs_filter`: if Some, only include that filesystem.
    /// `owner_filter`: if Some, only include subvolumes owned by that token.
    pub async fn list_all(
        &self,
        fs_filter: Option<&str>,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Subvolume>, SubvolumeError> {
        let all_fs = self
            .filesystems
            .list()
            .await
            .map_err(|e| SubvolumeError::CommandFailed(e.to_string()))?;

        let mut all = Vec::new();
        for fs in all_fs {
            if !fs.mounted {
                continue;
            }
            if let Some(filter) = fs_filter
                && fs.name != filter
            {
                continue;
            }
            match self.list(&fs.name, owner_filter).await {
                Ok(mut subvols) => all.append(&mut subvols),
                Err(_) => continue,
            }
        }
        Ok(all)
    }

    /// Get a single subvolume.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn get(
        &self,
        fs_name: &str,
        name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvolumes = self.list(fs_name, owner_filter).await?;
        subvolumes
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                // Distinguish "not found" from "exists but not yours"
                // We return NotFound in both cases to avoid leaking existence
                SubvolumeError::NotFound(name.to_string())
            })
    }

    /// Create a new subvolume.
    /// `owner`: if Some, records this token name as the subvolume owner.
    pub async fn create(
        &self,
        req: CreateSubvolumeRequest,
        owner: Option<String>,
    ) -> Result<Subvolume, SubvolumeError> {
        validate_subvolume_name(&req.name)?;

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

        if Path::new(&subvol_path).exists() {
            info!(
                "Subvolume '{}' already exists in filesystem '{}', returning existing (idempotent)",
                req.name, req.filesystem
            );
            return self.get(&req.filesystem, &req.name, None).await;
        }

        if req.subvolume_type == SubvolumeType::Block && req.volsize_bytes.is_none() {
            return Err(SubvolumeError::VolsizeRequired);
        }
        if let Some(bytes) = req.volsize_bytes {
            validate_volsize_bytes(bytes)?;
        }

        // Ensure parent directories exist for nested subvolumes (e.g. "projects/web")
        if let Some(parent) = Path::new(&subvol_path).parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create the bcachefs subvolume
        info!(
            "Creating subvolume '{}' in filesystem '{}'",
            req.name, req.filesystem
        );
        cmd::run_ok("bcachefs", &["subvolume", "create", &subvol_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        // New subvolumes must be writable through the sharing layer:
        // Samba forces writes through a fixed identity (`nobody` on
        // guest shares, the first valid user otherwise) and NFS through
        // squashed uids — access control lives in the protocol layer,
        // not POSIX bits, the same philosophy as the share-root chmod
        // in nasty-sharing::smb. Without this, a subvolume created
        // after the share stays root:0755 and every write into it is
        // denied until the operator chmods it by hand (#482).
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                tokio::fs::set_permissions(&subvol_path, std::fs::Permissions::from_mode(0o777))
                    .await
            {
                warn!(
                    "chmod 0777 {subvol_path} failed: {e} — SMB/NFS writes into the new subvolume may be denied"
                );
            }
        }

        // Set compression if specified
        if let Some(ref comp) = req.compression {
            info!("Setting compression={} on subvolume '{}'", comp, req.name);
            let _ = cmd::run_ok(
                "bcachefs",
                &[
                    "set-file-option",
                    &format!("--compression={comp}"),
                    &subvol_path,
                ],
            )
            .await;
        }

        // Set tiering targets if specified
        for (flag, value) in [
            ("--foreground_target", &req.foreground_target),
            ("--background_target", &req.background_target),
            ("--promote_target", &req.promote_target),
            ("--metadata_target", &req.metadata_target),
        ] {
            if let Some(t) = value {
                info!("Setting {}={} on subvolume '{}'", flag, t, req.name);
                let _ = cmd::run_ok(
                    "bcachefs",
                    &["set-file-option", &format!("{flag}={t}"), &subvol_path],
                )
                .await;
            }
        }

        // Set data replicas if specified
        if let Some(replicas) = req.data_replicas {
            info!(
                "Setting data_replicas={} on subvolume '{}'",
                replicas, req.name
            );
            let _ = cmd::run_ok(
                "bcachefs",
                &[
                    "set-file-option",
                    &format!("--data_replicas={replicas}"),
                    &subvol_path,
                ],
            )
            .await;
        }

        // For filesystem subvolumes: always assign a project ID so usage
        // tracking via repquota works regardless of whether the user set
        // a hard limit. `0` is the quota-tools convention for "no limit"
        // — repquota still reports usage, it just won't enforce.
        if req.subvolume_type == SubvolumeType::Filesystem {
            let projid = project_id_for(&req.filesystem, &req.name);
            let limit = req.volsize_bytes.unwrap_or(0);
            set_project_quota(&mount_point, &subvol_path, projid, limit).await;
        }

        // For block subvolumes: create sparse file and attach loop device
        if req.subvolume_type == SubvolumeType::Block {
            let volsize = req.volsize_bytes.unwrap();
            let img_path = format!("{subvol_path}/{BLOCK_FILE_NAME}");

            info!(
                "Creating block subvolume '{}' with size {} bytes",
                req.name, volsize
            );
            cmd::run_ok("truncate", &["-s", &volsize.to_string(), &img_path])
                .await
                .map_err(SubvolumeError::CommandFailed)?;

            // Set nocow on the sparse image — writes go in-place, reducing I/O stall
            // duration during bcachefs snapshots. Snapshots still work (COW is forced
            // for the first write after snapshot), but subsequent writes are in-place.
            //
            // HOWEVER: nocow implicitly disables encryption, checksums, and compression
            // at the extent level. On encrypted filesystems this causes reconcile errors
            // (extent_io_opts_not_set) because the checker finds unencrypted extents.
            // See: https://github.com/koverstreet/bcachefs/issues/1112
            let fs_encrypted = self
                .filesystems
                .get(&req.filesystem)
                .await
                .map(|fs| fs.options.encrypted == Some(true))
                .unwrap_or(false);

            if fs_encrypted {
                info!(
                    "Skipping nocow on {img_path} — filesystem is encrypted (nocow disables encryption)"
                );
            } else {
                match cmd::run_ok("bcachefs", &["set-file-option", "--nocow", &img_path]).await {
                    Ok(_) => info!("Set nocow on {img_path}"),
                    Err(e) => warn!("Failed to set nocow on {img_path}: {e}"),
                }
            }

            info!("Attaching loop device for '{}'", req.name);
            let mut losetup_args = vec!["--find", "--show"];
            if req.direct_io.unwrap_or(false) {
                losetup_args.push("--direct-io=on");
            }
            losetup_args.push(&img_path);
            cmd::run_ok("losetup", &losetup_args)
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }

        // Save metadata as xattrs on the subvolume directory
        write_meta_xattrs(
            &subvol_path,
            &req.subvolume_type,
            req.volsize_bytes,
            req.compression.as_deref(),
            req.comments.as_deref(),
            owner.as_deref(),
            req.direct_io.unwrap_or(false),
        )?;

        self.get(&req.filesystem, &req.name, None).await
    }

    /// List child subvolumes nested under a given parent.
    pub async fn list_children(
        &self,
        filesystem: &str,
        name: &str,
    ) -> Result<Vec<String>, SubvolumeError> {
        let mount_point = self.fs_mount_point(filesystem).await?;
        Ok(find_child_subvolumes(&mount_point, name).await)
    }

    /// Delete a subvolume.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn delete(
        &self,
        req: DeleteSubvolumeRequest,
        owner_filter: Option<&str>,
    ) -> Result<(), SubvolumeError> {
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;

        // For block subvolumes: detach loop device first. A failure here
        // (typically EBUSY — something still has the device open) used
        // to log a warn and continue; the subsequent bcachefs delete
        // would then fail because the backing file is in use, leaving
        // the loop device attached to a half-gone subvolume. Fail
        // loudly with the real cause instead so the operator knows
        // *which* device is stuck and why, before we touch the
        // filesystem state.
        //
        // Exception: tolerate "No such device" / "not in use" — that's
        // the shape losetup returns when the device is already detached
        // (operator cleaned up manually, or a prior failed delete made
        // it part-way). Treating that as success keeps retries idempotent.
        if subvol.subvolume_type == SubvolumeType::Block
            && let Some(ref loop_dev) = subvol.block_device
        {
            info!("Detaching loop device {} for '{}'", loop_dev, req.name);
            if let Err(e) = cmd::run_ok("losetup", &["-d", loop_dev]).await
                && !is_already_detached(&e)
            {
                return Err(SubvolumeError::LoopDetachFailed {
                    device: loop_dev.clone(),
                    reason: e,
                });
            }
        }

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

        // bcachefs snapshots are independent first-class subvolumes — they survive
        // parent deletion. We intentionally do NOT delete snapshots here so that
        // snapshot-based restore/DR scenarios work correctly.

        // Delete child subvolumes first (depth-first) — bcachefs rejects
        // deleting a subvolume that contains nested subvolumes.
        //
        // Try every child even if one fails, so the operator gets the
        // full list of stuck children in a single round trip instead
        // of fix-one, retry, fix-next, retry. The partial-deletion
        // window (some children gone, others not) is unavoidable —
        // bcachefs has no transactional batch-delete and no "undo".
        // Erroring out before the parent delete preserves the same
        // partial state the old warn-and-continue path produced, but
        // surfaces the real cause instead of a generic
        // "directory not empty" from the parent attempt.
        let children = find_child_subvolumes(&mount_point, &req.name).await;
        let mut stuck: Vec<String> = Vec::new();
        for child in children.iter().rev() {
            let child_path = format!("{mount_point}/{child}");
            info!(
                "Deleting child subvolume '{child}' before parent '{}'",
                req.name
            );
            if let Err(e) = cmd::run_ok("bcachefs", &["subvolume", "delete", &child_path]).await {
                warn!("Failed to delete child subvolume '{child}': {e}");
                stuck.push(format!("{child} ({e})"));
            }
        }
        if !stuck.is_empty() {
            return Err(SubvolumeError::ChildrenStuck(stuck.join("; ")));
        }

        info!(
            "Deleting subvolume '{}' from filesystem '{}'",
            req.name, req.filesystem
        );
        cmd::run_ok("bcachefs", &["subvolume", "delete", &subvol_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        // Remove project quota registration if this was a filesystem subvolume
        if subvol.subvolume_type == SubvolumeType::Filesystem {
            let projid = project_id_for(&req.filesystem, &req.name);
            unregister_project(projid);
        }

        // Xattrs are deleted automatically with the subvolume inode — no cleanup needed.

        Ok(())
    }

    /// Attach a block subvolume's loop device (e.g. after reboot).
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn attach(
        &self,
        fs_name: &str,
        name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(fs_name, name, owner_filter).await?;
        if subvol.subvolume_type != SubvolumeType::Block {
            return Err(SubvolumeError::CommandFailed(
                "only block subvolumes can be attached".to_string(),
            ));
        }
        if subvol.block_device.is_some() {
            return Ok(subvol);
        }

        let img_path = format!("{}/{}", subvol.path, BLOCK_FILE_NAME);
        info!("Attaching loop device for '{}'", name);
        let mut args = vec!["--find", "--show"];
        if subvol.direct_io {
            args.push("--direct-io=on");
        }
        args.push(&img_path);
        cmd::run_ok("losetup", &args)
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        self.get(fs_name, name, owner_filter).await
    }

    /// Detach a block subvolume's loop device.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn detach(
        &self,
        fs_name: &str,
        name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(fs_name, name, owner_filter).await?;
        if let Some(ref loop_dev) = subvol.block_device {
            info!("Detaching loop device {} for '{}'", loop_dev, name);
            cmd::run_ok("losetup", &["-d", loop_dev])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }
        self.get(fs_name, name, owner_filter).await
    }

    /// Resize a subvolume.
    /// For block subvolumes: resizes the sparse image and updates the loop device.
    /// For filesystem subvolumes: updates the bcachefs project quota limit.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn resize(
        &self,
        req: ResizeSubvolumeRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        validate_volsize_bytes(req.volsize_bytes)?;
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;

        match subvol.subvolume_type {
            SubvolumeType::Block => {
                let img_path = format!("{}/{}", subvol.path, BLOCK_FILE_NAME);
                info!(
                    "Resizing block subvolume '{}' to {} bytes",
                    req.name, req.volsize_bytes
                );
                cmd::run_ok(
                    "truncate",
                    &["-s", &req.volsize_bytes.to_string(), &img_path],
                )
                .await
                .map_err(SubvolumeError::CommandFailed)?;

                // If loop device is attached, inform the kernel of the new size
                if let Some(ref loop_dev) = subvol.block_device {
                    info!(
                        "Updating loop device {} capacity for '{}'",
                        loop_dev, req.name
                    );
                    cmd::run_ok("losetup", &["--set-capacity", loop_dev])
                        .await
                        .map_err(SubvolumeError::CommandFailed)?;
                }
            }
            SubvolumeType::Filesystem => {
                info!(
                    "Resizing filesystem subvolume '{}' quota to {} bytes",
                    req.name, req.volsize_bytes
                );
                let mount_point = self.fs_mount_point(&req.filesystem).await?;
                let projid = project_id_for(&req.filesystem, &req.name);
                let proj_name = format!("nasty-{projid}");
                let bytes_str = req.volsize_bytes.to_string();
                if let Err(e) = cmd::run_ok(
                    "setquota",
                    &[
                        "-P",
                        &proj_name,
                        &bytes_str,
                        &bytes_str,
                        "0",
                        "0",
                        &mount_point,
                    ],
                )
                .await
                {
                    warn!("setquota failed for project {proj_name} on {mount_point}: {e}");
                }
            }
        }

        // Update volsize xattr
        let path = subvol_path(&self.fs_mount_point(&req.filesystem).await?, &req.name);
        xattr::set(
            &path,
            XATTR_NASTY_VOLSIZE,
            req.volsize_bytes.to_string().as_bytes(),
        )
        .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr volsize: {e}")))?;

        self.get(&req.filesystem, &req.name, owner_filter).await
    }

    /// Update compression and/or comments on an existing subvolume.
    pub async fn update(
        &self,
        req: UpdateSubvolumeRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;
        let path = &subvol.path;

        if let Some(ref comp) = req.compression {
            let comp_value = if comp == "none" || comp.is_empty() {
                "none"
            } else {
                comp.as_str()
            };
            info!(
                "Setting compression={} on subvolume '{}'",
                comp_value, req.name
            );
            cmd::run_ok(
                "bcachefs",
                &[
                    "set-file-option",
                    &format!("--compression={comp_value}"),
                    path,
                ],
            )
            .await
            .map_err(SubvolumeError::CommandFailed)?;

            if comp_value == "none" {
                let _ = xattr::remove(path, XATTR_NASTY_COMPRESSION);
            } else {
                xattr::set(path, XATTR_NASTY_COMPRESSION, comp_value.as_bytes()).map_err(|e| {
                    SubvolumeError::CommandFailed(format!("setxattr compression: {e}"))
                })?;
            }
        }

        if let Some(ref comments) = req.comments {
            if comments.is_empty() {
                let _ = xattr::remove(path, XATTR_NASTY_COMMENT);
            } else {
                xattr::set(path, XATTR_NASTY_COMMENT, comments.as_bytes())
                    .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr comment: {e}")))?;
            }
        }

        // Update tiering targets if specified (use "-" to remove)
        for (flag, value) in [
            ("--foreground_target", &req.foreground_target),
            ("--background_target", &req.background_target),
            ("--promote_target", &req.promote_target),
            ("--metadata_target", &req.metadata_target),
        ] {
            if let Some(t) = value {
                info!("Setting {}={} on subvolume '{}'", flag, t, req.name);
                cmd::run_ok(
                    "bcachefs",
                    &["set-file-option", &format!("{flag}={t}"), path],
                )
                .await
                .map_err(SubvolumeError::CommandFailed)?;
            }
        }

        // Update data replicas if specified (use 0 to reset to filesystem default)
        if let Some(replicas) = req.data_replicas {
            info!(
                "Setting data_replicas={} on subvolume '{}'",
                replicas, req.name
            );
            let flag = if replicas == 0 {
                "--data_replicas=-".to_string()
            } else {
                format!("--data_replicas={replicas}")
            };
            cmd::run_ok("bcachefs", &["set-file-option", &flag, path])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }

        self.get(&req.filesystem, &req.name, owner_filter).await
    }

    /// Create a snapshot of a subvolume.
    /// `owner_filter`: if Some, verifies the caller owns the parent subvolume.
    pub async fn create_snapshot(
        &self,
        req: CreateSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<Snapshot, SubvolumeError> {
        // Verify ownership of the parent subvolume
        self.get(&req.filesystem, &req.subvolume, owner_filter)
            .await?;

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let source_path = subvol_path(&mount_point, &req.subvolume);
        let snap_path = snap_path(&mount_point, &req.subvolume, &req.name);

        if !Path::new(&source_path).exists() {
            return Err(SubvolumeError::NotFound(req.subvolume.clone()));
        }

        if Path::new(&snap_path).exists() {
            return Err(SubvolumeError::AlreadyExists(req.name.clone()));
        }

        // For block subvolumes, flush all pending I/O before snapshotting.
        // Initiators (iSCSI, NVMe-oF) may have dirty data in their page cache
        // that hasn't been written to the backing loop device yet. A sync ensures
        // the snapshot captures a consistent state.
        let subvol = self
            .get(&req.filesystem, &req.subvolume, owner_filter)
            .await?;
        if subvol.subvolume_type == SubvolumeType::Block
            && let Some(ref loop_dev) = subvol.block_device
        {
            info!("Flushing block device {} before snapshot", loop_dev);
            if let Err(e) = cmd::run_ok("blockdev", &["--flushbufs", loop_dev]).await {
                warn!("Failed to flush {loop_dev} before snapshot, proceeding anyway: {e}");
            }
        }

        info!(
            "Creating snapshot '{}' of subvolume '{}/{}'",
            req.name, req.filesystem, req.subvolume
        );
        // Snapshots are always read-only; use snapshot.clone for writable copies
        cmd::run_ok(
            "bcachefs",
            &["subvolume", "snapshot", "-r", &source_path, &snap_path],
        )
        .await
        .map_err(SubvolumeError::CommandFailed)?;

        Ok(Snapshot {
            name: req.name,
            subvolume: req.subvolume.clone(),
            filesystem: req.filesystem,
            path: snap_path,
            read_only: true,
            parent: Some(req.subvolume),
            block_device: None,
        })
    }

    /// Delete a snapshot.
    /// `owner_filter`: if Some, verifies the caller owns the parent subvolume.
    pub async fn delete_snapshot(
        &self,
        req: DeleteSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<(), SubvolumeError> {
        // Verify ownership if the parent subvolume still exists.
        // The parent may have been deleted (DR scenario) — orphaned snapshots
        // should still be deletable.
        if let Ok(_parent) = self
            .get(&req.filesystem, &req.subvolume, owner_filter)
            .await
        {
            // Parent exists and ownership verified
        }

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let snap_path = snap_path(&mount_point, &req.subvolume, &req.name);

        if !Path::new(&snap_path).exists() {
            return Err(SubvolumeError::NotFound(req.name.clone()));
        }

        info!(
            "Deleting snapshot '{}' of subvolume '{}/{}'",
            req.name, req.filesystem, req.subvolume
        );
        cmd::run_ok("bcachefs", &["subvolume", "delete", &snap_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        Ok(())
    }

    /// List snapshots for a specific subvolume using `bcachefs subvolume list-snapshots`.
    pub async fn list_snapshots_for(
        &self,
        fs_name: &str,
        subvol_name: &str,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.fs_mount_point(fs_name).await?;
        let subvol_path = subvol_path(&mount_point, subvol_name);

        if !Path::new(&subvol_path).exists() {
            return Ok(vec![]);
        }

        let info = bcachefs_list_all(&mount_point).await;
        let snap_prefix = format!("{subvol_name}@");
        let snapshots = info
            .snapshot_flags
            .into_iter()
            .filter(|(p, _)| p.starts_with(&snap_prefix) && !p.contains('/'))
            .map(|(p, read_only)| {
                let snap_name = p[snap_prefix.len()..].to_string();
                Snapshot {
                    path: snap_path(&mount_point, subvol_name, &snap_name),
                    name: snap_name,
                    subvolume: subvol_name.to_string(),
                    filesystem: fs_name.to_string(),
                    read_only,
                    parent: Some(subvol_name.to_string()),
                    block_device: None,
                }
            })
            .collect();

        Ok(snapshots)
    }

    /// List all snapshots across all subvolumes in a filesystem.
    /// `owner_filter`: if Some, only returns snapshots whose parent subvolume is owned by that token.
    /// Single-pass scan of the subvolumes/ directory for entries containing '@'.
    pub async fn list_snapshots(
        &self,
        fs_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.fs_mount_point(fs_name).await?;

        // Get owned subvolume names if filter is active
        let owned: Option<std::collections::HashSet<String>> = if owner_filter.is_some() {
            let owned_subvols = self.list(fs_name, owner_filter).await.unwrap_or_default();
            Some(owned_subvols.into_iter().map(|s| s.name).collect())
        } else {
            None
        };

        let info = bcachefs_list_all(&mount_point).await;

        let mut all_snapshots = Vec::new();
        for (rel_path, read_only) in info.snapshot_flags {
            // Snapshots live directly at filesystem root: "subvol@snap" (no '/')
            if rel_path.contains('/') {
                continue;
            }
            let Some(at_pos) = rel_path.find('@') else {
                continue;
            };
            let subvol_name = rel_path[..at_pos].to_string();
            let snap_name = rel_path[at_pos + 1..].to_string();
            if let Some(ref set) = owned
                && !set.contains(&subvol_name)
            {
                continue;
            }
            let parent = info.snapshot_parents.get(&rel_path).cloned();
            all_snapshots.push(Snapshot {
                name: snap_name.clone(),
                subvolume: subvol_name.clone(),
                filesystem: fs_name.to_string(),
                path: snap_path(&mount_point, &subvol_name, &snap_name),
                read_only,
                parent,
                block_device: None,
            });
        }

        Ok(all_snapshots)
    }

    /// Clone a snapshot into a new writable subvolume.
    /// `owner_filter`: if Some, verifies the caller owns the parent subvolume.
    pub async fn clone_snapshot(
        &self,
        req: CloneSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        if req.new_name.contains('@') {
            return Err(SubvolumeError::CommandFailed(
                "subvolume name may not contain '@'".to_string(),
            ));
        }

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let snap_path = snap_path(&mount_point, &req.subvolume, &req.snapshot);
        let new_subvol_path = subvol_path(&mount_point, &req.new_name);

        if !Path::new(&snap_path).exists() {
            return Err(SubvolumeError::NotFound(format!(
                "snapshot {}@{}",
                req.subvolume, req.snapshot
            )));
        }
        if Path::new(&new_subvol_path).exists() {
            info!(
                "Subvolume '{}' already exists in filesystem '{}', returning existing (idempotent)",
                req.new_name, req.filesystem
            );
            return self.get(&req.filesystem, &req.new_name, None).await;
        }

        info!(
            "Cloning snapshot '{}/{}@{}' to new subvolume '{}'",
            req.filesystem, req.subvolume, req.snapshot, req.new_name
        );
        materialize_subvol_from_snapshot(&snap_path, &new_subvol_path, owner_filter).await?;

        self.get(&req.filesystem, &req.new_name, None).await
    }

    /// Roll a filesystem subvolume back to a snapshot's state. Assumes the
    /// engine layer has already quiesced everything using the subvolume.
    ///
    /// Takes a read-only safety snapshot of the current state first (the
    /// undo point — it survives the delete), then deletes the live
    /// subvolume and recreates it writable from the target snapshot at the
    /// same path, re-applying the project quota. Name/path/owner/quota are
    /// preserved, so path-based consumers keep working.
    ///
    /// v1 scope: filesystem subvolumes with no nested children; block
    /// subvolumes (loop/iSCSI/NVMe-oF/VM-disk) are refused.
    pub async fn rollback(
        &self,
        req: RollbackSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<RollbackResult, SubvolumeError> {
        let subvol = self
            .get(&req.filesystem, &req.subvolume, owner_filter)
            .await?;

        if subvol.subvolume_type == SubvolumeType::Block {
            return Err(SubvolumeError::CommandFailed(
                "rollback is not yet supported for block subvolumes (iSCSI / NVMe-oF / VM disks)"
                    .into(),
            ));
        }

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let subvol_path = subvol_path(&mount_point, &req.subvolume);
        let snap_path = snap_path(&mount_point, &req.subvolume, &req.snapshot);

        if !Path::new(&snap_path).exists() {
            return Err(SubvolumeError::NotFound(format!(
                "snapshot {}@{}",
                req.subvolume, req.snapshot
            )));
        }
        let children = find_child_subvolumes(&mount_point, &req.subvolume).await;
        if !children.is_empty() {
            return Err(SubvolumeError::CommandFailed(format!(
                "rollback refused: '{}' has nested subvolumes ({}). Roll those back individually.",
                req.subvolume,
                children.join(", ")
            )));
        }

        // 1. Safety snapshot of the current state — the undo point. It's an
        //    independent subvolume, so it survives the delete below.
        let safety_name = format!(
            "pre-rollback-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        self.create_snapshot(
            CreateSnapshotRequest {
                filesystem: req.filesystem.clone(),
                subvolume: req.subvolume.clone(),
                name: safety_name.clone(),
                read_only: Some(true),
            },
            owner_filter,
        )
        .await?;

        // 2. Delete the live subvolume.
        info!(
            "Rollback: deleting live subvolume '{}/{}' before recreating from @{}",
            req.filesystem, req.subvolume, req.snapshot
        );
        cmd::run_ok("bcachefs", &["subvolume", "delete", &subvol_path])
            .await
            .map_err(|e| {
                SubvolumeError::CommandFailed(format!(
                    "rollback: deleting live subvolume failed ({e}). Your data is intact in \
                     safety snapshot '{}@{safety_name}'.",
                    req.subvolume
                ))
            })?;

        // 3. Recreate writable from the target snapshot at the same path.
        materialize_subvol_from_snapshot(&snap_path, &subvol_path, owner_filter)
            .await
            .map_err(|e| {
                SubvolumeError::CommandFailed(format!(
                    "rollback: recreating from snapshot failed ({e}). Recover from safety \
                     snapshot '{}@{safety_name}' or the target '@{}'.",
                    req.subvolume, req.snapshot
                ))
            })?;

        // 4. Re-apply the project quota — project id is deterministic from
        //    (fs, name), so the recreated subvolume reuses it; re-stamp the
        //    original limit (0 = tracked-unlimited).
        let projid = project_id_for(&req.filesystem, &req.subvolume);
        set_project_quota(
            &mount_point,
            &subvol_path,
            projid,
            subvol.quota_bytes.unwrap_or(0),
        )
        .await;

        let recreated = self.get(&req.filesystem, &req.subvolume, None).await?;
        Ok(RollbackResult {
            subvolume: recreated,
            safety_snapshot: safety_name,
        })
    }

    /// Clone a subvolume into a new writable subvolume (COW).
    /// Uses `bcachefs subvolume snapshot` without `-r`, creating a writable
    /// snapshot that shares data blocks with the source via COW — O(1) and
    /// the most natural clone primitive in bcachefs.
    pub async fn clone_subvolume(
        &self,
        req: CloneSubvolumeRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        if req.new_name.contains('@') {
            return Err(SubvolumeError::CommandFailed(
                "subvolume name may not contain '@'".to_string(),
            ));
        }

        let parent = self.get(&req.filesystem, &req.name, owner_filter).await?;

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let source_path = subvol_path(&mount_point, &req.name);
        let new_subvol_path = subvol_path(&mount_point, &req.new_name);

        if !Path::new(&source_path).exists() {
            return Err(SubvolumeError::NotFound(req.name.clone()));
        }
        if Path::new(&new_subvol_path).exists() {
            return Err(SubvolumeError::AlreadyExists(req.new_name.clone()));
        }

        // For block subvolumes, flush pending I/O before cloning
        if parent.subvolume_type == SubvolumeType::Block
            && let Some(ref loop_dev) = parent.block_device
        {
            info!("Flushing block device {} before clone", loop_dev);
            if let Err(e) = cmd::run_ok("blockdev", &["--flushbufs", loop_dev]).await {
                warn!("Failed to flush {loop_dev} before clone, proceeding anyway: {e}");
            }
        }

        info!(
            "Cloning subvolume '{}/{}' to new subvolume '{}'",
            req.filesystem, req.name, req.new_name
        );
        // Writable snapshot = COW clone
        cmd::run_ok(
            "bcachefs",
            &["subvolume", "snapshot", &source_path, &new_subvol_path],
        )
        .await
        .map_err(SubvolumeError::CommandFailed)?;

        write_meta_xattrs(
            &new_subvol_path,
            &parent.subvolume_type,
            parent.volsize_bytes,
            parent.compression.as_deref(),
            None,
            owner_filter,
            parent.direct_io,
        )?;

        // For block subvolumes, attach a loop device to the clone's sparse image
        // so it's immediately usable as an independent block device.
        if parent.subvolume_type == SubvolumeType::Block {
            let img_path = format!("{new_subvol_path}/{BLOCK_FILE_NAME}");
            if Path::new(&img_path).exists() {
                info!(
                    "Attaching loop device for cloned block subvolume '{}'",
                    req.new_name
                );
                let mut args = vec!["--find", "--show"];
                if parent.direct_io {
                    args.push("--direct-io=on");
                }
                args.push(&img_path);
                cmd::run_ok("losetup", &args)
                    .await
                    .map_err(SubvolumeError::CommandFailed)?;
            }
        }

        self.get(&req.filesystem, &req.new_name, None).await
    }

    /// Set (merge-upsert) xattr properties on a subvolume.
    pub async fn set_properties(
        &self,
        req: SetPropertiesRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;

        for (key, value) in &req.properties {
            let xattr_name = format!("{XATTR_NS}{key}");
            xattr::set(&subvol.path, &xattr_name, value.as_bytes()).map_err(|e| {
                SubvolumeError::CommandFailed(format!("setxattr {xattr_name}: {e}"))
            })?;
        }

        self.get(&req.filesystem, &req.name, owner_filter).await
    }

    /// Remove specific xattr properties from a subvolume.
    pub async fn remove_properties(
        &self,
        req: RemovePropertiesRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;

        for key in &req.keys {
            let xattr_name = format!("{XATTR_NS}{key}");
            match xattr::remove(&subvol.path, &xattr_name) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(SubvolumeError::CommandFailed(format!(
                        "removexattr {xattr_name}: {e}"
                    )));
                }
            }
        }

        self.get(&req.filesystem, &req.name, owner_filter).await
    }

    /// Find subvolumes where the given property key equals the given value.
    /// Optionally restricted to a single filesystem.
    pub async fn find_by_property(
        &self,
        req: FindByPropertyRequest,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Subvolume>, SubvolumeError> {
        let all = self
            .list_all(req.filesystem.as_deref(), owner_filter)
            .await?;
        Ok(all
            .into_iter()
            .filter(|s| {
                s.properties
                    .get(&req.key)
                    .map(|v| v == &req.value)
                    .unwrap_or(false)
            })
            .collect())
    }
}

/// Parsed result from `bcachefs subvolume list --snapshots --json`.
struct BcachefsInfo {
    /// Relative paths of non-snapshot subvolumes (e.g. "foo").
    subvol_paths: std::collections::HashSet<String>,
    /// Relative path of each snapshot → read_only flag (e.g. "foo@snap" → true).
    snapshot_flags: std::collections::HashMap<String, bool>,
    /// Relative path of each snapshot → parent path (from bcachefs snapshot_parent).
    snapshot_parents: std::collections::HashMap<String, String>,
}

/// Run `bcachefs subvolume list --snapshots --json <mount_point>` once and
/// return both the subvolume paths and per-snapshot read_only flags.
/// On any error returns empty collections so callers degrade gracefully.
async fn bcachefs_list_all(mount_point: &str) -> BcachefsInfo {
    #[derive(serde::Deserialize)]
    struct Entry {
        path: String,
        #[serde(default)]
        flags: Option<String>,
        snapshot_parent: Option<String>,
    }

    let output = cmd::run_ok(
        "bcachefs",
        &["subvolume", "list", "--snapshots", "--json", mount_point],
    )
    .await
    .unwrap_or_default();

    let entries: Vec<Entry> = serde_json::from_str(&output).unwrap_or_default();

    let mut subvol_paths = std::collections::HashSet::new();
    let mut snapshot_flags = std::collections::HashMap::new();
    let mut snapshot_parents = std::collections::HashMap::new();

    for entry in entries {
        let is_ro = entry.flags.as_deref() == Some("ro");
        if let Some(ref parent) = entry.snapshot_parent {
            if is_ro {
                // Read-only snapshot
                snapshot_flags.insert(entry.path.clone(), true);
            } else {
                // Writable clone — treat as regular subvolume
                subvol_paths.insert(entry.path.clone());
            }
            // Track parent for all snapshots/clones
            // snapshot_parent comes as "/parent-name", strip the leading "/"
            let parent_name = parent.strip_prefix('/').unwrap_or(parent).to_string();
            snapshot_parents.insert(entry.path, parent_name);
        } else {
            subvol_paths.insert(entry.path);
        }
    }

    BcachefsInfo {
        subvol_paths,
        snapshot_flags,
        snapshot_parents,
    }
}

/// Derive a stable 32-bit project ID from filesystem + subvolume name.
/// Zero is reserved by the kernel so we ensure the result is ≥ 1.
fn project_id_for(filesystem: &str, name: &str) -> u32 {
    let mut h = DefaultHasher::new();
    filesystem.hash(&mut h);
    name.hash(&mut h);
    let v = (h.finish() & 0xFFFF_FFFF) as u32;
    v.max(1)
}

/// Assign a bcachefs project ID to a subvolume directory and set its quota limit.
///
/// Uses `setproject` (from Kent Overstreet's linuxquota fork) to assign the
/// project ID, then `setquota` to set the hard block limit. Both tools must be
/// present on the system (provided via nixos/modules/linuxquota.nix).
///
/// Best-effort: logs a warning on failure rather than returning an error, since
/// quota enforcement requires `prjquota` mount option. Volume creation must not
/// fail if quota tools are unavailable.
async fn set_project_quota(mount_point: &str, dir_path: &str, projid: u32, bytes: u64) {
    // Register the project name in /etc/projid so that standard quota tools
    // (repquota, edquota) can display human-readable names.
    let proj_name = format!("nasty-{projid}");
    register_project(&proj_name, projid);

    // setproject -c -P <name> <path>
    // -c: create the project in /etc/projid if not present (idempotent)
    match cmd::run_ok("setproject", &["-c", "-P", &proj_name, dir_path]).await {
        Ok(_) => info!("set project {proj_name} (id={projid}) on {dir_path}"),
        Err(e) => {
            warn!("setproject failed on {dir_path}: {e}");
            return;
        }
    }

    // setquota -P <name> <soft> <hard> <isoft> <ihard> <mountpoint>
    // soft == hard (no grace period), no inode limits.
    //
    // setquota's block-limit arguments are in 1 KiB blocks, not bytes —
    // passing bytes directly stored every quota 1024× larger than
    // intended (e.g. a 5 GiB PVC ended up as 5 TiB on disk).  The read
    // side already converts KiB→bytes (`hard_kb * 1024` in
    // query_project_usages), so the round-trip was internally
    // consistent but consistently wrong.  Integer-divide here: typical
    // PVC sizes are exact Gi multiples and align cleanly to KiB.
    let kib_str = (bytes / 1024).to_string();
    match cmd::run_ok(
        "setquota",
        &["-P", &proj_name, &kib_str, &kib_str, "0", "0", mount_point],
    )
    .await
    {
        Ok(_) => info!("set quota {bytes} bytes for project {proj_name} on {mount_point}"),
        Err(e) => warn!("setquota failed for project {proj_name} on {mount_point}: {e}"),
    }
}

/// Write a `name:id` entry to /etc/projid if not already present.
/// This allows standard quota tools to resolve project IDs to names.
fn register_project(name: &str, projid: u32) {
    let entry = format!("{name}:{projid}\n");
    let path = "/etc/projid";

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // Check by both name and ID to avoid duplicates
    let name_prefix = format!("{name}:");
    let id_suffix = format!(":{projid}");
    if existing
        .lines()
        .any(|l| l.starts_with(&name_prefix) || l.ends_with(&id_suffix))
    {
        return;
    }
    if let Err(e) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(entry.as_bytes())
        })
    {
        warn!("register_project: could not write to {path}: {e}");
    }
}

/// Remove a project entry from /etc/projid on subvolume deletion.
fn unregister_project(projid: u32) {
    let path = "/etc/projid";
    let id_suffix = format!(":{projid}");
    let Ok(existing) = std::fs::read_to_string(path) else {
        return;
    };
    let filtered: String = existing
        .lines()
        .filter(|l| !l.ends_with(&id_suffix))
        .map(|l| format!("{l}\n"))
        .collect();
    if let Err(e) = std::fs::write(path, filtered) {
        warn!("unregister_project: could not write to {path}: {e}");
    }
}

/// Get disk usage for a block subvolume by statting the sparse image file directly.
/// Much faster than `du -sb` since it's a single syscall instead of a tree walk.
/// Actual on-disk allocation for a block subvolume's image, in bytes.
///
/// We use `st_blocks * 512` (allocated blocks) rather than `m.len()` (logical
/// size) so sparse images report what's truly written. Since the image is
/// created with `truncate -s <volsize>`, `len()` always equals volsize and
/// would make the progress bar a constant 100%.
fn block_image_size(subvol_path: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let img_path = format!("{subvol_path}/{BLOCK_FILE_NAME}");
    std::fs::metadata(&img_path).ok().map(|m| m.blocks() * 512)
}

/// Per-project quota state extracted from `repquota` — both the live
/// usage and the hard limit. `hard_bytes == 0` is the quota-tools
/// convention for "no limit".
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProjectQuotaInfo {
    pub used_bytes: u64,
    pub hard_bytes: u64,
}

/// Query project quota usage for all projects on a filesystem in one shot.
/// Returns a map of project_id → (used, hard limit).
/// Falls back to empty map if repquota is unavailable or fails.
async fn query_project_usages(mount_point: &str) -> HashMap<u32, ProjectQuotaInfo> {
    let mut usages = HashMap::new();

    // repquota -P --raw-grace outputs KB values with numeric project IDs when using -n
    let output = match cmd::run_ok("repquota", &["-P", "--raw-grace", "-n", mount_point]).await {
        Ok(o) => o,
        Err(e) => {
            warn!("repquota failed on {mount_point}: {e}");
            return usages;
        }
    };

    // Parse lines like: "#1689127545 -- 125486988 1073741824000 1073741824000 0 78 0 0 0"
    // Fields: project_name status used_kb soft_kb hard_kb grace files_used files_soft files_hard files_grace
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with('#') {
            continue;
        }
        let id_str = &line[1..]; // strip '#'
        let mut parts = id_str.split_whitespace();
        let projid: u32 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(id) if id > 0 => id,
            _ => continue,
        };
        let _status = parts.next(); // "--" or "+-" etc
        let used_kb: u64 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let _soft_kb = parts.next();
        let hard_kb: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        usages.insert(
            projid,
            ProjectQuotaInfo {
                used_bytes: used_kb * 1024,
                hard_bytes: hard_kb * 1024,
            },
        );
    }

    usages
}

/// Build a map of canonical backing-file path → loop device name.
/// Called once per `list()` invocation instead of per-subvolume.
async fn build_losetup_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let output = match cmd::run_ok(
        "losetup",
        &["--list", "--output", "NAME,BACK-FILE", "--noheadings"],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            // Empty loop-device map means the caller (subvolume
            // enumeration) can't resolve block-subvolume backing
            // files to /dev/loopN entries. The WebUI sees those
            // subvolumes as detached even when they aren't.
            warn!("losetup failed: {e}; loop-device map will be empty");
            return map;
        }
    };

    for line in output.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(dev), Some(back)) = (parts.next(), parts.next()) {
            map.insert(back.to_string(), dev.to_string());
        }
    }
    map
}

/// Find the loop device attached to a given file (single-call variant for non-list paths).
///
/// bcachefs COW clones preserve inode numbers, so `losetup -j` (which matches
/// by device+inode) incorrectly returns the original subvolume's loop device
/// when called on a clone's vol.img. We instead parse `losetup --list` output
/// and match by the exact canonical file path to avoid this false-positive.
async fn find_loop_device(file_path: &str) -> Option<String> {
    let map = build_losetup_map().await;
    find_loop_device_from_map(&map, file_path)
}

/// Look up the loop device for a given file path using a pre-built map.
fn find_loop_device_from_map(
    losetup_map: &HashMap<String, String>,
    file_path: &str,
) -> Option<String> {
    // Canonicalize the target path so symlinks / relative paths don't matter
    let canonical = std::fs::canonicalize(file_path).ok()?;
    let canonical_str = canonical.to_string_lossy().to_string();
    losetup_map.get(&canonical_str).cloned()
}

/// Find all child subvolumes under a given parent path using `bcachefs subvolume list -R`.
/// Returns paths relative to the mount point, sorted so deepest children come last
/// (caller should reverse for depth-first deletion).
async fn find_child_subvolumes(mount_point: &str, parent_name: &str) -> Vec<String> {
    let output = match cmd::run_ok("bcachefs", &["subvolume", "list", "-R", mount_point]).await {
        Ok(o) => o,
        Err(e) => {
            // Empty children list means the caller (subvolume delete
            // path) won't see nested subvolumes that need to be
            // removed first — the outer delete then fails with a
            // less actionable error. Surface the real cause here.
            warn!(
                "bcachefs subvolume list -R {mount_point} failed: {e}; \
                 children of {parent_name} will not be discovered"
            );
            return Vec::new();
        }
    };

    let prefix = format!("{parent_name}/");
    let mut children = Vec::new();
    for line in output.lines() {
        // Format: "Path                     ID       Created          Flags        Size"
        // Each line starts with the relative path, then whitespace-separated fields.
        let path = line.split_whitespace().next().unwrap_or("");
        if path.starts_with(&prefix) && path != parent_name {
            children.push(path.to_string());
        }
    }
    // Sort so deeper paths come later — reverse for depth-first deletion
    children.sort();
    children
}

/// Recreate a writable subvolume at `dest_path` from a snapshot at
/// `snap_path` (`bcachefs subvolume snapshot` without `-r`), re-stamping
/// the owner xattr when a scope filter is active and re-attaching a loop
/// device for block subvolumes. bcachefs copies the source's other xattrs
/// into the new subvolume automatically. Shared by `clone_snapshot` and
/// `rollback`.
async fn materialize_subvol_from_snapshot(
    snap_path: &str,
    dest_path: &str,
    owner_filter: Option<&str>,
) -> Result<(), SubvolumeError> {
    cmd::run_ok("bcachefs", &["subvolume", "snapshot", snap_path, dest_path])
        .await
        .map_err(SubvolumeError::CommandFailed)?;

    let snap_meta = read_meta_xattrs(Path::new(snap_path));
    if let Some(owner) = owner_filter {
        let _ = xattr::set(dest_path, XATTR_NASTY_OWNER, owner.as_bytes());
    }

    if snap_meta.subvolume_type == SubvolumeType::Block {
        let img_path = format!("{dest_path}/{BLOCK_FILE_NAME}");
        if Path::new(&img_path).exists() {
            info!("Attaching loop device for block subvolume at '{dest_path}'");
            let mut args = vec!["--find", "--show"];
            if snap_meta.direct_io {
                args.push("--direct-io=on");
            }
            args.push(&img_path);
            cmd::run_ok("losetup", &args)
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_subvolume_name_accepts_typical_names() {
        assert!(validate_subvolume_name("tank").is_ok());
        assert!(validate_subvolume_name("My-Docs_2024").is_ok());
        assert!(validate_subvolume_name("projects/web").is_ok());
        assert!(validate_subvolume_name("a/b/c/d").is_ok());
        assert!(validate_subvolume_name("name.with.dots").is_ok());
    }

    #[test]
    fn validate_subvolume_name_rejects_traversal() {
        assert!(validate_subvolume_name("..").is_err());
        assert!(validate_subvolume_name("../etc").is_err());
        assert!(validate_subvolume_name("ok/../bad").is_err());
        assert!(validate_subvolume_name("ok/.").is_err());
    }

    #[test]
    fn validate_subvolume_name_rejects_leading_separators() {
        assert!(validate_subvolume_name("/etc/passwd").is_err());
        assert!(validate_subvolume_name("//doubled").is_err());
        // Empty path components from a trailing or doubled `/` are flagged
        // because the resulting `format!("{mount}/{name}")` would silently
        // collapse them and confuse later listing/parse code.
        assert!(validate_subvolume_name("a//b").is_err());
    }

    #[test]
    fn validate_subvolume_name_rejects_hidden_prefix() {
        // Leading dot creates a directory that the WebUI doesn't surface;
        // the operator has no UI path to clean it up afterwards.
        assert!(validate_subvolume_name(".hidden").is_err());
    }

    #[test]
    fn validate_subvolume_name_rejects_snapshot_collision() {
        // `@` is bcachefs's snapshot separator (`subvol@snapname`).
        // Allowing it in the subvolume name would corrupt snapshot
        // lookups in find_child_subvolumes() and snap_path().
        assert!(validate_subvolume_name("subvol@snap").is_err());
        assert!(validate_subvolume_name("nested/@evil").is_err());
    }

    #[test]
    fn validate_subvolume_name_rejects_control_chars() {
        assert!(validate_subvolume_name("name\nwith\nnewline").is_err());
        assert!(validate_subvolume_name("name\twith\ttab").is_err());
        assert!(validate_subvolume_name("name\x00with\x00null").is_err());
    }

    #[test]
    fn validate_subvolume_name_rejects_empty_and_oversize() {
        assert!(validate_subvolume_name("").is_err());
        assert!(validate_subvolume_name(&"x".repeat(201)).is_err());
        // Boundary is inclusive: 200 chars is accepted.
        assert!(validate_subvolume_name(&"x".repeat(200)).is_ok());
    }

    #[test]
    fn validate_volsize_bytes_accepts_sensible_sizes() {
        assert!(validate_volsize_bytes(1024).is_ok());
        assert!(validate_volsize_bytes(1024 * 1024 * 1024).is_ok()); // 1 GiB
        assert!(validate_volsize_bytes(1024_u64.pow(4)).is_ok()); // 1 TiB
        assert!(validate_volsize_bytes(MAX_VOLSIZE_BYTES).is_ok()); // boundary
    }

    #[test]
    fn validate_volsize_bytes_rejects_zero() {
        // A zero-byte block subvolume would create a useless backing file
        // and is almost always a typo or a UI-bind glitch.
        assert!(validate_volsize_bytes(0).is_err());
    }

    #[test]
    fn validate_volsize_bytes_rejects_above_cap() {
        // 257 TiB > 256 TiB cap. A request of u64::MAX is the canonical
        // worst case (Operator-role caller trying to ENOSPC the filesystem).
        assert!(validate_volsize_bytes(MAX_VOLSIZE_BYTES + 1).is_err());
        assert!(validate_volsize_bytes(u64::MAX).is_err());
    }

    #[test]
    fn is_already_detached_matches_real_losetup_errors() {
        // Strings observed in the wild from util-linux losetup -d on a
        // device that isn't currently associated with a backing file.
        // Capitalization varies by version.
        assert!(is_already_detached(
            "losetup exited with exit code: 1: losetup: /dev/loop3: detach failed: No such device or address"
        ));
        assert!(is_already_detached(
            "losetup: /dev/loop0: detach failed: no such device"
        ));
        assert!(is_already_detached(
            "losetup: cannot find device /dev/loop9: loop device not in use"
        ));
    }

    #[test]
    fn is_already_detached_rejects_real_failures() {
        // Device is still in use by something — operator needs to see
        // this so they can find the process holding it open.
        assert!(!is_already_detached(
            "losetup exited with exit code: 1: losetup: /dev/loop3: detach failed: Device or resource busy"
        ));
        // Permission denied — a real configuration problem the
        // operator needs to act on, not an idempotent no-op.
        assert!(!is_already_detached(
            "losetup: /dev/loop3: detach failed: Operation not permitted"
        ));
        // Unrelated tool failure — log unchanged.
        assert!(!is_already_detached("failed to spawn losetup: ENOENT"));
    }

    #[test]
    fn subvolume_error_display_lists_stuck_children() {
        // The ChildrenStuck variant is operator-facing: the operator
        // sees this message in a toast in the WebUI and uses it to
        // figure out which subvolumes are blocking the parent delete.
        // Format must surface the names; pin it here so a future
        // accidental message change shows up as a test failure.
        let err = SubvolumeError::ChildrenStuck(
            "projects/web (busy); projects/api (mounted)".to_string(),
        );
        let msg = format!("{err}");
        assert!(msg.contains("projects/web"));
        assert!(msg.contains("projects/api"));
        assert!(msg.contains("busy"));
        assert!(msg.contains("mounted"));
    }

    #[test]
    fn subvolume_error_display_names_loop_device() {
        // Same operator-facing constraint as ChildrenStuck — they need
        // to know *which* device is stuck to run lsof / fuser / debug.
        let err = SubvolumeError::LoopDetachFailed {
            device: "/dev/loop7".to_string(),
            reason: "Device or resource busy".to_string(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("/dev/loop7"));
        assert!(msg.contains("Device or resource busy"));
    }
}
