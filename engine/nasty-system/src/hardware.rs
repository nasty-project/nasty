//! Read-only hardware discovery: PCI device tree grouped by IOMMU group.
//!
//! This is the data backbone for the Hardware page in the WebUI. It's
//! intentionally narrow in scope — IOMMU groups, PCI devices, current
//! driver bindings — because that's the information passthrough users
//! actually need to plan VM device assignment, and it's all derivable
//! from sysfs (deterministic, fast, no vendor quirks).
//!
//! Broader hardware overview (motherboard, BIOS, DIMMs, USB) is a
//! separate concern handled via `inxi` in a follow-up; the parsing
//! risk profile there is completely different.
//!
//! Sources:
//! - `/sys/kernel/iommu_groups/<N>/devices/<BDF>` — group membership
//! - `/sys/bus/pci/devices/<BDF>/{vendor,device,class}` — numeric IDs
//! - `/sys/bus/pci/devices/<BDF>/driver` symlink — currently bound driver
//! - `lspci -nnvmm` — human-readable vendor / device / class names
//!   (joined in by BDF; sysfs has the IDs but not the names)

use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashMap;
use tracing::warn;

const IOMMU_GROUPS_DIR: &str = "/sys/kernel/iommu_groups";
const PCI_DEVICES_DIR: &str = "/sys/bus/pci/devices";

/// One IOMMU group with its constituent PCI devices. Groups are the
/// unit of passthrough — assigning any device in a group to a VM
/// effectively claims the whole group, so this view is what users
/// need to make passthrough decisions.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IommuGroup {
    pub id: u32,
    pub devices: Vec<PciDevice>,
}

/// One PCI device. Numeric IDs come from sysfs; human-readable names
/// come from `lspci -nnvmm` (when available) and may be `None` if
/// `lspci` isn't installed or the device is too new for the local
/// `pci.ids` database.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PciDevice {
    /// Bus:Device.Function in canonical sysfs form, e.g. `0000:01:00.0`.
    pub bdf: String,
    /// 4-hex-digit vendor ID, e.g. `10de` (NVIDIA).
    pub vendor_id: String,
    /// 4-hex-digit device ID, e.g. `2204` (RTX 3080).
    pub device_id: String,
    /// 4-hex-digit class code, e.g. `0300` (VGA controller).
    pub class_id: String,
    /// Human-readable vendor name (from pci.ids), if available.
    pub vendor_name: Option<String>,
    /// Human-readable device name (from pci.ids), if available.
    pub device_name: Option<String>,
    /// Human-readable class name (from pci.ids), if available.
    pub class_name: Option<String>,
    /// Currently bound kernel driver, e.g. `vfio-pci`, `nvidia`,
    /// `e1000e`. `None` if no driver is bound (rare — usually means
    /// the device is reserved for explicit binding).
    pub driver: Option<String>,
}

/// Walk `/sys/kernel/iommu_groups/` and build the full grouping. The
/// returned vec is sorted by group id (ascending) with devices inside
/// each group sorted by BDF — stable order so the UI doesn't shuffle
/// between refreshes.
///
/// Returns an empty vec if IOMMU isn't enabled at the kernel level
/// (`iommu_groups` directory absent). Caller surfaces that as
/// "IOMMU off" rather than as an error.
pub async fn iommu_groups() -> Vec<IommuGroup> {
    let raw = match read_iommu_membership().await {
        Ok(m) => m,
        Err(e) => {
            warn!("iommu_groups: read failed ({e}); IOMMU likely off in BIOS");
            return Vec::new();
        }
    };
    if raw.is_empty() {
        return Vec::new();
    }

    let lspci_names = lspci_name_map().await;

    let mut groups: Vec<IommuGroup> = raw
        .into_iter()
        .map(|(id, bdfs)| {
            let mut devices: Vec<PciDevice> = bdfs
                .into_iter()
                .filter_map(|bdf| read_pci_device(&bdf, &lspci_names))
                .collect();
            devices.sort_by(|a, b| a.bdf.cmp(&b.bdf));
            IommuGroup { id, devices }
        })
        .collect();
    groups.sort_by_key(|g| g.id);
    groups
}

/// Map of `iommu_group_id → [bdf]` from sysfs. Pulled out so the
/// gathering step is testable against a fake fs in the future if
/// needed; for now we exercise the scalar parsers below.
async fn read_iommu_membership() -> std::io::Result<HashMap<u32, Vec<String>>> {
    let mut entries = tokio::fs::read_dir(IOMMU_GROUPS_DIR).await?;
    let mut out: HashMap<u32, Vec<String>> = HashMap::new();
    while let Some(entry) = entries.next_entry().await? {
        let Some(group_id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let devices_dir = entry.path().join("devices");
        let Ok(mut devs) = tokio::fs::read_dir(&devices_dir).await else {
            continue;
        };
        while let Some(d) = devs.next_entry().await? {
            if let Some(bdf) = d.file_name().to_str() {
                out.entry(group_id).or_default().push(bdf.to_string());
            }
        }
    }
    Ok(out)
}

/// Read a single PCI device's sysfs entry. Returns `None` if the
/// device disappears between the iommu_groups walk and our read
/// (unlikely; race with hot-unplug).
fn read_pci_device(bdf: &str, lspci_names: &HashMap<String, LspciNames>) -> Option<PciDevice> {
    let dev_dir = format!("{PCI_DEVICES_DIR}/{bdf}");
    let vendor_id = read_pci_id_field(&dev_dir, "vendor")?;
    let device_id = read_pci_id_field(&dev_dir, "device")?;
    let class_id = read_pci_id_field(&dev_dir, "class")?;
    let driver = read_driver(&dev_dir);
    let names = lspci_names.get(bdf).cloned().unwrap_or_default();
    Some(PciDevice {
        bdf: bdf.to_string(),
        vendor_id,
        device_id,
        class_id,
        vendor_name: names.vendor,
        device_name: names.device,
        class_name: names.class,
        driver,
    })
}

/// `/sys/bus/pci/devices/<BDF>/{vendor,device}` are 6-char strings of
/// the form `0xVVVV` for vendor/device, or `0xCCSSPP` for class
/// (class+subclass+programming-interface). Strip the `0x` and any
/// trailing newline. Returns the raw hex string for downstream
/// rendering — we don't care about numeric value.
fn read_pci_id_field(dev_dir: &str, field: &str) -> Option<String> {
    let raw = std::fs::read_to_string(format!("{dev_dir}/{field}")).ok()?;
    let trimmed = raw.trim();
    Some(trimmed.strip_prefix("0x").unwrap_or(trimmed).to_string())
}

/// Resolve the basename of the `driver` symlink. Sysfs uses a
/// symlink at `/sys/bus/pci/devices/<BDF>/driver` pointing into
/// `/sys/bus/pci/drivers/<name>`; absent if no driver is bound.
fn read_driver(dev_dir: &str) -> Option<String> {
    let link = std::fs::read_link(format!("{dev_dir}/driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// Cached lookup table built by parsing one `lspci -nnvmm` invocation.
#[derive(Debug, Clone, Default)]
struct LspciNames {
    vendor: Option<String>,
    device: Option<String>,
    class: Option<String>,
}

/// Run `lspci -nnvmm` once and parse it into a BDF→names map. This is
/// the only slow operation in the walker (~10ms on a typical box);
/// caller invokes it once per `iommu_groups()` call and joins by BDF.
async fn lspci_name_map() -> HashMap<String, LspciNames> {
    let output = match tokio::process::Command::new("lspci")
        .args(["-nnvmm", "-D"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            warn!(
                "lspci -nnvmm exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr)
            );
            return HashMap::new();
        }
        Err(e) => {
            warn!("lspci -nnvmm spawn failed: {e} — vendor/device names unavailable");
            return HashMap::new();
        }
    };
    parse_lspci_machine_readable(&String::from_utf8_lossy(&output))
}

/// Parse `lspci -nnvmm -D` output. Format: blocks separated by blank
/// lines, each block is `Key:\tValue` lines. Relevant keys for us:
///
/// ```text
/// Slot:    0000:01:00.0
/// Class:   VGA compatible controller [0300]
/// Vendor:  NVIDIA Corporation [10de]
/// Device:  GA102 [GeForce RTX 3080] [2204]
/// ```
///
/// Trailing `[<id>]` brackets are vendor/device IDs we already have
/// from sysfs — strip them out so the user sees just the name.
fn parse_lspci_machine_readable(output: &str) -> HashMap<String, LspciNames> {
    let mut out = HashMap::new();
    for block in output.split("\n\n") {
        let mut slot: Option<String> = None;
        let mut names = LspciNames::default();
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "Slot" => slot = Some(value.to_string()),
                "Class" => names.class = Some(strip_id_suffix(value).to_string()),
                "Vendor" => names.vendor = Some(strip_id_suffix(value).to_string()),
                "Device" => names.device = Some(strip_id_suffix(value).to_string()),
                _ => {}
            }
        }
        if let Some(s) = slot {
            out.insert(s, names);
        }
    }
    out
}

/// `"NVIDIA Corporation [10de]"` → `"NVIDIA Corporation"`. The id in
/// brackets is what `-nn` adds; we strip it because the JSON already
/// surfaces the id separately.
fn strip_id_suffix(s: &str) -> &str {
    if let Some(idx) = s.rfind(" [")
        && s.ends_with(']')
    {
        return s[..idx].trim();
    }
    s.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_id_suffix_removes_trailing_bracketed_id() {
        assert_eq!(
            strip_id_suffix("NVIDIA Corporation [10de]"),
            "NVIDIA Corporation"
        );
        assert_eq!(
            strip_id_suffix("VGA compatible controller [0300]"),
            "VGA compatible controller"
        );
    }

    #[test]
    fn strip_id_suffix_leaves_text_with_internal_brackets_alone() {
        // Some device names embed brackets (e.g. "AMD/ATI [Vega 10]")
        // — we only strip the trailing suffix, not internal brackets.
        assert_eq!(
            strip_id_suffix("AMD/ATI [Vega 10] [10de]"),
            "AMD/ATI [Vega 10]"
        );
    }

    #[test]
    fn strip_id_suffix_passes_through_when_no_bracket() {
        assert_eq!(strip_id_suffix("Some Device"), "Some Device");
        assert_eq!(strip_id_suffix(""), "");
    }

    #[test]
    fn parse_lspci_extracts_slot_class_vendor_device_per_block() {
        // Real `lspci -nnvmm -D` output: blocks separated by blank
        // lines. Includes lines we don't care about (Rev, ProgIf,
        // SVendor, SDevice, PhySlot, NUMANode) — they should be
        // ignored without breaking parsing.
        let output = "\
Slot:\t0000:00:00.0
Class:\tHost bridge [0600]
Vendor:\tIntel Corporation [8086]
Device:\t12th Gen Core Host Bridge [4660]
Rev:\t02

Slot:\t0000:01:00.0
Class:\tVGA compatible controller [0300]
Vendor:\tNVIDIA Corporation [10de]
Device:\tGA102 [GeForce RTX 3080] [2204]
SVendor:\tASUSTeK Computer Inc. [1043]
SDevice:\tROG STRIX RTX 3080 [8678]
Rev:\ta1
";
        let map = parse_lspci_machine_readable(output);
        assert_eq!(map.len(), 2);

        let host = map.get("0000:00:00.0").unwrap();
        assert_eq!(host.vendor.as_deref(), Some("Intel Corporation"));
        assert_eq!(host.device.as_deref(), Some("12th Gen Core Host Bridge"));
        assert_eq!(host.class.as_deref(), Some("Host bridge"));

        let gpu = map.get("0000:01:00.0").unwrap();
        assert_eq!(gpu.vendor.as_deref(), Some("NVIDIA Corporation"));
        assert_eq!(gpu.device.as_deref(), Some("GA102 [GeForce RTX 3080]"));
        assert_eq!(gpu.class.as_deref(), Some("VGA compatible controller"));
    }

    #[test]
    fn parse_lspci_handles_empty_input() {
        assert!(parse_lspci_machine_readable("").is_empty());
    }

    #[test]
    fn parse_lspci_skips_blocks_without_a_slot() {
        // Defensive: malformed block (no Slot:) shouldn't poison the
        // map by inserting under a previous block's key.
        let output = "\
Class:\tOrphan Class [0000]
Vendor:\tNobody [0000]

Slot:\t0000:02:00.0
Class:\tAudio device [0403]
Vendor:\tIntel Corporation [8086]
Device:\tCometLake-S cAVS [a3f0]
";
        let map = parse_lspci_machine_readable(output);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("0000:02:00.0"));
    }
}
