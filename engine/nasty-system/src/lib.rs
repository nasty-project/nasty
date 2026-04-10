pub mod alerts;
pub mod backup;
pub mod firmware;
pub mod network;
pub mod protocol;
pub mod settings;
pub mod tailscale;
pub mod update;

// Re-export metrics types from nasty-common so downstream code
// (nasty-engine, alerts) can still use `nasty_system::SystemStats` etc.
pub use nasty_common::metrics_types::*;

use schemars::JsonSchema;
use serde::Serialize;

use std::sync::Arc;
use tokio::sync::RwLock;

/// Cached values that only change on bcachefs switch or reboot.
#[derive(Clone)]
struct CachedInfo {
    bcachefs_version: String,
    bcachefs_commit: Option<String>,
    bcachefs_pinned_ref: Option<String>,
    debug_symbols: bool,
    /// Whether the RUNNING module is custom (version differs from default).
    bcachefs_is_custom_running: bool,
    /// Whether the RUNNING module has debug checks.
    bcachefs_debug_checks: bool,
}

pub struct SystemService {
    cached: Arc<RwLock<Option<CachedInfo>>>,
    engine_commit: Option<String>,
    engine_built: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SystemInfo {
    /// System hostname.
    pub hostname: String,
    /// NASty engine version string.
    pub version: String,
    /// Git commit the engine binary was compiled from.
    pub engine_commit: Option<String>,
    /// Build timestamp of the engine binary.
    pub engine_built: Option<String>,
    /// System uptime in seconds.
    pub uptime_seconds: u64,
    /// Running Linux kernel version string.
    pub kernel: String,
    /// Output of `bcachefs version` (first line).
    pub bcachefs_version: String,
    /// Short (12-char) commit SHA of the pinned bcachefs-tools in flake.lock
    pub bcachefs_commit: Option<String>,
    /// The ref currently pinned in `/etc/nixos/flake.lock` for `bcachefs-tools`.
    pub bcachefs_pinned_ref: Option<String>,
    /// True when the RUNNING bcachefs module version differs from the default.
    pub bcachefs_is_custom: bool,
    /// IANA timezone string (e.g. `America/New_York`).
    pub timezone: String,
    /// Whether the system clock is NTP-synchronized.
    pub ntp_synced: bool,
    /// Whether the loaded bcachefs kernel module contains debug symbols.
    pub bcachefs_debug_symbols: bool,
    /// Whether the RUNNING bcachefs module was built with debug checks.
    /// Only true when debug checks are configured AND the system has been rebooted into it.
    pub bcachefs_debug_checks: bool,
    /// Whether KVM hardware virtualization is available (/dev/kvm exists).
    pub kvm_available: bool,
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
    /// Display name (e.g. "Engine", "Metrics").
    pub name: String,
    /// Whether the service is currently active/running.
    pub running: bool,
    /// Resident memory usage in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// CPU time in seconds (user + system).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<f64>,
    /// Process uptime in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    /// Process ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

// SystemStats, CpuStats, MemoryStats, NetIfStats, DiskIoStats,
// DiskHealth, SmartAttribute — now defined in nasty_common::metrics_types
// and re-exported via `pub use` at the top of this file.

impl SystemService {
    pub fn new(engine_commit: Option<String>, engine_built: Option<String>) -> Self {
        Self { cached: Arc::new(RwLock::new(None)), engine_commit, engine_built }
    }

    /// Invalidate cached bcachefs info — call after rebuild or reboot.
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
        let (bcachefs_version, bcachefs_commit, (pinned_ref, _), debug_symbols, debug_checks, default_ref) = tokio::join!(
            bcachefs_version(),
            read_bcachefs_commit(),
            crate::update::read_flake_lock_bcachefs_pub(),
            bcachefs_has_debug_symbols(),
            bcachefs_has_debug_checks(),
            crate::update::read_flake_nix_default_ref_pub(),
        );
        // Running state: compare actual loaded module against default.
        // Strip leading 'v' from default ref for comparison (e.g. "v1.37.2" vs "1.37.2").
        let default_bare = default_ref.strip_prefix('v').unwrap_or(&default_ref);
        let bcachefs_is_custom_running = bcachefs_version != default_bare && bcachefs_version != "unknown";
        let info = CachedInfo {
            bcachefs_version,
            bcachefs_commit,
            bcachefs_pinned_ref: pinned_ref,
            debug_symbols,
            bcachefs_is_custom_running,
            bcachefs_debug_checks: debug_checks,
        };
        *self.cached.write().await = Some(info.clone());
        info
    }

    pub async fn info(&self) -> SystemInfo {
        let cached = self.get_cached_bcachefs().await;
        let (timezone, ntp_synced) = timedatectl_info().await;

        SystemInfo {
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            engine_commit: self.engine_commit.clone(),
            engine_built: self.engine_built.clone(),
            uptime_seconds: uptime_seconds(),
            kernel: kernel_version(),
            bcachefs_version: cached.bcachefs_version,
            bcachefs_commit: cached.bcachefs_commit,
            bcachefs_pinned_ref: cached.bcachefs_pinned_ref,
            bcachefs_is_custom: cached.bcachefs_is_custom_running,
            timezone,
            ntp_synced,
            bcachefs_debug_symbols: cached.debug_symbols,
            bcachefs_debug_checks: cached.bcachefs_debug_checks,
            kvm_available: std::path::Path::new("/dev/kvm").exists(),
        }
    }

    pub async fn health(&self) -> SystemHealth {
        let engine = self_service_status("Engine").await;
        let metrics = remote_service_status("Metrics", "nasty-metrics", "http://127.0.0.1:2138/health").await;

        let all_ok = engine.running && metrics.running;
        SystemHealth {
            status: if all_ok { "ok" } else { "degraded" }.to_string(),
            services: vec![engine, metrics],
        }
    }

}

fn hostname() -> String {
    // Read from kernel (set via /proc/sys/kernel/hostname), not /etc/hostname which is
    // read-only on NixOS and may be stale.
    std::fs::read_to_string("/proc/sys/kernel/hostname")
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
/// BCH_DEBUG_PARAMS_DEBUG() params (e.g. journal_seq_verify) are only compiled in
/// when CONFIG_BCACHEFS_DEBUG is set. We check /sys/module/ which reflects the actually
/// loaded module, not the .ko on disk (which may have been rebuilt already).
pub async fn bcachefs_has_debug_checks() -> bool {
    tokio::fs::metadata("/sys/module/bcachefs/parameters/journal_seq_verify")
        .await
        .is_ok()
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

// ── Service health helpers ─────────────────────────────────────

/// Build ServiceStatus for the current process (nasty-engine).
async fn self_service_status(name: &str) -> ServiceStatus {
    let pid = std::process::id();
    let (memory_bytes, cpu_seconds, uptime_secs) = read_proc_stats(pid).await;

    ServiceStatus {
        name: name.to_string(),
        running: true,
        memory_bytes: Some(memory_bytes),
        cpu_seconds: Some(cpu_seconds),
        uptime_seconds: Some(uptime_secs),
        pid: Some(pid),
    }
}

/// Build ServiceStatus for a remote service by checking its health endpoint
/// and looking up its systemd unit for PID/resource info.
async fn remote_service_status(name: &str, unit: &str, health_url: &str) -> ServiceStatus {
    // Check if the service responds
    let running = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()
        .map(|c| c.get(health_url).send())
        .is_some()
        && reqwest::Client::new()
            .get(health_url)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

    let (memory_bytes, cpu_seconds, uptime_secs, pid) = if running {
        if let Some(p) = systemd_main_pid(unit).await {
            let (mem, cpu, up) = read_proc_stats(p).await;
            (Some(mem), Some(cpu), Some(up), Some(p))
        } else {
            (None, None, None, None)
        }
    } else {
        (None, None, None, None)
    };

    ServiceStatus {
        name: name.to_string(),
        running,
        memory_bytes,
        cpu_seconds,
        uptime_seconds: uptime_secs,
        pid,
    }
}

/// Get the MainPID of a systemd unit.
async fn systemd_main_pid(unit: &str) -> Option<u32> {
    let output = tokio::process::Command::new("systemctl")
        .args(["show", &format!("{unit}.service"), "--property=MainPID", "--value"])
        .output()
        .await
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    if pid > 0 { Some(pid) } else { None }
}

/// Read RSS memory, CPU time, and process uptime from /proc/<pid>.
async fn read_proc_stats(pid: u32) -> (u64, f64, u64) {
    let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
        .await
        .unwrap_or_default();
    let status = tokio::fs::read_to_string(format!("/proc/{pid}/status"))
        .await
        .unwrap_or_default();

    // RSS from /proc/pid/status (VmRSS line, in kB)
    let memory_bytes = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024;

    // CPU time from /proc/pid/stat: fields 14 (utime) + 15 (stime) in clock ticks
    // Process start time: field 22 (starttime) in clock ticks since boot
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let cpu_seconds = if fields.len() > 14 {
        let utime: u64 = fields[13].parse().unwrap_or(0);
        let stime: u64 = fields[14].parse().unwrap_or(0);
        (utime + stime) as f64 / ticks_per_sec
    } else {
        0.0
    };

    let uptime_secs = if fields.len() > 21 {
        let starttime: u64 = fields[21].parse().unwrap_or(0);
        let system_uptime = uptime_seconds();
        let proc_start_secs = starttime as f64 / ticks_per_sec;
        system_uptime.saturating_sub(proc_start_secs as u64)
    } else {
        0
    };

    (memory_bytes, cpu_seconds, uptime_secs)
}

/// Read the pinned bcachefs-tools commit SHA from flake.lock (12-char short form).
async fn read_bcachefs_commit() -> Option<String> {
    let content = tokio::fs::read_to_string("/etc/nixos/flake.lock").await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let rev = v["nodes"]["bcachefs-tools"]["locked"]["rev"].as_str()?;
    Some(rev[..rev.len().min(12)].to_string())
}
