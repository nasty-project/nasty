use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/nut.json";
const NUT_CONF_DIR: &str = "/var/lib/nasty/nut";

// ── Config structs ───────────────────────────────────────────

/// NUT (Network UPS Tools) configuration for a locally connected UPS.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NutConfig {
    /// NUT driver name (e.g. `usbhid-ups`, `blazer_usb`, `snmp-ups`).
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Device port. `auto` for USB auto-detection, or a path like `/dev/ttyS0`.
    #[serde(default = "default_port")]
    pub port: String,
    /// UPS identifier used by upsc/upsd (e.g. `ups`).
    #[serde(default = "default_ups_name")]
    pub ups_name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Initiate shutdown when battery drops below this percentage.
    #[serde(default = "default_shutdown_percent")]
    pub shutdown_on_battery_percent: u32,
    /// Initiate shutdown after this many seconds on battery power.
    #[serde(default = "default_shutdown_seconds")]
    pub shutdown_on_battery_seconds: u32,
    /// Command to execute for system shutdown.
    #[serde(default = "default_shutdown_command")]
    pub shutdown_command: String,
}

fn default_driver() -> String { "usbhid-ups".into() }
fn default_port() -> String { "auto".into() }
fn default_ups_name() -> String { "ups".into() }
fn default_shutdown_percent() -> u32 { 20 }
fn default_shutdown_seconds() -> u32 { 120 }
fn default_shutdown_command() -> String {
    "/run/current-system/sw/bin/systemctl poweroff".into()
}

impl Default for NutConfig {
    fn default() -> Self {
        Self {
            driver: default_driver(),
            port: default_port(),
            ups_name: default_ups_name(),
            description: String::new(),
            shutdown_on_battery_percent: default_shutdown_percent(),
            shutdown_on_battery_seconds: default_shutdown_seconds(),
            shutdown_command: default_shutdown_command(),
        }
    }
}

/// Partial update for NUT configuration.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NutConfigUpdate {
    pub driver: Option<String>,
    pub port: Option<String>,
    pub ups_name: Option<String>,
    pub description: Option<String>,
    pub shutdown_on_battery_percent: Option<u32>,
    pub shutdown_on_battery_seconds: Option<u32>,
    pub shutdown_command: Option<String>,
}

/// Live UPS status read from `upsc`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UpsStatus {
    /// UPS status string (e.g. `OL` = online, `OB` = on battery, `LB` = low battery).
    pub status: String,
    /// Battery charge percentage.
    pub battery_charge: Option<f64>,
    /// Estimated battery runtime in seconds.
    pub battery_runtime: Option<u64>,
    /// Input voltage (from mains).
    pub input_voltage: Option<f64>,
    /// Output voltage (to load).
    pub output_voltage: Option<f64>,
    /// UPS load percentage.
    pub ups_load: Option<f64>,
    /// UPS model/product name.
    pub ups_model: Option<String>,
    /// UPS serial number.
    pub ups_serial: Option<String>,
    /// Whether the UPS service is running and reachable.
    pub available: bool,
    /// All raw key-value pairs from upsc.
    pub raw: HashMap<String, String>,
}

// ── Service ──────────────────────────────────────────────────

pub struct NutService {
    state: Arc<RwLock<NutConfig>>,
}

impl NutService {
    pub async fn new() -> Self {
        let config = load().await;
        Self {
            state: Arc::new(RwLock::new(config)),
        }
    }

    pub async fn get_config(&self) -> NutConfig {
        self.state.read().await.clone()
    }

    pub async fn update_config(&self, update: NutConfigUpdate) -> Result<NutConfig, String> {
        let mut config = self.state.write().await;

        if let Some(v) = update.driver { config.driver = v; }
        if let Some(v) = update.port { config.port = v; }
        if let Some(v) = update.ups_name {
            if v.is_empty() { return Err("ups_name cannot be empty".into()); }
            config.ups_name = v;
        }
        if let Some(v) = update.description { config.description = v; }
        if let Some(v) = update.shutdown_on_battery_percent {
            if v > 100 { return Err("shutdown_on_battery_percent must be 0-100".into()); }
            config.shutdown_on_battery_percent = v;
        }
        if let Some(v) = update.shutdown_on_battery_seconds {
            config.shutdown_on_battery_seconds = v;
        }
        if let Some(v) = update.shutdown_command { config.shutdown_command = v; }

        save(&config).await.map_err(|e| e.to_string())?;

        // Regenerate config files so next restart picks them up.
        // If NUT is currently running, restart it.
        if let Err(e) = write_config_files(&config).await {
            warn!("Failed to write NUT config files: {e}");
        }
        if is_nut_running().await {
            // Spawn restart in background — some drivers (nutdrv_qx) take 20-30s
            // to probe USB and we don't want the API call to block/timeout.
            tokio::spawn(async { restart_nut_services().await });
        }

        Ok(config.clone())
    }

    /// Read live UPS status via `upsc`.
    pub async fn status(&self) -> UpsStatus {
        let config = self.state.read().await;
        read_ups_status(&config.ups_name).await
    }

    /// Write NUT config files. Called before starting services.
    pub async fn write_configs(&self) -> Result<(), String> {
        let config = self.state.read().await;
        write_config_files(&config).await
    }
}

// ── Config file generation ───────────────────────────────────

pub async fn write_config_files(config: &NutConfig) -> Result<(), String> {
    tokio::fs::create_dir_all(NUT_CONF_DIR).await
        .map_err(|e| format!("failed to create {NUT_CONF_DIR}: {e}"))?;

    // ups.conf
    let ups_conf = format!(
        "[{name}]\n  driver = {driver}\n  port = {port}\n  desc = \"{desc}\"\n",
        name = config.ups_name,
        driver = config.driver,
        port = config.port,
        desc = config.description,
    );
    write_file("ups.conf", &ups_conf).await?;

    // upsd.conf
    write_file("upsd.conf", "LISTEN 0.0.0.0 3493\n").await?;

    // upsd.users
    let upsd_users = "[nasty]\n  password = nasty\n  upsmon master\n";
    write_file("upsd.users", upsd_users).await?;

    // upsmon.conf
    let upsmon_conf = format!(
        concat!(
            "MONITOR {name}@localhost 1 nasty nasty master\n",
            "SHUTDOWNCMD \"{cmd}\"\n",
            "MINSUPPLIES 1\n",
            "POLLFREQ 5\n",
            "POLLFREQALERT 5\n",
            "HOSTSYNC 15\n",
            "DEADTIME 15\n",
            "RBWARNTIME 43200\n",
            "NOCOMMWARNTIME 300\n",
            "FINALDELAY 5\n",
        ),
        name = config.ups_name,
        cmd = config.shutdown_command,
    );
    write_file("upsmon.conf", &upsmon_conf).await?;

    info!("NUT config files written to {NUT_CONF_DIR}");
    Ok(())
}

async fn write_file(name: &str, content: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let path = format!("{NUT_CONF_DIR}/{name}");
    tokio::fs::write(&path, content).await
        .map_err(|e| format!("failed to write {path}: {e}"))?;

    // upsd.users contains credentials — restrict to owner-only
    if name == "upsd.users" {
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).await
            .map_err(|e| format!("failed to set permissions on {path}: {e}"))?;
    }

    Ok(())
}

// ── Status reading ───────────────────────────────────────────

async fn read_ups_status(ups_name: &str) -> UpsStatus {
    let target = format!("{ups_name}@localhost");
    let output = tokio::process::Command::new("upsc")
        .arg(&target)
        .env("NUT_CONFPATH", NUT_CONF_DIR)
        .output()
        .await;

    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => {
            return UpsStatus {
                status: "UNAVAILABLE".into(),
                battery_charge: None,
                battery_runtime: None,
                input_voltage: None,
                output_voltage: None,
                ups_load: None,
                ups_model: None,
                ups_serial: None,
                available: false,
                raw: HashMap::new(),
            };
        }
    };

    let mut raw = HashMap::new();
    for line in output.lines() {
        if let Some((key, value)) = line.split_once(": ") {
            raw.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    let get_f64 = |key: &str| -> Option<f64> { raw.get(key).and_then(|v| v.parse().ok()) };
    let get_u64 = |key: &str| -> Option<u64> { raw.get(key).and_then(|v| v.parse().ok()) };
    let get_str = |key: &str| -> Option<String> {
        raw.get(key).filter(|v| !v.is_empty()).cloned()
    };

    UpsStatus {
        status: raw.get("ups.status").cloned().unwrap_or_else(|| "UNKNOWN".into()),
        battery_charge: get_f64("battery.charge"),
        battery_runtime: get_u64("battery.runtime"),
        input_voltage: get_f64("input.voltage"),
        output_voltage: get_f64("output.voltage"),
        ups_load: get_f64("ups.load"),
        ups_model: get_str("ups.model"),
        ups_serial: get_str("ups.serial"),
        available: true,
        raw,
    }
}

// ── Service control helpers ──────────────────────────────────

async fn is_nut_running() -> bool {
    tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "nut-server.service"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn restart_nut_services() {
    info!("Restarting NUT services after config change");
    for svc in &["nut-monitor.service", "nut-server.service", "nut-driver.service"] {
        let _ = tokio::process::Command::new("systemctl")
            .args(["restart", svc])
            .output()
            .await;
    }
}

// ── Persistence ──────────────────────────────────────────────

/// Load NUT config from disk (used by protocol.rs to generate config files before start).
pub async fn load_config() -> NutConfig {
    load().await
}

async fn load() -> NutConfig {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => NutConfig::default(),
    }
}

async fn save(config: &NutConfig) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all("/var/lib/nasty").await?;
    let json = serde_json::to_string_pretty(config).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}
