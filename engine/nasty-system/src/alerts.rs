use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

const STATE_PATH: &str = "/var/lib/nasty/alerts.json";
const STATE_DIR: &str = "/var/lib/nasty";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub metric: AlertMetric,
    pub condition: AlertCondition,
    pub threshold: f64,
    pub severity: AlertSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AlertMetric {
    PoolUsagePercent,
    CpuLoadPercent,
    MemoryUsagePercent,
    DiskTemperature,
    SmartHealth,
    SwapUsagePercent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AlertCondition {
    Above,
    Below,
    Equals,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveAlert {
    pub rule_id: String,
    pub rule_name: String,
    pub severity: AlertSeverity,
    pub metric: AlertMetric,
    pub message: String,
    pub current_value: f64,
    pub threshold: f64,
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

        // Seed default rules if empty
        if state.rules.is_empty() {
            state.rules = default_rules();
            save_state(&state).await.ok();
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
        pools: &[PoolUsage],
        disk_health: &[DiskHealthSummary],
    ) -> Vec<ActiveAlert> {
        let state = self.state.read().await;
        let mut alerts = Vec::new();

        for rule in state.rules.iter().filter(|r| r.enabled) {
            match rule.metric {
                AlertMetric::PoolUsagePercent => {
                    for pool in pools {
                        if pool.total_bytes == 0 {
                            continue;
                        }
                        let pct = (pool.used_bytes as f64 / pool.total_bytes as f64) * 100.0;
                        if check_condition(pct, &rule.condition, rule.threshold) {
                            alerts.push(ActiveAlert {
                                rule_id: rule.id.clone(),
                                rule_name: rule.name.clone(),
                                severity: rule.severity.clone(),
                                metric: rule.metric.clone(),
                                message: format!(
                                    "Pool \"{}\" usage at {:.1}% (threshold: {:.0}%)",
                                    pool.name, pct, rule.threshold
                                ),
                                current_value: pct,
                                threshold: rule.threshold,
                                source: pool.name.clone(),
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
            }
        }

        alerts
    }
}

/// Minimal pool info for alert evaluation
#[derive(Debug)]
pub struct PoolUsage {
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

#[derive(Debug, Deserialize)]
pub struct AlertRuleUpdate {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub threshold: Option<f64>,
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
            id: "pool-usage-warn".into(),
            name: "Pool usage warning".into(),
            enabled: true,
            metric: AlertMetric::PoolUsagePercent,
            condition: AlertCondition::Above,
            threshold: 80.0,
            severity: AlertSeverity::Warning,
        },
        AlertRule {
            id: "pool-usage-crit".into(),
            name: "Pool usage critical".into(),
            enabled: true,
            metric: AlertMetric::PoolUsagePercent,
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
