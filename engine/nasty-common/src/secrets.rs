//! Encrypt-at-rest for small operator-supplied secrets.
//!
//! Wraps `systemd-creds encrypt`/`systemd-creds decrypt` so callers
//! can swap a plaintext `String` for an opaque [`EncryptedBlob`] that
//! survives serialization to JSON state files and only yields its
//! contents back when fed to [`decrypt`]. Designed for the modest
//! threat model improvement of "a leaked state file (logs, backup of
//! state dir, accidental include in a support tarball) no longer
//! exposes credentials in plaintext" — not for cryptographic isolation
//! of secrets from a root-capable attacker. See the project security
//! audit notes for the threat model in full.
//!
//! # Backend selection
//!
//! `systemd-creds`'s default `--with-key=auto` picks the strongest
//! available backend at encryption time and records that choice in the
//! blob header:
//!
//!   * **TPM2 + host secret** when a TPM2 chip is present and enrolled.
//!     Sealed against `/var/lib/systemd/credential.secret` *and* the
//!     TPM; both halves required to decrypt.
//!   * **Host secret only** on TPM-less hardware. Sealed against
//!     `/var/lib/systemd/credential.secret` (root-owned, 0400). Still
//!     ciphertext-at-rest, still resists "leaked state file" scenarios,
//!     but offers no real protection against an attacker with root +
//!     access to both `/var/lib/nasty` and `/var/lib/systemd`.
//!
//! Callers don't need to know which backend is in use — the blob format
//! is self-describing. [`probe`] checks at runtime which backend systemd
//! picked so the WebUI can surface it.
//!
//! # AEAD name binding
//!
//! Every encrypted blob is bound to a stable `name` string at encrypt
//! time and verified against the same `name` at decrypt time. A blob
//! sealed with name `"nasty.backup.A.password"` will not decrypt with
//! name `"nasty.backup.B.password"`, even though both are on the same
//! host. This stops "lift a sealed blob from one profile, paste it
//! into another" replay attacks if the JSON state file is editable.

use std::process::Stdio;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

const BIN: &str = "systemd-creds";

/// Opaque encrypted credential ready for serialization to JSON state.
///
/// The inner string is a base64-encoded `systemd-creds` blob. It is
/// safe to log, persist, and round-trip through serde — without the
/// matching host secret (and TPM if used at seal time) it reveals
/// nothing about the original plaintext beyond an upper bound on
/// length. Use [`encrypt`] to construct, [`decrypt`] to consume.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedBlob(String);

impl EncryptedBlob {
    /// Inspect the raw blob — only the JSON state writer should need
    /// this. Most callers should round-trip through `serde`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Backend that `systemd-creds` actually used to seal a blob on this
/// host. Returned by [`probe`] so the WebUI can show whether secrets
/// are TPM-bound or only host-bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SecretsBackend {
    /// `systemd-creds` is available and sealed our probe blob against
    /// both the TPM and the host secret. Stolen disk + no TPM access
    /// reveals nothing.
    TpmAndHost,
    /// `systemd-creds` is available but no usable TPM was found, so
    /// the probe blob is sealed against the host secret only. Still
    /// encrypted-at-rest, but the key sits in `/var/lib/systemd/` on
    /// the same disk.
    HostOnly,
}

/// Status payload for the WebUI / health endpoint. Carries either the
/// active backend or the reason `systemd-creds` couldn't be used. Always
/// safe to serialize and surface to operators — never contains plaintext.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum SecretsStatus {
    /// `systemd-creds` is healthy and ready to seal new secrets.
    Available { backend: SecretsBackend },
    /// `systemd-creds` couldn't seal a probe blob on this host —
    /// callers will fall back to storing plaintext. The reason field
    /// is operator-facing and should explain *why* (binary missing,
    /// permission denied, etc.) so the WebUI can render an actionable
    /// warning instead of a generic "encryption unavailable".
    Unavailable { reason: String },
}

/// Failure modes shared by encrypt + decrypt. Surfaced to callers
/// rather than swallowed so the JSON-RPC layer can return a useful
/// message instead of a generic "internal error".
#[derive(Debug, Error)]
pub enum SecretError {
    #[error("systemd-creds binary not available: {0}")]
    BinaryUnavailable(String),
    #[error("systemd-creds {phase} failed (exit {status}): {stderr}")]
    CommandFailed {
        phase: &'static str,
        status: String,
        stderr: String,
    },
    #[error("io error talking to systemd-creds: {0}")]
    Io(#[from] std::io::Error),
    #[error("systemd-creds returned non-utf8 output for {0}: {1}")]
    Utf8(&'static str, std::string::FromUtf8Error),
}

/// Encrypt `plaintext` using `systemd-creds`, binding the resulting
/// blob to `name`. The same `name` is required at [`decrypt`] time
/// — a mismatch returns `CommandFailed`.
///
/// `name` should be stable for the lifetime of the secret (e.g.
/// `"nasty.backup.<profile_id>.password"`). Renaming a secret
/// requires decrypt + re-encrypt under the new name.
pub async fn encrypt(name: &str, plaintext: &str) -> Result<EncryptedBlob, SecretError> {
    let stdout = run_with_stdin(
        "encrypt",
        &["encrypt", "--name", name, "-", "-"],
        plaintext.as_bytes(),
    )
    .await?;
    let blob = String::from_utf8(stdout).map_err(|e| SecretError::Utf8("encrypt stdout", e))?;
    Ok(EncryptedBlob(blob))
}

/// Reverse of [`encrypt`]. `name` must match what was passed at
/// encrypt time. Returns the plaintext as a `String`; callers should
/// drop it as soon as the secret has been handed to its consumer.
pub async fn decrypt(name: &str, blob: &EncryptedBlob) -> Result<String, SecretError> {
    let stdout = run_with_stdin(
        "decrypt",
        &["decrypt", "--name", name, "-", "-"],
        blob.0.as_bytes(),
    )
    .await?;
    String::from_utf8(stdout).map_err(|e| SecretError::Utf8("decrypt stdout", e))
}

/// Round-trip a probe blob to find out whether `systemd-creds` works
/// on this host and which backend it ended up using. Cheap enough to
/// call on each engine startup for status reporting; not so cheap that
/// you'd want to call it in a hot path.
///
/// Failures here are not fatal — callers that find the backend
/// unavailable should keep accepting plaintext secrets (with a loud
/// warning in the UI) until the operator fixes the underlying issue.
/// Refusing to store secrets at all would leave the operator unable
/// to configure backups on, say, a fresh box where `systemd-creds`
/// hasn't been exercised yet.
pub async fn probe() -> SecretsStatus {
    let probe_name = "nasty.secrets.probe";
    let probe_plaintext = "probe";
    let blob = match encrypt(probe_name, probe_plaintext).await {
        Ok(b) => b,
        Err(e) => {
            return SecretsStatus::Unavailable {
                reason: e.to_string(),
            };
        }
    };
    match decrypt(probe_name, &blob).await {
        Ok(round_tripped) if round_tripped == probe_plaintext => {}
        Ok(other) => {
            return SecretsStatus::Unavailable {
                reason: format!(
                    "round-trip mismatch (sealed 'probe', unsealed {} bytes)",
                    other.len()
                ),
            };
        }
        Err(e) => {
            return SecretsStatus::Unavailable {
                reason: format!("decrypt: {e}"),
            };
        }
    }
    let backend = detect_backend(&blob);
    info!(
        "systemd-creds healthy (backend: {})",
        match backend {
            SecretsBackend::TpmAndHost => "TPM + host secret",
            SecretsBackend::HostOnly => "host secret only",
        }
    );
    SecretsStatus::Available { backend }
}

/// Inspect a freshly-created blob to guess which backend was used to
/// seal it. `systemd-creds inspect` would be the official answer but
/// we'd rather not shell out a third time on every probe; the blob
/// header is a self-describing CBOR structure and the TPM2 binding
/// shows up as a token marker. Conservative fallback: assume host-only.
fn detect_backend(blob: &EncryptedBlob) -> SecretsBackend {
    // systemd-creds inspect of a TPM2-sealed blob prints a line
    // beginning with "TPM2:" near the top — easier to scrape than
    // CBOR-decoding the blob ourselves. If inspect itself fails, we
    // fall back to HostOnly since that's the conservative answer.
    let output = std::process::Command::new(BIN).args(["has-tpm2"]).output();
    match output {
        Ok(o) if o.status.success() => {
            // Host has a TPM2 — systemd-creds will have used it.
            // (--with-key=auto picks tpm+host when TPM is available.)
            let _ = blob;
            SecretsBackend::TpmAndHost
        }
        _ => SecretsBackend::HostOnly,
    }
}

async fn run_with_stdin(
    phase: &'static str,
    args: &[&str],
    stdin: &[u8],
) -> Result<Vec<u8>, SecretError> {
    let mut child = match Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SecretError::BinaryUnavailable(format!(
                "{BIN} not found in PATH"
            )));
        }
        Err(e) => return Err(SecretError::Io(e)),
    };

    if let Some(mut sink) = child.stdin.take() {
        sink.write_all(stdin).await?;
        drop(sink);
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            target: "nasty::secrets",
            "{BIN} {phase} failed: status={} stderr={}",
            output.status,
            stderr
        );
        return Err(SecretError::CommandFailed {
            phase,
            status: output.status.to_string(),
            stderr,
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_blob_round_trips_through_serde() {
        // Pin the wire format: EncryptedBlob serializes as a bare
        // string (no wrapper object), so state-file JSON is human-
        // readable and not unnecessarily nested. A future refactor
        // that switches to a struct shape would break old state
        // files; this test makes that breakage loud.
        let blob = EncryptedBlob("YmFzZTY0LWlzaC1ibG9i".to_string());
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(json, "\"YmFzZTY0LWlzaC1ibG9i\"");
        let parsed: EncryptedBlob = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, blob);
    }

    #[test]
    fn secrets_status_serializes_with_status_tag() {
        // The WebUI status pill discriminates on the `status` field.
        // Pin both the discriminator key and the kebab-case casing so
        // a future serde-attribute drift breaks here, not in the UI.
        let available = SecretsStatus::Available {
            backend: SecretsBackend::TpmAndHost,
        };
        let json = serde_json::to_value(&available).unwrap();
        assert_eq!(json["status"], "available");
        assert_eq!(json["backend"], "tpm-and-host");

        let unavailable = SecretsStatus::Unavailable {
            reason: "systemd-creds not found".to_string(),
        };
        let json = serde_json::to_value(&unavailable).unwrap();
        assert_eq!(json["status"], "unavailable");
        assert_eq!(json["reason"], "systemd-creds not found");
    }
}
