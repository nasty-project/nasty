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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CpuStats {
    /// Number of logical CPU cores.
    pub count: u32,
    /// 1-minute load average.
    pub load_1: f64,
    /// 5-minute load average.
    pub load_5: f64,
    /// 15-minute load average.
    pub load_15: f64,
    /// CPU package temperature in degrees Celsius (from hwmon).
    pub temp_c: Option<i32>,
    /// Average current CPU frequency across all cores in MHz.
    pub freq_mhz: Option<u32>,
    /// CPU frequency scaling governor (e.g. "powersave", "performance").
    pub governor: Option<String>,
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
    /// smartctl transport flag used to reach this drive, e.g.
    /// `megaraid,0`, `sat+megaraid,2`, `areca,3`. `None` for drives
    /// reachable via smartctl's default transport (direct-attach
    /// SATA/NVMe). The pair `(device, transport)` uniquely identifies a
    /// physical drive — multiple drives behind a RAID controller share
    /// the same block device path but have distinct transport flags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// ATA/SATA port identifier (e.g. `ata5`), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ata_port: Option<String>,
    /// PCI address of the SATA/NVMe controller (e.g. `03:00.0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller_pci: Option<String>,
    /// Human-readable controller name (e.g. `ASMedia ASM1166`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller_name: Option<String>,
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
    /// ATA SMART attribute table (empty for NVMe and SAS drives).
    pub attributes: Vec<SmartAttribute>,
    /// NVMe SMART health information log (`Some` only on NVMe drives).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvme: Option<NvmeHealth>,
    /// SCSI / SAS health information (`Some` only on SAS / SCSI drives,
    /// including SAS drives reached via `-d megaraid,N`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scsi: Option<ScsiHealth>,
}

/// NVMe SMART health information, parsed from smartctl's
/// `nvme_smart_health_information_log` block. Fields preserve the NVMe
/// spec / smartctl names so operators familiar with `smartctl -a` see
/// the same identifiers in the UI.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NvmeHealth {
    /// Critical-warning bit field. `0` is healthy; non-zero bits flag
    /// spare-below-threshold (0x1), temperature (0x2), reliability (0x4),
    /// read-only (0x8), volatile-backup-failed (0x10), persistent-memory-
    /// region-RO (0x20).
    pub critical_warning: u8,
    /// Remaining spare blocks as a percentage of the initial reserve.
    /// Decreases as the drive remaps failed NAND cells.
    pub available_spare_percent: u8,
    /// Vendor-set threshold (typically 10%, sometimes higher) below which
    /// `available_spare_percent` triggers the spare-low critical warning.
    pub available_spare_threshold_percent: u8,
    /// Endurance estimate: 0 = new, 100 = nominal end of life. May exceed
    /// 100 on drives operated past their rated DWPD. Not a hard limit.
    pub percentage_used: u32,
    /// Read volume reported in NVMe "data units" (1 unit = 1000 × 512-byte
    /// LBAs = 512,000 bytes per spec). UI multiplies for human-readable
    /// totals.
    pub data_units_read: u64,
    /// Write volume in NVMe data units (see `data_units_read`).
    pub data_units_written: u64,
    /// Total host read commands issued to the controller.
    pub host_reads: u64,
    /// Total host write commands issued to the controller.
    pub host_writes: u64,
    /// Controller busy time in minutes.
    pub controller_busy_minutes: u64,
    /// Number of power cycles.
    pub power_cycles: u64,
    /// Number of unclean shutdowns (drive lost power without a graceful
    /// shutdown notify).
    pub unsafe_shutdowns: u64,
    /// Media and data integrity errors detected by the controller.
    pub media_errors: u64,
    /// Number of entries in the controller error information log.
    pub num_err_log_entries: u64,
    /// Human-readable status string of the most recent entry in the
    /// error information log table, when smartctl returned one. The
    /// table itself is only emitted by smartctl 7.4+; older versions
    /// give just the count above with no way to see what the errors
    /// actually were. `None` when the log is empty or unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub most_recent_error: Option<String>,
    /// Cumulative minutes the controller spent above the warning
    /// temperature threshold.
    pub warning_temp_minutes: u64,
    /// Cumulative minutes the controller spent above the critical
    /// temperature threshold.
    pub critical_comp_minutes: u64,
    /// Per-zone temperatures in degrees Celsius. Some drives only wire up
    /// a subset of sensors and report `null` for the rest (e.g. Kingston
    /// SNV3S reports `[null, 43]`).
    pub temperature_sensors_c: Vec<Option<i32>>,
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

/// SCSI / SAS health information, populated from the `scsi_*` family of
/// top-level fields smartctl emits when talking to a SAS or SCSI drive.
/// Field names trace back to the SCSI Primary Commands (SPC) standard
/// and the SCSI Block Commands (SBC) Log Page 2 / 3 / 6 definitions, so
/// operators reading vendor documentation find the same identifiers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScsiHealth {
    /// Transport protocol description (e.g. `"SAS (SPL-4)"`,
    /// `"SAS (SPL-3)"`, `"Fibre Channel"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_protocol: Option<String>,
    /// SCSI standard version (e.g. `"SPC-3"`, `"SPC-4"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scsi_version: Option<String>,
    /// Rotation rate in RPM. `0` = SSD; typical SAS spinner values are
    /// 7200, 10500 / 10033, 15000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation_rate: Option<u32>,
    /// Drive form factor as smartctl reports it (e.g. `"3.5 inches"`,
    /// `"2.5 inches"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form_factor: Option<String>,
    /// World-Wide Name / Logical Unit Identifier (hex string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_unit_id: Option<String>,
    /// Drive-trip temperature in °C — the controller's hard shutdown
    /// threshold. Useful context next to `temperature_c` so operators
    /// see how much headroom they have before the drive bails out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_trip_temp_c: Option<i32>,
    /// Year of manufacture (e.g. `"2019"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year_of_manufacture: Option<String>,
    /// Week of manufacture within that year (`"01"` – `"52"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub week_of_manufacture: Option<String>,
    /// Number of sectors moved to spare blocks since manufacture
    /// (SCSI Log Page 3 — Read Defect Data, grown defect list count).
    /// Non-zero means the drive has had to remap failing sectors; the
    /// rate of growth matters more than the absolute number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grown_defect_list: Option<u64>,
    /// Accumulated power-on minutes since the last format. Distinct
    /// from `power_on_hours` which counts since manufacture. The gap
    /// between the two shows pre-deployment burn-in time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_on_minutes_since_format: Option<u64>,
    /// Start/stop cycles accumulated vs the drive's design lifetime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_stop_cycles: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_stop_cycles_designed: Option<u64>,
    /// Load/unload cycles accumulated vs the drive's design lifetime.
    /// SAS drives self-park heads on idle so this typically grows much
    /// faster than start/stop cycles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_unload_cycles: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_unload_cycles_designed: Option<u64>,
    /// Per-I/O-type error counts from SCSI Log Page 2/3/5.
    pub read_errors: ScsiErrorCounters,
    pub write_errors: ScsiErrorCounters,
    pub verify_errors: ScsiErrorCounters,
    /// Most recent entry from the rolling SCSI self-test log (Log Page
    /// 0x10). `None` when no tests have been recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_self_test: Option<ScsiSelfTestEntry>,
    /// Number of completed self-test entries in the rolling log
    /// (smartctl numbers them `scsi_self_test_0` ‥ `scsi_self_test_19`).
    pub self_test_count: u32,
}

/// SCSI per-I/O-type error counters drawn from Log Page 0x02 (Write),
/// 0x03 (Read), and 0x05 (Verify). The `gigabytes_processed` field
/// gives the denominator so operators can reason about error *rates*
/// rather than raw counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScsiErrorCounters {
    /// Errors the drive recovered from automatically (ECC, rereads,
    /// rewrites). Informational — large values are normal on long-lived
    /// drives and don't indicate failure.
    pub corrected_total: u64,
    /// **Uncorrected errors are the failure signal.** Any non-zero
    /// value on the write or verify counter means the drive has lost
    /// or returned bad data. Even small counts warrant replacement
    /// planning — they don't decrease, and the rate tends to accelerate.
    pub uncorrected_total: u64,
    /// I/O volume in gigabytes processed since the counter was last
    /// reset (typically since drive format). Lets the UI show error
    /// rates as "N errors per TB" instead of raw counts.
    pub gigabytes_processed: f64,
}

/// One entry from the SCSI Self-Test rolling log (smartctl numbers them
/// `scsi_self_test_0` … `scsi_self_test_19`). We surface the most recent
/// only; deeper history is one `smartctl -a` away.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScsiSelfTestEntry {
    /// Test type — e.g. `"Background long"`, `"Background short"`,
    /// `"Foreground long"`.
    pub code: String,
    /// Result string — e.g. `"Completed"`, `"Aborted (device reset ?)"`,
    /// `"Self test in progress ..."`, `"Read element of test failed"`.
    pub result: String,
    /// Whether this entry represents a healthy outcome. True when the
    /// drive reported the test as successfully completed; false when
    /// it aborted, failed, or is still in progress.
    pub passed: bool,
    /// Drive's accumulated power-on hours when the test ran. Lets the
    /// UI render "X hours ago" relative to the drive's current
    /// `power_on_hours`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_on_hours: Option<u64>,
    /// True if smartctl reports a self-test is currently running. Only
    /// ever set on the most-recent entry.
    pub in_progress: bool,
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
