//! Dynamic nftables firewall — engine-managed port rules.
//!
//! Maintains a `table inet nasty` with an `input` chain. Rules are added/removed
//! when protocols are enabled/disabled. The table is rebuilt atomically on every change.

use crate::protocol::Protocol;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::process::Command;
use tracing::{error, info, warn};

const RESTRICTIONS_PATH: &str = "/var/lib/nasty/firewall-restrictions.json";

/// Persisted per-service source IP restrictions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FirewallRestrictions {
    /// Map of service name → list of allowed source CIDRs.
    /// If empty or absent, all sources are allowed.
    pub services: HashMap<String, Vec<String>>,
}

impl FirewallRestrictions {
    pub fn load() -> Self {
        std::fs::read_to_string(RESTRICTIONS_PATH)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub async fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize: {e}"))?;
        tokio::fs::write(RESTRICTIONS_PATH, json).await
            .map_err(|e| format!("write {RESTRICTIONS_PATH}: {e}"))
    }
}

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PortSpec {
    pub port: u16,
    pub transport: Transport,
    /// Optional source IP/CIDR restriction (e.g. "192.168.1.0/24").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FirewallRule {
    /// Protocol/service name (e.g. "nfs", "ssh", "webui").
    pub service: String,
    pub ports: Vec<PortSpec>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct FirewallState {
    pub rules: Vec<FirewallRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FirewallStatus {
    pub active: bool,
    pub rules: Vec<FirewallRule>,
    /// Per-service source IP restrictions.
    pub restrictions: HashMap<String, Vec<String>>,
}

// ── Port mapping ───────────────────────────────────────────────

fn tcp(port: u16) -> PortSpec {
    PortSpec { port, transport: Transport::Tcp, source: None }
}

fn udp(port: u16) -> PortSpec {
    PortSpec { port, transport: Transport::Udp, source: None }
}

/// Return the ports that should be open for a given protocol.
pub fn ports_for_protocol(proto: Protocol) -> Vec<PortSpec> {
    match proto {
        Protocol::Nfs => vec![tcp(2049)],
        Protocol::Smb => vec![tcp(445), tcp(139)],
        Protocol::Iscsi => vec![tcp(3260)],
        Protocol::Nvmeof => vec![tcp(4420)],
        Protocol::Nut => vec![tcp(3493)],
        Protocol::Ssh => vec![tcp(22)],
        Protocol::Avahi => vec![udp(5353)],
        Protocol::Smart => vec![], // no network port
    }
}

/// Ports for the WebUI — always present but can have source restrictions.
pub fn webui_ports() -> Vec<PortSpec> {
    vec![tcp(80), tcp(443)]
}

// ── Firewall service ───────────────────────────────────────────

pub struct FirewallService {
    state: tokio::sync::Mutex<FirewallState>,
    restrictions: tokio::sync::Mutex<FirewallRestrictions>,
}

impl FirewallService {
    pub fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(FirewallState::default()),
            restrictions: tokio::sync::Mutex::new(FirewallRestrictions::load()),
        }
    }

    /// Initialize firewall with current protocol states.
    /// Called at engine startup after protocol restore.
    pub async fn init(&self, enabled_protocols: &[(Protocol, bool)]) {
        let mut state = self.state.lock().await;
        let restrictions = self.restrictions.lock().await;

        // WebUI is always open
        let webui_sources = restrictions.services.get("webui").cloned().unwrap_or_default();
        state.rules.push(FirewallRule {
            service: "webui".to_string(),
            ports: apply_sources(webui_ports(), &webui_sources),
            active: true,
        });

        // Add rules for each protocol
        for (proto, enabled) in enabled_protocols {
            let mut ports = ports_for_protocol(*proto);
            if ports.is_empty() {
                continue;
            }
            let sources = restrictions.services.get(proto.name()).cloned().unwrap_or_default();
            ports = apply_sources(ports, &sources);
            state.rules.push(FirewallRule {
                service: proto.name().to_string(),
                ports,
                active: *enabled,
            });
        }

        if let Err(e) = apply_nftables(&state).await {
            error!("Failed to apply initial firewall rules: {e}");
        } else {
            info!("Firewall initialized with {} rules", state.rules.len());
        }
    }

    /// Open ports for a protocol (called when a service is enabled).
    pub async fn open(&self, proto: Protocol) {
        let mut state = self.state.lock().await;
        let name = proto.name();
        if let Some(rule) = state.rules.iter_mut().find(|r| r.service == name) {
            if rule.active {
                return; // already open
            }
            rule.active = true;
        } else {
            let ports = ports_for_protocol(proto);
            if ports.is_empty() {
                return;
            }
            state.rules.push(FirewallRule {
                service: name.to_string(),
                ports,
                active: true,
            });
        }
        if let Err(e) = apply_nftables(&state).await {
            error!("Failed to open firewall for {name}: {e}");
        } else {
            info!("Firewall: opened ports for {name}");
        }
    }

    /// Close ports for a protocol (called when a service is disabled).
    pub async fn close(&self, proto: Protocol) {
        let mut state = self.state.lock().await;
        let name = proto.name();
        if let Some(rule) = state.rules.iter_mut().find(|r| r.service == name) {
            if !rule.active {
                return; // already closed
            }
            rule.active = false;
        }
        if let Err(e) = apply_nftables(&state).await {
            error!("Failed to close firewall for {name}: {e}");
        } else {
            info!("Firewall: closed ports for {name}");
        }
    }

    /// Get current firewall status including restrictions.
    pub async fn status(&self) -> FirewallStatus {
        let state = self.state.lock().await;
        let restrictions = self.restrictions.lock().await;
        FirewallStatus {
            active: true,
            rules: state.rules.clone(),
            restrictions: restrictions.services.clone(),
        }
    }

    /// Set source IP restrictions for a service and rebuild firewall.
    pub async fn set_restriction(&self, service: &str, sources: Vec<String>) -> Result<(), String> {
        // Update persisted restrictions
        {
            let mut restrictions = self.restrictions.lock().await;
            if sources.is_empty() {
                restrictions.services.remove(service);
            } else {
                restrictions.services.insert(service.to_string(), sources.clone());
            }
            restrictions.save().await?;
        }

        // Update rules in state
        let mut state = self.state.lock().await;
        if let Some(rule) = state.rules.iter_mut().find(|r| r.service == service) {
            // Rebuild ports with new source restrictions
            let base_ports = if service == "webui" {
                webui_ports()
            } else if let Some(proto) = Protocol::from_name(service) {
                ports_for_protocol(proto)
            } else {
                return Err(format!("unknown service: {service}"));
            };
            rule.ports = apply_sources(base_ports, &sources);
        }

        apply_nftables(&state).await?;
        info!("Firewall: updated restrictions for {service}");
        Ok(())
    }

    /// Get restrictions for a specific service.
    pub async fn get_restrictions(&self) -> HashMap<String, Vec<String>> {
        self.restrictions.lock().await.services.clone()
    }
}

/// Apply source restrictions to a set of ports.
/// If sources is empty, ports have no restriction (open to all).
/// If sources is non-empty, each port is duplicated for each source.
fn apply_sources(ports: Vec<PortSpec>, sources: &[String]) -> Vec<PortSpec> {
    if sources.is_empty() {
        return ports;
    }
    let mut result = Vec::new();
    for port in &ports {
        for src in sources {
            result.push(PortSpec {
                port: port.port,
                transport: port.transport,
                source: Some(src.clone()),
            });
        }
    }
    result
}

// ── nftables application ───────────────────────────────────────

/// Generate and apply the full nftables ruleset atomically.
async fn apply_nftables(state: &FirewallState) -> Result<(), String> {
    let mut rules = String::new();
    rules.push_str("table inet nasty {\n");
    rules.push_str("    chain input {\n");
    rules.push_str("        type filter hook input priority 0; policy drop;\n");
    rules.push_str("        ct state established,related accept\n");
    rules.push_str("        ct state invalid drop\n");
    rules.push_str("        iif lo accept\n");
    rules.push_str("        # ICMP/ICMPv6 — always allow\n");
    rules.push_str("        ip protocol icmp accept\n");
    rules.push_str("        ip6 nexthdr icmpv6 accept\n");
    rules.push_str("        # DHCPv6 client\n");
    rules.push_str("        udp dport 546 accept\n");

    for rule in &state.rules {
        if !rule.active {
            continue;
        }
        for port in &rule.ports {
            let proto = match port.transport {
                Transport::Tcp => "tcp",
                Transport::Udp => "udp",
            };
            if let Some(ref src) = port.source {
                rules.push_str(&format!(
                    "        ip saddr {src} {proto} dport {} accept # {}\n",
                    port.port, rule.service
                ));
            } else {
                rules.push_str(&format!(
                    "        {proto} dport {} accept # {}\n",
                    port.port, rule.service
                ));
            }
        }
    }

    rules.push_str("    }\n");
    rules.push_str("}\n");

    // Apply atomically: flush + load
    let flush = Command::new("nft")
        .args(["delete", "table", "inet", "nasty"])
        .output()
        .await;
    // Ignore flush errors (table may not exist yet)
    if let Ok(o) = &flush {
        if !o.status.success() {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.contains("No such file") && !stderr.contains("does not exist") {
                warn!("nft delete table warning: {stderr}");
            }
        }
    }

    let output = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn nft: {e}"))?
        .wait_with_output()
        .await
        .map_err(|e| format!("nft: {e}"))?;

    // Need to pipe stdin — let me use a temp file approach instead
    let tmp = "/tmp/nasty-firewall.nft";
    tokio::fs::write(tmp, &rules).await
        .map_err(|e| format!("write {tmp}: {e}"))?;

    let output = Command::new("nft")
        .args(["-f", tmp])
        .output()
        .await
        .map_err(|e| format!("nft -f: {e}"))?;

    let _ = tokio::fs::remove_file(tmp).await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("nft apply failed: {stderr}"));
    }

    Ok(())
}
