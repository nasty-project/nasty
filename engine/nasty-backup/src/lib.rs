//! Backup system — deduplicating, encrypted backups via rustic_core.
//!
//! Manages backup profiles, scheduling, and execution. Uses rustic_core
//! library directly for backup operations (restic-compatible repo format).

pub mod scheduler;

use nasty_common::secrets::{self, EncryptedBlob, SecretsStatus};
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
    /// Repository password as the operator supplied it. On input, the
    /// engine accepts this field and (when `systemd-creds` is healthy
    /// on this host) encrypts it into `password_encrypted` before
    /// persisting. On output, this field is redacted to `***`. The
    /// field stays as `Option<String>` rather than required so an
    /// older engine downgrading after the migration can still load
    /// the JSON state without a serde error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Repository password encrypted at rest via systemd-creds.
    /// Populated by the engine on create/update when the secrets
    /// backend is available. Resolution prefers this over the legacy
    /// plaintext `password` when both are present (during the migration
    /// window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_encrypted: Option<EncryptedBlob>,
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
        /// Cloud secret key as the operator supplied it. Same shape +
        /// migration story as `BackupProfile.password`: optional on the
        /// wire so encrypted-only state files load cleanly, redacted
        /// on output, expected to be empty once `secret_key_encrypted`
        /// has been populated for this target.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_key: Option<String>,
        /// Cloud secret key encrypted at rest. Set by the engine when
        /// `systemd-creds` is healthy on this host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_key_encrypted: Option<EncryptedBlob>,
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
        /// B2 application key as the operator supplied it. Same shape
        /// + migration story as S3.secret_key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_key: Option<String>,
        /// B2 application key encrypted at rest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_key_encrypted: Option<EncryptedBlob>,
    },
}

/// Plaintext secrets resolved out of a `BackupTarget`'s encrypted /
/// legacy fields, ready to be combined with the target shape to build
/// rustic-compatible backend options. Decryption is async, target
/// construction must run sync inside `spawn_blocking`, so we resolve
/// once outside and pass this struct in.
#[derive(Debug, Default)]
struct ResolvedTargetSecrets {
    s3_secret_key: Option<String>,
    b2_account_key: Option<String>,
}

impl BackupTarget {
    /// Decrypt any encrypted secret fields so the caller can use them
    /// in a sync context. Variants with no secrets (Local, Sftp, Rest)
    /// return an empty `ResolvedTargetSecrets`.
    async fn resolve_secrets(&self, target_id: &str) -> Result<ResolvedTargetSecrets, BackupError> {
        Ok(match self {
            BackupTarget::S3 {
                secret_key,
                secret_key_encrypted,
                ..
            } => ResolvedTargetSecrets {
                s3_secret_key: Some(
                    resolve_secret(
                        &format!("nasty.backup.{target_id}.s3.secret_key"),
                        secret_key.as_deref(),
                        secret_key_encrypted.as_ref(),
                    )
                    .await?,
                ),
                ..Default::default()
            },
            BackupTarget::B2 {
                account_key,
                account_key_encrypted,
                ..
            } => ResolvedTargetSecrets {
                b2_account_key: Some(
                    resolve_secret(
                        &format!("nasty.backup.{target_id}.b2.account_key"),
                        account_key.as_deref(),
                        account_key_encrypted.as_ref(),
                    )
                    .await?,
                ),
                ..Default::default()
            },
            _ => ResolvedTargetSecrets::default(),
        })
    }

    fn to_backend_options(&self, resolved: &ResolvedTargetSecrets) -> BackendOptions {
        match self {
            BackupTarget::Local { path } => BackendOptions::default().repository(path),
            BackupTarget::S3 {
                endpoint,
                bucket,
                access_key,
                region,
                ..
            } => {
                let mut opts = BTreeMap::new();
                opts.insert("bucket".into(), bucket.clone());
                opts.insert("endpoint".into(), endpoint.clone());
                opts.insert("access_key_id".into(), access_key.clone());
                if let Some(secret) = &resolved.s3_secret_key {
                    opts.insert("secret_access_key".into(), secret.clone());
                }
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
                bucket, account_id, ..
            } => {
                let mut opts = BTreeMap::new();
                opts.insert("bucket".into(), bucket.clone());
                opts.insert("account_id".into(), account_id.clone());
                if let Some(key) = &resolved.b2_account_key {
                    opts.insert("account_key".into(), key.clone());
                }
                BackendOptions::default()
                    .repository("opendal:b2")
                    .options(opts)
            }
        }
    }
}

/// Return a sanitized clone of a profile suitable for JSON-RPC
/// responses: plaintext password and cloud secrets are replaced with
/// `"***"` markers when present; encrypted blobs are omitted entirely
/// so callers can tell whether a secret is set without exposing the
/// ciphertext (the blob is useless without the host secret, but
/// echoing it back invites replay-style mistakes).
impl BackupProfile {
    pub fn redacted(&self) -> Self {
        let mut clone = self.clone();
        if clone.password.is_some() {
            clone.password = Some("***".to_string());
        }
        clone.password_encrypted = None;
        clone.target = clone.target.redacted();
        clone
    }
}

impl BackupTarget {
    fn redacted(&self) -> Self {
        match self {
            BackupTarget::S3 {
                endpoint,
                bucket,
                access_key,
                secret_key,
                secret_key_encrypted: _,
                region,
            } => BackupTarget::S3 {
                endpoint: endpoint.clone(),
                bucket: bucket.clone(),
                access_key: access_key.clone(),
                secret_key: secret_key.as_ref().map(|_| "***".to_string()),
                secret_key_encrypted: None,
                region: region.clone(),
            },
            BackupTarget::B2 {
                bucket,
                account_id,
                account_key,
                account_key_encrypted: _,
            } => BackupTarget::B2 {
                bucket: bucket.clone(),
                account_id: account_id.clone(),
                account_key: account_key.as_ref().map(|_| "***".to_string()),
                account_key_encrypted: None,
            },
            other => other.clone(),
        }
    }
}

/// Encrypt the plaintext secret fields on a profile into their
/// `_encrypted` siblings, then blank the plaintext. Idempotent:
/// a profile whose secrets are already encrypted is unchanged.
/// On systemd-creds failure: log loudly, leave the plaintext field
/// in place so the profile remains usable. Better degraded than
/// rejecting the save entirely.
async fn encrypt_profile_secrets_in_place(profile: &mut BackupProfile) {
    if profile.password_encrypted.is_none()
        && let Some(plain) = profile.password.take()
    {
        match secrets::encrypt(&profile_password_name(&profile.id), &plain).await {
            Ok(blob) => {
                profile.password_encrypted = Some(blob);
            }
            Err(e) => {
                warn!(
                    "Failed to encrypt password for backup profile '{}' — keeping plaintext: {e}",
                    profile.id
                );
                profile.password = Some(plain);
            }
        }
    }
    encrypt_target_secrets_in_place(&profile.id, &mut profile.target).await;
}

async fn encrypt_target_secrets_in_place(target_id: &str, target: &mut BackupTarget) {
    match target {
        BackupTarget::S3 {
            secret_key,
            secret_key_encrypted,
            ..
        } => {
            if secret_key_encrypted.is_none()
                && let Some(plain) = secret_key.take()
            {
                let name = format!("nasty.backup.{target_id}.s3.secret_key");
                match secrets::encrypt(&name, &plain).await {
                    Ok(blob) => *secret_key_encrypted = Some(blob),
                    Err(e) => {
                        warn!(
                            "Failed to encrypt S3 secret_key for '{target_id}' — keeping plaintext: {e}"
                        );
                        *secret_key = Some(plain);
                    }
                }
            }
        }
        BackupTarget::B2 {
            account_key,
            account_key_encrypted,
            ..
        } => {
            if account_key_encrypted.is_none()
                && let Some(plain) = account_key.take()
            {
                let name = format!("nasty.backup.{target_id}.b2.account_key");
                match secrets::encrypt(&name, &plain).await {
                    Ok(blob) => *account_key_encrypted = Some(blob),
                    Err(e) => {
                        warn!(
                            "Failed to encrypt B2 account_key for '{target_id}' — keeping plaintext: {e}"
                        );
                        *account_key = Some(plain);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Pull encrypted-blob / legacy-plaintext fields from the existing
/// stored profile into the incoming update when the update doesn't
/// supply them. This lets the operator submit `{name, schedule}`
/// changes without having to re-enter the repository password or
/// cloud credentials every time.
/// Has the target's encrypted-secret field been populated yet?
/// Used by the migration to detect whether `encrypt_target_secrets_in_place`
/// actually changed anything (so the caller can decide whether to
/// re-save the state file).
fn encrypted_target_set(target: &BackupTarget) -> bool {
    matches!(
        target,
        BackupTarget::S3 {
            secret_key_encrypted: Some(_),
            ..
        }
    ) || matches!(
        target,
        BackupTarget::B2 {
            account_key_encrypted: Some(_),
            ..
        }
    )
}

fn carry_forward_existing_secrets(update: &mut BackupProfile, existing: &BackupProfile) {
    if update.password.is_none() && update.password_encrypted.is_none() {
        update.password = existing.password.clone();
        update.password_encrypted = existing.password_encrypted.clone();
    }
    match (&mut update.target, &existing.target) {
        (
            BackupTarget::S3 {
                secret_key,
                secret_key_encrypted,
                ..
            },
            BackupTarget::S3 {
                secret_key: existing_plain,
                secret_key_encrypted: existing_blob,
                ..
            },
        ) if secret_key.is_none() && secret_key_encrypted.is_none() => {
            *secret_key = existing_plain.clone();
            *secret_key_encrypted = existing_blob.clone();
        }
        (
            BackupTarget::B2 {
                account_key,
                account_key_encrypted,
                ..
            },
            BackupTarget::B2 {
                account_key: existing_plain,
                account_key_encrypted: existing_blob,
                ..
            },
        ) if account_key.is_none() && account_key_encrypted.is_none() => {
            *account_key = existing_plain.clone();
            *account_key_encrypted = existing_blob.clone();
        }
        _ => {}
    }
}

/// Resolve a single secret field, preferring the encrypted blob over
/// the legacy plaintext when both are present. Returns an error when
/// neither is set — that means the profile is malformed (operator
/// created it without supplying the field).
async fn resolve_secret(
    name: &str,
    plaintext: Option<&str>,
    encrypted: Option<&EncryptedBlob>,
) -> Result<String, BackupError> {
    if let Some(blob) = encrypted {
        return secrets::decrypt(name, blob)
            .await
            .map_err(|e| BackupError::Failed(format!("decrypt {name}: {e}")));
    }
    if let Some(plain) = plaintext {
        return Ok(plain.to_string());
    }
    Err(BackupError::Failed(format!(
        "secret '{name}' is neither encrypted nor plaintext (profile is missing required field)"
    )))
}

/// Resolve the repository password for a profile. Same precedence as
/// [`resolve_secret`]: encrypted blob beats legacy plaintext.
async fn resolve_profile_password(profile: &BackupProfile) -> Result<String, BackupError> {
    resolve_secret(
        &profile_password_name(&profile.id),
        profile.password.as_deref(),
        profile.password_encrypted.as_ref(),
    )
    .await
}

fn profile_password_name(profile_id: &str) -> String {
    format!("nasty.backup.{profile_id}.password")
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

/// Build a Repository from a profile's target. The caller is expected
/// to have already resolved target-side secrets via
/// `target.resolve_secrets(profile_id).await` and passed them in —
/// rustic's Repository construction is sync, so the decryption has to
/// happen outside this function before `spawn_blocking`.
fn make_repo(
    profile: &BackupProfile,
    resolved: &ResolvedTargetSecrets,
) -> Result<Repository<()>, BackupError> {
    let backends = profile
        .target
        .to_backend_options(resolved)
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
        self.profiles
            .lock()
            .await
            .iter()
            .map(|p| p.redacted())
            .collect()
    }

    pub async fn get_profile(&self, id: &str) -> Result<BackupProfile, BackupError> {
        self.get_profile_internal(id).await.map(|p| p.redacted())
    }

    /// Internal lookup that returns the profile **with secret material
    /// intact** — used by the run/init/check paths that need to
    /// resolve secrets. Callers that surface the profile to a JSON-RPC
    /// caller (the WebUI, external clients) must go through
    /// [`get_profile`] instead so passwords stay redacted on the wire.
    async fn get_profile_internal(&self, id: &str) -> Result<BackupProfile, BackupError> {
        self.profiles
            .lock()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| BackupError::NotFound(id.into()))
    }

    /// Report whether the secrets backend is available on this host
    /// and which backend (`tpm-and-host` / `host-only`) `systemd-creds`
    /// picked. Surfaced in the Backups UI as a status pill so operators
    /// can tell whether new profiles will have their passwords encrypted
    /// at rest. Cheap to call — round-trips a probe blob each time.
    pub async fn secrets_status(&self) -> SecretsStatus {
        secrets::probe().await
    }

    /// Eagerly encrypt any profiles still carrying plaintext secrets
    /// after a state-file load. Called once during engine boot so a
    /// freshly-upgraded box migrates its existing profiles without
    /// waiting for the operator to next edit each one. Safe to call
    /// multiple times — idempotent.
    ///
    /// On a host where `systemd-creds` is unavailable, this is a
    /// no-op (the encrypt call in [`encrypt_profile_secrets_in_place`]
    /// logs a warning and leaves the plaintext field intact). The
    /// engine boot does not fail because of a degraded secrets
    /// backend; backups keep working with plaintext-on-disk until
    /// the operator fixes the underlying issue.
    pub async fn migrate_secrets(&self) {
        let mut profiles = self.profiles.lock().await;
        let mut changed = false;
        for profile in profiles.iter_mut() {
            let before = (
                profile.password_encrypted.is_some(),
                encrypted_target_set(&profile.target),
            );
            encrypt_profile_secrets_in_place(profile).await;
            let after = (
                profile.password_encrypted.is_some(),
                encrypted_target_set(&profile.target),
            );
            if before != after {
                changed = true;
                info!(
                    "Migrated secrets for backup profile '{}' ({})",
                    profile.name, profile.id
                );
            }
        }
        if changed {
            save_profiles(&profiles).await;
        }
    }

    pub async fn create_profile(
        &self,
        mut profile: BackupProfile,
    ) -> Result<BackupProfile, BackupError> {
        // Encrypt plaintext secrets before persisting. If the secrets
        // backend is unavailable on this host (no systemd-creds, broken
        // TPM enrollment, etc.) we keep the legacy plaintext field
        // populated and log a warning — refusing to save the profile
        // would leave the operator unable to configure backups, which
        // is worse than the existing pre-this-PR behavior.
        if profile.id.is_empty() {
            profile.id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        }
        encrypt_profile_secrets_in_place(&mut profile).await;

        let mut profiles = self.profiles.lock().await;
        if profiles
            .iter()
            .any(|p| p.id == profile.id || p.name == profile.name)
        {
            return Err(BackupError::AlreadyExists(profile.name));
        }
        profiles.push(profile.clone());
        save_profiles(&profiles).await;
        info!("Created backup profile '{}' ({})", profile.name, profile.id);
        Ok(profile.redacted())
    }

    pub async fn update_profile(
        &self,
        id: &str,
        mut update: BackupProfile,
    ) -> Result<BackupProfile, BackupError> {
        // Same encryption-on-save invariant as create. The operator
        // can submit a plaintext password (rotate) or omit it (keep
        // existing); we carry the existing encrypted value forward
        // when the update doesn't supply a new one.
        carry_forward_existing_secrets(&mut update, &self.get_profile_internal(id).await?);
        encrypt_profile_secrets_in_place(&mut update).await;

        let mut profiles = self.profiles.lock().await;
        let idx = profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| BackupError::NotFound(id.into()))?;
        profiles[idx] = update.clone();
        save_profiles(&profiles).await;
        Ok(update.redacted())
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
        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.target.resolve_secrets(&profile.id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile, &resolved)?;
            repo.init(
                &creds(&password),
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
        let profile = self.get_profile_internal(id).await?;

        // Auto-init repo if not yet initialized
        if !profile.repo_initialized {
            info!(
                "Auto-initializing backup repo for profile '{}'",
                profile.name
            );
            self.init_repo(id).await?;
        }

        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.target.resolve_secrets(&profile.id).await?;
        let start = std::time::Instant::now();
        *self.running.lock().await = Some(id.to_string());

        let sources = profile.sources.clone();
        let backup_result = tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile, &resolved)?;
            let repo = repo
                .open(&creds(&password))
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
        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.target.resolve_secrets(&profile.id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile, &resolved)?;
            let repo = repo
                .open(&creds(&password))
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
        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.target.resolve_secrets(&profile.id).await?;
        let r = profile.retention.clone();

        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile, &resolved)?;
            let repo = repo
                .open(&creds(&password))
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
        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.target.resolve_secrets(&profile.id).await?;
        tokio::task::spawn_blocking(move || {
            let repo = make_repo(&profile, &resolved)?;
            let repo = repo
                .open(&creds(&password))
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

    /// Construct an `EncryptedBlob` without going through systemd-creds.
    /// The blob isn't decryptable — that's fine for tests that just
    /// need to assert "an encrypted value is present" or that resolve
    /// errors cleanly when the decrypt fails. Goes via the transparent
    /// serde representation so we don't have to expose a public ctor
    /// on EncryptedBlob.
    fn test_blob(payload: &str) -> EncryptedBlob {
        serde_json::from_str(&format!("\"{payload}\"")).expect("transparent string parses")
    }

    fn baseline_profile(target: BackupTarget) -> BackupProfile {
        BackupProfile {
            id: "abc12345".into(),
            name: "test".into(),
            enabled: true,
            sources: vec!["/data".into()],
            target,
            schedule: None,
            retention: RetentionPolicy::default(),
            password: Some("hunter2".into()),
            password_encrypted: None,
            snapshot_before: true,
            repo_initialized: false,
            last_run: None,
        }
    }

    fn s3_with_plaintext(secret: &str, region: Option<&str>) -> BackupTarget {
        BackupTarget::S3 {
            endpoint: "https://s3.example.com".into(),
            bucket: "my-bucket".into(),
            access_key: "AKIA".into(),
            secret_key: Some(secret.into()),
            secret_key_encrypted: None,
            region: region.map(String::from),
        }
    }

    fn b2_with_plaintext(key: &str) -> BackupTarget {
        BackupTarget::B2 {
            bucket: "my-b2".into(),
            account_id: "abc".into(),
            account_key: Some(key.into()),
            account_key_encrypted: None,
        }
    }

    /// Most tests want options derived as if secrets were already
    /// resolved (the live flow does that via `resolve_secrets` in an
    /// async context). For test purposes, build the resolved bundle
    /// directly so we can stay in a sync test fn.
    fn resolve_plaintext_for_test(target: &BackupTarget) -> ResolvedTargetSecrets {
        match target {
            BackupTarget::S3 { secret_key, .. } => ResolvedTargetSecrets {
                s3_secret_key: secret_key.clone(),
                ..Default::default()
            },
            BackupTarget::B2 { account_key, .. } => ResolvedTargetSecrets {
                b2_account_key: account_key.clone(),
                ..Default::default()
            },
            _ => ResolvedTargetSecrets::default(),
        }
    }

    #[test]
    fn backend_options_local_uses_plain_path() {
        let target = BackupTarget::Local {
            path: "/srv/backup".into(),
        };
        let opts = target.to_backend_options(&ResolvedTargetSecrets::default());
        assert_eq!(opts.repository.as_deref(), Some("/srv/backup"));
        // Local has no extra option keys — repository alone is enough.
        assert!(opts.options.is_empty(), "got {:?}", opts.options);
    }

    #[test]
    fn backend_options_s3_carries_credentials_and_endpoint() {
        let target = s3_with_plaintext("secret", Some("eu-west-1"));
        let opts = target.to_backend_options(&resolve_plaintext_for_test(&target));
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
        let target = s3_with_plaintext("s", None);
        let opts = target.to_backend_options(&resolve_plaintext_for_test(&target));
        assert!(
            !opts.options.contains_key("region"),
            "got {:?}",
            opts.options
        );
    }

    #[test]
    fn backend_options_s3_omits_secret_key_when_unresolved() {
        // A profile loaded out of an encrypted state file with a
        // broken systemd-creds backend would arrive here with no
        // plaintext secret available. Better to surface an opendal
        // auth failure than to inject an empty string and confuse
        // the operator.
        let target = BackupTarget::S3 {
            endpoint: "https://s3.example.com".into(),
            bucket: "b".into(),
            access_key: "AKIA".into(),
            secret_key: None,
            secret_key_encrypted: None,
            region: None,
        };
        let opts = target.to_backend_options(&ResolvedTargetSecrets::default());
        assert!(
            !opts.options.contains_key("secret_access_key"),
            "got {:?}",
            opts.options
        );
    }

    #[test]
    fn backend_options_sftp_defaults_port_to_22() {
        let target = BackupTarget::Sftp {
            host: "host.example.com".into(),
            user: "backup".into(),
            path: "/mnt/repo".into(),
            port: None,
        };
        let opts = target.to_backend_options(&ResolvedTargetSecrets::default());
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
        let target = BackupTarget::Sftp {
            host: "host.example.com".into(),
            user: "backup".into(),
            path: "/mnt/repo".into(),
            port: Some(2222),
        };
        let opts = target.to_backend_options(&ResolvedTargetSecrets::default());
        assert_eq!(
            opts.options.get("endpoint").map(String::as_str),
            Some("host.example.com:2222")
        );
    }

    #[test]
    fn backend_options_rest_prefixes_url() {
        // rustic's REST backend wants "rest:<url>" — losing the prefix
        // would silently fall through to opendal and break auth.
        let target = BackupTarget::Rest {
            url: "https://rest.example.com/repo".into(),
        };
        let opts = target.to_backend_options(&ResolvedTargetSecrets::default());
        assert_eq!(
            opts.repository.as_deref(),
            Some("rest:https://rest.example.com/repo")
        );
    }

    #[test]
    fn backend_options_b2_carries_credentials() {
        let target = b2_with_plaintext("def");
        let opts = target.to_backend_options(&resolve_plaintext_for_test(&target));
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
            account_key: Some("z".into()),
            account_key_encrypted: None,
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

    #[test]
    fn redacted_blanks_password_and_cloud_secrets() {
        // Single most important property of redacted(): the JSON
        // returned by backup.profile.list / backup.profile.get NEVER
        // exposes the operator's password or cloud secrets. A regression
        // here is a credential leak through the read API.
        let mut profile = baseline_profile(s3_with_plaintext("s3secret", None));
        // Pretend the migration already ran.
        profile.password_encrypted = Some(test_blob("ENC"));
        let redacted = profile.redacted();
        assert_eq!(redacted.password.as_deref(), Some("***"));
        assert!(
            redacted.password_encrypted.is_none(),
            "encrypted blob must not be echoed back in API responses"
        );
        if let BackupTarget::S3 {
            secret_key,
            secret_key_encrypted,
            ..
        } = redacted.target
        {
            assert_eq!(secret_key.as_deref(), Some("***"));
            assert!(secret_key_encrypted.is_none());
        } else {
            panic!("expected S3 variant");
        }
    }

    #[test]
    fn redacted_preserves_no_password_state() {
        // A profile that genuinely has no plaintext password (e.g.
        // already-encrypted-only) should redact to `None`, not to
        // "***". The WebUI uses None vs Some("***") to decide whether
        // to render the "password is set" badge.
        let mut profile = baseline_profile(BackupTarget::Local {
            path: "/srv".into(),
        });
        profile.password = None;
        let redacted = profile.redacted();
        assert!(redacted.password.is_none());
    }

    #[test]
    fn carry_forward_preserves_existing_password_when_update_silent() {
        // Operator submits an update with no password field (changing
        // schedule, say). The stored password must not be wiped.
        let mut existing = baseline_profile(BackupTarget::Local {
            path: "/srv".into(),
        });
        existing.password = None;
        existing.password_encrypted = Some(test_blob("STORED"));

        let mut update = existing.clone();
        update.password = None;
        update.password_encrypted = None;
        update.name = "renamed".into();

        carry_forward_existing_secrets(&mut update, &existing);
        assert!(update.password_encrypted.is_some());
        assert_eq!(update.name, "renamed");
    }

    #[test]
    fn carry_forward_lets_operator_rotate_password() {
        // When the operator DOES submit a new password (rotation),
        // we must replace — not merge — both fields.
        let mut existing = baseline_profile(BackupTarget::Local {
            path: "/srv".into(),
        });
        existing.password = None;
        existing.password_encrypted = Some(test_blob("OLD"));

        let mut update = existing.clone();
        update.password = Some("new-rotation".into());
        update.password_encrypted = None;

        carry_forward_existing_secrets(&mut update, &existing);
        // New plaintext wins — encrypt_profile_secrets_in_place will
        // seal it before persistence.
        assert_eq!(update.password.as_deref(), Some("new-rotation"));
        assert!(update.password_encrypted.is_none());
    }

    #[test]
    fn resolve_secret_prefers_encrypted_blob_over_plaintext() {
        // During the migration window a profile can carry both fields.
        // Resolution must prefer the encrypted blob — if the encrypted
        // path errors but plaintext still works we'd silently downgrade
        // an operator's security posture. The test runs the resolver
        // with both fields populated; without a real systemd-creds
        // call we can't decrypt, so we assert the error path is
        // exercised (which proves encrypted is preferred over the
        // available plaintext).
        let blob = test_blob("definitely-not-a-real-blob");
        let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
            resolve_secret("test.name", Some("plaintext-fallback"), Some(&blob)).await
        });
        // We expect an error (decrypt failure on a fake blob), NOT the
        // plaintext-fallback string — that would mean we silently
        // returned the legacy value when an encrypted one was present.
        assert!(
            result.is_err(),
            "resolve_secret must try encrypted first; got plaintext fallback {:?}",
            result
        );
    }

    #[test]
    fn redacted_post_migration_profile_is_unusable_for_internal_callers() {
        // Regression pin for the post-migration "neither encrypted nor
        // plaintext" failure observed on 10.10.10.84 after #401 landed:
        // migrate_secrets converted a plaintext password into
        // password_encrypted=Some(blob), leaving password=None. Routing
        // such a profile through redacted() blanks the encrypted blob,
        // so a downstream resolve_profile_password reports the profile
        // as missing both fields.
        //
        // The fix is structural — internal callers (init_repo,
        // run_backup, list_snapshots, prune, check_repo) must use
        // get_profile_internal, not the redacted-for-API get_profile.
        // This test pins the *symptom*: a post-migration profile,
        // round-tripped through redacted(), MUST come out unusable for
        // backup operations. If a future refactor makes redacted()
        // keep secrets, this test fails and forces a conversation
        // about why the JSON-RPC layer is now leaking blobs.
        let mut profile = baseline_profile(BackupTarget::Local {
            path: "/srv".into(),
        });
        profile.password = None;
        profile.password_encrypted = Some(test_blob("ENC"));

        let redacted = profile.redacted();
        assert!(
            redacted.password.is_none(),
            "redacted password leaked: {:?}",
            redacted.password
        );
        assert!(
            redacted.password_encrypted.is_none(),
            "redacted encrypted blob leaked: {:?}",
            redacted.password_encrypted
        );

        // Confirm the symptom: resolve fails on the redacted profile.
        // Callers that hit this error in production are using the
        // wrong getter — fix the caller, don't loosen redacted().
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { resolve_profile_password(&redacted).await });
        assert!(
            result.is_err(),
            "redacted post-migration profile must NOT resolve a password \
             (otherwise internal callers can accidentally backup with '***'); got {:?}",
            result
        );
    }
}
