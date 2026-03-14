pub mod alerts;
pub mod metrics;
pub mod protocol;
pub mod settings;
pub mod update;

use serde::{Deserialize, Serialize};

pub struct SystemService;

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub version: String,
    pub uptime_seconds: u64,
    pub kernel: String,
    pub timezone: String,
    pub ntp_synced: bool,
}

#[derive(Debug, Serialize)]
pub struct SystemHealth {
    pub status: String,
    pub services: Vec<ServiceStatus>,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub running: bool,
}

#[derive(Debug, Serialize)]
pub struct SystemStats {
    pub cpu: CpuStats,
    pub memory: MemoryStats,
    pub network: Vec<NetIfStats>,
    pub disk_io: Vec<DiskIoStats>,
}

#[derive(Debug, Serialize)]
pub struct DiskIoStats {
    pub name: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
    pub io_in_progress: u64,
}

#[derive(Debug, Serialize)]
pub struct CpuStats {
    pub count: u32,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
}

#[derive(Debug, Serialize)]
pub struct MemoryStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct NetIfStats {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub speed_mbps: Option<u32>,
    pub up: bool,
    pub addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DiskHealth {
    pub device: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub capacity_bytes: u64,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub health_passed: bool,
    pub smart_status: String,
    pub attributes: Vec<SmartAttribute>,
}

#[derive(Debug, Serialize)]
pub struct SmartAttribute {
    pub id: u32,
    pub name: String,
    pub value: u32,
    pub worst: u32,
    pub threshold: u32,
    pub raw_value: i64,
    pub failing: bool,
}

impl SystemService {
    pub fn new() -> Self {
        Self
    }

    pub async fn disks(&self) -> Vec<DiskHealth> {
        disk_health().await
    }

    pub async fn info(&self) -> SystemInfo {
        let hostname = hostname();
        let kernel = kernel_version();
        let uptime = uptime_seconds();
        let (timezone, ntp_synced) = timedatectl_info().await;

        SystemInfo {
            hostname,
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: uptime,
            kernel,
            timezone,
            ntp_synced,
        }
    }

    pub async fn health(&self) -> SystemHealth {
        // TODO: check actual service status via systemd D-Bus
        SystemHealth {
            status: "ok".to_string(),
            services: vec![
                ServiceStatus { name: "nasty-api".into(), running: true },
            ],
        }
    }

    pub async fn stats(&self) -> SystemStats {
        SystemStats {
            cpu: cpu_stats(),
            memory: memory_stats(),
            network: network_stats(),
            disk_io: disk_io_stats(),
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn kernel_version() -> String {
    std::fs::read_to_string("/proc/version")
        .map(|s| {
            s.split_whitespace()
                .nth(2)
                .unwrap_or("unknown")
                .to_string()
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

async fn timedatectl_info() -> (String, bool) {
    let output = tokio::process::Command::new("timedatectl")
        .args(["show", "--property=Timezone,NTPSynchronized"])
        .output()
        .await;

    let mut timezone = "UTC".to_string();
    let mut ntp_synced = false;

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(tz) = line.strip_prefix("Timezone=") {
                timezone = tz.trim().to_string();
            }
            if let Some(v) = line.strip_prefix("NTPSynchronized=") {
                ntp_synced = v.trim() == "yes";
            }
        }
    }

    (timezone, ntp_synced)
}

fn uptime_seconds() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(String::from))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

fn cpu_stats() -> CpuStats {
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

    CpuStats { count, load_1, load_5, load_15 }
}

fn memory_stats() -> MemoryStats {
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

fn interface_addresses() -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    let Ok(output) = std::process::Command::new("ip").args(["-j", "addr", "show"]).output() else {
        return map;
    };
    let Ok(json): Result<Vec<serde_json::Value>, _> = serde_json::from_slice(&output.stdout) else {
        return map;
    };
    for iface in json {
        let Some(name) = iface["ifname"].as_str() else { continue };
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

fn network_stats() -> Vec<NetIfStats> {
    let content = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let addr_map = interface_addresses();
    let mut interfaces = Vec::new();

    for line in content.lines().skip(2) {
        let line = line.trim();
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();

        // Skip loopback
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

        // Check if interface is up via operstate
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

// ── Disk I/O from /proc/diskstats ─────────────────────────────

fn disk_io_stats() -> Vec<DiskIoStats> {
    let content = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let mut results = Vec::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 14 {
            continue;
        }

        let name = parts[2];

        // Only include whole disks (sd*, nvme*n*, vd*), skip partitions
        let is_disk = (name.starts_with("sd") && name.len() == 3)
            || (name.starts_with("vd") && name.len() == 3)
            || (name.starts_with("nvme") && name.contains('n') && !name.contains('p'));

        if !is_disk {
            continue;
        }

        // /proc/diskstats fields (0-indexed from field 3):
        //  0: reads completed
        //  2: sectors read (each sector = 512 bytes)
        //  4: writes completed
        //  6: sectors written
        //  8: I/Os currently in progress
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

// ── SMART disk health via smartctl ────────────────────────────

/// Intermediate types for parsing smartctl --json output
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

/// List physical disks and query SMART data for each
async fn disk_health() -> Vec<DiskHealth> {
    // Find physical disk devices from lsblk
    let devices = match tokio::process::Command::new("lsblk")
        .args(["-dn", "-o", "NAME,TYPE"])
        .output()
        .await
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .filter_map(|line| {
                    let mut parts = line.split_whitespace();
                    let name = parts.next()?;
                    let dtype = parts.next()?;
                    if dtype == "disk" {
                        Some(format!("/dev/{name}"))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        }
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for dev in devices {
        if let Some(health) = query_smartctl(&dev).await {
            results.push(health);
        }
    }
    results
}

async fn query_smartctl(device: &str) -> Option<DiskHealth> {
    let output = tokio::process::Command::new("smartctl")
        .args(["-a", "--json=c", device])
        .output()
        .await
        .ok()?;

    // smartctl returns non-zero exit codes for various SMART states,
    // but still outputs valid JSON, so we parse regardless of exit code
    let json: SmartctlJson = serde_json::from_slice(&output.stdout).ok()?;

    let health_passed = json.smart_status.as_ref().map(|s| s.passed).unwrap_or(false);

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

    let smart_status = if health_passed {
        "PASSED".to_string()
    } else {
        "FAILED".to_string()
    };

    Some(DiskHealth {
        device: device.to_string(),
        model: json.model_name.unwrap_or_else(|| "Unknown".into()),
        serial: json.serial_number.unwrap_or_else(|| "Unknown".into()),
        firmware: json.firmware_version.unwrap_or_else(|| "Unknown".into()),
        capacity_bytes: json.user_capacity.map(|c| c.bytes).unwrap_or(0),
        temperature_c: json.temperature.and_then(|t| t.current),
        power_on_hours: json.power_on_time.and_then(|p| p.hours),
        health_passed,
        smart_status,
        attributes,
    })
}
