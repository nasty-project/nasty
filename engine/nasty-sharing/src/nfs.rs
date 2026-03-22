use std::path::Path;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

const NASTY_EXPORTS_DIR: &str = "/etc/exports.d";
const STATE_DIR: &str = "/var/lib/nasty/shares/nfs";

#[derive(Debug, Error)]
pub enum NfsError {
    #[error("share not found: {0}")]
    NotFound(String),
    #[error("share already exists for path: {0}")]
    AlreadyExists(String),
    #[error("path does not exist: {0}")]
    PathNotFound(String),
    #[error("path is not within a NASty pool: {0}")]
    PathNotInPool(String),
    #[error("exportfs failed: {0}")]
    ExportFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NfsShare {
    /// Unique share identifier (UUID).
    pub id: String,
    /// Absolute filesystem path being exported (must be under `/storage/`).
    pub path: String,
    /// Optional description of the share.
    pub comment: Option<String>,
    /// List of allowed clients and their export options.
    pub clients: Vec<NfsClient>,
    /// Whether the share is currently active in `/etc/exports.d/nasty.exports`.
    pub enabled: bool,
}

impl HasId for NfsShare {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NfsClient {
    /// Network or host: "192.168.1.0/24", "10.0.0.5", "*"
    pub host: String,
    /// NFS export options: "rw,sync,no_subtree_check,no_root_squash"
    pub options: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateNfsShareRequest {
    /// Absolute path to export (must exist and be under `/storage/`).
    pub path: String,
    /// Optional description.
    pub comment: Option<String>,
    /// Allowed clients and their export options.
    pub clients: Vec<NfsClient>,
    /// Whether to enable the share immediately (default: true).
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateNfsShareRequest {
    /// ID of the share to update.
    pub id: String,
    /// New description (optional).
    pub comment: Option<String>,
    /// Replacement client list (optional; replaces entire list when provided).
    pub clients: Option<Vec<NfsClient>>,
    /// Enable or disable the share (optional).
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteNfsShareRequest {
    pub id: String,
}

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

pub struct NfsService;

impl NfsService {
    pub fn new() -> Self {
        // Clean up legacy monolithic exports file if it exists.
        // Per-share files in /etc/exports.d/nasty-{id}.exports replace it.
        let legacy = format!("{NASTY_EXPORTS_DIR}/nasty.exports");
        if Path::new(&legacy).exists() {
            let _ = std::fs::remove_file(&legacy);
        }
        Self
    }

    /// List all NFS shares
    pub async fn list(&self) -> Result<Vec<NfsShare>, NfsError> {

        Ok(state_dir().load_all().await)
    }

    /// Get a single share by ID
    pub async fn get(&self, id: &str) -> Result<NfsShare, NfsError> {

        state_dir()
            .load::<NfsShare>(id)
            .await
            .ok_or_else(|| NfsError::NotFound(id.to_string()))
    }

    /// Create a new NFS share
    pub async fn create(&self, req: CreateNfsShareRequest) -> Result<NfsShare, NfsError> {
        if !Path::new(&req.path).exists() {
            return Err(NfsError::PathNotFound(req.path));
        }
        if !req.path.starts_with("/storage/") {
            return Err(NfsError::PathNotInPool(req.path));
        }


        let shares: Vec<NfsShare> = state_dir().load_all().await;

        if let Some(existing) = shares.into_iter().find(|s| s.path == req.path) {
            info!("NFS share for {} already exists, returning existing (idempotent)", req.path);
            return Ok(existing);
        }

        let share = NfsShare {
            id: Uuid::new_v4().to_string(),
            path: req.path,
            comment: req.comment,
            clients: req.clients,
            enabled: req.enabled.unwrap_or(true),
        };

        state_dir().save(&share.id, &share).await?;
        write_export_file(&share).await?;
        reload_exports().await?;

        info!("Created NFS share '{}' for {}", share.id, share.path);
        Ok(share)
    }

    /// Update an existing NFS share
    pub async fn update(&self, req: UpdateNfsShareRequest) -> Result<NfsShare, NfsError> {

        let mut share: NfsShare = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| NfsError::NotFound(req.id.clone()))?;

        if let Some(comment) = req.comment {
            share.comment = Some(comment);
        }
        if let Some(clients) = req.clients {
            share.clients = clients;
        }
        if let Some(enabled) = req.enabled {
            share.enabled = enabled;
        }

        state_dir().save(&share.id, &share).await?;
        write_export_file(&share).await?;
        reload_exports().await?;

        info!("Updated NFS share '{}'", share.id);
        Ok(share)
    }

    /// Delete an NFS share
    pub async fn delete(&self, req: DeleteNfsShareRequest) -> Result<(), NfsError> {

        let _: NfsShare = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| NfsError::NotFound(req.id.clone()))?;

        state_dir().remove(&req.id).await?;
        remove_export_file(&req.id).await;
        reload_exports().await?;

        info!("Deleted NFS share '{}'", req.id);
        Ok(())
    }
}

/// Write a single export file for one share: /etc/exports.d/nasty-{id}.exports
async fn write_export_file(share: &NfsShare) -> Result<(), NfsError> {
    tokio::fs::create_dir_all(NASTY_EXPORTS_DIR).await?;

    let path = export_file_path(&share.id);

    if !share.enabled || !Path::new(&share.path).exists() {
        // Disabled or stale — remove the file if it exists
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(());
    }

    let fsid = stable_fsid(&share.id);

    let clients: Vec<String> = share
        .clients
        .iter()
        .map(|c| {
            let mut opts = c.options.clone();
            if !opts.contains("fsid=") {
                opts = format!("{opts},fsid={fsid}");
            }
            if !opts.contains("insecure") && !opts.contains("secure") {
                opts = format!("{opts},insecure");
            }
            format!("{}({})", c.host, opts)
        })
        .collect();

    let content = format!(
        "# NASty share {}\n{}\t{}\n",
        share.id,
        share.path,
        clients.join(" ")
    );

    tokio::fs::write(&path, &content).await?;
    Ok(())
}

/// Remove the export file for a share.
async fn remove_export_file(id: &str) {
    let path = export_file_path(id);
    if let Err(e) = tokio::fs::remove_file(&path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove export file {path}: {e}");
        }
    }
}

/// Path to the per-share export file.
fn export_file_path(id: &str) -> String {
    format!("{NASTY_EXPORTS_DIR}/nasty-{id}.exports")
}

/// Derive a stable numeric fsid (1–2^31) from a share ID string.
/// NFS needs fsid to identify non-root filesystems like bcachefs subvolumes.
fn stable_fsid(id: &str) -> u32 {
    let mut hash: u32 = 5381;
    for b in id.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    // Keep in range 1..2^31 (0 is reserved for root fs)
    (hash % 0x7FFF_FFFE) + 1
}

async fn reload_exports() -> Result<(), NfsError> {
    let output = tokio::process::Command::new("exportfs")
        .args(["-ra"])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NfsError::ExportFailed(stderr.to_string()));
    }

    info!("NFS exports reloaded");
    Ok(())
}
