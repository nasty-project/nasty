//! Whole-subvolume rollback orchestration (feature B).
//!
//! Rolling a subvolume back to a snapshot deletes the live subvolume and
//! recreates it from the snapshot — so everything currently using it must
//! be quiesced first and resumed after. The storage layer
//! (`SubvolumeService::rollback`) does the pure swap (and takes a safety
//! snapshot of the current state for undo); this engine layer wraps it
//! with the dependent cascade, mirroring `fs_lock::lock_with_dependents`.
//!
//! Order: stop apps → graceful-stop VMs (kill on timeout) → disable SMB/NFS
//! exports → swap → resume in reverse (best-effort). Backup jobs are left
//! alone (idempotent, like fs_lock). Block subvolumes (iSCSI/NVMe-oF/VM
//! disks via loop) are refused for now — their loop-detach + target quiesce
//! is a riskier follow-up.

use std::time::Duration;

use nasty_sharing::nfs::UpdateNfsShareRequest;
use nasty_sharing::smb::UpdateSmbShareRequest;
use nasty_storage::subvolume::{RollbackResult, RollbackSnapshotRequest, SubvolumeType};
use tracing::{info, warn};

use crate::AppState;
use crate::fs_lock::wait_until_stopped;
use crate::subvolume_dependents::find_all_subvolume_dependents;

const VM_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const VM_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Roll a subvolume back to a snapshot, quiescing and resuming its
/// dependents around the storage-layer swap. Returns the recreated
/// subvolume plus the safety-snapshot name (the undo point).
pub async fn rollback_with_dependents(
    state: &AppState,
    req: RollbackSnapshotRequest,
    owner_filter: Option<&str>,
) -> Result<RollbackResult, String> {
    let sv = state
        .subvolumes
        .get(&req.filesystem, &req.subvolume, owner_filter)
        .await
        .map_err(|e| e.to_string())?;

    // Block subvolumes need loop-detach + target quiesce — not yet handled.
    // Refuse early (clearer than failing mid-cascade), before touching anything.
    if sv.subvolume_type == SubvolumeType::Block || sv.block_device.is_some() {
        return Err(
            "rollback is not yet supported for block subvolumes (iSCSI / NVMe-oF / VM disks)"
                .to_string(),
        );
    }

    let subvol_path = sv.path.clone();
    let deps = find_all_subvolume_dependents(state)
        .await
        .into_iter()
        .find(|d| d.filesystem == req.filesystem && d.name == req.subvolume)
        .unwrap_or_default();

    // ── Quiesce: apps ──
    // docker stop is blocking (Docker's own stop-then-kill). A failure here
    // means something is wrong; abort before any storage change, with
    // nothing yet stopped to restart.
    for app in &deps.apps {
        info!("Rollback cascade: stopping app '{app}'");
        if let Err(e) = state.apps.stop(app).await {
            return Err(format!(
                "rollback aborted before any change: app '{app}' failed to stop ({e})"
            ));
        }
    }

    // ── Quiesce: VMs ──
    // Dependents carry VM *names*; stop/start take the VM *id* — resolve.
    let vm_ids = resolve_vm_ids(state, &deps.vms).await;
    for (id, name) in &vm_ids {
        if !is_vm_running(state, id).await {
            continue;
        }
        info!("Rollback cascade: graceful shutdown of VM '{name}'");
        if let Err(e) = state.vms.stop(id).await {
            warn!("Rollback: VM '{name}' stop request failed: {e}; force-killing");
        } else if wait_until_stopped(
            || async { !is_vm_running(state, id).await },
            VM_SHUTDOWN_TIMEOUT,
            VM_POLL_INTERVAL,
        )
        .await
        {
            continue;
        }
        if let Err(e) = state.vms.kill(id).await {
            // Restart the apps we already stopped before bailing out.
            resume_apps(state, &deps.apps).await;
            return Err(format!(
                "rollback aborted: VM '{name}' wouldn't shut down and force-kill failed ({e}). \
                 Stopped apps were restarted; manually stop the VM and retry."
            ));
        }
    }

    // ── Quiesce: shares ── disable SMB/NFS exports on this subvolume so
    // clients drop their handles during the swap. (Daemons stay up.)
    let smb_ids = set_smb_shares_enabled(state, &subvol_path, false).await;
    let nfs_ids = set_nfs_shares_enabled(state, &subvol_path, false).await;

    // ── Swap ──
    let result = state
        .subvolumes
        .rollback(req, owner_filter)
        .await
        .map_err(|e| e.to_string());

    // ── Resume (best-effort, regardless of swap outcome — log, don't abort) ──
    reenable_nfs_shares(state, &nfs_ids).await;
    reenable_smb_shares(state, &smb_ids).await;
    for (id, name) in &vm_ids {
        if let Err(e) = state.vms.start(id).await {
            warn!("Rollback: restart of VM '{name}' failed: {e}");
        }
    }
    resume_apps(state, &deps.apps).await;

    result
}

async fn resolve_vm_ids(state: &AppState, names: &[String]) -> Vec<(String, String)> {
    let Ok(vms) = state.vms.list().await else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(|n| {
            vms.iter()
                .find(|v| &v.config.name == n)
                .map(|v| (v.config.id.clone(), n.clone()))
        })
        .collect()
}

async fn is_vm_running(state: &AppState, id: &str) -> bool {
    state
        .vms
        .list()
        .await
        .ok()
        .and_then(|vms| vms.into_iter().find(|v| v.config.id == id))
        .is_some_and(|v| v.running)
}

async fn resume_apps(state: &AppState, apps: &[String]) {
    for app in apps {
        if let Err(e) = state.apps.start(app).await {
            warn!("Rollback: restart of app '{app}' failed: {e}");
        }
    }
}

fn path_in_subvol(path: &str, subvol_path: &str) -> bool {
    path == subvol_path || path.starts_with(&format!("{subvol_path}/"))
}

/// Disable (enabled=false) every SMB share whose path is under
/// `subvol_path`, returning the ids touched so they can be re-enabled.
async fn set_smb_shares_enabled(state: &AppState, subvol_path: &str, enabled: bool) -> Vec<String> {
    let mut touched = Vec::new();
    let Ok(shares) = state.smb.list().await else {
        return touched;
    };
    for s in shares {
        if s.enabled != enabled && path_in_subvol(&s.path, subvol_path) {
            info!(
                "Rollback cascade: {} SMB share '{}'",
                if enabled { "re-enabling" } else { "disabling" },
                s.name
            );
            if let Err(e) = state.smb.update(smb_enabled_req(&s.id, enabled)).await {
                warn!("Rollback: toggling SMB share '{}' failed: {e}", s.name);
            } else {
                touched.push(s.id);
            }
        }
    }
    touched
}

async fn reenable_smb_shares(state: &AppState, ids: &[String]) {
    for id in ids {
        if let Err(e) = state.smb.update(smb_enabled_req(id, true)).await {
            warn!("Rollback: re-enabling SMB share '{id}' failed: {e}");
        }
    }
}

async fn set_nfs_shares_enabled(state: &AppState, subvol_path: &str, enabled: bool) -> Vec<String> {
    let mut touched = Vec::new();
    let Ok(shares) = state.nfs.list().await else {
        return touched;
    };
    for s in shares {
        if s.enabled != enabled && path_in_subvol(&s.path, subvol_path) {
            info!(
                "Rollback cascade: {} NFS share ({})",
                if enabled { "re-enabling" } else { "disabling" },
                s.path
            );
            if let Err(e) = state.nfs.update(nfs_enabled_req(&s.id, enabled)).await {
                warn!("Rollback: toggling NFS share '{}' failed: {e}", s.id);
            } else {
                touched.push(s.id);
            }
        }
    }
    touched
}

async fn reenable_nfs_shares(state: &AppState, ids: &[String]) {
    for id in ids {
        if let Err(e) = state.nfs.update(nfs_enabled_req(id, true)).await {
            warn!("Rollback: re-enabling NFS share '{id}' failed: {e}");
        }
    }
}

fn smb_enabled_req(id: &str, enabled: bool) -> UpdateSmbShareRequest {
    UpdateSmbShareRequest {
        id: id.to_string(),
        name: None,
        comment: None,
        read_only: None,
        browseable: None,
        guest_ok: None,
        valid_users: None,
        extra_params: None,
        time_machine: None,
        time_machine_max_size_gib: None,
        enabled: Some(enabled),
    }
}

fn nfs_enabled_req(id: &str, enabled: bool) -> UpdateNfsShareRequest {
    UpdateNfsShareRequest {
        id: id.to_string(),
        comment: None,
        clients: None,
        enabled: Some(enabled),
    }
}

#[cfg(test)]
mod tests {
    use super::path_in_subvol;

    #[test]
    fn path_in_subvol_matches_subtree_only() {
        let base = "/fs/tank/photos";
        // The subvolume root and anything under it count.
        assert!(path_in_subvol(base, base));
        assert!(path_in_subvol("/fs/tank/photos/2024/img.jpg", base));
        // A sibling that merely shares the prefix must NOT match.
        assert!(!path_in_subvol("/fs/tank/photos-backup", base));
        // An unrelated path doesn't match.
        assert!(!path_in_subvol("/fs/tank/docs", base));
    }
}
