//! Aggregate "what depends on this subvolume" across services.
//!
//! Sibling to `fs_dependents`, one level deeper: instead of asking
//! "what's on filesystem X", asks "what's in subvolume Y". Powers the
//! Usage column on the Subvolumes page (issue #81).
//!
//! Matching strategy is path-prefix based, same as fs_dependents:
//! a path "belongs to" a subvolume when it's the subvolume's mount
//! point or lives under `<mount_point>/`. Block subvolumes also
//! match by their `block_device` (e.g. `/dev/loopN`) so VM disks
//! and iSCSI/NVMe-oF backstores that reference them by device path
//! get attributed to the right subvolume too.
//!
//! Apps are attributed coarsely: every NASty-managed app lives under
//! `apps.status().storage_path` (the apps-data subvolume), so they
//! all attribute to whichever subvolume owns that path. Per-app
//! bind mounts outside that storage are a refinement that would need
//! per-app inspect — left for later, same as in fs_dependents.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Serialize;

use crate::AppState;

/// Names/IDs of every downstream entity that lives inside (or is
/// backed by) a given subvolume. Empty fields serialise as `[]` so
/// the WebUI can render without null-checking.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SubvolumeDependents {
    pub filesystem: String,
    pub name: String,
    pub path: String,
    pub apps: Vec<String>,
    pub vms: Vec<String>,
    pub backup_jobs: Vec<String>,
    pub nfs_shares: Vec<String>,
    pub smb_shares: Vec<String>,
    pub iscsi_targets: Vec<String>,
    pub nvmeof_subsystems: Vec<String>,
}

/// Find the subvolume (by path) that owns `target`. Returns `None`
/// when no subvolume's path is a prefix of `target` — orphaned paths
/// (sitting on the FS root outside any subvolume, or pointing somewhere
/// entirely off-tree) contribute to nothing, which is the conservative
/// behaviour. `paths_desc` must be sorted longest-first so the prefix
/// match picks the most-specific subvolume:
/// `/fs/tank/apps/grafana/data` attributes to `/fs/tank/apps/grafana`
/// rather than `/fs/tank` when both happen to exist as subvolumes.
fn owning_subvol<'a>(paths_desc: &'a [String], target: &str) -> Option<&'a str> {
    for p in paths_desc {
        if target == p.as_str() || target.starts_with(&format!("{p}/")) {
            return Some(p);
        }
    }
    None
}

/// Walk every downstream service once, and bucket every reference into
/// the subvolume that owns its path. Batched (rather than per-subvolume)
/// because the Usage column wants the value for every row at once —
/// per-subvolume would mean N × cost-of-listing-each-service round-trips
/// per page load, where the dominant cost (apps.list = a Docker
/// round-trip) doesn't get cheaper at finer granularity.
pub async fn find_all_subvolume_dependents(state: &AppState) -> Vec<SubvolumeDependents> {
    let subvols = state
        .subvolumes
        .list_all(None, None)
        .await
        .unwrap_or_default();

    // Seed the result: one entry per known subvolume, keyed by path so
    // attribution is a hashmap lookup, not a linear scan per service hit.
    let mut by_path: HashMap<String, SubvolumeDependents> = HashMap::new();
    // Index of block-device → subvolume path, for the loop-backed case.
    let mut by_block_dev: HashMap<String, String> = HashMap::new();
    // Subvolume paths sorted longest-first so the prefix match picks
    // the most-specific subvolume — `/fs/tank/apps/grafana/data`
    // attributes to `/fs/tank/apps/grafana` rather than `/fs/tank` if
    // both happen to exist.
    let mut paths_desc: Vec<String> = Vec::with_capacity(subvols.len());

    for sv in &subvols {
        by_path.insert(
            sv.path.clone(),
            SubvolumeDependents {
                filesystem: sv.filesystem.clone(),
                name: sv.name.clone(),
                path: sv.path.clone(),
                ..Default::default()
            },
        );
        if let Some(bd) = sv.block_device.clone() {
            by_block_dev.insert(bd, sv.path.clone());
        }
        paths_desc.push(sv.path.clone());
    }
    paths_desc.sort_by_key(|p| std::cmp::Reverse(p.len()));

    // Apps. Coarse: every managed app inherits the subvolume that
    // hosts the apps storage path. Per-app bind mounts that escape
    // that path are a refinement worth wiring later if anyone asks
    // (it'd need `apps.config` per app rather than `apps.list`).
    let apps_status = state.apps.status().await;
    if let Some(storage_path) = apps_status.storage_path.as_deref()
        && let Some(owning) = owning_subvol(&paths_desc, storage_path)
        && let Some(deps) = by_path.get_mut(owning)
        && let Ok(apps) = state.apps.list().await
    {
        deps.apps = apps.into_iter().map(|a| a.name).collect();
    }

    // VMs. Each VM disk path either points into a subvolume (image
    // file) or *is* a subvolume's block_device (loop-backed). Dedup
    // per-VM because a VM with multiple disks on the same subvolume
    // should appear once, not once per disk.
    if let Ok(vms) = state.vms.list().await {
        for vm in vms {
            let mut owners: std::collections::HashSet<String> = std::collections::HashSet::new();
            for disk in &vm.config.disks {
                if let Some(p) = owning_subvol(&paths_desc, &disk.path) {
                    owners.insert(p.to_string());
                }
                if let Some(p) = by_block_dev.get(&disk.path) {
                    owners.insert(p.clone());
                }
            }
            for path in owners {
                if let Some(deps) = by_path.get_mut(&path) {
                    deps.vms.push(vm.config.name.clone());
                }
            }
        }
    }

    // Backup jobs.
    let profiles = state.backups.list_profiles().await;
    for p in profiles {
        let mut owners: std::collections::HashSet<String> = std::collections::HashSet::new();
        for src in &p.sources {
            if let Some(o) = owning_subvol(&paths_desc, src) {
                owners.insert(o.to_string());
            }
        }
        for path in owners {
            if let Some(deps) = by_path.get_mut(&path) {
                deps.backup_jobs.push(p.name.clone());
            }
        }
    }

    // NFS shares.
    if let Ok(shares) = state.nfs.list().await {
        for s in shares {
            if let Some(p) = owning_subvol(&paths_desc, &s.path)
                && let Some(deps) = by_path.get_mut(p)
            {
                deps.nfs_shares.push(s.id);
            }
        }
    }

    // SMB shares.
    if let Ok(shares) = state.smb.list().await {
        for s in shares {
            if let Some(p) = owning_subvol(&paths_desc, &s.path)
                && let Some(deps) = by_path.get_mut(p)
            {
                deps.smb_shares.push(s.name);
            }
        }
    }

    // iSCSI targets. Each LUN's backstore_path is either a file path
    // (image-backed) or a block device (loop-backed).
    if let Ok(targets) = state.iscsi.list().await {
        for t in targets {
            let mut owners: std::collections::HashSet<String> = std::collections::HashSet::new();
            for l in &t.luns {
                if let Some(p) = owning_subvol(&paths_desc, &l.backstore_path) {
                    owners.insert(p.to_string());
                }
                if let Some(p) = by_block_dev.get(&l.backstore_path) {
                    owners.insert(p.clone());
                }
            }
            for path in owners {
                if let Some(deps) = by_path.get_mut(&path) {
                    deps.iscsi_targets.push(t.iqn.clone());
                }
            }
        }
    }

    // NVMe-oF subsystems.
    if let Ok(subs) = state.nvmeof.list().await {
        for s in subs {
            let mut owners: std::collections::HashSet<String> = std::collections::HashSet::new();
            for n in &s.namespaces {
                if let Some(p) = owning_subvol(&paths_desc, &n.device_path) {
                    owners.insert(p.to_string());
                }
                if let Some(p) = by_block_dev.get(&n.device_path) {
                    owners.insert(p.clone());
                }
            }
            for path in owners {
                if let Some(deps) = by_path.get_mut(&path) {
                    deps.nvmeof_subsystems.push(s.nqn.clone());
                }
            }
        }
    }

    // Return in input order so the Subvolumes page can pair entries
    // by index if it ever wants to (today it joins on path/name).
    subvols
        .into_iter()
        .filter_map(|sv| by_path.remove(&sv.path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owning_subvol_picks_longest_prefix() {
        // Sorted longest-first, same shape `find_all_subvolume_dependents`
        // builds it. Nested subvolume paths should win over their
        // shorter ancestors — a disk under /fs/tank/apps/grafana/data
        // attributes to /fs/tank/apps/grafana, not /fs/tank.
        let paths = vec![
            "/fs/tank/apps/grafana".to_string(),
            "/fs/tank/apps".to_string(),
            "/fs/tank".to_string(),
        ];
        assert_eq!(
            owning_subvol(&paths, "/fs/tank/apps/grafana/data/x.db"),
            Some("/fs/tank/apps/grafana")
        );
        assert_eq!(
            owning_subvol(&paths, "/fs/tank/apps/other-app/foo"),
            Some("/fs/tank/apps")
        );
        assert_eq!(
            owning_subvol(&paths, "/fs/tank/media/movie.mkv"),
            Some("/fs/tank")
        );
    }

    #[test]
    fn owning_subvol_exact_match_counts() {
        // The mount point itself is "in" the subvolume — needed so
        // an NFS share rooted exactly at /fs/tank/media (a common
        // case: share the whole subvolume) attributes correctly.
        let paths = vec!["/fs/tank/media".to_string()];
        assert_eq!(
            owning_subvol(&paths, "/fs/tank/media"),
            Some("/fs/tank/media")
        );
    }

    #[test]
    fn owning_subvol_rejects_sibling_prefix() {
        // Without the trailing-slash check, /fs/tank2/foo would falsely
        // match a subvolume at /fs/tank. Same trap fs_dependents has a
        // dedicated test for; we want the analogous guarantee here.
        let paths = vec!["/fs/tank".to_string()];
        assert_eq!(owning_subvol(&paths, "/fs/tank2/foo"), None);
        assert_eq!(owning_subvol(&paths, "/fs/tank2"), None);
    }

    #[test]
    fn owning_subvol_orphan_paths_return_none() {
        // A path entirely off-tree — engine state, a host bind mount,
        // an arbitrary /tmp file — has no owning subvolume. The caller
        // skips attribution rather than crediting it to whatever happens
        // to be first in the list.
        let paths = vec!["/fs/tank".to_string()];
        assert_eq!(owning_subvol(&paths, "/var/lib/nasty/foo"), None);
        assert_eq!(owning_subvol(&paths, "/tmp/whatever"), None);
        assert_eq!(owning_subvol(&paths, ""), None);
    }

    #[test]
    fn dependents_struct_defaults_serialise_as_empty_arrays() {
        // The WebUI renders the Usage column unconditionally — empty
        // fields must arrive as `[]`, not omitted, or the icons-row
        // map() would crash on undefined.
        let d = SubvolumeDependents::default();
        let j = serde_json::to_value(&d).unwrap();
        for k in [
            "apps",
            "vms",
            "backup_jobs",
            "nfs_shares",
            "smb_shares",
            "iscsi_targets",
            "nvmeof_subsystems",
        ] {
            assert!(j.get(k).unwrap().is_array(), "{k} should be an array");
            assert_eq!(j[k].as_array().unwrap().len(), 0);
        }
    }
}
