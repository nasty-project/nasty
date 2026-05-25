//! Secure Boot state readout — what does UEFI see right now?
//!
//! Source: `bootctl status`. systemd-bootctl is on every NixOS box
//! already (no extra dep), reports SB state, setup-mode flag, and the
//! "(unsupported)" parenthetical that distinguishes "firmware can't
//! enable SB" from "firmware can but operator hasn't" — three states
//! that efivars only expose across multiple variables and that sbctl
//! collapses into one unhelpful "disabled". As a bonus we get the
//! Measured UKI flag, which lanzaboote integration will want later.
//!
//! Parsing: plaintext, line-by-line, scanning the `System:` block.
//! Format has been stable since systemd 250-ish; both the label
//! widths and the parenthetical values are part of bootctl's
//! user-facing UI, not a serialization detail that drifts.
//!
//! Error model: never returns `Result::Err`. The Hardware page
//! renders this as a status pill — unknown SB pill should look the
//! same as one that says "disabled" or "enabled", never as a 500.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, warn};

/// Snapshot of the firmware's Secure Boot stance. All fields are
/// best-effort: any missing piece collapses to `None` rather than
/// blocking the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SecureBootStatus {
    /// `Some(true)` iff `bootctl status` reports `Secure Boot: enabled`.
    /// `Some(false)` for `disabled` (any parenthetical). `None` when we
    /// couldn't determine state — see `note`.
    pub enabled: Option<bool>,
    /// UEFI Setup Mode — when true the firmware accepts arbitrary key
    /// enrollment without PK signing. Sourced from the `(setup)`
    /// parenthetical on the `Secure Boot:` line.
    pub setup_mode: Option<bool>,
    /// `Some(true)` when bootctl reports `disabled (unsupported)` —
    /// the firmware lacks SB support entirely (e.g. OVMF without the
    /// SB build option, common on default QEMU). Distinct from a
    /// firmware that supports SB but has it switched off, so the
    /// WebUI can show "Unsupported" instead of nudging the operator
    /// to enable a feature they can't.
    pub unsupported: Option<bool>,
    /// `Some(true)` when bootctl reports `Measured UKI: yes` — kernel
    /// and initrd are loaded as a measured Unified Kernel Image.
    /// Useful signal for the future lanzaboote integration where SB
    /// and measured boot together strengthen the PCR-7 seal.
    pub measured_uki: Option<bool>,
    /// Free-form one-line reason when we couldn't determine the
    /// state ("bootctl unavailable: …", "bootctl status returned no
    /// System: block", etc.). Surfaced under the Hardware card's
    /// status pill.
    pub note: Option<String>,
}

impl SecureBootStatus {
    fn note(text: impl Into<String>) -> Self {
        Self {
            note: Some(text.into()),
            ..Self::default()
        }
    }
}

/// Inspect Secure Boot state via `bootctl status`. Never returns
/// `Result::Err`; every failure mode (bootctl missing, no System:
/// block, mangled line) collapses to a [`SecureBootStatus`] with
/// `enabled = None` and a human-readable note.
pub async fn status() -> SecureBootStatus {
    let output = match Command::new("bootctl").arg("status").output().await {
        Ok(o) => o,
        Err(e) => {
            debug!(target: "nasty::secure_boot", "bootctl spawn failed: {e}");
            return SecureBootStatus::note(format!("bootctl unavailable: {e}"));
        }
    };

    // bootctl status can exit non-zero on systems where it can't find
    // an ESP, but it still prints the System: block first if it got
    // far enough to read EFI variables — so parse stdout regardless
    // of exit code and only fall back to the error path if the parser
    // sees nothing useful.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_bootctl_status(&stdout);
    if parsed.enabled.is_some() {
        return parsed;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    warn!(
        target: "nasty::secure_boot",
        "bootctl status missing Secure Boot line ({}); stderr={stderr}",
        output.status
    );
    SecureBootStatus::note(if stderr.is_empty() {
        format!("bootctl produced no Secure Boot line ({})", output.status)
    } else {
        format!("bootctl: {stderr}")
    })
}

/// Parse the plaintext body of `bootctl status`. Picks out:
///   - `Secure Boot: <state> (<parenthetical>)?`
///   - `Measured UKI: yes/no`
///
/// Resilient to leading whitespace (the labels are right-aligned in
/// bootctl's output, so each line has varying indent), to extra lines
/// before/after the System: block, and to unknown parentheticals
/// (logged at debug and ignored).
fn parse_bootctl_status(body: &str) -> SecureBootStatus {
    let mut out = SecureBootStatus::default();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Secure Boot:") {
            let (state, paren) = split_state_and_paren(rest.trim());
            out.enabled = match state {
                "enabled" => Some(true),
                "disabled" => Some(false),
                other => {
                    debug!(target: "nasty::secure_boot", "unknown Secure Boot value: {other:?}");
                    None
                }
            };
            match paren {
                Some("setup") => out.setup_mode = Some(true),
                Some("unsupported") => out.unsupported = Some(true),
                Some(other) => {
                    debug!(target: "nasty::secure_boot", "unknown SB parenthetical: {other:?}")
                }
                None => {}
            }
        } else if let Some(rest) = trimmed.strip_prefix("Measured UKI:") {
            out.measured_uki = match rest.trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            };
        }
    }
    out
}

/// Split `"disabled (unsupported)"` into `("disabled", Some("unsupported"))`.
/// Returns `("disabled", None)` for a bare `"disabled"`. Whitespace-tolerant.
fn split_state_and_paren(s: &str) -> (&str, Option<&str>) {
    match s.find(" (") {
        Some(idx) => {
            let state = s[..idx].trim();
            let rest = s[idx + 2..].trim_end_matches(')').trim();
            (state, Some(rest))
        }
        None => (s, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(line: &str) -> String {
        format!(
            "System:\n      Firmware: UEFI 2.70 (vendor)\n Firmware Arch: x64\n   Secure Boot: {line}\n  TPM2 Support: yes\n  Measured UKI: no\n  Boot into FW: supported\n"
        )
    }

    #[test]
    fn parses_enabled() {
        let s = parse_bootctl_status(&fixture("enabled"));
        assert_eq!(s.enabled, Some(true));
        assert_eq!(s.setup_mode, None);
        assert_eq!(s.unsupported, None);
    }

    #[test]
    fn parses_disabled_plain() {
        let s = parse_bootctl_status(&fixture("disabled"));
        assert_eq!(s.enabled, Some(false));
        assert_eq!(s.unsupported, None);
        assert_eq!(s.setup_mode, None);
    }

    #[test]
    fn parses_disabled_unsupported() {
        // The case that prompted this whole rewrite: nasty.0f.ee's
        // OVMF build can't do Secure Boot, and we need to distinguish
        // that from "disabled but supported" so the UI doesn't nudge
        // the operator toward a firmware setting that doesn't exist.
        let s = parse_bootctl_status(&fixture("disabled (unsupported)"));
        assert_eq!(s.enabled, Some(false));
        assert_eq!(s.unsupported, Some(true));
        assert_eq!(s.setup_mode, None);
    }

    #[test]
    fn parses_disabled_setup() {
        let s = parse_bootctl_status(&fixture("disabled (setup)"));
        assert_eq!(s.enabled, Some(false));
        assert_eq!(s.setup_mode, Some(true));
        assert_eq!(s.unsupported, None);
    }

    #[test]
    fn parses_measured_uki() {
        let body = "System:\n   Secure Boot: enabled\n  Measured UKI: yes\n";
        let s = parse_bootctl_status(body);
        assert_eq!(s.measured_uki, Some(true));
    }

    #[test]
    fn missing_system_block_yields_no_signal() {
        let s = parse_bootctl_status("Some unrelated output\nWithout any System block\n");
        assert!(s.enabled.is_none());
        assert!(s.setup_mode.is_none());
        assert!(s.unsupported.is_none());
    }

    #[test]
    fn unknown_parenthetical_is_ignored_not_fatal() {
        // bootctl in some future systemd release could add new
        // parentheticals — we want the primary enabled flag to still
        // parse, with the unknown value silently dropped.
        let s = parse_bootctl_status(&fixture("disabled (audit)"));
        assert_eq!(s.enabled, Some(false));
        assert_eq!(s.setup_mode, None);
        assert_eq!(s.unsupported, None);
    }

    #[test]
    fn split_state_and_paren_handles_both_shapes() {
        assert_eq!(split_state_and_paren("enabled"), ("enabled", None));
        assert_eq!(
            split_state_and_paren("disabled (setup)"),
            ("disabled", Some("setup"))
        );
        assert_eq!(
            split_state_and_paren("disabled (unsupported)"),
            ("disabled", Some("unsupported"))
        );
    }

    #[test]
    fn note_helper_clears_signal_fields() {
        let s = SecureBootStatus::note("test reason");
        assert!(s.enabled.is_none());
        assert!(s.setup_mode.is_none());
        assert!(s.unsupported.is_none());
        assert!(s.measured_uki.is_none());
        assert_eq!(s.note.as_deref(), Some("test reason"));
    }
}
