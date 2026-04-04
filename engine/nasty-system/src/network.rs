//! Network configuration management.
//!
//! Persists user-configured settings to `/var/lib/nasty/networking.json`
//! and generates `/etc/nixos/networking.nix` for NixOS persistence.
//! Changes are applied immediately via `ip` commands without a full nixos-rebuild.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::warn;

const JSON_PATH: &str = "/var/lib/nasty/networking.json";
const NIX_PATH: &str = "/etc/nixos/networking.nix";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkConfig {
    /// Whether DHCP is enabled; if false, static address/gateway are used.
    pub dhcp: bool,
    /// Network interface name to configure (e.g. `eth0`). Auto-detected if empty.
    #[serde(default)]
    pub interface: String,
    /// Static IPv4 address (required when `dhcp` is false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Subnet prefix length, e.g. `24` for a /24 (required when `dhcp` is false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_length: Option<u8>,
    /// Default gateway IPv4 address (required when `dhcp` is false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// DNS nameserver addresses written to `/etc/resolv.conf`.
    #[serde(default)]
    pub nameservers: Vec<String>,
    // Live state — populated at read time, ignored on write
    /// Currently assigned addresses on the interface in CIDR notation (read-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_addresses: Vec<String>,
    /// Currently active default gateway (read-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_gateway: Option<String>,
}

pub struct NetworkService;

impl NetworkService {
    pub fn new() -> Self {
        Self
    }

    pub async fn get(&self) -> NetworkConfig {
        let mut config = load_config().await;
        // Populate live state
        let iface = if config.interface.is_empty() {
            detect_primary_interface().await.unwrap_or_default()
        } else {
            config.interface.clone()
        };
        config.live_addresses = live_addresses(&iface).await;
        config.live_gateway = live_gateway().await;
        config
    }

    pub async fn update(&self, mut config: NetworkConfig) -> Result<(), String> {
        if !config.dhcp {
            if config.address.is_none() || config.prefix_length.is_none() || config.gateway.is_none() {
                return Err("Static mode requires address, prefix_length, and gateway".into());
            }
        }

        // Auto-detect interface if not provided
        if config.interface.is_empty() {
            config.interface = detect_primary_interface().await
                .ok_or_else(|| "Could not detect network interface".to_string())?;
        }

        // Clear live fields before persisting
        config.live_addresses = Vec::new();
        config.live_gateway = None;

        // Persist JSON
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("serialization error: {e}"))?;
        tokio::fs::write(JSON_PATH, &json).await
            .map_err(|e| format!("failed to write {JSON_PATH}: {e}"))?;

        // Generate and persist networking.nix
        let nix = generate_nix(&config);
        if let Err(e) = tokio::fs::write(NIX_PATH, &nix).await {
            warn!("Failed to write {NIX_PATH}: {e} — config saved but won't persist across rebuilds");
        }

        // Apply immediately
        apply_config(&config).await?;

        Ok(())
    }
}

async fn load_config() -> NetworkConfig {
    match tokio::fs::read_to_string(JSON_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            warn!("Failed to parse {JSON_PATH}: {e}");
            default_config()
        }),
        Err(_) => default_config(),
    }
}

fn default_config() -> NetworkConfig {
    NetworkConfig {
        dhcp: true,
        interface: String::new(),
        address: None,
        prefix_length: None,
        gateway: None,
        nameservers: Vec::new(),
        live_addresses: Vec::new(),
        live_gateway: None,
    }
}

pub async fn detect_primary_interface() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-4", "route", "get", "1.1.1.1"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut iter = text.split_whitespace();
    while let Some(token) = iter.next() {
        if token == "dev" {
            return iter.next().map(|s| s.to_string());
        }
    }
    None
}

async fn live_addresses(iface: &str) -> Vec<String> {
    if iface.is_empty() {
        return Vec::new();
    }
    let Ok(output) = tokio::process::Command::new("ip")
        .args(["-4", "addr", "show", iface])
        .output()
        .await
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("inet ") {
                line.split_whitespace().nth(1).map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect()
}

async fn live_gateway() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut iter = text.split_whitespace();
    while let Some(token) = iter.next() {
        if token == "via" {
            return iter.next().map(|s| s.to_string());
        }
    }
    None
}

fn generate_nix(config: &NetworkConfig) -> String {
    let mut out = String::from(
        "# Managed by NASty — edit via WebUI Settings > Network\n{ ... }:\n{\n",
    );

    if config.dhcp {
        out.push_str("  networking.useDHCP = true;\n");
    } else {
        let address = config.address.as_deref().unwrap_or("");
        let prefix = config.prefix_length.unwrap_or(24);
        let gateway = config.gateway.as_deref().unwrap_or("");
        let iface = &config.interface;

        out.push_str("  networking.useDHCP = false;\n");
        out.push_str(&format!(
            "  networking.interfaces.{iface}.ipv4.addresses = \
             [{{ address = \"{address}\"; prefixLength = {prefix}; }}];\n"
        ));
        out.push_str(&format!("  networking.defaultGateway = \"{gateway}\";\n"));

        if !config.nameservers.is_empty() {
            let items: Vec<String> = config.nameservers.iter()
                .map(|ns| format!("\"{ns}\""))
                .collect();
            out.push_str(&format!("  networking.nameservers = [ {} ];\n", items.join(" ")));
        }
    }

    out.push_str("}\n");
    out
}

async fn apply_config(config: &NetworkConfig) -> Result<(), String> {
    let iface = &config.interface;

    if config.dhcp {
        // Restart dhcpcd to re-acquire DHCP lease on all interfaces
        let status = tokio::process::Command::new("systemctl")
            .args(["restart", "dhcpcd"])
            .status()
            .await
            .map_err(|e| format!("failed to restart dhcpcd: {e}"))?;
        if !status.success() {
            warn!("dhcpcd restart returned non-zero; DHCP may not be active immediately");
        }
    } else {
        let address = config.address.as_deref().unwrap_or("");
        let prefix = config.prefix_length.unwrap_or(24);
        let gateway = config.gateway.as_deref().unwrap_or("");
        let cidr = format!("{address}/{prefix}");

        // Flush existing addresses, assign new one, bring interface up
        run_ip(&["addr", "flush", "dev", iface]).await
            .map_err(|e| format!("ip addr flush: {e}"))?;
        run_ip(&["addr", "add", &cidr, "dev", iface]).await
            .map_err(|e| format!("ip addr add: {e}"))?;
        run_ip(&["link", "set", iface, "up"]).await
            .map_err(|e| format!("ip link set up: {e}"))?;
        run_ip(&["route", "replace", "default", "via", gateway, "dev", iface]).await
            .map_err(|e| format!("ip route replace: {e}"))?;

        // Write resolv.conf if nameservers specified
        if !config.nameservers.is_empty() {
            let resolv: String = config.nameservers.iter()
                .map(|ns| format!("nameserver {ns}\n"))
                .collect();
            tokio::fs::write("/etc/resolv.conf", resolv).await
                .map_err(|e| format!("failed to write /etc/resolv.conf: {e}"))?;
        }
    }

    Ok(())
}

async fn run_ip(args: &[&str]) -> Result<(), String> {
    let status = tokio::process::Command::new("ip")
        .args(args)
        .status()
        .await
        .map_err(|e| format!("failed to run ip: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ip {} exited with non-zero status", args.join(" ")))
    }
}
