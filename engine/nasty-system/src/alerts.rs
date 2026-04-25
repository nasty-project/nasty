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

    pub async fn update_rule(&self, id: &str, update: AlertRuleUpdate) -> Result<AlertRule, String> {
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

    /// Evaluate all enabled rules against current system state
    pub async fn evaluate(
        &self,
        stats: &super::SystemStats,
        filesystems: &[FsUsage],
        disk_health: &[DiskHealthSummary],
        bcachefs_health: &[BcachefsHealth],
        kernel_errors: &KernelErrorAlert,
    ) -> Vec<ActiveAlert> {
        let state = self.state.read().await;
        let mut alerts = Vec::new();

        for rule in state.rules.iter().filter(|r| r.enabled) {
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
                        let pct = (stats.memory.used_bytes as f64
                            / stats.memory.total_bytes as f64)
                            * 100.0;
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
                    // threshold=1 means "alert when health_passed == false"
                    for disk in disk_health {
                        if !disk.health_passed {
                            alerts.push(ActiveAlert {
                                rule_id: rule.id.clone(),
                                rule_name: rule.name.clone(),
                                severity: rule.severity.clone(),
                                metric: rule.metric.clone(),
                                message: format!(
                                    "Disk {} SMART health check FAILED",
                                    disk.device
                                ),
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
            }
        }

        alerts
    }
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
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

async fn load_state() -> AlertState {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => AlertState::default(),
    }
}

async fn save_state(state: &AlertState) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(state).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}
