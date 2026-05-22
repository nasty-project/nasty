//! TPM2 sealing primitives for the dataset-key auto-unlock flow (#102).
//!
//! Shells out to the `tpm2-tools` family — same shape as the rest of the
//! engine's hardware integrations. The blob is bound to **PCR 7**
//! (Secure Boot configuration) so:
//!
//!   - Kernel / initrd / bootloader / userspace updates still unseal —
//!     none of those touch PCR 7.
//!   - Booting alternate media, disabling Secure Boot, or moving the
//!     drive to a different host voids the seal.
//!
//! The blob format on disk is a small JSON wrapper containing base64
//! TPM2B_PUBLIC and TPM2B_PRIVATE bytes. The Storage Root primary used
//! to load it is re-derived on every call from the owner hierarchy
//! seed (deterministic per TPM until the chip is cleared) — we never
//! persist a primary handle.

use std::path::Path;

use base64::Engine;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, warn};

/// PCR selection the sealing policy binds to. PCR 7 is the Secure Boot
/// state — keys, db, dbx, MOK — extended by EDK2/shim/grub during boot.
/// Keep this stable: changing it invalidates every sealed blob on every
/// host running NASty, with no migration path.
pub const PCR_SELECTION: &str = "sha256:7";

/// Schema version for the on-disk JSON. Bump if the field layout
/// changes incompatibly; never reuse an old number.
const BLOB_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum TpmError {
    #[error("TPM2 not available on this host (/dev/tpmrm0 missing)")]
    Unavailable,
    #[error("tpm2-tools `{step}` failed: {message}")]
    ToolFailed { step: &'static str, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sealed blob malformed: {0}")]
    Blob(String),
}

/// On-disk wrapper around the raw TPM2 blobs. Stored next to the
/// plaintext `.key` file as `<name>.tpm`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedBlob {
    /// Format version of this struct — see [`BLOB_VERSION`].
    pub version: u32,
    /// PCR selection string the policy was built against
    /// (e.g. `"sha256:7"`). Mirrored into the file so a future change
    /// to [`PCR_SELECTION`] doesn't silently break already-sealed
    /// blobs — we replay this exact selection during unseal.
    pub pcrs: String,
    /// TPM2B_PUBLIC, base64-standard.
    pub pub_b64: String,
    /// TPM2B_PRIVATE, base64-standard.
    pub priv_b64: String,
}

/// Cheap usability check — does the kernel expose the TPM2 resource
/// manager? If yes, tpm2-tools have a device to talk to. Doesn't
/// verify the binaries exist; a missing tool surfaces as a clear
/// `ToolFailed { step: "createprimary", … }` on the first seal/unseal.
pub async fn is_available() -> bool {
    tokio::fs::metadata("/dev/tpmrm0").await.is_ok()
}

/// Seal `plaintext` under a PCR-7 policy. The returned blob can be
/// JSON-serialised to disk and later passed to [`unseal`].
pub async fn seal_with_pcr7(plaintext: &[u8]) -> Result<SealedBlob, TpmError> {
    if !is_available().await {
        return Err(TpmError::Unavailable);
    }

    let dir = tempfile::tempdir()?;
    let dir_path = dir.path();

    let plaintext_path = dir_path.join("plain.bin");
    tokio::fs::write(&plaintext_path, plaintext).await?;

    let primary_ctx = dir_path.join("primary.ctx");
    run_tool(
        "createprimary",
        &[
            "tpm2_createprimary",
            "-Q",
            "-C",
            "o",
            "-G",
            "ecc",
            "-c",
            path_str(&primary_ctx)?,
        ],
    )
    .await?;

    let session_ctx = dir_path.join("trial.ctx");
    let policy_dat = dir_path.join("policy.dat");
    run_tool(
        "startauthsession",
        &["tpm2_startauthsession", "-Q", "-S", path_str(&session_ctx)?],
    )
    .await?;
    run_tool(
        "policypcr",
        &[
            "tpm2_policypcr",
            "-Q",
            "-S",
            path_str(&session_ctx)?,
            "-l",
            PCR_SELECTION,
            "-L",
            path_str(&policy_dat)?,
        ],
    )
    .await?;
    run_tool(
        "flushcontext",
        &["tpm2_flushcontext", "-Q", path_str(&session_ctx)?],
    )
    .await?;

    let pub_path = dir_path.join("sealed.pub");
    let priv_path = dir_path.join("sealed.priv");
    run_tool(
        "create",
        &[
            "tpm2_create",
            "-Q",
            "-C",
            path_str(&primary_ctx)?,
            "-u",
            path_str(&pub_path)?,
            "-r",
            path_str(&priv_path)?,
            "-L",
            path_str(&policy_dat)?,
            "-a",
            "fixedtpm|fixedparent|adminwithpolicy",
            "-i",
            path_str(&plaintext_path)?,
        ],
    )
    .await?;

    let pub_bytes = tokio::fs::read(&pub_path).await?;
    let priv_bytes = tokio::fs::read(&priv_path).await?;

    let engine = base64::engine::general_purpose::STANDARD;
    Ok(SealedBlob {
        version: BLOB_VERSION,
        pcrs: PCR_SELECTION.to_string(),
        pub_b64: engine.encode(&pub_bytes),
        priv_b64: engine.encode(&priv_bytes),
    })
}

/// Unseal a blob previously produced by [`seal_with_pcr7`]. Fails if
/// the PCR state at the time of the call doesn't match what the seal
/// was bound to (e.g. Secure Boot disabled, host swapped, TPM cleared).
pub async fn unseal(blob: &SealedBlob) -> Result<Vec<u8>, TpmError> {
    if !is_available().await {
        return Err(TpmError::Unavailable);
    }
    if blob.version != BLOB_VERSION {
        return Err(TpmError::Blob(format!(
            "unsupported blob version {} (expected {})",
            blob.version, BLOB_VERSION
        )));
    }

    let engine = base64::engine::general_purpose::STANDARD;
    let pub_bytes = engine
        .decode(&blob.pub_b64)
        .map_err(|e| TpmError::Blob(format!("pub_b64: {e}")))?;
    let priv_bytes = engine
        .decode(&blob.priv_b64)
        .map_err(|e| TpmError::Blob(format!("priv_b64: {e}")))?;

    let dir = tempfile::tempdir()?;
    let dir_path = dir.path();

    let pub_path = dir_path.join("sealed.pub");
    let priv_path = dir_path.join("sealed.priv");
    tokio::fs::write(&pub_path, &pub_bytes).await?;
    tokio::fs::write(&priv_path, &priv_bytes).await?;

    let primary_ctx = dir_path.join("primary.ctx");
    run_tool(
        "createprimary",
        &[
            "tpm2_createprimary",
            "-Q",
            "-C",
            "o",
            "-G",
            "ecc",
            "-c",
            path_str(&primary_ctx)?,
        ],
    )
    .await?;

    let sealed_ctx = dir_path.join("sealed.ctx");
    run_tool(
        "load",
        &[
            "tpm2_load",
            "-Q",
            "-C",
            path_str(&primary_ctx)?,
            "-u",
            path_str(&pub_path)?,
            "-r",
            path_str(&priv_path)?,
            "-c",
            path_str(&sealed_ctx)?,
        ],
    )
    .await?;

    let session_ctx = dir_path.join("session.ctx");
    run_tool(
        "startauthsession",
        &[
            "tpm2_startauthsession",
            "-Q",
            "--policy-session",
            "-S",
            path_str(&session_ctx)?,
        ],
    )
    .await?;
    run_tool(
        "policypcr",
        &[
            "tpm2_policypcr",
            "-Q",
            "-S",
            path_str(&session_ctx)?,
            "-l",
            &blob.pcrs,
        ],
    )
    .await?;

    let session_arg = format!("session:{}", path_str(&session_ctx)?);
    let plaintext = capture_tool(
        "unseal",
        &[
            "tpm2_unseal",
            "-c",
            path_str(&sealed_ctx)?,
            "-p",
            &session_arg,
        ],
    )
    .await?;

    // Best-effort flush — the dir gets dropped right after but the
    // TPM keeps the session slot until told otherwise. Failure here
    // doesn't invalidate the unseal we already got.
    let _ = run_tool(
        "flushcontext",
        &["tpm2_flushcontext", "-Q", path_str(&session_ctx)?],
    )
    .await;

    Ok(plaintext)
}

/// Run a tpm2-tools invocation that returns no useful stdout. `step`
/// is a short label used only for error messages; `cmd` is `[binary,
/// ...args]`.
async fn run_tool(step: &'static str, cmd: &[&str]) -> Result<(), TpmError> {
    let (program, args) = cmd
        .split_first()
        .expect("run_tool called with empty cmd slice");
    debug!(target: "nasty::tpm", "exec: {} {}", program, args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| TpmError::ToolFailed {
            step,
            message: format!("spawn {program}: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            target: "nasty::tpm",
            "{program} exit {}: {stderr}",
            output.status
        );
        return Err(TpmError::ToolFailed {
            step,
            message: format!("exit {}: {stderr}", output.status),
        });
    }
    Ok(())
}

/// Same as [`run_tool`] but returns stdout bytes — used for
/// `tpm2_unseal`, whose payload is the plaintext.
async fn capture_tool(step: &'static str, cmd: &[&str]) -> Result<Vec<u8>, TpmError> {
    let (program, args) = cmd
        .split_first()
        .expect("capture_tool called with empty cmd slice");
    debug!(target: "nasty::tpm", "exec: {} {}", program, args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| TpmError::ToolFailed {
            step,
            message: format!("spawn {program}: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            target: "nasty::tpm",
            "{program} exit {}: {stderr}",
            output.status
        );
        return Err(TpmError::ToolFailed {
            step,
            message: format!("exit {}: {stderr}", output.status),
        });
    }
    Ok(output.stdout)
}

fn path_str(p: &Path) -> Result<&str, TpmError> {
    p.to_str()
        .ok_or_else(|| TpmError::Blob(format!("non-utf8 path: {p:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_blob_json_roundtrip() {
        let blob = SealedBlob {
            version: BLOB_VERSION,
            pcrs: PCR_SELECTION.to_string(),
            pub_b64: "AAEC".into(),
            priv_b64: "AwQF".into(),
        };
        let json = serde_json::to_string(&blob).expect("serialize");
        let back: SealedBlob = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.version, BLOB_VERSION);
        assert_eq!(back.pcrs, PCR_SELECTION);
        assert_eq!(back.pub_b64, "AAEC");
        assert_eq!(back.priv_b64, "AwQF");
    }

    #[tokio::test]
    async fn is_available_returns_bool_without_panic() {
        // Just exercises the syscall — value depends on host.
        let _ = is_available().await;
    }

    #[tokio::test]
    async fn unseal_rejects_unsupported_version() {
        let blob = SealedBlob {
            version: 99,
            pcrs: PCR_SELECTION.to_string(),
            pub_b64: "AAEC".into(),
            priv_b64: "AwQF".into(),
        };
        // Skip when /dev/tpmrm0 is missing — version check runs after
        // the availability gate.
        if !is_available().await {
            return;
        }
        let err = unseal(&blob).await.expect_err("should reject");
        match err {
            TpmError::Blob(msg) => assert!(msg.contains("version"), "got: {msg}"),
            other => panic!("expected Blob, got {other:?}"),
        }
    }
}
