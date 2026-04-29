//! Backup system — deduplicating, encrypted backups via rustic_core.
//!
//! Manages backup profiles, scheduling, and execution. Uses rustic_core
//! library directly for backup operations (restic-compatible repo format).

use rustic_backend::BackendOptions;
use rustic_core::{
    repofile::SnapshotFile,
    BackupOptions, CheckOptions, ConfigOptions, Credentials, ForgetGroups, Grouped,
    KeyOptions, KeepOptions, PathList, Repository, RepositoryOptions,
    SnapshotGroupCriterion, SnapshotOptions,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tracing::{error, info, warn};

const STATE_PATH: &str = "/var/lib/nasty/backups.json";

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupProfile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub sources: Vec<String>,
    pub target: BackupTarget,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub retention: RetentionPolicy,
    pub password: String,
    #[serde(default = "default_true")]
    pub snapshot_before: bool,
    #[serde(default)]
    pub repo_initialized: bool,
    #[serde(default)]
    pub last_run: Option<BackupRunResult>,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackupTarget {
    Local { path: String },
    S3 {
        endpoint: String, bucket: String,
        access_key: String, secret_key: String,
        #[serde(default)] region: Option<String>,
    },
    Sftp {
        host: String, user: String, path: String,
        #[serde(default)] port: Option<u16>,
    },
    Rest { url: String },
    B2 { bucket: String, account_id: String, account_key: String },
}

impl BackupTarget {
    fn to_backend_options(&self) -> BackendOptions {
        match self {
            BackupTarget::Local { path } => {
                BackendOptions::default().repository(path)
            }
            BackupTarget::S3 { endpoint, bucket, access_key, secret_key, region } => {
                let mut opts = BTreeMap::new();
                opts.insert("bucket".into(), bucket.clone());
                opts.insert("endpoint".into(), endpoint.clone());
                opts.insert("access_key_id".into(), access_key.clone());
                opts.insert("secret_access_key".into(), secret_key.clone());
                if let Some(r) = region { opts.insert("region".into(), r.clone()); }
                BackendOptions::default().repository("opendal:s3").options(opts)
            }
            BackupTarget::Sftp { host, user, path, port } => {
                let mut opts = BTreeMap::new();
                opts.insert("endpoint".into(), format!("{}:{}", host, port.unwrap_or(22)));
                opts.insert("user".into(), user.clone());
                opts.insert("root".into(), path.clone());
                BackendOptions::default().repository("opendal:sftp").options(opts)
            }
            BackupTarget::Rest { url } => {
                BackendOptions::default().repository(&format!("rest:{url}"))
            }
            BackupTarget::B2 { bucket, account_id, account_key } => {
                let mut opts = BTreeMap::new();
                opts.insert("bucket".into(), bucket.clone());
                opts.insert("account_id".into(), account_id.clone());
                opts.insert("account_key".into(), account_key.clone());
                BackendOptions::default().repository("opendal:b2").options(opts)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct RetentionPolicy {
    #[serde(default)] pub keep_last: Option<u32>,
    #[serde(default)] pub keep_daily: Option<u32>,
    #[serde(default)] pub keep_weekly: Option<u32>,
    #[serde(default)] pub keep_monthly: Option<u32>,
    #[serde(default)] pub keep_yearly: Option<u32>,
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
    pub fn to_rpc_error(&self) -> String { self.to_string() }
}

// ── Repository helpers ────────────────────────────────────────

/// Build a Repository from a profile's target and password.
fn make_repo(profile: &BackupProfile) -> Result<Repository<()>, BackupError> {
    let backends = profile.target.to_backend_options().to_backends()
        .map_err(|e| BackupError::Failed(format!("backend: {e}")))?;
    let repo_opts = RepositoryOptions::default();
    Repository::new(&repo_opts, &backends)
        .map_err(|e| BackupError::Failed(format!("repo: {e}")))
}

fn creds(password: &str) -> Credentials {
    Credentials::password(password)
}

// ── Service ────────────────────────────────────────────────────

pub struct BackupService {
    profiles: std::sync::Arc<tokio::sync::Mutex<Vec<BackupProfile>>>,
    running: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
}

impl BackupService {
    pub fn new() -> Self {
        Self {
            profiles: std::sync::Arc::new(tokio::sync::Mutex::new(load_profiles())),
            running: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn clone_for_task(&self) -> Self {
        Self { profiles: self.profiles.clone(), running: self.running.clone() }
    }

    pub async fn list_profiles(&self) -> Vec<BackupProfile> {
        self.profiles.lock().await.clone()
    }

    pub async fn get_profile(&self, id: &str) -> Result<BackupProfile, BackupError> {
        self.profiles.lock().await.iter()
            .find(|p| p.id == id).cloned()
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
        if profiles.len() == len { return Err(BackupError::NotFound(id.into())); }
        save_profiles(&profiles).await;
        info!("Deleted backup profile '{id}'");
        Ok(())
    }

    pub async fn status(&self) -> BackupStatus {
        let running_id = self.running.lock().await.clone();
        BackupStatus { running: running_id.is_some(), profile_id: running_id, progress: None }
    }

    pub async fn init_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile(id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            repo.init(&creds(&profile.password), &KeyOptions::default(), &ConfigOptions::default())
                .map_err(|e| BackupError::Failed(format!("init: {e}")))?;
            Ok::<_, BackupError>(())
        }).await.map_err(|e| BackupError::Failed(format!("spawn: {e}")))??;

        let mut profiles = self.profiles.lock().await;
        if let Some(p) = profiles.iter_mut().find(|p| p.id == id) {
            p.repo_initialized = true;
        }
        save_profiles(&profiles).await;
        info!("Initialized backup repo for profile '{id}'");
        Ok("Repository initialized".into())
    }

    pub async fn run_backup(&self, id: &str) -> Result<BackupRunResult, BackupError> {
        let profile = self.get_profile(id).await?;
        let start = std::time::Instant::now();
        *self.running.lock().await = Some(id.to_string());

        let sources = profile.sources.clone();
        let backup_result = tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo.open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            let repo = repo.to_indexed_ids()
                .map_err(|e| BackupError::Failed(format!("index: {e}")))?;

            let source = PathList::from_iter(sources.iter().map(|s| s.as_str()));
            let snap = SnapshotOptions::default().to_snapshot()
                .map_err(|e| BackupError::Failed(format!("snapshot opts: {e}")))?;

            let result = repo.backup(&BackupOptions::default(), &source, snap)
                .map_err(|e| BackupError::Failed(format!("backup: {e}")))?;

            Ok::<_, BackupError>((
                result.summary.as_ref().map(|s| s.data_added),
                result.summary.as_ref().map(|s| s.files_new),
                result.summary.as_ref().map(|s| s.files_changed),
            ))
        }).await.map_err(|e| BackupError::Failed(format!("spawn: {e}")))?;

        *self.running.lock().await = None;
        let duration = start.elapsed().as_secs();

        let result = match backup_result {
            Ok((bytes_added, files_new, files_changed)) => BackupRunResult {
                timestamp: chrono::Utc::now().to_rfc3339(),
                success: true,
                message: "Backup completed successfully".into(),
                duration_secs: duration,
                bytes_added, files_new, files_changed,
            },
            Err(e) => BackupRunResult {
                timestamp: chrono::Utc::now().to_rfc3339(),
                success: false,
                message: format!("Backup failed: {e}"),
                duration_secs: duration,
                bytes_added: None, files_new: None, files_changed: None,
            },
        };

        {
            let mut profiles = self.profiles.lock().await;
            if let Some(p) = profiles.iter_mut().find(|p| p.id == id) {
                p.last_run = Some(result.clone());
            }
            save_profiles(&profiles).await;
        }

        if result.success {
            info!("Backup completed in {}s", duration);
            if let Err(e) = self.prune(id).await { warn!("Auto-prune failed: {e}"); }
        } else {
            error!("Backup failed: {}", result.message);
        }
        Ok(result)
    }

    pub async fn list_snapshots(&self, id: &str) -> Result<Vec<BackupSnapshot>, BackupError> {
        let profile = self.get_profile(id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo.open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            let snaps: Vec<SnapshotFile> = repo.get_all_snapshots()
                .map_err(|e| BackupError::Failed(format!("snapshots: {e}")))?;

            Ok(snaps.into_iter().map(|s| BackupSnapshot {
                id: s.id.to_hex().to_string(),
                time: s.time.to_string(),
                hostname: s.hostname.clone(),
                paths: s.paths.iter().map(|p| p.to_string()).collect(),
                tags: s.tags.iter().map(|t| t.to_string()).collect(),
            }).collect())
        }).await.map_err(|e| BackupError::Failed(format!("spawn: {e}")))?
    }

    async fn prune(&self, id: &str) -> Result<(), BackupError> {
        let profile = self.get_profile(id).await?;
        let r = profile.retention.clone();

        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo.open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;

            let snaps = repo.get_all_snapshots()
                .map_err(|e| BackupError::Failed(format!("get snapshots: {e}")))?;

            let mut keep = KeepOptions::default();
            if let Some(n) = r.keep_last { keep = keep.keep_last(n as i32); }
            if let Some(n) = r.keep_daily { keep = keep.keep_daily(n as i32); }
            if let Some(n) = r.keep_weekly { keep = keep.keep_weekly(n as i32); }
            if let Some(n) = r.keep_monthly { keep = keep.keep_monthly(n as i32); }
            if let Some(n) = r.keep_yearly { keep = keep.keep_yearly(n as i32); }

            let criterion = SnapshotGroupCriterion::default();
            let grouped = Grouped::from_items(snaps, criterion);
            let now = jiff::Zoned::now();
            let forget = ForgetGroups::from_grouped_snapshots_with_retention(grouped, &keep, &now)
                .map_err(|e| BackupError::Failed(format!("forget plan: {e}")))?;

            let ids = forget.into_forget_ids();
            if !ids.is_empty() {
                info!("Pruning {} snapshot(s)", ids.len());
                repo.delete_snapshots(&ids)
                    .map_err(|e| BackupError::Failed(format!("delete: {e}")))?;
            }
            Ok::<_, BackupError>(())
        }).await.map_err(|e| BackupError::Failed(format!("spawn: {e}")))??;
        Ok(())
    }

    pub async fn check_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile(id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo.open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            repo.check(CheckOptions::default())
                .map_err(|e| BackupError::Failed(format!("check: {e}")))?;
            Ok("Repository check passed".to_string())
        }).await.map_err(|e| BackupError::Failed(format!("spawn: {e}")))?
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
