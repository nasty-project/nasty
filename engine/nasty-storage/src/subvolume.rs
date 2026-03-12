use std::path::Path;

use nasty_common::{HasId, StateDir};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::cmd;
use crate::pool::PoolService;

const STATE_DIR: &str = "/var/lib/nasty/subvolumes";
const BLOCK_FILE_NAME: &str = "vol.img";

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub subvolume: String,
    pub pool: String,
    pub path: String,
    pub read_only: bool,
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

            let img_path = format!("{mount_point}/{}/{BLOCK_FILE_NAME}", meta.name);
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

    /// List subvolumes in a pool
    pub async fn list(&self, pool_name: &str) -> Result<Vec<Subvolume>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;
        let state: Vec<SubvolumeMeta> = state_dir().load_all().await;
        let mut subvolumes = Vec::new();

        let mut entries = tokio::fs::read_dir(&mount_point).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip the .snapshots directory
            if name == ".snapshots" {
                continue;
            }

            if path.is_dir() && is_subvolume(&path).await {
                let snapshots = self
                    .list_snapshots_for(pool_name, &name)
                    .await
                    .unwrap_or_default();

                let size = dir_usage(&path).await;
                let path_str = path.to_string_lossy().to_string();

                let meta = state
                    .iter()
                    .find(|m| m.pool == pool_name && m.name == name);

                let (subvolume_type, volsize_bytes, compression, comments) =
                    if let Some(m) = meta {
                        (
                            m.subvolume_type.clone(),
                            m.volsize_bytes,
                            m.compression.clone(),
                            m.comments.clone(),
                        )
                    } else {
                        // Auto-detect: if vol.img exists, it's a block subvolume
                        let img_path = format!("{path_str}/{BLOCK_FILE_NAME}");
                        if Path::new(&img_path).exists() {
                            (SubvolumeType::Block, file_size(&img_path).await, None, None)
                        } else {
                            (SubvolumeType::Filesystem, None, None, None)
                        }
                    };

                let block_device = if subvolume_type == SubvolumeType::Block {
                    let img_path = format!("{path_str}/{BLOCK_FILE_NAME}");
                    find_loop_device(&img_path).await
                } else {
                    None
                };

                subvolumes.push(Subvolume {
                    name,
                    pool: pool_name.to_string(),
                    subvolume_type,
                    path: path_str,
                    used_bytes: size,
                    compression,
                    comments,
                    volsize_bytes,
                    block_device,
                    snapshots: snapshots.iter().map(|s| s.name.clone()).collect(),
                });
            }
        }

        Ok(subvolumes)
    }

    /// List subvolumes across all mounted pools
    pub async fn list_all(&self) -> Result<Vec<Subvolume>, SubvolumeError> {
        let pools = self.pools.list().await
            .map_err(|e| SubvolumeError::CommandFailed(e.to_string()))?;

        let mut all = Vec::new();
        for pool in pools {
            if !pool.mounted {
                continue;
            }
            match self.list(&pool.name).await {
                Ok(mut subvols) => all.append(&mut subvols),
                Err(_) => continue,
            }
        }
        Ok(all)
    }

    /// Get a single subvolume
    pub async fn get(
        &self,
        pool_name: &str,
        name: &str,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvolumes = self.list(pool_name).await?;
        subvolumes
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| SubvolumeError::NotFound(name.to_string()))
    }

    /// Create a new subvolume
    pub async fn create(&self, req: CreateSubvolumeRequest) -> Result<Subvolume, SubvolumeError> {
        let mount_point = self.pool_mount_point(&req.pool).await?;
        let subvol_path = format!("{mount_point}/{}", req.name);

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
        };
        state_dir().save(&id, &meta).await?;

        self.get(&req.pool, &req.name).await
    }

    /// Delete a subvolume
    pub async fn delete(&self, req: DeleteSubvolumeRequest) -> Result<(), SubvolumeError> {
        let subvol = self.get(&req.pool, &req.name).await?;

        // For block subvolumes: detach loop device first
        if subvol.subvolume_type == SubvolumeType::Block {
            if let Some(ref loop_dev) = subvol.block_device {
                info!("Detaching loop device {} for '{}'", loop_dev, req.name);
                let _ = cmd::run_ok("losetup", &["-d", loop_dev]).await;
            }
        }

        let mount_point = self.pool_mount_point(&req.pool).await?;
        let subvol_path = format!("{mount_point}/{}", req.name);

        // Delete all snapshots for this subvolume first
        let snapshots = self
            .list_snapshots_for(&req.pool, &req.name)
            .await
            .unwrap_or_default();
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

    /// Attach a block subvolume's loop device (e.g. after reboot)
    pub async fn attach(
        &self,
        pool_name: &str,
        name: &str,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(pool_name, name).await?;
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

        self.get(pool_name, name).await
    }

    /// Detach a block subvolume's loop device
    pub async fn detach(
        &self,
        pool_name: &str,
        name: &str,
    ) -> Result<Subvolume, SubvolumeError> {
        let subvol = self.get(pool_name, name).await?;
        if let Some(ref loop_dev) = subvol.block_device {
            info!("Detaching loop device {} for '{}'", loop_dev, name);
            cmd::run_ok("losetup", &["-d", loop_dev])
                .await
                .map_err(SubvolumeError::CommandFailed)?;
        }
        self.get(pool_name, name).await
    }

    /// Create a snapshot of a subvolume
    pub async fn create_snapshot(
        &self,
        req: CreateSnapshotRequest,
    ) -> Result<Snapshot, SubvolumeError> {
        let mount_point = self.pool_mount_point(&req.pool).await?;
        let source_path = format!("{mount_point}/{}", req.subvolume);
        let snap_dir = format!("{mount_point}/.snapshots/{}", req.subvolume);
        let snap_path = format!("{snap_dir}/{}", req.name);

        if !Path::new(&source_path).exists() {
            return Err(SubvolumeError::NotFound(req.subvolume.clone()));
        }

        if Path::new(&snap_path).exists() {
            return Err(SubvolumeError::AlreadyExists(req.name.clone()));
        }

        // Ensure snapshot directory exists
        tokio::fs::create_dir_all(&snap_dir).await?;

        let mut args = vec!["subvolume", "snapshot"];
        if req.read_only == Some(true) {
            args.push("-r");
        }
        args.push(&source_path);
        args.push(&snap_path);

        info!(
            "Creating snapshot '{}' of subvolume '{}/{}'",
            req.name, req.pool, req.subvolume
        );
        cmd::run_ok("bcachefs", &args)
            .await
            .map_err(SubvolumeError::CommandFailed)?;

        Ok(Snapshot {
            name: req.name,
            subvolume: req.subvolume,
            pool: req.pool,
            path: snap_path,
            read_only: req.read_only.unwrap_or(false),
        })
    }

    /// Delete a snapshot
    pub async fn delete_snapshot(
        &self,
        req: DeleteSnapshotRequest,
    ) -> Result<(), SubvolumeError> {
        let mount_point = self.pool_mount_point(&req.pool).await?;
        let snap_path = format!(
            "{mount_point}/.snapshots/{}/{}",
            req.subvolume, req.name
        );

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

    /// List snapshots for a specific subvolume
    pub async fn list_snapshots_for(
        &self,
        pool_name: &str,
        subvol_name: &str,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;
        let snap_dir = format!("{mount_point}/.snapshots/{subvol_name}");

        if !Path::new(&snap_dir).exists() {
            return Ok(vec![]);
        }

        let mut snapshots = Vec::new();
        let mut entries = tokio::fs::read_dir(&snap_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                snapshots.push(Snapshot {
                    name,
                    subvolume: subvol_name.to_string(),
                    pool: pool_name.to_string(),
                    path: entry.path().to_string_lossy().to_string(),
                    read_only: false, // TODO: detect from bcachefs attributes
                });
            }
        }

        Ok(snapshots)
    }

    /// List all snapshots across all subvolumes in a pool
    pub async fn list_snapshots(
        &self,
        pool_name: &str,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;
        let snap_base = format!("{mount_point}/.snapshots");

        if !Path::new(&snap_base).exists() {
            return Ok(vec![]);
        }

        let mut all_snapshots = Vec::new();
        let mut entries = tokio::fs::read_dir(&snap_base).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().is_dir() {
                let subvol_name = entry.file_name().to_string_lossy().to_string();
                let mut snaps = self
                    .list_snapshots_for(pool_name, &subvol_name)
                    .await
                    .unwrap_or_default();
                all_snapshots.append(&mut snaps);
            }
        }

        Ok(all_snapshots)
    }
}

/// Check if a directory is a bcachefs subvolume.
/// For now we treat all direct children dirs of the mount as subvolumes.
async fn is_subvolume(path: &Path) -> bool {
    // TODO: use `bcachefs subvolume list` or check xattrs to distinguish
    // real subvolumes from regular directories.
    path.is_dir()
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

/// Find the loop device attached to a given file
async fn find_loop_device(file_path: &str) -> Option<String> {
    let output = cmd::run_ok("losetup", &["-j", file_path]).await.ok()?;
    let line = output.lines().next()?;
    let dev = line.split(':').next()?;
    if dev.starts_with("/dev/loop") {
        Some(dev.to_string())
    } else {
        None
    }
}

/// Get file size
async fn file_size(path: &str) -> Option<u64> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.len())
}
