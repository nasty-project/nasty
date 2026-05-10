//! Persistent vfio-pci passthrough configuration.
//!
//! Users mark PCI (vendor, device) ID pairs to be claimed by `vfio-pci`
//! at boot, before regular drivers can grab them. The flow:
//!
//! ```text
//! WebUI toggle
//!   ↓
//! state file: /var/lib/nasty/passthrough.json
//!   ↓
//! engine writes: /etc/nixos/passthrough.nix
//!   `{ boot.kernelParams = [ "vfio-pci.ids=AAAA:BBBB,CCCC:DDDD" ]; }`
//!   ↓
//! wrapper flake imports passthrough.nix
//!   ↓
//! next nixos-rebuild + reboot — vfio-pci grabs the device early
//! ```
//!
//! Passthrough state is keyed by **vendor:device IDs**, not BDFs, because
//! that's what `vfio-pci.ids=` consumes and because the binding survives
//! slot moves. Caveat: when two devices share a (vendor, device) pair
//! (e.g. two identical NICs), marking that pair claims **both**. The
//! `validate_request` check guards against the mgmt-iface case
//! specifically; deeper "is this device the boot disk's controller"
//! detection is a follow-up.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tokio::sync::Mutex;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/passthrough.json";
const NIX_PATH: &str = "/etc/nixos/passthrough.nix";

/// One PCI device-class identifier — the granularity vfio-pci.ids
/// operates at. Not a BDF; that's intentional.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, Hash,
)]
pub struct DeviceId {
    /// 4-hex-digit vendor ID, e.g. `10de` (NVIDIA). Lowercase.
    pub vendor: String,
    /// 4-hex-digit device ID, e.g. `2204` (RTX 3080). Lowercase.
    pub device: String,
}

impl DeviceId {
    /// Render as the `vendor:device` form vfio-pci.ids consumes.
    pub fn to_vfio_id(&self) -> String {
        format!("{}:{}", self.vendor, self.device)
    }
}

/// Persistent passthrough config — what we'd write into the kernel
/// command line on the next boot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PassthroughConfig {
    /// (vendor, device) pairs to bind to vfio-pci at boot. Order is
    /// not significant — we sort+dedupe on save and write.
    pub ids: Vec<DeviceId>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PassthroughUpdate {
    pub ids: Vec<DeviceId>,
}

/// Lock held only across reads/writes so concurrent RPCs can't tear
/// the state file. Initialized lazily — no global setup needed.
static STATE_LOCK: Mutex<()> = Mutex::const_new(());

pub async fn load() -> PassthroughConfig {
    let _guard = STATE_LOCK.lock().await;
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => PassthroughConfig::default(),
    }
}

/// Apply a request: validate against the management interface
/// collision, normalize (sort + dedupe), persist to disk, and
/// regenerate `/etc/nixos/passthrough.nix`. Returns the saved config.
///
/// `mgmt_iface` is the kernel iface name resolved from the calling
/// client's socket (same `mgmt_iface_for_peer` the network code uses
/// for risk classification). When `Some`, the validator refuses
/// requests that would claim a device matching the mgmt iface's
/// (vendor:device) pair.
///
/// The change is **not active until reboot**. Users see a "Reboot
/// required" banner; the engine doesn't trigger nixos-rebuild
/// automatically (mirrors how PR #113 handled hostname.nix).
pub async fn save_and_apply(
    update: PassthroughUpdate,
    mgmt_iface: Option<&str>,
) -> Result<PassthroughConfig, String> {
    let mut normalized = normalize_ids(update.ids);
    validate_request(&normalized, mgmt_iface).await?;
    // Defensive: lowercase IDs were already enforced by normalize_ids,
    // but if a caller bypassed it we still want canonical output.
    for id in &mut normalized {
        id.vendor = id.vendor.to_ascii_lowercase();
        id.device = id.device.to_ascii_lowercase();
    }
    let cfg = PassthroughConfig { ids: normalized };

    let _guard = STATE_LOCK.lock().await;
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| format!("serialize: {e}"))?;
    if let Some(parent) = std::path::Path::new(STATE_PATH).parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        warn!("create {}: {e}", parent.display());
    }
    tokio::fs::write(STATE_PATH, json)
        .await
        .map_err(|e| format!("write {STATE_PATH}: {e}"))?;
    write_nix_file(&cfg).await?;
    info!(
        "Passthrough config updated: {} device(s); reboot required to apply",
        cfg.ids.len()
    );
    Ok(cfg)
}

/// Render `cfg` as a NixOS module fragment and write it to
/// `/etc/nixos/passthrough.nix`. The wrapper flake imports this file
/// (with a `pathExists` fallback so a fresh install before any
/// passthrough toggling doesn't fail to evaluate).
///
/// Empty `cfg.ids` produces a no-op module rather than removing the
/// file — keeps the import contract stable regardless of state.
async fn write_nix_file(cfg: &PassthroughConfig) -> Result<(), String> {
    let nix_dir = std::path::Path::new(NIX_PATH).parent();
    let Some(nix_dir) = nix_dir else {
        return Err(format!("{NIX_PATH}: no parent dir"));
    };
    if !nix_dir.exists() {
        // Fresh install before rebootstrap. We can't write the file,
        // but state.json is saved so the next rebootstrap (which
        // creates /etc/nixos) can pick it up. Not an error.
        return Ok(());
    }

    let body = render_nix_module(cfg);
    tokio::fs::write(NIX_PATH, body)
        .await
        .map_err(|e| format!("write {NIX_PATH}: {e}"))
}

/// Pure renderer — returns the full content for `/etc/nixos/passthrough.nix`.
/// Always emits a valid module body, even when `cfg.ids` is empty.
fn render_nix_module(cfg: &PassthroughConfig) -> String {
    if cfg.ids.is_empty() {
        return "# Generated by nasty-engine. Empty passthrough config.\n\
                { ... }: { }\n"
            .to_string();
    }
    let joined = cfg
        .ids
        .iter()
        .map(|id| id.to_vfio_id())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "# Generated by nasty-engine. Reboot to apply.\n\
         #\n\
         # `vfio-pci.ids` is consumed at kernel boot to claim devices for\n\
         # passthrough before regular drivers (nvidia, e1000e, ...) bind.\n\
         # Source of truth: /var/lib/nasty/passthrough.json — edit via\n\
         # the WebUI's Hardware page rather than this file directly.\n\
         {{ ... }}: {{\n\
         \x20\x20boot.kernelParams = [ \"vfio-pci.ids={joined}\" ];\n\
         }}\n"
    )
}

/// Sort + dedupe; lowercase the hex strings. Skips obviously-malformed
/// entries (non-4-hex-digit ids) since they'd produce a kernel param
/// that fails to parse and silently disables the whole list.
fn normalize_ids(ids: Vec<DeviceId>) -> Vec<DeviceId> {
    let mut set = BTreeSet::new();
    for id in ids {
        let v = id.vendor.to_ascii_lowercase();
        let d = id.device.to_ascii_lowercase();
        if !is_hex4(&v) || !is_hex4(&d) {
            warn!(
                "ignoring malformed passthrough id: {}:{}",
                id.vendor, id.device
            );
            continue;
        }
        set.insert(DeviceId {
            vendor: v,
            device: d,
        });
    }
    set.into_iter().collect()
}

fn is_hex4(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Refuse requests that would claim the device on which the caller is
/// reaching the engine. Returns `Ok(())` if safe, `Err(reason)` if the
/// request would brick connectivity at next boot.
///
/// MVP scope:
/// - Resolve mgmt iface name → PCI BDF via `/sys/class/net/<iface>/device`
///   symlink. Skip the check if the iface isn't directly PCI (e.g.
///   it's a bridge or bond — the underlying member resolution is a
///   future enhancement; users with that topology are sophisticated
///   enough to verify manually).
/// - Read `/sys/bus/pci/devices/<BDF>/{vendor,device}`.
/// - Refuse if any requested id matches that pair.
///
/// Out of scope here (planned follow-ups):
/// - Boot disk's storage controller (refuse claiming it).
/// - IOMMU group-wide enforcement (some users do partial-group
///   passthrough with ACS overrides — refusing it outright would
///   block valid setups).
async fn validate_request(ids: &[DeviceId], mgmt_iface: Option<&str>) -> Result<(), String> {
    let Some(iface) = mgmt_iface else {
        // Caller couldn't resolve the management iface — fail safe by
        // letting the user proceed. The classifier in the network
        // module makes the same conservative choice.
        return Ok(());
    };
    let Some(mgmt_id) = mgmt_iface_pci_id(iface).await else {
        warn!(
            "passthrough: management iface '{iface}' has no resolvable PCI device — \
             skipping mgmt-collision check, user is on their own"
        );
        return Ok(());
    };
    if ids.contains(&mgmt_id) {
        return Err(format!(
            "refusing to claim {}:{} for passthrough — that pair matches the management \
             interface '{}', so vfio-pci would steal it at boot and lock you out. \
             Reach the box on a different interface (e.g. another NIC, or console) and \
             retry; or buy a second NIC of a different make/model.",
            mgmt_id.vendor, mgmt_id.device, iface
        ));
    }
    Ok(())
}

/// Resolve `<iface>` → DeviceId by reading the `/sys/class/net/<iface>/device`
/// symlink and the device's vendor/device sysfs files.
async fn mgmt_iface_pci_id(iface: &str) -> Option<DeviceId> {
    let device_link = format!("/sys/class/net/{iface}/device");
    let canonical = tokio::fs::canonicalize(&device_link).await.ok()?;
    let vendor = read_pci_id_field(&canonical, "vendor").await?;
    let device = read_pci_id_field(&canonical, "device").await?;
    Some(DeviceId { vendor, device })
}

async fn read_pci_id_field(dev_dir: &std::path::Path, field: &str) -> Option<String> {
    let raw = tokio::fs::read_to_string(dev_dir.join(field)).await.ok()?;
    let trimmed = raw.trim();
    Some(
        trimmed
            .strip_prefix("0x")
            .unwrap_or(trimmed)
            .to_ascii_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(v: &str, d: &str) -> DeviceId {
        DeviceId {
            vendor: v.into(),
            device: d.into(),
        }
    }

    #[test]
    fn vfio_id_renders_as_vendor_colon_device() {
        // Format consumed by the kernel's `vfio-pci.ids=` parameter.
        assert_eq!(id("10de", "2204").to_vfio_id(), "10de:2204");
    }

    #[test]
    fn normalize_lowercases_and_sorts_dedupes() {
        // Real input might come from upper-case lspci output or be
        // duplicated by overzealous UI clicking. Output should be
        // canonical: lowercase, sorted, no dupes.
        let raw = vec![
            id("8086", "1539"),
            id("10DE", "2204"),
            id("10de", "2204"),
            id("8086", "1539"),
        ];
        let out = normalize_ids(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], id("10de", "2204"));
        assert_eq!(out[1], id("8086", "1539"));
    }

    #[test]
    fn normalize_drops_malformed_ids() {
        // 4-hex-digit only. Non-hex or wrong length would generate a
        // kernel param like `vfio-pci.ids=10de:nope` which the kernel
        // silently rejects, taking the whole list with it.
        let raw = vec![
            id("10de", "2204"),
            id("zzzz", "0000"),
            id("10de", "abc"),   // 3 hex digits
            id("10de", "12345"), // 5 hex digits
            id("8086", "1539"),
        ];
        let out = normalize_ids(raw);
        assert_eq!(
            out,
            vec![id("10de", "2204"), id("8086", "1539")],
            "only well-formed ids should survive"
        );
    }

    #[test]
    fn render_nix_module_produces_kernel_param_with_joined_ids() {
        let cfg = PassthroughConfig {
            ids: vec![id("10de", "2204"), id("8086", "1539")],
        };
        let nix = render_nix_module(&cfg);
        // The crucial assertion: the joined `vfio-pci.ids=...` exactly
        // matches what the kernel expects, with comma separators and
        // colon-paired ids.
        assert!(nix.contains(r#"boot.kernelParams = [ "vfio-pci.ids=10de:2204,8086:1539" ];"#));
    }

    #[test]
    fn render_nix_module_emits_noop_when_ids_empty() {
        // When the user untoggles their last device, the file should
        // still be valid Nix so the wrapper flake's import keeps
        // evaluating cleanly. Removing the file would also work, but
        // emitting a no-op is simpler and idempotent.
        let cfg = PassthroughConfig::default();
        let nix = render_nix_module(&cfg);
        assert!(nix.contains("{ ... }: { }"));
        // No leftover kernelParams line.
        assert!(!nix.contains("kernelParams"));
    }

    #[tokio::test]
    async fn validate_passes_when_mgmt_iface_unknown() {
        // No mgmt info → conservative-pass, same convention as the
        // network risk classifier. Without it we couldn't toggle any
        // passthrough on a freshly-booted box where the engine
        // doesn't yet know which iface the user is on.
        let res = validate_request(&[id("10de", "2204")], None).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn validate_passes_when_mgmt_iface_has_no_pci_device() {
        // Bridges, bonds, and tunnel interfaces don't have a direct
        // PCI device. We log + skip rather than refuse.
        // Using a bogus iface name ensures the symlink lookup fails.
        let res = validate_request(&[id("10de", "2204")], Some("nonexistent-iface-zzz")).await;
        assert!(res.is_ok());
    }
}
