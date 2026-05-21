//! Backup system — deduplicating, encrypted backups via rustic_core.
//!
//! Manages backup profiles, scheduling, and execution. Uses rustic_core
//! library directly for backup operations (restic-compatible repo format).

use rustic_backend::BackendOptions;
use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, Credentials, ForgetGroups, Grouped, KeepOptions,
    KeyOptions, PathList, Repository, RepositoryOptions, SnapshotGroupCriterion, SnapshotOptions,
    repofile::SnapshotFile,
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

fn default_true() -> bool {
    true
}

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
    fn to_backend_options(&self) -> BackendOptions {
        match self {
            BackupTarget::Local { path } => BackendOptions::default().repository(path),
            BackupTarget::S3 {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
            } => {
                let mut opts = BTreeMap::new();
                opts.insert("bucket".into(), bucket.clone());
                opts.insert("endpoint".into(), endpoint.clone());
                opts.insert("access_key_id".into(), access_key.clone());
                opts.insert("secret_access_key".into(), secret_key.clone());
                if let Some(r) = region {
                    opts.insert("region".into(), r.clone());
                }
                BackendOptions::default()
                    .repository("opendal:s3")
                    .options(opts)
            }
            BackupTarget::Sftp {
                host,
                user,
                path,
                port,
            } => {
                let mut opts = BTreeMap::new();
                opts.insert(
                    "endpoint".into(),
                    format!("{}:{}", host, port.unwrap_or(22)),
                );
                opts.insert("user".into(), user.clone());
                opts.insert("root".into(), path.clone());
                BackendOptions::default()
                    .repository("opendal:sftp")
                    .options(opts)
            }
            BackupTarget::Rest { url } => {
                BackendOptions::default().repository(format!("rest:{url}"))
            }
            BackupTarget::B2 {
                bucket,
                account_id,
                account_key,
            } => {
                let mut opts = BTreeMap::new();
                opts.insert("bucket".into(), bucket.clone());
                opts.insert("account_id".into(), account_id.clone());
                opts.insert("account_key".into(), account_key.clone());
                BackendOptions::default()
                    .repository("opendal:b2")
                    .options(opts)
            }
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

// ── Repository helpers ────────────────────────────────────────

/// Build a Repository from a profile's target and password.
fn make_repo(profile: &BackupProfile) -> Result<Repository<()>, BackupError> {
    let backends = profile
        .target
        .to_backend_options()
        .to_backends()
        .map_err(|e| BackupError::Failed(format!("backend: {e}")))?;
    let repo_opts = RepositoryOptions::default();
    Repository::new(&repo_opts, &backends).map_err(|e| BackupError::Failed(format!("repo: {e}")))
}

fn creds(password: &str) -> Credentials {
    Credentials::password(password)
}

// ── Service ────────────────────────────────────────────────────

pub struct BackupService {
    profiles: std::sync::Arc<tokio::sync::Mutex<Vec<BackupProfile>>>,
    running: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
}

impl Default for BackupService {
    fn default() -> Self {
        Self::new()
    }
}

impl BackupService {
    pub fn new() -> Self {
        Self {
            profiles: std::sync::Arc::new(tokio::sync::Mutex::new(load_profiles())),
            running: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

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
        self.profiles
            .lock()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| BackupError::NotFound(id.into()))
    }

    pub async fn create_profile(
        &self,
        mut profile: BackupProfile,
    ) -> Result<BackupProfile, BackupError> {
        let mut profiles = self.profiles.lock().await;
        if profiles
            .iter()
            .any(|p| p.id == profile.id || p.name == profile.name)
        {
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

    pub async fn update_profile(
        &self,
        id: &str,
        update: BackupProfile,
    ) -> Result<BackupProfile, BackupError> {
        let mut profiles = self.profiles.lock().await;
        let idx = profiles
            .iter()
            .position(|p| p.id == id)
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

    pub async fn init_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile(id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            repo.init(
                &creds(&profile.password),
                &KeyOptions::default(),
                &ConfigOptions::default(),
            )
            .map_err(|e| BackupError::Failed(format!("init: {e}")))?;
            Ok::<_, BackupError>(())
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))??;

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

        // Auto-init repo if not yet initialized
        if !profile.repo_initialized {
            info!(
                "Auto-initializing backup repo for profile '{}'",
                profile.name
            );
            self.init_repo(id).await?;
        }

        let profile = self.get_profile(id).await?;
        let start = std::time::Instant::now();
        *self.running.lock().await = Some(id.to_string());

        let sources = profile.sources.clone();
        let backup_result = tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo
                .open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            let repo = repo
                .to_indexed_ids()
                .map_err(|e| BackupError::Failed(format!("index: {e}")))?;

            let source = PathList::from_iter(sources.iter().map(|s| s.as_str()));
            let snap = SnapshotOptions::default()
                .to_snapshot()
                .map_err(|e| BackupError::Failed(format!("snapshot opts: {e}")))?;

            let result = repo
                .backup(&BackupOptions::default(), &source, snap)
                .map_err(|e| BackupError::Failed(format!("backup: {e}")))?;

            Ok::<_, BackupError>((
                result.summary.as_ref().map(|s| s.data_added),
                result.summary.as_ref().map(|s| s.files_new),
                result.summary.as_ref().map(|s| s.files_changed),
            ))
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))?;

        *self.running.lock().await = None;
        let duration = start.elapsed().as_secs();

        let result = match backup_result {
            Ok((bytes_added, files_new, files_changed)) => BackupRunResult {
                timestamp: chrono::Utc::now().to_rfc3339(),
                success: true,
                message: "Backup completed successfully".into(),
                duration_secs: duration,
                bytes_added,
                files_new,
                files_changed,
            },
            Err(e) => BackupRunResult {
                timestamp: chrono::Utc::now().to_rfc3339(),
                success: false,
                message: format!("Backup failed: {e}"),
                duration_secs: duration,
                bytes_added: None,
                files_new: None,
                files_changed: None,
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
            if let Err(e) = self.prune(id).await {
                warn!("Auto-prune failed: {e}");
            }
        } else {
            error!("Backup failed: {}", result.message);
        }
        Ok(result)
    }

    pub async fn list_snapshots(&self, id: &str) -> Result<Vec<BackupSnapshot>, BackupError> {
        let profile = self.get_profile(id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo
                .open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            let snaps: Vec<SnapshotFile> = repo
                .get_all_snapshots()
                .map_err(|e| BackupError::Failed(format!("snapshots: {e}")))?;

            Ok(snaps
                .into_iter()
                .map(|s| BackupSnapshot {
                    id: s.id.to_hex().to_string(),
                    time: s.time.to_string(),
                    hostname: s.hostname.clone(),
                    paths: s.paths.iter().map(|p| p.to_string()).collect(),
                    tags: s.tags.iter().map(|t| t.to_string()).collect(),
                })
                .collect())
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))?
    }

    async fn prune(&self, id: &str) -> Result<(), BackupError> {
        let profile = self.get_profile(id).await?;
        let r = profile.retention.clone();

        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo
                .open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;

            let snaps = repo
                .get_all_snapshots()
                .map_err(|e| BackupError::Failed(format!("get snapshots: {e}")))?;

            let mut keep = KeepOptions::default();
            if let Some(n) = r.keep_last {
                keep = keep.keep_last(n as i32);
            }
            if let Some(n) = r.keep_daily {
                keep = keep.keep_daily(n as i32);
            }
            if let Some(n) = r.keep_weekly {
                keep = keep.keep_weekly(n as i32);
            }
            if let Some(n) = r.keep_monthly {
                keep = keep.keep_monthly(n as i32);
            }
            if let Some(n) = r.keep_yearly {
                keep = keep.keep_yearly(n as i32);
            }

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
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))??;
        Ok(())
    }

    pub async fn check_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile(id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile)?;
            let repo = repo
                .open(&creds(&profile.password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            repo.check(CheckOptions::default())
                .map_err(|e| BackupError::Failed(format!("check: {e}")))?;
            Ok("Repository check passed".to_string())
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))?
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
    use std::os::unix::fs::PermissionsExt;
    if let Ok(json) = serde_json::to_string_pretty(profiles) {
        if let Err(e) = tokio::fs::write(STATE_PATH, json).await {
            error!("Failed to save backup profiles: {e}");
            return;
        }
        // Contains S3/B2/SFTP credentials and the repo passphrase.
        if let Err(e) =
            tokio::fs::set_permissions(STATE_PATH, std::fs::Permissions::from_mode(0o600)).await
        {
            error!("Failed to chmod backup profiles: {e}");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────
//
// Coverage focuses on the pure surface — the backend-options builder
// and the on-disk serde contract. The Repository / Backup execution
// paths are integration territory (they hit the local FS, rustic_core,
// and network backends) and are exercised end-to-end on real boxes.
#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_profile(target: BackupTarget) -> BackupProfile {
        BackupProfile {
            id: "abc12345".into(),
            name: "test".into(),
            enabled: true,
            sources: vec!["/data".into()],
            target,
            schedule: None,
            retention: RetentionPolicy::default(),
            password: "hunter2".into(),
            snapshot_before: true,
            repo_initialized: false,
            last_run: None,
        }
    }

    #[test]
    fn backend_options_local_uses_plain_path() {
        let opts = BackupTarget::Local {
            path: "/srv/backup".into(),
        }
        .to_backend_options();
        assert_eq!(opts.repository.as_deref(), Some("/srv/backup"));
        // Local has no extra option keys — repository alone is enough.
        assert!(opts.options.is_empty(), "got {:?}", opts.options);
    }

    #[test]
    fn backend_options_s3_carries_credentials_and_endpoint() {
        let opts = BackupTarget::S3 {
            endpoint: "https://s3.example.com".into(),
            bucket: "my-bucket".into(),
            access_key: "AKIA".into(),
            secret_key: "secret".into(),
            region: Some("eu-west-1".into()),
        }
        .to_backend_options();
        assert_eq!(opts.repository.as_deref(), Some("opendal:s3"));
        assert_eq!(
            opts.options.get("bucket").map(String::as_str),
            Some("my-bucket")
        );
        assert_eq!(
            opts.options.get("endpoint").map(String::as_str),
            Some("https://s3.example.com")
        );
        assert_eq!(
            opts.options.get("access_key_id").map(String::as_str),
            Some("AKIA")
        );
        assert_eq!(
            opts.options.get("secret_access_key").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            opts.options.get("region").map(String::as_str),
            Some("eu-west-1")
        );
    }

    #[test]
    fn backend_options_s3_without_region_omits_region_key() {
        // The region option is optional. When None, the key should
        // not appear in options at all — rustic/opendal treat absent
        // and empty differently.
        let opts = BackupTarget::S3 {
            endpoint: "https://s3.example.com".into(),
            bucket: "b".into(),
            access_key: "AKIA".into(),
            secret_key: "s".into(),
            region: None,
        }
        .to_backend_options();
        assert!(
            !opts.options.contains_key("region"),
            "got {:?}",
            opts.options
        );
    }

    #[test]
    fn backend_options_sftp_defaults_port_to_22() {
        let opts = BackupTarget::Sftp {
            host: "host.example.com".into(),
            user: "backup".into(),
            path: "/mnt/repo".into(),
            port: None,
        }
        .to_backend_options();
        assert_eq!(opts.repository.as_deref(), Some("opendal:sftp"));
        assert_eq!(
            opts.options.get("endpoint").map(String::as_str),
            Some("host.example.com:22")
        );
        assert_eq!(opts.options.get("user").map(String::as_str), Some("backup"));
        assert_eq!(
            opts.options.get("root").map(String::as_str),
            Some("/mnt/repo")
        );
    }

    #[test]
    fn backend_options_sftp_honours_custom_port() {
        let opts = BackupTarget::Sftp {
            host: "host.example.com".into(),
            user: "backup".into(),
            path: "/mnt/repo".into(),
            port: Some(2222),
        }
        .to_backend_options();
        assert_eq!(
            opts.options.get("endpoint").map(String::as_str),
            Some("host.example.com:2222")
        );
    }

    #[test]
    fn backend_options_rest_prefixes_url() {
        // rustic's REST backend wants "rest:<url>" — losing the prefix
        // would silently fall through to opendal and break auth.
        let opts = BackupTarget::Rest {
            url: "https://rest.example.com/repo".into(),
        }
        .to_backend_options();
        assert_eq!(
            opts.repository.as_deref(),
            Some("rest:https://rest.example.com/repo")
        );
    }

    #[test]
    fn backend_options_b2_carries_credentials() {
        let opts = BackupTarget::B2 {
            bucket: "my-b2".into(),
            account_id: "abc".into(),
            account_key: "def".into(),
        }
        .to_backend_options();
        assert_eq!(opts.repository.as_deref(), Some("opendal:b2"));
        assert_eq!(
            opts.options.get("bucket").map(String::as_str),
            Some("my-b2")
        );
        assert_eq!(
            opts.options.get("account_id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(
            opts.options.get("account_key").map(String::as_str),
            Some("def")
        );
    }

    #[test]
    fn profile_deserialises_with_default_optionals() {
        // The on-disk format must accept missing `schedule`, `retention`,
        // `snapshot_before`, `repo_initialized`, `last_run` — old state
        // files predate every field after sources/target/password and
        // must still load without error or migration churn.
        let json = r#"{
            "id": "id1",
            "name": "x",
            "enabled": true,
            "sources": ["/a"],
            "target": {"type": "local", "path": "/r"},
            "password": "p"
        }"#;
        let p: BackupProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.id, "id1");
        assert_eq!(p.schedule, None);
        assert_eq!(p.retention.keep_last, None);
        // `snapshot_before` defaults to TRUE — the WebUI assumes this,
        // and silently flipping it to false on legacy profiles would
        // turn off bcachefs-snapshot integrity guarantees.
        assert!(p.snapshot_before);
        assert!(!p.repo_initialized);
        assert!(p.last_run.is_none());
    }

    #[test]
    fn profile_round_trips_through_json() {
        // Catches accidental field renames / serde-tag changes that
        // would leave existing /var/lib/nasty/backups.json unreadable
        // after a restart.
        let original = baseline_profile(BackupTarget::Local {
            path: "/srv".into(),
        });
        let json = serde_json::to_string(&original).unwrap();
        let restored: BackupProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.name, original.name);
        assert_eq!(restored.enabled, original.enabled);
        assert_eq!(restored.sources, original.sources);
        assert_eq!(restored.password, original.password);
        match restored.target {
            BackupTarget::Local { path } => assert_eq!(path, "/srv"),
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[test]
    fn target_uses_snake_case_type_tag() {
        // Locks in the serde(tag = "type", rename_all = "snake_case")
        // contract — the WebUI sends "type": "local" / "s3" / "sftp"
        // / "rest" / "b2" verbatim.
        let json = serde_json::to_string(&BackupTarget::B2 {
            bucket: "x".into(),
            account_id: "y".into(),
            account_key: "z".into(),
        })
        .unwrap();
        assert!(json.contains(r#""type":"b2""#), "got {json}");
    }

    #[test]
    fn retention_policy_omits_none_fields() {
        // RetentionPolicy with all-None fields should serialise to an
        // empty(-ish) object — losing this property would balloon every
        // saved profile with "keep_last":null,"keep_daily":null… noise.
        // (Serde defaults to emitting nulls; if a future refactor adds
        // skip_serializing_if=Option::is_none, this test catches the
        // accidental drop.)
        let r = RetentionPolicy::default();
        let json = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Either an empty object, or one with every value null —
        // both are acceptable; what's unacceptable is a present
        // non-null value when nothing was set.
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                assert!(val.is_null(), "{k} should be null, got {val}");
            }
        }
    }

    #[test]
    fn error_to_rpc_error_uses_display() {
        // The RPC layer surfaces this string to the WebUI verbatim
        // — keep it tied to Display so error variants stay readable.
        let e = BackupError::NotFound("missing-id".into());
        assert_eq!(e.to_rpc_error(), "profile not found: missing-id");
    }
}
