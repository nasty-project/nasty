//! Read-only hardware discovery for the Hardware page.
//!
//! Two surfaces, both driven from this module:
//!
//! 1. **IOMMU + PCI tree** (`iommu_groups()`) — sysfs walker, deterministic,
//!    cheap, the data passthrough users need. No caching needed.
//! 2. **Hardware summary** (`system_summary()`) — motherboard, BIOS, CPU,
//!    DIMMs, USB. Backed by `dmidecode`, `/proc/cpuinfo`, `lsusb`.
//!    Cached 60s in-memory because dmidecode takes ~50–200ms and the
//!    underlying state doesn't change between runs.
//!
//! Sources:
//! - `/sys/kernel/iommu_groups/<N>/devices/<BDF>` — group membership
//! - `/sys/bus/pci/devices/<BDF>/{vendor,device,class}` — numeric IDs
//! - `/sys/bus/pci/devices/<BDF>/driver` symlink — currently bound driver
//! - `lspci -nnvmm` — human-readable vendor / device / class names
//! - `dmidecode -t baseboard,bios,memory` — DMI tables. We parse three
//!   specific subsection types (Base Board, BIOS, Memory Device) which
//!   have stable, well-documented field names — much narrower than
//!   parsing all of dmidecode.
//! - `/proc/cpuinfo` — CPU model + cores
//! - `lsusb -t` and `lsusb` — USB device tree

use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
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

// ── Hardware summary ───────────────────────────────────────────
//
// Motherboard / BIOS / CPU / DIMMs / USB. Cached for 60s because
// dmidecode is slow and the underlying data is effectively static
// between reboots — no point re-running it on every page render.

const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(60);

static SUMMARY_CACHE: OnceLock<Mutex<Option<(Instant, HardwareSummary)>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HardwareSummary {
    pub system: Option<DmiSystem>,
    pub bios: Option<DmiBios>,
    pub cpu: Option<CpuSummary>,
    pub memory: MemorySummary,
    pub usb: Vec<UsbDevice>,
    /// TPM chip presence + usability for the encryption-key sealing
    /// flows that #102 is building toward. `None` when no TPM device
    /// is enumerated by the kernel at all (no chip, disabled in
    /// firmware, missing driver).
    pub tpm: Option<TpmInfo>,
    /// Secure Boot state as reported by `sbctl status --json`. Always
    /// present — failure modes (BIOS boot, sbctl missing, sbctl
    /// errored) collapse into a struct with `enabled = None` and a
    /// human-readable `note` rather than an absent field. The WebUI
    /// renders one of: enabled / disabled / unknown.
    pub secure_boot: nasty_common::secure_boot::SecureBootStatus,
}

/// Snapshot of the host's first TPM device — what the kernel sees at
/// `/sys/class/tpm/tpm0/`. We only inspect index 0 because everything
/// downstream (sealing, unsealing, auto-unlock) targets the system's
/// "main" TPM; multi-TPM boxes are out of scope.
///
/// The two booleans together answer the question that matters for #102:
///   - `version_major == 2` AND `rm_available` ⇒ usable for sealing.
///   - Anything else ⇒ surface as "incompatible" in the WebUI and skip
///     the bind-to-TPM affordance.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TpmInfo {
    /// `1` for TPM 1.2 (incompatible with the planned sealing flow),
    /// `2` for TPM 2.0. Read from `tpm_version_major`.
    pub version_major: Option<u32>,
    /// `/dev/tpmrm0` is the resource-manager interface tpm2-tools and
    /// any sealing code actually talks to. Present on TPM 2.0 systems
    /// with the in-kernel resource manager enabled (default on every
    /// modern kernel). When false, the chip exists but the sealing
    /// path will fail at the device-open step.
    pub rm_available: bool,
    /// 4-character ASCII manufacturer code reported by the chip via
    /// `TPM2_PT_MANUFACTURER` — `IFX` (Infineon), `STM`
    /// (STMicroelectronics), `NTC` (Nuvoton), `IBM ` (swtpm), `AMD`
    /// (fTPM), etc. Queried via `tpm2_getcap` since sysfs doesn't
    /// expose it on most kernel drivers (an empirically-discovered
    /// gap — swtpm and many real TPM drivers publish only
    /// `tpm_version_major`).
    pub manufacturer: Option<String>,
    /// Vendor's marketing model string, also from `tpm2_getcap` —
    /// assembled from `TPM2_PT_VENDOR_STRING_1..4`. E.g. `"SLB9665"`
    /// for an Infineon chip, `"SW   TPM"` for swtpm. Empty / None
    /// when the chip doesn't publish a vendor string or the query
    /// fails.
    pub vendor_string: Option<String>,
}

/// DMI Type 1 + Type 2 — the "what hardware is this box" basics.
/// Serial numbers are deliberately not included; they're sensitive
/// and rarely useful in a UI.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DmiSystem {
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub version: Option<String>,
}

/// DMI Type 0 — the BIOS info row. `release_date` is the original
/// firmware date in `MM/DD/YYYY` (DMI convention).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DmiBios {
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub release_date: Option<String>,
}

/// CPU info pulled from /proc/cpuinfo. Logical cores = `processor`
/// entries; physical cores = unique `(physical id, core id)` pairs.
/// We only store a handful of fields; for live frequency/temperature
/// the dashboard already uses the `system.stats` endpoint.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CpuSummary {
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub physical_cores: u32,
    pub logical_cores: u32,
    /// Max advertised speed in MHz from `cpu MHz` (often 0 on idle
    /// systems; better signal than `lscpu --max`).
    pub max_mhz: Option<u32>,
}

/// Memory subsystem summary. Slot counts and per-DIMM detail come
/// from DMI Type 16 (Memory Array) and Type 17 (Memory Device).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct MemorySummary {
    /// Sum of all populated DIMM sizes in bytes.
    pub total_bytes: u64,
    /// Total DIMM slots on the system (populated + empty).
    pub slots_total: u32,
    /// Slots with a DIMM in them.
    pub slots_used: u32,
    /// Whether the memory array supports ECC (single bit, multi-bit, or chipkill).
    pub ecc: bool,
    pub dimms: Vec<DimmInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DimmInfo {
    /// Slot identifier from DMI Type 17 `Locator`, e.g. `DIMM_A1`.
    pub locator: String,
    /// Bytes; 0 means slot is empty.
    pub size_bytes: u64,
    /// `DDR4`, `DDR5`, `LPDDR4`, etc. Empty/None when slot is empty.
    pub mem_type: Option<String>,
    /// MT/s rated speed.
    pub speed_mts: Option<u32>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UsbDevice {
    /// Bus number from `lsusb` (decimal).
    pub bus: u32,
    /// Device address on the bus.
    pub device: u32,
    /// 4-hex-digit vendor ID.
    pub vendor_id: String,
    /// 4-hex-digit product ID.
    pub product_id: String,
    /// Single-line "Vendor Name Product Name" rendered by lsusb. The
    /// embedded `pci.ids`/`usb.ids` lookup is lsusb's job, not ours.
    pub description: String,
}

/// Public entry point — returns the cached summary, refreshing if
/// older than `SUMMARY_CACHE_TTL`. The first call after engine start
/// pays the dmidecode cost (~50–200ms); subsequent ones are
/// effectively free.
pub async fn system_summary() -> HardwareSummary {
    let cell = SUMMARY_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().await;
    if let Some((at, ref cached)) = *guard
        && at.elapsed() < SUMMARY_CACHE_TTL
    {
        return cached.clone();
    }
    let fresh = build_summary().await;
    *guard = Some((Instant::now(), fresh.clone()));
    fresh
}

async fn build_summary() -> HardwareSummary {
    let dmidecode_baseboard = run_text(
        "dmidecode",
        &[
            "-q", "-t", "1", "-t", "2", "-t", "0", "-t", "16", "-t", "17",
        ],
    )
    .await
    .unwrap_or_default();

    let (system, bios, memory) = parse_dmidecode(&dmidecode_baseboard);
    let cpu = read_cpu_info().await;
    let usb = read_usb_devices().await;
    let tpm = read_tpm_info().await;
    let secure_boot = nasty_common::secure_boot::status().await;
    HardwareSummary {
        system,
        bios,
        cpu,
        memory,
        usb,
        tpm,
        secure_boot,
    }
}

/// Probe `/sys/class/tpm/tpm0/` for TPM presence + version, then
/// query the chip directly via `tpm2_getcap` for vendor/model
/// strings the kernel doesn't expose. Returns `None` when the device
/// doesn't exist in sysfs at all — i.e. no chip, no driver, or the
/// host firmware has TPM disabled (BIOS `fTPM` toggle off, etc.).
///
/// Why two paths: sysfs is the cheap presence check (no subprocess),
/// but on swtpm and most real-hardware drivers it only publishes
/// `tpm_version_major` — the `description` / `tpm_vendor` /
/// `manufacturer` files I'd guessed at don't exist. The actual
/// vendor identity has to come from the chip via the TPM2 protocol.
async fn read_tpm_info() -> Option<TpmInfo> {
    const TPM_SYSFS: &str = "/sys/class/tpm/tpm0";
    // First gate: does the sysfs entry exist? Missing ⇒ no TPM the
    // kernel could see. Cheap stat — we read the directory metadata,
    // not its contents.
    if tokio::fs::metadata(TPM_SYSFS).await.is_err() {
        return None;
    }

    let version_major = tokio::fs::read_to_string(format!("{TPM_SYSFS}/tpm_version_major"))
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());

    // `/dev/tpmrm0` is the resource-manager char dev. Without it the
    // sealing path can't open anything to talk to — that's the
    // distinction between "kernel saw a TPM" and "kernel made it
    // usable for tpm2-tools."
    let rm_available = tokio::fs::metadata("/dev/tpmrm0").await.is_ok();

    // Vendor info only matters for TPM 2.0 chips that we can actually
    // reach (rm_available). Skip the subprocess for TPM 1.2 or for
    // RM-less setups — the resulting None columns are honest and
    // cheaper than spawning a binary that'll fail.
    let (manufacturer, vendor_string) = if version_major == Some(2) && rm_available {
        read_tpm2_vendor().await
    } else {
        (None, None)
    };

    Some(TpmInfo {
        version_major,
        rm_available,
        manufacturer,
        vendor_string,
    })
}

/// Run `tpm2_getcap properties-fixed` and pull manufacturer +
/// vendor-string fields out of its YAML output. Returns `(None, None)`
/// on any subprocess failure (tpm2-tools missing, no permission,
/// chip refused the command) — the caller already establishes the
/// chip is reachable, so a failure here is best-effort silently
/// degrading the Hardware page rather than a real fault.
///
/// Output format (one block per property):
///
///   TPM2_PT_MANUFACTURER:
///     raw: 0x49465800
///     value: "IFX"
///   TPM2_PT_VENDOR_STRING_1:
///     raw: 0x534C4239
///     value: "SLB9"
///
/// Vendor string is published in four 4-byte chunks
/// (`_1` … `_4`); concatenating their `value:` quotes gives the
/// human marketing name. `manufacturer` is the single 4-char ASCII
/// code (`IFX`, `STM`, `NTC`, `IBM `, `AMD`, etc.).
async fn read_tpm2_vendor() -> (Option<String>, Option<String>) {
    let output = match tokio::process::Command::new("tpm2_getcap")
        .arg("properties-fixed")
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            warn!(
                "tpm2_getcap properties-fixed exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return (None, None);
        }
        Err(e) => {
            warn!("tpm2_getcap spawn failed: {e}");
            return (None, None);
        }
    };
    parse_tpm2_getcap_properties_fixed(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the `TPM2_PT_MANUFACTURER` and `TPM2_PT_VENDOR_STRING_1..4`
/// fields out of `tpm2_getcap properties-fixed` output. Extracted
/// from `read_tpm2_vendor` so the parsing rules can be pinned by
/// unit tests without spawning subprocesses.
///
/// Single-pass parser: track the most recent `TPM2_PT_*:` header,
/// and when we hit a `  value: "..."` line stash it under that key.
/// Avoids pulling in a YAML crate just for five fields out of ~30.
///
/// Returns `(manufacturer, vendor_string)`.
fn parse_tpm2_getcap_properties_fixed(text: &str) -> (Option<String>, Option<String>) {
    let mut current_key: Option<&str> = None;
    let mut manufacturer = None;
    let mut vendor_chunks: [Option<String>; 4] = [None, None, None, None];
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(name) = trimmed
            .strip_prefix("TPM2_PT_")
            .and_then(|rest| rest.strip_suffix(':'))
        {
            current_key = match name {
                "MANUFACTURER" => Some("manufacturer"),
                "VENDOR_STRING_1" => Some("vendor_1"),
                "VENDOR_STRING_2" => Some("vendor_2"),
                "VENDOR_STRING_3" => Some("vendor_3"),
                "VENDOR_STRING_4" => Some("vendor_4"),
                _ => None,
            };
            continue;
        }
        let Some(key) = current_key else { continue };
        // Value lines are indented `  value: "..."`. Grab the quoted
        // payload. Some chunks come back as empty strings — the chip
        // pads vendor-string slots it doesn't need.
        let value = line
            .trim_start()
            .strip_prefix("value:")
            .map(|s| s.trim())
            .and_then(|s| s.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
            .map(|s| s.to_string());
        if let Some(v) = value {
            match key {
                "manufacturer" => manufacturer = Some(v).filter(|s| !s.is_empty()),
                "vendor_1" => vendor_chunks[0] = Some(v),
                "vendor_2" => vendor_chunks[1] = Some(v),
                "vendor_3" => vendor_chunks[2] = Some(v),
                "vendor_4" => vendor_chunks[3] = Some(v),
                _ => {}
            }
            current_key = None;
        }
    }

    // Concatenate vendor chunks. Each chunk is up to 4 bytes; some
    // chips publish trailing-NUL slots that the YAML printer leaves
    // empty or null-padded.
    let vendor_string = {
        let combined: String = vendor_chunks
            .iter()
            .filter_map(|c| c.as_deref())
            .collect::<String>()
            .trim_end_matches('\0')
            .trim()
            .to_string();
        if combined.is_empty() {
            None
        } else {
            Some(combined)
        }
    };

    (manufacturer, vendor_string)
}

async fn run_text(cmd: &str, args: &[&str]) -> Option<String> {
    match tokio::process::Command::new(cmd).args(args).output().await {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            warn!(
                "{cmd} {args:?} exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            None
        }
        Err(e) => {
            warn!("{cmd} spawn failed: {e}");
            None
        }
    }
}

/// Parse `dmidecode -q -t 0 -t 1 -t 2 -t 16 -t 17` output. Format:
/// blank-line-separated blocks; each block starts with a non-indented
/// section title line (e.g. `BIOS Information`) and continues with
/// indented `Key: Value` pairs. Type 16 (Memory Array) and Type 17
/// (Memory Device) blocks are aggregated into the MemorySummary.
fn parse_dmidecode(input: &str) -> (Option<DmiSystem>, Option<DmiBios>, MemorySummary) {
    let mut system: Option<DmiSystem> = None;
    let mut bios: Option<DmiBios> = None;
    let mut memory = MemorySummary::default();

    for block in input.split("\n\n") {
        let mut lines = block.lines();
        // First non-empty line is the section title (e.g.
        // "Base Board Information"). The "Handle 0xNNNN, DMI type..."
        // line is filtered out by `-q`.
        let Some(title) = lines.find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let kv = collect_kv(lines);
        match title.trim() {
            "System Information" if system.is_none() => {
                // Prefer Base Board if available, but fall back to System
                // when /sys/devices/virtual/dmi has no baseboard table
                // (e.g. some VMs).
                system = Some(DmiSystem {
                    manufacturer: kv.get("Manufacturer").cloned(),
                    product: kv.get("Product Name").cloned(),
                    version: kv.get("Version").cloned(),
                });
            }
            "Base Board Information" => {
                system = Some(DmiSystem {
                    manufacturer: kv.get("Manufacturer").cloned(),
                    product: kv.get("Product Name").cloned(),
                    version: kv.get("Version").cloned(),
                });
            }
            "BIOS Information" => {
                bios = Some(DmiBios {
                    vendor: kv.get("Vendor").cloned(),
                    version: kv.get("Version").cloned(),
                    release_date: kv.get("Release Date").cloned(),
                });
            }
            "Physical Memory Array" => {
                // Slot count & ECC support live here; per-DIMM detail
                // is in the Type 17 blocks below.
                if let Some(devices) = kv.get("Number Of Devices")
                    && let Ok(n) = devices.parse::<u32>()
                {
                    memory.slots_total += n;
                }
                if let Some(ec) = kv.get("Error Correction Type")
                    && !ec.eq_ignore_ascii_case("None")
                    && !ec.eq_ignore_ascii_case("Unknown")
                {
                    memory.ecc = true;
                }
            }
            "Memory Device" => {
                let locator = kv
                    .get("Locator")
                    .cloned()
                    .unwrap_or_else(|| "(unknown)".to_string());
                let size_bytes = kv.get("Size").map(|s| parse_dmi_size(s)).unwrap_or(0);
                let populated = size_bytes > 0;
                if populated {
                    memory.slots_used += 1;
                    memory.total_bytes += size_bytes;
                }
                memory.dimms.push(DimmInfo {
                    locator,
                    size_bytes,
                    mem_type: kv.get("Type").filter(|s| *s != "Unknown").cloned(),
                    speed_mts: kv.get("Speed").and_then(|s| parse_dmi_speed(s)),
                    manufacturer: kv
                        .get("Manufacturer")
                        .filter(|s| !s.starts_with("Not Specified") && !s.starts_with("Unknown"))
                        .cloned(),
                    part_number: kv
                        .get("Part Number")
                        .filter(|s| !s.starts_with("Not Specified") && !s.starts_with("Unknown"))
                        .cloned(),
                });
            }
            _ => {}
        }
    }

    (system, bios, memory)
}

/// Collect indented `Key: Value` lines into a map. Sub-list lines
/// (deeper indentation, no `:`) are ignored — we don't surface
/// characteristic flags etc.
fn collect_kv<'a, I: Iterator<Item = &'a str>>(lines: I) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in lines {
        if !line.starts_with('\t') && !line.starts_with("    ") {
            continue;
        }
        let trimmed = line.trim_start();
        let Some((k, v)) = trimmed.split_once(':') else {
            continue;
        };
        // Sub-properties (e.g. Characteristics list) start with a
        // capital-letter prefix in the value but no colon — those
        // are filtered out by the split_once above.
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        out.insert(k.trim().to_string(), v.to_string());
    }
    out
}

/// `"16 GB"` → 17179869184; `"8192 MB"` → 8589934592. Returns 0 on
/// `"No Module Installed"` or any unrecognized format. Accepts both
/// decimal-prefix (`KB`/`MB`/`GB`/`TB`) and binary-prefix (`KiB`/`MiB`/
/// `GiB`/`TiB`) spellings — different dmidecode versions emit one or
/// the other, and the values are 1024-based in both cases (DMI table
/// sizes are always powers of two). The Hardware page silently showed
/// "0 B / 0 of N slots populated" on machines where dmidecode emits
/// the IEC form because the old match arms only covered the decimal
/// spellings.
fn parse_dmi_size(s: &str) -> u64 {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("No Module Installed") {
        return 0;
    }
    let mut parts = trimmed.split_whitespace();
    let Some(num) = parts.next().and_then(|n| n.parse::<u64>().ok()) else {
        return 0;
    };
    match parts.next().unwrap_or("").to_ascii_lowercase().as_str() {
        "kb" | "kib" => num * 1024,
        "mb" | "mib" => num * 1024 * 1024,
        "gb" | "gib" => num * 1024 * 1024 * 1024,
        "tb" | "tib" => num * 1024_u64.pow(4),
        _ => 0,
    }
}

/// `"3200 MT/s"` → `Some(3200)`; `"Unknown"` → `None`.
fn parse_dmi_speed(s: &str) -> Option<u32> {
    s.split_whitespace().next()?.parse::<u32>().ok()
}

async fn read_cpu_info() -> Option<CpuSummary> {
    let raw = tokio::fs::read_to_string("/proc/cpuinfo").await.ok()?;
    Some(parse_cpuinfo(&raw))
}

fn parse_cpuinfo(input: &str) -> CpuSummary {
    let mut model: Option<String> = None;
    let mut vendor: Option<String> = None;
    let mut logical = 0u32;
    let mut max_mhz: Option<u32> = None;
    let mut phys: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut current_phys_id = String::new();
    let mut current_core_id = String::new();

    for line in input.lines() {
        let Some((k, v)) = line.split_once(':') else {
            if line.trim().is_empty() {
                if !current_phys_id.is_empty() && !current_core_id.is_empty() {
                    phys.insert((current_phys_id.clone(), current_core_id.clone()));
                }
                current_phys_id.clear();
                current_core_id.clear();
            }
            continue;
        };
        let key = k.trim();
        let value = v.trim();
        match key {
            "processor" => logical += 1,
            "model name" if model.is_none() => model = Some(value.to_string()),
            "vendor_id" if vendor.is_none() => vendor = Some(value.to_string()),
            "physical id" => current_phys_id = value.to_string(),
            "core id" => current_core_id = value.to_string(),
            "cpu MHz" => {
                if let Ok(mhz) = value.parse::<f64>() {
                    let rounded = mhz.round() as u32;
                    max_mhz = Some(max_mhz.map_or(rounded, |m| m.max(rounded)));
                }
            }
            _ => {}
        }
    }
    // Catch the last block if the file didn't end with a blank line.
    if !current_phys_id.is_empty() && !current_core_id.is_empty() {
        phys.insert((current_phys_id, current_core_id));
    }

    CpuSummary {
        model,
        vendor,
        physical_cores: phys.len() as u32,
        logical_cores: logical,
        max_mhz,
    }
}

async fn read_usb_devices() -> Vec<UsbDevice> {
    let Some(text) = run_text("lsusb", &[]).await else {
        return Vec::new();
    };
    parse_lsusb(&text)
}

/// Parse plain `lsusb` output. Format per line:
///   `Bus 002 Device 003: ID 1234:5678 Vendor Name Product Name`
fn parse_lsusb(input: &str) -> Vec<UsbDevice> {
    let mut out = Vec::new();
    for line in input.lines() {
        let Some(rest) = line.strip_prefix("Bus ") else {
            continue;
        };
        let mut parts = rest.splitn(6, ' ');
        let bus: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let _device_word = parts.next();
        let dev_str = parts.next().and_then(|s| s.strip_suffix(':'));
        let device: u32 = dev_str.and_then(|s| s.parse().ok()).unwrap_or(0);
        let _id_word = parts.next();
        let id_pair = parts.next().unwrap_or("");
        let description = parts.next().unwrap_or("").trim().to_string();
        let Some((vendor_id, product_id)) = id_pair.split_once(':') else {
            continue;
        };
        out.push(UsbDevice {
            bus,
            device,
            vendor_id: vendor_id.to_string(),
            product_id: product_id.to_string(),
            description,
        });
    }
    out
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

    // ── parse_dmi_size ────────────────────────────────────────────

    #[test]
    fn parse_dmi_size_handles_all_units() {
        // Sizes in /sys/firmware/dmi/tables come through dmidecode's
        // -t 17 already converted to GB / MB / TB depending on the
        // module. Cover all the units to make sure none are silently
        // returning 0.
        assert_eq!(parse_dmi_size("16 GB"), 16 * 1024 * 1024 * 1024);
        assert_eq!(parse_dmi_size("8192 MB"), 8192 * 1024 * 1024);
        assert_eq!(parse_dmi_size("64 KB"), 64 * 1024);
        assert_eq!(parse_dmi_size("1 TB"), 1024_u64.pow(4));
    }

    #[test]
    fn parse_dmi_size_handles_iec_binary_prefixes() {
        // Newer dmidecode versions emit "32 GiB" instead of "32 GB" for
        // Memory Device entries. Same 1024-based value either way (DMI
        // sizes are always powers of two), so the parser has to accept
        // both spellings. Without this the Hardware page silently shows
        // "0 B / 0 of N slots populated" on those machines — the
        // unit-suffix match falls through to the catch-all and we
        // return 0. Reported live on an Odroid H3 with `Size: 32 GiB`.
        assert_eq!(parse_dmi_size("32 GiB"), 32 * 1024 * 1024 * 1024);
        assert_eq!(parse_dmi_size("8 GiB"), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_dmi_size("4096 MiB"), 4096 * 1024 * 1024);
        assert_eq!(parse_dmi_size("64 KiB"), 64 * 1024);
        assert_eq!(parse_dmi_size("2 TiB"), 2 * 1024_u64.pow(4));
        // Case-insensitive — some printers use mixed case.
        assert_eq!(parse_dmi_size("16 gib"), 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_dmi_size_returns_zero_for_empty_slot() {
        // dmidecode prints this exact string for unpopulated slots.
        // We use 0 to mark "empty"; any other return value would
        // wrongly inflate the total memory count.
        assert_eq!(parse_dmi_size("No Module Installed"), 0);
        assert_eq!(parse_dmi_size("garbage"), 0);
    }

    // ── parse_dmi_speed ──────────────────────────────────────────

    #[test]
    fn parse_dmi_speed_takes_first_numeric_token() {
        // Real values are like "3200 MT/s" or "DDR4 3200 MT/s".
        // Only the leading numeric word matters for our display.
        assert_eq!(parse_dmi_speed("3200 MT/s"), Some(3200));
        assert_eq!(parse_dmi_speed("Unknown"), None);
        assert_eq!(parse_dmi_speed(""), None);
    }

    // ── parse_dmidecode (DMI sections) ───────────────────────────

    #[test]
    fn parse_dmidecode_extracts_baseboard_bios_and_dimms() {
        // Synthetic-but-format-faithful sample mixing all four DMI
        // types we care about. dmidecode -q output: blank-line-
        // separated blocks, indented key:value pairs, no Handle line.
        let input = "\
Base Board Information
\tManufacturer: Dell Inc.
\tProduct Name: 0XYZ123
\tVersion: A02

BIOS Information
\tVendor: Dell Inc.
\tVersion: 1.2.3
\tRelease Date: 12/01/2024

Physical Memory Array
\tNumber Of Devices: 4
\tError Correction Type: Multi-bit ECC

Memory Device
\tLocator: DIMM_A1
\tSize: 16 GB
\tType: DDR4
\tSpeed: 3200 MT/s
\tManufacturer: Kingston
\tPart Number: KSM32ED8/16HD

Memory Device
\tLocator: DIMM_A2
\tSize: No Module Installed
\tType: Unknown
\tSpeed: Unknown
\tManufacturer: Not Specified
\tPart Number: Not Specified
";
        let (sys, bios, mem) = parse_dmidecode(input);

        let sys = sys.unwrap();
        assert_eq!(sys.manufacturer.as_deref(), Some("Dell Inc."));
        assert_eq!(sys.product.as_deref(), Some("0XYZ123"));
        assert_eq!(sys.version.as_deref(), Some("A02"));

        let bios = bios.unwrap();
        assert_eq!(bios.vendor.as_deref(), Some("Dell Inc."));
        assert_eq!(bios.version.as_deref(), Some("1.2.3"));
        assert_eq!(bios.release_date.as_deref(), Some("12/01/2024"));

        // Slot accounting: 4 total, 1 populated, ECC enabled.
        assert_eq!(mem.slots_total, 4);
        assert_eq!(mem.slots_used, 1);
        assert!(mem.ecc);
        assert_eq!(mem.total_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(mem.dimms.len(), 2);

        let populated = &mem.dimms[0];
        assert_eq!(populated.locator, "DIMM_A1");
        assert_eq!(populated.size_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(populated.mem_type.as_deref(), Some("DDR4"));
        assert_eq!(populated.speed_mts, Some(3200));
        assert_eq!(populated.manufacturer.as_deref(), Some("Kingston"));

        // Empty slot still listed but with size 0 and unknown fields
        // suppressed (so the UI can render "—" cleanly).
        let empty = &mem.dimms[1];
        assert_eq!(empty.locator, "DIMM_A2");
        assert_eq!(empty.size_bytes, 0);
        assert_eq!(empty.mem_type, None);
        assert_eq!(empty.speed_mts, None);
        assert_eq!(empty.manufacturer, None);
    }

    #[test]
    fn parse_dmidecode_handles_no_ecc() {
        // Consumer boards report "None" for Error Correction Type.
        // Make sure we don't accidentally flag that as ECC.
        let input = "\
Physical Memory Array
\tNumber Of Devices: 2
\tError Correction Type: None
";
        let (_, _, mem) = parse_dmidecode(input);
        assert!(!mem.ecc);
        assert_eq!(mem.slots_total, 2);
    }

    #[test]
    fn parse_dmidecode_falls_back_to_system_when_no_baseboard() {
        // Some VMs / virtualized hardware emit Type 1 (System
        // Information) but no Type 2 (Base Board Information).
        // We accept Type 1 as a fallback for the box-identity card.
        let input = "\
System Information
\tManufacturer: QEMU
\tProduct Name: Standard PC (Q35 + ICH9, 2009)
\tVersion: pc-q35-9.0
";
        let (sys, _, _) = parse_dmidecode(input);
        let sys = sys.unwrap();
        assert_eq!(sys.manufacturer.as_deref(), Some("QEMU"));
    }

    // ── parse_cpuinfo ─────────────────────────────────────────────

    #[test]
    fn parse_cpuinfo_counts_physical_and_logical_cores() {
        // Synthetic 2-physical-core / 4-logical-core CPU. The
        // `(physical id, core id)` pair is what `lscpu` uses to
        // dedupe SMT siblings, so we mirror it.
        let input = "\
processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Core(TM) i5-8400
physical id\t: 0
core id\t: 0
cpu MHz\t\t: 3800.000

processor\t: 1
vendor_id\t: GenuineIntel
model name\t: Intel(R) Core(TM) i5-8400
physical id\t: 0
core id\t: 0
cpu MHz\t\t: 3500.000

processor\t: 2
vendor_id\t: GenuineIntel
model name\t: Intel(R) Core(TM) i5-8400
physical id\t: 0
core id\t: 1
cpu MHz\t\t: 4000.000

processor\t: 3
vendor_id\t: GenuineIntel
model name\t: Intel(R) Core(TM) i5-8400
physical id\t: 0
core id\t: 1
cpu MHz\t\t: 3200.000
";
        let cpu = parse_cpuinfo(input);
        assert_eq!(cpu.logical_cores, 4);
        assert_eq!(cpu.physical_cores, 2);
        assert_eq!(cpu.model.as_deref(), Some("Intel(R) Core(TM) i5-8400"));
        assert_eq!(cpu.vendor.as_deref(), Some("GenuineIntel"));
        // Max across all `cpu MHz` entries — ignores per-core throttle.
        assert_eq!(cpu.max_mhz, Some(4000));
    }

    #[test]
    fn parse_cpuinfo_handles_missing_topology_fields() {
        // Some VMs don't expose `physical id` / `core id`; fall back
        // to "physical = logical" rather than reporting 0 cores
        // (which would look like "no CPU detected").
        let input = "\
processor\t: 0
model name\t: Single Core CPU

processor\t: 1
model name\t: Single Core CPU
";
        let cpu = parse_cpuinfo(input);
        assert_eq!(cpu.logical_cores, 2);
        // No topology info means we can't dedupe — physical = 0.
        // The UI is responsible for falling back to logical_cores
        // when this is 0; documented behaviour.
        assert_eq!(cpu.physical_cores, 0);
    }

    // ── parse_lsusb ───────────────────────────────────────────────

    #[test]
    fn parse_lsusb_extracts_one_device_per_line() {
        // Verbatim format from `lsusb` (no -v). The IDs are the
        // canonical 4-hex form; the description is whatever string
        // lsusb resolved from /var/lib/usbutils/usb.ids.
        let input = "\
Bus 002 Device 003: ID 046d:c52b Logitech, Inc. Unifying Receiver
Bus 001 Device 002: ID 8087:0029 Intel Corp. AX200 Bluetooth
Bus 001 Device 001: ID 1d6b:0002 Linux Foundation 2.0 root hub
";
        let devs = parse_lsusb(input);
        assert_eq!(devs.len(), 3);

        assert_eq!(devs[0].bus, 2);
        assert_eq!(devs[0].device, 3);
        assert_eq!(devs[0].vendor_id, "046d");
        assert_eq!(devs[0].product_id, "c52b");
        assert_eq!(devs[0].description, "Logitech, Inc. Unifying Receiver");

        assert_eq!(devs[2].vendor_id, "1d6b");
        assert_eq!(devs[2].description, "Linux Foundation 2.0 root hub");
    }

    #[test]
    fn parse_lsusb_returns_empty_for_no_buses() {
        // Defensive — empty input shouldn't panic.
        assert!(parse_lsusb("").is_empty());
        // Lines that don't start with "Bus " are ignored.
        assert!(parse_lsusb("not lsusb output\n").is_empty());
    }

    #[test]
    fn parse_tpm2_getcap_infineon_fixture() {
        // Real `tpm2_getcap properties-fixed` output from an Infineon
        // SLB 9665 — trimmed to the four fields we care about plus
        // some surrounding noise so the "skip unknown TPM2_PT_*" path
        // is exercised.
        let input = "\
TPM2_PT_FAMILY_INDICATOR:
  raw: 0x322E3000
  value: \"2.0\"
TPM2_PT_MANUFACTURER:
  raw: 0x49465800
  value: \"IFX\"
TPM2_PT_VENDOR_STRING_1:
  raw: 0x534C4239
  value: \"SLB9\"
TPM2_PT_VENDOR_STRING_2:
  raw: 0x36363500
  value: \"665\"
TPM2_PT_VENDOR_STRING_3:
  raw: 0x00000000
  value: \"\"
TPM2_PT_VENDOR_STRING_4:
  raw: 0x00000000
  value: \"\"
";
        let (mfr, vendor) = parse_tpm2_getcap_properties_fixed(input);
        assert_eq!(mfr.as_deref(), Some("IFX"));
        assert_eq!(vendor.as_deref(), Some("SLB9665"));
    }

    #[test]
    fn parse_tpm2_getcap_swtpm_fixture() {
        // swtpm (IBM software TPM) — manufacturer is `IBM ` with a
        // trailing space, vendor string is similarly padded with
        // spaces and NULs. Verifies the trim path at the end of the
        // assembler so we don't end up with `"SW   TPM     "`.
        let input = "\
TPM2_PT_MANUFACTURER:
  raw: 0x49424D00
  value: \"IBM \"
TPM2_PT_VENDOR_STRING_1:
  raw: 0x53572020
  value: \"SW  \"
TPM2_PT_VENDOR_STRING_2:
  raw: 0x54504D20
  value: \"TPM \"
TPM2_PT_VENDOR_STRING_3:
  raw: 0x00000000
  value: \"\"
TPM2_PT_VENDOR_STRING_4:
  raw: 0x00000000
  value: \"\"
";
        let (mfr, vendor) = parse_tpm2_getcap_properties_fixed(input);
        assert_eq!(mfr.as_deref(), Some("IBM "));
        // Trailing spaces stripped, internal spaces preserved.
        assert_eq!(vendor.as_deref(), Some("SW  TPM"));
    }

    #[test]
    fn parse_tpm2_getcap_empty_input() {
        // Total subprocess failure shouldn't crash — empty stdout
        // returns (None, None) and the WebUI quietly drops the
        // vendor line.
        let (mfr, vendor) = parse_tpm2_getcap_properties_fixed("");
        assert!(mfr.is_none());
        assert!(vendor.is_none());
    }

    #[test]
    fn parse_tpm2_getcap_no_relevant_fields() {
        // Output that doesn't include MANUFACTURER / VENDOR_STRING_*
        // (e.g. the chip refused those subcaps, or the output came
        // from a different command). All other TPM2_PT_* keys must
        // be silently ignored.
        let input = "\
TPM2_PT_FAMILY_INDICATOR:
  raw: 0x322E3000
  value: \"2.0\"
TPM2_PT_LEVEL:
  raw: 0
TPM2_PT_REVISION:
  raw: 0x9E
";
        let (mfr, vendor) = parse_tpm2_getcap_properties_fixed(input);
        assert!(mfr.is_none());
        assert!(vendor.is_none());
    }
}
