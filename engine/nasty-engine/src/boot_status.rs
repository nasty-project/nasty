//! Per-phase boot status tracker shared between `main`'s restoration
//! sequence and the `/api/boot_status` REST endpoint.
//!
//! Part of #299. PR 1 made each startup phase non-fatal by wrapping
//! it in a `tokio::time::timeout`. This module captures the result
//! of each phase in a snapshot the WebUI can poll **without
//! authenticating** so it can render the "NASty is starting up"
//! overlay on the login screen — exactly when the engine isn't yet
//! ready to accept logins. After READY fires the same snapshot is
//! the source of truth for "did anything go wrong at boot" badges
//! in the dashboard.

use schemars::JsonSchema;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info};

/// Per-phase lifecycle state. The WebUI maps these to icons (clock /
/// spinner / check / cross) on the boot screen and on dashboard
/// badges. Order matters: `Pending → Running → {Ok | Failed}` is
/// the only legal transition path; nothing un-finishes.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PhaseState {
    /// Hasn't started yet. All phases begin here when the tracker
    /// is built so the WebUI sees the complete checklist even
    /// before the engine has visited any of them.
    Pending,
    /// Currently executing.
    Running,
    /// Completed within its budget without an error.
    Ok,
    /// Hit its `tokio::time::timeout` ceiling, or — if we ever wire
    /// in panic capture — crashed. The `error` field carries the
    /// reason for display in the UI.
    Failed,
}

/// One row in the boot checklist. Timestamps are milliseconds since
/// the engine process started — chosen over wall-clock so the UI
/// renders the same `1.2 s` regardless of system clock skew, and
/// because `Instant` already gives us a monotonic baseline that's
/// trivial to subtract.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BootPhase {
    /// Machine-readable name (`filesystems.restore_mounts`, etc.).
    /// Stable across releases so the WebUI can attach phase-specific
    /// remediation hints (e.g. "click for the Storage tab") by name.
    pub name: String,
    pub state: PhaseState,
    /// Milliseconds since process start when this phase entered
    /// `Running`. `None` while `Pending`.
    pub started_at_ms: Option<u64>,
    /// Milliseconds since process start when this phase transitioned
    /// to `Ok` or `Failed`. `None` while `Pending` or `Running`.
    pub finished_at_ms: Option<u64>,
    /// `finished_at_ms - started_at_ms` once the phase completes.
    /// Convenience field so the UI doesn't have to subtract.
    pub duration_ms: Option<u64>,
    /// The engine-side message when `state == Failed`. For the
    /// current timeout-only failure mode this looks like
    /// `"exceeded 60 s budget"`; future panic capture would surface
    /// the panic message here too.
    pub error: Option<String>,
}

/// What state the engine *as a whole* is in. The WebUI gates the
/// login screen on this: `Booting` → show the overlay; either
/// `Ready` value → show the login form (and stash the
/// `ReadyWithErrors` badge for after auth).
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverallState {
    Booting,
    /// All phases finished `Ok` (or were never registered).
    Ready,
    /// At least one phase ended in `Failed`. The engine is still
    /// accepting requests — the operator just needs to know
    /// something didn't come up cleanly.
    ReadyWithErrors,
}

/// Snapshot returned by `/api/boot_status`. Cheap to clone — the
/// tracker rebuilds this on every read by holding the RwLock briefly.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BootStatus {
    pub overall: OverallState,
    pub phases: Vec<BootPhase>,
    /// Unix seconds at which the engine process started. Lets the
    /// UI render absolute wall-clock times if it wants to.
    pub process_started_at_unix: i64,
    /// Milliseconds from process start to the call that flipped
    /// `overall` away from `Booting`. `None` while still booting.
    pub ready_at_ms: Option<u64>,
}

/// Shared handle. `Clone` is cheap — internally an `Arc` to the
/// `RwLock` so every component (the `run_phase` helper, the REST
/// handler, future RPC handlers) can hold its own copy without
/// fighting over ownership.
#[derive(Clone)]
pub struct BootStatusTracker {
    inner: Arc<RwLock<BootStatus>>,
    process_start: Instant,
}

impl BootStatusTracker {
    /// Build a fresh tracker with every phase pre-registered as
    /// `Pending`. Pre-registration matters for the UX: the WebUI
    /// can show the complete checklist immediately rather than
    /// having rows pop in as each phase starts.
    pub fn new(phase_names: &[&str]) -> Self {
        let phases = phase_names
            .iter()
            .map(|name| BootPhase {
                name: (*name).to_string(),
                state: PhaseState::Pending,
                started_at_ms: None,
                finished_at_ms: None,
                duration_ms: None,
                error: None,
            })
            .collect();
        let now_unix = chrono::Utc::now().timestamp();
        Self {
            inner: Arc::new(RwLock::new(BootStatus {
                overall: OverallState::Booting,
                phases,
                process_started_at_unix: now_unix,
                ready_at_ms: None,
            })),
            process_start: Instant::now(),
        }
    }

    pub async fn snapshot(&self) -> BootStatus {
        self.inner.read().await.clone()
    }

    /// Run a phase under its timeout budget, recording state
    /// transitions in the snapshot as they happen. The return value
    /// of the wrapped future is passed through on success;
    /// `T::default()` is returned on timeout (caller continues).
    ///
    /// Replaces the standalone `run_phase` helper PR 1 shipped; the
    /// behavioural contract is identical (logs match, defaults
    /// returned the same way) so the existing test plan still
    /// applies. The new bit is the snapshot write-through.
    pub async fn run_phase<T, F>(&self, name: &str, max: Duration, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
        T: Default,
    {
        self.mark_running(name).await;
        let started = Instant::now();
        match tokio::time::timeout(max, fut).await {
            Ok(v) => {
                let elapsed = started.elapsed();
                info!(
                    "boot phase '{name}' completed in {} ms",
                    elapsed.as_millis()
                );
                self.mark_finished(name, elapsed, None).await;
                v
            }
            Err(_) => {
                let elapsed = started.elapsed();
                let msg = format!("exceeded {} s budget", max.as_secs());
                error!(
                    "boot phase '{name}' {msg} — continuing without it; \
                     check the prior log lines for what stalled"
                );
                self.mark_finished(name, elapsed, Some(msg)).await;
                T::default()
            }
        }
    }

    async fn mark_running(&self, name: &str) {
        let now_ms = self.elapsed_ms();
        let mut snap = self.inner.write().await;
        if let Some(p) = snap.phases.iter_mut().find(|p| p.name == name) {
            p.state = PhaseState::Running;
            p.started_at_ms = Some(now_ms);
        } else {
            // Phase wasn't pre-registered. Append it instead of
            // dropping the update — the alternative would be a
            // silent gap in the snapshot whenever main.rs and the
            // tracker's expected-phase list drift apart.
            snap.phases.push(BootPhase {
                name: name.to_string(),
                state: PhaseState::Running,
                started_at_ms: Some(now_ms),
                finished_at_ms: None,
                duration_ms: None,
                error: None,
            });
        }
    }

    async fn mark_finished(&self, name: &str, elapsed: Duration, error: Option<String>) {
        let finished_ms = self.elapsed_ms();
        let duration_ms = elapsed.as_millis() as u64;
        let mut snap = self.inner.write().await;
        if let Some(p) = snap.phases.iter_mut().find(|p| p.name == name) {
            p.state = if error.is_some() {
                PhaseState::Failed
            } else {
                PhaseState::Ok
            };
            p.finished_at_ms = Some(finished_ms);
            p.duration_ms = Some(duration_ms);
            p.error = error;
        }
    }

    /// Call this exactly once after every phase has been awaited.
    /// Flips `overall` to `Ready` if all phases ended `Ok`, or to
    /// `ReadyWithErrors` if any ended `Failed`. Records the
    /// millisecond timestamp so the WebUI can show "engine reached
    /// READY at +14.2 s".
    pub async fn mark_ready(&self) {
        let now_ms = self.elapsed_ms();
        let mut snap = self.inner.write().await;
        let any_failed = snap.phases.iter().any(|p| p.state == PhaseState::Failed);
        snap.overall = if any_failed {
            OverallState::ReadyWithErrors
        } else {
            OverallState::Ready
        };
        snap.ready_at_ms = Some(now_ms);
    }

    fn elapsed_ms(&self) -> u64 {
        self.process_start.elapsed().as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pre_registered_phases_start_pending() {
        let t = BootStatusTracker::new(&["a", "b", "c"]);
        let snap = t.snapshot().await;
        assert_eq!(snap.overall, OverallState::Booting);
        assert_eq!(snap.phases.len(), 3);
        for p in &snap.phases {
            assert_eq!(p.state, PhaseState::Pending);
            assert!(p.started_at_ms.is_none());
            assert!(p.finished_at_ms.is_none());
        }
    }

    #[tokio::test]
    async fn successful_phase_transitions_pending_to_ok() {
        let t = BootStatusTracker::new(&["x"]);
        let result: u32 = t.run_phase("x", Duration::from_secs(5), async { 42 }).await;
        assert_eq!(result, 42);
        let snap = t.snapshot().await;
        let p = &snap.phases[0];
        assert_eq!(p.state, PhaseState::Ok);
        assert!(p.started_at_ms.is_some());
        assert!(p.finished_at_ms.is_some());
        assert!(p.duration_ms.is_some());
        assert!(p.error.is_none());
    }

    #[tokio::test]
    async fn timed_out_phase_transitions_pending_to_failed() {
        let t = BootStatusTracker::new(&["slow"]);
        // tokio time pausing isn't enabled by default for the test
        // binary; use a real (very short) sleep and a tiny budget.
        let result: u32 = t
            .run_phase("slow", Duration::from_millis(20), async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                7
            })
            .await;
        // T::default() returned on timeout.
        assert_eq!(result, 0);
        let snap = t.snapshot().await;
        let p = &snap.phases[0];
        assert_eq!(p.state, PhaseState::Failed);
        assert!(p.error.as_ref().unwrap().contains("exceeded"));
    }

    #[tokio::test]
    async fn unregistered_phase_is_appended_not_dropped() {
        // We could panic on an unknown phase, but the engine running
        // in production beats "lost a status update because the
        // tracker's expected-phase list got out of sync." Append it.
        let t = BootStatusTracker::new(&[]);
        let _: () = t
            .run_phase("surprise", Duration::from_secs(5), async {})
            .await;
        let snap = t.snapshot().await;
        assert_eq!(snap.phases.len(), 1);
        assert_eq!(snap.phases[0].name, "surprise");
        assert_eq!(snap.phases[0].state, PhaseState::Ok);
    }

    #[tokio::test]
    async fn mark_ready_picks_overall_state_from_phase_outcomes() {
        let t = BootStatusTracker::new(&["a", "b"]);
        let _: () = t.run_phase("a", Duration::from_secs(5), async {}).await;
        let _: () = t.run_phase("b", Duration::from_secs(5), async {}).await;
        t.mark_ready().await;
        assert_eq!(t.snapshot().await.overall, OverallState::Ready);

        let t = BootStatusTracker::new(&["a", "b"]);
        let _: () = t.run_phase("a", Duration::from_secs(5), async {}).await;
        let _: u32 = t
            .run_phase("b", Duration::from_millis(10), async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                0
            })
            .await;
        t.mark_ready().await;
        assert_eq!(t.snapshot().await.overall, OverallState::ReadyWithErrors);
    }

    #[tokio::test]
    async fn snapshot_before_ready_is_booting() {
        let t = BootStatusTracker::new(&["a"]);
        assert_eq!(t.snapshot().await.overall, OverallState::Booting);
    }
}
