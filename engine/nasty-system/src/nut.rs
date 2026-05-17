use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/nut.json";
const NUT_CONF_DIR: &str = "/var/lib/nasty/nut";

// ── Config structs ───────────────────────────────────────────

/// Where the UPS lives.  `Local` runs the full NUT stack
/// (driver + upsd + upsmon) against a USB/serial UPS connected to
/// this host.  `Remote` runs only upsmon and points it at an
/// already-running NUT server elsewhere (e.g. a Ubiquiti UPS, a
/// Synology, a neighbouring NASty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NutMode {
    #[default]
    Local,
    Remote,
}

/// NUT (Network UPS Tools) configuration.  In `local` mode the
/// `driver`/`port` fields describe the attached UPS; in `remote`
/// mode they're ignored and `remote_*` fields describe the upstream
/// NUT server.  `ups_name` is reused in both modes — locally it's
/// the name we expose, remotely it's the name the upstream uses.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NutConfig {
    /// Whether the UPS is attached locally or monitored over the network.
    #[serde(default)]
    pub mode: NutMode,
    /// NUT driver name (e.g. `usbhid-ups`, `blazer_usb`, `snmp-ups`).
    /// Local mode only.
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Device port. `auto` for USB auto-detection, or a path like `/dev/ttyS0`.
    /// Local mode only.
    #[serde(default = "default_port")]
    pub port: String,
    /// UPS identifier used by upsc/upsd (e.g. `ups`).
    #[serde(default = "default_ups_name")]
    pub ups_name: String,
    /// Human-readable description.  Local mode only.
    #[serde(default)]
    pub description: String,
    /// Hostname or IP of the remote NUT server.  Remote mode only.
    #[serde(default)]
    pub remote_host: String,
    /// Port the remote NUT server listens on (default 3493).  Remote mode only.
    #[serde(default = "default_remote_port")]
    pub remote_port: u16,
    /// Username configured in the remote upsd.users.  Remote mode only.
    #[serde(default)]
    pub remote_username: String,
    /// Password configured in the remote upsd.users.  Remote mode only.
    #[serde(default)]
    pub remote_password: String,
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

fn default_driver() -> String {
    "usbhid-ups".into()
}
fn default_port() -> String {
    "auto".into()
}
fn default_ups_name() -> String {
    "ups".into()
}
fn default_remote_port() -> u16 {
    3493
}
fn default_shutdown_percent() -> u32 {
    20
}
fn default_shutdown_seconds() -> u32 {
    120
}
fn default_shutdown_command() -> String {
    "/run/current-system/sw/bin/systemctl poweroff".into()
}

impl Default for NutConfig {
    fn default() -> Self {
        Self {
            mode: NutMode::default(),
            driver: default_driver(),
            port: default_port(),
            ups_name: default_ups_name(),
            description: String::new(),
            remote_host: String::new(),
            remote_port: default_remote_port(),
            remote_username: String::new(),
            remote_password: String::new(),
            shutdown_on_battery_percent: default_shutdown_percent(),
            shutdown_on_battery_seconds: default_shutdown_seconds(),
            shutdown_command: default_shutdown_command(),
        }
    }
}

/// Partial update for NUT configuration.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NutConfigUpdate {
    pub mode: Option<NutMode>,
    pub driver: Option<String>,
    pub port: Option<String>,
    pub ups_name: Option<String>,
    pub description: Option<String>,
    pub remote_host: Option<String>,
    pub remote_port: Option<u16>,
    pub remote_username: Option<String>,
    pub remote_password: Option<String>,
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

        if let Some(v) = update.mode {
            config.mode = v;
        }
        if let Some(v) = update.driver {
            config.driver = v;
        }
        if let Some(v) = update.port {
            config.port = v;
        }
        if let Some(v) = update.ups_name {
            if v.is_empty() {
                return Err("ups_name cannot be empty".into());
            }
            config.ups_name = v;
        }
        if let Some(v) = update.description {
            config.description = v;
        }
        if let Some(v) = update.remote_host {
            config.remote_host = v;
        }
        if let Some(v) = update.remote_port {
            config.remote_port = v;
        }
        if let Some(v) = update.remote_username {
            config.remote_username = v;
        }
        if let Some(v) = update.remote_password {
            config.remote_password = v;
        }
        if let Some(v) = update.shutdown_on_battery_percent {
            if v > 100 {
                return Err("shutdown_on_battery_percent must be 0-100".into());
            }
            config.shutdown_on_battery_percent = v;
        }
        if let Some(v) = update.shutdown_on_battery_seconds {
            config.shutdown_on_battery_seconds = v;
        }
        if let Some(v) = update.shutdown_command {
            config.shutdown_command = v;
        }

        if config.mode == NutMode::Remote && config.remote_host.is_empty() {
            return Err("remote_host is required when mode = remote".into());
        }
        let new_mode = config.mode;

        save(&config).await.map_err(|e| e.to_string())?;

        // Regenerate config files so next restart picks them up.
        // If NUT is currently running, restart it.
        if let Err(e) = write_config_files(&config).await {
            warn!("Failed to write NUT config files: {e}");
        }
        if is_nut_enabled().await {
            // Spawn restart in background — some drivers (nutdrv_qx) take 20-30s
            // to probe USB and we don't want the API call to block/timeout.
            // restart_nut_services() logs per-service errors itself; the spawn
            // wrapper just guards against a task-panic vanishing into nothing.
            let h = tokio::spawn(async move { reconcile_nut_services(new_mode).await });
            tokio::spawn(async move {
                if let Err(e) = h.await {
                    warn!("NUT restart task panicked / cancelled: {e}");
                }
            });
        }

        Ok(config.clone())
    }

    /// Read live UPS status via `upsc`.  In local mode talks to the
    /// local upsd; in remote mode talks to the upstream NUT server.
    pub async fn status(&self) -> UpsStatus {
        let config = self.state.read().await;
        let target = match config.mode {
            NutMode::Local => format!("{}@localhost", config.ups_name),
            NutMode::Remote => format!(
                "{}@{}:{}",
                config.ups_name, config.remote_host, config.remote_port
            ),
        };
        read_ups_status(&target).await
    }

    /// Write NUT config files. Called before starting services.
    pub async fn write_configs(&self) -> Result<(), String> {
        let config = self.state.read().await;
        write_config_files(&config).await
    }
}

// ── Config file generation ───────────────────────────────────

pub async fn write_config_files(config: &NutConfig) -> Result<(), String> {
    tokio::fs::create_dir_all(NUT_CONF_DIR)
        .await
        .map_err(|e| format!("failed to create {NUT_CONF_DIR}: {e}"))?;

    match config.mode {
        NutMode::Local => write_local_configs(config).await?,
        NutMode::Remote => write_remote_configs(config).await?,
    }

    info!(
        "NUT config files written to {NUT_CONF_DIR} (mode={:?})",
        config.mode
    );
    Ok(())
}

async fn write_local_configs(config: &NutConfig) -> Result<(), String> {
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
    Ok(())
}

async fn write_remote_configs(config: &NutConfig) -> Result<(), String> {
    // In remote mode we don't run upsd or any driver — only upsmon,
    // pointed at the upstream NUT server in `secondary` role
    // (modern NUT name; replaces the deprecated `slave`).  Credentials
    // come from the remote upsd.users; we never see them here.
    //
    // Stale ups.conf/upsd.conf/upsd.users from a prior local-mode
    // session are left in place — nut-driver and nut-server aren't
    // started in remote mode, so they don't get read.
    let upsmon_conf = format!(
        concat!(
            "MONITOR {name}@{host}:{port} 1 {user} {pass} secondary\n",
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
        host = config.remote_host,
        port = config.remote_port,
        user = config.remote_username,
        pass = config.remote_password,
        cmd = config.shutdown_command,
    );
    write_file("upsmon.conf", &upsmon_conf).await?;
    Ok(())
}

async fn write_file(name: &str, content: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let path = format!("{NUT_CONF_DIR}/{name}");
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| format!("failed to write {path}: {e}"))?;

    // upsd.users (local upsd creds) and upsmon.conf (the MONITOR line
    // embeds either local upsd creds or the remote NUT server's creds)
    // both contain plaintext passwords — restrict to owner+group.
    if name == "upsd.users" || name == "upsmon.conf" {
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .await
            .map_err(|e| format!("failed to set permissions on {path}: {e}"))?;
    }

    Ok(())
}

// ── Status reading ───────────────────────────────────────────

async fn read_ups_status(target: &str) -> UpsStatus {
    let output = tokio::process::Command::new("upsc")
        .arg(target)
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
    let get_str = |key: &str| -> Option<String> { raw.get(key).filter(|v| !v.is_empty()).cloned() };

    UpsStatus {
        status: raw
            .get("ups.status")
            .cloned()
            .unwrap_or_else(|| "UNKNOWN".into()),
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

/// All systemd units the NUT stack could ever own.  The actual set
/// that *should* run depends on mode — see [`services_for_mode`].
const ALL_NUT_SERVICES: &[&str] = &[
    "nut-driver.service",
    "nut-server.service",
    "nut-monitor.service",
];

/// Which systemd units should be running for a given mode.
/// Used by protocol.rs at start/stop time and by [`reconcile_nut_services`]
/// after a config change.
pub fn services_for_mode(mode: NutMode) -> &'static [&'static str] {
    match mode {
        NutMode::Local => ALL_NUT_SERVICES,
        NutMode::Remote => &["nut-monitor.service"],
    }
}

/// Read mode from disk synchronously (blocking).  Called from
/// protocol.rs, which needs the unit list before it can await
/// anything.  Falls back to Local on any error so a missing or
/// corrupt nut.json doesn't disable monitoring entirely.
pub fn mode_sync() -> NutMode {
    match std::fs::read_to_string(STATE_PATH) {
        Ok(content) => serde_json::from_str::<NutConfig>(&content)
            .map(|c| c.mode)
            .unwrap_or_default(),
        Err(_) => NutMode::default(),
    }
}

/// The systemd unit whose "active" state tells us the NUT protocol
/// is healthy in the current mode.  upsd is local-only; upsmon runs
/// in both modes and is the right canary.
pub fn status_unit(mode: NutMode) -> &'static str {
    match mode {
        NutMode::Local => "nut-server.service",
        NutMode::Remote => "nut-monitor.service",
    }
}

async fn is_nut_enabled() -> bool {
    // "Any NUT-stack service is currently active" — used to decide
    // whether to restart after a config update.  We can't just check
    // one specific unit because the set depends on mode and the
    // user might be flipping that very setting.
    for svc in ALL_NUT_SERVICES {
        let ok = tokio::process::Command::new("systemctl")
            .args(["is-active", "--quiet", svc])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

async fn reconcile_nut_services(mode: NutMode) {
    let want: std::collections::HashSet<&str> = services_for_mode(mode).iter().copied().collect();
    info!("Reconciling NUT services for mode={mode:?}: want {want:?}");

    // Stop any service that should NOT be running in this mode.
    for svc in ALL_NUT_SERVICES {
        if want.contains(svc) {
            continue;
        }
        match tokio::process::Command::new("systemctl")
            .args(["stop", svc])
            .output()
            .await
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => warn!(
                "systemctl stop {svc} exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => warn!("systemctl stop {svc} failed to spawn: {e}"),
        }
    }

    // Restart (or start) every service that should be running.
    for svc in services_for_mode(mode) {
        match tokio::process::Command::new("systemctl")
            .args(["restart", svc])
            .output()
            .await
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => warn!(
                "systemctl restart {svc} exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => warn!("systemctl restart {svc} failed to spawn: {e}"),
        }
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
