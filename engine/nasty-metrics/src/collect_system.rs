//! System metrics collection from /proc and smartctl.

use nasty_common::metrics_types::*;
use serde::Deserialize;

// ── CPU ─────────────────────────────────────────────────────────

pub fn cpu_stats() -> CpuStats {
    let count = std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.matches("processor").count() as u32)
        .unwrap_or(1);

    let (load_1, load_5, load_15) = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| {
            let mut parts = s.split_whitespace();
            let l1 = parts.next()?.parse::<f64>().ok()?;
            let l5 = parts.next()?.parse::<f64>().ok()?;
            let l15 = parts.next()?.parse::<f64>().ok()?;
            Some((l1, l5, l15))
        })
        .unwrap_or((0.0, 0.0, 0.0));

    let temp_c = read_cpu_temperature();
    let (freq_mhz, governor) = read_cpu_frequency();

    CpuStats {
        count,
        load_1,
        load_5,
        load_15,
        temp_c,
        freq_mhz,
        governor,
    }
}

/// Read CPU package temperature from hwmon sysfs (coretemp / k10temp).
fn read_cpu_temperature() -> Option<i32> {
    let hwmon = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in hwmon.flatten() {
        let path = entry.path();
        let name = std::fs::read_to_string(path.join("name")).unwrap_or_default();
        let name = name.trim();
        // coretemp (Intel), k10temp (AMD), zenpower (AMD alt)
        if matches!(name, "coretemp" | "k10temp" | "zenpower") {
            // temp1_input is typically the package/Tdie temperature
            if let Ok(val) = std::fs::read_to_string(path.join("temp1_input"))
                && let Ok(millideg) = val.trim().parse::<i64>()
            {
                return Some((millideg / 1000) as i32);
            }
        }
    }
    // Fallback: first thermal zone
    if let Ok(val) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        && let Ok(millideg) = val.trim().parse::<i64>()
    {
        return Some((millideg / 1000) as i32);
    }
    None
}

/// Read average CPU frequency and governor from cpufreq sysfs.
fn read_cpu_frequency() -> (Option<u32>, Option<String>) {
    let mut total_khz: u64 = 0;
    let mut count: u32 = 0;
    let mut governor: Option<String> = None;

    let cpus = match std::fs::read_dir("/sys/devices/system/cpu") {
        Ok(d) => d,
        Err(_) => return (None, None),
    };

    for entry in cpus.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }
        let cpufreq = entry.path().join("cpufreq");
        if let Ok(val) = std::fs::read_to_string(cpufreq.join("scaling_cur_freq"))
            && let Ok(khz) = val.trim().parse::<u64>()
        {
            total_khz += khz;
            count += 1;
        }
        if governor.is_none()
            && let Ok(val) = std::fs::read_to_string(cpufreq.join("scaling_governor"))
        {
            let g = val.trim().to_string();
            if !g.is_empty() {
                governor = Some(g);
            }
        }
    }

    let avg_mhz = if count > 0 {
        Some((total_khz / count as u64 / 1000) as u32)
    } else {
        None
    };

    (avg_mhz, governor)
}

// ── Memory ──────────────────────────────────────────────────────

pub fn memory_stats() -> MemoryStats {
    let content = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total = 0u64;
    let mut available = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let val: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        // Values in /proc/meminfo are in kB
        match key {
            "MemTotal:" => total = val * 1024,
            "MemAvailable:" => available = val * 1024,
            "SwapTotal:" => swap_total = val * 1024,
            "SwapFree:" => swap_free = val * 1024,
            _ => {}
        }
    }

    MemoryStats {
        total_bytes: total,
        used_bytes: total.saturating_sub(available),
        available_bytes: available,
        swap_total_bytes: swap_total,
        swap_used_bytes: swap_total.saturating_sub(swap_free),
    }
}

// ── Network ─────────────────────────────────────────────────────

fn interface_addresses() -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    let Ok(output) = std::process::Command::new("ip")
        .args(["-j", "addr", "show"])
        .output()
    else {
        return map;
    };
    let Ok(json): Result<Vec<serde_json::Value>, _> = serde_json::from_slice(&output.stdout) else {
        return map;
    };
    for iface in json {
        let Some(name) = iface["ifname"].as_str() else {
            continue;
        };
        let mut addrs = Vec::new();
        if let Some(addr_info) = iface["addr_info"].as_array() {
            for ai in addr_info {
                if let Some(local) = ai["local"].as_str() {
                    let prefix = ai["prefixlen"].as_u64().unwrap_or(0);
                    addrs.push(format!("{local}/{prefix}"));
                }
            }
        }
        map.insert(name.to_string(), addrs);
    }
    map
}

pub fn network_stats() -> Vec<NetIfStats> {
    let content = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let addr_map = interface_addresses();
    let mut interfaces = Vec::new();

    for line in content.lines().skip(2) {
        let line = line.trim();
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();

        if name == "lo" {
            continue;
        }

        let vals: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();

        if vals.len() < 10 {
            continue;
        }

        let up = std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false);

        let speed_mbps = std::fs::read_to_string(format!("/sys/class/net/{name}/speed"))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .and_then(|v| if v > 0 { Some(v as u32) } else { None });

        let addresses = addr_map.get(name).cloned().unwrap_or_default();

        interfaces.push(NetIfStats {
            name: name.to_string(),
            rx_bytes: vals[0],
            tx_bytes: vals[8],
            rx_packets: vals[1],
            tx_packets: vals[9],
            speed_mbps,
            up,
            addresses,
        });
    }

    interfaces
}

// ── Disk I/O ────────────────────────────────────────────────────

pub fn disk_io_stats() -> Vec<DiskIoStats> {
    let content = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let mut results = Vec::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 14 {
            continue;
        }

        let name = parts[2];

        let is_disk = (name.starts_with("sd") && name.len() == 3)
            || (name.starts_with("vd") && name.len() == 3)
            || (name.starts_with("nvme") && name.contains('n') && !name.contains('p'));

        if !is_disk {
            continue;
        }

        let read_ios: u64 = parts[3].parse().unwrap_or(0);
        let read_sectors: u64 = parts[5].parse().unwrap_or(0);
        let write_ios: u64 = parts[7].parse().unwrap_or(0);
        let write_sectors: u64 = parts[9].parse().unwrap_or(0);
        let io_in_progress: u64 = parts[11].parse().unwrap_or(0);

        results.push(DiskIoStats {
            name: name.to_string(),
            read_bytes: read_sectors * 512,
            write_bytes: write_sectors * 512,
            read_ios,
            write_ios,
            io_in_progress,
        });
    }

    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

// ── System stats aggregate ──────────────────────────────────────

pub fn system_stats() -> SystemStats {
    SystemStats {
        cpu: cpu_stats(),
        memory: memory_stats(),
        network: network_stats(),
        disk_io: disk_io_stats(),
    }
}

// ── SMART / disk health ─────────────────────────────────────────

#[derive(Deserialize)]
struct SmartctlJson {
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    serial_number: Option<String>,
    #[serde(default)]
    firmware_version: Option<String>,
    #[serde(default)]
    user_capacity: Option<SmartctlCapacity>,
    #[serde(default)]
    smart_status: Option<SmartctlStatus>,
    #[serde(default)]
    temperature: Option<SmartctlTemp>,
    #[serde(default)]
    power_on_time: Option<SmartctlPowerOn>,
    #[serde(default)]
    ata_smart_attributes: Option<SmartctlAtaAttrs>,
    #[serde(default, rename = "nvme_smart_health_information_log")]
    nvme_log: Option<SmartctlNvmeLog>,
    /// Sibling block of `nvme_smart_health_information_log`. Carries the
    /// actual error events behind the count. Only emitted by smartctl
    /// 7.4+; on 7.3 we just get the count in `nvme_log` with no way to
    /// inspect what the errors were.
    #[serde(default)]
    nvme_error_information_log: Option<SmartctlNvmeErrorLog>,
}

#[derive(Deserialize)]
struct SmartctlNvmeErrorLog {
    #[serde(default)]
    table: Vec<SmartctlNvmeErrorEntry>,
}

#[derive(Deserialize)]
struct SmartctlNvmeErrorEntry {
    #[serde(default)]
    status_field: Option<SmartctlNvmeErrorStatus>,
}

#[derive(Deserialize)]
struct SmartctlNvmeErrorStatus {
    #[serde(default)]
    string: String,
}

#[derive(Deserialize)]
struct SmartctlNvmeLog {
    #[serde(default)]
    critical_warning: u8,
    #[serde(default)]
    available_spare: u8,
    #[serde(default)]
    available_spare_threshold: u8,
    #[serde(default)]
    percentage_used: u32,
    #[serde(default)]
    data_units_read: u64,
    #[serde(default)]
    data_units_written: u64,
    #[serde(default)]
    host_reads: u64,
    #[serde(default)]
    host_writes: u64,
    #[serde(default)]
    controller_busy_time: u64,
    #[serde(default)]
    power_cycles: u64,
    #[serde(default)]
    unsafe_shutdowns: u64,
    #[serde(default)]
    media_errors: u64,
    #[serde(default)]
    num_err_log_entries: u64,
    #[serde(default)]
    warning_temp_time: u64,
    #[serde(default)]
    critical_comp_time: u64,
    // Vendors with un-wired sensors emit `null` entries (e.g. Kingston
    // SNV3S → `[null, 43]`). Use Option<i32> so the deserialize succeeds.
    #[serde(default)]
    temperature_sensors: Vec<Option<i32>>,
}

#[derive(Deserialize)]
struct SmartctlCapacity {
    #[serde(default)]
    bytes: u64,
}

#[derive(Deserialize)]
struct SmartctlStatus {
    #[serde(default)]
    passed: bool,
}

#[derive(Deserialize)]
struct SmartctlTemp {
    #[serde(default)]
    current: Option<i32>,
}

#[derive(Deserialize)]
struct SmartctlPowerOn {
    #[serde(default)]
    hours: Option<u64>,
}

#[derive(Deserialize)]
struct SmartctlAtaAttrs {
    #[serde(default)]
    table: Vec<SmartctlAtaAttr>,
}

#[derive(Deserialize)]
struct SmartctlAtaAttr {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: u32,
    #[serde(default)]
    worst: u32,
    #[serde(default)]
    thresh: u32,
    #[serde(default)]
    raw: Option<SmartctlRaw>,
    #[serde(default)]
    when_failed: String,
}

#[derive(Deserialize)]
struct SmartctlRaw {
    #[serde(default)]
    value: i64,
}

/// What lsblk knows about a disk before we ask smartctl anything.
///
/// Keeping this around as a separate row means a disk shows up in the
/// `system.disks` answer even when smartctl can't talk to it — useful for
/// fresh / unformatted / wonky-USB-bridge drives (#349). Without this the
/// older code path silently dropped the device, so the operator only saw
/// disks that had already been formatted.
struct LsblkDisk {
    device: String,
    size_bytes: u64,
    model: String,
    serial: String,
}

/// Smart status sentinel used when smartctl produced no usable data for
/// the device. WebUI styles this distinctly from PASSED/FAILED, and the
/// alert rules (see `nasty_system::alerts::AlertMetric::SmartHealth`)
/// intentionally don't fire on it — UNAVAILABLE is "unknown", not "bad".
pub const SMART_STATUS_UNAVAILABLE: &str = "UNAVAILABLE";

pub async fn disk_health() -> Vec<DiskHealth> {
    // Two-stage enumeration:
    //   1. `smartctl --scan-open -j` is the authoritative SMART source.
    //      It's the only way to discover physical drives behind RAID
    //      controllers (megaraid, areca, 3ware, cciss…) where one block
    //      device path fronts many drives, each reachable only with a
    //      transport flag like `-d megaraid,N`. For direct-attach
    //      SATA/NVMe it returns one endpoint per device.
    //   2. lsblk-discovered block devices that scan-open did NOT surface
    //      still get a row with smart_status=UNAVAILABLE. Covers
    //      USB-SATA bridges that need `-d sat`, fresh / unformatted
    //      drives smartctl rejects before lsblk does, and any case
    //      where smartctl is missing entirely. This preserves the #349
    //      guarantee that every block device shows up somewhere.
    let endpoints = scan_smartctl_endpoints().await;
    let lsblk_disks = enumerate_disks().await;
    let scan_paths: std::collections::HashSet<String> =
        endpoints.iter().map(|e| e.device.clone()).collect();

    let mut results = Vec::with_capacity(endpoints.len() + lsblk_disks.len());
    // Index lsblk hits by path so single-endpoint paths can borrow
    // model/serial/size as fallback when smartctl returns blanks. Multi-
    // endpoint paths (megaraid) skip the fallback — lsblk only knows the
    // RAID volume's metadata, not the physical drives behind it.
    let mut lsblk_by_path: std::collections::HashMap<String, &LsblkDisk> =
        lsblk_disks.iter().map(|d| (d.device.clone(), d)).collect();
    let endpoint_count_by_path: std::collections::HashMap<String, usize> = {
        let mut m: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for e in &endpoints {
            *m.entry(e.device.clone()).or_default() += 1;
        }
        m
    };

    for endpoint in &endpoints {
        let hint = if endpoint_count_by_path
            .get(&endpoint.device)
            .copied()
            .unwrap_or(1)
            == 1
        {
            lsblk_by_path.remove(&endpoint.device)
        } else {
            None
        };
        results.push(build_disk_health(endpoint, hint).await);
    }
    for (_, disk) in lsblk_by_path {
        if scan_paths.contains(&disk.device) {
            continue;
        }
        results.push(build_disk_health_unreachable(disk));
    }
    results
}

/// One SMART-reachable endpoint discovered by `smartctl --scan-open -j`.
/// `device` is the block device path smartctl reports; `transport` is
/// the `-d` flag needed to talk to it (`None` for the default transport).
#[derive(Debug, Clone)]
struct SmartctlEndpoint {
    device: String,
    transport: Option<String>,
}

#[derive(Deserialize)]
struct SmartctlScanResult {
    #[serde(default)]
    devices: Vec<SmartctlScanDevice>,
}

#[derive(Deserialize)]
struct SmartctlScanDevice {
    #[serde(default)]
    name: String,
    /// smartctl's `type` carries the `-d` flag. Direct-attach SATA shows
    /// `"sat"`; NVMe shows `"nvme"`; megaraid-tunneled SAT shows
    /// `"sat+megaraid,0"`. We strip `"sat"` / `"nvme"` (smartctl's
    /// defaults) so direct-attach drives keep `transport: None`.
    #[serde(default, rename = "type")]
    type_: String,
    /// Empty string when the open succeeded. Non-empty means smartctl
    /// could enumerate the slot but couldn't open it (drive missing,
    /// controller refused, slot empty) — skip these.
    #[serde(default)]
    open_error: String,
}

async fn scan_smartctl_endpoints() -> Vec<SmartctlEndpoint> {
    let output = match tokio::process::Command::new("smartctl")
        .args(["--scan-open", "-j"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    parse_scan_open(&output.stdout)
}

fn parse_scan_open(stdout: &[u8]) -> Vec<SmartctlEndpoint> {
    let scan: SmartctlScanResult = match serde_json::from_slice(stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    scan.devices
        .into_iter()
        .filter(|d| d.open_error.is_empty() && !d.name.is_empty())
        .map(|d| SmartctlEndpoint {
            device: d.name,
            transport: parse_transport(&d.type_),
        })
        .collect()
}

/// Strip smartctl's default transports so direct-attach drives stay
/// `None`. A pure `"sat"`, `"nvme"`, `"ata"` or `"scsi"` carries no
/// useful info (smartctl would have chosen the same flag by default
/// for the corresponding protocol); anything else (`megaraid,*`,
/// `sat+megaraid,*`, `areca,*`, `cciss,*`, …) we keep verbatim so
/// query_smartctl can pass it through with `-d`.
fn parse_transport(type_field: &str) -> Option<String> {
    let trimmed = type_field.trim();
    if trimmed.is_empty()
        || trimmed == "sat"
        || trimmed == "nvme"
        || trimmed == "ata"
        || trimmed == "scsi"
    {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn enumerate_disks() -> Vec<LsblkDisk> {
    // -J = JSON output (parses cleanly even when MODEL has spaces / lsblk
    // version differs across NixOS releases). -d = top-level only.
    // -b = bytes. -n = no header. -o = explicit column list.
    let output = match tokio::process::Command::new("lsblk")
        .args(["-Jdbno", "NAME,TYPE,SIZE,MODEL,SERIAL"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = parsed.get("blockdevices").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|d| {
            let name = d.get("name").and_then(|v| v.as_str())?;
            let dtype = d.get("type").and_then(|v| v.as_str())?;
            if dtype != "disk"
                || name.starts_with("mmcblk")
                || name.starts_with("loop")
                || name.starts_with("ram")
                || name.starts_with("zram")
            {
                return None;
            }
            Some(LsblkDisk {
                device: format!("/dev/{name}"),
                size_bytes: d
                    .get("size")
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0),
                model: d
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Unknown")
                    .to_string(),
                serial: d
                    .get("serial")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Unknown")
                    .to_string(),
            })
        })
        .collect()
}

async fn build_disk_health(
    endpoint: &SmartctlEndpoint,
    lsblk_hint: Option<&LsblkDisk>,
) -> DiskHealth {
    let dev_name = endpoint
        .device
        .strip_prefix("/dev/")
        .unwrap_or(&endpoint.device);
    // Sysfs lookups make sense only for direct-attach drives — for
    // RAID-tunneled drives the block device path is the controller's
    // logical volume, not the physical drive, so `/sys/block/sda/device`
    // resolves to the RAID controller (already correct for
    // controller_pci, but ata_port wouldn't be meaningful).
    let (ata_port, controller_pci) = if endpoint.transport.is_none() {
        resolve_device_path(dev_name)
    } else {
        (None, None)
    };
    let controller_name = controller_pci.as_deref().and_then(resolve_pci_name);

    match query_smartctl(&endpoint.device, endpoint.transport.as_deref()).await {
        Some(s) => DiskHealth {
            device: endpoint.device.clone(),
            transport: endpoint.transport.clone(),
            ata_port,
            controller_pci,
            controller_name,
            // smartctl's strings are usually more accurate than lsblk's
            // (lsblk reads sysfs's `model`, which truncates / pads), but
            // for unfamiliar transports lsblk sometimes wins — prefer
            // smartctl, fall back to lsblk, then to "Unknown".
            model: s
                .model
                .filter(|s| !s.is_empty())
                .or_else(|| lsblk_hint.map(|d| d.model.clone()))
                .unwrap_or_else(|| "Unknown".into()),
            serial: s
                .serial
                .filter(|s| !s.is_empty())
                .or_else(|| lsblk_hint.map(|d| d.serial.clone()))
                .unwrap_or_else(|| "Unknown".into()),
            firmware: s.firmware.unwrap_or_else(|| "Unknown".into()),
            capacity_bytes: if s.capacity_bytes > 0 {
                s.capacity_bytes
            } else {
                lsblk_hint.map(|d| d.size_bytes).unwrap_or(0)
            },
            temperature_c: s.temperature_c,
            power_on_hours: s.power_on_hours,
            health_passed: s.health_passed,
            smart_status: if s.health_passed {
                "PASSED".to_string()
            } else {
                "FAILED".to_string()
            },
            attributes: s.attributes,
            nvme: s.nvme,
        },
        None => build_disk_health_unreachable_for_endpoint(endpoint, lsblk_hint),
    }
}

fn build_disk_health_unreachable_for_endpoint(
    endpoint: &SmartctlEndpoint,
    lsblk_hint: Option<&LsblkDisk>,
) -> DiskHealth {
    let dev_name = endpoint
        .device
        .strip_prefix("/dev/")
        .unwrap_or(&endpoint.device);
    let (ata_port, controller_pci) = if endpoint.transport.is_none() {
        resolve_device_path(dev_name)
    } else {
        (None, None)
    };
    let controller_name = controller_pci.as_deref().and_then(resolve_pci_name);
    DiskHealth {
        device: endpoint.device.clone(),
        transport: endpoint.transport.clone(),
        ata_port,
        controller_pci,
        controller_name,
        model: lsblk_hint
            .map(|d| d.model.clone())
            .unwrap_or_else(|| "Unknown".into()),
        serial: lsblk_hint
            .map(|d| d.serial.clone())
            .unwrap_or_else(|| "Unknown".into()),
        firmware: "Unknown".into(),
        capacity_bytes: lsblk_hint.map(|d| d.size_bytes).unwrap_or(0),
        temperature_c: None,
        power_on_hours: None,
        // Distinct from FAILED so the WebUI can style + the alert
        // rules can skip. See SMART_STATUS_UNAVAILABLE.
        health_passed: false,
        smart_status: SMART_STATUS_UNAVAILABLE.to_string(),
        attributes: Vec::new(),
        nvme: None,
    }
}

fn build_disk_health_unreachable(disk: &LsblkDisk) -> DiskHealth {
    let endpoint = SmartctlEndpoint {
        device: disk.device.clone(),
        transport: None,
    };
    build_disk_health_unreachable_for_endpoint(&endpoint, Some(disk))
}

/// SMART-derived fields, lifted out of `DiskHealth` so the no-data path
/// is a clean `None` rather than a half-filled struct.
struct SmartReport {
    model: Option<String>,
    serial: Option<String>,
    firmware: Option<String>,
    capacity_bytes: u64,
    temperature_c: Option<i32>,
    power_on_hours: Option<u64>,
    health_passed: bool,
    attributes: Vec<SmartAttribute>,
    nvme: Option<NvmeHealth>,
}

async fn query_smartctl(device: &str, transport: Option<&str>) -> Option<SmartReport> {
    let mut cmd = tokio::process::Command::new("smartctl");
    cmd.args(["-a", "--json=c"]);
    if let Some(t) = transport {
        cmd.args(["-d", t]);
    }
    cmd.arg(device);
    let output = cmd.output().await.ok()?;

    let json: SmartctlJson = serde_json::from_slice(&output.stdout).ok()?;

    // Treat "no smart_status object at all" as "unavailable" rather than
    // "FAILED". smartctl returns the object on every supported transport;
    // its absence means we got JSON back but it carried no SMART payload
    // (USB-SATA bridge that needs `-d sat`, controller that doesn't
    // proxy SMART, etc.). Returning None here flows through to the
    // UNAVAILABLE branch in `build_disk_health` so the disk still
    // shows up in the WebUI with whatever lsblk could tell us.
    let smart_status = json.smart_status.as_ref()?;
    let health_passed = smart_status.passed;

    let attributes: Vec<SmartAttribute> = json
        .ata_smart_attributes
        .map(|attrs| {
            attrs
                .table
                .into_iter()
                .map(|a| SmartAttribute {
                    id: a.id,
                    name: a.name,
                    value: a.value,
                    worst: a.worst,
                    threshold: a.thresh,
                    raw_value: a.raw.map(|r| r.value).unwrap_or(0),
                    failing: !a.when_failed.is_empty() && a.when_failed != "-",
                })
                .collect()
        })
        .unwrap_or_default();

    // smartctl returns table entries newest-first when sorted by
    // error_count, so the head is the most recent event we have a
    // string for. Skip blank strings (some firmware reports an entry
    // with a numeric status_code but no decoded text) — surfacing an
    // empty string in the UI would just be noise.
    let most_recent_error = json
        .nvme_error_information_log
        .as_ref()
        .and_then(|log| log.table.first())
        .and_then(|entry| entry.status_field.as_ref())
        .map(|sf| sf.string.trim().to_string())
        .filter(|s| !s.is_empty());

    let nvme = json.nvme_log.map(|n| NvmeHealth {
        critical_warning: n.critical_warning,
        available_spare_percent: n.available_spare,
        available_spare_threshold_percent: n.available_spare_threshold,
        percentage_used: n.percentage_used,
        data_units_read: n.data_units_read,
        data_units_written: n.data_units_written,
        host_reads: n.host_reads,
        host_writes: n.host_writes,
        controller_busy_minutes: n.controller_busy_time,
        power_cycles: n.power_cycles,
        unsafe_shutdowns: n.unsafe_shutdowns,
        media_errors: n.media_errors,
        num_err_log_entries: n.num_err_log_entries,
        warning_temp_minutes: n.warning_temp_time,
        critical_comp_minutes: n.critical_comp_time,
        temperature_sensors_c: n.temperature_sensors,
        most_recent_error,
    });

    Some(SmartReport {
        model: json.model_name,
        serial: json.serial_number,
        firmware: json.firmware_version,
        capacity_bytes: json.user_capacity.map(|c| c.bytes).unwrap_or(0),
        temperature_c: json.temperature.and_then(|t| t.current),
        power_on_hours: json.power_on_time.and_then(|p| p.hours),
        health_passed,
        attributes,
        nvme,
    })
}

/// Resolve ATA port and PCI controller address from sysfs device path.
/// e.g. `/sys/block/sde/device` → (Some("ata5"), Some("03:00.0"))
fn resolve_device_path(dev_name: &str) -> (Option<String>, Option<String>) {
    let link = format!("/sys/block/{dev_name}/device");
    let resolved = std::fs::canonicalize(&link).ok();
    let path_str = match resolved.as_ref().and_then(|p| p.to_str()) {
        Some(s) => s,
        None => return (None, None),
    };

    let mut ata_port = None;
    let mut pci_addr = None;

    for component in path_str.split('/') {
        // Match ataX
        if component.starts_with("ata")
            && component.len() > 3
            && component[3..].chars().all(|c| c.is_ascii_digit())
        {
            ata_port = Some(component.to_string());
        }
        // Match PCI address like 0000:03:00.0 → extract 03:00.0
        if component.contains(':') && component.contains('.') {
            if let Some(short) = component.strip_prefix("0000:") {
                pci_addr = Some(short.to_string());
            } else if component.len() <= 8
                && component
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
            {
                pci_addr = Some(component.to_string());
            }
        }
    }

    (ata_port, pci_addr)
}

/// Look up a human-readable PCI device name via lspci.
fn resolve_pci_name(pci_addr: &str) -> Option<String> {
    let output = std::process::Command::new("lspci")
        .args(["-s", pci_addr])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout);
    // Format: "03:00.0 SATA controller: ASMedia Technology Inc. ASM1166 ..."
    let after_colon = line.find(':')?;
    let desc = line[after_colon + 1..].trim();
    // Skip the type prefix (e.g. "SATA controller: ")
    if let Some(pos) = desc.find(": ") {
        Some(desc[pos + 2..].trim().to_string())
    } else {
        Some(desc.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real-world `smartctl -a --json=c` payloads collected from four
    // independent NVMe vendors so the parser keeps working when smartctl
    // versions, vendor firmware, or namespace layouts shift. Each fixture
    // captures one schema quirk worth pinning:
    //   * Samsung 980 PRO   — baseline smartctl 7.3 layout
    //   * Samsung 980       — smartctl 7.4 (adds nvme_error_information_log
    //                         and nvme_self_test_log siblings)
    //   * Kingston SNV3S    — temperature_sensors with a `null` entry,
    //                         the only field that ever blocked us
    //   * SK hynix BC901    — vendor-specific spare threshold (50 vs 10)
    //                         and the first sample with non-zero
    //                         num_err_log_entries

    fn parse(raw: &str) -> SmartctlJson {
        serde_json::from_str(raw).expect("smartctl fixture must parse")
    }

    #[test]
    fn parses_samsung_980_pro() {
        let j = parse(include_str!("../fixtures/nvme_samsung_980_pro.json"));
        let n = j.nvme_log.expect("nvme block present");
        assert_eq!(n.percentage_used, 3);
        assert_eq!(n.available_spare, 100);
        assert_eq!(n.media_errors, 0);
        assert_eq!(n.temperature_sensors, vec![Some(58), Some(60)]);
    }

    #[test]
    fn parses_samsung_980_with_smartctl_7_4_extras() {
        let j = parse(include_str!("../fixtures/nvme_samsung_980.json"));
        let n = j.nvme_log.expect("nvme block present");
        // 62% worn — the operator-actionable sample in the set.
        assert_eq!(n.percentage_used, 62);
        // 19,451 minutes above warning threshold (~13.5 days).
        assert_eq!(n.warning_temp_time, 19451);
    }

    #[test]
    fn parses_kingston_with_null_temperature_sensor() {
        // The only fixture where temperature_sensors contains `null`.
        // If this stops parsing, the type of `temperature_sensors` in
        // `SmartctlNvmeLog` regressed from Vec<Option<i32>> to Vec<i32>.
        let j = parse(include_str!("../fixtures/nvme_kingston_snv3s.json"));
        let n = j.nvme_log.expect("nvme block present");
        assert_eq!(n.temperature_sensors, vec![None, Some(43)]);
    }

    #[test]
    fn parses_hynix_with_nondefault_spare_threshold() {
        let j = parse(include_str!("../fixtures/nvme_hynix_bc901.json"));
        let n = j.nvme_log.expect("nvme block present");
        // Vendor-specific: most drives report 10; SK hynix reports 50.
        assert_eq!(n.available_spare_threshold, 50);
        assert_eq!(n.num_err_log_entries, 3);
    }

    #[test]
    fn parses_goodram_irdm_with_populated_error_log_table() {
        // GoodRam IRDM IRP-SSDPR — first fixture in the set with a
        // non-zero error log AND smartctl 7.5 emitting the actual
        // table behind the count. The smartctl 7.5 schema also adds
        // a leading `nsid: -1` field in nvme_smart_health_information_log
        // which our parser must silently tolerate (serde ignores
        // unknown fields by default; this fixture guards against a
        // future #[serde(deny_unknown_fields)] regression).
        let j = parse(include_str!("../fixtures/nvme_goodram_irdm.json"));
        let n = j.nvme_log.expect("nvme block present");
        assert_eq!(n.num_err_log_entries, 1033);
        // Yet another spare threshold — confirms the value is
        // genuinely vendor-specific (we've now seen 5, 10, 50).
        assert_eq!(n.available_spare_threshold, 5);

        let log = j
            .nvme_error_information_log
            .expect("smartctl 7.4+ emits the error log table");
        let entry = log.table.first().expect("table has at least one entry");
        let status = entry.status_field.as_ref().expect("status_field on entry");
        assert_eq!(status.string, "Invalid Field in Command");
    }

    #[test]
    fn parse_scan_open_real_megaraid_dump() {
        // Real `smartctl --scan-open -j` from a system with a MegaRAID
        // controller. Two important real-world details the synthetic
        // fixture got wrong:
        //   1. Physical megaraid drives address `/dev/bus/0` (the
        //      smartctl management device), NOT the `/dev/sda` block
        //      device that holds the RAID volume. We don't dedupe with
        //      lsblk on these because lsblk doesn't know about
        //      /dev/bus/N.
        //   2. The RAID logical volume itself shows up separately as
        //      `/dev/sda` with `type: "scsi"` — the smartctl default
        //      for SAS/SCSI targets. `scsi` must strip to `None` so
        //      the UI doesn't show a redundant `[scsi]` chip next to
        //      what's already visibly a SCSI device.
        let raw = include_bytes!("../fixtures/scan_open_megaraid.json");
        let endpoints = parse_scan_open(raw);
        assert_eq!(endpoints.len(), 4);

        // RAID logical volume — default scsi transport stripped.
        let raid_volume = endpoints
            .iter()
            .find(|e| e.device == "/dev/sda")
            .expect("/dev/sda RAID volume entry");
        assert!(
            raid_volume.transport.is_none(),
            "scsi is smartctl's default for SCSI targets — must not chip the UI"
        );

        // Three physical drives addressed via /dev/bus/0 with distinct
        // megaraid slot transports. The (device, transport) pair is
        // the physical drive identity.
        let megaraid: Vec<&SmartctlEndpoint> = endpoints
            .iter()
            .filter(|e| e.device == "/dev/bus/0")
            .collect();
        assert_eq!(megaraid.len(), 3);
        assert_eq!(megaraid[0].transport.as_deref(), Some("sat+megaraid,0"));
        assert_eq!(megaraid[1].transport.as_deref(), Some("sat+megaraid,1"));
        assert_eq!(megaraid[2].transport.as_deref(), Some("sat+megaraid,2"));
    }

    #[test]
    fn parse_scan_open_filters_endpoints_with_open_error() {
        // smartctl emits `open_error` on entries it could enumerate
        // but not open (empty controller slots, drives that returned
        // an error on probe, etc.). Real smartctl 7.4 omits the field
        // entirely on successful opens (the megaraid fixture above
        // has no `open_error` keys at all), so the parser must:
        //   * treat absent open_error as success (serde default = "")
        //   * filter out entries where it IS present and non-empty
        // Inline JSON because no real-world fixture in the set
        // happens to contain a populated open_error.
        let raw = br#"{
            "devices": [
                {"name": "/dev/sda", "type": "sat"},
                {"name": "/dev/bus/0", "type": "megaraid,7",
                 "open_error": "DEVICESCAN failed: empty slot"},
                {"name": "", "type": "nvme"}
            ]
        }"#;
        let endpoints = parse_scan_open(raw);
        assert_eq!(endpoints.len(), 1, "only the unfilled-error entry survives");
        assert_eq!(endpoints[0].device, "/dev/sda");
    }

    #[test]
    fn megaraid_tunneled_sat_drive_parses_through_existing_ata_path() {
        // The whole point of scan-open enumeration: once smartctl is
        // invoked with the right `-d sat+megaraid,N` flag, the JSON it
        // returns is ordinary ATA SMART. No new parser needed — the
        // existing ata_smart_attributes path handles it. This fixture
        // is a real `smartctl -d megaraid,0 -a /dev/sda --json=c` dump
        // from a Samsung 860 EVO behind a MegaRAID controller.
        let j = parse(include_str!("../fixtures/ata_megaraid_sat.json"));
        let attrs = j.ata_smart_attributes.expect("ata attributes parse").table;
        // Wear_Leveling_Count (id 177) is the headline operator metric
        // on Samsung SSDs — normalized value started at 100 and is now
        // 32, meaning ~68% of the rated write endurance has been used.
        let wear = attrs
            .iter()
            .find(|a| a.id == 177)
            .expect("wear leveling attribute present");
        assert_eq!(wear.value, 32);
        assert_eq!(wear.raw.as_ref().map(|r| r.value), Some(1232));
        // No NVMe block on an ATA device.
        assert!(j.nvme_log.is_none());
    }

    #[test]
    fn parse_transport_strips_smartctl_defaults() {
        // Default protocol transports carry no info — keep them as None
        // so direct-attach drives don't get a useless `[sat]` chip.
        // `scsi` is included because it's the smartctl default for
        // SAS / RAID-volume targets — a real /dev/sda RAID volume
        // shows up as `type: "scsi"` in --scan-open and a redundant
        // `[scsi]` chip would just be visual noise.
        assert_eq!(parse_transport(""), None);
        assert_eq!(parse_transport("sat"), None);
        assert_eq!(parse_transport("nvme"), None);
        assert_eq!(parse_transport("ata"), None);
        assert_eq!(parse_transport("scsi"), None);
        // Anything that needs `-d <flag>` to actually open the device
        // must round-trip verbatim.
        assert_eq!(
            parse_transport("megaraid,0"),
            Some("megaraid,0".to_string())
        );
        assert_eq!(
            parse_transport("sat+megaraid,7"),
            Some("sat+megaraid,7".to_string())
        );
        assert_eq!(parse_transport("areca,3"), Some("areca,3".to_string()));
    }
}
