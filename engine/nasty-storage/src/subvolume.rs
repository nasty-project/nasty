use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::cmd;
use crate::pool::PoolService;

const STATE_DIR: &str = "/var/lib/nasty/subvolumes";
const BLOCK_FILE_NAME: &str = "vol.img";

fn subvol_path(mount_point: &str, name: &str) -> String {
    format!("{mount_point}/{name}")
}

fn snap_path(mount_point: &str, subvol: &str, snap: &str) -> String {
    format!("{mount_point}/{subvol}@{snap}")
}

/// POSIX xattr namespace prefix for all nasty-csi properties.
/// E.g. logical key "nasty-csi:managed_by" → xattr "user.nasty-csi:managed_by".
const XATTR_NS: &str = "user.";

#[derive(Debug, Error)]
pub enum SubvolumeError {
    #[error("pool not found: {0}")]
    PoolNotFound(String),
    #[error("pool not mounted: {0}")]
    PoolNotMounted(String),
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
    /// Subvolume name (unique within the pool).
    pub name: String,
    /// Name of the pool that contains this subvolume.
    pub pool: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// Snapshot name (unique within the parent subvolume).
    pub name: String,
    /// Name of the parent subvolume.
    pub subvolume: String,
    /// Name of the pool that contains this snapshot.
    pub pool: String,
    /// Absolute filesystem path to the snapshot directory.
    pub path: String,
    /// Whether this snapshot is read-only.
    pub read_only: bool,
    /// Loop device path if this snapshot's vol.img is currently attached (block snapshots only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_device: Option<String>,
}

/// Persisted metadata for subvolumes (things bcachefs doesn't track)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubvolumeMeta {
    id: String,
    name: String,
    pool: String,
    subvolume_type: SubvolumeType,
    volsize_bytes: Option<u64>,
    compression: Option<String>,
    comments: Option<String>,
    /// Token name that created this subvolume; None for human-created subvolumes.
    #[serde(default)]
    owner: Option<String>,
}

impl SubvolumeMeta {
    fn make_id(pool: &str, name: &str) -> String {
        format!("{pool}_{name}")
    }
}

impl HasId for SubvolumeMeta {
    fn id(&self) -> &str {
        &self.id
    }
}

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSubvolumeRequest {
    /// Name of the pool to create the subvolume in.
    pub pool: String,
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
}

fn default_type() -> SubvolumeType {
    SubvolumeType::Filesystem
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSubvolumeRequest {
    /// Name of the pool containing the subvolume.
    pub pool: String,
    /// Name of the subvolume to delete.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSnapshotRequest {
    /// Name of the pool containing the subvolume.
    pub pool: String,
    /// Name of the subvolume to snapshot.
    pub subvolume: String,
    /// Name for the new snapshot.
    pub name: String,
    /// Whether to create a read-only snapshot (default: true).
    pub read_only: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSnapshotRequest {
    /// Name of the pool containing the snapshot.
    pub pool: String,
    /// Name of the parent subvolume.
    pub subvolume: String,
    /// Name of the snapshot to delete.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CloneSnapshotRequest {
    /// Name of the pool containing the snapshot.
    pub pool: String,
    /// Name of the parent subvolume.
    pub subvolume: String,
    /// Name of the snapshot to clone.
    pub snapshot: String,
    /// Name for the new writable subvolume created from the snapshot.
    pub new_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResizeSubvolumeRequest {
    /// Name of the pool containing the subvolume.
    pub pool: String,
    /// Name of the subvolume to resize.
    pub name: String,
    /// New size in bytes. For block subvolumes: sparse image size. For filesystem subvolumes: bcachefs quota limit.
    pub volsize_bytes: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetPropertiesRequest {
    /// Name of the pool containing the subvolume.
    pub pool: String,
    /// Name of the subvolume to update.
    pub name: String,
    /// Key-value pairs to set (merged with existing properties).
    pub properties: HashMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemovePropertiesRequest {
    /// Name of the pool containing the subvolume.
    pub pool: String,
    /// Name of the subvolume to update.
    pub name: String,
    /// Property keys to remove.
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindByPropertyRequest {
    /// Optional pool to restrict the search to.
    pub pool: Option<String>,
    /// xattr property key to match against.
    pub key: String,
    /// Value that the property key must equal.
    pub value: String,
}

pub struct SubvolumeService {
    pools: PoolService,
}

impl SubvolumeService {
    pub fn new(pools: PoolService) -> Self {
        Self { pools }
    }

    /// Re-attach loop devices for block subvolumes after pools are mounted.
    /// Re-attach loop devices for all block subvolumes after a reboot.
    /// Returns a map of subvolume_name → current loop device path so callers
    /// can patch NVMe-oF / iSCSI state files before those services start.
    pub async fn restore_block_devices(&self) -> std::collections::HashMap<String, String> {
        let metas: Vec<SubvolumeMeta> = state_dir().load_all().await;
        let block_metas: Vec<_> = metas
            .iter()
            .filter(|m| m.subvolume_type == SubvolumeType::Block)
            .collect();

        let mut dev_map = std::collections::HashMap::new();

        if block_metas.is_empty() {
            info!("No block subvolumes to restore");
            return dev_map;
        }

        for meta in block_metas {
            let mount_point = match self.pool_mount_point(&meta.pool).await {
                Ok(mp) => mp,
                Err(_) => {
                    warn!(
                        "Pool '{}' not mounted, skipping block restore for '{}'",
                        meta.pool, meta.name
                    );
                    continue;
                }
            };

            let img_path = format!("{}/{BLOCK_FILE_NAME}", subvol_path(&mount_point, &meta.name));
            if !Path::new(&img_path).exists() {
                warn!("Block image {img_path} not found for {}/{}", meta.pool, meta.name);
                continue;
            }

            // Use existing loop device if already attached (engine restart, not reboot)
            let loop_dev = if let Some(existing) = find_loop_device(&img_path).await {
                info!("Loop device already attached for {}/{}", meta.pool, meta.name);
                existing
            } else {
                match cmd::run_ok("losetup", &["--find", "--show", &img_path]).await {
                    Ok(dev) => {
                        let dev = dev.trim().to_string();
                        info!("Attached {} for block subvolume {}/{}", dev, meta.pool, meta.name);
                        dev
                    }
                    Err(e) => {
                        warn!("Failed to attach loop device for {}/{}: {e}", meta.pool, meta.name);
                        continue;
                    }
                }
            };

            dev_map.insert(meta.name.clone(), loop_dev);
        }

        dev_map
    }

    /// Get the mount point for a pool, or error if not mounted
    async fn pool_mount_point(&self, pool_name: &str) -> Result<String, SubvolumeError> {
        let pool = self
            .pools
            .get(pool_name)
            .await
            .map_err(|_| SubvolumeError::PoolNotFound(pool_name.to_string()))?;

        pool.mount_point
            .ok_or_else(|| SubvolumeError::PoolNotMounted(pool_name.to_string()))
    }

    /// List subvolumes in a pool.
    /// `owner_filter`: if Some, only return subvolumes owned by that token.
    pub async fn list(&self, pool_name: &str, owner_filter: Option<&str>) -> Result<Vec<Subvolume>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;
        let state: Vec<SubvolumeMeta> = state_dir().load_all().await;
        let mut subvolumes = Vec::new();

        // Ask bcachefs which paths are real subvolumes (filters out plain dirs)
        let info = bcachefs_list_all(&mount_point).await;

        // Subvolumes sit directly at the pool root — no subdirectory prefix.
        // Exclude anything with '/' (nested) or '@' (snapshot).
        for name in info.subvol_paths.iter().filter(|p| !p.is_empty() && !p.contains('/') && !p.contains('@')) {
            let path_str = subvol_path(&mount_point, name);
            let path = Path::new(&path_str);

            let meta = state.iter().find(|m| m.pool == pool_name && m.name == name.as_str());

            // Apply owner filter: operators only see their own subvolumes
            if let Some(filter) = owner_filter {
                match meta {
                    Some(m) if m.owner.as_deref() == Some(filter) => {}
                    _ => continue,
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
                    Snapshot {
                        name: snap_name.clone(),
                        subvolume: name.to_string(),
                        pool: pool_name.to_string(),
                        path: snap_path(&mount_point, name, &snap_name),
                        read_only,
                        block_device: None,
                    }
                })
                .collect();
            let size = dir_usage(path).await;

            let (subvolume_type, volsize_bytes, compression, comments, owner) =
                if let Some(m) = meta {
                    (m.subvolume_type.clone(), m.volsize_bytes, m.compression.clone(), m.comments.clone(), m.owner.clone())
                } else {
                    // Auto-detect: if vol.img exists, it's a block subvolume
                    let img_path = format!("{path_str}/{BLOCK_FILE_NAME}");
                    if Path::new(&img_path).exists() {
                        (SubvolumeType::Block, file_size(&img_path).await, None, None, None)
                    } else {
                        (SubvolumeType::Filesystem, None, None, None, None)
                    }
                };

            let block_device = if subvolume_type == SubvolumeType::Block {
                let img_path = format!("{path_str}/{BLOCK_FILE_NAME}");
                find_loop_device(&img_path).await
            } else {
                None
            };

            let properties = read_xattrs(path);

            subvolumes.push(Subvolume {
                name: name.to_string(),
                pool: pool_name.to_string(),
                subvolume_type,
                path: path_str,
                used_bytes: size,
                compression,
                comments,
                volsize_bytes,
                block_device,
                snapshots: snapshots.iter().map(|s| s.name.clone()).collect(),
                owner,
                properties,
            });
        }

        Ok(subvolumes)
    }

    /// List subvolumes across all mounted pools.
    /// `pool_filter`: if Some, only include that pool.
    /// `owner_filter`: if Some, only include subvolumes owned by that token.
    pub async fn list_all(&self, pool_filter: Option<&str>, owner_filter: Option<&str>) -> Result<Vec<Subvolume>, SubvolumeError> {
        let pools = self.pools.list().await
            .map_err(|e| SubvolumeError::CommandFailed(e.to_string()))?;

        let mut all = Vec::new();
        for pool in pools {
            if !pool.mounted {
                continue;
            }
            if let Some(filter) = pool_filter {
                if pool.name != filter {
                    continue;
                }
            }
            match self.list(&pool.name, owner_filter).await {
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
        pool_name: &str,
        name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvolumes = self.list(pool_name, owner_filter).await?;
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

        let mount_point = self.pool_mount_point(&req.pool).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

        if Path::new(&subvol_path).exists() {
            return Err(SubvolumeError::AlreadyExists(req.name.clone()));
        }

        if req.subvolume_type == SubvolumeType::Block && req.volsize_bytes.is_none() {
            return Err(SubvolumeError::VolsizeRequired);
        }

        // Create the bcachefs subvolume
        info!("Creating subvolume '{}' in pool '{}'", req.name, req.pool);
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
                let projid = project_id_for(&req.pool, &req.name);
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

            info!("Attaching loop device for '{}'", req.name);
            cmd::run_ok("losetup", &["--find", "--show", &img_path])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }

        // Save metadata
        let id = SubvolumeMeta::make_id(&req.pool, &req.name);
        let meta = SubvolumeMeta {
            id: id.clone(),
            name: req.name.clone(),
            pool: req.pool.clone(),
            subvolume_type: req.subvolume_type,
            volsize_bytes: req.volsize_bytes,
            compression: req.compression,
            comments: req.comments,
            owner,
        };
        state_dir().save(&id, &meta).await?;

        self.get(&req.pool, &req.name, None).await
    }

    /// Delete a subvolume.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn delete(&self, req: DeleteSubvolumeRequest, owner_filter: Option<&str>) -> Result<(), SubvolumeError> {
        let subvol = self.get(&req.pool, &req.name, owner_filter).await?;

        // For block subvolumes: detach loop device first
        if subvol.subvolume_type == SubvolumeType::Block {
            if let Some(ref loop_dev) = subvol.block_device {
                info!("Detaching loop device {} for '{}'", loop_dev, req.name);
                if let Err(e) = cmd::run_ok("losetup", &["-d", loop_dev]).await {
                    warn!("Failed to detach loop device {loop_dev}: {e}");
                }
            }
        }

        let mount_point = self.pool_mount_point(&req.pool).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

        // Delete all snapshots for this subvolume first
        let snapshots = self
            .list_snapshots_for(&req.pool, &req.name)
            .await?;
        for snap in snapshots {
            info!("Deleting snapshot '{}' before subvolume deletion", snap.name);
            cmd::run_ok("bcachefs", &["subvolume", "delete", &snap.path])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }

        info!("Deleting subvolume '{}' from pool '{}'", req.name, req.pool);
        cmd::run_ok("bcachefs", &["subvolume", "delete", &subvol_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        // Remove project quota registration if this was a filesystem subvolume
        if subvol.subvolume_type == SubvolumeType::Filesystem {
            let projid = project_id_for(&req.pool, &req.name);
            unregister_project(projid);
        }

        // Remove from state
        let id = SubvolumeMeta::make_id(&req.pool, &req.name);
        state_dir().remove(&id).await?;

        Ok(())
    }

    /// Attach a block subvolume's loop device (e.g. after reboot).
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn attach(
        &self,
        pool_name: &str,
        name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(pool_name, name, owner_filter).await?;
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
        cmd::run_ok("losetup", &["--find", "--show", &img_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        self.get(pool_name, name, owner_filter).await
    }

    /// Detach a block subvolume's loop device.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn detach(
        &self,
        pool_name: &str,
        name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(pool_name, name, owner_filter).await?;
        if let Some(ref loop_dev) = subvol.block_device {
            info!("Detaching loop device {} for '{}'", loop_dev, name);
            cmd::run_ok("losetup", &["-d", loop_dev])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }
        self.get(pool_name, name, owner_filter).await
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
        let subvol = self.get(&req.pool, &req.name, owner_filter).await?;

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
                let mount_point = self.pool_mount_point(&req.pool).await?;
                let projid = project_id_for(&req.pool, &req.name);
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

        // Update stored metadata
        let id = SubvolumeMeta::make_id(&req.pool, &req.name);
        if let Some(mut meta) = state_dir().load::<SubvolumeMeta>(&id).await {
            meta.volsize_bytes = Some(req.volsize_bytes);
            state_dir().save(&id, &meta).await?;
        }

        self.get(&req.pool, &req.name, owner_filter).await
    }

    /// Create a snapshot of a subvolume.
    /// `owner_filter`: if Some, verifies the caller owns the parent subvolume.
    pub async fn create_snapshot(
        &self,
        req: CreateSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<Snapshot, SubvolumeError> {
        // Verify ownership of the parent subvolume
        self.get(&req.pool, &req.subvolume, owner_filter).await?;

        let mount_point = self.pool_mount_point(&req.pool).await?;
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
        let subvol = self.get(&req.pool, &req.subvolume, owner_filter).await?;
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
            req.name, req.pool, req.subvolume
        );
        // Snapshots are always read-only; use snapshot.clone for writable copies
        cmd::run_ok("bcachefs", &["subvolume", "snapshot", "-r", &source_path, &snap_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        Ok(Snapshot {
            name: req.name,
            subvolume: req.subvolume,
            pool: req.pool,
            path: snap_path,
            read_only: true,
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
        self.get(&req.pool, &req.subvolume, owner_filter).await?;
        let mount_point = self.pool_mount_point(&req.pool).await?;
        let snap_path = snap_path(&mount_point, &req.subvolume, &req.name);

        if !Path::new(&snap_path).exists() {
            return Err(SubvolumeError::NotFound(req.name.clone()));
        }

        info!(
            "Deleting snapshot '{}' of subvolume '{}/{}'",
            req.name, req.pool, req.subvolume
        );
        cmd::run_ok("bcachefs", &["subvolume", "delete", &snap_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        Ok(())
    }

    /// List snapshots for a specific subvolume using `bcachefs subvolume list-snapshots`.
    pub async fn list_snapshots_for(
        &self,
        pool_name: &str,
        subvol_name: &str,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;
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
                    pool: pool_name.to_string(),
                    read_only,
                    block_device: None,
                }
            })
            .collect();

        Ok(snapshots)
    }

    /// List all snapshots across all subvolumes in a pool.
    /// `owner_filter`: if Some, only returns snapshots whose parent subvolume is owned by that token.
    /// Single-pass scan of the subvolumes/ directory for entries containing '@'.
    pub async fn list_snapshots(
        &self,
        pool_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;

        // Get owned subvolume names if filter is active
        let owned: Option<std::collections::HashSet<String>> = if owner_filter.is_some() {
            let owned_subvols = self.list(pool_name, owner_filter).await.unwrap_or_default();
            Some(owned_subvols.into_iter().map(|s| s.name).collect())
        } else {
            None
        };

        let info = bcachefs_list_all(&mount_point).await;

        let mut all_snapshots = Vec::new();
        for (rel_path, read_only) in info.snapshot_flags {
            // Snapshots live directly at pool root: "subvol@snap" (no '/')
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
            all_snapshots.push(Snapshot {
                name: snap_name.clone(),
                subvolume: subvol_name.clone(),
                pool: pool_name.to_string(),
                path: snap_path(&mount_point, &subvol_name, &snap_name),
                read_only,
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

        // Verify ownership of the parent subvolume and inherit its type
        let parent = self.get(&req.pool, &req.subvolume, owner_filter).await?;

        let mount_point = self.pool_mount_point(&req.pool).await?;
        let snap_path = snap_path(&mount_point, &req.subvolume, &req.snapshot);
        let new_subvol_path = subvol_path(&mount_point, &req.new_name);

        if !Path::new(&snap_path).exists() {
            return Err(SubvolumeError::NotFound(req.snapshot.clone()));
        }
        if Path::new(&new_subvol_path).exists() {
            return Err(SubvolumeError::AlreadyExists(req.new_name.clone()));
        }

        info!(
            "Cloning snapshot '{}/{}@{}' to new subvolume '{}'",
            req.pool, req.subvolume, req.snapshot, req.new_name
        );
        // bcachefs subvolume snapshot without -r creates a writable subvolume from snapshot
        cmd::run_ok("bcachefs", &["subvolume", "snapshot", &snap_path, &new_subvol_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        // Save metadata for the new subvolume, inheriting the parent's type and size
        let id = SubvolumeMeta::make_id(&req.pool, &req.new_name);
        let meta = SubvolumeMeta {
            id: id.clone(),
            name: req.new_name.clone(),
            pool: req.pool.clone(),
            subvolume_type: parent.subvolume_type,
            volsize_bytes: parent.volsize_bytes,
            compression: parent.compression,
            comments: None,
            owner: owner_filter.map(|s| s.to_string()),
        };
        state_dir().save(&id, &meta).await?;

        self.get(&req.pool, &req.new_name, None).await
    }

    /// Set (merge-upsert) xattr properties on a subvolume.
    pub async fn set_properties(
        &self,
        req: SetPropertiesRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(&req.pool, &req.name, owner_filter).await?;

        for (key, value) in &req.properties {
            let xattr_name = format!("{XATTR_NS}{key}");
            xattr::set(&subvol.path, &xattr_name, value.as_bytes())
                .map_err(|e| SubvolumeError::CommandFailed(
                    format!("setxattr {xattr_name}: {e}")
                ))?;
        }

        self.get(&req.pool, &req.name, owner_filter).await
    }

    /// Remove specific xattr properties from a subvolume.
    pub async fn remove_properties(
        &self,
        req: RemovePropertiesRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(&req.pool, &req.name, owner_filter).await?;

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

        self.get(&req.pool, &req.name, owner_filter).await
    }

    /// Find subvolumes where the given property key equals the given value.
    /// Optionally restricted to a single pool.
    pub async fn find_by_property(
        &self,
        req: FindByPropertyRequest,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Subvolume>, SubvolumeError> {
        let all = self.list_all(req.pool.as_deref(), owner_filter).await?;
        Ok(all
            .into_iter()
            .filter(|s| s.properties.get(&req.key).map(|v| v == &req.value).unwrap_or(false))
            .collect())
    }
}

/// Read all nasty-csi xattr properties from a path.
/// Returns a map of logical key → value (strips the "user." namespace prefix).
/// Non-UTF-8 values and unreadable xattrs are silently skipped.
fn read_xattrs(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let attrs = match xattr::list(path) {
        Ok(a) => a,
        Err(_) => return map,
    };
    for name in attrs {
        let name_str = name.to_string_lossy();
        // Only expose user.* namespace, strip the "user." prefix for the logical key
        let Some(key) = name_str.strip_prefix(XATTR_NS) else { continue };
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

    for entry in entries {
        let is_ro = entry.flags.as_deref() == Some("ro");
        if entry.snapshot_parent.is_some() && is_ro {
            // Read-only snapshot
            snapshot_flags.insert(entry.path, true);
        } else if entry.snapshot_parent.is_some() {
            // Writable clone (bcachefs subvolume snapshot without -r):
            // has snapshot_parent but is not ro — treat as a regular subvolume
            subvol_paths.insert(entry.path);
        } else {
            subvol_paths.insert(entry.path);
        }
    }

    BcachefsInfo { subvol_paths, snapshot_flags }
}

/// Derive a stable 32-bit project ID from pool + subvolume name.
/// Zero is reserved by the kernel so we ensure the result is ≥ 1.
fn project_id_for(pool: &str, name: &str) -> u32 {
    let mut h = DefaultHasher::new();
    pool.hash(&mut h);
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

/// Get file size
async fn file_size(path: &str) -> Option<u64> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.len())
}
