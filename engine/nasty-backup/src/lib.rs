//! Backup system — deduplicating, encrypted backups via rustic/restic.
//!
//! Manages backup profiles, scheduling, and execution. Uses rustic CLI
//! for the actual backup operations (restic-compatible repo format).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{error, info, warn};

const STATE_PATH: &str = "/var/lib/nasty/backups.json";

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupProfile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Source paths to back up (e.g. ["/fs/first/media", "/fs/first/docs"]).
    pub sources: Vec<String>,
    /// Backup target configuration.
    pub target: BackupTarget,
    /// Cron expression for scheduled backups (e.g. "0 3 * * *" = daily 3am).
    #[serde(default)]
    pub schedule: Option<String>,
    /// Retention policy for snapshot pruning.
    #[serde(default)]
    pub retention: RetentionPolicy,
    /// Encryption password for the repository.
    pub password: String,
    /// Whether to create a bcachefs snapshot before backup.
    #[serde(default = "default_true")]
    pub snapshot_before: bool,
    /// Whether the repo has been initialized.
    #[serde(default)]
    pub repo_initialized: bool,
    /// Last backup result.
    #[serde(default)]
    pub last_run: Option<BackupRunResult>,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackupTarget {
    Local {
        path: String,
    },
    S3 {
        endpoint: String,
        bucket: String,
        access_key: String,
        secret_key: String,
        #[serde(default)]
        region: Option<String>,
    },
    Sftp {
        host: String,
        user: String,
        path: String,
        #[serde(default)]
        port: Option<u16>,
    },
    Rest {
        url: String,
    },
    B2 {
        bucket: String,
        account_id: String,
        account_key: String,
    },
}

impl BackupTarget {
    /// Convert to restic/rustic repository URL.
    fn to_repo_url(&self) -> String {
        match self {
            BackupTarget::Local { path } => path.clone(),
            BackupTarget::S3 { endpoint, bucket, .. } => {
                if endpoint.contains("amazonaws.com") {
                    format!("s3:{endpoint}/{bucket}")
                } else {
                    format!("s3:{endpoint}/{bucket}")
                }
            }
            BackupTarget::Sftp { host, user, path, port } => {
                let p = port.unwrap_or(22);
                format!("sftp:{user}@{host}:{p}/{path}")
            }
            BackupTarget::Rest { url } => format!("rest:{url}"),
            BackupTarget::B2 { bucket, .. } => format!("b2:{bucket}"),
        }
    }

    /// Get environment variables for authentication.
    fn env_vars(&self) -> Vec<(String, String)> {
        match self {
            BackupTarget::S3 { access_key, secret_key, .. } => vec![
                ("AWS_ACCESS_KEY_ID".into(), access_key.clone()),
                ("AWS_SECRET_ACCESS_KEY".into(), secret_key.clone()),
            ],
            BackupTarget::B2 { account_id, account_key, .. } => vec![
                ("B2_ACCOUNT_ID".into(), account_id.clone()),
                ("B2_ACCOUNT_KEY".into(), account_key.clone()),
            ],
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct RetentionPolicy {
    #[serde(default)]
    pub keep_last: Option<u32>,
    #[serde(default)]
    pub keep_daily: Option<u32>,
    #[serde(default)]
    pub keep_weekly: Option<u32>,
    #[serde(default)]
    pub keep_monthly: Option<u32>,
    #[serde(default)]
    pub keep_yearly: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupRunResult {
    pub timestamp: String,
    pub success: bool,
    pub message: String,
    pub duration_secs: u64,
    pub bytes_added: Option<u64>,
    pub files_new: Option<u64>,
    pub files_changed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupSnapshot {
    pub id: String,
    pub time: String,
    pub hostname: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupStatus {
    pub running: bool,
    pub profile_id: Option<String>,
    pub progress: Option<String>,
}

// ── Errors ─────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("profile not found: {0}")]
    NotFound(String),
    #[error("profile already exists: {0}")]
    AlreadyExists(String),
    #[error("backup failed: {0}")]
    Failed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl BackupError {
    pub fn to_rpc_error(&self) -> String {
        self.to_string()
    }
}

// ── Service ────────────────────────────────────────────────────

pub struct BackupService {
    profiles: std::sync::Arc<tokio::sync::Mutex<Vec<BackupProfile>>>,
    running: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
}

impl BackupService {
    pub fn new() -> Self {
        let profiles = load_profiles();
        Self {
            profiles: std::sync::Arc::new(tokio::sync::Mutex::new(profiles)),
            running: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Clone for use in spawned tasks.
    pub fn clone_for_task(&self) -> Self {
        Self {
            profiles: self.profiles.clone(),
            running: self.running.clone(),
        }
    }

    pub async fn list_profiles(&self) -> Vec<BackupProfile> {
        self.profiles.lock().await.clone()
    }

    pub async fn get_profile(&self, id: &str) -> Result<BackupProfile, BackupError> {
        self.profiles.lock().await.iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| BackupError::NotFound(id.into()))
    }

    pub async fn create_profile(&self, mut profile: BackupProfile) -> Result<BackupProfile, BackupError> {
        let mut profiles = self.profiles.lock().await;
        if profiles.iter().any(|p| p.id == profile.id || p.name == profile.name) {
            return Err(BackupError::AlreadyExists(profile.name));
        }
        if profile.id.is_empty() {
            profile.id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        }
        profiles.push(profile.clone());
        save_profiles(&profiles).await;
        info!("Created backup profile '{}' ({})", profile.name, profile.id);
        Ok(profile)
    }

    pub async fn update_profile(&self, id: &str, update: BackupProfile) -> Result<BackupProfile, BackupError> {
        let mut profiles = self.profiles.lock().await;
        let idx = profiles.iter().position(|p| p.id == id)
            .ok_or_else(|| BackupError::NotFound(id.into()))?;
        profiles[idx] = update.clone();
        save_profiles(&profiles).await;
        Ok(update)
    }

    pub async fn delete_profile(&self, id: &str) -> Result<(), BackupError> {
        let mut profiles = self.profiles.lock().await;
        let len = profiles.len();
        profiles.retain(|p| p.id != id);
        if profiles.len() == len {
            return Err(BackupError::NotFound(id.into()));
        }
        save_profiles(&profiles).await;
        info!("Deleted backup profile '{id}'");
        Ok(())
    }

    pub async fn status(&self) -> BackupStatus {
        let running_id = self.running.lock().await.clone();
        BackupStatus {
            running: running_id.is_some(),
            profile_id: running_id,
            progress: None,
        }
    }

    /// Initialize a backup repository at the target.
    pub async fn init_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile(id).await?;
        let repo = profile.target.to_repo_url();
        let env = profile.target.env_vars();

        let mut cmd = Command::new("rustic");
        cmd.args(["--repository", &repo, "--password", &profile.password, "init"]);
        for (k, v) in &env { cmd.env(k, v); }

        let output = cmd.output().await
            .map_err(|e| BackupError::Failed(format!("rustic init: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BackupError::Failed(format!("init failed: {stderr}")));
        }

        // Mark as initialized
        let mut profiles = self.profiles.lock().await;
        if let Some(p) = profiles.iter_mut().find(|p| p.id == id) {
            p.repo_initialized = true;
        }
        save_profiles(&profiles).await;

        info!("Initialized backup repo for profile '{id}' at {repo}");
        Ok("Repository initialized".into())
    }

    /// Run a backup for a profile (blocking — use in background task or WS stream).
    pub async fn run_backup(&self, id: &str) -> Result<BackupRunResult, BackupError> {
        let profile = self.get_profile(id).await?;
        let repo = profile.target.to_repo_url();
        let env = profile.target.env_vars();
        let start = std::time::Instant::now();

        // Set running state
        *self.running.lock().await = Some(id.to_string());

        let mut cmd = Command::new("rustic");
        cmd.args(["--repository", &repo, "--password", &profile.password, "backup"]);
        for src in &profile.sources {
            cmd.arg(src);
        }
        for (k, v) in &env { cmd.env(k, v); }

        let output = cmd.output().await
            .map_err(|e| BackupError::Failed(format!("rustic backup: {e}")))?;

        // Clear running state
        *self.running.lock().await = None;

        let duration = start.elapsed().as_secs();

        let result = if output.status.success() {
            BackupRunResult {
                timestamp: chrono::Utc::now().to_rfc3339(),
                success: true,
                message: "Backup completed successfully".into(),
                duration_secs: duration,
                bytes_added: None, // TODO: parse from rustic output
                files_new: None,
                files_changed: None,
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            BackupRunResult {
                timestamp: chrono::Utc::now().to_rfc3339(),
                success: false,
                message: format!("Backup failed: {stderr}"),
                duration_secs: duration,
                bytes_added: None,
                files_new: None,
                files_changed: None,
            }
        };

        // Save result to profile
        {
            let mut profiles = self.profiles.lock().await;
            if let Some(p) = profiles.iter_mut().find(|p| p.id == id) {
                p.last_run = Some(result.clone());
            }
            save_profiles(&profiles).await;
        }

        if result.success {
            info!("Backup '{}' completed in {}s", profile.name, duration);
            // Auto-prune with retention policy
            if let Err(e) = self.prune(id).await {
                warn!("Auto-prune after backup failed: {e}");
            }
        } else {
            error!("Backup '{}' failed: {}", profile.name, result.message);
        }

        Ok(result)
    }

    /// List snapshots in a profile's repository.
    pub async fn list_snapshots(&self, id: &str) -> Result<Vec<BackupSnapshot>, BackupError> {
        let profile = self.get_profile(id).await?;
        let repo = profile.target.to_repo_url();
        let env = profile.target.env_vars();

        let mut cmd = Command::new("rustic");
        cmd.args(["--repository", &repo, "--password", &profile.password, "snapshots", "--json"]);
        for (k, v) in &env { cmd.env(k, v); }

        let output = cmd.output().await
            .map_err(|e| BackupError::Failed(format!("rustic snapshots: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BackupError::Failed(format!("list snapshots: {stderr}")));
        }

        let snapshots: Vec<BackupSnapshot> = serde_json::from_slice(&output.stdout)
            .unwrap_or_default();
        Ok(snapshots)
    }

    /// Prune old snapshots based on retention policy.
    async fn prune(&self, id: &str) -> Result<(), BackupError> {
        let profile = self.get_profile(id).await?;
        let repo = profile.target.to_repo_url();
        let env = profile.target.env_vars();
        let r = &profile.retention;

        let mut args = vec![
            "--repository".to_string(), repo,
            "--password".to_string(), profile.password.clone(),
            "forget".to_string(), "--prune".to_string(),
        ];

        if let Some(n) = r.keep_last { args.extend(["--keep-last".into(), n.to_string()]); }
        if let Some(n) = r.keep_daily { args.extend(["--keep-daily".into(), n.to_string()]); }
        if let Some(n) = r.keep_weekly { args.extend(["--keep-weekly".into(), n.to_string()]); }
        if let Some(n) = r.keep_monthly { args.extend(["--keep-monthly".into(), n.to_string()]); }
        if let Some(n) = r.keep_yearly { args.extend(["--keep-yearly".into(), n.to_string()]); }

        let mut cmd = Command::new("rustic");
        cmd.args(&args);
        for (k, v) in &env { cmd.env(k, v); }

        let output = cmd.output().await
            .map_err(|e| BackupError::Failed(format!("rustic forget: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BackupError::Failed(format!("prune failed: {stderr}")));
        }

        Ok(())
    }

    /// Check repository integrity.
    pub async fn check_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile(id).await?;
        let repo = profile.target.to_repo_url();
        let env = profile.target.env_vars();

        let mut cmd = Command::new("rustic");
        cmd.args(["--repository", &repo, "--password", &profile.password, "check"]);
        for (k, v) in &env { cmd.env(k, v); }

        let output = cmd.output().await
            .map_err(|e| BackupError::Failed(format!("rustic check: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(BackupError::Failed(format!("check failed: {stderr}")))
        }
    }
}

// ── Persistence ────────────────────────────────────────────────

fn load_profiles() -> Vec<BackupProfile> {
    std::fs::read_to_string(STATE_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

async fn save_profiles(profiles: &[BackupProfile]) {
    if let Ok(json) = serde_json::to_string_pretty(profiles) {
        if let Err(e) = tokio::fs::write(STATE_PATH, json).await {
            error!("Failed to save backup profiles: {e}");
        }
    }
}
