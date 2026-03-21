use std::collections::HashMap;
use std::path::Path;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

const NASTY_SMB_CONF_PATH: &str = "/etc/samba/smb.nasty.conf";
const STATE_DIR: &str = "/var/lib/nasty/shares/smb";

#[derive(Debug, Error)]
pub enum SmbError {
    #[error("share not found: {0}")]
    NotFound(String),
    #[error("share name already exists: {0}")]
    NameExists(String),
    #[error("path does not exist: {0}")]
    PathNotFound(String),
    #[error("path is not within a NASty pool: {0}")]
    PathNotInPool(String),
    #[error("invalid share name: {0}")]
    InvalidName(String),
    #[error("samba reload failed: {0}")]
    ReloadFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmbShare {
    /// Unique share identifier (UUID).
    pub id: String,
    /// Samba share name used in `\\server\name` UNC paths.
    pub name: String,
    /// Absolute filesystem path being shared (must be under `/storage/`).
    pub path: String,
    /// Optional description shown in share listings.
    pub comment: Option<String>,
    /// Whether the share is read-only.
    pub read_only: bool,
    /// Whether the share is visible in network browse lists.
    pub browseable: bool,
    /// Whether unauthenticated guest access is allowed.
    pub guest_ok: bool,
    /// Usernames allowed to connect (empty means no restriction beyond authentication).
    pub valid_users: Vec<String>,
    /// Additional raw Samba parameters written to the share section.
    pub extra_params: HashMap<String, String>,
    /// Whether the share is active in `smb.nasty.conf`.
    pub enabled: bool,
}

impl HasId for SmbShare {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSmbShareRequest {
    /// Samba share name (1–80 characters, no special characters).
    pub name: String,
    /// Absolute path to share (must exist and be under `/storage/`).
    pub path: String,
    /// Optional description.
    pub comment: Option<String>,
    /// Whether the share is read-only (default: false).
    pub read_only: Option<bool>,
    /// Whether the share appears in browse lists (default: true).
    pub browseable: Option<bool>,
    /// Whether guest access is allowed (default: false).
    pub guest_ok: Option<bool>,
    /// Allowed usernames; empty means no per-user restriction.
    pub valid_users: Option<Vec<String>>,
    /// Additional raw Samba parameters for this share section.
    pub extra_params: Option<HashMap<String, String>>,
    /// Whether to enable the share immediately (default: true).
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateSmbShareRequest {
    /// ID of the share to update.
    pub id: String,
    /// New share name (optional; must be unique).
    pub name: Option<String>,
    /// New description (optional).
    pub comment: Option<String>,
    /// Update read-only flag (optional).
    pub read_only: Option<bool>,
    /// Update browseable flag (optional).
    pub browseable: Option<bool>,
    /// Update guest access flag (optional).
    pub guest_ok: Option<bool>,
    /// Replacement allowed-users list (optional).
    pub valid_users: Option<Vec<String>>,
    /// Replacement extra Samba parameters (optional).
    pub extra_params: Option<HashMap<String, String>>,
    /// Enable or disable the share (optional).
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSmbShareRequest {
    pub id: String,
}

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

pub struct SmbService;

impl SmbService {
    pub fn new() -> Self {
        Self
    }

    pub async fn list(&self) -> Result<Vec<SmbShare>, SmbError> {

        Ok(state_dir().load_all().await)
    }

    pub async fn get(&self, id: &str) -> Result<SmbShare, SmbError> {

        state_dir()
            .load::<SmbShare>(id)
            .await
            .ok_or_else(|| SmbError::NotFound(id.to_string()))
    }

    pub async fn create(&self, req: CreateSmbShareRequest) -> Result<SmbShare, SmbError> {
        validate_share_name(&req.name)?;

        if !Path::new(&req.path).exists() {
            return Err(SmbError::PathNotFound(req.path));
        }
        if !req.path.starts_with("/storage/") {
            return Err(SmbError::PathNotInPool(req.path));
        }


        let shares: Vec<SmbShare> = state_dir().load_all().await;

        if shares.iter().any(|s| s.name == req.name) {
            return Err(SmbError::NameExists(req.name));
        }

        let share = SmbShare {
            id: Uuid::new_v4().to_string(),
            name: req.name,
            path: req.path,
            comment: req.comment,
            read_only: req.read_only.unwrap_or(false),
            browseable: req.browseable.unwrap_or(true),
            guest_ok: req.guest_ok.unwrap_or(false),
            valid_users: req.valid_users.unwrap_or_default(),
            extra_params: req.extra_params.unwrap_or_default(),
            enabled: req.enabled.unwrap_or(true),
        };

        state_dir().save(&share.id, &share).await?;
        apply_config().await?;

        info!("Created SMB share '{}' at {}", share.name, share.path);
        Ok(share)
    }

    pub async fn update(&self, req: UpdateSmbShareRequest) -> Result<SmbShare, SmbError> {
        if let Some(ref new_name) = req.name {
            validate_share_name(new_name)?;
        }


        let mut share: SmbShare = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| SmbError::NotFound(req.id.clone()))?;

        // Check name uniqueness if changing
        if let Some(ref new_name) = req.name {
            let shares: Vec<SmbShare> = state_dir().load_all().await;
            if shares
                .iter()
                .any(|s| s.name == *new_name && s.id != req.id)
            {
                return Err(SmbError::NameExists(new_name.clone()));
            }
        }

        if let Some(name) = req.name {
            share.name = name;
        }
        if let Some(comment) = req.comment {
            share.comment = Some(comment);
        }
        if let Some(read_only) = req.read_only {
            share.read_only = read_only;
        }
        if let Some(browseable) = req.browseable {
            share.browseable = browseable;
        }
        if let Some(guest_ok) = req.guest_ok {
            share.guest_ok = guest_ok;
        }
        if let Some(valid_users) = req.valid_users {
            share.valid_users = valid_users;
        }
        if let Some(extra_params) = req.extra_params {
            share.extra_params = extra_params;
        }
        if let Some(enabled) = req.enabled {
            share.enabled = enabled;
        }

        state_dir().save(&share.id, &share).await?;
        apply_config().await?;

        info!("Updated SMB share '{}'", share.name);
        Ok(share)
    }

    pub async fn delete(&self, req: DeleteSmbShareRequest) -> Result<(), SmbError> {

        let _: SmbShare = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| SmbError::NotFound(req.id.clone()))?;

        state_dir().remove(&req.id).await?;
        apply_config().await?;

        info!("Deleted SMB share '{}'", req.id);
        Ok(())
    }
}

fn validate_share_name(name: &str) -> Result<(), SmbError> {
    if name.is_empty()
        || name.len() > 80
        || name.contains(['/', '\\', '[', ']', ':', '|', '<', '>', '+', '=', ';', ',', '?', '*'])
    {
        return Err(SmbError::InvalidName(
            "Share name must be 1-80 chars without special characters".to_string(),
        ));
    }
    Ok(())
}

/// Generate smb.nasty.conf from all share files and reload samba
async fn apply_config() -> Result<(), SmbError> {
    let shares: Vec<SmbShare> = state_dir().load_all().await;

    let mut conf = String::from("# Managed by NASty — do not edit manually\n\n");

    for share in &shares {
        if !share.enabled {
            continue;
        }

        conf.push_str(&format!("[{}]\n", share.name));
        conf.push_str(&format!("    path = {}\n", share.path));

        if let Some(ref comment) = share.comment {
            conf.push_str(&format!("    comment = {comment}\n"));
        }

        conf.push_str(&format!(
            "    read only = {}\n",
            if share.read_only { "yes" } else { "no" }
        ));
        conf.push_str(&format!(
            "    browseable = {}\n",
            if share.browseable { "yes" } else { "no" }
        ));
        conf.push_str(&format!(
            "    guest ok = {}\n",
            if share.guest_ok { "yes" } else { "no" }
        ));

        // When guest access is enabled, force operations as root so guests
        // can read/write regardless of underlying Unix permissions.
        // Clear "invalid users" to override the NixOS global default that
        // blocks root — without this, force user = root is rejected.
        if share.guest_ok {
            conf.push_str("    force user = root\n");
            conf.push_str("    force group = root\n");
            conf.push_str("    invalid users =\n");
        }

        if !share.valid_users.is_empty() {
            conf.push_str(&format!(
                "    valid users = {}\n",
                share.valid_users.join(" ")
            ));
        }

        let mut extra: Vec<_> = share.extra_params.iter().collect();
        extra.sort_by_key(|(k, _)| *k);
        for (key, value) in extra {
            conf.push_str(&format!("    {key} = {value}\n"));
        }

        conf.push('\n');
    }

    tokio::fs::write(NASTY_SMB_CONF_PATH, &conf).await?;
    reload_samba().await?;
    Ok(())
}

async fn reload_samba() -> Result<(), SmbError> {
    let output = tokio::process::Command::new("smbcontrol")
        .args(["all", "reload-config"])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SmbError::ReloadFailed(stderr.to_string()));
    }

    info!("Samba configuration reloaded");
    Ok(())
}
