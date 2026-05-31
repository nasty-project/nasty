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
    let disks = enumerate_disks().await;
    let mut results = Vec::with_capacity(disks.len());
    for disk in disks {
        results.push(build_disk_health(disk).await);
    }
    results
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

async fn build_disk_health(disk: LsblkDisk) -> DiskHealth {
    let dev_name = disk.device.strip_prefix("/dev/").unwrap_or(&disk.device);
    let (ata_port, controller_pci) = resolve_device_path(dev_name);
    let controller_name = controller_pci.as_deref().and_then(resolve_pci_name);

    match query_smartctl(&disk.device).await {
        Some(s) => DiskHealth {
            device: disk.device,
            ata_port,
            controller_pci,
            controller_name,
            // smartctl's strings are usually more accurate than lsblk's
            // (lsblk reads sysfs's `model`, which truncates / pads), but
            // for unfamiliar transports lsblk sometimes wins — prefer
            // smartctl, fall back to lsblk, then to "Unknown".
            model: s.model.filter(|s| !s.is_empty()).unwrap_or(disk.model),
            serial: s.serial.filter(|s| !s.is_empty()).unwrap_or(disk.serial),
            firmware: s.firmware.unwrap_or_else(|| "Unknown".into()),
            capacity_bytes: if s.capacity_bytes > 0 {
                s.capacity_bytes
            } else {
                disk.size_bytes
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
        },
        None => DiskHealth {
            device: disk.device,
            ata_port,
            controller_pci,
            controller_name,
            model: disk.model,
            serial: disk.serial,
            firmware: "Unknown".into(),
            capacity_bytes: disk.size_bytes,
            temperature_c: None,
            power_on_hours: None,
            // Distinct from FAILED so the WebUI can style + the alert
            // rules can skip. See SMART_STATUS_UNAVAILABLE.
            health_passed: false,
            smart_status: SMART_STATUS_UNAVAILABLE.to_string(),
            attributes: Vec::new(),
        },
    }
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
}

async fn query_smartctl(device: &str) -> Option<SmartReport> {
    let output = tokio::process::Command::new("smartctl")
        .args(["-a", "--json=c", device])
        .output()
        .await
        .ok()?;

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

    Some(SmartReport {
        model: json.model_name,
        serial: json.serial_number,
        firmware: json.firmware_version,
        capacity_bytes: json.user_capacity.map(|c| c.bytes).unwrap_or(0),
        temperature_c: json.temperature.and_then(|t| t.current),
        power_on_hours: json.power_on_time.and_then(|p| p.hours),
        health_passed,
        attributes,
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
