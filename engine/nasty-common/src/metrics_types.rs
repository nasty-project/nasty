//! Shared types for system and storage metrics.
//!
//! These types are produced by `nasty-metrics` and consumed by `nasty-engine`
//! (via HTTP) and the WebUI (via JSON-RPC). Both `Serialize` and `Deserialize`
//! are derived so the engine can round-trip them over HTTP.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── System stats ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

// ── Disk health (SMART) ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiskHealth {
    /// Block device path (e.g. `/dev/sda`).
    pub device: String,
    /// ATA/SATA port identifier (e.g. `ata5`), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ata_port: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

// ── Kernel errors ──────────────────────────────────────────────

/// A suspicious kernel message detected in the ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct KernelError {
    /// Timestamp in microseconds from boot.
    pub timestamp_usec: u64,
    /// The raw kernel message text.
    pub message: String,
    /// Category of error: `sata`, `nvme`, `filesystem`, `memory`, `generic`.
    pub category: String,
    /// Source device or subsystem if identifiable (e.g. `ata5`, `nvme0`).
    pub source: String,
}

/// Summary of kernel errors since boot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct KernelErrorSummary {
    /// Total suspicious kernel messages since boot.
    pub total_count: u64,
    /// Per-category error counts.
    pub by_category: Vec<CategoryCount>,
    /// Most recent errors (capped at 50).
    pub recent_errors: Vec<KernelError>,
}

/// Error count for a single category.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CategoryCount {
    /// Category name.
    pub category: String,
    /// Number of errors in this category.
    pub count: u64,
}

// ── Time-series (metrics history) ──────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct IoSample {
    /// Unix epoch milliseconds.
    pub ts: i64,
    pub in_rate: f64,
    pub out_rate: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResourceHistory {
    pub name: String,
    pub samples: Vec<IoSample>,
}
