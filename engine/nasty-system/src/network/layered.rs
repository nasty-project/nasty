//! Layered network model — the shape we hand to NetworkManager.  See
//! `docs/network-architecture.md` for the architectural rationale.
//!
//! Two shapes live in this codebase: the persisted `NetworkConfig`
//! (the WebUI's wire format) and this `LayeredConfig`.  The persisted
//! shape conflates L2 (a bridge has members) with L3 (a bridge has an
//! IP); the layered shape splits them — `Link` is L2-only, `Address`
//! attaches L3 to a link by name.  `to_layered` converts on every
//! apply and `validate` rejects structurally-broken graphs before NM
//! sees them.

use std::collections::{HashMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    BondConfig, BondMode, BridgeConfig, InterfaceConfig, IpConfig, IpMethod, NetworkConfig,
    VlanConfig,
};

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct LayeredConfig {
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub addresses: Vec<Address>,
    /// Static routes.  Empty when `gateway` on an `Address` covers the
    /// default-route case.  Reserved for policy routing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// IP rules (policy routing).  Reserved; currently always empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub dns: Vec<String>,
}

/// L2 link — a single network interface, of any kind. The `kind` field
/// tag controls which extra fields are meaningful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Link {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Explicit MAC override. `None` lets the kernel decide (lowest
    /// member MAC for bridges, hardware MAC for physical).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(flatten)]
    pub kind: LinkKind,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkKind {
    /// A hardware NIC. Discovered, not created. Members of bonds/bridges
    /// are usually Physical (or Bond, transitively).
    Physical,
    Bond {
        #[serde(default)]
        members: Vec<String>,
        mode: BondMode,
        /// User-controlled: when true, the bond inherits its MAC
        /// from the primary member (mgmt iface preferred, else
        /// first declared member). See `BondConfig::inherit_member_mac`.
        /// Default true; the WebUI exposes an inverted "Don't inherit"
        /// checkbox.
        #[serde(default = "default_true")]
        inherit_member_mac: bool,
    },
    Bridge {
        #[serde(default)]
        members: Vec<String>,
        #[serde(default)]
        stp: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forward_delay_s: Option<u8>,
        /// Same as `LinkKind::Bond::inherit_member_mac` — controls
        /// whether the bridge adopts a member's MAC (DHCP-stable)
        /// instead of getting a random one (NM/kernel default).
        #[serde(default = "default_true")]
        inherit_member_mac: bool,
    },
    Vlan {
        parent: String,
        id: u16,
    },
}

/// L3 address binding. Multiple `Address` rows can target the same link
/// (dual-stack, additional addresses) — that's the natural shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Address {
    pub link: String,
    pub family: Family,
    pub method: AddressMethod,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cidr: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    V4,
    V6,
}

/// Same variants as `IpMethod` on the wire shape.  Aliased here so
/// the two can diverge later if NM grows finer-grained methods we
/// want to surface separately.
pub type AddressMethod = IpMethod;

/// Static route.  Currently always empty in produced configs —
/// reserved for explicit routing / per-service-binding work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Route {
    /// Routing table id. 254 (main) is the kernel default.
    #[serde(default = "default_table")]
    pub table: u32,
    /// Destination CIDR or `"default"`.
    pub dst: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub dev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<u32>,
}

fn default_table() -> u32 {
    254
}

/// Policy routing rule.  Currently always empty in produced configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rule {
    pub priority: u32,
    pub table: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iif: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oif: Option<String>,
}

// ── Converters ─────────────────────────────────────────────────

/// Project the legacy flat `NetworkConfig` onto the layered model.
/// Lossless on every legacy field.
pub fn to_layered(legacy: &NetworkConfig) -> LayeredConfig {
    let mut links = Vec::new();
    let mut addresses = Vec::new();

    for iface in &legacy.interfaces {
        links.push(Link {
            name: iface.name.clone(),
            enabled: iface.enabled,
            mtu: iface.mtu,
            mac: None,
            kind: LinkKind::Physical,
        });
        push_addresses(&mut addresses, &iface.name, &iface.ipv4, &iface.ipv6);
    }
    for bond in &legacy.bonds {
        links.push(Link {
            name: bond.name.clone(),
            enabled: true,
            mtu: bond.mtu,
            mac: None,
            kind: LinkKind::Bond {
                members: bond.members.clone(),
                mode: bond.mode.clone(),
                inherit_member_mac: bond.inherit_member_mac,
            },
        });
        push_addresses(&mut addresses, &bond.name, &bond.ipv4, &bond.ipv6);
    }
    for bridge in &legacy.bridges {
        links.push(Link {
            name: bridge.name.clone(),
            enabled: true,
            mtu: bridge.mtu,
            mac: None,
            kind: LinkKind::Bridge {
                members: bridge.members.clone(),
                stp: bridge.stp,
                forward_delay_s: bridge.forward_delay_s,
                inherit_member_mac: bridge.inherit_member_mac,
            },
        });
        push_addresses(&mut addresses, &bridge.name, &bridge.ipv4, &bridge.ipv6);
    }
    for vlan in &legacy.vlans {
        let name = format!("{}.{}", vlan.parent, vlan.vlan_id);
        links.push(Link {
            name: name.clone(),
            enabled: true,
            mtu: vlan.mtu,
            mac: None,
            kind: LinkKind::Vlan {
                parent: vlan.parent.clone(),
                id: vlan.vlan_id,
            },
        });
        push_addresses(&mut addresses, &name, &vlan.ipv4, &vlan.ipv6);
    }

    LayeredConfig {
        links,
        addresses,
        routes: Vec::new(),
        rules: Vec::new(),
        dns: legacy.dns.clone(),
    }
}

fn push_addresses(out: &mut Vec<Address>, link: &str, v4: &IpConfig, v6: &IpConfig) {
    // Skip emitting an Address row if both method and cidr/gateway say
    // "nothing here" — keeps the JSON tidy and round-trips cleanly.
    if !is_empty_ip(v4) {
        out.push(Address {
            link: link.into(),
            family: Family::V4,
            method: v4.method.clone(),
            cidr: v4.addresses.clone(),
            gateway: v4.gateway.clone(),
        });
    }
    if !is_empty_ip(v6) {
        out.push(Address {
            link: link.into(),
            family: Family::V6,
            method: v6.method.clone(),
            cidr: v6.addresses.clone(),
            gateway: v6.gateway.clone(),
        });
    }
}

fn is_empty_ip(ip: &IpConfig) -> bool {
    matches!(ip.method, IpMethod::Disabled) && ip.addresses.is_empty() && ip.gateway.is_none()
}

/// Project the layered model back to the flat `NetworkConfig` wire
/// shape.  Lossy on fields that exist only in the layered model:
/// `routes`, `rules`, `Link.mac`.  `to_layered` produces those fields
/// as empty/`None`, so a round-trip through both directions is the
/// identity for any input that started as the flat shape.
pub fn from_layered(layered: &LayeredConfig) -> NetworkConfig {
    let addrs_by_link = group_addresses(&layered.addresses);

    let mut interfaces = Vec::new();
    let mut bonds = Vec::new();
    let mut bridges = Vec::new();
    let mut vlans = Vec::new();

    for link in &layered.links {
        let (ipv4, ipv6) = addrs_by_link
            .get(link.name.as_str())
            .cloned()
            .unwrap_or_default();

        match &link.kind {
            LinkKind::Physical => interfaces.push(InterfaceConfig {
                name: link.name.clone(),
                enabled: link.enabled,
                ipv4,
                ipv6,
                mtu: link.mtu,
            }),
            LinkKind::Bond {
                members,
                mode,
                inherit_member_mac,
            } => bonds.push(BondConfig {
                name: link.name.clone(),
                members: members.clone(),
                mode: mode.clone(),
                ipv4,
                ipv6,
                mtu: link.mtu,
                inherit_member_mac: *inherit_member_mac,
            }),
            LinkKind::Bridge {
                members,
                stp,
                forward_delay_s,
                inherit_member_mac,
            } => bridges.push(BridgeConfig {
                name: link.name.clone(),
                members: members.clone(),
                ipv4,
                ipv6,
                mtu: link.mtu,
                stp: *stp,
                forward_delay_s: *forward_delay_s,
                inherit_member_mac: *inherit_member_mac,
            }),
            LinkKind::Vlan { parent, id } => vlans.push(VlanConfig {
                parent: parent.clone(),
                vlan_id: *id,
                ipv4,
                ipv6,
                mtu: link.mtu,
            }),
        }
    }

    NetworkConfig {
        interfaces,
        dns: layered.dns.clone(),
        bonds,
        vlans,
        bridges,
    }
}

fn group_addresses(addrs: &[Address]) -> HashMap<&str, (IpConfig, IpConfig)> {
    let mut out: HashMap<&str, (IpConfig, IpConfig)> = HashMap::new();
    for addr in addrs {
        let entry = out.entry(addr.link.as_str()).or_default();
        let target = match addr.family {
            Family::V4 => &mut entry.0,
            Family::V6 => &mut entry.1,
        };
        target.method = addr.method.clone();
        target.addresses = addr.cidr.clone();
        target.gateway = addr.gateway.clone();
    }
    out
}

// ── Validation ─────────────────────────────────────────────────

/// Reject configs that can't be applied: dup names, dangling refs,
/// double-enslavement, self-reference, cycles. Cheap; runs before
/// persist so structurally-broken input never reaches the kernel.
/// Callers treat the result as authoritative — a failed validation
/// is a hard apply error, not a warning.
pub fn validate(layered: &LayeredConfig) -> Result<(), String> {
    // 1. No duplicate link names.
    let mut names = HashSet::new();
    for link in &layered.links {
        if !names.insert(link.name.as_str()) {
            return Err(format!("duplicate link name '{}'", link.name));
        }
    }

    // 2. References resolve and don't self-target.
    for link in &layered.links {
        match &link.kind {
            LinkKind::Bond { members, .. } | LinkKind::Bridge { members, .. } => {
                for m in members {
                    if m == &link.name {
                        return Err(format!("link '{}' enslaves itself", link.name));
                    }
                    if !names.contains(m.as_str()) {
                        return Err(format!(
                            "link '{}' references missing member '{m}'",
                            link.name
                        ));
                    }
                }
            }
            LinkKind::Vlan { parent, .. } => {
                if parent == &link.name {
                    return Err(format!("vlan '{}' is its own parent", link.name));
                }
                if !names.contains(parent.as_str()) {
                    return Err(format!(
                        "vlan '{}' references missing parent '{parent}'",
                        link.name
                    ));
                }
            }
            LinkKind::Physical => {}
        }
    }

    // 3. Address links resolve.
    for addr in &layered.addresses {
        if !names.contains(addr.link.as_str()) {
            return Err(format!("address references missing link '{}'", addr.link));
        }
    }

    // 4. Each link is the member of at most one master.
    let mut master_of: HashMap<&str, &str> = HashMap::new();
    for link in &layered.links {
        if let LinkKind::Bond { members, .. } | LinkKind::Bridge { members, .. } = &link.kind {
            for m in members {
                if let Some(&existing) = master_of.get(m.as_str()) {
                    return Err(format!(
                        "link '{m}' is a member of both '{existing}' and '{}'",
                        link.name
                    ));
                }
                master_of.insert(m.as_str(), link.name.as_str());
            }
        }
    }

    // 5. No cycles in the membership graph (three-color DFS).
    detect_cycles(&layered.links)?;

    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Color {
    Gray,
    Black,
}

fn detect_cycles(links: &[Link]) -> Result<(), String> {
    let by_name: HashMap<&str, &Link> = links.iter().map(|l| (l.name.as_str(), l)).collect();
    let mut color: HashMap<String, Color> = HashMap::new();
    for link in links {
        visit_for_cycles(&link.name, &by_name, &mut color)?;
    }
    Ok(())
}

fn visit_for_cycles(
    node: &str,
    by_name: &HashMap<&str, &Link>,
    color: &mut HashMap<String, Color>,
) -> Result<(), String> {
    match color.get(node).copied() {
        Some(Color::Black) => return Ok(()),
        Some(Color::Gray) => return Err(format!("cycle detected through link '{node}'")),
        None => {}
    }
    color.insert(node.to_string(), Color::Gray);
    if let Some(link) = by_name.get(node) {
        for dep in deps(link) {
            visit_for_cycles(dep, by_name, color)?;
        }
    }
    color.insert(node.to_string(), Color::Black);
    Ok(())
}

fn deps(link: &Link) -> Vec<&str> {
    match &link.kind {
        LinkKind::Bond { members, .. } | LinkKind::Bridge { members, .. } => {
            members.iter().map(String::as_str).collect()
        }
        LinkKind::Vlan { parent, .. } => vec![parent.as_str()],
        LinkKind::Physical => Vec::new(),
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_iface(name: &str) -> InterfaceConfig {
        InterfaceConfig {
            name: name.to_string(),
            enabled: true,
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: None,
        }
    }

    fn legacy_bridge(name: &str, members: &[&str]) -> BridgeConfig {
        BridgeConfig {
            name: name.to_string(),
            members: members.iter().map(|s| (*s).to_string()).collect(),
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: None,
            stp: false,
            forward_delay_s: None,
            inherit_member_mac: false,
        }
    }

    fn legacy_bond(name: &str, members: &[&str]) -> BondConfig {
        BondConfig {
            name: name.to_string(),
            members: members.iter().map(|s| (*s).to_string()).collect(),
            mode: BondMode::Lacp,
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: None,
            inherit_member_mac: false,
        }
    }

    // ── Round-trip identity ────────────────────────────────────

    fn assert_roundtrip(legacy: NetworkConfig) {
        let layered = to_layered(&legacy);
        let back = from_layered(&layered);
        assert_eq!(
            legacy, back,
            "legacy → layered → legacy should be identity\nlegacy: {legacy:#?}\nback: {back:#?}",
        );
    }

    #[test]
    fn roundtrip_empty() {
        assert_roundtrip(NetworkConfig::default());
    }

    #[test]
    fn roundtrip_interface_only() {
        let mut eth0 = legacy_iface("eth0");
        eth0.mtu = Some(9000);
        eth0.ipv4 = IpConfig {
            method: IpMethod::Static,
            addresses: vec!["192.168.1.10/24".into()],
            gateway: Some("192.168.1.1".into()),
        };
        assert_roundtrip(NetworkConfig {
            interfaces: vec![eth0],
            ..Default::default()
        });
    }

    #[test]
    fn roundtrip_bridge_with_inherited_ipv4_and_ipv6() {
        let mut br = legacy_bridge("br0", &["eth0"]);
        br.ipv4 = IpConfig {
            method: IpMethod::Inherit,
            addresses: vec![],
            gateway: None,
        };
        br.ipv6 = IpConfig {
            method: IpMethod::Slaac,
            addresses: vec![],
            gateway: None,
        };
        br.stp = true;
        br.forward_delay_s = Some(0);
        assert_roundtrip(NetworkConfig {
            interfaces: vec![legacy_iface("eth0")],
            bridges: vec![br],
            ..Default::default()
        });
    }

    #[test]
    fn roundtrip_bond_bridge_vlan_dns_full_stack() {
        // Full topology stack: physical → bond → bridge → vlan, plus DNS.
        let mut br = legacy_bridge("br0", &["bond0"]);
        br.ipv4 = IpConfig {
            method: IpMethod::Static,
            addresses: vec!["10.0.0.1/24".into()],
            gateway: None,
        };
        let mut vlan = VlanConfig {
            parent: "br0".into(),
            vlan_id: 10,
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: Some(1400),
        };
        vlan.ipv4 = IpConfig {
            method: IpMethod::Dhcp,
            addresses: vec![],
            gateway: None,
        };
        assert_roundtrip(NetworkConfig {
            interfaces: vec![legacy_iface("eth0"), legacy_iface("eth1")],
            dns: vec!["1.1.1.1".into(), "1.0.0.1".into()],
            bonds: vec![legacy_bond("bond0", &["eth0", "eth1"])],
            vlans: vec![vlan],
            bridges: vec![br],
        });
    }

    #[test]
    fn roundtrip_disabled_interface_stays_disabled() {
        let mut eth0 = legacy_iface("eth0");
        eth0.enabled = false;
        assert_roundtrip(NetworkConfig {
            interfaces: vec![eth0],
            ..Default::default()
        });
    }

    // ── Layered shape spot-checks ──────────────────────────────

    #[test]
    fn to_layered_emits_no_address_for_default_disabled_iface() {
        let layered = to_layered(&NetworkConfig {
            interfaces: vec![legacy_iface("eth0")],
            ..Default::default()
        });
        assert_eq!(layered.links.len(), 1);
        assert!(
            layered.addresses.is_empty(),
            "default Disabled iface should not produce an Address row"
        );
    }

    #[test]
    fn to_layered_separates_v4_and_v6_addresses() {
        let mut eth0 = legacy_iface("eth0");
        eth0.ipv4 = IpConfig {
            method: IpMethod::Dhcp,
            ..Default::default()
        };
        eth0.ipv6 = IpConfig {
            method: IpMethod::Slaac,
            ..Default::default()
        };
        let layered = to_layered(&NetworkConfig {
            interfaces: vec![eth0],
            ..Default::default()
        });
        let v4 = layered.addresses.iter().find(|a| a.family == Family::V4);
        let v6 = layered.addresses.iter().find(|a| a.family == Family::V6);
        assert_eq!(v4.unwrap().method, IpMethod::Dhcp);
        assert_eq!(v6.unwrap().method, IpMethod::Slaac);
    }

    #[test]
    fn to_layered_vlan_synthesizes_parent_dot_id_name() {
        let layered = to_layered(&NetworkConfig {
            interfaces: vec![legacy_iface("eth0")],
            vlans: vec![VlanConfig {
                parent: "eth0".into(),
                vlan_id: 100,
                ipv4: IpConfig::default(),
                ipv6: IpConfig::default(),
                mtu: None,
            }],
            ..Default::default()
        });
        let vlan_link = layered
            .links
            .iter()
            .find(|l| matches!(l.kind, LinkKind::Vlan { .. }))
            .unwrap();
        assert_eq!(vlan_link.name, "eth0.100");
    }

    // ── Validation ─────────────────────────────────────────────

    fn link_phys(name: &str) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            kind: LinkKind::Physical,
        }
    }

    fn link_bridge(name: &str, members: &[&str]) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            kind: LinkKind::Bridge {
                members: members.iter().map(|s| (*s).to_string()).collect(),
                stp: false,
                forward_delay_s: None,
                inherit_member_mac: false,
            },
        }
    }

    fn link_bond(name: &str, members: &[&str]) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            kind: LinkKind::Bond {
                members: members.iter().map(|s| (*s).to_string()).collect(),
                mode: BondMode::Lacp,
                inherit_member_mac: false,
            },
        }
    }

    fn link_vlan(name: &str, parent: &str, id: u16) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            kind: LinkKind::Vlan {
                parent: parent.into(),
                id,
            },
        }
    }

    #[test]
    fn validate_passes_empty() {
        validate(&LayeredConfig::default()).unwrap();
    }

    #[test]
    fn validate_passes_full_stack() {
        let cfg = LayeredConfig {
            links: vec![
                link_phys("eth0"),
                link_phys("eth1"),
                link_bond("bond0", &["eth0", "eth1"]),
                link_bridge("br0", &["bond0"]),
                link_vlan("br0.10", "br0", 10),
            ],
            addresses: vec![Address {
                link: "br0".into(),
                family: Family::V4,
                method: IpMethod::Dhcp,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_duplicate_link_names() {
        let cfg = LayeredConfig {
            links: vec![link_phys("eth0"), link_phys("eth0")],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("duplicate"));
        assert!(err.contains("eth0"));
    }

    #[test]
    fn validate_rejects_bond_with_missing_member() {
        let cfg = LayeredConfig {
            links: vec![link_phys("eth0"), link_bond("bond0", &["eth0", "eth9"])],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("missing member"));
        assert!(err.contains("eth9"));
    }

    #[test]
    fn validate_rejects_self_enslavement() {
        let cfg = LayeredConfig {
            links: vec![link_bridge("br0", &["br0"])],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("enslaves itself"));
    }

    #[test]
    fn validate_rejects_vlan_with_missing_parent() {
        let cfg = LayeredConfig {
            links: vec![link_vlan("ghost.10", "ghost", 10)],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("missing parent"));
        assert!(err.contains("ghost"));
    }

    #[test]
    fn validate_rejects_vlan_as_own_parent() {
        let cfg = LayeredConfig {
            links: vec![link_vlan("v", "v", 1)],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("own parent"));
    }

    #[test]
    fn validate_rejects_address_with_missing_link() {
        let cfg = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "nonexistent".into(),
                family: Family::V4,
                method: IpMethod::Dhcp,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("missing link"));
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn validate_rejects_double_enslavement() {
        let cfg = LayeredConfig {
            links: vec![
                link_phys("eth0"),
                link_bridge("br0", &["eth0"]),
                link_bridge("br1", &["eth0"]),
            ],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("member of both"));
    }

    #[test]
    fn validate_rejects_membership_cycle() {
        // br0 includes br1, br1 includes br0. The double-enslavement
        // check fires first if both lists have all members; this case
        // is constructed so each bridge has only the *other*, exercising
        // the cycle detector specifically.
        let cfg = LayeredConfig {
            links: vec![link_bridge("br0", &["br1"]), link_bridge("br1", &["br0"])],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn validate_passes_bridge_with_no_members() {
        // Host-internal bridge for VMs — common shape, must validate.
        let cfg = LayeredConfig {
            links: vec![link_bridge("vmbr0", &[])],
            ..Default::default()
        };
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_passes_long_legitimate_chain() {
        // eth0 → bond0 → br0 → br0.10 (vlan). No cycles, all references
        // resolve. The cycle detector should reach Black and accept.
        let cfg = LayeredConfig {
            links: vec![
                link_phys("eth0"),
                link_phys("eth1"),
                link_bond("bond0", &["eth0", "eth1"]),
                link_bridge("br0", &["bond0"]),
                link_vlan("br0.10", "br0", 10),
            ],
            ..Default::default()
        };
        validate(&cfg).unwrap();
    }
}
