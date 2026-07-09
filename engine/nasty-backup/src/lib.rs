//! Backup system — deduplicating, encrypted backups via rustic_core.
//!
//! Manages backup profiles, scheduling, and execution. Uses rustic_core
//! library directly for backup operations (restic-compatible repo format).

pub mod jobs;
pub mod restore;
pub mod scheduler;

use jobs::{BackupJob, BackupJobKind, JobError, JobRegistry};
use nasty_common::secrets::{self, EncryptedBlob, SecretsStatus};
use restore::{RestoreProgress, RestoreProgressBars, validate_restore_dest};
use rustic_backend::BackendOptions;
use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, Credentials, ForgetGroups, Grouped, KeepOptions,
    KeyOptions, LocalDestination, LsOptions, PathList, Repository, RepositoryOptions,
    RestoreOptions, SnapshotGroupCriterion, SnapshotOptions, repofile::SnapshotFile,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tracing::{error, info, warn};

const STATE_PATH: &str = "/var/lib/nasty/backups.json";

/// Root that every restore destination must resolve under — bcachefs
/// storage. Mirrors `guestshare.rs`'s FILES_ROOT jail.
const FS_ROOT: &str = "/fs";

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
    /// PEM-encoded CA certificate(s) to trust as an additional root
    /// for this profile's TLS-using target (REST today; S3/B2 with
    /// custom self-signed endpoints come along when we extend opendal
    /// option plumbing). Set when the destination box serves HTTPS
    /// with a Caddy-internal-CA cert (or any self-signed cert) that
    /// isn't in the source box's system trust store — without this,
    /// the connection fails with `unable to get local issuer
    /// certificate`. Validates against the destination's specific
    /// cert (strictly safer than "skip verify": a leaked-but-valid
    /// cert on a different host still gets rejected). Public info,
    /// not encrypted on disk; written into a per-profile cacert file
    /// at runtime that rustic_backend reads via its `cacert` option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_cacert: Option<String>,
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
        /// Bare URL of the rest-server, e.g. `https://nasty.0f.ee:8000`.
        /// Credentials go in the separate username/password fields
        /// below — the WebUI used to make operators inline them as
        /// `https://user:pass@host` userinfo, which leaked the
        /// password in cleartext on every list response.
        url: String,
        /// HTTP basic auth username. The rest-server requires auth as
        /// of v0.0.10 (#408) — empty on legacy unauthenticated
        /// servers is still accepted, in which case no userinfo is
        /// injected at request time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// HTTP basic auth password as the operator supplied it. Same
        /// shape + migration story as S3.secret_key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        /// HTTP basic auth password encrypted at rest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_encrypted: Option<EncryptedBlob>,
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
/// legacy fields, plus any runtime-only path data (e.g. the cacert
/// file we materialize from `profile.trusted_cacert`). Ready to be
/// combined with the target shape to build rustic-compatible backend
/// options. Decryption + cacert file writes are async; target
/// construction must run sync inside `spawn_blocking`, so we resolve
/// once outside and pass this struct in.
#[derive(Debug, Default)]
struct ResolvedTargetSecrets {
    s3_secret_key: Option<String>,
    b2_account_key: Option<String>,
    /// Decrypted HTTP basic-auth password for the rest-server. Empty
    /// when the operator never set credentials (legacy unauthenticated
    /// servers) — `to_backend_options` then leaves the URL alone.
    rest_password: Option<String>,
    /// Absolute path of a PEM file containing the operator-supplied
    /// trusted CA cert (`profile.trusted_cacert`). Materialized by
    /// [`BackupProfile::resolve_runtime`] before the spawn_blocking
    /// boundary so the sync `to_backend_options` only needs to pass
    /// the path through to rustic_backend's `cacert` option.
    cacert_path: Option<String>,
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
            BackupTarget::Rest {
                username,
                password,
                password_encrypted,
                ..
            } => ResolvedTargetSecrets {
                // Username is plaintext on disk (it's a username,
                // not a secret); the password is what we have to
                // decrypt. Both can be absent — a legacy operator
                // with an old unauthenticated rest-server is still
                // a valid configuration.
                rest_password: if username.is_some() {
                    Some(
                        resolve_secret(
                            &format!("nasty.backup.{target_id}.rest.password"),
                            password.as_deref(),
                            password_encrypted.as_ref(),
                        )
                        .await?,
                    )
                } else {
                    None
                },
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
            BackupTarget::Rest { url, username, .. } => {
                // Inject HTTP basic-auth userinfo into the URL when
                // the operator provided credentials. `url::Url` does
                // the percent-encoding for us — copying the
                // username/password into the URL by string
                // concatenation would corrupt any `@`, `:`, or `/`
                // in the password. If the URL is already
                // malformed we fall back to the bare string and let
                // rustic_backend surface the error.
                let repo = match (username.as_deref(), resolved.rest_password.as_deref()) {
                    (Some(user), Some(pass)) if !user.is_empty() => {
                        match url::Url::parse(url) {
                            Ok(mut u) => {
                                // set_username can fail on opaque-host
                                // URLs (mailto:, data:, …) which aren't
                                // valid here anyway; treat as
                                // pass-through.
                                let _ = u.set_username(user);
                                let _ = u.set_password(Some(pass));
                                format!("rest:{}", u)
                            }
                            Err(_) => format!("rest:{url}"),
                        }
                    }
                    _ => format!("rest:{url}"),
                };
                let mut opts = BackendOptions::default().repository(repo);
                // rustic_backend's REST client uses the system trust
                // store by default and doesn't expose a "skip TLS
                // verify" knob (`rest.rs:189-206` only accepts
                // retry/timeout/cacert/tls-client-cert). Operators
                // serving HTTPS with a self-signed cert (or a Caddy
                // internal-CA cert, which is what NASty boxes get out
                // of the box without a public domain) must paste the
                // server's CA cert into profile.trusted_cacert; we
                // materialize it to a file at resolve time and hand
                // the path to rustic via the `cacert` option here.
                if let Some(path) = &resolved.cacert_path {
                    let mut o = BTreeMap::new();
                    o.insert("cacert".into(), path.clone());
                    opts = opts.options(o);
                }
                opts
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
            BackupTarget::Rest {
                url,
                username,
                password,
                password_encrypted: _,
            } => BackupTarget::Rest {
                url: url.clone(),
                username: username.clone(),
                password: password.as_ref().map(|_| "***".to_string()),
                password_encrypted: None,
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
        BackupTarget::Rest {
            password,
            password_encrypted,
            ..
        } => {
            if password_encrypted.is_none()
                && let Some(plain) = password.take()
            {
                let name = format!("nasty.backup.{target_id}.rest.password");
                match secrets::encrypt(&name, &plain).await {
                    Ok(blob) => *password_encrypted = Some(blob),
                    Err(e) => {
                        warn!(
                            "Failed to encrypt rest-server password for '{target_id}' — keeping plaintext: {e}"
                        );
                        *password = Some(plain);
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
    ) || matches!(
        target,
        BackupTarget::Rest {
            password_encrypted: Some(_),
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
        (
            BackupTarget::Rest {
                password,
                password_encrypted,
                ..
            },
            BackupTarget::Rest {
                password: existing_plain,
                password_encrypted: existing_blob,
                ..
            },
        ) if password.is_none() && password_encrypted.is_none() => {
            *password = existing_plain.clone();
            *password_encrypted = existing_blob.clone();
        }
        _ => {}
    }
}

/// Directory holding per-profile cacert PEM files. Files inside are
/// 0644 (public material); the directory is created on demand by
/// [`materialize_cacert`].
const CACERT_DIR: &str = "/var/lib/nasty/cacerts";

impl BackupProfile {
    /// Resolve secrets AND materialize the runtime cacert file (if
    /// the profile has one). Single async entry point that the
    /// init/run/check/snapshots/prune paths call before the
    /// `spawn_blocking` boundary.
    async fn resolve_runtime(&self) -> Result<ResolvedTargetSecrets, BackupError> {
        let mut resolved = self.target.resolve_secrets(&self.id).await?;
        if let Some(pem) = self
            .trusted_cacert
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            resolved.cacert_path = Some(materialize_cacert(&self.id, pem).await?);
        }
        Ok(resolved)
    }
}

/// Write the operator-supplied PEM to `/var/lib/nasty/cacerts/<id>.pem`
/// (atomic via tmp+rename). Returns the path so the caller can pass
/// it as `cacert` to rustic_backend. Idempotent: re-writing the same
/// content is a no-op from rustic's POV.
async fn materialize_cacert(profile_id: &str, pem: &str) -> Result<String, BackupError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(CACERT_DIR)
        .await
        .map_err(BackupError::Io)?;
    let path = format!("{CACERT_DIR}/{profile_id}.pem");
    let tmp = format!("{path}.tmp");
    tokio::fs::write(&tmp, pem.as_bytes())
        .await
        .map_err(BackupError::Io)?;
    // CA certs are public material; 0644 is fine. Stricter perms
    // would just risk a future "rest-server can't read my cacert"
    // bug if we ever drop the engine off root.
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
        .await
        .map_err(BackupError::Io)?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(BackupError::Io)?;
    Ok(path)
}

/// Delete the per-profile cacert file. Called on profile delete so
/// the cacerts directory doesn't accumulate dead files; idempotent
/// (the file may not exist).
async fn drop_cacert(profile_id: &str) {
    let path = format!("{CACERT_DIR}/{profile_id}.pem");
    if let Err(e) = tokio::fs::remove_file(&path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!("backup: failed to remove cacert file {path}: {e}");
    }
}

/// Quick shape check on operator-supplied PEM input. Doesn't fully
/// parse the cert (rustic_backend does that at use time and will
/// fail loudly there); just refuses input that obviously isn't a
/// PEM cert so the operator gets feedback on save rather than at
/// the next backup attempt.
fn validate_pem_cert(pem: &str) -> Result<(), BackupError> {
    let trimmed = pem.trim();
    if trimmed.is_empty() {
        return Err(BackupError::Failed(
            "trusted_cacert: empty input — leave the field unset to remove".to_string(),
        ));
    }
    if !trimmed.contains("-----BEGIN CERTIFICATE-----")
        || !trimmed.contains("-----END CERTIFICATE-----")
    {
        return Err(BackupError::Failed(
            "trusted_cacert: not a PEM certificate — paste the contents of a .pem / .crt file \
             starting with -----BEGIN CERTIFICATE-----"
                .to_string(),
        ));
    }
    Ok(())
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestoreSummary {
    /// Number of files written to the destination.
    pub files_restored: u64,
    /// Total bytes written to the destination.
    pub bytes_restored: u64,
    /// Absolute destination the snapshot was restored into.
    pub dest: String,
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

/// Like [`make_repo`] but builds the repository with a live progress-bar
/// backend so restore-byte progress can be surfaced on the job. Same
/// secret-resolution contract as `make_repo`.
fn make_repo_with_progress(
    profile: &BackupProfile,
    resolved: &ResolvedTargetSecrets,
    progress: RestoreProgress,
) -> Result<Repository<()>, BackupError> {
    let backends = profile
        .target
        .to_backend_options(resolved)
        .to_backends()
        .map_err(|e| BackupError::Failed(format!("backend: {e}")))?;
    let repo_opts = RepositoryOptions::default();
    Repository::new_with_progress(&repo_opts, &backends, RestoreProgressBars(progress))
        .map_err(|e| BackupError::Failed(format!("repo: {e}")))
}

fn creds(password: &str) -> Credentials {
    Credentials::password(password)
}

// ── Service ────────────────────────────────────────────────────

pub struct BackupService {
    profiles: std::sync::Arc<tokio::sync::Mutex<Vec<BackupProfile>>>,
    running: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    /// In-memory registry of long-running backup jobs
    /// (init / run / check). The async start_* methods spawn into
    /// this. See `jobs.rs` for the lifecycle + GC story.
    jobs: JobRegistry,
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
            jobs: JobRegistry::new(),
        }
    }

    pub fn clone_for_task(&self) -> Self {
        Self {
            profiles: self.profiles.clone(),
            running: self.running.clone(),
            jobs: self.jobs.clone(),
        }
    }

    /// Read-only access to the job registry — the router uses this
    /// to expose backup.jobs.list / backup.jobs.get.
    pub fn jobs(&self) -> &JobRegistry {
        &self.jobs
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
        // Validate the trusted CA cert shape before we save. Catches
        // typos / wrong file pastes (e.g. operator pasted the private
        // key by mistake) at create time rather than at the next
        // backup attempt.
        if let Some(pem) = &profile.trusted_cacert {
            validate_pem_cert(pem)?;
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
        if let Some(pem) = &update.trusted_cacert {
            validate_pem_cert(pem)?;
        }
        encrypt_profile_secrets_in_place(&mut update).await;

        let mut profiles = self.profiles.lock().await;
        let idx = profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| BackupError::NotFound(id.into()))?;
        profiles[idx] = update.clone();
        save_profiles(&profiles).await;
        drop(profiles);
        // Operator cleared the textarea ⇒ trusted_cacert is now None
        // ⇒ the materialized PEM at /var/lib/nasty/cacerts/<id>.pem
        // is no longer referenced. Drop it so the cacerts dir stays
        // clean and a stale file can't surprise a future audit. If a
        // new cert is set on a subsequent call, materialize_cacert
        // recreates the file.
        if update.trusted_cacert.is_none() {
            drop_cacert(id).await;
        }
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
        // Drop the per-profile cacert file (if any) so the cacerts
        // directory doesn't accumulate dead files across profile
        // churn. Idempotent — missing file is fine.
        drop_cacert(id).await;
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

    /// Async wrapper around [`init_repo`]: creates a [`BackupJob`],
    /// spawns the actual work, returns the job handle immediately so
    /// the caller can poll. Returns `JobError::AlreadyRunning` if a
    /// non-terminal job for the same profile already exists.
    pub async fn start_init_repo(&self, id: &str) -> Result<BackupJob, JobError> {
        let job = self.jobs.start(id, BackupJobKind::InitRepo).await?;
        let job_id = job.id.clone();
        let profile_id = id.to_string();
        let registry = self.jobs.clone();
        let service = self.clone_for_task();
        tokio::spawn(async move {
            registry.mark_running(&job_id).await;
            match service.init_repo(&profile_id).await {
                Ok(msg) => {
                    registry
                        .mark_succeeded(&job_id, serde_json::Value::String(msg))
                        .await;
                }
                Err(e) => {
                    registry.mark_failed(&job_id, e.to_string()).await;
                }
            }
        });
        Ok(job)
    }

    /// Async wrapper around [`run_backup`]: same shape as
    /// [`start_init_repo`]. The job's `result` payload on success is
    /// the serialized [`BackupRunResult`] (bytes_added, files_new,
    /// etc.) so the WebUI can show what the run actually did.
    pub async fn start_run_backup(&self, id: &str) -> Result<BackupJob, JobError> {
        let job = self.jobs.start(id, BackupJobKind::RunBackup).await?;
        let job_id = job.id.clone();
        let profile_id = id.to_string();
        let registry = self.jobs.clone();
        let service = self.clone_for_task();
        tokio::spawn(async move {
            registry.mark_running(&job_id).await;
            match service.run_backup(&profile_id).await {
                Ok(result) => {
                    let value = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
                    if result.success {
                        registry.mark_succeeded(&job_id, value).await;
                    } else {
                        // run_backup() returns Ok with success=false
                        // when rustic reported a failure. Map that to
                        // a Failed job state so the WebUI's polling
                        // loop renders it consistently with the
                        // "engine returned an error" case.
                        registry.mark_failed(&job_id, result.message.clone()).await;
                    }
                }
                Err(e) => {
                    registry.mark_failed(&job_id, e.to_string()).await;
                }
            }
        });
        Ok(job)
    }

    /// Async wrapper around [`check_repo`].
    pub async fn start_check_repo(&self, id: &str) -> Result<BackupJob, JobError> {
        let job = self.jobs.start(id, BackupJobKind::CheckRepo).await?;
        let job_id = job.id.clone();
        let profile_id = id.to_string();
        let registry = self.jobs.clone();
        let service = self.clone_for_task();
        tokio::spawn(async move {
            registry.mark_running(&job_id).await;
            match service.check_repo(&profile_id).await {
                Ok(msg) => {
                    registry
                        .mark_succeeded(&job_id, serde_json::Value::String(msg))
                        .await;
                }
                Err(e) => {
                    registry.mark_failed(&job_id, e.to_string()).await;
                }
            }
        });
        Ok(job)
    }

    /// Validate the destination under `/fs`, start a `Restore` job, and
    /// spawn the background worker plus a 1 s progress poller. Returns
    /// the `Pending` job immediately (poll `backup.jobs.get`). Dest and
    /// job-collision errors surface synchronously as `BackupError`.
    pub async fn start_restore(
        &self,
        profile_id: &str,
        snapshot_id: &str,
        dest: &str,
        allow_overwrite: bool,
    ) -> Result<BackupJob, BackupError> {
        // Pre-flight: jail + fs-exists + non-empty gate, before any job.
        let resolved_dest = validate_restore_dest(
            std::path::Path::new(dest),
            std::path::Path::new(FS_ROOT),
            allow_overwrite,
        )
        .map_err(|e| BackupError::Failed(e.to_string()))?;

        // Intentionally does NOT set `self.running` — restore concurrency is tracked via the
        // JobRegistry (`self.jobs`) below, so `backup.status()`'s `running` field reflects only backup runs.
        let job = self
            .jobs
            .start(profile_id, BackupJobKind::Restore)
            .await
            .map_err(|e| BackupError::Failed(e.to_string()))?;

        let job_id = job.id.clone();
        let profile_id = profile_id.to_string();
        let snapshot_id = snapshot_id.to_string();
        let registry = self.jobs.clone();
        let service = self.clone_for_task();
        let progress = RestoreProgress::new();
        let poll_progress = progress.clone();

        tokio::spawn(async move {
            registry.mark_running(&job_id).await;

            let work = service.restore_inner(&profile_id, &snapshot_id, resolved_dest, progress);
            tokio::pin!(work);
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.tick().await; // first tick fires immediately; skip it

            loop {
                tokio::select! {
                    res = &mut work => {
                        match res {
                            Ok(summary) => {
                                let value = serde_json::to_value(&summary)
                                    .unwrap_or(serde_json::Value::Null);
                                registry.mark_progress(&job_id, 1.0).await;
                                registry.mark_succeeded(&job_id, value).await;
                            }
                            Err(e) => registry.mark_failed(&job_id, e.to_string()).await,
                        }
                        break;
                    }
                    _ = ticker.tick() => {
                        registry.mark_progress(&job_id, poll_progress.fraction()).await;
                    }
                }
            }
        });

        Ok(job)
    }

    pub async fn init_repo(&self, id: &str) -> Result<String, BackupError> {
        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.resolve_runtime().await?;
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
        let resolved = profile.resolve_runtime().await?;
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
        let resolved = profile.resolve_runtime().await?;
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

    /// Restore a whole snapshot into an already-validated destination.
    /// Opens the repo with a progress backend, resolves the snapshot's
    /// root node, and restores it (merge semantics: create/overwrite,
    /// never delete). Returns a summary of what was written.
    async fn restore_inner(
        &self,
        profile_id: &str,
        snapshot_id: &str,
        dest: std::path::PathBuf,
        progress: RestoreProgress,
    ) -> Result<RestoreSummary, BackupError> {
        let profile = self.get_profile_internal(profile_id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.resolve_runtime().await?;
        let snapshot_id = snapshot_id.to_string();
        let dest_str = dest.to_string_lossy().to_string();

        tokio::task::spawn_blocking(move || {
            let repo = make_repo_with_progress(&profile, &resolved, progress)?;
            let repo = repo
                .open(&creds(&password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            let repo = repo
                .to_indexed()
                .map_err(|e| BackupError::Failed(format!("index: {e}")))?;

            // Resolve the snapshot's root node ("<id>" with no subpath).
            let node = repo
                .node_from_snapshot_path(&snapshot_id, |_| true)
                .map_err(|e| BackupError::Failed(format!("snapshot not found: {e}")))?;

            // Destination directory (create=true, expect_file=false).
            let dest = LocalDestination::new(&dest_str, true, false)
                .map_err(|e| BackupError::Failed(format!("destination: {e}")))?;

            let opts = RestoreOptions::default(); // delete = false

            // prepare_restore consumes a node streamer; restore needs a
            // fresh one, so build the recursive streamer twice.
            let plan = repo
                .prepare_restore(
                    &opts,
                    repo.ls(&node, &LsOptions::default())
                        .map_err(|e| BackupError::Failed(format!("list: {e}")))?,
                    &dest,
                    false,
                )
                .map_err(|e| BackupError::Failed(format!("prepare: {e}")))?;

            let files_restored = plan.stats.files.restore;
            let bytes_restored = plan.restore_size;

            repo.restore(
                plan,
                &opts,
                repo.ls(&node, &LsOptions::default())
                    .map_err(|e| BackupError::Failed(format!("list: {e}")))?,
                &dest,
            )
            .map_err(|e| BackupError::Failed(format!("restore: {e}")))?;

            Ok::<_, BackupError>(RestoreSummary {
                files_restored,
                bytes_restored,
                dest: dest_str,
            })
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))?
    }

    async fn prune(&self, id: &str) -> Result<(), BackupError> {
        let profile = self.get_profile_internal(id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.resolve_runtime().await?;
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
        let resolved = profile.resolve_runtime().await?;
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
            trusted_cacert: None,
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
            username: None,
            password: None,
            password_encrypted: None,
        };
        let opts = target.to_backend_options(&ResolvedTargetSecrets::default());
        assert_eq!(
            opts.repository.as_deref(),
            Some("rest:https://rest.example.com/repo")
        );
    }

    #[test]
    fn backend_options_rest_injects_userinfo_when_creds_set() {
        // When username + decrypted password are both present,
        // to_backend_options inlines them into the URL so rustic's
        // REST client picks them up as HTTP basic auth. The
        // userinfo is the only authn channel rustic_backend's REST
        // client exposes — there's no separate auth option.
        let target = BackupTarget::Rest {
            url: "https://rest.example.com/repo".into(),
            username: Some("nasty-backup".into()),
            password: None,
            password_encrypted: None,
        };
        let resolved = ResolvedTargetSecrets {
            rest_password: Some("hunter2".into()),
            ..Default::default()
        };
        let opts = target.to_backend_options(&resolved);
        assert_eq!(
            opts.repository.as_deref(),
            Some("rest:https://nasty-backup:hunter2@rest.example.com/repo")
        );
    }

    #[test]
    fn backend_options_rest_percent_encodes_password_specials() {
        // Operators paste rest-server passwords directly out of the
        // /services panel — that generator produces alphanumeric
        // only, but operator-supplied passwords from rotate or
        // pre-existing setups can contain `@`, `:`, `/`, `?`, `#`
        // — every one of which is a URL delimiter that would
        // truncate or relocate the authority if injected verbatim.
        // url::Url::set_password is what does the percent-encoding;
        // this test pins that we're actually delegating to it.
        let target = BackupTarget::Rest {
            url: "https://rest.example.com/repo".into(),
            username: Some("user".into()),
            password: None,
            password_encrypted: None,
        };
        let resolved = ResolvedTargetSecrets {
            rest_password: Some("p@ss:wo/rd".into()),
            ..Default::default()
        };
        let opts = target.to_backend_options(&resolved);
        let repo = opts.repository.as_deref().unwrap();
        // Either the percent-encoded form or url's canonical form
        // is fine — what we care about is that the host is still
        // `rest.example.com`, not `ss:wo` or `rd` (which is what
        // happens with naive `format!`).
        let parsed = url::Url::parse(repo.strip_prefix("rest:").unwrap()).unwrap();
        assert_eq!(parsed.host_str(), Some("rest.example.com"));
        assert_eq!(parsed.username(), "user");
        // `Url::password()` returns the percent-encoded form (that's
        // what's on the wire); rest-server's HTTP Basic decoder
        // percent-decodes before the htpasswd lookup. Pinning the
        // encoded form here means a future url-crate revision that
        // changed the encoding alphabet would surface as a test
        // failure rather than as silent auth breakage.
        assert_eq!(parsed.password(), Some("p%40ss%3Awo%2Frd"));
    }

    #[test]
    fn backend_options_rest_unauthenticated_passes_url_through() {
        // A legacy operator with a pre-#408 rest-server still has a
        // bare URL and no credentials; the backend must use it
        // unmodified.
        let target = BackupTarget::Rest {
            url: "https://legacy.example.com/repo".into(),
            username: None,
            password: None,
            password_encrypted: None,
        };
        let resolved = ResolvedTargetSecrets {
            // Decrypted password is None too — resolve_secrets only
            // fills rest_password when username.is_some().
            rest_password: None,
            ..Default::default()
        };
        let opts = target.to_backend_options(&resolved);
        assert_eq!(
            opts.repository.as_deref(),
            Some("rest:https://legacy.example.com/repo")
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

    #[test]
    fn validate_pem_cert_accepts_minimal_pem_shape() {
        // The validator is shape-only — we trust rustic_backend +
        // rustls to do real X.509 parsing at use time. This test
        // pins that a bare-minimum well-formed PEM passes; if a
        // future refactor tightens the check, real operator certs
        // shouldn't start getting rejected.
        let pem = "-----BEGIN CERTIFICATE-----\nMIIBkTCB+w\n-----END CERTIFICATE-----\n";
        assert!(validate_pem_cert(pem).is_ok());
    }

    #[test]
    fn validate_pem_cert_rejects_obvious_mistakes() {
        // Empty / whitespace-only input. The skip_serializing_if=Option::is_none
        // serde attribute means the field comes off the wire as Some("")
        // when the WebUI sends an empty textarea, so we have to handle
        // that distinctly from None.
        assert!(validate_pem_cert("").is_err());
        assert!(validate_pem_cert("   \n   ").is_err());

        // Operator pastes the private key by mistake — common
        // mis-paste when the cert + key live in the same file.
        let key_pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIB\n-----END RSA PRIVATE KEY-----\n";
        assert!(validate_pem_cert(key_pem).is_err());

        // Operator pastes the URL of the cert instead of its contents.
        assert!(validate_pem_cert("https://example.com/ca.crt").is_err());

        // Partial paste — only the BEGIN line.
        assert!(validate_pem_cert("-----BEGIN CERTIFICATE-----\nMIIBkTCB+w").is_err());
    }
}
