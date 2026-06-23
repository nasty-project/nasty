//! Manual disk-type override for VM environments (#552).
//!
//! In VMs, `lsblk`'s ROTA bit is unreliable — virtio-scsi / VMware
//! PVSCSI / LSI / Hyper-V virtual disks almost all report `ROTA=1`, so
//! solid-state-backed storage is misclassified as `hdd`. This module
//! lets an operator pin a disk's type from the `/disks` page.
//!
//! The hard part is **identity**: `/dev/sdX` drifts across reboots (a
//! disk can move from `sdd` to `sda`), so an override must be keyed to
//! something stable. Field data across hypervisors showed:
//!   - Proxmox virtio / Hyper-V / VMware-SATA+NVMe: have a unique
//!     `wwn-*` / `nvme-eui.*` / `scsi-3<NAA>` by-id link.
//!   - VMware PVSCSI / LSI virtual disks: NO serial, NO WWN, no
//!     whole-disk by-id link — but every disk still has a stable
//!     `/dev/disk/by-path/` (`ID_PATH`) entry.
//!   - VMware SATA serials are a constant `00000000000000000001`, so
//!     serial-based keys can COLLIDE — they're deliberately not used.
//!
//! So the key is chosen first-present from:
//!   1. a unique by-id link (`wwn-` / `nvme-eui.` / `scsi-3<NAA>`) — `hardware`
//!   2. the `by-path` link — `slot` (reboot-stable; changes only if the
//!      disk is re-slotted in the VM config)
//!   3. the `/dev/sdX` name — `volatile` (last resort, won't survive
//!      re-lettering; surfaced as such in the UI)

use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::Mutex;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/disk-type-overrides.json";
const BY_ID_DIR: &str = "/dev/disk/by-id";
const BY_PATH_DIR: &str = "/dev/disk/by-path";

/// Lock around the override state file so concurrent RPCs can't tear it.
static STATE_LOCK: Mutex<()> = Mutex::const_new(());

/// Request to set (or clear) a disk's type. `device_class` of `auto`
/// (or empty) clears the override and restores detection.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DiskTypeUpdate {
    /// Current device path (e.g. `/dev/sda`) — resolved to a stable key.
    pub path: String,
    /// `ssd` | `hdd` | `nvme` | `auto`.
    pub device_class: String,
}

/// Persisted overrides, keyed by the chosen stable identity string.
pub type DiskTypeOverrides = HashMap<String, String>;

pub async fn load() -> DiskTypeOverrides {
    let _guard = STATE_LOCK.lock().await;
    nasty_common::load_singleton_or_recover(STATE_PATH).await
}

/// Map a stored class to `(device_class, rotational)`. `nvme`/`ssd` are
/// non-rotational; `hdd` is rotational.
pub fn class_to_fields(class: &str) -> Option<(String, bool)> {
    match class {
        "ssd" => Some(("ssd".to_string(), false)),
        "hdd" => Some(("hdd".to_string(), true)),
        "nvme" => Some(("nvme".to_string(), false)),
        _ => None,
    }
}

/// Pick the stable key + its kind for a disk, given the by-id link
/// basenames that resolve to it, its by-path basename, and the `/dev`
/// name. Pure so the priority logic is unit-tested. Returns
/// `(key, kind)` where `kind` ∈ `hardware` | `slot` | `volatile`.
pub fn choose_stable_id(
    by_id_names: &[String],
    by_path: Option<&str>,
    dev_name: &str,
) -> (String, &'static str) {
    // Prefer a globally-unique hardware id. Deliberately skip serial- and
    // model-based by-id links (ata-*, scsi-0ATA_*, scsi-SATA_*, nvme-<model>)
    // because VMware reuses a constant serial across SATA disks.
    let hardware = by_id_names
        .iter()
        .find(|n| n.starts_with("wwn-") || n.starts_with("nvme-eui.") || n.starts_with("scsi-3"));
    if let Some(id) = hardware {
        return (id.clone(), "hardware");
    }
    if let Some(p) = by_path {
        return (p.to_string(), "slot");
    }
    (dev_name.to_string(), "volatile")
}

/// Read `/dev/disk/by-id` and `/dev/disk/by-path`, returning reverse maps
/// from a disk's `/dev` basename (e.g. `sda`) to the link basenames that
/// resolve to it. Partition links (`*-partN`) are excluded. Best-effort:
/// a missing dir yields an empty map (e.g. a disk with no stable id).
async fn read_link_maps() -> (HashMap<String, Vec<String>>, HashMap<String, String>) {
    let by_id = read_link_dir(BY_ID_DIR).await;
    let by_path = read_link_dir(BY_PATH_DIR).await;
    // by-path is one-per-disk; collapse to a single basename.
    let by_path_single: HashMap<String, String> = by_path
        .into_iter()
        .filter_map(|(dev, mut links)| {
            links.sort();
            links.into_iter().next().map(|l| (dev, l))
        })
        .collect();
    (by_id, by_path_single)
}

async fn read_link_dir(dir: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return map,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip partition links — we key whole disks only.
        if name.contains("-part") {
            continue;
        }
        let Ok(target) = tokio::fs::read_link(entry.path()).await else {
            continue;
        };
        // Targets look like `../../sda`; take the final component.
        if let Some(dev) = target.file_name().and_then(|s| s.to_str()) {
            map.entry(dev.to_string()).or_default().push(name);
        }
    }
    map
}

/// Identity (key + kind) for a single `/dev` path, recomputed live.
/// Used by the set/clear RPC to resolve the override key.
pub async fn identity_for_path(path: &str) -> (String, &'static str) {
    let dev_name = path.trim_start_matches("/dev/").to_string();
    let (by_id, by_path) = read_link_maps().await;
    choose_stable_id(
        by_id.get(&dev_name).map(|v| v.as_slice()).unwrap_or(&[]),
        by_path.get(&dev_name).map(|s| s.as_str()),
        &dev_name,
    )
}

/// Resolver bound to one listing pass: precomputes the link maps once,
/// then answers `(stable_id, kind)` per disk synchronously so the
/// (sync) device-collection walk can attach identity without re-reading
/// the dirs for every device.
pub struct IdentityResolver {
    by_id: HashMap<String, Vec<String>>,
    by_path: HashMap<String, String>,
}

impl IdentityResolver {
    pub async fn new() -> Self {
        let (by_id, by_path) = read_link_maps().await;
        Self { by_id, by_path }
    }

    pub fn resolve(&self, dev_name: &str) -> (String, &'static str) {
        choose_stable_id(
            self.by_id
                .get(dev_name)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            self.by_path.get(dev_name).map(|s| s.as_str()),
            dev_name,
        )
    }
}

/// Set or clear the override for `update.path`. Returns the resolved
/// stable key so callers can confirm what was pinned.
pub async fn set(update: DiskTypeUpdate) -> Result<String, String> {
    let (key, kind) = identity_for_path(&update.path).await;
    let clearing = update.device_class == "auto" || update.device_class.is_empty();

    if !clearing && class_to_fields(&update.device_class).is_none() {
        return Err(format!(
            "invalid device class '{}': expected ssd, hdd, nvme, or auto",
            update.device_class
        ));
    }

    let _guard = STATE_LOCK.lock().await;
    let mut overrides: DiskTypeOverrides =
        nasty_common::load_singleton_or_recover(STATE_PATH).await;

    if clearing {
        overrides.remove(&key);
    } else {
        overrides.insert(key.clone(), update.device_class.clone());
    }

    let json = serde_json::to_string_pretty(&overrides).map_err(|e| format!("serialize: {e}"))?;
    if let Some(parent) = std::path::Path::new(STATE_PATH).parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        warn!("create {}: {e}", parent.display());
    }
    tokio::fs::write(STATE_PATH, json)
        .await
        .map_err(|e| format!("write {STATE_PATH}: {e}"))?;

    info!(
        "Disk type override for {} ({kind} key '{key}'): {}",
        update.path,
        if clearing {
            "cleared".to_string()
        } else {
            update.device_class.clone()
        }
    );
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_wwn_over_path_and_dev() {
        let by_id = vec![
            "scsi-0QEMU_QEMU_HARDDISK_drive-scsi0".to_string(),
            "wwn-0x5000c291082e5f93".to_string(),
        ];
        let (key, kind) = choose_stable_id(&by_id, Some("pci-0000:00:10.0-scsi-0:0:0:0"), "sdd");
        assert_eq!(key, "wwn-0x5000c291082e5f93");
        assert_eq!(kind, "hardware");
    }

    #[test]
    fn nvme_eui_and_scsi_naa_count_as_hardware() {
        let (k1, kind1) = choose_stable_id(
            &["nvme-eui.9dfd990a196977a6000c296a31919277".to_string()],
            Some("pci-0000:04:00.0-nvme-1"),
            "nvme0n1",
        );
        assert_eq!(kind1, "hardware");
        assert!(k1.starts_with("nvme-eui."));

        let (k2, kind2) = choose_stable_id(&["scsi-35000c291082e5f93".to_string()], None, "sdd");
        assert_eq!(kind2, "hardware");
        assert_eq!(k2, "scsi-35000c291082e5f93");
    }

    #[test]
    fn skips_serial_and_model_based_ids_then_falls_to_by_path() {
        // VMware PVSCSI/LSI disk: only serial/model-ish by-id (or none) +
        // a by-path. Serial-based links must be skipped (constant serial
        // collides), so we land on the by-path key.
        let by_id = vec![
            "scsi-0ATA_VMware_Virtual_S_00000000000000000001".to_string(),
            "scsi-SATA_VMware_Virtual_S_00000000000000000001".to_string(),
        ];
        let (key, kind) = choose_stable_id(&by_id, Some("pci-0000:13:00.0-sas-phy0-lun-0"), "sdc");
        assert_eq!(key, "pci-0000:13:00.0-sas-phy0-lun-0");
        assert_eq!(kind, "slot");
    }

    #[test]
    fn falls_back_to_dev_name_when_nothing_stable() {
        let (key, kind) = choose_stable_id(&[], None, "sda");
        assert_eq!(key, "sda");
        assert_eq!(kind, "volatile");
    }

    #[test]
    fn class_mapping_sets_rotational_correctly() {
        assert_eq!(class_to_fields("ssd"), Some(("ssd".to_string(), false)));
        assert_eq!(class_to_fields("nvme"), Some(("nvme".to_string(), false)));
        assert_eq!(class_to_fields("hdd"), Some(("hdd".to_string(), true)));
        assert_eq!(class_to_fields("auto"), None);
        assert_eq!(class_to_fields("garbage"), None);
    }
}
