use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/tailscale.json";
const SYSTEMD_UNIT: &str = "nasty-tailscale";
const TAILSCALE_SOCKET: &str = "/run/tailscale/tailscaled.sock";

/// Persisted Tailscale configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TailscaleConfig {
    /// Whether Tailscale should be enabled.
    pub enabled: bool,
    /// Tailscale auth key for `tailscale up --authkey`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_key: Option<String>,
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auth_key: None,
        }
    }
}

/// Update request for Tailscale configuration.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TailscaleUpdate {
    pub enabled: Option<bool>,
    pub auth_key: Option<String>,
}

/// Live Tailscale status returned to the WebUI.
#[derive(Debug, Serialize, JsonSchema)]
pub struct TailscaleStatus {
    /// Persisted configuration.
    pub enabled: bool,
    /// Whether the tailscaled daemon is running.
    pub daemon_running: bool,
    /// Whether Tailscale is connected to the network.
    pub connected: bool,
    /// Tailscale IPv4 address (100.x.y.z).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// Tailscale hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Tailscale client version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether an auth key is configured.
    pub has_auth_key: bool,
}

pub struct TailscaleService {
    config: Arc<RwLock<TailscaleConfig>>,
}

impl TailscaleService {
    pub async fn new() -> Self {
        let config = load_config().await;
        Self {
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// Restore Tailscale state from persisted config (called at engine startup).
    pub async fn restore(&self) {
        let config = self.config.read().await.clone();
        if config.enabled {
            info!("Restoring Tailscale from persisted config");
            if let Err(e) = start_tailscale(config.auth_key.as_deref()).await {
                warn!("Failed to restore Tailscale: {e}");
            }
        }
    }

    /// Get current status (config + live state).
    pub async fn get(&self) -> TailscaleStatus {
        let config = self.config.read().await.clone();
        let daemon_running = is_daemon_running().await;

        let (connected, ip, hostname, version) = if daemon_running {
            query_status().await
        } else {
            (false, None, None, None)
        };

        TailscaleStatus {
            enabled: config.enabled,
            daemon_running,
            connected,
            ip,
            hostname,
            version,
            has_auth_key: config.auth_key.is_some(),
        }
    }

    /// Update Tailscale configuration and apply changes.
    pub async fn update(&self, update: TailscaleUpdate) -> Result<TailscaleStatus, String> {
        let mut config = self.config.write().await;

        if let Some(key) = update.auth_key {
            config.auth_key = if key.is_empty() { None } else { Some(key) };
        }

        if let Some(enabled) = update.enabled {
            if enabled && !config.enabled {
                info!("Enabling Tailscale");
                start_tailscale(config.auth_key.as_deref()).await?;
                config.enabled = true;
            } else if !enabled && config.enabled {
                info!("Disabling Tailscale");
                stop_tailscale().await?;
                config.enabled = false;
            }
        }

        save_config(&config).await.map_err(|e| format!("Failed to save config: {e}"))?;
        drop(config);

        Ok(self.get().await)
    }
}

// ── Lifecycle commands ──────────────────────────────────────────

async fn start_tailscale(auth_key: Option<&str>) -> Result<(), String> {
    // Start the daemon
    run_cmd("systemctl", &["start", SYSTEMD_UNIT]).await?;

    // Wait for the socket to appear
    for _ in 0..20 {
        if std::path::Path::new(TAILSCALE_SOCKET).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Authenticate and connect — requires an auth key.
    // Without a key, `tailscale up` blocks waiting for browser auth which hangs the engine.
    let Some(key) = auth_key else {
        info!("Tailscale daemon started but no auth key configured — skipping tailscale up");
        return Ok(());
    };

    if key.is_empty() {
        info!("Tailscale daemon started but auth key is empty — skipping tailscale up");
        return Ok(());
    }

    // Use a timeout to prevent hanging if auth key is invalid or network is unreachable
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_cmd("tailscale", &["up", "--accept-routes", "--authkey", key]),
    ).await;

    match result {
        Ok(Ok(_)) => info!("Tailscale started and connected"),
        Ok(Err(e)) => return Err(format!("tailscale up failed: {e}")),
        Err(_) => return Err("tailscale up timed out after 30s — check your auth key".to_string()),
    }

    Ok(())
}

async fn stop_tailscale() -> Result<(), String> {
    // Disconnect from network
    let _ = run_cmd("tailscale", &["down"]).await;
    // Stop the daemon
    run_cmd("systemctl", &["stop", SYSTEMD_UNIT]).await?;
    info!("Tailscale stopped");
    Ok(())
}

// ── Status queries ──────────────────────────────────────────────

async fn is_daemon_running() -> bool {
    tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", SYSTEMD_UNIT])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn query_status() -> (bool, Option<String>, Option<String>, Option<String>) {
    let output = match tokio::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return (false, None, None, None),
    };

    let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return (false, None, None, None),
    };

    // BackendState: "Running" means connected
    let connected = json["BackendState"].as_str() == Some("Running");

    // Get our own Tailscale IP from Self.TailscaleIPs[0]
    let ip = json["Self"]["TailscaleIPs"]
        .as_array()
        .and_then(|ips| ips.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Hostname from Self.HostName
    let hostname = json["Self"]["HostName"]
        .as_str()
        .map(|s| s.to_string());

    // Version from Version
    let version = json["Version"]
        .as_str()
        .map(|s| s.to_string());

    (connected, ip, hostname, version)
}

// ── Persistence ─────────────────────────────────────────────────

async fn load_config() -> TailscaleConfig {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => TailscaleConfig::default(),
    }
}

async fn save_config(config: &TailscaleConfig) -> Result<(), std::io::Error> {
    let dir = std::path::Path::new(STATE_PATH).parent().unwrap();
    tokio::fs::create_dir_all(dir).await?;
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    tokio::fs::write(STATE_PATH, json).await
}

// ── Command helper ──────────────────────────────────────────────

async fn run_cmd(program: &str, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to execute {program}: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{program} failed: {stderr}"))
    }
}
