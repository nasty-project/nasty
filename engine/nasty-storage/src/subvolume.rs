use std::collections::HashMap;
use std::path::Path;

use nasty_common::{HasId, StateDir};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SubvolumeType {
    Filesystem,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subvolume {
    pub name: String,
    pub pool: String,
    pub subvolume_type: SubvolumeType,
    pub path: String,
    pub used_bytes: Option<u64>,
    pub compression: Option<String>,
    pub comments: Option<String>,
    // Block-specific
    pub volsize_bytes: Option<u64>,
    pub block_device: Option<String>,
    pub snapshots: Vec<String>,
    /// Token name that created this subvolume; None for subvolumes created by human users.
    pub owner: Option<String>,
    /// Arbitrary key-value metadata stored as POSIX xattrs (user.* namespace).
    /// Used by nasty-csi to track CSI volume metadata without sidecar files.
    #[serde(default)]
    pub properties: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub subvolume: String,
    pub pool: String,
    pub path: String,
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

#[derive(Debug, Deserialize)]
pub struct CreateSubvolumeRequest {
    pub pool: String,
    pub name: String,
    #[serde(default = "default_type")]
    pub subvolume_type: SubvolumeType,
    pub volsize_bytes: Option<u64>,
    pub compression: Option<String>,
    pub comments: Option<String>,
}

fn default_type() -> SubvolumeType {
    SubvolumeType::Filesystem
}

#[derive(Debug, Deserialize)]
pub struct DeleteSubvolumeRequest {
    pub pool: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSnapshotRequest {
    pub pool: String,
    pub subvolume: String,
    pub name: String,
    pub read_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteSnapshotRequest {
    pub pool: String,
    pub subvolume: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct CloneSnapshotRequest {
    pub pool: String,
    pub subvolume: String,
    pub snapshot: String,
    pub new_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ResizeSubvolumeRequest {
    pub pool: String,
    pub name: String,
    pub volsize_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct SetPropertiesRequest {
    pub pool: String,
    pub name: String,
    /// Key-value pairs to set (merged with existing properties).
    pub properties: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct RemovePropertiesRequest {
    pub pool: String,
    pub name: String,
    /// Property keys to remove.
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindByPropertyRequest {
    /// Optional pool to restrict the search to.
    pub pool: Option<String>,
    pub key: String,
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
    pub async fn restore_block_devices(&self) {
        let metas: Vec<SubvolumeMeta> = state_dir().load_all().await;
        let block_metas: Vec<_> = metas
            .iter()
            .filter(|m| m.subvolume_type == SubvolumeType::Block)
            .collect();

        if block_metas.is_empty() {
            info!("No block subvolumes to restore");
            return;
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
            // owner_filter = None: restore runs as system, not bound to any token

            let img_path = format!("{}/{BLOCK_FILE_NAME}", subvol_path(&mount_point, &meta.name));
            if !Path::new(&img_path).exists() {
                warn!("Block image {img_path} not found for {}/{}", meta.pool, meta.name);
                continue;
            }

            // Already attached?
            if find_loop_device(&img_path).await.is_some() {
                info!("Loop device already attached for {}/{}", meta.pool, meta.name);
                continue;
            }

            match cmd::run_ok("losetup", &["--find", "--show", &img_path]).await {
                Ok(dev) => info!("Attached {} for block subvolume {}/{}", dev.trim(), meta.pool, meta.name),
                Err(e) => warn!("Failed to attach loop device for {}/{}: {e}", meta.pool, meta.name),
            }
        }
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

    /// Attach a block snapshot for read verification via a reflink staging copy.
    ///
    /// nvmet requires a read-write block device (always opens with FMODE_WRITE),
    /// so we can't use losetup -r on the bcachefs read-only snapshot directly.
    /// Instead, we cp --reflink=always the snapshot's vol.img to a staging
    /// directory on the same bcachefs pool (instant COW duplicate, no data
    /// copied) and attach that staging copy as a normal read-write loop device.
    /// The initiator mounts it read-only on its side.
    pub async fn attach_snapshot(
        &self,
        pool_name: &str,
        subvol_name: &str,
        snap_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Snapshot, SubvolumeError> {
        self.get(pool_name, subvol_name, owner_filter).await?;
        let snaps = self.list_snapshots_for(pool_name, subvol_name).await?;
        let snap = snaps
            .into_iter()
            .find(|s| s.name == snap_name)
            .ok_or_else(|| SubvolumeError::NotFound(snap_name.to_string()))?;

        let img_path = format!("{}/{}", snap.path, BLOCK_FILE_NAME);
        if !Path::new(&img_path).exists() {
            return Err(SubvolumeError::CommandFailed(format!(
                "snapshot '{}' has no vol.img — not a block snapshot",
                snap_name
            )));
        }

        let mount_point = self.pool_mount_point(pool_name).await?;
        let staging_dir = format!("{mount_point}/.snap-attach");
        tokio::fs::create_dir_all(&staging_dir).await.map_err(SubvolumeError::Io)?;
        let staging_path = format!("{staging_dir}/{pool_name}_{subvol_name}_{snap_name}.img");

        // Already attached via staging copy?
        if let Some(dev) = find_loop_device(&staging_path).await {
            return Ok(Snapshot { block_device: Some(dev), ..snap });
        }

        // Reflink copy is instant on bcachefs (COW duplicate, no data written)
        info!("Creating reflink staging copy of snapshot '{}' for attachment", snap_name);
        cmd::run_ok("cp", &["--reflink=always", &img_path, &staging_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        let dev = cmd::run_ok("losetup", &["--find", "--show", &staging_path])
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        Ok(Snapshot { block_device: Some(dev.trim().to_string()), ..snap })
    }

    /// Detach a block snapshot's staging loop device and remove the staging copy.
    pub async fn detach_snapshot(
        &self,
        pool_name: &str,
        subvol_name: &str,
        snap_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<(), SubvolumeError> {
        self.get(pool_name, subvol_name, owner_filter).await?;

        let mount_point = self.pool_mount_point(pool_name).await?;
        let staging_path = format!(
            "{mount_point}/.snap-attach/{pool_name}_{subvol_name}_{snap_name}.img"
        );

        if let Some(dev) = find_loop_device(&staging_path).await {
            info!("Detaching staging loop device {} for snapshot '{}'", dev, snap_name);
            cmd::run_ok("losetup", &["-d", &dev])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }
        let _ = tokio::fs::remove_file(&staging_path).await;
        Ok(())
    }

    /// Resize a block subvolume's underlying sparse image.
    /// `owner_filter`: if Some, returns `AccessDenied` if the subvolume has a different owner.
    pub async fn resize(
        &self,
        req: ResizeSubvolumeRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(&req.pool, &req.name, owner_filter).await?;
        if subvol.subvolume_type != SubvolumeType::Block {
            return Err(SubvolumeError::CommandFailed(
                "only block subvolumes can be resized".to_string(),
            ));
        }

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
