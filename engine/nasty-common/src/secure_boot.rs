//! Secure Boot state readout — what does UEFI see right now?
//!
//! Single source of truth: `sbctl status --json`. We treat sbctl as a
//! read-only observability tool here; signing + key enrollment is
//! lanzaboote's job (declarative at NixOS build time, not a runtime
//! CLI), so this module never invokes `sbctl enroll-keys`, `sbctl sign`,
//! `sbctl reset`, etc. — only `status`.
//!
//! Why sbctl rather than reading efivars directly: the efivar bytes
//! cover SB on/off and SetupMode, but not "did our key actually land
//! in db?" or "is the running shim signed?". Even though we don't act
//! on those today, the eventual SB UX will ("show me the trust chain
//! this box is on"), and pinning the engine to one tool keeps the
//! callsite stable.
//!
//! Error model: anything short of a parsed JSON response → `Unknown`,
//! not an error return. The Hardware page renders this as a status
//! pill and a missing TPM/SB pill should look the same as one that
//! says "unavailable" — never as a 500.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, warn};

/// What the host's firmware is currently doing about Secure Boot.
/// All fields are best-effort: any missing piece collapses to `None`
/// rather than blocking the rest. Consumers that need a single yes/no
/// should read [`SecureBootStatus::enabled`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecureBootStatus {
    /// Best available answer to "is Secure Boot enforcing right now?".
    /// `Some(true)` / `Some(false)` come from sbctl; `None` means we
    /// couldn't determine it (no sbctl, not UEFI, sbctl errored).
    pub enabled: Option<bool>,
    /// UEFI Setup Mode — when true the firmware will accept arbitrary
    /// key enrollment without PK signing. Relevant for enrollment
    /// flows we don't drive today but will surface later.
    pub setup_mode: Option<bool>,
    /// True iff `sbctl`'s own keys / signing artifacts are installed on
    /// this host. NASty doesn't use sbctl for signing (lanzaboote does
    /// that), so this is expected to be `false` on every NASty box —
    /// but it's worth surfacing so an operator who manually installed
    /// sbctl-managed keys doesn't get a misleading UI claim.
    pub sbctl_installed: Option<bool>,
    /// Free-form one-line reason when we couldn't get a definitive
    /// answer ("sbctl not on PATH", "not booted via UEFI", "sbctl exit
    /// 1: …"). Surfaced in the WebUI tooltip on the status pill.
    pub note: Option<String>,
}

impl SecureBootStatus {
    fn unknown(note: impl Into<String>) -> Self {
        Self {
            enabled: None,
            setup_mode: None,
            sbctl_installed: None,
            note: Some(note.into()),
        }
    }
}

/// Inspect Secure Boot state via `sbctl status --json`. Never returns
/// `Result::Err` — failure modes (missing tool, BIOS boot, sbctl
/// exiting non-zero) all collapse into a [`SecureBootStatus`] with
/// `enabled = None` and a human-readable `note`.
pub async fn status() -> SecureBootStatus {
    // Cheap pre-check: if /sys/firmware/efi doesn't exist we're on a
    // BIOS/legacy boot and there's no point asking sbctl — its first
    // act would be to bail with the same finding via a noisier
    // pathway.
    if tokio::fs::metadata("/sys/firmware/efi").await.is_err() {
        return SecureBootStatus::unknown("host is not booted via UEFI");
    }

    let output = match Command::new("sbctl")
        .arg("status")
        .arg("--json")
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            debug!(target: "nasty::secure_boot", "sbctl spawn failed: {e}");
            return SecureBootStatus::unknown(format!("sbctl unavailable: {e}"));
        }
    };

    // sbctl currently exits non-zero on some happy paths (e.g. when
    // SB is disabled and it considers that a "problem") — but the
    // JSON it prints in those cases is still the source of truth.
    // So we parse stdout regardless of exit status, and only fall
    // back to the exit-status branch if parsing fails too.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match serde_json::from_str::<RawSbctlStatus>(stdout.trim()) {
        Ok(raw) => SecureBootStatus {
            enabled: raw.secure_boot,
            setup_mode: raw.setup_mode,
            sbctl_installed: raw.installed,
            note: None,
        },
        Err(parse_err) => {
            warn!(
                target: "nasty::secure_boot",
                "sbctl status --json unparseable (exit {}): {parse_err}; stderr={stderr}",
                output.status
            );
            SecureBootStatus::unknown(format!(
                "sbctl returned unparseable output (exit {})",
                output.status
            ))
        }
    }
}

/// Mirror of the subset of `sbctl status --json` fields we care about.
/// Every field is optional — sbctl has reshaped its output schema
/// across releases (the `installed` key in particular was added
/// post-0.10) and we want to keep working under both old and new.
#[derive(Debug, Deserialize)]
struct RawSbctlStatus {
    #[serde(default)]
    secure_boot: Option<bool>,
    #[serde(default)]
    setup_mode: Option<bool>,
    #[serde(default)]
    installed: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_sbctl_output() {
        let payload = r#"{
            "installed": false,
            "guid": "00000000-0000-0000-0000-000000000000",
            "setup_mode": false,
            "secure_boot": true,
            "vendors": []
        }"#;
        let raw: RawSbctlStatus = serde_json::from_str(payload).unwrap();
        assert_eq!(raw.secure_boot, Some(true));
        assert_eq!(raw.setup_mode, Some(false));
        assert_eq!(raw.installed, Some(false));
    }

    #[test]
    fn tolerates_missing_installed_field() {
        // Older sbctl releases (pre-0.10) don't emit `installed`.
        let payload = r#"{"setup_mode": false, "secure_boot": false}"#;
        let raw: RawSbctlStatus = serde_json::from_str(payload).unwrap();
        assert_eq!(raw.secure_boot, Some(false));
        assert_eq!(raw.installed, None);
    }

    #[test]
    fn tolerates_unknown_fields() {
        // Future sbctl additions must not break parsing.
        let payload = r#"{
            "secure_boot": true,
            "setup_mode": false,
            "future_field_xyz": "anything"
        }"#;
        let raw: RawSbctlStatus = serde_json::from_str(payload).unwrap();
        assert_eq!(raw.secure_boot, Some(true));
    }

    #[test]
    fn unknown_carries_note() {
        let s = SecureBootStatus::unknown("test");
        assert!(s.enabled.is_none());
        assert_eq!(s.note.as_deref(), Some("test"));
    }
}
