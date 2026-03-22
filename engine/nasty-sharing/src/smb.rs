use std::collections::HashMap;
use std::path::Path;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

const NASTY_SMB_CONF_PATH: &str = "/etc/samba/smb.nasty.conf";
const NASTY_SMB_SHARE_DIR: &str = "/etc/samba/nasty.d";
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

        if let Some(existing) = shares.into_iter().find(|s| s.name == req.name) {
            info!("SMB share '{}' already exists, returning existing (idempotent)", req.name);
            return Ok(existing);
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
        write_share_conf(&share).await?;
        rebuild_include_list().await?;
        reload_samba().await?;

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
        write_share_conf(&share).await?;
        rebuild_include_list().await?;
        reload_samba().await?;

        info!("Updated SMB share '{}'", share.name);
        Ok(share)
    }

    pub async fn delete(&self, req: DeleteSmbShareRequest) -> Result<(), SmbError> {

        let _: SmbShare = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| SmbError::NotFound(req.id.clone()))?;

        state_dir().remove(&req.id).await?;
        remove_share_conf(&req.id).await;
        rebuild_include_list().await?;
        reload_samba().await?;

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

/// Write a single share config file: /etc/samba/nasty.d/{id}.conf
async fn write_share_conf(share: &SmbShare) -> Result<(), SmbError> {
    tokio::fs::create_dir_all(NASTY_SMB_SHARE_DIR).await?;

    let path = share_conf_path(&share.id);

    if !share.enabled {
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(());
    }

    let mut conf = format!("[{}]\n", share.name);
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

    if share.guest_ok {
        conf.push_str("    force user = nobody\n");
        conf.push_str("    force group = nogroup\n");
        conf.push_str("    create mask = 0666\n");
        conf.push_str("    directory mask = 0777\n");
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

    tokio::fs::write(&path, &conf).await?;

    // Ensure guest share directories are world-writable
    if share.guest_ok {
        let _ = tokio::process::Command::new("chmod")
            .args(["0777", &share.path])
            .output()
            .await;
    }

    Ok(())
}

/// Remove the config file for a share.
async fn remove_share_conf(id: &str) {
    let path = share_conf_path(id);
    if let Err(e) = tokio::fs::remove_file(&path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Failed to remove share conf {path}: {e}");
        }
    }
}

/// Rebuild smb.nasty.conf as a list of includes from per-share files.
/// This is the only shared file — it's just a directory listing, no parsing.
async fn rebuild_include_list() -> Result<(), SmbError> {
    tokio::fs::create_dir_all(NASTY_SMB_SHARE_DIR).await?;

    let mut includes = String::from("# Managed by NASty — do not edit manually\n");
    includes.push_str("# Per-share configs in /etc/samba/nasty.d/\n\n");

    let mut dir = tokio::fs::read_dir(NASTY_SMB_SHARE_DIR).await?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".conf") {
            includes.push_str(&format!("include = {NASTY_SMB_SHARE_DIR}/{name}\n"));
        }
    }

    tokio::fs::write(NASTY_SMB_CONF_PATH, &includes).await?;
    Ok(())
}

/// Path to the per-share SMB config file.
fn share_conf_path(id: &str) -> String {
    format!("{NASTY_SMB_SHARE_DIR}/{id}.conf")
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

// ── SMB User Management ─────────────────────────────────────────

const SMB_USER_UID_MIN: u32 = 3000;

/// SMB user info returned by list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmbUser {
    /// Linux username.
    pub username: String,
    /// Unix UID.
    pub uid: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSmbUserRequest {
    /// Username (alphanumeric + hyphens, 1-32 chars).
    pub username: String,
    /// Password for SMB authentication.
    pub password: String,
}

impl SmbService {
    /// Create a Linux system user and set their Samba password.
    pub async fn create_user(&self, req: CreateSmbUserRequest) -> Result<SmbUser, SmbError> {
        let username = req.username.trim();
        if username.is_empty() || username.len() > 32 {
            return Err(SmbError::InvalidName("username must be 1-32 characters".into()));
        }
        if !username.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(SmbError::InvalidName("username must be alphanumeric, hyphens, or underscores".into()));
        }

        // Check if user already exists
        let check = tokio::process::Command::new("id")
            .arg(username)
            .output().await
            .map_err(|e| SmbError::ReloadFailed(format!("id: {e}")))?;
        if check.status.success() {
            return Err(SmbError::NameExists(username.to_string()));
        }

        // Find next available UID
        let uid = next_available_uid().await;

        // Create system user with no shell, no home
        let output = tokio::process::Command::new("useradd")
            .args([
                "--system",
                "--uid", &uid.to_string(),
                "--no-create-home",
                "--shell", "/usr/sbin/nologin",
                username,
            ])
            .output().await
            .map_err(|e| SmbError::ReloadFailed(format!("useradd: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SmbError::ReloadFailed(format!("useradd failed: {stderr}")));
        }

        // Set Samba password
        set_smb_password(username, &req.password).await?;

        info!("Created SMB user '{username}' (UID {uid})");
        Ok(SmbUser { username: username.to_string(), uid })
    }

    /// Delete a Linux system user and remove their Samba password.
    pub async fn delete_user(&self, username: &str) -> Result<(), SmbError> {
        // Remove Samba password
        let _ = tokio::process::Command::new("smbpasswd")
            .args(["-x", username])
            .output().await;

        // Delete system user
        let output = tokio::process::Command::new("userdel")
            .arg(username)
            .output().await
            .map_err(|e| SmbError::ReloadFailed(format!("userdel: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SmbError::ReloadFailed(format!("userdel failed: {stderr}")));
        }

        info!("Deleted SMB user '{username}'");
        Ok(())
    }

    /// Change an SMB user's password.
    pub async fn set_user_password(&self, username: &str, password: &str) -> Result<(), SmbError> {
        set_smb_password(username, password).await?;
        info!("Changed password for SMB user '{username}'");
        Ok(())
    }

    /// List SMB users (system users with UID >= SMB_USER_UID_MIN and in Samba's database).
    pub async fn list_users(&self) -> Result<Vec<SmbUser>, SmbError> {
        // List users from smbpasswd database
        let output = tokio::process::Command::new("pdbedit")
            .args(["-L", "-d", "0"])
            .output().await
            .map_err(|e| SmbError::ReloadFailed(format!("pdbedit: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut users = Vec::new();
        for line in stdout.lines() {
            // pdbedit -L format: "username:uid:full name"
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() >= 2 {
                if let Ok(uid) = parts[1].parse::<u32>() {
                    if uid >= SMB_USER_UID_MIN {
                        users.push(SmbUser {
                            username: parts[0].to_string(),
                            uid,
                        });
                    }
                }
            }
        }
        Ok(users)
    }
}

/// Set Samba password for a user via smbpasswd stdin.
async fn set_smb_password(username: &str, password: &str) -> Result<(), SmbError> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("smbpasswd")
        .args(["-a", "-s", username])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| SmbError::ReloadFailed(format!("smbpasswd: {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        // smbpasswd -s reads password twice from stdin
        let input = format!("{password}\n{password}\n");
        stdin.write_all(input.as_bytes()).await
            .map_err(|e| SmbError::ReloadFailed(format!("smbpasswd stdin: {e}")))?;
    }

    let output = child.wait_with_output().await
        .map_err(|e| SmbError::ReloadFailed(format!("smbpasswd wait: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SmbError::ReloadFailed(format!("smbpasswd failed: {stderr}")));
    }
    Ok(())
}

/// Find the next available UID starting from SMB_USER_UID_MIN.
async fn next_available_uid() -> u32 {
    for uid in SMB_USER_UID_MIN..SMB_USER_UID_MIN + 1000 {
        let check = tokio::process::Command::new("id")
            .arg(uid.to_string())
            .output().await;
        if let Ok(out) = check {
            if !out.status.success() {
                return uid;
            }
        }
    }
    SMB_USER_UID_MIN // fallback
}
