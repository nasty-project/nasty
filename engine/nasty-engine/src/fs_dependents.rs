//! Aggregate "what depends on this filesystem" across services.
//!
//! Backs the impact-preview dialog before destructive FS operations
//! (currently the encrypted-FS Lock action — see issue #86's discussion).
//! Walks each downstream service that can reference a filesystem path,
//! returns names/IDs grouped by service. Read-only — never mutates.
//!
//! Matching strategy: most services store a path. We treat a path as
//! "depending on FS X" when it lives under `/fs/<X>/` (the canonical
//! NASty mount root, see `NASTY_MOUNT_BASE` in nasty-storage). Paths
//! that point to `/dev/loopN` are mapped to the underlying subvolume's
//! filesystem via `Subvolume.block_device` — that's how VM disks and
//! iSCSI/NVMe-oF backstores get detected.
//!
//! This module deliberately doesn't `tokio::join!` the queries —
//! list-call latency is dominated by `apps.list()` (Docker round-trip)
//! and parallelizing the others doesn't move the wall clock noticeably.
//! Sequential keeps the failure mode trivial: one slow service blocks
//! the rest, but the dialog is acceptable up to several hundred ms.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::Serialize;

use crate::AppState;

/// Names/IDs of every downstream entity that depends on a given
/// filesystem. Empty fields are serialized as `[]` so the WebUI can
/// render unconditionally without null-checking.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FsDependents {
    pub filesystem: String,
    pub mounted: bool,
    pub subvolumes: Vec<String>,
    pub apps: Vec<String>,
    pub vms: Vec<String>,
    pub backup_jobs: Vec<String>,
    pub nfs_shares: Vec<String>,
    pub smb_shares: Vec<String>,
    pub iscsi_targets: Vec<String>,
    pub nvmeof_subsystems: Vec<String>,
}

/// True if `path` falls under `/fs/<fs_name>/` (the canonical NASty
/// mount root). Trailing slash matters: without it `/fs/tank2` would
/// match a query for FS `tank`.
fn path_belongs_to_fs(path: &str, fs_name: &str) -> bool {
    let prefix = format!("/fs/{fs_name}/");
    path.starts_with(&prefix) || path == format!("/fs/{fs_name}")
}

/// Build the dependents view by querying every service that can hold
/// a filesystem reference. Best-effort: a service that errors out
/// contributes an empty list rather than failing the whole query.
pub async fn find_dependents(state: &AppState, fs_name: &str) -> FsDependents {
    let mut out = FsDependents {
        filesystem: fs_name.to_string(),
        ..Default::default()
    };

    // Filesystem mount state — orients the UI message ("currently
    // mounted, will be unmounted" vs "already unmounted, only
    // revoking the key").
    if let Ok(fs) = state.filesystems.get(fs_name).await {
        out.mounted = fs.mounted;
    }

    // Subvolumes are the cheapest hop: we already filter by fs.
    let subvols = state
        .subvolumes
        .list_all(Some(fs_name), None)
        .await
        .unwrap_or_default();
    // Block devices owned by subvolumes on this FS — used to detect
    // VM disks / iSCSI backstores / NVMe-oF namespaces that reference
    // them by `/dev/loopN` rather than path.
    let block_devs: HashSet<String> = subvols
        .iter()
        .filter_map(|s| s.block_device.clone())
        .collect();
    out.subvolumes = subvols.into_iter().map(|s| s.name).collect();

    // Apps: when the docker storage is on this FS, every app is on
    // it (their layered images, named volumes, default bind base
    // all live under the apps storage path). Per-app bind mounts
    // outside that base are a refinement we can add later — they're
    // surfaced via app inspect, not the lightweight `list()`.
    let apps_status = state.apps.status().await;
    if apps_status
        .storage_path
        .as_deref()
        .is_some_and(|p| path_belongs_to_fs(p, fs_name))
        && let Ok(apps) = state.apps.list().await
    {
        out.apps = apps.into_iter().map(|a| a.name).collect();
    }

    // VMs: disk path either under /fs/<X>/... directly, or a loop
    // device that's the block_device of a subvolume on this FS.
    if let Ok(vms) = state.vms.list().await {
        for vm in vms {
            let touches_fs = vm
                .config
                .disks
                .iter()
                .any(|d| path_belongs_to_fs(&d.path, fs_name) || block_devs.contains(&d.path));
            if touches_fs {
                out.vms.push(vm.config.name);
            }
        }
    }

    // Backup jobs: any source under /fs/<X>/. A job pointing only
    // somewhere else (e.g. a subset of a different FS) is left alone.
    let profiles = state.backups.list_profiles().await;
    for p in profiles {
        if p.sources.iter().any(|src| path_belongs_to_fs(src, fs_name)) {
            out.backup_jobs.push(p.name);
        }
    }

    // Shares: NFS/SMB use a single `path`; iSCSI/NVMe-oF use device
    // paths that are usually loop devices for block subvolumes.
    if let Ok(shares) = state.nfs.list().await {
        out.nfs_shares = shares
            .into_iter()
            .filter(|s| path_belongs_to_fs(&s.path, fs_name))
            .map(|s| s.id)
            .collect();
    }
    if let Ok(shares) = state.smb.list().await {
        out.smb_shares = shares
            .into_iter()
            .filter(|s| path_belongs_to_fs(&s.path, fs_name))
            .map(|s| s.name)
            .collect();
    }
    if let Ok(targets) = state.iscsi.list().await {
        out.iscsi_targets = targets
            .into_iter()
            .filter(|t| {
                t.luns.iter().any(|l| {
                    path_belongs_to_fs(&l.backstore_path, fs_name)
                        || block_devs.contains(&l.backstore_path)
                })
            })
            .map(|t| t.iqn)
            .collect();
    }
    if let Ok(subs) = state.nvmeof.list().await {
        out.nvmeof_subsystems = subs
            .into_iter()
            .filter(|s| {
                s.namespaces.iter().any(|n| {
                    path_belongs_to_fs(&n.device_path, fs_name)
                        || block_devs.contains(&n.device_path)
                })
            })
            .map(|s| s.nqn)
            .collect();
    }

    out
}

/// Reverse-index of currently-locked encrypted filesystems → what
/// would come back to life if they were unlocked. Powers the
/// "🔒 on tank" badges on the Apps and VMs pages: those pages need
/// to know "is *my* app/VM blocked by a locked FS, and which one?"
/// without hitting `find_dependents` per FS in the browser.
///
/// Only includes FSes that are currently encrypted AND locked AND
/// have at least one app or VM among their dependents — empty
/// entries would just be wire bytes the UI filters back out.
pub async fn find_locked_dependents(state: &AppState) -> Vec<FsDependents> {
    let Ok(filesystems) = state.filesystems.list().await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for fs in filesystems {
        // Only encrypted-and-locked is interesting here. A plain
        // unmounted FS has no badge story; an encrypted-but-unlocked
        // one is just waiting for `fs.mount`.
        if fs.options.encrypted != Some(true) || fs.options.locked != Some(true) {
            continue;
        }
        let deps = find_dependents(state, &fs.name).await;
        if !deps.apps.is_empty() || !deps.vms.is_empty() {
            out.push(deps);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_match_handles_prefix_and_exact() {
        // Trailing slash matters — without it /fs/tank2 would falsely
        // match a query for /fs/tank.
        assert!(path_belongs_to_fs("/fs/tank/foo", "tank"));
        assert!(path_belongs_to_fs("/fs/tank/sub/file", "tank"));
        // Exact match (no trailing slash, no children) — the FS
        // mount-point itself, not a child path.
        assert!(path_belongs_to_fs("/fs/tank", "tank"));
        // Sibling-prefix attack: /fs/tank2 must not match `tank`.
        assert!(!path_belongs_to_fs("/fs/tank2/foo", "tank"));
        assert!(!path_belongs_to_fs("/fs/tank2", "tank"));
        // Other roots untouched.
        assert!(!path_belongs_to_fs("/var/lib/nasty", "tank"));
        assert!(!path_belongs_to_fs("", "tank"));
    }
}
