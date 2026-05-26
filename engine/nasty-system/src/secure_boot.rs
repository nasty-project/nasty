//! Readiness probe for the lanzaboote-backed Secure Boot opt-in.
//!
//! This is the engine half of PR #2a (the WebUI half is the
//! Hardware-page panel that renders a checklist from this struct).
//! Returns a structured [`ReadinessReport`] so the WebUI can render
//! each check independently with its own state — pass, fail, or
//! not-applicable — rather than burying the result in a single
//! pass/fail boolean. The blocker string is the operator-facing
//! "what's stopping me right now" summary.
//!
//! Read-only: this module does not flip any switches, does not
//! invoke `sbctl create-keys`, does not touch firmware state. The
//! enrollment ceremony itself (which writes `nasty.secureBoot.enable
//! = true` to /etc/nixos/configuration.nix and triggers the
//! rebuild + reboot) belongs to PR #2b.

use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;
use tracing::warn;

/// Minimum free space we want on the ESP before enabling lanzaboote.
/// Lanzaboote stores ~50 MB per NixOS generation (signed stub +
/// kernel + initrd, deduplicated where possible); 200 MB gives an
/// operator headroom for ~4 generations of rollback while leaving
/// space for the next upgrade's working set. Hard-coded here rather
/// than configurable because the operator-facing answer to "is my
/// ESP big enough" is binary and shouldn't be customisable via the
/// readiness probe.
pub const ESP_HEADROOM_REQUIRED_BYTES: u64 = 200 * 1024 * 1024;

const WRAPPER_FLAKE_PATH: &str = "/etc/nixos/flake.nix";
const SBCTL_KEY_PATH: &str = "/var/lib/sbctl/keys/db/db.key";

/// Per-check status the UI can render uniformly. The "checklist"
/// shape ("UEFI? yes ✓ / no ✗") with mixed `Option<bool>` semantics
/// (some checks aren't applicable on certain hosts — e.g. "Secure
/// Boot currently off" is `None` when the box isn't UEFI at all)
/// lets the WebUI distinguish "blocked" from "not applicable" from
/// "passed."
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadinessReport {
    /// Top-level answer: can the operator flip
    /// `services.nasty.secureBoot.enable = true` right now and
    /// have it succeed end-to-end? `false` while any blocker
    /// remains.
    pub ready: bool,
    /// Human-readable one-line summary of why `ready == false`. The
    /// WebUI surfaces this next to a disabled "Enable Secure Boot"
    /// button.
    pub blocker: Option<String>,
    /// True iff `/sys/firmware/efi` exists — i.e. the kernel sees a
    /// UEFI boot environment. BIOS / legacy boots return false and
    /// every downstream check collapses to `None`.
    pub uefi_boot: bool,
    /// `Some(true)` when bootctl reports SB support. `Some(false)`
    /// when bootctl reports `disabled (unsupported)` — the firmware
    /// itself lacks SB. `None` when we couldn't determine (BIOS
    /// boot, bootctl missing).
    pub sb_supported_by_firmware: Option<bool>,
    /// `Some(true)` when SB is currently off — i.e. ready to enable.
    /// `Some(false)` when SB is already on (this readiness probe
    /// doesn't apply to that path). `None` on BIOS boots / when
    /// bootctl can't read the state.
    pub sb_currently_off: Option<bool>,
    /// True iff `/dev/tpmrm0` is present — TPM2 sealing of bcachefs
    /// keys requires it. Without TPM the SB opt-in still works, but
    /// the bcachefs-binding part of NASty's TPM story has nothing
    /// to seal to, so we treat this as a hard requirement for the
    /// ceremony.
    pub tpm2_available: bool,
    /// Free bytes on `/boot` as reported by `statvfs`. `None` when
    /// `/boot` isn't a separate mount or `statvfs` fails (rare).
    pub esp_free_bytes: Option<u64>,
    /// Threshold the WebUI compares `esp_free_bytes` against. Echoed
    /// in the response so the UI doesn't have to hard-code the same
    /// number on its side.
    pub esp_required_bytes: u64,
    /// `Some(true)` when `/etc/nixos/flake.nix` declares
    /// `lanzaboote.url = ...` at top level. `Some(false)` on pre-
    /// this-PR wrappers (operator needs to upgrade once so the
    /// engine re-renders the template). `None` when we couldn't
    /// read /etc/nixos/flake.nix (operator running an unusual
    /// install or read failed).
    pub wrapper_has_lanzaboote_input: Option<bool>,
    /// True iff `/var/lib/sbctl/keys/db/db.key` exists. Purely
    /// informational — when lanzaboote turns on with
    /// `autoGenerateKeys.enable = true`, the keys get generated on
    /// the first SB-enabled boot, so `false` here is the expected
    /// state pre-enrollment. The WebUI surfaces this so an operator
    /// who manually pre-generated keys sees that NASty noticed.
    pub sbctl_keys_already_generated: bool,
}

/// Compute the readiness checklist. Never returns `Result::Err` —
/// failure modes (bootctl missing, /etc/nixos unreadable, ESP not
/// mounted) collapse to `None`/`false` fields rather than an error,
/// so the WebUI always has something to render.
pub async fn readiness() -> ReadinessReport {
    let sb = nasty_common::secure_boot::status().await;
    let uefi_boot = tokio::fs::metadata("/sys/firmware/efi").await.is_ok();
    let sb_supported_by_firmware = if sb.enabled.is_some() {
        // bootctl reported a definitive state. `unsupported = Some(true)`
        // (i.e., the `(unsupported)` parenthetical) ⇒ firmware can't
        // do SB. Anything else (including `None` from the parser)
        // means SB is supported.
        Some(sb.unsupported != Some(true))
    } else {
        None
    };
    let sb_currently_off = sb.enabled.map(|e| !e);
    let tpm2_available = nasty_common::tpm::is_available().await;
    let esp_free_bytes = statvfs_free_bytes("/boot");
    let wrapper_has_lanzaboote_input = check_wrapper_has_lanzaboote().await;
    let sbctl_keys_already_generated = Path::new(SBCTL_KEY_PATH).exists();

    let report_without_overall = ReadinessReport {
        ready: false,
        blocker: None,
        uefi_boot,
        sb_supported_by_firmware,
        sb_currently_off,
        tpm2_available,
        esp_free_bytes,
        esp_required_bytes: ESP_HEADROOM_REQUIRED_BYTES,
        wrapper_has_lanzaboote_input,
        sbctl_keys_already_generated,
    };

    let blocker = compute_blocker(&report_without_overall);
    let ready = blocker.is_none();

    ReadinessReport {
        ready,
        blocker,
        ..report_without_overall
    }
}

/// Walks the checks in order and returns the first that's failing.
/// Order matters for UX: the blocker the operator sees should be the
/// one closest to the start of the boot stack — fixing it might
/// cascade.
fn compute_blocker(r: &ReadinessReport) -> Option<String> {
    if !r.uefi_boot {
        return Some("host is not booted via UEFI".into());
    }
    match r.sb_supported_by_firmware {
        Some(false) => {
            return Some(
                "firmware does not support Secure Boot (common on default QEMU OVMF)".into(),
            );
        }
        None => {
            return Some("Secure Boot state could not be read from firmware".into());
        }
        Some(true) => {}
    }
    match r.sb_currently_off {
        Some(false) => return Some("Secure Boot is already enabled in firmware".into()),
        None => return Some("Secure Boot state could not be read from firmware".into()),
        Some(true) => {}
    }
    if !r.tpm2_available {
        return Some(
            "TPM2 not available (/dev/tpmrm0 missing) — SB without TPM sealing isn't useful on NASty"
                .into(),
        );
    }
    match r.esp_free_bytes {
        Some(free) if free < r.esp_required_bytes => {
            return Some(format!(
                "ESP (/boot) has {} MB free; lanzaboote needs ≥{} MB headroom",
                free / 1_048_576,
                r.esp_required_bytes / 1_048_576,
            ));
        }
        None => {
            return Some(
                "/boot is not a separate mount — lanzaboote needs a real ESP partition mounted at /boot".into(),
            );
        }
        Some(_) => {}
    }
    match r.wrapper_has_lanzaboote_input {
        Some(false) => {
            return Some(
                "wrapper flake at /etc/nixos/flake.nix doesn't declare the lanzaboote input — \
                 run any upgrade once on the new engine to re-render it"
                    .into(),
            );
        }
        None => {
            return Some(
                "could not read /etc/nixos/flake.nix to check for the lanzaboote input".into(),
            );
        }
        Some(true) => {}
    }
    None
}

/// `statvfs(path)` → free bytes. Returns `None` when the syscall
/// fails (path doesn't exist, not a mount point, EACCES). Matches
/// the helper used in `alerts.rs` so behavior is consistent across
/// the codebase.
fn statvfs_free_bytes(path: &str) -> Option<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let path = CString::new(path).ok()?;
    let mut buf = MaybeUninit::<libc::statvfs>::uninit();
    let ret = unsafe { libc::statvfs(path.as_ptr(), buf.as_mut_ptr()) };
    if ret != 0 {
        return None;
    }
    let stat = unsafe { buf.assume_init() };
    // f_bavail is u32 in the libc bindings on this target; f_frsize is
    // already u64 (the alerts.rs path multiplies as f64 so it dodges
    // the type mismatch). One explicit widen is enough.
    Some(stat.f_bavail as u64 * stat.f_frsize)
}

/// Scan `/etc/nixos/flake.nix` for a top-level `lanzaboote.url`
/// declaration. Returns `None` when the file can't be read,
/// `Some(false)` when the file exists but the input isn't declared,
/// `Some(true)` otherwise.
///
/// Uses a plain prefix match rather than rnix because the input
/// declaration in NASty's template is always at the start of a
/// line (per `nixos/system-flake/flake.nix.template`'s shape) and a
/// false positive from a comment line would require an operator to
/// have hand-written `lanzaboote.url ...` in a comment, which they
/// haven't. PR #2b's actual enrollment trigger will use rnix for
/// the same reason `update.rs` does — there it's load-bearing.
async fn check_wrapper_has_lanzaboote() -> Option<bool> {
    let content = match tokio::fs::read_to_string(WRAPPER_FLAKE_PATH).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                target: "nasty::secure_boot",
                "could not read {WRAPPER_FLAKE_PATH} for readiness check: {e}",
            );
            return None;
        }
    };
    Some(content.lines().any(|line| {
        let trimmed = line.trim_start();
        // Require the actual setter shape — `lanzaboote.url = "..."` or
        // `lanzaboote.url="..."`. Bare mentions in comments don't match.
        trimmed.starts_with("lanzaboote.url ") || trimmed.starts_with("lanzaboote.url=")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_report() -> ReadinessReport {
        ReadinessReport {
            ready: false,
            blocker: None,
            uefi_boot: true,
            sb_supported_by_firmware: Some(true),
            sb_currently_off: Some(true),
            tpm2_available: true,
            esp_free_bytes: Some(500 * 1024 * 1024),
            esp_required_bytes: ESP_HEADROOM_REQUIRED_BYTES,
            wrapper_has_lanzaboote_input: Some(true),
            sbctl_keys_already_generated: false,
        }
    }

    #[test]
    fn no_blocker_when_all_checks_pass() {
        let r = base_report();
        assert!(compute_blocker(&r).is_none());
    }

    #[test]
    fn blocker_uefi_first() {
        let r = ReadinessReport {
            uefi_boot: false,
            ..base_report()
        };
        assert!(compute_blocker(&r).unwrap().contains("UEFI"));
    }

    #[test]
    fn blocker_unsupported_firmware() {
        let r = ReadinessReport {
            sb_supported_by_firmware: Some(false),
            ..base_report()
        };
        assert!(
            compute_blocker(&r)
                .unwrap()
                .contains("firmware does not support Secure Boot")
        );
    }

    #[test]
    fn blocker_sb_already_on() {
        let r = ReadinessReport {
            sb_currently_off: Some(false),
            ..base_report()
        };
        assert!(compute_blocker(&r).unwrap().contains("already enabled"));
    }

    #[test]
    fn blocker_no_tpm() {
        let r = ReadinessReport {
            tpm2_available: false,
            ..base_report()
        };
        assert!(compute_blocker(&r).unwrap().contains("TPM2 not available"));
    }

    #[test]
    fn blocker_esp_too_small() {
        let r = ReadinessReport {
            esp_free_bytes: Some(50 * 1024 * 1024),
            ..base_report()
        };
        let msg = compute_blocker(&r).unwrap();
        assert!(msg.contains("ESP"));
        assert!(msg.contains("50 MB"));
    }

    #[test]
    fn blocker_no_lanzaboote_in_wrapper() {
        let r = ReadinessReport {
            wrapper_has_lanzaboote_input: Some(false),
            ..base_report()
        };
        assert!(
            compute_blocker(&r)
                .unwrap()
                .contains("doesn't declare the lanzaboote input")
        );
    }

    #[test]
    fn blocker_walks_in_order_uefi_before_firmware() {
        // Both checks failing → operator should see UEFI message
        // first (the more fundamental issue).
        let r = ReadinessReport {
            uefi_boot: false,
            sb_supported_by_firmware: Some(false),
            sb_currently_off: None,
            tpm2_available: false,
            ..base_report()
        };
        assert!(compute_blocker(&r).unwrap().contains("UEFI"));
    }
}
