use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

const STATE_PATH: &str = "/var/lib/nasty/alerts.json";
const STATE_DIR: &str = "/var/lib/nasty";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AlertRule {
    /// Unique rule identifier.
    pub id: String,
    /// Human-readable rule name.
    pub name: String,
    /// Whether the rule is active and evaluated.
    pub enabled: bool,
    /// The system metric this rule monitors.
    pub metric: AlertMetric,
    /// Comparison operator applied between the metric value and the threshold.
    pub condition: AlertCondition,
    /// Threshold value the metric is compared against.
    pub threshold: f64,
    /// Severity level when the rule fires.
    pub severity: AlertSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertMetric {
    FsUsagePercent,
    CpuLoadPercent,
    MemoryUsagePercent,
    DiskTemperature,
    SmartHealth,
    SwapUsagePercent,
    // bcachefs health (always-on, threshold ignored)
    BcachefsDegraded,
    BcachefsDeviceError,
    BcachefsDeviceState,
    BcachefsIOErrors,
    BcachefsScrubErrors,
    BcachefsReconcileStalled,
    /// Root partition free space in GB.
    RootDiskFreeGb,
    /// /boot (ESP) free space in MB. Tiny by design (often 250–512 MB)
    /// and a single kernel+initrd pair is ~50 MB, so an MB-scale alert
    /// gives users meaningful warning before the next system update's
    /// switch-to-configuration step fails with ENOSPC trying to copy
    /// the new initrd onto the ESP.
    BootDiskFreeMb,
    // Kernel error monitoring
    KernelErrors,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertCondition {
    Above,
    Below,
    Equals,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActiveAlert {
    /// ID of the rule that triggered this alert.
    pub rule_id: String,
    /// Name of the rule that triggered this alert.
    pub rule_name: String,
    /// Severity level of the alert.
    pub severity: AlertSeverity,
    /// Metric that triggered the alert.
    pub metric: AlertMetric,
    /// Human-readable description of the alert condition.
    pub message: String,
    /// Current metric value at the time the alert was evaluated.
    pub current_value: f64,
    /// Threshold value configured in the rule.
    pub threshold: f64,
    /// Identifier of the specific resource that triggered the alert (e.g. filesystem name, device path).
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AlertState {
    rules: Vec<AlertRule>,
}

pub struct AlertService {
    state: Arc<RwLock<AlertState>>,
}

impl AlertService {
    pub async fn new() -> Self {
        let mut state = load_state().await;

        // Seed default rules on first run, and backfill any new defaults
        // added in later versions (matched by id).
        let defaults = default_rules();
        if state.rules.is_empty() {
            state.rules = defaults;
            save_state(&state).await.ok();
        } else {
            let mut added = false;
            for default in &defaults {
                if !state.rules.iter().any(|r| r.id == default.id) {
                    state.rules.push(default.clone());
                    added = true;
                }
            }
            if added {
                save_state(&state).await.ok();
            }
        }

        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    pub async fn list_rules(&self) -> Vec<AlertRule> {
        self.state.read().await.rules.clone()
    }

    pub async fn create_rule(&self, rule: AlertRule) -> Result<AlertRule, String> {
        let mut state = self.state.write().await;
        if state.rules.iter().any(|r| r.id == rule.id) {
            return Err("rule ID already exists".into());
        }
        let rule = AlertRule {
            id: if rule.id.is_empty() {
                uuid_v4()
            } else {
                rule.id
            },
            ..rule
        };
        state.rules.push(rule.clone());
        save_state(&state).await.map_err(|e| e.to_string())?;
        Ok(rule)
    }

    pub async fn update_rule(
        &self,
        id: &str,
        update: AlertRuleUpdate,
    ) -> Result<AlertRule, String> {
        let mut state = self.state.write().await;
        let rule = state
            .rules
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| "rule not found".to_string())?;

        if let Some(name) = update.name {
            rule.name = name;
        }
        if let Some(enabled) = update.enabled {
            rule.enabled = enabled;
        }
        if let Some(threshold) = update.threshold {
            rule.threshold = threshold;
        }
        if let Some(severity) = update.severity {
            rule.severity = severity;
        }

        let rule = rule.clone();
        save_state(&state).await.map_err(|e| e.to_string())?;
        Ok(rule)
    }

    pub async fn delete_rule(&self, id: &str) -> Result<(), String> {
        let mut state = self.state.write().await;
        let before = state.rules.len();
        state.rules.retain(|r| r.id != id);
        if state.rules.len() == before {
            return Err("rule not found".into());
        }
        save_state(&state).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Evaluate all enabled rules against current system state.
    /// Reads the configured root partition free space via `statvfs("/")`
    /// and the ESP free space via `statvfs("/boot")`.
    pub async fn evaluate(
        &self,
        stats: &super::SystemStats,
        filesystems: &[FsUsage],
        disk_health: &[DiskHealthSummary],
        bcachefs_health: &[BcachefsHealth],
        kernel_errors: &KernelErrorAlert,
    ) -> Vec<ActiveAlert> {
        let state = self.state.read().await;
        evaluate_rules(
            &state.rules,
            stats,
            filesystems,
            disk_health,
            bcachefs_health,
            kernel_errors,
            DiskFreeSpace {
                root_free_gb: root_free_gb(),
                boot_free_mb: boot_free_mb(),
            },
        )
    }
}

/// Free-space readings for the alert metrics that need them. Bundled
/// so the rule evaluator's signature stays manageable and so tests
/// can express "/boot unknown, / known" without juggling positional
/// `Option<f64>` args.
#[derive(Debug, Clone, Copy, Default)]
struct DiskFreeSpace {
    root_free_gb: Option<f64>,
    boot_free_mb: Option<f64>,
}

/// Pure rule dispatch — no I/O, no async, no shared state. Disk-free
/// values are injected via `DiskFreeSpace` so tests don't depend on
/// `statvfs("/")` / `statvfs("/boot")` and can drive every metric.
fn evaluate_rules(
    rules: &[AlertRule],
    stats: &super::SystemStats,
    filesystems: &[FsUsage],
    disk_health: &[DiskHealthSummary],
    bcachefs_health: &[BcachefsHealth],
    kernel_errors: &KernelErrorAlert,
    disk_free: DiskFreeSpace,
) -> Vec<ActiveAlert> {
    let mut alerts = Vec::new();

    for rule in rules.iter().filter(|r| r.enabled) {
        match rule.metric {
            AlertMetric::FsUsagePercent => {
                for fs in filesystems {
                    if fs.total_bytes == 0 {
                        continue;
                    }
                    let pct = (fs.used_bytes as f64 / fs.total_bytes as f64) * 100.0;
                    if check_condition(pct, &rule.condition, rule.threshold) {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!(
                                "Filesystem \"{}\" usage at {:.1}% (threshold: {:.0}%)",
                                fs.name, pct, rule.threshold
                            ),
                            current_value: pct,
                            threshold: rule.threshold,
                            source: fs.name.clone(),
                        });
                    }
                }
            }
            AlertMetric::CpuLoadPercent => {
                let pct = if stats.cpu.count > 0 {
                    (stats.cpu.load_1 / stats.cpu.count as f64) * 100.0
                } else {
                    0.0
                };
                if check_condition(pct, &rule.condition, rule.threshold) {
                    alerts.push(ActiveAlert {
                        rule_id: rule.id.clone(),
                        rule_name: rule.name.clone(),
                        severity: rule.severity.clone(),
                        metric: rule.metric.clone(),
                        message: format!(
                            "CPU load at {:.1}% (threshold: {:.0}%)",
                            pct, rule.threshold
                        ),
                        current_value: pct,
                        threshold: rule.threshold,
                        source: "cpu".into(),
                    });
                }
            }
            AlertMetric::MemoryUsagePercent => {
                if stats.memory.total_bytes > 0 {
                    let pct =
                        (stats.memory.used_bytes as f64 / stats.memory.total_bytes as f64) * 100.0;
                    if check_condition(pct, &rule.condition, rule.threshold) {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!(
                                "Memory usage at {:.1}% (threshold: {:.0}%)",
                                pct, rule.threshold
                            ),
                            current_value: pct,
                            threshold: rule.threshold,
                            source: "memory".into(),
                        });
                    }
                }
            }
            AlertMetric::SwapUsagePercent => {
                if stats.memory.swap_total_bytes > 0 {
                    let pct = (stats.memory.swap_used_bytes as f64
                        / stats.memory.swap_total_bytes as f64)
                        * 100.0;
                    if check_condition(pct, &rule.condition, rule.threshold) {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!(
                                "Swap usage at {:.1}% (threshold: {:.0}%)",
                                pct, rule.threshold
                            ),
                            current_value: pct,
                            threshold: rule.threshold,
                            source: "swap".into(),
                        });
                    }
                }
            }
            AlertMetric::DiskTemperature => {
                for disk in disk_health {
                    if let Some(temp) = disk.temperature_c {
                        let val = temp as f64;
                        if check_condition(val, &rule.condition, rule.threshold) {
                            alerts.push(ActiveAlert {
                                rule_id: rule.id.clone(),
                                rule_name: rule.name.clone(),
                                severity: rule.severity.clone(),
                                metric: rule.metric.clone(),
                                message: format!(
                                    "Disk {} temperature at {}°C (threshold: {:.0}°C)",
                                    disk.device, temp, rule.threshold
                                ),
                                current_value: val,
                                threshold: rule.threshold,
                                source: disk.device.clone(),
                            });
                        }
                    }
                }
            }
            AlertMetric::SmartHealth => {
                // threshold=1 means "alert when health_passed == false".
                // Skip disks where SMART itself is UNAVAILABLE (USB-SATA
                // bridge without `-d sat`, controller that doesn't proxy
                // SMART, unsupported transport, …) — "no data" isn't
                // "FAILED", and firing a critical alert on every disk
                // the metrics service can't read SMART for would be a
                // false-positive storm. The disk still appears in the
                // WebUI Disks page with status UNAVAILABLE, so the
                // operator sees the gap.
                for disk in disk_health {
                    if disk.smart_status == "UNAVAILABLE" {
                        continue;
                    }
                    if !disk.health_passed {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!("Disk {} SMART health check FAILED", disk.device),
                            current_value: 0.0,
                            threshold: rule.threshold,
                            source: disk.device.clone(),
                        });
                    }
                }
            }
            // ── bcachefs health checks (always-on, threshold ignored) ──
            AlertMetric::BcachefsDegraded => {
                for fs in bcachefs_health {
                    if fs.degraded {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!(
                                "Filesystem \"{}\" is running in DEGRADED mode (missing device)",
                                fs.fs_name
                            ),
                            current_value: 1.0,
                            threshold: 0.0,
                            source: fs.fs_name.clone(),
                        });
                    }
                }
            }
            AlertMetric::BcachefsDeviceState => {
                for fs in bcachefs_health {
                    for dev in &fs.devices {
                        if dev.state != "rw" && dev.state != "spare" {
                            alerts.push(ActiveAlert {
                                    rule_id: rule.id.clone(),
                                    rule_name: rule.name.clone(),
                                    severity: rule.severity.clone(),
                                    metric: rule.metric.clone(),
                                    message: format!(
                                        "Device {} in filesystem \"{}\" is in '{}' state (expected 'rw')",
                                        dev.path, fs.fs_name, dev.state
                                    ),
                                    current_value: 0.0,
                                    threshold: 0.0,
                                    source: dev.path.clone(),
                                });
                        }
                    }
                }
            }
            AlertMetric::BcachefsDeviceError => {
                for fs in bcachefs_health {
                    for dev in &fs.devices {
                        if dev.has_errors {
                            alerts.push(ActiveAlert {
                                rule_id: rule.id.clone(),
                                rule_name: rule.name.clone(),
                                severity: rule.severity.clone(),
                                metric: rule.metric.clone(),
                                message: format!(
                                    "Device {} in filesystem \"{}\" has IO errors",
                                    dev.path, fs.fs_name
                                ),
                                current_value: 1.0,
                                threshold: 0.0,
                                source: dev.path.clone(),
                            });
                        }
                    }
                }
            }
            AlertMetric::BcachefsIOErrors => {
                for fs in bcachefs_health {
                    if fs.io_error_count > 0 {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!(
                                "Filesystem \"{}\" has {} IO errors",
                                fs.fs_name, fs.io_error_count
                            ),
                            current_value: fs.io_error_count as f64,
                            threshold: 0.0,
                            source: fs.fs_name.clone(),
                        });
                    }
                }
            }
            AlertMetric::BcachefsScrubErrors => {
                for fs in bcachefs_health {
                    if fs.scrub_errors {
                        alerts.push(ActiveAlert {
                            rule_id: rule.id.clone(),
                            rule_name: rule.name.clone(),
                            severity: rule.severity.clone(),
                            metric: rule.metric.clone(),
                            message: format!(
                                "Filesystem \"{}\" scrub found data corruption",
                                fs.fs_name
                            ),
                            current_value: 1.0,
                            threshold: 0.0,
                            source: fs.fs_name.clone(),
                        });
                    }
                }
            }
            AlertMetric::BcachefsReconcileStalled => {
                for fs in bcachefs_health {
                    if fs.reconcile_stalled {
                        alerts.push(ActiveAlert {
                                rule_id: rule.id.clone(),
                                rule_name: rule.name.clone(),
                                severity: rule.severity.clone(),
                                metric: rule.metric.clone(),
                                message: format!(
                                    "Filesystem \"{}\" reconcile is stalled — background work not progressing",
                                    fs.fs_name
                                ),
                                current_value: 1.0,
                                threshold: 0.0,
                                source: fs.fs_name.clone(),
                            });
                    }
                }
            }
            AlertMetric::KernelErrors => {
                let val = kernel_errors.total_count as f64;
                if check_condition(val, &rule.condition, rule.threshold) {
                    let cat_list = if kernel_errors.categories.is_empty() {
                        "none".to_string()
                    } else {
                        kernel_errors.categories.join(", ")
                    };
                    alerts.push(ActiveAlert {
                        rule_id: rule.id.clone(),
                        rule_name: rule.name.clone(),
                        severity: rule.severity.clone(),
                        metric: rule.metric.clone(),
                        message: format!(
                            "{} kernel error(s) detected (categories: {})",
                            kernel_errors.total_count, cat_list
                        ),
                        current_value: val,
                        threshold: rule.threshold,
                        source: "kernel".into(),
                    });
                }
            }
            AlertMetric::RootDiskFreeGb => {
                if let Some(free_gb) = disk_free.root_free_gb
                    && check_condition(free_gb, &rule.condition, rule.threshold)
                {
                    alerts.push(ActiveAlert {
                        rule_id: rule.id.clone(),
                        rule_name: rule.name.clone(),
                        severity: rule.severity.clone(),
                        metric: rule.metric.clone(),
                        message: format!(
                            "Root partition has {:.1} GB free (threshold: {:.0} GB)",
                            free_gb, rule.threshold
                        ),
                        current_value: free_gb,
                        threshold: rule.threshold,
                        source: "/".into(),
                    });
                }
            }
            AlertMetric::BootDiskFreeMb => {
                if let Some(free_mb) = disk_free.boot_free_mb
                    && check_condition(free_mb, &rule.condition, rule.threshold)
                {
                    alerts.push(ActiveAlert {
                        rule_id: rule.id.clone(),
                        rule_name: rule.name.clone(),
                        severity: rule.severity.clone(),
                        metric: rule.metric.clone(),
                        // Mention the actionable remedy in the message
                        // so the user doesn't have to dig — the typical
                        // path is "trim old generations" not "resize ESP".
                        // `nasty-cleanup` is the one-shot helper that
                        // does delete-old-generations + nix-gc +
                        // switch-to-configuration boot in order.
                        message: format!(
                            "/boot has {:.0} MB free (threshold: {:.0} MB). The next system update may fail to install its initrd. Run `nasty-cleanup` to reclaim space.",
                            free_mb, rule.threshold
                        ),
                        current_value: free_mb,
                        threshold: rule.threshold,
                        source: "/boot".into(),
                    });
                }
            }
        }
    }

    alerts
}

/// Minimal filesystem info for alert evaluation
#[derive(Debug)]
pub struct FsUsage {
    pub name: String,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// Minimal disk info for alert evaluation
#[derive(Debug)]
pub struct DiskHealthSummary {
    pub device: String,
    pub temperature_c: Option<i32>,
    pub health_passed: bool,
    /// Mirror of `DiskHealth::smart_status`. Carried into the summary so
    /// the `SmartHealth` alert rule can distinguish "FAILED" (alert) from
    /// "UNAVAILABLE" (don't alert — smartctl couldn't read SMART, that's
    /// not the same as a confirmed health failure).
    pub smart_status: String,
}

/// Kernel error data for alert evaluation.
#[derive(Debug, Default)]
pub struct KernelErrorAlert {
    /// Total error count since boot.
    pub total_count: u64,
    /// Category names that have errors.
    pub categories: Vec<String>,
}

/// bcachefs filesystem health for alert evaluation
#[derive(Debug)]
pub struct BcachefsHealth {
    pub fs_name: String,
    /// Mounted in degraded mode (missing devices)
    pub degraded: bool,
    /// Per-device state and error info
    pub devices: Vec<BcachefsDeviceHealth>,
    /// IO error counts from sysfs counters (read_errors + write_errors)
    pub io_error_count: u64,
    /// Whether a scrub found errors (from last scrub status)
    pub scrub_errors: bool,
    /// Whether reconcile has pending work but isn't making progress
    pub reconcile_stalled: bool,
}

#[derive(Debug)]
pub struct BcachefsDeviceHealth {
    pub path: String,
    /// Device state: "rw", "ro", "evacuating", "spare"
    pub state: String,
    /// Whether the device has IO errors reported in sysfs
    pub has_errors: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AlertRuleUpdate {
    /// ID of the rule to update.
    pub id: String,
    /// New name for the rule (optional).
    #[serde(default)]
    pub name: Option<String>,
    /// Enable or disable the rule (optional).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New threshold value (optional).
    #[serde(default)]
    pub threshold: Option<f64>,
    /// New severity level (optional).
    #[serde(default)]
    pub severity: Option<AlertSeverity>,
}

fn root_free_gb() -> Option<f64> {
    statvfs_free_bytes("/").map(|b| b / 1_073_741_824.0)
}

/// Free bytes on the ESP, reported in MB. Returns None when /boot
/// isn't a separate mount or statvfs fails — in that case the alert
/// arm sees no value and no alert fires (a not-mounted /boot can't
/// "fill up", so silence is correct).
fn boot_free_mb() -> Option<f64> {
    statvfs_free_bytes("/boot").map(|b| b / 1_048_576.0)
}

fn statvfs_free_bytes(path: &str) -> Option<f64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let path = CString::new(path).ok()?;
    let mut buf = MaybeUninit::<libc::statvfs>::uninit();
    let ret = unsafe { libc::statvfs(path.as_ptr(), buf.as_mut_ptr()) };
    if ret != 0 {
        return None;
    }
    let stat = unsafe { buf.assume_init() };
    Some(stat.f_bavail as f64 * stat.f_frsize as f64)
}

fn check_condition(value: f64, condition: &AlertCondition, threshold: f64) -> bool {
    match condition {
        AlertCondition::Above => value > threshold,
        AlertCondition::Below => value < threshold,
        AlertCondition::Equals => (value - threshold).abs() < 0.001,
    }
}

fn default_rules() -> Vec<AlertRule> {
    vec![
        AlertRule {
            id: "fs-usage-warn".into(),
            name: "Filesystem usage warning".into(),
            enabled: true,
            metric: AlertMetric::FsUsagePercent,
            condition: AlertCondition::Above,
            threshold: 80.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "fs-usage-crit".into(),
            name: "Filesystem usage critical".into(),
            enabled: true,
            metric: AlertMetric::FsUsagePercent,
            condition: AlertCondition::Above,
            threshold: 95.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "disk-temp-warn".into(),
            name: "Disk temperature warning".into(),
            enabled: true,
            metric: AlertMetric::DiskTemperature,
            condition: AlertCondition::Above,
            threshold: 50.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "disk-temp-crit".into(),
            name: "Disk temperature critical".into(),
            enabled: true,
            metric: AlertMetric::DiskTemperature,
            condition: AlertCondition::Above,
            threshold: 60.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "smart-health".into(),
            name: "SMART health failure".into(),
            enabled: true,
            metric: AlertMetric::SmartHealth,
            condition: AlertCondition::Equals,
            threshold: 1.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "memory-warn".into(),
            name: "Memory usage warning".into(),
            enabled: true,
            metric: AlertMetric::MemoryUsagePercent,
            condition: AlertCondition::Above,
            threshold: 90.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "cpu-load-warn".into(),
            name: "CPU load warning".into(),
            enabled: true,
            metric: AlertMetric::CpuLoadPercent,
            condition: AlertCondition::Above,
            threshold: 90.0,
            severity: AlertSeverity::Warning,
        },
        // bcachefs health (always-on, threshold not used)
        AlertRule {
            id: "bcachefs-degraded".into(),
            name: "bcachefs degraded (missing device)".into(),
            enabled: true,
            metric: AlertMetric::BcachefsDegraded,
            condition: AlertCondition::Equals,
            threshold: 1.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "bcachefs-device-state".into(),
            name: "bcachefs device not read-write".into(),
            enabled: true,
            metric: AlertMetric::BcachefsDeviceState,
            condition: AlertCondition::Equals,
            threshold: 1.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "bcachefs-device-errors".into(),
            name: "bcachefs device IO errors".into(),
            enabled: true,
            metric: AlertMetric::BcachefsDeviceError,
            condition: AlertCondition::Equals,
            threshold: 1.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "bcachefs-io-errors".into(),
            name: "bcachefs filesystem IO errors".into(),
            enabled: true,
            metric: AlertMetric::BcachefsIOErrors,
            condition: AlertCondition::Above,
            threshold: 0.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "bcachefs-scrub-errors".into(),
            name: "bcachefs scrub found corruption".into(),
            enabled: true,
            metric: AlertMetric::BcachefsScrubErrors,
            condition: AlertCondition::Equals,
            threshold: 1.0,
            severity: AlertSeverity::Critical,
        },
        AlertRule {
            id: "bcachefs-reconcile-stalled".into(),
            name: "bcachefs reconcile stalled".into(),
            enabled: true,
            metric: AlertMetric::BcachefsReconcileStalled,
            condition: AlertCondition::Equals,
            threshold: 1.0,
            severity: AlertSeverity::Warning,
        },
        // Root partition space
        AlertRule {
            id: "root-disk-low".into(),
            name: "Root partition low on space".into(),
            enabled: true,
            metric: AlertMetric::RootDiskFreeGb,
            condition: AlertCondition::Below,
            threshold: 10.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "root-disk-crit".into(),
            name: "Root partition critically low".into(),
            enabled: true,
            metric: AlertMetric::RootDiskFreeGb,
            condition: AlertCondition::Below,
            threshold: 3.0,
            severity: AlertSeverity::Critical,
        },
        // /boot (ESP) space. Each kernel+initrd pair is roughly 50 MB;
        // when the ESP is under 100 MB free the next system update is
        // at real risk of failing in switch-to-configuration's bootloader
        // install step. 30 MB is "you got a build through but barely" —
        // upgrade to critical so the user fixes it before the next try.
        AlertRule {
            id: "boot-disk-low".into(),
            name: "/boot partition low on space".into(),
            enabled: true,
            metric: AlertMetric::BootDiskFreeMb,
            condition: AlertCondition::Below,
            threshold: 100.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "boot-disk-crit".into(),
            name: "/boot partition critically low".into(),
            enabled: true,
            metric: AlertMetric::BootDiskFreeMb,
            condition: AlertCondition::Below,
            threshold: 30.0,
            severity: AlertSeverity::Critical,
        },
        // Kernel error monitoring
        AlertRule {
            id: "kernel-errors".into(),
            name: "Kernel errors detected".into(),
            enabled: true,
            metric: AlertMetric::KernelErrors,
            condition: AlertCondition::Above,
            threshold: 0.0,
            severity: AlertSeverity::Warning,
        },
    ]
}

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    // Use /dev/urandom for random bytes
    if let Ok(data) = std::fs::read("/dev/urandom") {
        for (i, b) in data.iter().take(16).enumerate() {
            bytes[i] = *b;
        }
    }
    // Set version and variant bits
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

async fn load_state() -> AlertState {
    nasty_common::load_singleton_or_recover(STATE_PATH).await
}

async fn save_state(state: &AlertState) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(state).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SystemStats;
    use nasty_common::metrics_types::{CpuStats, MemoryStats};

    fn rule(metric: AlertMetric, condition: AlertCondition, threshold: f64) -> AlertRule {
        AlertRule {
            id: "r".to_string(),
            name: "rule".to_string(),
            enabled: true,
            metric,
            condition,
            threshold,
            severity: AlertSeverity::Warning,
        }
    }

    fn zero_stats() -> SystemStats {
        SystemStats {
            cpu: CpuStats {
                count: 0,
                load_1: 0.0,
                load_5: 0.0,
                load_15: 0.0,
                temp_c: None,
                freq_mhz: None,
                governor: None,
            },
            memory: MemoryStats {
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            network: vec![],
            disk_io: vec![],
        }
    }

    fn run(rules: &[AlertRule], stats: &SystemStats) -> Vec<ActiveAlert> {
        evaluate_rules(
            rules,
            stats,
            &[],
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        )
    }

    // ── check_condition ────────────────────────────────────────────

    #[test]
    fn check_condition_above() {
        assert!(check_condition(91.0, &AlertCondition::Above, 90.0));
        assert!(!check_condition(90.0, &AlertCondition::Above, 90.0));
        assert!(!check_condition(89.9, &AlertCondition::Above, 90.0));
    }

    #[test]
    fn check_condition_below() {
        assert!(check_condition(2.9, &AlertCondition::Below, 3.0));
        assert!(!check_condition(3.0, &AlertCondition::Below, 3.0));
        assert!(!check_condition(3.1, &AlertCondition::Below, 3.0));
    }

    #[test]
    fn check_condition_equals_uses_float_tolerance() {
        assert!(check_condition(1.0, &AlertCondition::Equals, 1.0));
        assert!(check_condition(1.0005, &AlertCondition::Equals, 1.0));
        assert!(!check_condition(1.01, &AlertCondition::Equals, 1.0));
    }

    // ── evaluate_rules: dispatch / disable ─────────────────────────

    #[test]
    fn evaluate_rules_skips_disabled_rules() {
        let mut r = rule(AlertMetric::FsUsagePercent, AlertCondition::Above, 80.0);
        r.enabled = false;
        let fs = vec![FsUsage {
            name: "tank".into(),
            used_bytes: 95,
            total_bytes: 100,
        }];
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &fs,
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert!(alerts.is_empty());
    }

    // ── evaluate_rules: per-metric ─────────────────────────────────

    #[test]
    fn evaluate_rules_fs_usage_fires_above_threshold_and_skips_zero_total() {
        let r = rule(AlertMetric::FsUsagePercent, AlertCondition::Above, 80.0);
        let fs = vec![
            FsUsage {
                name: "tank".into(),
                used_bytes: 90,
                total_bytes: 100,
            },
            FsUsage {
                name: "empty".into(),
                used_bytes: 0,
                total_bytes: 0, // skipped
            },
        ];
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &fs,
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "tank");
        assert!((alerts[0].current_value - 90.0).abs() < 0.001);
    }

    #[test]
    fn evaluate_rules_cpu_load_normalises_by_core_count() {
        // load_1=4 on 8 cores → 50% — below threshold 90 → no fire.
        let r = rule(AlertMetric::CpuLoadPercent, AlertCondition::Above, 90.0);
        let rules = std::slice::from_ref(&r);
        let mut s = zero_stats();
        s.cpu.count = 8;
        s.cpu.load_1 = 4.0;
        assert!(run(rules, &s).is_empty());
        // load_1=8 on 8 cores → 100% — fires.
        s.cpu.load_1 = 8.0;
        let alerts = run(rules, &s);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "cpu");
    }

    #[test]
    fn evaluate_rules_memory_fires_above_threshold() {
        let r = rule(AlertMetric::MemoryUsagePercent, AlertCondition::Above, 90.0);
        let mut s = zero_stats();
        s.memory.total_bytes = 100;
        s.memory.used_bytes = 95;
        let alerts = run(&[r], &s);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "memory");
    }

    #[test]
    fn evaluate_rules_swap_skipped_when_swap_total_zero() {
        let r = rule(AlertMetric::SwapUsagePercent, AlertCondition::Above, 0.0);
        let s = zero_stats();
        assert!(run(&[r], &s).is_empty());
    }

    #[test]
    fn evaluate_rules_disk_temperature_fires_per_disk_with_value() {
        let r = rule(AlertMetric::DiskTemperature, AlertCondition::Above, 50.0);
        let disks = vec![
            DiskHealthSummary {
                device: "sda".into(),
                temperature_c: Some(60),
                health_passed: true,
                smart_status: "PASSED".into(),
            },
            DiskHealthSummary {
                device: "sdb".into(),
                temperature_c: None, // skipped
                health_passed: true,
                smart_status: "PASSED".into(),
            },
            DiskHealthSummary {
                device: "sdc".into(),
                temperature_c: Some(45), // below threshold
                health_passed: true,
                smart_status: "PASSED".into(),
            },
        ];
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &disks,
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "sda");
    }

    #[test]
    fn evaluate_rules_smart_health_fires_only_on_failure() {
        let r = rule(AlertMetric::SmartHealth, AlertCondition::Equals, 1.0);
        let disks = vec![
            DiskHealthSummary {
                device: "sda".into(),
                temperature_c: None,
                health_passed: true,
                smart_status: "PASSED".into(),
            },
            DiskHealthSummary {
                device: "sdb".into(),
                temperature_c: None,
                health_passed: false,
                smart_status: "FAILED".into(),
            },
        ];
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &disks,
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "sdb");
    }

    #[test]
    fn evaluate_rules_smart_health_skips_unavailable_disks() {
        // Regression for #349: disks with smart_status="UNAVAILABLE"
        // (smartctl couldn't read SMART — USB-SATA bridge that needs
        // -d sat, controller that doesn't proxy SMART, fresh disk
        // before kernel finished initializing) carry health_passed=false
        // by construction, but that's "unknown" not "FAILED". The
        // SmartHealth rule must not fire on them.
        let r = rule(AlertMetric::SmartHealth, AlertCondition::Equals, 1.0);
        let disks = vec![DiskHealthSummary {
            device: "sdb".into(),
            temperature_c: None,
            health_passed: false,
            smart_status: "UNAVAILABLE".into(),
        }];
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &disks,
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert!(
            alerts.is_empty(),
            "UNAVAILABLE smart_status must not trigger SmartHealth alerts"
        );
    }

    fn bcachefs_health(devices: Vec<BcachefsDeviceHealth>) -> BcachefsHealth {
        BcachefsHealth {
            fs_name: "tank".into(),
            degraded: false,
            devices,
            io_error_count: 0,
            scrub_errors: false,
            reconcile_stalled: false,
        }
    }

    fn dev(path: &str, state: &str, has_errors: bool) -> BcachefsDeviceHealth {
        BcachefsDeviceHealth {
            path: path.into(),
            state: state.into(),
            has_errors,
        }
    }

    #[test]
    fn evaluate_rules_bcachefs_degraded() {
        let r = rule(AlertMetric::BcachefsDegraded, AlertCondition::Equals, 1.0);
        let mut h = bcachefs_health(vec![]);
        h.degraded = true;
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &[],
            &[h],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "tank");
    }

    #[test]
    fn evaluate_rules_bcachefs_device_state_treats_spare_as_ok() {
        let r = rule(
            AlertMetric::BcachefsDeviceState,
            AlertCondition::Equals,
            1.0,
        );
        let h = bcachefs_health(vec![
            dev("/dev/sda", "rw", false),
            dev("/dev/sdb", "spare", false), // not an alert
            dev("/dev/sdc", "ro", false),    // alert
        ]);
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &[],
            &[h],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "/dev/sdc");
    }

    #[test]
    fn evaluate_rules_bcachefs_device_error_fires_when_has_errors() {
        let r = rule(
            AlertMetric::BcachefsDeviceError,
            AlertCondition::Equals,
            1.0,
        );
        let h = bcachefs_health(vec![dev("/dev/sda", "rw", true)]);
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &[],
            &[h],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "/dev/sda");
    }

    #[test]
    fn evaluate_rules_bcachefs_io_errors_above_zero() {
        let r = rule(AlertMetric::BcachefsIOErrors, AlertCondition::Above, 0.0);
        let mut h = bcachefs_health(vec![]);
        h.io_error_count = 7;
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &[],
            &[h],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert!((alerts[0].current_value - 7.0).abs() < 0.001);
    }

    #[test]
    fn evaluate_rules_bcachefs_scrub_and_reconcile() {
        let scrub = rule(
            AlertMetric::BcachefsScrubErrors,
            AlertCondition::Equals,
            1.0,
        );
        let reconcile = rule(
            AlertMetric::BcachefsReconcileStalled,
            AlertCondition::Equals,
            1.0,
        );
        let mut h = bcachefs_health(vec![]);
        h.scrub_errors = true;
        h.reconcile_stalled = true;
        let alerts = evaluate_rules(
            &[scrub, reconcile],
            &zero_stats(),
            &[],
            &[],
            &[h],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 2);
    }

    #[test]
    fn evaluate_rules_kernel_errors_includes_categories_in_message() {
        let r = rule(AlertMetric::KernelErrors, AlertCondition::Above, 0.0);
        let kernel_errors = KernelErrorAlert {
            total_count: 3,
            categories: vec!["mce".into(), "oom".into()],
        };
        let alerts = evaluate_rules(
            &[r],
            &zero_stats(),
            &[],
            &[],
            &[],
            &kernel_errors,
            DiskFreeSpace::default(),
        );
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("mce"));
        assert!(alerts[0].message.contains("oom"));
    }

    #[test]
    fn evaluate_rules_root_disk_free_gb_fires_when_below_threshold() {
        let r = rule(AlertMetric::RootDiskFreeGb, AlertCondition::Below, 10.0);
        let rules = std::slice::from_ref(&r);
        let alerts = evaluate_rules(
            rules,
            &zero_stats(),
            &[],
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace {
                root_free_gb: Some(5.0),
                boot_free_mb: None,
            },
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "/");
        // None means "unknown" — no fire.
        let alerts = evaluate_rules(
            rules,
            &zero_stats(),
            &[],
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert!(alerts.is_empty());
    }

    #[test]
    fn evaluate_rules_boot_disk_free_mb_fires_when_below_threshold() {
        let r = rule(AlertMetric::BootDiskFreeMb, AlertCondition::Below, 100.0);
        let rules = std::slice::from_ref(&r);
        let alerts = evaluate_rules(
            rules,
            &zero_stats(),
            &[],
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace {
                root_free_gb: None,
                boot_free_mb: Some(20.0),
            },
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, "/boot");
        assert!((alerts[0].current_value - 20.0).abs() < 0.001);
        // None means "/boot not statvfs'able" (e.g. not a separate
        // mount) — no fire, silence is correct.
        let alerts = evaluate_rules(
            rules,
            &zero_stats(),
            &[],
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace::default(),
        );
        assert!(alerts.is_empty());
        // Above threshold — no fire.
        let alerts = evaluate_rules(
            rules,
            &zero_stats(),
            &[],
            &[],
            &[],
            &KernelErrorAlert::default(),
            DiskFreeSpace {
                root_free_gb: None,
                boot_free_mb: Some(150.0),
            },
        );
        assert!(alerts.is_empty());
    }

    // ── default_rules smoke ────────────────────────────────────────

    #[test]
    fn default_rules_have_unique_ids() {
        let rules = default_rules();
        assert!(!rules.is_empty());
        let mut ids: Vec<_> = rules.iter().map(|r| r.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(
            ids.len(),
            rules.len(),
            "duplicate rule ids in default_rules"
        );
    }
}
