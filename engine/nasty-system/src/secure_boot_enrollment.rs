//! Secure Boot enrollment ceremony state machine (issue ADR #324).
//!
//! Drives the per-box "go from `lanzaboote loaded but inert` to `SB
//! enforcing with NASty-owned keys`" flow. Highly experimental — the
//! WebUI wraps every step in a clear EXPERIMENTAL badge. The state
//! machine survives engine restarts and reboots via a small JSON
//! file at `/var/lib/nasty/secure-boot-enrollment.json`.
//!
//! ## Phases
//!
//! ```text
//! NotStarted
//!     │ begin(operator)
//!     ▼
//! OverlayWritten             ←──── operator runs `nasty-rebuild`,
//!     │                            then enters BIOS Setup Mode and
//!     │                            reboots; firmware auto-enrolls
//!     │                            NASty's keys via systemd-boot
//!     ▼                            and reboots into SB-on state
//! PostEnrollment             (detected on engine startup —
//!     │                       bootctl reports SB now enabled,
//!     │                       and any TPM-bound bcachefs FSes
//!     │                       have stale `.tpm` blobs that need
//!     │                       re-binding from /filesystems)
//!     │ complete(operator)
//!     ▼
//! Complete
//! ```
//!
//! From `NotStarted` or `Aborted`, `begin` is allowed. From any
//! pre-`PostEnrollment` phase, `abort` is allowed (removes the
//! overlay file, reverts to NotStarted-equivalent state); after
//! `PostEnrollment` `abort` becomes a no-op because SB has already
//! been enrolled into firmware and only a BIOS visit can undo it.
//!
//! ## Re-seal
//!
//! When SB transitions on, the firmware's PCR-7 reading changes —
//! existing `.tpm` blobs that sealed the bcachefs key against the
//! pre-SB PCR-7 will fail to unseal. We don't drive re-seal from
//! this module; instead `PostEnrollment.stale_tpm_bindings` lists
//! the affected FSes and the wizard links the operator to the
//! existing `Bind to TPM` button on /filesystems for each. That
//! button already re-seals against the current PCR-7 using the
//! plaintext `.key` file (or the operator's passphrase if they've
//! gone through `Export Key → Destroy Key`).

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{info, warn};

const REBUILD_UNIT: &str = "nasty-secureboot-rebuild";

const STATE_PATH: &str = "/var/lib/nasty/secure-boot-enrollment.json";
const STATE_DIR: &str = "/var/lib/nasty";
const NIX_OVERLAY_PATH: &str = "/etc/nixos/secure-boot.nix";
const NASTY_KEYS_DIR: &str = "/var/lib/nasty/keys";

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnrollmentPhase {
    /// Default. Operator hasn't started the ceremony, or has been
    /// reset after a previous abort. The wizard renders the
    /// "Begin enrollment" button.
    #[default]
    NotStarted,
    /// The overlay file is on disk with both
    /// `services.nasty.secureBoot.enable` and the lanzaboote auto-
    /// enroll knobs set. Operator now needs to (a) run
    /// `nasty-rebuild` to apply the config, (b) reboot into BIOS,
    /// put firmware in Setup Mode, save, (c) reboot again — auto-
    /// enroll picks up the staged keys on that boot.
    OverlayWritten {
        /// Unix seconds when the overlay was written.
        overlay_at: u64,
    },
    /// Engine detected (on startup, after the operator's reboot
    /// dance) that Secure Boot has transitioned to enabled. The
    /// `stale_tpm_bindings` field names every bcachefs filesystem
    /// whose `.tpm` blob predates the SB transition and now needs
    /// re-binding under the new PCR-7. Operator works through the
    /// list via /filesystems → Bind to TPM.
    PostEnrollment {
        /// Unix seconds when the engine detected the transition.
        detected_at: u64,
        /// FS names with a `.tpm` blob present in `/var/lib/nasty/keys/`.
        /// Empty list = the box had no TPM-bound filesystems at the
        /// time of enrollment.
        stale_tpm_bindings: Vec<String>,
    },
    /// Operator has confirmed they're done. Optional terminal
    /// state — `NotStarted` would also be a sane stopping point,
    /// but `Complete` lets the wizard render a "you finished this"
    /// summary on subsequent visits instead of suggesting the
    /// operator start over.
    Complete {
        /// Unix seconds the operator clicked Done.
        completed_at: u64,
    },
    /// Operator cancelled before the firmware-level commit (i.e.
    /// before `PostEnrollment`). Engine has removed the overlay
    /// file. State persists so the wizard can render a "previous
    /// attempt was aborted" hint on the next Begin click.
    Aborted { aborted_at: u64, reason: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EnrollmentState {
    pub phase: EnrollmentPhase,
    /// Username that initiated the most recent ceremony attempt.
    /// `None` only on a freshly-installed box that's never started
    /// enrollment.
    #[serde(default)]
    pub initiated_by: Option<String>,
    /// Unix seconds when the most recent wizard-driven
    /// `nasty-rebuild` was triggered. `None` until the operator
    /// clicks Rebuild from the wizard. Cleared back to `None` on
    /// every `begin()` / `abort()` so each fresh ceremony starts
    /// without a stale marker. The Abort copy reads this to
    /// decide whether "the overlay was never applied" or "you
    /// need to rebuild once more to revert" is accurate.
    #[serde(default)]
    pub rebuild_triggered_at: Option<u64>,
}

/// Live snapshot of the wizard-driven rebuild, queried via
/// `systemctl show` on every status call (we don't persist the
/// status — systemd is the source of truth for what's running).
/// Returned as a sibling field of `EnrollmentState` from the
/// `status` RPC.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RebuildSnapshot {
    /// `running` / `succeeded` / `failed` / `not_run`. Last
    /// transition is also visible on the wizard's polled UI.
    pub status: RebuildStatus,
    /// Exit code from the last finished run, if any. Useful for
    /// the failed-rebuild error toast.
    pub exit_code: Option<i32>,
    /// Tail of the unit's journal (last ~20 lines), surfaced
    /// verbatim in the wizard's "rebuild output" expandable.
    /// Empty when the unit was never started, or when journalctl
    /// failed (we log + skip rather than abort the status call).
    pub journal_tail: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RebuildStatus {
    /// systemd doesn't know the unit (never triggered through this
    /// engine instance, or `systemctl reset-failed` cleared it).
    #[default]
    NotRun,
    /// `systemctl is-active` says the unit is still doing work.
    Running,
    /// Unit exited with status 0.
    Succeeded,
    /// Unit exited non-zero. `exit_code` field carries the number.
    Failed,
}

/// Combined response from `system.secure_boot.enrollment.status` —
/// the persistent enrollment state plus the live rebuild snapshot
/// in a single shape so the wizard doesn't need a second round-
/// trip on every poll.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EnrollmentStatusResponse {
    #[serde(flatten)]
    pub state: EnrollmentState,
    pub rebuild: RebuildSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollmentError {
    #[error(
        "secure-boot enrollment can only begin from NotStarted or Aborted state — current phase blocks it"
    )]
    AlreadyInProgress,
    #[error(
        "can't abort — firmware enrollment has already committed (phase is past PostEnrollment)"
    )]
    PastPointOfNoReturn,
    #[error("can only complete from PostEnrollment phase")]
    NotInPostEnrollment,
    #[error("io: {0}")]
    Io(String),
}

impl From<std::io::Error> for EnrollmentError {
    fn from(e: std::io::Error) -> Self {
        EnrollmentError::Io(e.to_string())
    }
}

pub struct SecureBootEnrollmentService {
    state: Arc<RwLock<EnrollmentState>>,
}

impl SecureBootEnrollmentService {
    pub async fn new() -> Self {
        let state = load_state().await.unwrap_or_default();
        let svc = Self {
            state: Arc::new(RwLock::new(state)),
        };
        // One-shot startup check: if the previous state was
        // `OverlayWritten` and Secure Boot is now reported as
        // enabled, advance to `PostEnrollment`. Done here (not in
        // a recurring poll) because the SB transition only happens
        // across a reboot, and the engine's startup itself is the
        // signal that we're on the new side of it.
        svc.maybe_detect_transition().await;
        svc
    }

    /// Combined enrollment state + live rebuild snapshot. Wizard
    /// polls this every few seconds; the rebuild snapshot is
    /// re-queried from systemd on each call (cheap — two
    /// `systemctl` / `journalctl` invocations).
    pub async fn status(&self) -> EnrollmentStatusResponse {
        let state = self.state.read().await.clone();
        let rebuild = query_rebuild_snapshot().await;
        EnrollmentStatusResponse { state, rebuild }
    }

    /// Start the ceremony: write the Nix overlay that enables
    /// lanzaboote auto-enroll on next rebuild, persist the state.
    /// Returns the new state so the WebUI can render the next
    /// wizard step without a follow-up status call.
    pub async fn begin(&self, actor: &str) -> Result<EnrollmentState, EnrollmentError> {
        {
            let current = self.state.read().await;
            match current.phase {
                EnrollmentPhase::NotStarted | EnrollmentPhase::Aborted { .. } => {}
                _ => return Err(EnrollmentError::AlreadyInProgress),
            }
        }
        write_overlay_file().await?;
        // Best-effort: clear any leftover systemd unit state from a
        // previous ceremony so the new ceremony's rebuild snapshot
        // starts from `not_run`. Ignored on failure (unit may not
        // exist yet on a fresh install).
        nasty_common::cmd::try_run("systemctl", &["reset-failed", REBUILD_UNIT]).await;
        let new_state = EnrollmentState {
            phase: EnrollmentPhase::OverlayWritten {
                overlay_at: unix_now(),
            },
            initiated_by: Some(actor.to_string()),
            rebuild_triggered_at: None,
        };
        *self.state.write().await = new_state.clone();
        save_state(&new_state).await?;
        info!(
            target: "nasty::secure_boot_enrollment",
            "enrollment begun by {actor} — overlay written, awaiting operator rebuild + BIOS Setup Mode"
        );
        Ok(new_state)
    }

    /// Abort the ceremony before firmware enrollment. Removes the
    /// overlay (so the next rebuild reverts to systemd-boot without
    /// SB), records the abort reason. No-op when already past the
    /// point where firmware has enrolled keys.
    pub async fn abort(
        &self,
        actor: &str,
        reason: &str,
    ) -> Result<EnrollmentState, EnrollmentError> {
        {
            let current = self.state.read().await;
            match current.phase {
                EnrollmentPhase::NotStarted
                | EnrollmentPhase::PostEnrollment { .. }
                | EnrollmentPhase::Complete { .. } => {
                    // PostEnrollment + Complete: firmware has the keys,
                    // abort is meaningless. NotStarted: nothing to undo.
                    return Err(EnrollmentError::PastPointOfNoReturn);
                }
                _ => {}
            }
        }
        remove_overlay_file().await?;
        let new_state = EnrollmentState {
            phase: EnrollmentPhase::Aborted {
                aborted_at: unix_now(),
                reason: reason.to_string(),
            },
            initiated_by: Some(actor.to_string()),
            // Don't clear `rebuild_triggered_at` here — the WebUI's
            // Abort dialog reads it to decide whether the operator
            // needs to run nasty-rebuild once more to revert
            // (rebuild_triggered_at = Some) or whether abort is
            // clean because the overlay was never applied
            // (rebuild_triggered_at = None).
            rebuild_triggered_at: self.state.read().await.rebuild_triggered_at,
        };
        *self.state.write().await = new_state.clone();
        save_state(&new_state).await?;
        info!(
            target: "nasty::secure_boot_enrollment",
            "enrollment aborted by {actor}: {reason}"
        );
        Ok(new_state)
    }

    /// Mark the ceremony done. Only valid from PostEnrollment —
    /// the wizard's Complete button surfaces after the operator
    /// has re-bound their TPM filesystems (or explicitly chosen to
    /// skip).
    pub async fn complete(&self, actor: &str) -> Result<EnrollmentState, EnrollmentError> {
        {
            let current = self.state.read().await;
            if !matches!(current.phase, EnrollmentPhase::PostEnrollment { .. }) {
                return Err(EnrollmentError::NotInPostEnrollment);
            }
        }
        let new_state = EnrollmentState {
            phase: EnrollmentPhase::Complete {
                completed_at: unix_now(),
            },
            initiated_by: Some(actor.to_string()),
            rebuild_triggered_at: self.state.read().await.rebuild_triggered_at,
        };
        *self.state.write().await = new_state.clone();
        save_state(&new_state).await?;
        info!(
            target: "nasty::secure_boot_enrollment",
            "enrollment marked complete by {actor}"
        );
        Ok(new_state)
    }

    /// Trigger `nasty-rebuild` via systemd-run so the operator
    /// doesn't have to SSH or use the Terminal page to apply the
    /// overlay the wizard just wrote. The unit runs detached
    /// (--no-block --collect), so the engine survives nixos-rebuild
    /// restarting services mid-switch; the wizard polls
    /// `status()` to see the rebuild's live state.
    ///
    /// Only valid from `OverlayWritten`. Re-runnable (idempotent
    /// `reset-failed` + new systemd-run) so an operator who hit a
    /// transient failure can retry without aborting first.
    pub async fn rebuild(&self) -> Result<(), EnrollmentError> {
        {
            let current = self.state.read().await;
            if !matches!(current.phase, EnrollmentPhase::OverlayWritten { .. }) {
                return Err(EnrollmentError::AlreadyInProgress);
            }
        }
        // Clear any prior unit state — without this, a previous
        // failed run leaves the unit in `failed` and systemd-run
        // rejects the new --unit invocation. `try_run` logs
        // failures but doesn't propagate, which is exactly what we
        // want here (the unit might not exist on first use).
        nasty_common::cmd::try_run("systemctl", &["reset-failed", REBUILD_UNIT]).await;
        nasty_common::cmd::try_run("systemctl", &["stop", REBUILD_UNIT]).await;

        // `nasty-rebuild` is the shell wrapper in nasty.nix's
        // systemPackages. Same script the Terminal page invocation
        // would run; --collect cleans the unit after exit so a
        // future ceremony starts from a clean state.
        let output = Command::new("systemd-run")
            .args([
                "--unit",
                REBUILD_UNIT,
                "--collect",
                "--no-block",
                "--description=NASty Secure Boot enrollment rebuild",
                "/run/current-system/sw/bin/nasty-rebuild",
            ])
            .output()
            .await
            .map_err(|e| EnrollmentError::Io(format!("systemd-run: {e}")))?;
        if !output.status.success() {
            return Err(EnrollmentError::Io(format!(
                "systemd-run exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let mut state = self.state.write().await;
        state.rebuild_triggered_at = Some(unix_now());
        let snapshot = state.clone();
        drop(state);
        save_state(&snapshot).await?;
        info!(
            target: "nasty::secure_boot_enrollment",
            "secure-boot enrollment rebuild triggered via systemd-run"
        );
        Ok(())
    }

    /// Engine-startup check: if we left off in `OverlayWritten`
    /// and the firmware now reports SB enabled, advance to
    /// `PostEnrollment`. Idempotent — running again from
    /// `PostEnrollment` does nothing.
    async fn maybe_detect_transition(&self) {
        let current_phase = self.state.read().await.phase.clone();
        if !matches!(current_phase, EnrollmentPhase::OverlayWritten { .. }) {
            return;
        }
        let sb = nasty_common::secure_boot::status().await;
        if sb.enabled != Some(true) {
            // Still pre-enrollment from firmware's perspective.
            // Operator hasn't completed the BIOS Setup Mode dance
            // yet (or did and it didn't take). Leave state alone
            // so the wizard keeps showing the "go to BIOS" hint.
            return;
        }
        let stale = enumerate_tpm_bindings().await;
        let new_state = EnrollmentState {
            phase: EnrollmentPhase::PostEnrollment {
                detected_at: unix_now(),
                stale_tpm_bindings: stale,
            },
            initiated_by: self.state.read().await.initiated_by.clone(),
            rebuild_triggered_at: self.state.read().await.rebuild_triggered_at,
        };
        *self.state.write().await = new_state.clone();
        if let Err(e) = save_state(&new_state).await {
            warn!(
                target: "nasty::secure_boot_enrollment",
                "could not persist post-enrollment state: {e}"
            );
        }
        info!(
            target: "nasty::secure_boot_enrollment",
            "secure boot transitioned to enabled — entering PostEnrollment, {} stale TPM binding(s) to re-seal",
            match &new_state.phase {
                EnrollmentPhase::PostEnrollment { stale_tpm_bindings, .. } => stale_tpm_bindings.len(),
                _ => 0,
            }
        );
    }
}

async fn load_state() -> Option<EnrollmentState> {
    let bytes = tokio::fs::read(STATE_PATH).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn save_state(state: &EnrollmentState) -> Result<(), EnrollmentError> {
    if let Some(parent) = Path::new(STATE_PATH).parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        warn!(target: "nasty::secure_boot_enrollment", "create_dir_all({STATE_DIR}): {e}");
    }
    let body = serde_json::to_vec_pretty(state).map_err(|e| EnrollmentError::Io(e.to_string()))?;
    tokio::fs::write(STATE_PATH, body).await?;
    Ok(())
}

async fn write_overlay_file() -> Result<(), EnrollmentError> {
    let nix_dir = Path::new(NIX_OVERLAY_PATH)
        .parent()
        .ok_or_else(|| EnrollmentError::Io(format!("{NIX_OVERLAY_PATH}: no parent dir")))?;
    if !nix_dir.exists() {
        return Err(EnrollmentError::Io(format!(
            "{} does not exist — is this a NASty install?",
            nix_dir.display()
        )));
    }
    tokio::fs::write(NIX_OVERLAY_PATH, OVERLAY_BODY).await?;
    Ok(())
}

async fn remove_overlay_file() -> Result<(), EnrollmentError> {
    match tokio::fs::remove_file(NIX_OVERLAY_PATH).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Enumerate filesystems that currently have a `.tpm` blob in
/// `/var/lib/nasty/keys/`. After SB transitions, every one of these
/// has been sealed against the old (pre-SB) PCR-7 reading and needs
/// re-binding. Returns the bare filesystem names (the part before
/// `.tpm`). Filenames not matching the expected shape are skipped
/// silently — keys/ is shared with `.key` and other future per-FS
/// artifacts.
/// Query systemd for the wizard-driven rebuild's live state.
/// Always succeeds; failure modes (systemctl missing, unit
/// unknown, journalctl unhappy) all collapse to `NotRun` /
/// empty-tail so the wizard always has something to render.
async fn query_rebuild_snapshot() -> RebuildSnapshot {
    let show = Command::new("systemctl")
        .args([
            "show",
            REBUILD_UNIT,
            "--property=ActiveState,Result,ExecMainStatus",
        ])
        .output()
        .await;
    let mut active_state = String::new();
    let mut result_field = String::new();
    let mut exec_status: Option<i32> = None;
    if let Ok(out) = &show {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(v) = line.strip_prefix("ActiveState=") {
                active_state = v.to_string();
            } else if let Some(v) = line.strip_prefix("Result=") {
                result_field = v.to_string();
            } else if let Some(v) = line.strip_prefix("ExecMainStatus=") {
                exec_status = v.parse().ok();
            }
        }
    }

    let status = match active_state.as_str() {
        "" | "inactive" => {
            // No record of the unit (never run, or fully cleaned up
            // by --collect). Distinguish "ran and succeeded" from
            // "never ran" via the Result field — systemd remembers
            // it briefly even after the unit terminates.
            if result_field == "success" && exec_status == Some(0) {
                RebuildStatus::Succeeded
            } else if result_field == "exit-code" || exec_status.unwrap_or(0) != 0 {
                RebuildStatus::Failed
            } else {
                RebuildStatus::NotRun
            }
        }
        "active" | "activating" | "reloading" | "deactivating" => RebuildStatus::Running,
        "failed" => RebuildStatus::Failed,
        _ => RebuildStatus::NotRun,
    };

    // Tail of the unit's journal — cheap to fetch, surfaces real
    // error messages when the rebuild failed. Capped at 20 lines
    // so a runaway rebuild doesn't pump the RPC response full of
    // log spam.
    let journal = Command::new("journalctl")
        .args([
            "-u",
            REBUILD_UNIT,
            "-n",
            "20",
            "--no-pager",
            "--output",
            "short-iso",
        ])
        .output()
        .await;
    let journal_tail = match journal {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.starts_with("-- "))
            .map(|s| s.to_string())
            .collect(),
        Err(e) => {
            warn!(
                target: "nasty::secure_boot_enrollment",
                "journalctl -u {REBUILD_UNIT}: {e}"
            );
            Vec::new()
        }
    };

    RebuildSnapshot {
        status,
        exit_code: exec_status,
        journal_tail,
    }
}

async fn enumerate_tpm_bindings() -> Vec<String> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(NASTY_KEYS_DIR).await {
        Ok(e) => e,
        Err(_) => return out,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".tpm")
            && !stem.is_empty()
        {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The Nix overlay content. Writes both `services.nasty.secureBoot.enable
/// = true` (which the nasty module then turns into the lanzaboote
/// enable + systemd-boot disable wired in PR #325) AND the auto-
/// enroll bits that make next boot under firmware Setup Mode commit
/// the keys.
const OVERLAY_BODY: &str = r#"# Generated by nasty-engine — Secure Boot enrollment ceremony.
# Remove this file (or use Abort in the WebUI wizard) to roll back
# before firmware enrollment fires. After enrollment has committed,
# rolling back also requires entering BIOS and disabling Secure Boot.
{ ... }: {
  services.nasty.secureBoot.enable = true;
  # Stage PK / KEK / db on the ESP under /loader/keys/auto/. On the
  # next boot, systemd-boot enrolls them into firmware — but only
  # when firmware is in Setup Mode. The wizard tells the operator
  # to enter Setup Mode before rebooting; if they reboot without it
  # the keys stay staged and nothing happens, which is fine — they
  # can try again.
  boot.lanzaboote.autoEnrollKeys.enable = true;
  boot.lanzaboote.autoEnrollKeys.autoReboot = true;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_body_contains_required_lines() {
        // Render quirks would be invisible until the rebuild fires
        // and produces a misleading error. Cheap to pin the exact
        // lines we depend on.
        assert!(OVERLAY_BODY.contains("services.nasty.secureBoot.enable = true;"));
        assert!(OVERLAY_BODY.contains("boot.lanzaboote.autoEnrollKeys.enable = true;"));
        assert!(OVERLAY_BODY.contains("boot.lanzaboote.autoEnrollKeys.autoReboot = true;"));
    }

    #[test]
    fn default_phase_is_not_started() {
        let s = EnrollmentState::default();
        assert!(matches!(s.phase, EnrollmentPhase::NotStarted));
        assert!(s.initiated_by.is_none());
    }

    #[test]
    fn phase_serde_roundtrip() {
        // Forward-compat: future variants need this discriminated-
        // tag shape (`kind: "post_enrollment"`) for `#[serde(other)]`-
        // style additions to deserialise cleanly.
        let phase = EnrollmentPhase::PostEnrollment {
            detected_at: 1234,
            stale_tpm_bindings: vec!["tank".into(), "data".into()],
        };
        let json = serde_json::to_string(&phase).unwrap();
        let back: EnrollmentPhase = serde_json::from_str(&json).unwrap();
        match back {
            EnrollmentPhase::PostEnrollment {
                detected_at,
                stale_tpm_bindings,
            } => {
                assert_eq!(detected_at, 1234);
                assert_eq!(stale_tpm_bindings, vec!["tank", "data"]);
            }
            other => panic!("expected PostEnrollment, got {other:?}"),
        }
    }
}
