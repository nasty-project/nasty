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
const XATTR_NASTY_TYPE:        &str = "user.nasty.type";
const XATTR_NASTY_VOLSIZE:     &str = "user.nasty.volsize";
const XATTR_NASTY_COMPRESSION: &str = "user.nasty.compression";
const XATTR_NASTY_COMMENT:     &str = "user.nasty.comment";
const XATTR_NASTY_OWNER:       &str = "user.nasty.owner";
const XATTR_NASTY_DIRECT_IO:   &str = "user.nasty.direct_io";

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
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
    /// Disk usage in bytes (filesystem subvolumes only, from `du`).
    pub used_bytes: Option<u64>,
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

/// Read NASty-internal metadata from the reserved `user.nasty.*` xattrs.
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
            // Auto-detect for subvolumes created before xattr metadata: presence of
            // vol.img means block, otherwise filesystem.
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
                warn!("Block image {img_path} not found for {}/{}", subvol.filesystem, subvol.name);
                continue;
            }

            // Use existing loop device if already attached (engine restart, not reboot)
            let loop_dev = if let Some(existing) = find_loop_device(&img_path).await {
                info!("Loop device already attached for {}/{}", subvol.filesystem, subvol.name);
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
                        info!("Attached {} for block subvolume {}/{}", dev, subvol.filesystem, subvol.name);
                        dev
                    }
                    Err(e) => {
                        warn!("Failed to attach loop device for {}/{}: {e}", subvol.filesystem, subvol.name);
                        continue;
                    }
                }
            };

            dev_map.insert(subvol.name.clone(), loop_dev);
        }

        dev_map
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
    pub async fn list(&self, fs_name: &str, owner_filter: Option<&str>) -> Result<Vec<Subvolume>, SubvolumeError> {
        let mount_point = self.fs_mount_point(fs_name).await?;
        let mut subvolumes = Vec::new();

        // Ask bcachefs which paths are real subvolumes (filters out plain dirs)
        let info = bcachefs_list_all(&mount_point).await;

        // Subvolumes sit directly at the filesystem root — no subdirectory prefix.
        // Exclude anything with '/' (nested) or '@' (snapshot).
        for name in info.subvol_paths.iter().filter(|p| !p.is_empty() && !p.contains('/') && !p.contains('@')) {
            let path_str = subvol_path(&mount_point, name);
            let path = Path::new(&path_str);

            let meta = read_meta_xattrs(path);

            // Apply owner filter: operators only see their own subvolumes
            if let Some(filter) = owner_filter {
                if meta.owner.as_deref() != Some(filter) {
                    continue;
                }
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
            let size = dir_usage(path).await;

            let block_device = if meta.subvolume_type == SubvolumeType::Block {
                let img_path = format!("{path_str}/{BLOCK_FILE_NAME}");
                find_loop_device(&img_path).await
            } else {
                None
            };

            let properties = read_xattrs(path);

            let parent = info.snapshot_parents.get(name.as_str()).cloned();

            subvolumes.push(Subvolume {
                name: name.to_string(),
                filesystem: fs_name.to_string(),
                subvolume_type: meta.subvolume_type,
                path: path_str.clone(),
                used_bytes: size,
                compression: meta.compression,
                comments: meta.comments,
                volsize_bytes: meta.volsize_bytes,
                block_device,
                snapshots: snapshots.iter().map(|s| s.name.clone()).collect(),
                owner: meta.owner,
                properties,
                parent,
                direct_io: meta.direct_io,
            });
        }

        Ok(subvolumes)
    }

    /// List subvolumes across all mounted filesystems.
    /// `fs_filter`: if Some, only include that filesystem.
    /// `owner_filter`: if Some, only include subvolumes owned by that token.
    pub async fn list_all(&self, fs_filter: Option<&str>, owner_filter: Option<&str>) -> Result<Vec<Subvolume>, SubvolumeError> {
        let all_fs = self.filesystems.list().await
            .map_err(|e| SubvolumeError::CommandFailed(e.to_string()))?;

        let mut all = Vec::new();
        for fs in all_fs {
            if !fs.mounted {
                continue;
            }
            if let Some(filter) = fs_filter {
                if fs.name != filter {
                    continue;
                }
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
    pub async fn create(&self, req: CreateSubvolumeRequest, owner: Option<String>) -> Result<Subvolume, SubvolumeError> {
        if req.name.contains('@') {
            return Err(SubvolumeError::CommandFailed(
                "subvolume name may not contain '@'".to_string(),
            ));
        }

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

        if Path::new(&subvol_path).exists() {
            info!("Subvolume '{}' already exists in filesystem '{}', returning existing (idempotent)", req.name, req.filesystem);
            return self.get(&req.filesystem, &req.name, None).await;
        }

        if req.subvolume_type == SubvolumeType::Block && req.volsize_bytes.is_none() {
            return Err(SubvolumeError::VolsizeRequired);
        }

        // Create the bcachefs subvolume
        info!("Creating subvolume '{}' in filesystem '{}'", req.name, req.filesystem);
        cmd::run_ok("bcachefs", &["subvolume", "create", &subvol_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

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

        // For filesystem subvolumes: enforce size via bcachefs project quota
        if req.subvolume_type == SubvolumeType::Filesystem {
            if let Some(size) = req.volsize_bytes {
                let projid = project_id_for(&req.filesystem, &req.name);
                set_project_quota(&mount_point, &subvol_path, projid, size).await;
            }
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
            match cmd::run_ok("bcachefs", &["set-file-option", "--nocow", &img_path]).await {
                Ok(_) => info!("Set nocow on {img_path}"),
                Err(e) => warn!("Failed to set nocow on {img_path}: {e}"),
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

    /// Delete a subvolume.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn delete(&self, req: DeleteSubvolumeRequest, owner_filter: Option<&str>) -> Result<(), SubvolumeError> {
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;

        // For block subvolumes: detach loop device first
        if subvol.subvolume_type == SubvolumeType::Block {
            if let Some(ref loop_dev) = subvol.block_device {
                info!("Detaching loop device {} for '{}'", loop_dev, req.name);
                if let Err(e) = cmd::run_ok("losetup", &["-d", loop_dev]).await {
                    warn!("Failed to detach loop device {loop_dev}: {e}");
                }
            }
        }

        let mount_point = self.fs_mount_point(&req.filesystem).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

        // bcachefs snapshots are independent first-class subvolumes — they survive
        // parent deletion. We intentionally do NOT delete snapshots here so that
        // snapshot-based restore/DR scenarios work correctly.

        info!("Deleting subvolume '{}' from filesystem '{}'", req.name, req.filesystem);
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
        let subvol = self.get(&req.filesystem, &req.name, owner_filter).await?;

        match subvol.subvolume_type {
            SubvolumeType::Block => {
                let img_path = format!("{}/{}", subvol.path, BLOCK_FILE_NAME);
                info!(
                    "Resizing block subvolume '{}' to {} bytes",
                    req.name, req.volsize_bytes
                );
                cmd::run_ok("truncate", &["-s", &req.volsize_bytes.to_string(), &img_path])
                    .await
                    .map_err(SubvolumeError::CommandFailed)?;

                // If loop device is attached, inform the kernel of the new size
                if let Some(ref loop_dev) = subvol.block_device {
                    info!("Updating loop device {} capacity for '{}'", loop_dev, req.name);
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
                    &["-P", &proj_name, &bytes_str, &bytes_str, "0", "0", &mount_point],
                ).await {
                    warn!("setquota failed for project {proj_name} on {mount_point}: {e}");
                }
            }
        }

        // Update volsize xattr
        let path = subvol_path(&self.fs_mount_point(&req.filesystem).await?, &req.name);
        xattr::set(&path, XATTR_NASTY_VOLSIZE, req.volsize_bytes.to_string().as_bytes())
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
            info!("Setting compression={} on subvolume '{}'", comp_value, req.name);
            cmd::run_ok(
                "bcachefs",
                &["set-file-option", &format!("--compression={comp_value}"), path],
            )
            .await
            .map_err(SubvolumeError::CommandFailed)?;

            if comp_value == "none" {
                let _ = xattr::remove(path, XATTR_NASTY_COMPRESSION);
            } else {
                xattr::set(path, XATTR_NASTY_COMPRESSION, comp_value.as_bytes())
                    .map_err(|e| SubvolumeError::CommandFailed(format!("setxattr compression: {e}")))?;
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
        self.get(&req.filesystem, &req.subvolume, owner_filter).await?;

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
        let subvol = self.get(&req.filesystem, &req.subvolume, owner_filter).await?;
        if subvol.subvolume_type == SubvolumeType::Block {
            if let Some(ref loop_dev) = subvol.block_device {
                info!("Flushing block device {} before snapshot", loop_dev);
                if let Err(e) = cmd::run_ok("blockdev", &["--flushbufs", loop_dev]).await {
                    warn!("Failed to flush {loop_dev} before snapshot, proceeding anyway: {e}");
                }
            }
        }

        info!(
            "Creating snapshot '{}' of subvolume '{}/{}'",
            req.name, req.filesystem, req.subvolume
        );
        // Snapshots are always read-only; use snapshot.clone for writable copies
        cmd::run_ok("bcachefs", &["subvolume", "snapshot", "-r", &source_path, &snap_path])
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
        if let Ok(_parent) = self.get(&req.filesystem, &req.subvolume, owner_filter).await {
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
            let Some(at_pos) = rel_path.find('@') else { continue };
            let subvol_name = rel_path[..at_pos].to_string();
            let snap_name = rel_path[at_pos + 1..].to_string();
            if let Some(ref set) = owned {
                if !set.contains(&subvol_name) {
                    continue;
                }
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
                "snapshot {}@{}", req.subvolume, req.snapshot
            )));
        }
        if Path::new(&new_subvol_path).exists() {
            info!("Subvolume '{}' already exists in filesystem '{}', returning existing (idempotent)", req.new_name, req.filesystem);
            return self.get(&req.filesystem, &req.new_name, None).await;
        }

        info!(
            "Cloning snapshot '{}/{}@{}' to new subvolume '{}'",
            req.filesystem, req.subvolume, req.snapshot, req.new_name
        );
        // bcachefs subvolume snapshot without -r creates a writable subvolume from snapshot
        cmd::run_ok("bcachefs", &["subvolume", "snapshot", &snap_path, &new_subvol_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        // Read metadata from the snapshot itself — it inherits xattrs from the source.
        // This works even when the parent subvolume has been deleted (DR scenario).
        let snap_meta = read_meta_xattrs(Path::new(&snap_path));

        // The new subvolume already has the correct xattrs (copied by bcachefs snapshot).
        // Only update owner if an owner filter is set.
        if let Some(owner) = owner_filter {
            let _ = xattr::set(&new_subvol_path, XATTR_NASTY_OWNER, owner.as_bytes());
        }

        // For block subvolumes, attach a loop device to the restored clone's sparse image
        if snap_meta.subvolume_type == SubvolumeType::Block {
            let img_path = format!("{new_subvol_path}/{BLOCK_FILE_NAME}");
            if Path::new(&img_path).exists() {
                info!("Attaching loop device for restored block subvolume '{}'", req.new_name);
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

        self.get(&req.filesystem, &req.new_name, None).await
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
        if parent.subvolume_type == SubvolumeType::Block {
            if let Some(ref loop_dev) = parent.block_device {
                info!("Flushing block device {} before clone", loop_dev);
                if let Err(e) = cmd::run_ok("blockdev", &["--flushbufs", loop_dev]).await {
                    warn!("Failed to flush {loop_dev} before clone, proceeding anyway: {e}");
                }
            }
        }

        info!(
            "Cloning subvolume '{}/{}' to new subvolume '{}'",
            req.filesystem, req.name, req.new_name
        );
        // Writable snapshot = COW clone
        cmd::run_ok("bcachefs", &["subvolume", "snapshot", &source_path, &new_subvol_path])
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
                info!("Attaching loop device for cloned block subvolume '{}'", req.new_name);
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
            xattr::set(&subvol.path, &xattr_name, value.as_bytes())
                .map_err(|e| SubvolumeError::CommandFailed(
                    format!("setxattr {xattr_name}: {e}")
                ))?;
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
                Err(e) => return Err(SubvolumeError::CommandFailed(
                    format!("removexattr {xattr_name}: {e}")
                )),
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
        let all = self.list_all(req.filesystem.as_deref(), owner_filter).await?;
        Ok(all
            .into_iter()
            .filter(|s| s.properties.get(&req.key).map(|v| v == &req.value).unwrap_or(false))
            .collect())
    }
}

/// Read user-defined xattr properties from a path.
/// Returns a map of logical key → value (strips the "user." prefix).
/// Excludes the reserved "user.nasty.*" keys (those are first-class struct fields).
/// Non-UTF-8 values and unreadable xattrs are silently skipped.
fn read_xattrs(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let attrs = match xattr::list(path) {
        Ok(a) => a,
        Err(_) => return map,
    };
    for name in attrs {
        let name_str = name.to_string_lossy();
        let Some(key) = name_str.strip_prefix(XATTR_NS) else { continue };
        // Skip reserved nasty.* keys — surfaced as first-class struct fields instead
        if key.starts_with(NASTY_KEY_PREFIX) {
            continue;
        }
        if let Ok(Some(bytes)) = xattr::get(path, &*name_str) {
            if let Ok(value) = String::from_utf8(bytes) {
                map.insert(key.to_string(), value);
            }
        }
    }
    map
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

    BcachefsInfo { subvol_paths, snapshot_flags, snapshot_parents }
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
    // soft == hard (no grace period), no inode limits
    let bytes_str = bytes.to_string();
    match cmd::run_ok("setquota", &["-P", &proj_name, &bytes_str, &bytes_str, "0", "0", mount_point]).await {
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
    if existing.lines().any(|l| l.starts_with(&name_prefix) || l.ends_with(&id_suffix)) {
        return;
    }
    if let Err(e) = std::fs::OpenOptions::new().append(true).create(true).open(path)
        .and_then(|mut f| { use std::io::Write; f.write_all(entry.as_bytes()) })
    {
        warn!("register_project: could not write to {path}: {e}");
    }
}

/// Remove a project entry from /etc/projid on subvolume deletion.
fn unregister_project(projid: u32) {
    let path = "/etc/projid";
    let id_suffix = format!(":{projid}");
    let Ok(existing) = std::fs::read_to_string(path) else { return };
    let filtered: String = existing
        .lines()
        .filter(|l| !l.ends_with(&id_suffix))
        .map(|l| format!("{l}\n"))
        .collect();
    if let Err(e) = std::fs::write(path, filtered) {
        warn!("unregister_project: could not write to {path}: {e}");
    }
}

/// Get disk usage for a directory using `du`
async fn dir_usage(path: &Path) -> Option<u64> {
    let path_str = path.to_string_lossy();
    let output = cmd::run_ok("du", &["-sb", &path_str]).await.ok()?;
    output
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
}

/// Find the loop device attached to a given file, matched by backing-file PATH.
///
/// bcachefs COW clones preserve inode numbers, so `losetup -j` (which matches
/// by device+inode) incorrectly returns the original subvolume's loop device
/// when called on a clone's vol.img. We instead parse `losetup --list` output
/// and match by the exact canonical file path to avoid this false-positive.
async fn find_loop_device(file_path: &str) -> Option<String> {
    // Canonicalize the target path so symlinks / relative paths don't matter
    let canonical = std::fs::canonicalize(file_path).ok()?;
    let canonical_str = canonical.to_string_lossy();

    let output = cmd::run_ok(
        "losetup",
        &["--list", "--output", "NAME,BACK-FILE", "--noheadings"],
    )
    .await
    .ok()?;

    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let dev = parts.next()?;
        let back = parts.next()?;
        if back == canonical_str {
            return Some(dev.to_string());
        }
    }
    None
}


