//! Lock-with-dependents orchestration for encrypted filesystems.
//!
//! Issue #86 follow-up to the impact-preview dialog (#124). When the
//! user clicks Lock and confirms, we want to actually carry out the
//! cascade — stop apps and VMs that touch the FS, then unmount and
//! revoke the key — instead of leaving the unmount to fail with EBUSY
//! and the user with a half-broken state.
//!
//! Order of operations:
//!   1. App containers (docker stop, blocking; uses Docker's own
//!      stop-then-kill timeout).
//!   2. VMs (QMP `system_powerdown`, then poll for shutdown up to
//!      `VM_SHUTDOWN_TIMEOUT`, fall back to `kill` if still running).
//!   3. The underlying `FilesystemService::lock(name)` (unmount +
//!      keyctl unlink, unchanged from before).
//!
//! Out of scope (deliberately, see PR description):
//! - Backup jobs: scheduled, idempotent on failure. Letting them try
//!   and fail post-lock is acceptable; their next run after unlock
//!   succeeds normally. Actively killing a running rclone/restic mid-
//!   transfer is a worse outcome than letting it error.
//! - Shares (NFS/SMB/iSCSI/NVMe-oF): the daemons tolerate a missing
//!   path. Clients see I/O errors that recover when the FS comes
//!   back. Stopping the daemons would also affect *other* shares on
//!   other filesystems, which is too broad.
//!
//! Failure modes:
//! - App stop fails → abort, report which app, leave anything we
//!   already stopped *stopped* (don't try to "undo" — adds another
//!   failure surface and the user just confirmed they want the lock).
//! - VM graceful shutdown times out → force-kill and continue. The
//!   user wanted the lock; an unresponsive guest doesn't get a veto.

use std::time::Duration;

use nasty_storage::filesystem::Filesystem;
use tracing::{info, warn};

use crate::AppState;
use crate::fs_dependents::find_dependents;

/// How long to wait for a VM to shut down gracefully before falling
/// back to kill. Generous because Windows guests can take 30+ seconds
/// to honor system_powerdown when busy.
const VM_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);

/// Polling interval for the VM-shutdown wait loop. 1s keeps the
/// total `lock` latency tight without thrashing the engine.
const VM_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Lock an encrypted filesystem after stopping its dependents. Wraps
/// the lower-level `FilesystemService::lock` so the storage layer
/// doesn't need to know about apps/VMs/shares — orchestration lives
/// here in the engine layer where `AppState` is.
pub async fn lock_with_dependents(state: &AppState, fs_name: &str) -> Result<Filesystem, String> {
    let deps = find_dependents(state, fs_name).await;

    // Apps first: docker stop is blocking, runs Docker's stop-then-kill
    // timeout, returns when the container is actually stopped. Errors
    // bubble up immediately because there's no clean recovery — if
    // we can't stop an app, the FS will refuse to unmount anyway.
    for app in &deps.apps {
        info!("Lock cascade: stopping app '{app}'");
        if let Err(e) = state.apps.stop(app).await {
            return Err(format!(
                "lock aborted: app '{app}' failed to stop ({e}). \
                 Stopped apps are not restarted automatically — \
                 see Apps page once the issue is resolved."
            ));
        }
    }

    // VMs: graceful shutdown via QMP, then poll. If the guest doesn't
    // honor it within VM_SHUTDOWN_TIMEOUT, force-kill. This matches
    // user intent: they confirmed they want the lock; an unresponsive
    // guest shouldn't block it.
    for vm_name in &deps.vms {
        if !is_vm_running(state, vm_name).await {
            continue;
        }
        info!("Lock cascade: requesting graceful shutdown of VM '{vm_name}'");
        if let Err(e) = state.vms.stop(vm_name).await {
            warn!("Lock cascade: VM '{vm_name}' stop request failed: {e}; trying kill");
        } else if wait_until_stopped(
            || async { !is_vm_running(state, vm_name).await },
            VM_SHUTDOWN_TIMEOUT,
            VM_POLL_INTERVAL,
        )
        .await
        {
            continue;
        }

        warn!(
            "Lock cascade: VM '{vm_name}' didn't shut down in {VM_SHUTDOWN_TIMEOUT:?}, force-killing"
        );
        if let Err(e) = state.vms.kill(vm_name).await {
            return Err(format!(
                "lock aborted: VM '{vm_name}' didn't shut down gracefully and force-kill failed ({e}). \
                 Manually stop the VM, then retry."
            ));
        }
    }

    // Now the FS is quiescent — drop to the storage-layer lock for the
    // actual unmount + key revoke.
    state
        .filesystems
        .lock(fs_name)
        .await
        .map_err(|e| e.to_string())
}

async fn is_vm_running(state: &AppState, name: &str) -> bool {
    state
        .vms
        .list()
        .await
        .ok()
        .and_then(|vms| vms.into_iter().find(|v| v.config.name == name))
        .is_some_and(|v| v.running)
}

/// Poll `predicate` every `interval` up to `timeout`. Returns true
/// when the predicate first becomes true, false on timeout. Pulled
/// out so the timeout/poll math is unit-testable without spinning up
/// real services.
pub(crate) async fn wait_until_stopped<F, Fut>(
    predicate: F,
    timeout: Duration,
    interval: Duration,
) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn wait_returns_true_immediately_when_predicate_starts_true() {
        // Common case: app stop returned, VM was already stopped, etc.
        // Don't spin pointlessly — first check should short-circuit.
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let result = wait_until_stopped(
            move || {
                let c = calls_c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    true
                }
            },
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .await;
        assert!(result);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_true_when_predicate_flips_mid_poll() {
        // Realistic scenario: VM takes 5s to shut down. Predicate
        // returns false initially, then true once shutdown completes.
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let result = wait_until_stopped(
            move || {
                let c = calls_c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    // Stops pretending-to-run on the 4th call (3 polls in).
                    n >= 3
                }
            },
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .await;
        assert!(result);
        assert!(calls.load(Ordering::SeqCst) >= 4);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_false_on_timeout() {
        // Stuck VM scenario: predicate stays false. Waiter must give
        // up at the deadline so caller can fall back to kill.
        let result = wait_until_stopped(
            || async { false },
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
        .await;
        assert!(!result);
    }
}
