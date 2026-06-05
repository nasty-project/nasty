//! Restic REST server credentials.
//!
//! The on-box backup receiver (`restic-rest-server`) used to run with
//! `--no-auth`, which meant anyone who could reach port 8000 could
//! not just push backups but also *delete* existing repos. This
//! module owns the basic-auth credentials that gate write access.
//!
//! State lives in two files under `/var/lib/nasty/`:
//!
//!   * `rest-server-credentials.json` — `{ username, password_encrypted }`.
//!     Password sealed via `nasty-common::secrets` (systemd-creds),
//!     so a leaked state file doesn't expose the plaintext.
//!   * `rest-server.htpasswd` — the bcrypted Apache-format htpasswd
//!     file the systemd service hands to `rest-server --htpasswd-file`.
//!     Regenerated from the plaintext at every credential rotation.
//!
//! Engine guarantees credentials exist before the systemd service is
//! ever started — see `prepare_protocol(Protocol::RestServer)` in
//! `protocol.rs`. The service is configured to require auth (no
//! `--no-auth` fallback), so a missing or unreadable htpasswd file
//! fails the service start loudly instead of silently allowing
//! anonymous writes.
//!
//! ## Failure mode
//!
//! If systemd-creds can't decrypt the stored password (TPM cleared,
//! host secret rotated, hardware swap), the WebUI's "show credentials"
//! call returns an error and the operator clicks "Rotate" to generate
//! fresh creds. Backup *data* on the rest-server is encrypted by
//! rustic itself with the per-profile repository password, which
//! lives on the SOURCE side — so credential rotation doesn't risk
//! data loss, just requires updating the URL on every source profile
//! pointing at this rest-server.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nasty_common::secrets::{self, EncryptedBlob, SecretError};
use rand::distr::{Alphanumeric, SampleString};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/rest-server-credentials.json";
const HTPASSWD_PATH: &str = "/var/lib/nasty/rest-server.htpasswd";
const DEFAULT_USERNAME: &str = "nasty-backup";
const PASSWORD_LEN: usize = 32;
const SECRETS_NAME: &str = "nasty.rest_server.password";

/// Single-flight lock so two concurrent "ensure exists" calls don't
/// race and write the file twice (with different passwords). The
/// state file write itself is atomic via temp+rename, but the
/// generate-then-write sequence isn't.
static GUARD: Mutex<()> = Mutex::const_new(());

/// Persisted credential record. Username is plaintext (it's not
/// secret; it's part of the URL operators paste into source-side
/// profiles); the password lives as an encrypted blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    username: String,
    password_encrypted: EncryptedBlob,
}

/// Decrypted credential pair surfaced to the WebUI. Carry-by-value,
/// drop ASAP — never logged. The `url_template` field is rendered
/// client-side from these + the operator's hostname; we don't ship
/// it from the engine because the engine doesn't know which
/// hostname the operator wants to use (LAN IP vs Tailscale vs public
/// FQDN — they all reach the same rest-server).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RestServerCredentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Error)]
pub enum RestServerError {
    #[error("rest-server credentials state corrupt at {STATE_PATH}: {0}")]
    Corrupt(String),
    #[error("decrypt rest-server password failed: {0}")]
    Decrypt(SecretError),
    #[error("encrypt rest-server password failed: {0}")]
    Encrypt(SecretError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bcrypt: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),
}

/// Ensure a credential file exists. If one is already on disk it's
/// left alone (idempotent — safe to call on every protocol enable
/// and on engine startup). If missing, generate a fresh username +
/// random password, encrypt + persist, write the htpasswd file the
/// systemd service will read.
///
/// Called by `prepare_protocol(Protocol::RestServer)` before
/// `systemctl start nasty-rest-server` runs — so the service never
/// sees a missing-htpasswd state at first start.
pub async fn ensure_credentials() -> Result<(), RestServerError> {
    let _g = GUARD.lock().await;
    if Path::new(STATE_PATH).exists() && Path::new(HTPASSWD_PATH).exists() {
        return Ok(());
    }

    // Either side missing → regenerate both. If only the state file
    // exists (htpasswd accidentally deleted), we can't recover the
    // plaintext without decrypting, but rotate covers that path.
    // For the no-state-file branch, generate fresh.
    if !Path::new(STATE_PATH).exists() {
        let username = DEFAULT_USERNAME.to_string();
        let password = generate_password();
        persist(&username, &password).await?;
        info!("rest-server: generated initial credentials (username: {username})");
    } else {
        // State file exists but htpasswd is missing — regenerate
        // htpasswd from the encrypted password. No need to touch
        // the password itself.
        let stored = load_state().await?;
        let password = decrypt_password(&stored).await?;
        write_htpasswd(&stored.username, &password).await?;
        info!("rest-server: regenerated htpasswd from existing credentials");
    }
    Ok(())
}

/// Return the credential pair, decrypting on the fly. Used by the
/// WebUI's "show credentials" surface so the operator can build the
/// URL for the source-side backup profile.
pub async fn get_credentials() -> Result<RestServerCredentials, RestServerError> {
    let stored = load_state().await?;
    let password = decrypt_password(&stored).await?;
    Ok(RestServerCredentials {
        username: stored.username,
        password,
    })
}

/// Generate a fresh random password, re-encrypt + persist, rewrite
/// the htpasswd file. Username is unchanged unless the caller
/// supplies a new one. After this returns successfully, the
/// rest-server unit needs to be restarted to pick up the new
/// htpasswd file — done by the calling RPC handler, not here.
pub async fn rotate_password(
    new_username: Option<String>,
) -> Result<RestServerCredentials, RestServerError> {
    let _g = GUARD.lock().await;
    let username = match new_username {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => load_state()
            .await
            .map(|s| s.username)
            .unwrap_or_else(|_| DEFAULT_USERNAME.to_string()),
    };
    let password = generate_password();
    persist(&username, &password).await?;
    info!("rest-server: rotated credentials (username: {username})");
    Ok(RestServerCredentials { username, password })
}

fn generate_password() -> String {
    Alphanumeric.sample_string(&mut rand::rng(), PASSWORD_LEN)
}

async fn persist(username: &str, password: &str) -> Result<(), RestServerError> {
    let encrypted = secrets::encrypt(SECRETS_NAME, password)
        .await
        .map_err(RestServerError::Encrypt)?;
    let stored = Stored {
        username: username.to_string(),
        password_encrypted: encrypted,
    };
    save_state(&stored).await?;
    write_htpasswd(username, password).await?;
    Ok(())
}

async fn save_state(stored: &Stored) -> Result<(), RestServerError> {
    let json = serde_json::to_string_pretty(stored)
        .map_err(|e| RestServerError::Corrupt(format!("serialize: {e}")))?;
    // tmp+rename for atomicity — half-written JSON would leave the
    // box unable to read its own credentials at next boot.
    let tmp = format!("{STATE_PATH}.tmp");
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&tmp, STATE_PATH).await?;
    Ok(())
}

async fn load_state() -> Result<Stored, RestServerError> {
    let body = tokio::fs::read_to_string(STATE_PATH).await?;
    serde_json::from_str(&body).map_err(|e| RestServerError::Corrupt(e.to_string()))
}

async fn decrypt_password(stored: &Stored) -> Result<String, RestServerError> {
    secrets::decrypt(SECRETS_NAME, &stored.password_encrypted)
        .await
        .map_err(RestServerError::Decrypt)
}

/// Write the htpasswd file the systemd service hands to
/// `restic-rest-server --htpasswd-file`. Bcrypt-hashed; default cost
/// (12) is comfortable for service-start latency and a strong work
/// factor for offline cracking attempts on a leaked file. The htpasswd
/// itself is mode 0600 because while bcrypt hashes don't expose the
/// password directly, there's no reason to advertise them.
async fn write_htpasswd(username: &str, password: &str) -> Result<(), RestServerError> {
    let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST)?;
    // Apache htpasswd format: `<user>:<bcrypt-hash>\n`
    let line = format!("{username}:{hash}\n");
    let tmp = format!("{HTPASSWD_PATH}.tmp");
    tokio::fs::write(&tmp, line).await?;
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&tmp, HTPASSWD_PATH).await?;
    Ok(())
}

/// Best-effort wipe of the persisted state when the operator disables
/// the rest-server protocol entirely. Idempotent. Doesn't touch the
/// repo data itself — only the auth-side files. Called from the
/// protocol-disable hook (see protocol.rs).
pub async fn wipe() {
    if let Err(e) = tokio::fs::remove_file(STATE_PATH).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!("rest-server: failed to remove {STATE_PATH}: {e}");
    }
    if let Err(e) = tokio::fs::remove_file(HTPASSWD_PATH).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!("rest-server: failed to remove {HTPASSWD_PATH}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_password_is_long_random_alphanumeric() {
        // Pin the length + charset of generate_password so a future
        // refactor doesn't silently shorten or weaken it. The
        // alphanumeric-only choice matters for URL-embedding via
        // `https://user:password@host/`: characters like `@`, `/`,
        // `?`, and `#` would have to be percent-encoded by the
        // operator and frequently get pasted wrong.
        for _ in 0..32 {
            let p = generate_password();
            assert_eq!(p.len(), PASSWORD_LEN);
            assert!(p.chars().all(|c| c.is_ascii_alphanumeric()), "got: {p}");
        }
    }

    #[test]
    fn two_consecutive_passwords_differ() {
        // Sanity check that the RNG isn't seeded deterministically.
        // Both passwords sharing all 32 chars by chance is on the
        // order of 1 in 62^32, comfortably ignorable.
        let a = generate_password();
        let b = generate_password();
        assert_ne!(a, b);
    }
}
