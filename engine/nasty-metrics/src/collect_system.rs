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

    // SCSI / SAS — populated when smartctl talks to a SAS drive
    // directly (`type: "scsi"`) or via a controller passthrough
    // (`type: "megaraid,N"` for pure SCSI behind megaraid, vs the
    // `"sat+megaraid,N"` we already see for SATA-tunneled).
    #[serde(default)]
    scsi_revision: Option<String>,
    #[serde(default)]
    scsi_version: Option<String>,
    #[serde(default)]
    rotation_rate: Option<u32>,
    #[serde(default)]
    form_factor: Option<SmartctlFormFactor>,
    #[serde(default)]
    logical_unit_id: Option<String>,
    #[serde(default)]
    scsi_transport_protocol: Option<SmartctlScsiTransport>,
    #[serde(default)]
    scsi_grown_defect_list: Option<u64>,
    #[serde(default)]
    scsi_format_status: Option<SmartctlScsiFormatStatus>,
    #[serde(default)]
    scsi_start_stop_cycle_counter: Option<SmartctlScsiCycleCounter>,
    #[serde(default)]
    scsi_error_counter_log: Option<SmartctlScsiErrorCounterLog>,

    // ATA — interface link speed + endurance. The SMART attribute table
    // already covers everything else ATA reports; this block exists for
    // fields outside the attributes payload.
    #[serde(default)]
    interface_speed: Option<SmartctlInterfaceSpeed>,
    /// smartctl 7.5+ exposes `endurance_used.current_percent` at the
    /// top level for ATA drives — computed from each drive's
    /// Media_Wearout_Indicator-equivalent attribute (id varies per
    /// vendor). Saves us from hunting through the attributes table for
    /// vendor-specific encodings. Absent on drives without wear
    /// reporting (spinners, pre-MWI SSDs) and on smartctl < 7.5.
    #[serde(default)]
    endurance_used: Option<SmartctlEnduranceUsed>,

    // SCSI self-test entries are emitted as numbered top-level keys
    // (`scsi_self_test_0`, `scsi_self_test_1`, …) rather than an
    // array, so we capture them via `#[serde(flatten)]` and walk the
    // raw map after the typed parse. Drives we've seen carry 4–14
    // entries; the SCSI standard caps at 20.
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct SmartctlInterfaceSpeed {
    #[serde(default)]
    current: Option<SmartctlInterfaceSpeedRate>,
    #[serde(default)]
    max: Option<SmartctlInterfaceSpeedRate>,
}

#[derive(Deserialize)]
struct SmartctlInterfaceSpeedRate {
    /// Pre-formatted speed string smartctl emits (e.g. `"6.0 Gb/s"`).
    /// We could compute it from `units_per_second × bits_per_unit` but
    /// the string already matches what operators see in `smartctl -a`.
    #[serde(default)]
    string: String,
}

#[derive(Deserialize)]
struct SmartctlEnduranceUsed {
    #[serde(default)]
    current_percent: Option<u32>,
}

#[derive(Deserialize)]
struct SmartctlFormFactor {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct SmartctlScsiTransport {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct SmartctlScsiFormatStatus {
    #[serde(default)]
    power_on_minutes_since_format: Option<u64>,
}

#[derive(Deserialize)]
struct SmartctlScsiCycleCounter {
    #[serde(default)]
    year_of_manufacture: Option<String>,
    #[serde(default)]
    week_of_manufacture: Option<String>,
    #[serde(default)]
    specified_cycle_count_over_device_lifetime: Option<u64>,
    #[serde(default)]
    accumulated_start_stop_cycles: Option<u64>,
    #[serde(default)]
    specified_load_unload_count_over_device_lifetime: Option<u64>,
    #[serde(default)]
    accumulated_load_unload_cycles: Option<u64>,
}

#[derive(Deserialize)]
struct SmartctlScsiErrorCounterLog {
    #[serde(default)]
    read: Option<SmartctlScsiErrorCounters>,
    #[serde(default)]
    write: Option<SmartctlScsiErrorCounters>,
    #[serde(default)]
    verify: Option<SmartctlScsiErrorCounters>,
}

#[derive(Deserialize)]
struct SmartctlScsiErrorCounters {
    #[serde(default)]
    total_errors_corrected: u64,
    #[serde(default)]
    total_uncorrected_errors: u64,
    /// smartctl emits this as a string with decimals
    /// (e.g. `"1112235.529"`) so we deserialize via string then parse.
    #[serde(default)]
    gigabytes_processed: String,
}

#[derive(Deserialize)]
struct SmartctlScsiSelfTestEntry {
    #[serde(default)]
    code: Option<SmartctlScsiTestCode>,
    #[serde(default)]
    result: Option<SmartctlScsiTestResult>,
    /// Some smartctl versions report `self_test_in_progress: true` on
    /// the active entry rather than embedding it in the result string.
    #[serde(default)]
    self_test_in_progress: bool,
    /// Newer smartctl emits `power_on_time: {hours, aka}`; we also
    /// accept plain `power_on_hours` for forward compatibility.
    #[serde(default)]
    power_on_time: Option<SmartctlPowerOn>,
    #[serde(default)]
    power_on_hours: Option<u64>,
}

#[derive(Deserialize)]
struct SmartctlScsiTestCode {
    #[serde(default)]
    string: String,
}

#[derive(Deserialize)]
struct SmartctlScsiTestResult {
    #[serde(default)]
    string: String,
    /// smartctl reports 0 = completed cleanly; non-zero codes for
    /// aborted, in-progress, or failed outcomes.
    #[serde(default)]
    value: i64,
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
    /// Drive's hard shutdown temperature in °C. SAS drives populate
    /// this from the SCSI Informational Exceptions log; NVMe drives
    /// expose `nvme_composite_temperature_threshold` separately and
    /// leave this `None`.
    #[serde(default)]
    drive_trip: Option<i32>,
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
    // Resolution differs by how smartctl reaches the drive:
    //   * Direct-attach (transport=None): walk /sys/block/<name>/device
    //     for both ata_port and controller_pci.
    //   * RAID-tunneled (transport=Some("megaraid,N"), etc.): the
    //     "device" is /dev/bus/N, a smartctl management node — not a
    //     block device, so /sys/block lookup is meaningless. The drive
    //     has no meaningful ata_port (it's behind a SAS / SATA-tunneled
    //     backplane), but the host controller IS a PCIe device and its
    //     upstream link is the actual bandwidth ceiling for every
    //     physical drive behind it. Walk /sys/class/scsi_host/hostN to
    //     reach it.
    let (ata_port, controller_pci) = if endpoint.transport.is_none() {
        resolve_device_path(dev_name)
    } else {
        (None, resolve_raid_host_pci(&endpoint.device))
    };
    let controller_name = controller_pci.as_deref().and_then(resolve_pci_name);
    let pcie_link = controller_pci.as_deref().and_then(resolve_pcie_link);

    match query_smartctl(&endpoint.device, endpoint.transport.as_deref()).await {
        Some(s) => DiskHealth {
            device: endpoint.device.clone(),
            transport: endpoint.transport.clone(),
            ata_port,
            controller_pci,
            controller_name,
            pcie_link,
            // smartctl's strings are usually more accurate than lsblk's
            // (lsblk reads sysfs's `model`, which truncates / pads), but
            // for unfamiliar transports lsblk sometimes wins — prefer
            // smartctl, fall back to lsblk, then to "Unknown". Both
            // sources can carry padded SCSI Inquiry whitespace; normalize
            // the lsblk fallback for the same reason we normalize
            // smartctl's output in `query_smartctl`.
            model: s
                .model
                .filter(|s| !s.is_empty())
                .or_else(|| lsblk_hint.and_then(|d| normalize_model(&d.model)))
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
            scsi: s.scsi,
            ata: s.ata,
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
        (None, resolve_raid_host_pci(&endpoint.device))
    };
    let controller_name = controller_pci.as_deref().and_then(resolve_pci_name);
    let pcie_link = controller_pci.as_deref().and_then(resolve_pcie_link);
    DiskHealth {
        device: endpoint.device.clone(),
        transport: endpoint.transport.clone(),
        ata_port,
        controller_pci,
        controller_name,
        pcie_link,
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
        scsi: None,
        ata: None,
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
    scsi: Option<ScsiHealth>,
    ata: Option<AtaHealth>,
}

/// Normalize a SCSI Inquiry / ATA Identify product string by trimming
/// edges and collapsing runs of internal whitespace to a single space.
/// The SCSI Inquiry response pads model fields to a fixed width, so
/// real-world dumps carry strings like `"MG08ACP1 6TE           SM"`
/// — left as-is the WebUI displays that gap verbatim. Returns `None`
/// when the normalized result is empty so the caller can fall through
/// to its existing fallback chain.
fn normalize_model(raw: &str) -> Option<String> {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
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

    // All borrow-based extractions happen BEFORE any `.map()` that
    // moves a field out of `json`. build_scsi_health borrows the
    // whole struct and most_recent_error borrows the NVMe error log;
    // both need to complete before we start consuming fields below.
    let scsi = build_scsi_health(&json);
    let ata = build_ata_health(&json);

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

    // smart_status.passed is the drive's own self-assessment, but for
    // SAS drives that bar is set so high that drives with eight
    // uncorrected write errors and nine remapped sectors still report
    // PASSED — by the time it flips, you've already lost data. Override
    // health to FAILED when any I/O type shows uncorrected errors so
    // the existing SmartHealth alert + WebUI badge fire while there's
    // still time to plan a replacement. We deliberately don't trigger
    // on grown_defect_list alone: a non-zero defect list is normal on
    // aging drives and would generate too many false positives; the
    // trend matters more than the absolute count.
    let scsi_failure_override = scsi.as_ref().is_some_and(|s| {
        s.read_errors.uncorrected_total > 0
            || s.write_errors.uncorrected_total > 0
            || s.verify_errors.uncorrected_total > 0
    });
    let health_passed = health_passed && !scsi_failure_override;

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
        // SCSI Inquiry pads model fields to a fixed width — `"MG08ACP1
        // 6TE           SM"` is real-world output. Normalize so the UI
        // shows a clean string.
        model: json.model_name.as_deref().and_then(normalize_model),
        serial: json.serial_number,
        // SAS dumps don't populate `firmware_version`; the SCSI Inquiry
        // exposes the same string as `scsi_revision`. Fall through so
        // SAS drives stop showing "Unknown" firmware.
        firmware: json.firmware_version.or(json.scsi_revision),
        capacity_bytes: json.user_capacity.map(|c| c.bytes).unwrap_or(0),
        temperature_c: json.temperature.and_then(|t| t.current),
        power_on_hours: json.power_on_time.and_then(|p| p.hours),
        health_passed,
        attributes,
        nvme,
        scsi,
        ata,
    })
}

/// Build the ATA / SATA summary block. Returns `None` for non-ATA
/// drives so DiskHealth.ata stays absent in the JSON for NVMe / SAS.
fn build_ata_health(json: &SmartctlJson) -> Option<AtaHealth> {
    // Trigger: smartctl returned an interface_speed OR endurance_used
    // object. ATA dumps emit one or both; NVMe and SAS dumps don't
    // emit interface_speed (NVMe uses PCIe link, SAS uses scsi_transport_
    // protocol; neither is a smartctl interface_speed). Endurance is
    // present only on smartctl 7.5+ ATA dumps for drives that report
    // wear, so it's a weaker signal — interface_speed remains the
    // primary detector.
    let cur = json
        .interface_speed
        .as_ref()
        .and_then(|s| s.current.as_ref())
        .map(|r| r.string.trim().to_string())
        .filter(|s| !s.is_empty());
    let max = json
        .interface_speed
        .as_ref()
        .and_then(|s| s.max.as_ref())
        .map(|r| r.string.trim().to_string())
        .filter(|s| !s.is_empty());
    let endurance = json.endurance_used.as_ref().and_then(|e| e.current_percent);
    if cur.is_none() && max.is_none() && endurance.is_none() {
        return None;
    }
    Some(AtaHealth {
        interface_speed_current: cur,
        interface_speed_max: max,
        endurance_used_percent: endurance,
    })
}

/// Build the SAS / SCSI health block from a parsed smartctl dump.
/// Returns `None` when the dump carries no SCSI-specific fields — i.e.
/// the drive is ATA or NVMe — so the resulting `DiskHealth.scsi` field
/// stays absent in the API response.
fn build_scsi_health(json: &SmartctlJson) -> Option<ScsiHealth> {
    // Heuristic for "this is a SAS / SCSI drive": at least one of the
    // SCSI-flavoured fields is populated. We don't gate on a single
    // field because real-world dumps vary — an old drive may lack
    // scsi_format_status, a fresh drive may lack scsi_grown_defect_list,
    // etc. Drives via megaraid don't carry `scsi_transport_protocol`
    // either, so combine several signals.
    let looks_scsi = json.scsi_revision.is_some()
        || json.scsi_version.is_some()
        || json.scsi_transport_protocol.is_some()
        || json.scsi_grown_defect_list.is_some()
        || json.scsi_error_counter_log.is_some()
        || json.scsi_start_stop_cycle_counter.is_some();
    if !looks_scsi {
        return None;
    }

    let cycle = &json.scsi_start_stop_cycle_counter;
    let read_errors = json
        .scsi_error_counter_log
        .as_ref()
        .and_then(|l| l.read.as_ref())
        .map(scsi_error_counters_from)
        .unwrap_or_default();
    let write_errors = json
        .scsi_error_counter_log
        .as_ref()
        .and_then(|l| l.write.as_ref())
        .map(scsi_error_counters_from)
        .unwrap_or_default();
    let verify_errors = json
        .scsi_error_counter_log
        .as_ref()
        .and_then(|l| l.verify.as_ref())
        .map(scsi_error_counters_from)
        .unwrap_or_default();

    let (self_test_count, last_self_test) = collect_scsi_self_tests(&json.extra);

    Some(ScsiHealth {
        transport_protocol: json
            .scsi_transport_protocol
            .as_ref()
            .map(|t| t.name.clone())
            .filter(|s| !s.is_empty()),
        scsi_version: json.scsi_version.clone().filter(|s| !s.is_empty()),
        rotation_rate: json.rotation_rate,
        form_factor: json
            .form_factor
            .as_ref()
            .map(|f| f.name.clone())
            .filter(|s| !s.is_empty()),
        logical_unit_id: json.logical_unit_id.clone().filter(|s| !s.is_empty()),
        drive_trip_temp_c: json.temperature.as_ref().and_then(|t| t.drive_trip),
        year_of_manufacture: cycle
            .as_ref()
            .and_then(|c| c.year_of_manufacture.clone())
            .filter(|s| !s.is_empty()),
        week_of_manufacture: cycle
            .as_ref()
            .and_then(|c| c.week_of_manufacture.clone())
            .filter(|s| !s.is_empty()),
        grown_defect_list: json.scsi_grown_defect_list,
        power_on_minutes_since_format: json
            .scsi_format_status
            .as_ref()
            .and_then(|f| f.power_on_minutes_since_format),
        start_stop_cycles: cycle.as_ref().and_then(|c| c.accumulated_start_stop_cycles),
        start_stop_cycles_designed: cycle
            .as_ref()
            .and_then(|c| c.specified_cycle_count_over_device_lifetime),
        load_unload_cycles: cycle
            .as_ref()
            .and_then(|c| c.accumulated_load_unload_cycles),
        load_unload_cycles_designed: cycle
            .as_ref()
            .and_then(|c| c.specified_load_unload_count_over_device_lifetime),
        read_errors,
        write_errors,
        verify_errors,
        last_self_test,
        self_test_count,
    })
}

fn scsi_error_counters_from(c: &SmartctlScsiErrorCounters) -> ScsiErrorCounters {
    ScsiErrorCounters {
        corrected_total: c.total_errors_corrected,
        uncorrected_total: c.total_uncorrected_errors,
        // smartctl writes the I/O volume as a decimal-quoted string
        // ("1112235.529"). Round-tripping through f64 keeps the
        // precision the operator cares about (we display 1 decimal at
        // most). On parse failure default to 0.0 — better than failing
        // the whole SCSI block over a single field.
        gigabytes_processed: c.gigabytes_processed.parse().unwrap_or(0.0),
    }
}

/// Walk the flattened map of unknown keys for `scsi_self_test_N` entries,
/// returning (total_count, most_recent_entry). Drives we've seen carry
/// 4–14 entries; smartctl numbers them 0 = newest, N = oldest.
fn collect_scsi_self_tests(
    extra: &std::collections::BTreeMap<String, serde_json::Value>,
) -> (u32, Option<ScsiSelfTestEntry>) {
    let mut count: u32 = 0;
    let mut most_recent: Option<ScsiSelfTestEntry> = None;
    // BTreeMap is sorted lexically; scsi_self_test_0..scsi_self_test_9
    // come before _10.._19 numerically too, so the first-by-index entry
    // is what we want for "most recent".
    for n in 0..20 {
        let key = format!("scsi_self_test_{n}");
        let Some(value) = extra.get(&key) else {
            continue;
        };
        count += 1;
        if most_recent.is_some() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_value::<SmartctlScsiSelfTestEntry>(value.clone()) {
            most_recent = Some(ScsiSelfTestEntry {
                code: parsed
                    .code
                    .as_ref()
                    .map(|c| c.code_string())
                    .unwrap_or_default(),
                result: parsed
                    .result
                    .as_ref()
                    .map(|r| r.string.clone())
                    .unwrap_or_default(),
                // result.value 0 = "Completed without error" / "Completed".
                // Anything else (aborted, in progress, failed) is not a
                // healthy passed-test signal.
                passed: parsed.result.as_ref().is_some_and(|r| r.value == 0),
                power_on_hours: parsed
                    .power_on_time
                    .as_ref()
                    .and_then(|p| p.hours)
                    .or(parsed.power_on_hours),
                in_progress: parsed.self_test_in_progress,
            });
        }
    }
    (count, most_recent)
}

impl SmartctlScsiTestCode {
    fn code_string(&self) -> String {
        self.string.clone()
    }
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

/// Resolve the PCI BDF for the SCSI host backing a RAID-tunneled
/// smartctl device. Smartctl reaches drives behind a megaraid (or
/// areca / cciss / 3ware) controller via `/dev/bus/N` — a management
/// node, not a block device — so `/sys/block` lookups don't apply.
/// Instead, walk `/sys/class/scsi_host/hostN` back to the kernel's
/// PCI device representation of the controller.
///
/// Today this only handles the `/dev/bus/N` form smartctl uses for
/// megaraid; areca / cciss / 3ware use different device-name
/// conventions (`/dev/sgN`, `/dev/cciss/cNd0`, `/dev/twaN`) and would
/// each need their own host-number extraction. The sysfs walk itself
/// is generic — only the device → host-number translation differs.
fn resolve_raid_host_pci(device: &str) -> Option<String> {
    let host_num = device.strip_prefix("/dev/bus/")?.parse::<u32>().ok()?;
    let resolved = std::fs::canonicalize(format!("/sys/class/scsi_host/host{host_num}")).ok()?;
    extract_deepest_pci_bdf(resolved.to_str()?)
}

/// Pull the deepest (most-specific, leaf-ward) PCI BDF out of a sysfs
/// device path. The kernel formats PCI components as full domain BDFs
/// `0000:03:00.0`; we strip the canonical `0000:` domain to match the
/// short-form BDF used by `resolve_device_path` and consumed by the
/// lspci wrapper. Returns `None` for paths with no PCI components
/// (devices on non-PCI buses — software iSCSI hosts, virtio, etc.).
fn extract_deepest_pci_bdf(path: &str) -> Option<String> {
    // Walk components leaf-to-root, return the first short-form BDF
    // we find. "Last" semantically — but iterating in reverse lets
    // us short-circuit on first hit.
    path.split('/')
        .rev()
        .filter_map(|c| c.strip_prefix("0000:"))
        .find(|s| s.contains(':') && s.contains('.'))
        .map(str::to_string)
}

/// Read PCIe link state for a storage controller from sysfs. Returns
/// `None` when the controller isn't found, lacks the link files (some
/// virtual PCIe devices, very old kernels), or returns malformed data.
///
/// `pci_addr` is the short BDF form (e.g. `"03:00.0"`) — we prepend the
/// canonical `0000:` domain when looking up the sysfs path because
/// that's what `resolve_device_path()` returns elsewhere in this file
/// and we want consistency.
fn resolve_pcie_link(pci_addr: &str) -> Option<PcieLink> {
    let base = format!("/sys/bus/pci/devices/0000:{pci_addr}");
    parse_pcie_link(
        &std::fs::read_to_string(format!("{base}/current_link_speed")).ok()?,
        &std::fs::read_to_string(format!("{base}/max_link_speed")).ok()?,
        &std::fs::read_to_string(format!("{base}/current_link_width")).ok()?,
        &std::fs::read_to_string(format!("{base}/max_link_width")).ok()?,
    )
}

/// Pure-data parser for PCIe link sysfs strings. Split out from
/// `resolve_pcie_link` so the parser can be unit-tested without a real
/// `/sys` tree to point at.
///
/// Kernels normalize speed strings to forms like `"8.0 GT/s PCIe"`
/// (recent) or `"8.0 GT/s"` (older); both pass through verbatim after
/// whitespace-trimming. Width files contain a bare decimal. Any value
/// that fails to parse (or returns the literal `"Unknown"` some
/// kernels emit for downed links) makes the whole block return `None`
/// — we don't surface a half-populated link record.
fn parse_pcie_link(
    cur_speed: &str,
    max_speed: &str,
    cur_width: &str,
    max_width: &str,
) -> Option<PcieLink> {
    let cur_speed = cur_speed.trim();
    let max_speed = max_speed.trim();
    if cur_speed.is_empty()
        || max_speed.is_empty()
        || cur_speed.eq_ignore_ascii_case("Unknown")
        || max_speed.eq_ignore_ascii_case("Unknown")
    {
        return None;
    }
    let cur_width: u8 = cur_width.trim().parse().ok()?;
    let max_width: u8 = max_width.trim().parse().ok()?;
    // A zero width means the link is in some degenerate state (e.g.
    // device powered off, hot-removed). Reporting "PCIe x0" would be
    // misleading; drop the record entirely.
    if cur_width == 0 || max_width == 0 {
        return None;
    }
    Some(PcieLink {
        current_speed: cur_speed.to_string(),
        max_speed: max_speed.to_string(),
        current_width: cur_width,
        max_width,
    })
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

    // ── SAS / SCSI ─────────────────────────────────────────────────
    //
    // Two real-world SAS dumps capture the operator-relevant range:
    //   * sas_seagate_clean    — healthy 4TB 7200 RPM SAS spinner,
    //                            5+ years powered on, all zero error
    //                            counters, mid-self-test (proves the
    //                            in-progress entry doesn't break the
    //                            parser).
    //   * sas_seagate_failing  — dying 400GB 10K RPM SAS spinner
    //                            behind megaraid: 9 grown defects,
    //                            8 uncorrected write errors. smartctl
    //                            STILL says smart_status.passed=true
    //                            on this drive, which is the entire
    //                            reason we need the health override.

    #[test]
    fn sas_clean_drive_parses_complete_scsi_block() {
        let j = parse(include_str!("../fixtures/sas_seagate_clean.json"));
        let s = build_scsi_health(&j).expect("SCSI block present");

        assert_eq!(s.transport_protocol.as_deref(), Some("SAS (SPL-4)"));
        assert_eq!(s.scsi_version.as_deref(), Some("SPC-3"));
        assert_eq!(s.rotation_rate, Some(7200));
        assert_eq!(s.form_factor.as_deref(), Some("3.5 inches"));
        assert_eq!(s.drive_trip_temp_c, Some(68));
        assert_eq!(s.grown_defect_list, Some(0));
        assert_eq!(s.year_of_manufacture.as_deref(), Some("2019"));
        assert_eq!(s.week_of_manufacture.as_deref(), Some("18"));
        assert_eq!(s.power_on_minutes_since_format, Some(221818));
        assert_eq!(s.start_stop_cycles, Some(97));
        assert_eq!(s.start_stop_cycles_designed, Some(10000));
        assert_eq!(s.load_unload_cycles, Some(1959));

        // All error counters zero on a healthy drive.
        assert_eq!(s.read_errors.uncorrected_total, 0);
        assert_eq!(s.write_errors.uncorrected_total, 0);
        assert_eq!(s.verify_errors.uncorrected_total, 0);
        // gigabytes_processed is parsed from its string representation.
        assert!((s.read_errors.gigabytes_processed - 1_112_235.529).abs() < 0.001);

        // 14 self-test entries (0 through 13). The most recent (entry 0)
        // is mid-test: must parse the in_progress flag correctly.
        assert_eq!(s.self_test_count, 14);
        let last = s.last_self_test.expect("most-recent self-test present");
        assert!(
            last.in_progress,
            "scsi_self_test_0 has self_test_in_progress=true"
        );
        assert_eq!(last.code, "Background long");
    }

    #[test]
    fn sas_failing_drive_triggers_health_override() {
        // This is the entire reason the SAS PR exists. The drive
        // reports smart_status.passed=true (smartctl's own bar is
        // way too high for SAS) but has 8 uncorrected write errors
        // and 9 grown defects. Our override flips health_passed to
        // false so the SmartHealth alert fires and the operator
        // sees a FAILED badge while there's still time to replace it.
        let raw = include_str!("../fixtures/sas_seagate_failing.json");
        let j: SmartctlJson = serde_json::from_str(raw).expect("parse failing SAS dump");

        // Drive's own self-assessment: still PASSED. Operator would
        // never know without us digging deeper.
        assert!(j.smart_status.as_ref().is_some_and(|s| s.passed));

        let scsi = build_scsi_health(&j).expect("SCSI block present");
        assert_eq!(scsi.grown_defect_list, Some(9));
        assert_eq!(scsi.write_errors.uncorrected_total, 8);
        assert_eq!(scsi.read_errors.uncorrected_total, 0);
        assert_eq!(scsi.verify_errors.uncorrected_total, 0);

        // The override condition we apply in query_smartctl:
        // health flips to false if ANY I/O type has uncorrected errors.
        let failure_override = scsi.read_errors.uncorrected_total > 0
            || scsi.write_errors.uncorrected_total > 0
            || scsi.verify_errors.uncorrected_total > 0;
        assert!(
            failure_override,
            "uncorrected-error override must fire on this drive"
        );

        // Drive is behind megaraid: no scsi_format_status, no
        // scsi_start_stop_cycle_counter. Our deserializer must
        // tolerate the absence and leave the corresponding ScsiHealth
        // fields None (rather than failing the whole SCSI parse).
        assert_eq!(scsi.power_on_minutes_since_format, None);
        assert_eq!(scsi.year_of_manufacture, None);
        assert_eq!(scsi.start_stop_cycles, None);
    }

    #[test]
    fn scsi_revision_falls_back_when_firmware_version_absent() {
        // SAS dumps don't populate `firmware_version`. The SCSI
        // Inquiry exposes the same field as `scsi_revision`. Without
        // this fallback every SAS drive would show "Unknown" firmware.
        let j = parse(include_str!("../fixtures/sas_seagate_clean.json"));
        assert!(j.firmware_version.is_none());
        assert_eq!(j.scsi_revision.as_deref(), Some("BS03"));
        // Mirror the query_smartctl fallback logic.
        let firmware = j.firmware_version.or(j.scsi_revision);
        assert_eq!(firmware.as_deref(), Some("BS03"));
    }

    #[test]
    fn nvme_drive_has_no_scsi_block() {
        // The detection heuristic in build_scsi_health must NOT
        // misfire on a pure NVMe dump — none of the scsi_* fields
        // are present.
        let j = parse(include_str!("../fixtures/nvme_samsung_980_pro.json"));
        assert!(build_scsi_health(&j).is_none());
    }

    // ── ATA ────────────────────────────────────────────────────────

    #[test]
    fn ata_he10_parses_interface_speed_and_helium_attribute() {
        // First helium-drive fixture. HGST Ultrastar He10 (10TB SATA
        // spinner) trained down to 3.0 Gb/s on a 6.0 Gb/s port —
        // common cable / backplane symptom. Also carries the
        // vendor-specific Helium_Level attribute (id 22) we want to
        // call out in the WebUI's criticalIds set.
        let j = parse(include_str!("../fixtures/ata_hgst_he10.json"));

        // Borrow-based checks first — both ata + scsi build_* take &j.
        let ata = build_ata_health(&j).expect("ATA block present");
        assert_eq!(ata.interface_speed_current.as_deref(), Some("3.0 Gb/s"));
        assert_eq!(ata.interface_speed_max.as_deref(), Some("6.0 Gb/s"));
        // No SCSI block on an ATA dump.
        assert!(build_scsi_health(&j).is_none());

        // Helium_Level is just another row in the SMART attribute
        // table — no special parser path, but pin it so a future
        // refactor of the attribute parser doesn't silently drop it.
        let attrs = j.ata_smart_attributes.expect("ata attributes parse").table;
        let helium = attrs
            .iter()
            .find(|a| a.id == 22)
            .expect("Helium_Level attribute present");
        assert_eq!(helium.name, "Helium_Level");
        assert_eq!(helium.value, 100);
        assert_eq!(helium.thresh, 25);
    }

    #[test]
    fn nvme_drive_has_no_ata_block() {
        // Reverse symmetry of nvme_drive_has_no_scsi_block — NVMe
        // dumps don't carry interface_speed either.
        let j = parse(include_str!("../fixtures/nvme_samsung_980_pro.json"));
        assert!(build_ata_health(&j).is_none());
    }

    #[test]
    fn ata_ssd_parses_endurance_used_from_smartctl_7_5() {
        // KIOXIA EXCERIA SATA SSD — smartctl 7.5 emits a top-level
        // endurance_used.current_percent for ATA SSDs, computed from
        // each drive's Media_Wearout_Indicator attribute (the encoding
        // varies per vendor; smartctl normalizes it for us). Saves the
        // operator from having to know that for KIOXIA the wear lives
        // in attribute 173 with a vendor-specific raw encoding.
        let j = parse(include_str!("../fixtures/ata_kioxia_ssd.json"));
        let ata = build_ata_health(&j).expect("ATA block present");
        // Fresh drive — 0% endurance consumed.
        assert_eq!(ata.endurance_used_percent, Some(0));
        // Interface fields still populate alongside endurance.
        assert_eq!(ata.interface_speed_current.as_deref(), Some("6.0 Gb/s"));
    }

    #[test]
    fn ata_spinner_without_endurance_still_builds() {
        // HGST He10 is a spinner — no Media_Wearout_Indicator, no
        // endurance_used in the dump. The ATA block must still build
        // (driven by interface_speed) with endurance_used_percent=None.
        // Skip-serialize means the field disappears from the JSON for
        // these drives; the UI just hides the tile.
        let j = parse(include_str!("../fixtures/ata_hgst_he10.json"));
        let ata = build_ata_health(&j).expect("ATA block present");
        assert_eq!(ata.endurance_used_percent, None);
        assert!(ata.interface_speed_current.is_some());
    }

    // ── PCIe link state ───────────────────────────────────────────

    #[test]
    fn parse_pcie_link_typical_modern_kernel_output() {
        // What `cat /sys/bus/pci/devices/0000:03:00.0/current_link_speed`
        // emits on a recent kernel — single newline, "PCIe" suffix.
        let link = parse_pcie_link("8.0 GT/s PCIe\n", "8.0 GT/s PCIe\n", "2\n", "2\n")
            .expect("link present");
        assert_eq!(link.current_speed, "8.0 GT/s PCIe");
        assert_eq!(link.max_speed, "8.0 GT/s PCIe");
        assert_eq!(link.current_width, 2);
        assert_eq!(link.max_width, 2);
    }

    #[test]
    fn parse_pcie_link_handles_older_kernel_format_without_pcie_suffix() {
        // Older kernels emit "8 GT/s" without the "PCIe" suffix —
        // accept verbatim, don't try to normalize.
        let link = parse_pcie_link("8 GT/s\n", "8 GT/s\n", "4\n", "4\n").expect("link present");
        assert_eq!(link.current_speed, "8 GT/s");
        assert_eq!(link.max_width, 4);
    }

    #[test]
    fn parse_pcie_link_detects_downgrade() {
        // Trained-down case — controller could do 16 GT/s x4 but the
        // slot or cable only negotiated 8 GT/s x2. WebUI flags this
        // amber.
        let link = parse_pcie_link("8.0 GT/s\n", "16.0 GT/s\n", "2\n", "4\n").expect("link");
        assert_ne!(link.current_speed, link.max_speed);
        assert_ne!(link.current_width, link.max_width);
    }

    #[test]
    fn parse_pcie_link_rejects_degenerate_states() {
        // Kernel sometimes reports "Unknown" speed during a power-state
        // transition. Reporting "PCIe Unknown x0" would be worse than
        // hiding the chip entirely.
        assert!(parse_pcie_link("Unknown\n", "8.0 GT/s\n", "2\n", "2\n").is_none());
        assert!(parse_pcie_link("8.0 GT/s\n", "8.0 GT/s\n", "0\n", "2\n").is_none());
        // Empty / missing files — caller passes empty strings.
        assert!(parse_pcie_link("\n", "\n", "0\n", "0\n").is_none());
        // Garbage in width files (non-numeric).
        assert!(parse_pcie_link("8.0 GT/s\n", "8.0 GT/s\n", "x4\n", "4\n").is_none());
    }

    #[test]
    fn extract_pci_bdf_picks_deepest_controller_in_path() {
        // Real canonical sysfs path for a megaraid host. The walk
        // should return the megaraid controller's BDF (01:00.0), not
        // the upstream root-port (00:01.1). resolve_pcie_link will
        // then read /sys/bus/pci/devices/0000:01:00.0/{current,max}_*.
        let path = "/sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0/host0";
        assert_eq!(extract_deepest_pci_bdf(path).as_deref(), Some("01:00.0"));
    }

    #[test]
    fn extract_pci_bdf_strips_canonical_domain_prefix() {
        // Sysfs always emits the full domain (0000:); the rest of
        // the codebase uses the short BDF form. Verify we hand back
        // the short form so resolve_pcie_link / resolve_pci_name
        // don't double-prefix the domain when building their paths.
        let path = "/sys/devices/pci0000:00/0000:03:00.0/ata5/host5";
        let bdf = extract_deepest_pci_bdf(path).expect("BDF present");
        assert!(!bdf.starts_with("0000:"));
        assert_eq!(bdf, "03:00.0");
    }

    #[test]
    fn extract_pci_bdf_handles_paths_without_pci_components() {
        // Software iSCSI hosts, virtio devices in some configs, and
        // other non-PCI bus types yield paths with no BDF components.
        // Return None so the caller's chain (controller_pci → name +
        // pcie_link) all stays empty rather than spuriously firing.
        assert_eq!(
            extract_deepest_pci_bdf("/sys/devices/platform/some_iscsi/host7"),
            None
        );
        assert_eq!(extract_deepest_pci_bdf(""), None);
    }

    #[test]
    fn normalize_model_collapses_scsi_inquiry_padding() {
        // Real Toshiba MG08 SAS dump returns:
        //   scsi_product:    "6TE           SM"
        //   model_name:      "MG08ACP1 6TE           SM"
        // The padded run of spaces is from the SCSI Inquiry's
        // fixed-width product field — left as-is the WebUI shows
        // a 13-character gap mid-string.
        let j = parse(include_str!("../fixtures/sas_toshiba_mg08_padded.json"));
        let raw = j.model_name.as_deref().expect("model_name present");
        assert!(
            raw.contains("           "),
            "fixture must preserve raw padding"
        );
        assert_eq!(normalize_model(raw).as_deref(), Some("MG08ACP1 6TE SM"));
    }

    #[test]
    fn normalize_model_handles_edge_cases() {
        assert_eq!(normalize_model(""), None);
        assert_eq!(normalize_model("   "), None);
        assert_eq!(
            normalize_model("Samsung SSD 980 PRO"),
            Some("Samsung SSD 980 PRO".into())
        );
        // Leading + trailing whitespace gets trimmed; interior runs
        // collapse to one space (not removed entirely — we still want
        // word boundaries).
        assert_eq!(
            normalize_model("  HGST    HUH721010ALE600  "),
            Some("HGST HUH721010ALE600".into())
        );
    }
}
