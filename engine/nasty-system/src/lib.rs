pub mod alerts;
pub mod metrics;
pub mod network;
pub mod protocol;
pub mod settings;
pub mod update;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use tokio::sync::RwLock;

/// Cached values that only change on bcachefs switch or reboot.
#[derive(Clone)]
struct CachedInfo {
    bcachefs_version: String,
    bcachefs_commit: Option<String>,
    bcachefs_pinned_ref: Option<String>,
    bcachefs_is_custom: bool,
    debug_symbols: bool,
    debug_checks: bool,
}

pub struct SystemService {
    cached: Arc<RwLock<Option<CachedInfo>>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SystemInfo {
    /// System hostname.
    pub hostname: String,
    /// NASty engine version string.
    pub version: String,
    /// System uptime in seconds.
    pub uptime_seconds: u64,
    /// Running Linux kernel version string.
    pub kernel: String,
    /// Output of `bcachefs version` (first line).
    pub bcachefs_version: String,
    /// Short (12-char) commit SHA of the pinned bcachefs-tools in flake.lock
    pub bcachefs_commit: Option<String>,
    /// The ref stored in the state file: tag name (e.g. "v1.37.1") or short SHA
    pub bcachefs_pinned_ref: Option<String>,
    /// True when the user has overridden the default bcachefs-tools version
    pub bcachefs_is_custom: bool,
    /// IANA timezone string (e.g. `America/New_York`).
    pub timezone: String,
    /// Whether the system clock is NTP-synchronized.
    pub ntp_synced: bool,
    /// Whether the loaded bcachefs kernel module contains debug symbols.
    pub bcachefs_debug_symbols: bool,
    /// Whether the loaded bcachefs kernel module was built with CONFIG_BCACHEFS_DEBUG.
    pub bcachefs_debug_checks: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SystemHealth {
    /// Overall health status string (e.g. `ok`, `degraded`).
    pub status: String,
    /// Status of individual systemd services.
    pub services: Vec<ServiceStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ServiceStatus {
    /// systemd service name.
    pub name: String,
    /// Whether the service is currently active/running.
    pub running: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SystemStats {
    /// CPU core count and load averages.
    pub cpu: CpuStats,
    /// Memory and swap usage.
    pub memory: MemoryStats,
    /// Per-interface network statistics.
    pub network: Vec<NetIfStats>,
    /// Per-disk I/O statistics.
    pub disk_io: Vec<DiskIoStats>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiskIoStats {
    /// Kernel device name (e.g. `sda`, `nvme0n1`).
    pub name: String,
    /// Cumulative bytes read since boot (from `/proc/diskstats`).
    pub read_bytes: u64,
    /// Cumulative bytes written since boot.
    pub write_bytes: u64,
    /// Cumulative read I/O operations completed since boot.
    pub read_ios: u64,
    /// Cumulative write I/O operations completed since boot.
    pub write_ios: u64,
    /// Number of I/O operations currently in progress.
    pub io_in_progress: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CpuStats {
    /// Number of logical CPU cores.
    pub count: u32,
    /// 1-minute load average.
    pub load_1: f64,
    /// 5-minute load average.
    pub load_5: f64,
    /// 15-minute load average.
    pub load_15: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MemoryStats {
    /// Total installed RAM in bytes.
    pub total_bytes: u64,
    /// RAM currently in use (total minus available).
    pub used_bytes: u64,
    /// RAM available for allocation without swapping.
    pub available_bytes: u64,
    /// Total swap space in bytes.
    pub swap_total_bytes: u64,
    /// Swap space currently in use.
    pub swap_used_bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NetIfStats {
    /// Network interface name (e.g. `eth0`, `ens3`).
    pub name: String,
    /// Cumulative bytes received since boot.
    pub rx_bytes: u64,
    /// Cumulative bytes transmitted since boot.
    pub tx_bytes: u64,
    /// Cumulative packets received since boot.
    pub rx_packets: u64,
    /// Cumulative packets transmitted since boot.
    pub tx_packets: u64,
    /// Link speed in Mbit/s (None if unavailable, e.g. virtual interfaces).
    pub speed_mbps: Option<u32>,
    /// Whether the interface's operstate is `up`.
    pub up: bool,
    /// IPv4 and IPv6 addresses in CIDR notation (e.g. `192.168.1.10/24`).
    pub addresses: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiskHealth {
    /// Block device path (e.g. `/dev/sda`).
    pub device: String,
    /// Drive model name reported by SMART.
    pub model: String,
    /// Drive serial number.
    pub serial: String,
    /// Drive firmware version string.
    pub firmware: String,
    /// Total drive capacity in bytes.
    pub capacity_bytes: u64,
    /// Current drive temperature in degrees Celsius.
    pub temperature_c: Option<i32>,
    /// Accumulated powered-on time in hours.
    pub power_on_hours: Option<u64>,
    /// Whether the SMART overall-health self-assessment test passed.
    pub health_passed: bool,
    /// Human-readable SMART health status (`PASSED` or `FAILED`).
    pub smart_status: String,
    /// ATA SMART attribute table (may be empty for NVMe drives).
    pub attributes: Vec<SmartAttribute>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SmartAttribute {
    /// ATA attribute ID (1–255).
    pub id: u32,
    /// Attribute name (e.g. `Raw_Read_Error_Rate`).
    pub name: String,
    /// Normalized current value (higher is better for most attributes).
    pub value: u32,
    /// Worst normalized value ever recorded.
    pub worst: u32,
    /// Failure threshold; attribute is failing when value drops below this.
    pub threshold: u32,
    /// Raw (vendor-specific) attribute value.
    pub raw_value: i64,
    /// Whether this attribute is currently at or below its failure threshold.
    pub failing: bool,
}

impl SystemService {
    pub fn new() -> Self {
        Self { cached: Arc::new(RwLock::new(None)) }
    }

    /// Invalidate cached bcachefs info — call after bcachefs switch or reboot.
    pub async fn invalidate_bcachefs_cache(&self) {
        *self.cached.write().await = None;
    }

    async fn get_cached_bcachefs(&self) -> CachedInfo {
        {
            let guard = self.cached.read().await;
            if let Some(ref c) = *guard {
                return c.clone();
            }
        }
        // Compute — run subprocess calls in parallel.
        let (bcachefs_version, bcachefs_commit, pinned_ref_raw, debug_symbols, debug_checks) = tokio::join!(
            bcachefs_version(),
            read_bcachefs_commit(),
            async {
                tokio::fs::read_to_string("/var/lib/nasty/bcachefs-tools-ref").await
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            },
            bcachefs_has_debug_symbols(),
            bcachefs_has_debug_checks(),
        );
        let bcachefs_is_custom = pinned_ref_raw.is_some();
        let info = CachedInfo {
            bcachefs_version,
            bcachefs_commit,
            bcachefs_pinned_ref: pinned_ref_raw,
            bcachefs_is_custom,
            debug_symbols,
            debug_checks,
        };
        *self.cached.write().await = Some(info.clone());
        info
    }

    pub async fn disks(&self) -> Vec<DiskHealth> {
        disk_health().await
    }

    pub async fn info(&self) -> SystemInfo {
        let cached = self.get_cached_bcachefs().await;
        let (timezone, ntp_synced) = timedatectl_info().await;

        SystemInfo {
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: uptime_seconds(),
            kernel: kernel_version(),
            bcachefs_version: cached.bcachefs_version,
            bcachefs_commit: cached.bcachefs_commit,
            bcachefs_pinned_ref: cached.bcachefs_pinned_ref,
            bcachefs_is_custom: cached.bcachefs_is_custom,
            timezone,
            ntp_synced,
            bcachefs_debug_symbols: cached.debug_symbols,
            bcachefs_debug_checks: cached.debug_checks,
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

async fn bcachefs_version() -> String {
    // Read the version of the currently loaded kernel module — this is the authoritative
    // running version. bcachefs version (userspace) can differ when a reboot is pending.
    let output = tokio::process::Command::new("modinfo")
        .args(["bcachefs", "--field", "version"])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if v.is_empty() { "unknown".to_string() } else { v }
        }
        _ => "unknown".to_string(),
    }
}

/// Detect whether the loaded bcachefs kernel module contains debug symbols.
/// Decompresses the .ko.xz and pipes through `file` looking for "debug_info".
pub async fn bcachefs_has_debug_symbols() -> bool {
    // Get the module file path from modinfo
    let filename_out = tokio::process::Command::new("modinfo")
        .args(["bcachefs", "--field", "filename"])
        .output()
        .await;
    let ko_path = match filename_out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return false,
    };
    if ko_path.is_empty() {
        return false;
    }
    // xz -dc <file> | file - → look for "debug_info"
    let xz = tokio::process::Command::new("sh")
        .args(["-c", &format!("xz -dc '{}' | file -", ko_path)])
        .output()
        .await;
    match xz {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("debug_info"),
        Err(_) => false,
    }
}

/// Detect whether the loaded bcachefs kernel module was built with CONFIG_BCACHEFS_DEBUG.
/// When enabled, bcachefs exposes debug_check_* module parameters.
/// Check if debug checks are enabled by reading the state file.
/// The debug_check_* module parameters in modinfo are always present
/// (static_key_t) regardless of CONFIG_BCACHEFS_DEBUG, so we cannot
/// detect it from the loaded module.
pub async fn bcachefs_has_debug_checks() -> bool {
    tokio::fs::metadata("/var/lib/nasty/bcachefs-debug-checks").await.is_ok()
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

    // NTPSynchronized=yes only flips when timesyncd itself adjusts the clock.
    // On VMs the hypervisor pre-sets the clock so timesyncd never needs to step it,
    // leaving the flag as "no" even though the service is healthy and polling.
    // Fall back to checking whether timesyncd is actively running.
    if !ntp_synced {
        ntp_synced = systemd_unit_active("systemd-timesyncd").await;
    }

    (timezone, ntp_synced)
}

async fn systemd_unit_active(unit: &str) -> bool {
    tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Read the pinned bcachefs-tools commit SHA from flake.lock (12-char short form).
async fn read_bcachefs_commit() -> Option<String> {
    let content = tokio::fs::read_to_string("/etc/nixos/nixos/flake.lock").await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let rev = v["nodes"]["bcachefs-tools"]["locked"]["rev"].as_str()?;
    Some(rev[..rev.len().min(12)].to_string())
}
