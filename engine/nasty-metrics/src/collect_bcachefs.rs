//! bcachefs metrics collection from sysfs and CLI.
//!
//! Discovers mounted bcachefs filesystems from `/proc/mounts`, reads counters,
//! time stats, per-device stats, space usage, and background op status from
//! `/sys/fs/bcachefs/<uuid>/`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A discovered bcachefs filesystem.
#[derive(Debug, Clone)]
pub struct BcachefsFs {
    pub uuid: String,
    pub mount_point: String,
    /// Human-readable name (basename of mount_point, e.g. "tank").
    pub fs_name: String,
    pub sysfs_path: PathBuf,
}

/// All metrics for one bcachefs filesystem.
#[derive(Debug, Default, Serialize)]
pub struct BcachefsMetrics {
    pub uuid: String,
    pub fs_name: String,

    /// Persistent counters (since mount).
    pub counters: HashMap<String, u64>,

    /// Time stats with min/max/mean/stddev (from time_stats_json/).
    pub time_stats: HashMap<String, TimeStatEntry>,

    /// Per-device metrics.
    pub devices: Vec<DeviceMetrics>,

    /// Space usage from statvfs.
    pub space: SpaceUsage,

    /// Filesystem options from sysfs.
    pub options: HashMap<String, String>,

    /// Background op status.
    pub background: BackgroundOps,

    /// Compression stats.
    pub compression: Vec<CompressionEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TimeStatEntry {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub mean_ns: u64,
    #[serde(default)]
    pub min_ns: u64,
    #[serde(default)]
    pub max_ns: u64,
    #[serde(default)]
    pub stddev_ns: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct DeviceMetrics {
    pub index: u32,
    pub name: String,
    pub label: Option<String>,
    /// Current read latency in nanoseconds.
    pub io_latency_read_ns: u64,
    /// Current write latency in nanoseconds.
    pub io_latency_write_ns: u64,
    /// Bytes done per data type per direction.
    pub io_done: Option<serde_json::Value>,
    /// IO error summary.
    pub io_errors: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct SpaceUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct BackgroundOps {
    pub copy_gc_wait: Option<String>,
    pub reconcile_status: Option<String>,
    pub journal_debug: Option<String>,
    pub btree_cache_size_bytes: Option<u64>,
    pub btree_write_buffer: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct CompressionEntry {
    pub algorithm: String,
    pub compressed_bytes: u64,
    pub uncompressed_bytes: u64,
}

// ── Discovery ───────────────────────────────────────────────────

/// Discover mounted bcachefs filesystems from /proc/mounts.
pub fn discover_filesystems() -> Vec<BcachefsFs> {
    let content = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut result = Vec::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        if parts[2] != "bcachefs" {
            continue;
        }

        let mount_point = parts[1].to_string();
        let fs_name = Path::new(&mount_point)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Find UUID from sysfs — scan /sys/fs/bcachefs/ for a UUID whose
        // mount matches, or extract from the device field.
        if let Some(uuid) = find_uuid_for_mount(&mount_point) {
            let sysfs_path = PathBuf::from(format!("/sys/fs/bcachefs/{uuid}"));
            result.push(BcachefsFs {
                uuid,
                mount_point,
                fs_name,
                sysfs_path,
            });
        }
    }

    result
}

fn find_uuid_for_mount(mount_point: &str) -> Option<String> {
    // Iterate /sys/fs/bcachefs/ directories (each is a UUID)
    let dir = std::fs::read_dir("/sys/fs/bcachefs/").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Verify this UUID matches by checking if any of its devices
        // are part of the mount. A simpler heuristic: check statvfs on
        // the mount and compare with the sysfs UUID. For now, we use
        // /proc/mounts device field which for bcachefs is "UUID=..." or
        // device paths. We'll just return the first UUID that exists.

        // Actually: match via /proc/mounts — re-read and find which device
        // maps to this UUID. Simplest approach: read the fs UUID from the
        // mounted filesystem's xattr or just use the sysfs UUID directly
        // if the mount_point's st_dev matches.

        // Pragmatic approach: read /sys/fs/bcachefs/<uuid>/ and check
        // if a dev-* subdir's block device is part of the mount source.
        // But the simplest: use statfs f_fsid or just iterate and match
        // by checking the mount's device string.

        // Simplest reliable approach: read /proc/self/mountinfo which
        // has the filesystem UUID for bcachefs.
        let _ = name; // suppress unused warning
    }

    // Fallback: parse /proc/self/mountinfo for the super_options which
    // contain the UUID for bcachefs mounts.
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in mountinfo.lines() {
        if !line.contains(mount_point) || !line.contains("bcachefs") {
            continue;
        }
        // The mount source for bcachefs contains the UUID after the
        // separator "- bcachefs <devices> ..." but the UUID is typically
        // the directory name under /sys/fs/bcachefs/. Extract it by
        // looking for a UUID pattern in the line or by checking which
        // sysfs UUID directory has dev-* entries matching the mount.

        // Best approach: after "- bcachefs" in mountinfo, the next
        // field is the device(s). We can cross-reference with sysfs.
        // But actually, the easiest: scan /sys/fs/bcachefs/ and for each
        // UUID, read `dev-0/block` symlink target and check if it appears
        // in the mount source.
        if let Some(idx) = line.find("- bcachefs") {
            let rest = &line[idx..];
            // rest = "- bcachefs /dev/sda,/dev/sdb rw,..."
            let fields: Vec<&str> = rest.split_whitespace().collect();
            if fields.len() >= 3 {
                let devices_str = fields[2];
                let first_dev = devices_str.split(':').next().unwrap_or(devices_str);

                // Now find which sysfs UUID has this device
                if let Ok(sysfs_dir) = std::fs::read_dir("/sys/fs/bcachefs/") {
                    for entry in sysfs_dir.flatten() {
                        let uuid = entry.file_name().to_string_lossy().into_owned();
                        let sysfs = entry.path();

                        // Check dev-* subdirs for block symlink matching
                        if let Ok(dev_dir) = std::fs::read_dir(&sysfs) {
                            for dev_entry in dev_dir.flatten() {
                                let dev_name = dev_entry.file_name();
                                if !dev_name.to_string_lossy().starts_with("dev-") {
                                    continue;
                                }
                                let block_link = dev_entry.path().join("block");
                                if let Ok(target) = std::fs::read_link(&block_link) {
                                    let block_name = target
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default();
                                    if first_dev.contains(&block_name)
                                        || first_dev.ends_with(&block_name)
                                    {
                                        return Some(uuid);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Last resort: if there's only one UUID in sysfs, use it.
    if let Ok(dir) = std::fs::read_dir("/sys/fs/bcachefs/") {
        let entries: Vec<_> = dir.flatten().collect();
        if entries.len() == 1 {
            return Some(entries[0].file_name().to_string_lossy().into_owned());
        }
    }

    None
}

// ── Counters ────────────────────────────────────────────────────

/// Read all persistent counters from `/sys/fs/bcachefs/<uuid>/counters/`.
/// Returns a map of counter_name → value (since mount).
pub fn read_counters(sysfs: &Path) -> HashMap<String, u64> {
    let mut counters = HashMap::new();
    let counters_dir = sysfs.join("counters");

    let Ok(dir) = std::fs::read_dir(&counters_dir) else {
        return counters;
    };

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };

        // Format:
        //   since mount:	12345
        //   since filesystem creation:	67890
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("since mount:") {
                if let Ok(val) = rest.trim().parse::<u64>() {
                    counters.insert(name.clone(), val);
                }
                break;
            }
        }
    }

    counters
}

// ── Time stats ──────────────────────────────────────────────────

/// Read time stats from `/sys/fs/bcachefs/<uuid>/time_stats_json/`.
pub fn read_time_stats(sysfs: &Path) -> HashMap<String, TimeStatEntry> {
    let mut stats = HashMap::new();
    let dir_path = sysfs.join("time_stats_json");

    let Ok(dir) = std::fs::read_dir(&dir_path) else {
        return stats;
    };

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };

        // The JSON format from bcachefs contains nested stats.
        // Parse the top-level object and extract the fields we need.
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let entry_val = TimeStatEntry {
                count: json["count"].as_u64().unwrap_or(0),
                mean_ns: json["mean_ns"].as_u64().unwrap_or(0),
                min_ns: json["min_ns"].as_u64().unwrap_or(0),
                max_ns: json["max_ns"].as_u64().unwrap_or(0),
                stddev_ns: json["stddev_ns"].as_u64().unwrap_or(0),
            };
            stats.insert(name, entry_val);
        }
    }

    stats
}

// ── Per-device stats ────────────────────────────────────────────

/// Read per-device metrics from `/sys/fs/bcachefs/<uuid>/dev-*/`.
pub fn read_device_stats(sysfs: &Path) -> Vec<DeviceMetrics> {
    let mut devices = Vec::new();

    let Ok(dir) = std::fs::read_dir(sysfs) else {
        return devices;
    };

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(idx_str) = name.strip_prefix("dev-") else {
            continue;
        };
        let Ok(index) = idx_str.parse::<u32>() else {
            continue;
        };

        let dev_path = entry.path();

        // Block device name from symlink
        let block_name = std::fs::read_link(dev_path.join("block"))
            .ok()
            .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| name.clone());

        let label = read_sysfs_str(&dev_path.join("label"));

        let io_latency_read_ns = read_sysfs_u64(&dev_path.join("io_latency_read"));
        let io_latency_write_ns = read_sysfs_u64(&dev_path.join("io_latency_write"));

        // io_done is JSON
        let io_done = std::fs::read_to_string(dev_path.join("io_done"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        let io_errors = read_sysfs_str(&dev_path.join("io_errors"));

        devices.push(DeviceMetrics {
            index,
            name: block_name,
            label,
            io_latency_read_ns,
            io_latency_write_ns,
            io_done,
            io_errors,
        });
    }

    devices.sort_by_key(|d| d.index);
    devices
}

// ── Space usage ─────────────────────────────────────────────────

/// Read space usage via statvfs.
pub fn read_space(mount_point: &str) -> SpaceUsage {
    use std::ffi::CString;

    let Ok(path) = CString::new(mount_point) else {
        return SpaceUsage::default();
    };

    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) == 0 {
            let block_size = stat.f_frsize as u64;
            let total = stat.f_blocks * block_size;
            let available = stat.f_bavail * block_size;
            let used = total.saturating_sub(stat.f_bfree * block_size);
            SpaceUsage {
                total_bytes: total,
                used_bytes: used,
                available_bytes: available,
            }
        } else {
            SpaceUsage::default()
        }
    }
}

// ── Filesystem options ────────────────────────────────────────────────

/// Read filesystem options from `/sys/fs/bcachefs/<uuid>/options/`.
pub fn read_fs_options(sysfs: &Path) -> HashMap<String, String> {
    let mut options = HashMap::new();
    let opts_dir = sysfs.join("options");

    let Ok(dir) = std::fs::read_dir(&opts_dir) else {
        return options;
    };

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(val) = read_sysfs_str(&entry.path()) {
            options.insert(name, val);
        }
    }

    options
}

// ── Background ops ──────────────────────────────────────────────

/// Read background operation status from sysfs.
pub fn read_background_ops(sysfs: &Path) -> BackgroundOps {
    let internal = sysfs.join("internal");

    let btree_cache_size_bytes = read_sysfs_str(&sysfs.join("btree_cache_size"))
        .and_then(|s| parse_human_bytes(&s));

    BackgroundOps {
        copy_gc_wait: read_sysfs_str(&internal.join("copy_gc_wait")),
        reconcile_status: read_sysfs_str(&sysfs.join("reconcile_status")),
        journal_debug: read_sysfs_str(&internal.join("journal_debug")),
        btree_cache_size_bytes,
        btree_write_buffer: read_sysfs_str(&internal.join("btree_write_buffer")),
    }
}

// ── Compression stats ───────────────────────────────────────────

/// Parse compression_stats from sysfs.
pub fn read_compression_stats(sysfs: &Path) -> Vec<CompressionEntry> {
    let mut entries = Vec::new();
    let Ok(content) = std::fs::read_to_string(sysfs.join("compression_stats")) else {
        return entries;
    };

    // Format (tab-separated, first line is header):
    //   type    compressed      uncompressed    average extent size
    //   lz4     1.2G            3.5G            128K
    for line in content.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let algorithm = parts[0].to_string();
        let compressed = parse_human_bytes(parts[1]).unwrap_or(0);
        let uncompressed = parse_human_bytes(parts[2]).unwrap_or(0);

        if compressed > 0 || uncompressed > 0 {
            entries.push(CompressionEntry {
                algorithm,
                compressed_bytes: compressed,
                uncompressed_bytes: uncompressed,
            });
        }
    }

    entries
}

// ── Collect all ─────────────────────────────────────────────────

/// Collect all bcachefs metrics for all mounted filesystems.
pub fn collect_all() -> Vec<BcachefsMetrics> {
    let filesystems = discover_filesystems();
    let mut all = Vec::new();

    for fs in &filesystems {
        let metrics = BcachefsMetrics {
            uuid: fs.uuid.clone(),
            fs_name: fs.fs_name.clone(),
            counters: read_counters(&fs.sysfs_path),
            time_stats: read_time_stats(&fs.sysfs_path),
            devices: read_device_stats(&fs.sysfs_path),
            space: read_space(&fs.mount_point),
            options: read_fs_options(&fs.sysfs_path),
            background: read_background_ops(&fs.sysfs_path),
            compression: read_compression_stats(&fs.sysfs_path),
        };
        all.push(metrics);
    }

    all
}

// ── Helpers ─────────────────────────────────────────────────────

fn read_sysfs_str(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_sysfs_u64(path: &Path) -> u64 {
    read_sysfs_str(path)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Parse human-readable byte values like "1.2G", "512M", "128K", "4096".
fn parse_human_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Try plain integer first
    if let Ok(v) = s.parse::<u64>() {
        return Some(v);
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('K') {
        (n, 1024u64)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('T') {
        (n, 1024 * 1024 * 1024 * 1024)
    } else {
        return None;
    };

    num_str
        .parse::<f64>()
        .ok()
        .map(|v| (v * multiplier as f64) as u64)
}
