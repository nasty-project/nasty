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

/// Persisted per-service access restrictions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FirewallRestrictions {
    /// Map of service name → list of allowed source CIDRs.
    /// If empty or absent, all sources are allowed.
    #[serde(default)]
    pub services: HashMap<String, Vec<String>>,
    /// Map of service name → list of allowed interfaces.
    /// If empty or absent, all interfaces are accepted.
    #[serde(default)]
    pub interfaces: HashMap<String, Vec<String>>,
}

impl FirewallRestrictions {
    pub fn load() -> Self {
        std::fs::read_to_string(RESTRICTIONS_PATH)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub async fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        tokio::fs::write(RESTRICTIONS_PATH, json)
            .await
            .map_err(|e| format!("write {RESTRICTIONS_PATH}: {e}"))
    }

    /// Drop every reference to interfaces in `removed` from this
    /// restrictions config. Keeps the firewall in sync when an iface
    /// disappears from networking.json — without this, dangling rules
    /// would point at a now-nonexistent iface that the WebUI can't
    /// unselect because the dropdown source no longer offers it.
    /// Returns true when the config changed — caller decides whether
    /// to persist.
    pub fn strip_iface_refs(&mut self, removed: &[String]) -> bool {
        if removed.is_empty() || self.interfaces.is_empty() {
            return false;
        }
        let drop: std::collections::HashSet<&str> = removed.iter().map(|s| s.as_str()).collect();
        let mut changed = false;
        // Per-service iface lists: filter out removed names. Service
        // entries that drop to empty are themselves removed (empty
        // means "no restriction" — same as not being in the map).
        self.interfaces.retain(|_service, ifaces| {
            let before = ifaces.len();
            ifaces.retain(|iface| !drop.contains(iface.as_str()));
            if ifaces.len() != before {
                changed = true;
            }
            !ifaces.is_empty()
        });
        changed
    }
}

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortSpec {
    pub port: u16,
    pub transport: Transport,
    /// Optional source IP/CIDR restriction (e.g. "192.168.1.0/24").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional interface restriction (e.g. "tailscale0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
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

/// A user-managed firewall port rule (issue #620). Opens a single TCP/UDP
/// port or a contiguous range on the host `input` chain, independent of
/// NASty's service model. Persisted to `firewall-custom.json` and rendered
/// into `table inet nasty` alongside the service rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CustomRule {
    /// Engine-generated opaque id (UUID). Stable across label edits; used as
    /// the nft comment so free-text never enters the ruleset.
    pub id: String,
    /// Required human label ("Plex (host mode)"). UI only.
    pub label: String,
    pub transport: Transport,
    /// Low port of the range (== `to` for a single port).
    pub from: u16,
    /// High port of the range.
    pub to: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    pub enabled: bool,
}

const CUSTOM_PATH: &str = "/var/lib/nasty/firewall-custom.json";

/// Load persisted custom rules; empty on missing/corrupt file (same
/// tolerance as `FirewallRestrictions::load`).
fn load_custom_rules() -> Vec<CustomRule> {
    std::fs::read_to_string(CUSTOM_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

async fn save_custom_rules(rules: &[CustomRule]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(rules).map_err(|e| format!("serialize: {e}"))?;
    tokio::fs::write(CUSTOM_PATH, json)
        .await
        .map_err(|e| format!("write {CUSTOM_PATH}: {e}"))
}

fn default_true() -> bool {
    true
}

/// Fields a client sends to create/update a custom rule (no `id` — the
/// engine assigns it on create and preserves it on update).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CustomRuleInput {
    pub label: String,
    pub transport: Transport,
    pub from: u16,
    pub to: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Reject a bare IP or CIDR that isn't parseable, so nothing unsafe reaches
/// the `ip saddr <...>` interpolation. Accepts "10.0.0.5", "10.0.0.0/8",
/// "2001:db8::/32".
fn valid_source(s: &str) -> bool {
    use std::net::IpAddr;
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (s, None),
    };
    let ip: IpAddr = match addr.parse() {
        Ok(ip) => ip,
        Err(_) => return false,
    };
    match prefix {
        None => true,
        Some(p) => match p.parse::<u8>() {
            Ok(bits) => match ip {
                IpAddr::V4(_) => bits <= 32,
                IpAddr::V6(_) => bits <= 128,
            },
            Err(_) => false,
        },
    }
}

/// Interface names: Linux IFNAMSIZ is 16 (15 usable chars). Restrict to a
/// safe charset so nothing breaks out of the `iifname "<...>"` string.
fn valid_iface(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 15
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// Validate a custom-rule input independent of current firewall state:
/// range sanity, label hygiene, and source/iface sanitization.
pub fn validate_custom_input(input: &CustomRuleInput) -> Result<(), String> {
    if input.from == 0 {
        return Err("port must be ≥ 1".into());
    }
    if input.from > input.to {
        return Err("range start must be ≤ range end".into());
    }
    let label = input.label.trim();
    if label.is_empty() {
        return Err("label is required".into());
    }
    if label.len() > 64 {
        return Err("label must be ≤ 64 characters".into());
    }
    if input.label.chars().any(|c| c.is_control()) {
        return Err("label must not contain control characters".into());
    }
    if let Some(src) = &input.source
        && !valid_source(src)
    {
        return Err(format!("invalid source (expected an IP or CIDR): {src}"));
    }
    if let Some(iface) = &input.iface
        && !valid_iface(iface)
    {
        return Err(format!("invalid interface name: {iface}"));
    }
    Ok(())
}

/// If `[from,to]` (of `transport`) intersects any port a service rule owns,
/// return that service's name. Covers active AND inactive service rules —
/// a disabled service still owns its port.
pub fn service_port_conflict(
    state: &FirewallState,
    transport: Transport,
    from: u16,
    to: u16,
) -> Option<String> {
    for rule in &state.rules {
        for port in &rule.ports {
            if port.transport == transport && port.port >= from && port.port <= to {
                return Some(rule.service.clone());
            }
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FirewallStatus {
    pub active: bool,
    pub rules: Vec<FirewallRule>,
    /// Per-service source IP restrictions.
    pub restrictions: HashMap<String, Vec<String>>,
    /// Per-service interface restrictions.
    pub interface_restrictions: HashMap<String, Vec<String>>,
    /// Ports that Docker-managed apps publish on the host. These are NOT
    /// governed by this firewall — Docker DNATs published ports in
    /// `prerouting` straight to the container, so they bypass the `inet
    /// nasty` input chain entirely. Listed here for visibility only, so an
    /// operator sees the full "what's open on this box" picture in one
    /// place; their only real gate is the upstream/cloud firewall. The
    /// engine layer fills this from `apps.list`; the firewall module
    /// itself has no knowledge of Docker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published_app_ports: Vec<PublishedAppPort>,
    /// User-managed custom port rules (issue #620). Rendered into the
    /// firewall alongside service rules; editable on the Firewall page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_rules: Vec<CustomRule>,
}

/// One host port published by a Docker-managed app. Read-only; surfaced
/// on the firewall page alongside the service rules. See
/// [`FirewallStatus::published_app_ports`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PublishedAppPort {
    /// App that published the port.
    pub app: String,
    /// Host-side port (bound on 0.0.0.0).
    pub host_port: u16,
    /// Container-side port the host port maps to.
    pub container_port: u16,
    /// Transport ("tcp" / "udp").
    pub transport: String,
}

// ── Port mapping ───────────────────────────────────────────────

fn tcp(port: u16) -> PortSpec {
    PortSpec {
        port,
        transport: Transport::Tcp,
        source: None,
        iface: None,
    }
}

fn udp(port: u16) -> PortSpec {
    PortSpec {
        port,
        transport: Transport::Udp,
        source: None,
        iface: None,
    }
}

/// Return the ports that should be open for a given protocol.
pub fn ports_for_protocol(proto: Protocol) -> Vec<PortSpec> {
    match proto {
        Protocol::Nfs => vec![tcp(2049)],
        // 445/139: Samba serving. 3702/udp: WSDD announcements for
        // Windows 10/11 Explorer discovery (samba-wsdd.service).
        Protocol::Smb => vec![tcp(445), tcp(139), udp(3702)],
        Protocol::Iscsi => vec![tcp(3260)],
        Protocol::Nvmeof => vec![tcp(4420)],
        Protocol::Nut => vec![tcp(3493)],
        Protocol::Ssh => vec![tcp(22)],
        Protocol::Avahi => vec![udp(5353)],
        Protocol::Smart => vec![], // no network port
        Protocol::RestServer => vec![tcp(8000)],
    }
}

/// Ports for the WebUI — always present but can have source restrictions.
pub fn webui_ports() -> Vec<PortSpec> {
    vec![tcp(80), tcp(443)]
}

/// Ports for the RDMA share transports (per-box opt-in, #602).
/// udp/4791 is RoCEv2's encapsulation port — ALL RoCE traffic
/// (NVMe/RDMA "4420", NFS-RDMA "20049", iSER) rides inside it; those
/// numbers are RDMA-CM service ids, not IP ports. tcp/20049 covers
/// iWARP NFS-RDMA. Native InfiniBand never traverses netfilter — no
/// rule is needed or possible. Consequence for operators: per-service
/// source restrictions on RoCE can only filter at 4791 granularity.
pub fn rdma_ports() -> Vec<PortSpec> {
    vec![udp(4791), tcp(20049)]
}

// ── Firewall service ───────────────────────────────────────────

pub struct FirewallService {
    state: tokio::sync::Mutex<FirewallState>,
    restrictions: tokio::sync::Mutex<FirewallRestrictions>,
    custom: tokio::sync::Mutex<Vec<CustomRule>>,
}

impl Default for FirewallService {
    fn default() -> Self {
        Self::new()
    }
}

impl FirewallService {
    pub fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(FirewallState::default()),
            restrictions: tokio::sync::Mutex::new(FirewallRestrictions::default()),
            custom: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Initialize firewall with current protocol states.
    /// Called at engine startup after protocol restore.
    pub async fn init(&self, enabled_protocols: &[(Protocol, bool)]) {
        let mut state = self.state.lock().await;
        let mut restrictions = self.restrictions.lock().await;
        *restrictions = FirewallRestrictions::load();
        let mut custom = self.custom.lock().await;
        *custom = load_custom_rules();

        // WebUI is always open
        let webui_sources = restrictions
            .services
            .get("webui")
            .cloned()
            .unwrap_or_default();
        let webui_ifaces = restrictions
            .interfaces
            .get("webui")
            .cloned()
            .unwrap_or_default();
        state.rules.push(FirewallRule {
            service: "webui".to_string(),
            ports: apply_restrictions(webui_ports(), &webui_sources, &webui_ifaces),
            active: true,
        });

        // Add rules for each protocol
        for (proto, enabled) in enabled_protocols {
            let mut ports = ports_for_protocol(*proto);
            if ports.is_empty() {
                continue;
            }
            let sources = restrictions
                .services
                .get(proto.name())
                .cloned()
                .unwrap_or_default();
            let ifaces = restrictions
                .interfaces
                .get(proto.name())
                .cloned()
                .unwrap_or_default();
            ports = apply_restrictions(ports, &sources, &ifaces);
            state.rules.push(FirewallRule {
                service: proto.name().to_string(),
                ports,
                active: *enabled,
            });
        }

        if let Err(e) = apply_nftables(&state, &custom).await {
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
        let custom = self.custom.lock().await;
        if let Err(e) = apply_nftables(&state, &custom).await {
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
        let custom = self.custom.lock().await;
        if let Err(e) = apply_nftables(&state, &custom).await {
            error!("Failed to close firewall for {name}: {e}");
        } else {
            info!("Firewall: closed ports for {name}");
        }
    }

    /// Open the named RDMA transport rule (per-box opt-in, #602).
    pub async fn open_rdma(&self) {
        let mut state = self.state.lock().await;
        if let Some(rule) = state.rules.iter_mut().find(|r| r.service == "rdma") {
            if rule.active {
                return;
            }
            rule.active = true;
        } else {
            state.rules.push(FirewallRule {
                service: "rdma".to_string(),
                ports: rdma_ports(),
                active: true,
            });
        }
        let custom = self.custom.lock().await;
        if let Err(e) = apply_nftables(&state, &custom).await {
            error!("Failed to open firewall for rdma: {e}");
        } else {
            info!("Firewall: opened ports for rdma");
        }
    }

    /// Close the named RDMA transport rule.
    pub async fn close_rdma(&self) {
        let mut state = self.state.lock().await;
        if let Some(rule) = state.rules.iter_mut().find(|r| r.service == "rdma") {
            if !rule.active {
                return;
            }
            rule.active = false;
        }
        let custom = self.custom.lock().await;
        if let Err(e) = apply_nftables(&state, &custom).await {
            error!("Failed to close firewall for rdma: {e}");
        } else {
            info!("Firewall: closed ports for rdma");
        }
    }

    /// Get current firewall status including restrictions.
    pub async fn status(&self) -> FirewallStatus {
        let state = self.state.lock().await;
        let restrictions = self.restrictions.lock().await;
        let custom = self.custom.lock().await;
        FirewallStatus {
            active: true,
            rules: state.rules.clone(),
            restrictions: restrictions.services.clone(),
            interface_restrictions: restrictions.interfaces.clone(),
            // Populated by the engine layer (router) which has the apps
            // handle; the firewall module has no Docker knowledge.
            published_app_ports: Vec::new(),
            custom_rules: custom.clone(),
        }
    }

    /// Set source IP and/or interface restrictions for a service and rebuild firewall.
    pub async fn set_restriction(
        &self,
        service: &str,
        sources: Vec<String>,
        ifaces: Vec<String>,
    ) -> Result<(), String> {
        // Update persisted restrictions
        {
            let mut restrictions = self.restrictions.lock().await;
            if sources.is_empty() {
                restrictions.services.remove(service);
            } else {
                restrictions
                    .services
                    .insert(service.to_string(), sources.clone());
            }
            if ifaces.is_empty() {
                restrictions.interfaces.remove(service);
            } else {
                restrictions
                    .interfaces
                    .insert(service.to_string(), ifaces.clone());
            }
            restrictions.save().await?;
        }

        // Update rules in state
        let mut state = self.state.lock().await;
        if let Some(rule) = state.rules.iter_mut().find(|r| r.service == service) {
            let base_ports = if service == "webui" {
                webui_ports()
            } else if service == "rdma" {
                rdma_ports()
            } else if let Some(proto) = Protocol::from_name(service) {
                ports_for_protocol(proto)
            } else {
                return Err(format!("unknown service: {service}"));
            };
            rule.ports = apply_restrictions(base_ports, &sources, &ifaces);
        }

        let custom = self.custom.lock().await;
        apply_nftables(&state, &custom).await?;
        info!("Firewall: updated restrictions for {service}");
        Ok(())
    }

    /// Get restrictions for a specific service.
    pub async fn get_restrictions(&self) -> HashMap<String, Vec<String>> {
        self.restrictions.lock().await.services.clone()
    }

    /// Point a service's firewall rule at a new port set. iSCSI and
    /// NVMe-oF listen on operator-chosen portal ports, so their rules
    /// follow the configured portals instead of the protocol default
    /// (#602: a portal on a custom port was silently unreachable).
    /// Preserves the rule's open/closed state; no-op (and no nft
    /// apply) when the effective set is unchanged.
    pub async fn set_service_ports(&self, proto: Protocol, ports: Vec<PortSpec>) {
        let mut state = self.state.lock().await;
        let restrictions = self.restrictions.lock().await;
        if !replace_rule_ports(&mut state, &restrictions, proto.name(), ports) {
            return;
        }
        let custom = self.custom.lock().await;
        if let Err(e) = apply_nftables(&state, &custom).await {
            error!("Failed to update firewall ports for {}: {e}", proto.name());
        } else {
            info!("Firewall: updated ports for {}", proto.name());
        }
    }

    /// Create a custom port rule. Validates input + service-port collision +
    /// exact-duplicate, persists, and rebuilds the firewall. Returns the
    /// created rule (with its assigned id). Duplicate/collision/validation
    /// failures return an `Err` and change nothing.
    pub async fn add_custom_rule(&self, input: CustomRuleInput) -> Result<CustomRule, String> {
        validate_custom_input(&input)?;

        let state = self.state.lock().await;
        if let Some(owner) = service_port_conflict(&state, input.transport, input.from, input.to) {
            return Err(format!(
                "{}/{}-{} overlaps ports managed by {owner} — enable {owner} to open them",
                transport_str(input.transport),
                input.from,
                input.to,
            ));
        }
        let mut custom = self.custom.lock().await;
        if custom.iter().any(|r| same_rule(r, &input)) {
            return Err("an identical custom rule already exists".into());
        }

        let rule = CustomRule {
            id: uuid::Uuid::new_v4().to_string(),
            label: input.label.trim().to_string(),
            transport: input.transport,
            from: input.from,
            to: input.to,
            source: input.source.clone(),
            iface: input.iface.clone(),
            enabled: input.enabled,
        };
        custom.push(rule.clone());
        save_custom_rules(&custom).await?;
        apply_nftables(&state, &custom).await?;
        info!("Firewall: added custom rule {} ({})", rule.id, rule.label);
        Ok(rule)
    }

    /// Update an existing custom rule by id (full replace of the mutable
    /// fields). Re-validates; the duplicate check ignores the rule itself.
    pub async fn update_custom_rule(
        &self,
        id: &str,
        input: CustomRuleInput,
    ) -> Result<CustomRule, String> {
        validate_custom_input(&input)?;

        let state = self.state.lock().await;
        if let Some(owner) = service_port_conflict(&state, input.transport, input.from, input.to) {
            return Err(format!(
                "{}/{}-{} overlaps ports managed by {owner} — enable {owner} to open them",
                transport_str(input.transport),
                input.from,
                input.to,
            ));
        }
        let mut custom = self.custom.lock().await;
        if custom.iter().any(|r| r.id != id && same_rule(r, &input)) {
            return Err("an identical custom rule already exists".into());
        }
        let Some(rule) = custom.iter_mut().find(|r| r.id == id) else {
            return Err(format!("custom rule not found: {id}"));
        };
        rule.label = input.label.trim().to_string();
        rule.transport = input.transport;
        rule.from = input.from;
        rule.to = input.to;
        rule.source = input.source.clone();
        rule.iface = input.iface.clone();
        rule.enabled = input.enabled;
        let updated = rule.clone();

        save_custom_rules(&custom).await?;
        apply_nftables(&state, &custom).await?;
        info!("Firewall: updated custom rule {id}");
        Ok(updated)
    }

    /// Remove a custom rule by id and rebuild the firewall. No-op error if the
    /// id is unknown.
    pub async fn remove_custom_rule(&self, id: &str) -> Result<(), String> {
        let state = self.state.lock().await;
        let mut custom = self.custom.lock().await;
        let before = custom.len();
        custom.retain(|r| r.id != id);
        if custom.len() == before {
            return Err(format!("custom rule not found: {id}"));
        }
        save_custom_rules(&custom).await?;
        apply_nftables(&state, &custom).await?;
        info!("Firewall: removed custom rule {id}");
        Ok(())
    }
}

/// Replace `service`'s port set in `state`, re-applying that service's
/// source/interface restrictions. Creates the rule (closed) when the
/// service has none yet, so a later `open` keeps the right ports.
/// Returns whether anything changed, so callers can skip the nft apply.
fn replace_rule_ports(
    state: &mut FirewallState,
    restrictions: &FirewallRestrictions,
    service: &str,
    ports: Vec<PortSpec>,
) -> bool {
    let sources = restrictions
        .services
        .get(service)
        .cloned()
        .unwrap_or_default();
    let ifaces = restrictions
        .interfaces
        .get(service)
        .cloned()
        .unwrap_or_default();
    let ports = apply_restrictions(ports, &sources, &ifaces);

    if let Some(rule) = state.rules.iter_mut().find(|r| r.service == service) {
        if rule.ports == ports {
            return false;
        }
        rule.ports = ports;
    } else {
        state.rules.push(FirewallRule {
            service: service.to_string(),
            ports,
            active: false,
        });
    }
    true
}

/// Apply source and interface restrictions to a set of ports.
fn apply_restrictions(
    ports: Vec<PortSpec>,
    sources: &[String],
    ifaces: &[String],
) -> Vec<PortSpec> {
    if sources.is_empty() && ifaces.is_empty() {
        return ports;
    }

    let mut result = Vec::new();
    for port in &ports {
        if !sources.is_empty() && !ifaces.is_empty() {
            // Both: create a rule for each source × interface combination
            for src in sources {
                for iface in ifaces {
                    result.push(PortSpec {
                        port: port.port,
                        transport: port.transport,
                        source: Some(src.clone()),
                        iface: Some(iface.clone()),
                    });
                }
            }
        } else if !sources.is_empty() {
            for src in sources {
                result.push(PortSpec {
                    port: port.port,
                    transport: port.transport,
                    source: Some(src.clone()),
                    iface: None,
                });
            }
        } else {
            for iface in ifaces {
                result.push(PortSpec {
                    port: port.port,
                    transport: port.transport,
                    source: None,
                    iface: Some(iface.clone()),
                });
            }
        }
    }
    result
}

fn transport_str(t: Transport) -> &'static str {
    match t {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    }
}

/// True when an existing rule is an exact duplicate of the input
/// (transport + range + source + iface). Label and enabled are ignored.
fn same_rule(r: &CustomRule, input: &CustomRuleInput) -> bool {
    r.transport == input.transport
        && r.from == input.from
        && r.to == input.to
        && r.source == input.source
        && r.iface == input.iface
}

// ── nftables application ───────────────────────────────────────

/// Generate and apply the full nftables ruleset atomically.
async fn apply_nftables(state: &FirewallState, custom: &[CustomRule]) -> Result<(), String> {
    let rules = render_ruleset(state, custom);

    // Apply atomically: flush + load
    let flush = Command::new("nft")
        .args(["delete", "table", "inet", "nasty"])
        .output()
        .await;
    // Ignore flush errors (table may not exist yet)
    if let Ok(o) = &flush
        && !o.status.success()
    {
        let stderr = String::from_utf8_lossy(&o.stderr);
        if !stderr.contains("No such file") && !stderr.contains("does not exist") {
            warn!("nft delete table warning: {stderr}");
        }
    }

    // Apply the ruleset by writing it to a temp file and pointing nft at it.
    // An earlier version tried `nft -f -` with piped stdin but never wrote to
    // the pipe, so nft hung waiting for EOF until the spawn went out of scope.
    let tmp = "/tmp/nasty-firewall.nft";
    tokio::fs::write(tmp, &rules)
        .await
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

/// Build the full `table inet nasty` ruleset text from the service rules and
/// the custom rules. Pure — no I/O — so it can be unit-tested.
pub fn render_ruleset(state: &FirewallState, custom: &[CustomRule]) -> String {
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
            let mut conditions = Vec::new();
            if let Some(ref iface) = port.iface {
                conditions.push(format!("iifname \"{iface}\""));
            }
            if let Some(ref src) = port.source {
                conditions.push(format!("ip saddr {src}"));
            }
            conditions.push(format!("{proto} dport {}", port.port));
            rules.push_str(&format!(
                "        {} accept # {}\n",
                conditions.join(" "),
                rule.service
            ));
        }
    }

    for rule in custom {
        if !rule.enabled {
            continue;
        }
        let proto = match rule.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        let mut conditions = Vec::new();
        if let Some(ref iface) = rule.iface {
            conditions.push(format!("iifname \"{iface}\""));
        }
        if let Some(ref src) = rule.source {
            conditions.push(format!("ip saddr {src}"));
        }
        if rule.from == rule.to {
            conditions.push(format!("{proto} dport {}", rule.from));
        } else {
            conditions.push(format!("{proto} dport {}-{}", rule.from, rule.to));
        }
        rules.push_str(&format!(
            "        {} accept # custom:{}\n",
            conditions.join(" "),
            rule.id
        ));
    }

    rules.push_str("    }\n");
    rules.push_str("}\n");
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smb_opens_serving_and_wsdd_discovery_ports() {
        // 445 + 139 are Samba's serving ports; 3702/udp is WSDD's
        // multicast WS-Discovery port — without it Windows 10/11
        // Explorer can't browse the host. Pin them so a refactor
        // doesn't silently drop discovery and turn NASty invisible
        // to Windows file managers again (issue #70).
        let ports = ports_for_protocol(Protocol::Smb);
        assert!(
            ports
                .iter()
                .any(|p| p.port == 445 && p.transport == Transport::Tcp)
        );
        assert!(
            ports
                .iter()
                .any(|p| p.port == 139 && p.transport == Transport::Tcp)
        );
        assert!(
            ports
                .iter()
                .any(|p| p.port == 3702 && p.transport == Transport::Udp)
        );
    }

    // ── strip_iface_refs ──────────────────────────────────────────

    fn restrictions_with(items: &[(&str, &[&str])]) -> FirewallRestrictions {
        FirewallRestrictions {
            services: HashMap::new(),
            interfaces: items
                .iter()
                .map(|(svc, ifaces)| {
                    (
                        svc.to_string(),
                        ifaces.iter().map(|i| i.to_string()).collect(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn strip_iface_refs_removes_named_iface_from_each_service_list() {
        // The reproducer from issue #96: user attached restrictions
        // to bond0 + enp4s0 across multiple protocols, then bond0
        // went away. We must remove bond0 from every service's list
        // without dropping the unaffected entries (enp4s0).
        let mut r = restrictions_with(&[
            ("nfs", &["bond0", "enp4s0"]),
            ("smb", &["bond0"]),
            ("iscsi", &["enp4s0"]),
        ]);
        let changed = r.strip_iface_refs(&["bond0".to_string()]);
        assert!(changed);
        // nfs: enp4s0 survives.
        assert_eq!(
            r.interfaces.get("nfs").unwrap(),
            &vec!["enp4s0".to_string()]
        );
        // smb: only had bond0, list goes empty so the service entry
        // is dropped (empty == "no restriction" — same as not being
        // in the map at all, but we keep the map clean).
        assert!(!r.interfaces.contains_key("smb"));
        // iscsi: untouched.
        assert_eq!(
            r.interfaces.get("iscsi").unwrap(),
            &vec!["enp4s0".to_string()]
        );
    }

    #[test]
    fn strip_iface_refs_returns_false_when_nothing_changed() {
        let mut r = restrictions_with(&[("nfs", &["enp4s0"])]);
        assert!(!r.strip_iface_refs(&["bond0".to_string()]));
        assert_eq!(
            r.interfaces.get("nfs").unwrap(),
            &vec!["enp4s0".to_string()]
        );
    }

    #[test]
    fn strip_iface_refs_handles_empty_inputs() {
        // Defensive — neither side should panic when the other is empty.
        let mut empty = FirewallRestrictions::default();
        assert!(!empty.strip_iface_refs(&["bond0".to_string()]));

        let mut populated = restrictions_with(&[("nfs", &["enp4s0"])]);
        assert!(!populated.strip_iface_refs(&[]));
    }

    // ── replace_rule_ports ────────────────────────────────────────
    //
    // iSCSI/NVMe-oF listen ports follow their configured portals
    // (#602: a portal on a custom port was unreachable because the
    // service rule was pinned to the default port). These pin the
    // port-replacement semantics the engine relies on after every
    // portal mutation.

    fn state_with_rule(service: &str, ports: Vec<PortSpec>, active: bool) -> FirewallState {
        FirewallState {
            rules: vec![FirewallRule {
                service: service.to_string(),
                ports,
                active,
            }],
        }
    }

    fn no_restrictions() -> FirewallRestrictions {
        FirewallRestrictions {
            services: HashMap::new(),
            interfaces: HashMap::new(),
        }
    }

    #[test]
    fn replace_rule_ports_swaps_ports_and_keeps_active_flag() {
        let mut state = state_with_rule("iscsi", vec![tcp(3260)], true);
        let changed = replace_rule_ports(
            &mut state,
            &no_restrictions(),
            "iscsi",
            vec![tcp(3260), tcp(3261)],
        );
        assert!(changed);
        assert_eq!(state.rules[0].ports, vec![tcp(3260), tcp(3261)]);
        assert!(state.rules[0].active, "active flag must survive the swap");
    }

    #[test]
    fn replace_rule_ports_preserves_inactive_flag() {
        let mut state = state_with_rule("iscsi", vec![tcp(3260)], false);
        replace_rule_ports(&mut state, &no_restrictions(), "iscsi", vec![tcp(3999)]);
        assert!(
            !state.rules[0].active,
            "a disabled service must not be opened by a port resync"
        );
        assert_eq!(state.rules[0].ports, vec![tcp(3999)]);
    }

    #[test]
    fn replace_rule_ports_applies_service_restrictions_to_new_ports() {
        let mut state = state_with_rule("iscsi", vec![tcp(3260)], true);
        let restrictions = FirewallRestrictions {
            services: HashMap::from([("iscsi".to_string(), vec!["192.168.1.0/24".to_string()])]),
            interfaces: HashMap::new(),
        };
        replace_rule_ports(&mut state, &restrictions, "iscsi", vec![tcp(3261)]);
        assert_eq!(state.rules[0].ports.len(), 1);
        assert_eq!(state.rules[0].ports[0].port, 3261);
        assert_eq!(
            state.rules[0].ports[0].source.as_deref(),
            Some("192.168.1.0/24"),
            "restrictions must be re-applied to the replacement ports"
        );
    }

    #[test]
    fn replace_rule_ports_creates_inactive_rule_for_unknown_service() {
        // A resync can land before the service was ever opened; the
        // rule must exist (so a later `open` keeps the right ports)
        // but stay closed.
        let mut state = FirewallState::default();
        let changed = replace_rule_ports(&mut state, &no_restrictions(), "nvmeof", vec![tcp(4421)]);
        assert!(changed);
        assert_eq!(state.rules.len(), 1);
        assert_eq!(state.rules[0].service, "nvmeof");
        assert_eq!(state.rules[0].ports, vec![tcp(4421)]);
        assert!(!state.rules[0].active);
    }

    #[test]
    fn replace_rule_ports_reports_unchanged_for_identical_set() {
        let mut state = state_with_rule("iscsi", vec![tcp(3260)], true);
        let changed = replace_rule_ports(&mut state, &no_restrictions(), "iscsi", vec![tcp(3260)]);
        assert!(!changed, "identical port set must not trigger an nft apply");
    }

    // ── render_ruleset custom rules (#620) ──────────────────────────

    #[test]
    fn render_includes_enabled_custom_single_port() {
        let state = FirewallState::default();
        let custom = vec![CustomRule {
            id: "id1".into(),
            label: "plex".into(),
            transport: Transport::Tcp,
            from: 32400,
            to: 32400,
            source: None,
            iface: None,
            enabled: true,
        }];
        let out = render_ruleset(&state, &custom);
        assert!(
            out.contains("tcp dport 32400 accept # custom:id1"),
            "got:\n{out}"
        );
    }

    #[test]
    fn render_includes_range_and_conditions() {
        let state = FirewallState::default();
        let custom = vec![CustomRule {
            id: "id2".into(),
            label: "games".into(),
            transport: Transport::Udp,
            from: 8000,
            to: 8010,
            source: Some("10.0.0.0/8".into()),
            iface: Some("eth0".into()),
            enabled: true,
        }];
        let out = render_ruleset(&state, &custom);
        assert!(
            out.contains(
                "iifname \"eth0\" ip saddr 10.0.0.0/8 udp dport 8000-8010 accept # custom:id2"
            ),
            "got:\n{out}"
        );
    }

    #[test]
    fn render_omits_disabled_custom() {
        let state = FirewallState::default();
        let custom = vec![CustomRule {
            id: "id3".into(),
            label: "off".into(),
            transport: Transport::Tcp,
            from: 9999,
            to: 9999,
            source: None,
            iface: None,
            enabled: false,
        }];
        let out = render_ruleset(&state, &custom);
        assert!(
            !out.contains("9999"),
            "disabled rule must not render; got:\n{out}"
        );
    }

    // ── validate_custom_input / service_port_conflict (#620) ────────

    fn input(from: u16, to: u16, transport: Transport) -> CustomRuleInput {
        CustomRuleInput {
            label: "test".into(),
            transport,
            from,
            to,
            source: None,
            iface: None,
            enabled: true,
        }
    }

    #[test]
    fn validate_rejects_bad_range_and_input() {
        // from > to
        assert!(validate_custom_input(&input(100, 50, Transport::Tcp)).is_err());
        // from == 0
        assert!(validate_custom_input(&input(0, 10, Transport::Tcp)).is_err());
        // empty label
        let mut i = input(80, 80, Transport::Tcp);
        i.label = "".into();
        assert!(validate_custom_input(&i).is_err());
        // control char in label
        let mut i = input(80, 80, Transport::Tcp);
        i.label = "a\nb".into();
        assert!(validate_custom_input(&i).is_err());
        // bad source
        let mut i = input(80, 80, Transport::Tcp);
        i.source = Some("1.2.3.4 accept".into());
        assert!(validate_custom_input(&i).is_err());
        // bad iface
        let mut i = input(80, 80, Transport::Tcp);
        i.iface = Some("eth0; drop".into());
        assert!(validate_custom_input(&i).is_err());
        // valid: single port, valid CIDR, valid iface
        let mut i = input(8000, 8010, Transport::Udp);
        i.source = Some("192.168.1.0/24".into());
        i.iface = Some("tailscale0".into());
        assert!(validate_custom_input(&i).is_ok());
        // valid: bare IP source
        let mut i = input(8000, 8000, Transport::Tcp);
        i.source = Some("10.0.0.5".into());
        assert!(validate_custom_input(&i).is_ok());
    }

    #[test]
    fn service_conflict_detects_owned_ports() {
        let mut state = FirewallState::default();
        // SMB owns tcp/445 (active), a disabled service owns tcp/2049,
        // transport matters.
        state.rules.push(FirewallRule {
            service: "smb".into(),
            ports: vec![PortSpec {
                port: 445,
                transport: Transport::Tcp,
                source: None,
                iface: None,
            }],
            active: true,
        });
        state.rules.push(FirewallRule {
            service: "nfs".into(),
            ports: vec![PortSpec {
                port: 2049,
                transport: Transport::Tcp,
                source: None,
                iface: None,
            }],
            active: false, // disabled service still owns its port
        });

        assert_eq!(
            service_port_conflict(&state, Transport::Tcp, 445, 445).as_deref(),
            Some("smb")
        );
        // range spanning the port
        assert_eq!(
            service_port_conflict(&state, Transport::Tcp, 440, 450).as_deref(),
            Some("smb")
        );
        // disabled service still conflicts
        assert_eq!(
            service_port_conflict(&state, Transport::Tcp, 2049, 2049).as_deref(),
            Some("nfs")
        );
        // transport mismatch → no conflict
        assert_eq!(
            service_port_conflict(&state, Transport::Udp, 445, 445),
            None
        );
        // free port → no conflict
        assert_eq!(
            service_port_conflict(&state, Transport::Tcp, 32400, 32400),
            None
        );
    }
}
