//! Dynamic nftables firewall — engine-managed port rules.
//!
//! Maintains a `table inet nasty` with an `input` chain. Rules are added/removed
//! when protocols are enabled/disabled. The table is rebuilt atomically on every change.

use crate::protocol::Protocol;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
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
    fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(json) => {
                serde_json::from_str(&json).map_err(|e| format!("parse {}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
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
    /// Optional end of a contiguous port range (`port`..=`to`). `None`
    /// means a single port. First used by the DC role's dynamic-RPC range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<u16>,
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
fn load_custom_rules(path: &Path) -> Result<Vec<CustomRule>, String> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            serde_json::from_str(&json).map_err(|e| format!("parse {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
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
            let hi = port.to.unwrap_or(port.port);
            if port.transport == transport && port.port <= to && hi >= from {
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
    /// Ports that Docker-managed apps publish on the host. The firewall's
    /// early forward hook permits these explicitly by original DNAT port and
    /// drops other original-direction inbound DNAT traffic.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
        to: None,
        transport: Transport::Tcp,
        source: None,
        iface: None,
    }
}

fn udp(port: u16) -> PortSpec {
    PortSpec {
        port,
        to: None,
        transport: Transport::Udp,
        source: None,
        iface: None,
    }
}

fn tcp_range(from: u16, to: u16) -> PortSpec {
    PortSpec {
        port: from,
        to: Some(to),
        transport: Transport::Tcp,
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

/// Ports for the Active Directory DC role (#20): DNS, Kerberos +
/// kpasswd, RPC endpoint mapper, NetBIOS, LDAP (tcp) + CLDAP DC-locator
/// ping (udp), LDAP(S), SMB, Global Catalog, and the dynamic RPC range.
/// Deliberately no NTP — the box does not serve time to domain clients
/// (documented limitation).
pub fn dc_ports() -> Vec<PortSpec> {
    let mut ports = Vec::new();
    for p in [53u16, 88, 389, 464] {
        ports.push(tcp(p));
        ports.push(udp(p));
    }
    for p in [135u16, 139, 445, 636, 3268, 3269] {
        ports.push(tcp(p));
    }
    for p in [137u16, 138] {
        ports.push(udp(p));
    }
    ports.push(tcp_range(49152, 65535));
    ports
}

// ── Firewall service ───────────────────────────────────────────

#[derive(Clone, Default)]
struct FirewallConfig {
    state: FirewallState,
    restrictions: FirewallRestrictions,
    custom: Vec<CustomRule>,
    published: Vec<PublishedAppPort>,
}

struct FirewallPaths {
    restrictions: PathBuf,
    custom: PathBuf,
}

pub struct FirewallService {
    config: tokio::sync::Mutex<FirewallConfig>,
    paths: FirewallPaths,
    nft_program: PathBuf,
}

impl Default for FirewallService {
    fn default() -> Self {
        Self::new()
    }
}

impl FirewallService {
    pub fn new() -> Self {
        Self {
            config: tokio::sync::Mutex::new(FirewallConfig::default()),
            paths: FirewallPaths {
                restrictions: PathBuf::from(RESTRICTIONS_PATH),
                custom: PathBuf::from(CUSTOM_PATH),
            },
            nft_program: PathBuf::from("nft"),
        }
    }

    async fn commit_candidate(
        &self,
        current: &mut FirewallConfig,
        candidate: FirewallConfig,
        staged: Option<StagedJson>,
    ) -> Result<(), String> {
        if let Err(e) = apply_nftables_with(
            &self.nft_program,
            &candidate.state,
            &candidate.custom,
            &candidate.published,
        )
        .await
        {
            if let Some(staged) = staged {
                staged.discard().await;
            }
            return Err(e);
        }

        if let Some(staged) = staged
            && let Err(persist_error) = staged.commit().await
        {
            let rollback = apply_nftables_with(
                &self.nft_program,
                &current.state,
                &current.custom,
                &current.published,
            )
            .await;
            if let Err(rollback_error) = rollback {
                error!(
                    "CRITICAL: firewall persistence failed and live-rule rollback also failed: \
                     persist={persist_error}; rollback={rollback_error}"
                );
                // The candidate is still live in the kernel. Keep status and
                // subsequent transactions aligned with reality even though
                // disk remains stale; the returned error forces the caller to
                // surface the degraded persistence state.
                *current = candidate;
                return Err(format!(
                    "persist firewall state: {persist_error}; live rollback also failed: \
                     {rollback_error}; in-memory state reflects the live candidate"
                ));
            }
            return Err(format!(
                "persist firewall state: {persist_error}; previous live rules restored"
            ));
        }

        *current = candidate;
        Ok(())
    }

    async fn set_rule_active(
        &self,
        service: &str,
        base_ports: Vec<PortSpec>,
        active: bool,
    ) -> Result<(), String> {
        if base_ports.is_empty() {
            return Ok(());
        }
        let mut current = self.config.lock().await;
        let mut candidate = current.clone();
        if let Some(rule) = candidate
            .state
            .rules
            .iter_mut()
            .find(|rule| rule.service == service)
        {
            if rule.active == active {
                return Ok(());
            }
            rule.active = active;
        } else {
            let sources = candidate
                .restrictions
                .services
                .get(service)
                .cloned()
                .unwrap_or_default();
            let ifaces = candidate
                .restrictions
                .interfaces
                .get(service)
                .cloned()
                .unwrap_or_default();
            candidate.state.rules.push(FirewallRule {
                service: service.to_string(),
                ports: apply_restrictions(base_ports, &sources, &ifaces),
                active,
            });
        }
        self.commit_candidate(&mut current, candidate, None).await
    }

    /// Initialize the complete firewall policy in one transaction before any
    /// engine-managed network-facing services are restored.
    pub async fn init(
        &self,
        enabled_protocols: &[(Protocol, bool)],
        rdma_enabled: bool,
        dc_enabled: bool,
        iscsi_ports: Vec<PortSpec>,
        nvmeof_ports: Vec<PortSpec>,
        published: Vec<PublishedAppPort>,
    ) -> Result<(), String> {
        let mut current = self.config.lock().await;
        let restrictions = FirewallRestrictions::load(&self.paths.restrictions)?;
        let custom = load_custom_rules(&self.paths.custom)?;
        let mut candidate = FirewallConfig {
            state: FirewallState::default(),
            restrictions,
            custom,
            published,
        };

        let webui_sources = candidate
            .restrictions
            .services
            .get("webui")
            .cloned()
            .unwrap_or_default();
        let webui_ifaces = candidate
            .restrictions
            .interfaces
            .get("webui")
            .cloned()
            .unwrap_or_default();
        candidate.state.rules.push(FirewallRule {
            service: "webui".to_string(),
            ports: apply_restrictions(webui_ports(), &webui_sources, &webui_ifaces),
            active: true,
        });

        for (proto, enabled) in enabled_protocols {
            let ports = ports_for_protocol(*proto);
            if ports.is_empty() {
                continue;
            }
            let sources = candidate
                .restrictions
                .services
                .get(proto.name())
                .cloned()
                .unwrap_or_default();
            let ifaces = candidate
                .restrictions
                .interfaces
                .get(proto.name())
                .cloned()
                .unwrap_or_default();
            candidate.state.rules.push(FirewallRule {
                service: proto.name().to_string(),
                ports: apply_restrictions(ports, &sources, &ifaces),
                active: *enabled,
            });
        }

        if rdma_enabled {
            let sources = candidate
                .restrictions
                .services
                .get("rdma")
                .cloned()
                .unwrap_or_default();
            let ifaces = candidate
                .restrictions
                .interfaces
                .get("rdma")
                .cloned()
                .unwrap_or_default();
            candidate.state.rules.push(FirewallRule {
                service: "rdma".to_string(),
                ports: apply_restrictions(rdma_ports(), &sources, &ifaces),
                active: true,
            });
        }
        if dc_enabled {
            let sources = candidate
                .restrictions
                .services
                .get("dc")
                .cloned()
                .unwrap_or_default();
            let ifaces = candidate
                .restrictions
                .interfaces
                .get("dc")
                .cloned()
                .unwrap_or_default();
            candidate.state.rules.push(FirewallRule {
                service: "dc".to_string(),
                ports: apply_restrictions(dc_ports(), &sources, &ifaces),
                active: true,
            });
        }
        replace_rule_ports(
            &mut candidate.state,
            &candidate.restrictions,
            Protocol::Iscsi.name(),
            iscsi_ports,
        );
        replace_rule_ports(
            &mut candidate.state,
            &candidate.restrictions,
            Protocol::Nvmeof.name(),
            nvmeof_ports,
        );

        let rule_count = candidate.state.rules.len();
        self.commit_candidate(&mut current, candidate, None).await?;
        info!("Firewall initialized with {rule_count} rules");
        Ok(())
    }

    /// Reconcile protocol rules after service restoration, which may disable a
    /// persisted protocol whose daemon failed to start.
    pub async fn set_protocol_states(
        &self,
        enabled_protocols: &[(Protocol, bool)],
    ) -> Result<(), String> {
        let mut current = self.config.lock().await;
        let mut candidate = current.clone();
        let mut changed = false;
        for (proto, enabled) in enabled_protocols {
            if let Some(rule) = candidate
                .state
                .rules
                .iter_mut()
                .find(|rule| rule.service == proto.name())
                && rule.active != *enabled
            {
                rule.active = *enabled;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        self.commit_candidate(&mut current, candidate, None).await
    }

    /// Replace the Docker DNAT allowlist rendered into the early forward hook.
    pub async fn set_published_app_ports(
        &self,
        mut published: Vec<PublishedAppPort>,
    ) -> Result<(), String> {
        published.sort_by(|a, b| {
            a.transport
                .cmp(&b.transport)
                .then_with(|| a.host_port.cmp(&b.host_port))
                .then_with(|| a.app.cmp(&b.app))
        });
        published.dedup_by(|a, b| {
            a.transport == b.transport && a.host_port == b.host_port && a.app == b.app
        });
        let mut current = self.config.lock().await;
        if current.published == published {
            return Ok(());
        }
        let mut candidate = current.clone();
        candidate.published = published;
        self.commit_candidate(&mut current, candidate, None).await
    }

    pub async fn open(&self, proto: Protocol) -> Result<(), String> {
        let name = proto.name();
        self.set_rule_active(name, ports_for_protocol(proto), true)
            .await?;
        info!("Firewall: opened ports for {name}");
        Ok(())
    }

    pub async fn close(&self, proto: Protocol) -> Result<(), String> {
        let name = proto.name();
        self.set_rule_active(name, ports_for_protocol(proto), false)
            .await?;
        info!("Firewall: closed ports for {name}");
        Ok(())
    }

    pub async fn open_rdma(&self) -> Result<(), String> {
        self.set_rule_active("rdma", rdma_ports(), true).await?;
        info!("Firewall: opened ports for rdma");
        Ok(())
    }

    pub async fn close_rdma(&self) -> Result<(), String> {
        self.set_rule_active("rdma", rdma_ports(), false).await?;
        info!("Firewall: closed ports for rdma");
        Ok(())
    }

    pub async fn open_dc(&self) -> Result<(), String> {
        self.set_rule_active("dc", dc_ports(), true).await?;
        info!("Firewall: opened ports for dc");
        Ok(())
    }

    pub async fn close_dc(&self) -> Result<(), String> {
        self.set_rule_active("dc", dc_ports(), false).await?;
        info!("Firewall: closed ports for dc");
        Ok(())
    }

    pub async fn status(&self) -> FirewallStatus {
        let config = self.config.lock().await;
        FirewallStatus {
            active: true,
            rules: config.state.rules.clone(),
            restrictions: config.restrictions.services.clone(),
            interface_restrictions: config.restrictions.interfaces.clone(),
            published_app_ports: config.published.clone(),
            custom_rules: config.custom.clone(),
        }
    }

    pub async fn set_restriction(
        &self,
        service: &str,
        sources: Vec<String>,
        ifaces: Vec<String>,
    ) -> Result<(), String> {
        for source in &sources {
            if !valid_source(source) {
                return Err(format!("invalid source (expected an IP or CIDR): {source}"));
            }
        }
        for iface in &ifaces {
            if !valid_iface(iface) {
                return Err(format!("invalid interface name: {iface}"));
            }
        }
        let default_ports = if service == "webui" {
            webui_ports()
        } else if service == "rdma" {
            rdma_ports()
        } else if service == "dc" {
            dc_ports()
        } else if let Some(proto) = Protocol::from_name(service) {
            ports_for_protocol(proto)
        } else {
            return Err(format!("unknown service: {service}"));
        };

        let mut current = self.config.lock().await;
        let base_ports = current
            .state
            .rules
            .iter()
            .find(|rule| rule.service == service)
            .map(|rule| {
                let mut ports = Vec::new();
                for mut port in rule.ports.iter().cloned() {
                    port.source = None;
                    port.iface = None;
                    if !ports.contains(&port) {
                        ports.push(port);
                    }
                }
                ports
            })
            .unwrap_or(default_ports);
        let mut candidate = current.clone();
        if sources.is_empty() {
            candidate.restrictions.services.remove(service);
        } else {
            candidate
                .restrictions
                .services
                .insert(service.to_string(), sources.clone());
        }
        if ifaces.is_empty() {
            candidate.restrictions.interfaces.remove(service);
        } else {
            candidate
                .restrictions
                .interfaces
                .insert(service.to_string(), ifaces.clone());
        }
        if let Some(rule) = candidate
            .state
            .rules
            .iter_mut()
            .find(|rule| rule.service == service)
        {
            rule.ports = apply_restrictions(base_ports, &sources, &ifaces);
        }

        let staged = StagedJson::stage(&self.paths.restrictions, &candidate.restrictions).await?;
        self.commit_candidate(&mut current, candidate, Some(staged))
            .await?;
        info!("Firewall: updated restrictions for {service}");
        Ok(())
    }

    pub async fn get_restrictions(&self) -> HashMap<String, Vec<String>> {
        self.config.lock().await.restrictions.services.clone()
    }

    pub async fn set_service_ports(
        &self,
        proto: Protocol,
        ports: Vec<PortSpec>,
    ) -> Result<(), String> {
        let mut current = self.config.lock().await;
        let mut candidate = current.clone();
        if !replace_rule_ports(
            &mut candidate.state,
            &candidate.restrictions,
            proto.name(),
            ports,
        ) {
            return Ok(());
        }
        self.commit_candidate(&mut current, candidate, None).await?;
        info!("Firewall: updated ports for {}", proto.name());
        Ok(())
    }

    pub async fn set_portal_ports(
        &self,
        iscsi_ports: Vec<PortSpec>,
        nvmeof_ports: Vec<PortSpec>,
    ) -> Result<(), String> {
        let mut current = self.config.lock().await;
        let mut candidate = current.clone();
        let iscsi_changed = replace_rule_ports(
            &mut candidate.state,
            &candidate.restrictions,
            Protocol::Iscsi.name(),
            iscsi_ports,
        );
        let nvmeof_changed = replace_rule_ports(
            &mut candidate.state,
            &candidate.restrictions,
            Protocol::Nvmeof.name(),
            nvmeof_ports,
        );
        if !iscsi_changed && !nvmeof_changed {
            return Ok(());
        }
        self.commit_candidate(&mut current, candidate, None).await?;
        info!("Firewall: updated iSCSI and NVMe-oF portal ports");
        Ok(())
    }

    pub async fn add_custom_rule(&self, input: CustomRuleInput) -> Result<CustomRule, String> {
        validate_custom_input(&input)?;
        let mut current = self.config.lock().await;
        if let Some(owner) =
            service_port_conflict(&current.state, input.transport, input.from, input.to)
        {
            return Err(format!(
                "{}/{}-{} overlaps ports managed by {owner} — enable {owner} to open them",
                transport_str(input.transport),
                input.from,
                input.to,
            ));
        }
        if current.custom.iter().any(|rule| same_rule(rule, &input)) {
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
        let mut candidate = current.clone();
        candidate.custom.push(rule.clone());
        let staged = StagedJson::stage(&self.paths.custom, &candidate.custom).await?;
        self.commit_candidate(&mut current, candidate, Some(staged))
            .await?;
        info!("Firewall: added custom rule {} ({})", rule.id, rule.label);
        Ok(rule)
    }

    pub async fn update_custom_rule(
        &self,
        id: &str,
        input: CustomRuleInput,
    ) -> Result<CustomRule, String> {
        validate_custom_input(&input)?;
        let mut current = self.config.lock().await;
        if let Some(owner) =
            service_port_conflict(&current.state, input.transport, input.from, input.to)
        {
            return Err(format!(
                "{}/{}-{} overlaps ports managed by {owner} — enable {owner} to open them",
                transport_str(input.transport),
                input.from,
                input.to,
            ));
        }
        if current
            .custom
            .iter()
            .any(|rule| rule.id != id && same_rule(rule, &input))
        {
            return Err("an identical custom rule already exists".into());
        }

        let mut candidate = current.clone();
        let Some(rule) = candidate.custom.iter_mut().find(|rule| rule.id == id) else {
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
        let staged = StagedJson::stage(&self.paths.custom, &candidate.custom).await?;
        self.commit_candidate(&mut current, candidate, Some(staged))
            .await?;
        info!("Firewall: updated custom rule {id}");
        Ok(updated)
    }

    pub async fn remove_custom_rule(&self, id: &str) -> Result<(), String> {
        let mut current = self.config.lock().await;
        let mut candidate = current.clone();
        let before = candidate.custom.len();
        candidate.custom.retain(|rule| rule.id != id);
        if candidate.custom.len() == before {
            return Err(format!("custom rule not found: {id}"));
        }
        let staged = StagedJson::stage(&self.paths.custom, &candidate.custom).await?;
        self.commit_candidate(&mut current, candidate, Some(staged))
            .await?;
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
                        to: port.to,
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
                    to: port.to,
                    transport: port.transport,
                    source: Some(src.clone()),
                    iface: None,
                });
            }
        } else {
            for iface in ifaces {
                result.push(PortSpec {
                    port: port.port,
                    to: port.to,
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

struct StagedJson {
    temp: PathBuf,
    destination: PathBuf,
    finished: bool,
}

impl StagedJson {
    async fn stage<T: Serialize>(destination: &Path, value: &T) -> Result<Self, String> {
        let parent = destination
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid state path: {}", destination.display()))?;
        let temp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        let json = serde_json::to_vec_pretty(value).map_err(|e| format!("serialize: {e}"))?;

        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .await
            .map_err(|e| format!("create {}: {e}", temp.display()))?;
        if let Err(e) = file.write_all(&json).await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(format!("write {}: {e}", temp.display()));
        }
        if let Err(e) = file.sync_all().await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(format!("sync {}: {e}", temp.display()));
        }
        drop(file);

        Ok(Self {
            temp,
            destination: destination.to_path_buf(),
            finished: false,
        })
    }

    async fn commit(mut self) -> Result<(), String> {
        tokio::fs::rename(&self.temp, &self.destination)
            .await
            .map_err(|e| {
                format!(
                    "rename {} to {}: {e}",
                    self.temp.display(),
                    self.destination.display()
                )
            })?;
        self.finished = true;
        if let Some(parent) = self.destination.parent() {
            match tokio::fs::File::open(parent).await {
                Ok(directory) => {
                    if let Err(e) = directory.sync_all().await {
                        warn!("sync firewall state directory {}: {e}", parent.display());
                    }
                }
                Err(e) => warn!(
                    "open firewall state directory {} for sync: {e}",
                    parent.display()
                ),
            }
        }
        Ok(())
    }

    async fn discard(mut self) {
        let _ = tokio::fs::remove_file(&self.temp).await;
        self.finished = true;
    }
}

impl Drop for StagedJson {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

// ── nftables application ───────────────────────────────────────

/// Validate the exact replacement batch, then submit it once. nft sends each
/// file as one netlink transaction, so a failure leaves the current table
/// untouched instead of exposing the host between delete and reload commands.
async fn apply_nftables_with(
    nft_program: &Path,
    state: &FirewallState,
    custom: &[CustomRule],
    published: &[PublishedAppPort],
) -> Result<(), String> {
    let transaction = render_transaction(state, custom, published);
    run_nft(
        nft_program,
        &["--check", "--file", "-"],
        &transaction,
        "validation",
    )
    .await?;
    run_nft(nft_program, &["--file", "-"], &transaction, "apply").await
}

async fn run_nft(
    nft_program: &Path,
    args: &[&str],
    transaction: &str,
    stage: &str,
) -> Result<(), String> {
    let mut command = Command::new(nft_program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn nft {stage}: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("open nft {stage} stdin"))?;
    let transaction = transaction.as_bytes().to_vec();
    let writer_stage = stage.to_string();
    let writer = tokio::spawn(async move {
        stdin
            .write_all(&transaction)
            .await
            .map_err(|e| format!("write nft {writer_stage} stdin: {e}"))
    });
    let (write_result, output_result) =
        tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            tokio::join!(writer, child.wait_with_output())
        })
        .await
        .map_err(|_| format!("nft {stage} timed out"))?;
    write_result.map_err(|e| format!("join nft {stage} stdin writer: {e}"))??;
    let output = output_result.map_err(|e| format!("wait for nft {stage}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "nft {stage} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn render_transaction(
    state: &FirewallState,
    custom: &[CustomRule],
    published: &[PublishedAppPort],
) -> String {
    let mut transaction = String::from("destroy table inet nasty\n");
    transaction.push_str(&render_ruleset_with_published(state, custom, published));
    transaction
}

/// nft source-address clause with the correct family: `ip6 saddr` for an
/// IPv6 source, `ip saddr` for IPv4. `src` may be a bare address or CIDR;
/// the family is decided by the address part. Defaults to `ip saddr` if the
/// address part doesn't parse (renderer is downstream of validation).
fn saddr_clause(src: &str) -> String {
    let addr = src.split('/').next().unwrap_or(src);
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("ip6 saddr {src}"),
        _ => format!("ip saddr {src}"),
    }
}

/// Build the full `table inet nasty` ruleset text from the service rules and
/// the custom rules. Pure — no I/O — so it can be unit-tested.
pub fn render_ruleset(state: &FirewallState, custom: &[CustomRule]) -> String {
    render_ruleset_with_published(state, custom, &[])
}

fn render_ruleset_with_published(
    state: &FirewallState,
    custom: &[CustomRule],
    published: &[PublishedAppPort],
) -> String {
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
                conditions.push(saddr_clause(src));
            }
            match port.to {
                Some(to) => conditions.push(format!("{proto} dport {}-{to}", port.port)),
                None => conditions.push(format!("{proto} dport {}", port.port)),
            }
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
            conditions.push(saddr_clause(src));
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
    rules.push_str("    chain forward {\n");
    rules.push_str("        type filter hook forward priority -10; policy accept;\n");
    rules.push_str("        # Explicitly allowed Docker-published host ports\n");
    for port in published {
        let proto = if port.transport.eq_ignore_ascii_case("tcp") {
            "tcp"
        } else if port.transport.eq_ignore_ascii_case("udp") {
            "udp"
        } else {
            continue;
        };
        rules.push_str(&format!(
            "        ct direction original ct status dnat meta l4proto {proto} ct original proto-dst {} accept # app-published\n",
            port.host_port
        ));
    }
    // Custom host-port rules also authorize matching inbound DNAT traffic.
    for rule in custom {
        if !rule.enabled {
            continue;
        }
        let proto = match rule.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        let mut conditions = vec![
            "ct direction original".to_string(),
            "ct status dnat".to_string(),
        ];
        if let Some(ref iface) = rule.iface {
            conditions.push(format!("iifname \"{iface}\""));
        }
        if let Some(ref src) = rule.source {
            conditions.push(saddr_clause(src));
        }
        conditions.push(format!("meta l4proto {proto}"));
        if rule.from == rule.to {
            conditions.push(format!("ct original proto-dst {}", rule.from));
        } else {
            conditions.push(format!("ct original proto-dst {}-{}", rule.from, rule.to));
        }
        rules.push_str(&format!(
            "        {} accept # custom:{}\n",
            conditions.join(" "),
            rule.id
        ));
    }
    rules.push_str("        ct direction original ct status dnat drop\n");
    rules.push_str("    }\n");
    rules.push_str("}\n");
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mock_nft(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("mock-nft");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn test_service(dir: &Path, nft_program: PathBuf, config: FirewallConfig) -> FirewallService {
        FirewallService {
            config: tokio::sync::Mutex::new(config),
            paths: FirewallPaths {
                restrictions: dir.join("restrictions.json"),
                custom: dir.join("custom.json"),
            },
            nft_program,
        }
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn setting_restriction_preserves_custom_portal_ports() {
        let dir = tempfile::tempdir().unwrap();
        let nft = mock_nft(dir.path(), "cat >/dev/null\nexit 0");
        let config = FirewallConfig {
            state: state_with_rule("iscsi", vec![tcp(3261)], true),
            ..FirewallConfig::default()
        };
        let service = test_service(dir.path(), nft, config);

        service
            .set_restriction(
                "iscsi",
                vec!["10.0.0.0/8".into(), "192.168.0.0/16".into()],
                Vec::new(),
            )
            .await
            .unwrap();
        service
            .set_restriction(
                "iscsi",
                vec!["10.0.0.0/8".into(), "192.168.0.0/16".into()],
                Vec::new(),
            )
            .await
            .unwrap();

        let rule = service
            .status()
            .await
            .rules
            .into_iter()
            .find(|rule| rule.service == "iscsi")
            .unwrap();
        assert_eq!(rule.ports.len(), 2, "re-saving must not multiply ports");
        assert!(rule.ports.iter().all(|port| port.port == 3261));
        assert_eq!(rule.ports[0].source.as_deref(), Some("10.0.0.0/8"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn portal_ports_commit_in_one_firewall_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let invocations = dir.path().join("invocations");
        let nft = mock_nft(
            dir.path(),
            &format!("cat >> \"{}\"\nexit 0", invocations.display()),
        );
        let config = FirewallConfig {
            state: FirewallState {
                rules: vec![
                    FirewallRule {
                        service: "iscsi".into(),
                        ports: vec![tcp(3260)],
                        active: true,
                    },
                    FirewallRule {
                        service: "nvmeof".into(),
                        ports: vec![tcp(4420)],
                        active: true,
                    },
                ],
            },
            ..FirewallConfig::default()
        };
        let service = test_service(dir.path(), nft, config);

        service
            .set_portal_ports(vec![tcp(3261)], vec![tcp(4421)])
            .await
            .unwrap();

        let status = service.status().await;
        assert_eq!(
            status
                .rules
                .iter()
                .find(|rule| rule.service == "iscsi")
                .unwrap()
                .ports[0]
                .port,
            3261
        );
        assert_eq!(
            status
                .rules
                .iter()
                .find(|rule| rule.service == "nvmeof")
                .unwrap()
                .ports[0]
                .port,
            4421
        );
        // One check and one apply, both carrying the complete two-service batch.
        let recorded = std::fs::read_to_string(invocations).unwrap();
        assert_eq!(recorded.matches("destroy table inet nasty").count(), 2);
        assert_eq!(recorded.matches("3261").count(), 2);
        assert_eq!(recorded.matches("4421").count(), 2);
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
    fn transaction_destroys_and_replaces_table_in_one_batch() {
        let transaction = render_transaction(&FirewallState::default(), &[], &[]);
        assert!(transaction.starts_with("destroy table inet nasty\n"));
        assert_eq!(transaction.matches("destroy table inet nasty").count(), 1);
        assert_eq!(transaction.matches("table inet nasty {").count(), 1);
        assert!(!transaction.contains("delete table"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_failure_preserves_in_memory_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = FirewallConfig {
            state: state_with_rule("ssh", vec![tcp(22)], false),
            ..Default::default()
        };
        let service = test_service(dir.path(), dir.path().join("missing-nft-program"), config);

        assert!(service.open(Protocol::Ssh).await.is_err());
        let status = service.status().await;
        assert!(!status.rules[0].active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn validation_failure_preserves_in_memory_state() {
        let dir = tempfile::tempdir().unwrap();
        let nft = mock_nft(
            dir.path(),
            "cat >/dev/null\necho validation rejected >&2\nexit 1",
        );
        let config = FirewallConfig {
            state: state_with_rule("ssh", vec![tcp(22)], false),
            ..Default::default()
        };
        let service = test_service(dir.path(), nft, config);

        let error = service.open(Protocol::Ssh).await.unwrap_err();
        assert!(error.contains("validation rejected"));
        assert!(!service.status().await.rules[0].active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_failure_preserves_in_memory_state() {
        let dir = tempfile::tempdir().unwrap();
        let nft = mock_nft(
            dir.path(),
            "cat >/dev/null\nif [ \"$1\" = \"--check\" ]; then exit 0; fi\necho apply rejected >&2\nexit 1",
        );
        let config = FirewallConfig {
            state: state_with_rule("ssh", vec![tcp(22)], false),
            ..Default::default()
        };
        let service = test_service(dir.path(), nft, config);

        let error = service.open(Protocol::Ssh).await.unwrap_err();
        assert!(error.contains("apply rejected"));
        assert!(!service.status().await.rules[0].active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_and_apply_receive_the_identical_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("transactions");
        let nft = mock_nft(
            dir.path(),
            &format!(
                "cat >> \"{}\"\nprintf '\\n---transaction---\\n' >> \"{}\"\nexit 0",
                log.display(),
                log.display()
            ),
        );

        apply_nftables_with(&nft, &state_with_rule("ssh", vec![tcp(22)], true), &[], &[])
            .await
            .unwrap();
        let recorded = std::fs::read_to_string(log).unwrap();
        let transactions: Vec<&str> = recorded
            .split("---transaction---")
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0], transactions[1]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn corrupt_persisted_state_aborts_initialization_without_applying() {
        let dir = tempfile::tempdir().unwrap();
        let invocations = dir.path().join("invocations");
        let nft = mock_nft(
            dir.path(),
            &format!(
                "cat >/dev/null\nprintf x >> \"{}\"\nexit 0",
                invocations.display()
            ),
        );
        std::fs::write(dir.path().join("restrictions.json"), "{not-json").unwrap();
        let config = FirewallConfig {
            state: state_with_rule("ssh", vec![tcp(22)], false),
            ..Default::default()
        };
        let service = test_service(dir.path(), nft, config);

        let error = service
            .init(&[], false, false, vec![tcp(3260)], vec![tcp(4420)], vec![])
            .await
            .unwrap_err();
        assert!(error.contains("parse"));
        assert!(!invocations.exists());
        assert!(!service.status().await.rules[0].active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initialization_applies_special_and_portal_rules_once() {
        let dir = tempfile::tempdir().unwrap();
        let invocations = dir.path().join("invocations");
        let nft = mock_nft(
            dir.path(),
            &format!(
                "cat >/dev/null\nprintf x >> \"{}\"\nexit 0",
                invocations.display()
            ),
        );
        let service = test_service(dir.path(), nft, FirewallConfig::default());

        service
            .init(
                &[(Protocol::Iscsi, true), (Protocol::Nvmeof, true)],
                true,
                true,
                vec![tcp(3261)],
                vec![tcp(4421)],
                vec![PublishedAppPort {
                    app: "media".into(),
                    host_port: 32400,
                    container_port: 32400,
                    transport: "tcp".into(),
                }],
            )
            .await
            .unwrap();
        let status = service.status().await;
        assert!(status.rules.iter().any(|rule| rule.service == "rdma"));
        assert!(status.rules.iter().any(|rule| rule.service == "dc"));
        assert_eq!(status.published_app_ports.len(), 1);
        assert_eq!(
            status
                .rules
                .iter()
                .find(|rule| rule.service == "iscsi")
                .unwrap()
                .ports[0]
                .port,
            3261
        );
        assert_eq!(
            status
                .rules
                .iter()
                .find(|rule| rule.service == "nvmeof")
                .unwrap()
                .ports[0]
                .port,
            4421
        );
        assert_eq!(std::fs::read_to_string(invocations).unwrap().len(), 2);
    }

    #[test]
    fn forward_policy_allows_managed_and_custom_dnat_then_drops_the_rest() {
        let custom = vec![CustomRule {
            id: "external".into(),
            label: "external container".into(),
            transport: Transport::Udp,
            from: 9000,
            to: 9010,
            source: Some("10.0.0.0/8".into()),
            iface: Some("eth0".into()),
            enabled: true,
        }];
        let published = vec![PublishedAppPort {
            app: "media".into(),
            host_port: 32400,
            container_port: 32400,
            transport: "tcp".into(),
        }];
        let rules = render_ruleset_with_published(&FirewallState::default(), &custom, &published);
        assert!(rules.contains(
            "ct direction original ct status dnat meta l4proto tcp ct original proto-dst 32400 accept # app-published"
        ));
        assert!(rules.contains(
            "ct direction original ct status dnat iifname \"eth0\" ip saddr 10.0.0.0/8 meta l4proto udp ct original proto-dst 9000-9010 accept # custom:external"
        ));
        assert!(rules.contains("ct direction original ct status dnat drop"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistence_failure_restores_live_and_in_memory_state() {
        let dir = tempfile::tempdir().unwrap();
        let invocations = dir.path().join("invocations");
        let nft = mock_nft(
            dir.path(),
            &format!(
                "cat >> \"{}\"\nprintf '\\n---transaction---\\n' >> \"{}\"\nexit 0",
                invocations.display(),
                invocations.display()
            ),
        );
        // Renaming the staged JSON over a directory fails after candidate rules
        // were applied, forcing the compensating old-rules transaction.
        std::fs::create_dir(dir.path().join("custom.json")).unwrap();
        let service = test_service(dir.path(), nft, FirewallConfig::default());

        let error = service
            .add_custom_rule(input(9000, 9000, Transport::Tcp))
            .await
            .unwrap_err();
        assert!(error.contains("previous live rules restored"));
        assert!(service.status().await.custom_rules.is_empty());
        let recorded = std::fs::read_to_string(invocations).unwrap();
        let transactions: Vec<&str> = recorded
            .split("---transaction---")
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(transactions.len(), 4);
        assert!(transactions[0].contains("9000"));
        assert!(transactions[1].contains("9000"));
        assert!(!transactions[2].contains("9000"));
        assert!(!transactions[3].contains("9000"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rollback_failure_tracks_the_candidate_that_remains_live() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let nft = mock_nft(
            dir.path(),
            &format!(
                "cat >/dev/null\nn=$(/bin/cat \"{}\" 2>/dev/null || printf 0)\n\
                 n=$((n + 1))\nprintf '%s' \"$n\" > \"{}\"\n\
                 if [ \"$n\" -ge 3 ]; then exit 1; fi\nexit 0",
                count.display(),
                count.display()
            ),
        );
        std::fs::create_dir(dir.path().join("custom.json")).unwrap();
        let service = test_service(dir.path(), nft, FirewallConfig::default());

        let error = service
            .add_custom_rule(input(9000, 9000, Transport::Tcp))
            .await
            .unwrap_err();

        assert!(error.contains("in-memory state reflects the live candidate"));
        assert_eq!(service.status().await.custom_rules.len(), 1);
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
    fn render_uses_ip6_saddr_for_ipv6_custom_source() {
        // Regression for the fail-open bug: `ip saddr <ipv6>` is a family
        // mismatch that fails `nft -f` at load time. Because apply_nftables
        // deletes `table inet nasty` before reloading, a failed load leaves
        // the host with NO firewall table at all — policy-drop and every
        // service rule gone, fail-open. The renderer must pick `ip6 saddr`
        // for IPv6 sources.
        let state = FirewallState::default();
        let custom = vec![CustomRule {
            id: "id4".into(),
            label: "v6-restricted".into(),
            transport: Transport::Tcp,
            from: 51820,
            to: 51820,
            source: Some("2001:db8::/32".into()),
            iface: None,
            enabled: true,
        }];
        let out = render_ruleset(&state, &custom);
        assert!(
            out.contains("ip6 saddr 2001:db8::/32 tcp dport 51820 accept # custom:id4"),
            "got:\n{out}"
        );
        assert!(
            !out.contains("ip saddr 2001"),
            "must not emit the IPv4 family clause for an IPv6 source; got:\n{out}"
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
                to: None,
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
                to: None,
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

    // ── ranged service ports + dc rule set (#20) ─────────────────────

    #[test]
    fn render_emits_ranged_service_port() {
        let mut state = FirewallState::default();
        state.rules.push(FirewallRule {
            service: "dc".into(),
            ports: vec![PortSpec {
                port: 49152,
                to: Some(65535),
                transport: Transport::Tcp,
                source: None,
                iface: None,
            }],
            active: true,
        });
        let out = render_ruleset(&state, &[]);
        assert!(
            out.contains("tcp dport 49152-65535 accept # dc"),
            "got:\n{out}"
        );
    }

    #[test]
    fn service_conflict_matches_ranged_service_ports() {
        let mut state = FirewallState::default();
        state.rules.push(FirewallRule {
            service: "dc".into(),
            ports: vec![PortSpec {
                port: 49152,
                to: Some(65535),
                transport: Transport::Tcp,
                source: None,
                iface: None,
            }],
            active: false, // inactive still owns its ports
        });
        // custom range overlapping the service range → conflict
        assert_eq!(
            service_port_conflict(&state, Transport::Tcp, 50000, 50010).as_deref(),
            Some("dc")
        );
        // below the range → free
        assert_eq!(
            service_port_conflict(&state, Transport::Tcp, 40000, 49151),
            None
        );
    }

    #[test]
    fn dc_ports_cover_ad_services() {
        let ports = dc_ports();
        let has = |t: Transport, p: u16| {
            ports
                .iter()
                .any(|s| s.transport == t && s.port == p && s.to.is_none())
        };
        for p in [53u16, 88, 389, 464] {
            assert!(
                has(Transport::Tcp, p) && has(Transport::Udp, p),
                "missing tcp+udp {p}"
            );
        }
        for p in [135u16, 139, 445, 636, 3268, 3269] {
            assert!(has(Transport::Tcp, p), "missing tcp {p}");
        }
        for p in [137u16, 138] {
            assert!(has(Transport::Udp, p), "missing udp {p}");
        }
        assert!(
            ports
                .iter()
                .any(|s| s.transport == Transport::Tcp && s.port == 49152 && s.to == Some(65535)),
            "missing RPC range"
        );
        // No NTP — the box does not serve time.
        assert!(!ports.iter().any(|s| s.port == 123));
    }
}
