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
const SUBVOLUMES_DIR: &str = "subvolumes";

fn subvolumes_dir(mount_point: &str) -> String {
    format!("{mount_point}/{SUBVOLUMES_DIR}")
}

fn subvol_path(mount_point: &str, name: &str) -> String {
    format!("{mount_point}/{SUBVOLUMES_DIR}/{name}")
}

fn snap_path(mount_point: &str, subvol: &str, snap: &str) -> String {
    format!("{mount_point}/{SUBVOLUMES_DIR}/{subvol}@{snap}")
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

        let subvol_dir = subvolumes_dir(&mount_point);
        tokio::fs::create_dir_all(&subvol_dir).await?;
        let mut entries = tokio::fs::read_dir(&subvol_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Snapshot siblings use '@' naming — skip them here
            if name.contains('@') {
                continue;
            }

            if path.is_dir() && is_subvolume(&path).await {
                let meta = state
                    .iter()
                    .find(|m| m.pool == pool_name && m.name == name);

                // Apply owner filter: operators only see their own subvolumes
                if let Some(filter) = owner_filter {
                    match meta {
                        Some(m) if m.owner.as_deref() == Some(filter) => {}
                        _ => continue,
                    }
                }

                let snapshots = self
                    .list_snapshots_for(pool_name, &name)
                    .await
                    .unwrap_or_default();

                let size = dir_usage(&path).await;
                let path_str = path.to_string_lossy().to_string();

                let (subvolume_type, volsize_bytes, compression, comments, owner) =
                    if let Some(m) = meta {
                        (
                            m.subvolume_type.clone(),
                            m.volsize_bytes,
                            m.compression.clone(),
                            m.comments.clone(),
                            m.owner.clone(),
                        )
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

                let properties = read_xattrs(&path);

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
                    owner,
                    properties,
                });
            }
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
        let subvol_dir = subvolumes_dir(&mount_point);
        tokio::fs::create_dir_all(&subvol_dir).await?;
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
                let _ = cmd::run_ok("losetup", &["-d", loop_dev]).await;
            }
        }

        let mount_point = self.pool_mount_point(&req.pool).await?;
        let subvol_path = subvol_path(&mount_point, &req.name);

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

    /// List snapshots for a specific subvolume.
    /// Scans the subvolumes/ directory for sibling entries named `<subvol>@<snap>`.
    pub async fn list_snapshots_for(
        &self,
        pool_name: &str,
        subvol_name: &str,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        let mount_point = self.pool_mount_point(pool_name).await?;
        let subvol_dir = subvolumes_dir(&mount_point);
        let prefix = format!("{subvol_name}@");

        if !Path::new(&subvol_dir).exists() {
            return Ok(vec![]);
        }

        let mut snapshots = Vec::new();
        let mut entries = tokio::fs::read_dir(&subvol_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && entry.path().is_dir() {
                let snap_name = name[prefix.len()..].to_string();
                snapshots.push(Snapshot {
                    name: snap_name,
                    subvolume: subvol_name.to_string(),
                    pool: pool_name.to_string(),
                    path: entry.path().to_string_lossy().to_string(),
                    read_only: false, // TODO: detect from bcachefs attributes
                });
            }
        }

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
        let subvol_dir = subvolumes_dir(&mount_point);

        if !Path::new(&subvol_dir).exists() {
            return Ok(vec![]);
        }

        // Get owned subvolume names if filter is active
        let owned: Option<std::collections::HashSet<String>> = if owner_filter.is_some() {
            let owned_subvols = self.list(pool_name, owner_filter).await.unwrap_or_default();
            Some(owned_subvols.into_iter().map(|s| s.name).collect())
        } else {
            None
        };

        let mut all_snapshots = Vec::new();
        let mut entries = tokio::fs::read_dir(&subvol_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(at_pos) = name.find('@') {
                if entry.path().is_dir() {
                    let subvol_name = name[..at_pos].to_string();
                    let snap_name = name[at_pos + 1..].to_string();
                    if let Some(ref set) = owned {
                        if !set.contains(&subvol_name) {
                            continue;
                        }
                    }
                    all_snapshots.push(Snapshot {
                        name: snap_name,
                        subvolume: subvol_name,
                        pool: pool_name.to_string(),
                        path: entry.path().to_string_lossy().to_string(),
                        read_only: false, // TODO: detect from bcachefs attributes
                    });
                }
            }
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

        // Verify ownership of the parent subvolume
        self.get(&req.pool, &req.subvolume, owner_filter).await?;

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

        // Save metadata for the new subvolume (inherits Filesystem type; no volsize)
        let id = SubvolumeMeta::make_id(&req.pool, &req.new_name);
        let meta = SubvolumeMeta {
            id: id.clone(),
            name: req.new_name.clone(),
            pool: req.pool.clone(),
            subvolume_type: SubvolumeType::Filesystem,
            volsize_bytes: None,
            compression: None,
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
