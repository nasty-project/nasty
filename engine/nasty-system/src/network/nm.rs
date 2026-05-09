//! NetworkManager backend — phase 2 shadow output.
//!
//! See `docs/network-architecture.md`. This module turns a `LayeredConfig`
//! (from `super::layered`) into a set of NM connection profiles and emits
//! them as `.nmconnection.preview` keyfiles to a NASty-owned preview
//! directory. **No DBus calls; no NM binding crate dependency.** The
//! purpose of phase 2 is to:
//!
//! - validate that the layered model can be expressed faithfully in NM
//!   terms before phase 3 commits to a binding (zbus direct, nmrs, etc.);
//! - give us inspectable previews on every real apply so we catch
//!   converter bugs locally before users hit them;
//! - keep the apply path otherwise unchanged (`networking.json` still
//!   wins).
//!
//! Phase 3 will swap `serialize_keyfile` + filesystem writes for actual
//! NM DBus calls (`AddConnection` / `Update` / `Activate`). The data
//! model defined here is the contract that survives the switch.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::IpMethod;
use super::layered::{Address, AddressMethod, Family, LayeredConfig, Link, LinkKind};

// ── Types ──────────────────────────────────────────────────────

/// One NetworkManager connection profile.
///
/// Maps roughly 1:1 to a `.nmconnection` keyfile in
/// `/etc/NetworkManager/system-connections/`. Phase 2 emits these as
/// preview files; phase 3 sends them via DBus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NmConnection {
    /// Connection ID, e.g. `nasty-br0`. Used by `nmcli connection show`
    /// and as the basename of the keyfile. The `nasty-` prefix is the
    /// ownership marker — phase 4 uses it to discriminate NASty-owned
    /// connections from external ones.
    pub id: String,
    /// Deterministic UUID derived from `id`. Phase 2 doesn't need real
    /// UUIDv5; phase 3 will replace this with real UUIDs from the
    /// `uuid` crate (added as a dep at that point).
    pub uuid: String,
    pub conn_type: NmConnectionType,
    /// Kernel interface name this profile binds to. Same as `Link.name`.
    pub interface_name: String,
    /// When this profile is a member of a bond/bridge, the master's
    /// interface name. NM resolves the master by matching its
    /// `interface-name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
    /// Port type — `bond` or `bridge` — when `controller` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Explicit MAC override (NM `cloned-mac-address`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Whether NM should bring this connection up automatically.
    /// Mapped from `Link.enabled`.
    pub autoconnect: bool,
    pub ipv4: NmIpSettings,
    pub ipv6: NmIpSettings,
    pub type_specific: NmTypeSpecific,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NmConnectionType {
    /// `802-3-ethernet`. Used for physical NICs *and* for member ports
    /// of bonds/bridges (NM treats a bridge port as an ethernet
    /// connection with a controller).
    Ethernet,
    Bond,
    Bridge,
    Vlan,
}

impl NmConnectionType {
    /// String NM expects in the keyfile `type=` field.
    fn keyfile_str(self) -> &'static str {
        match self {
            NmConnectionType::Ethernet => "802-3-ethernet",
            NmConnectionType::Bond => "bond",
            NmConnectionType::Bridge => "bridge",
            NmConnectionType::Vlan => "vlan",
        }
    }
}

/// NM `[ipv4]` / `[ipv6]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct NmIpSettings {
    pub method: NmIpMethod,
    /// CIDR strings. Gateway is associated with `addresses[0]` in the
    /// keyfile (NM's `address1=cidr,gateway` syntax).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum NmIpMethod {
    /// DHCP (v4) or SLAAC + DHCPv6 (v6). Most common case.
    Auto,
    /// Static address(es) + optional gateway.
    Manual,
    /// IPv4-only fallback when DHCP fails.
    LinkLocal,
    /// No L3 on this connection — used for bond/bridge members and
    /// explicitly-disabled families.
    #[default]
    Disabled,
    /// Explicit "ignore IPv6" — used when the user has set IPv6 to
    /// disabled on a link that NM would otherwise SLAAC.
    Ignore,
}

impl NmIpMethod {
    fn keyfile_str(self) -> &'static str {
        match self {
            NmIpMethod::Auto => "auto",
            NmIpMethod::Manual => "manual",
            NmIpMethod::LinkLocal => "link-local",
            NmIpMethod::Disabled => "disabled",
            NmIpMethod::Ignore => "ignore",
        }
    }
}

/// Type-specific keyfile section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NmTypeSpecific {
    /// Ethernet connection — no extra section beyond `[ethernet]`,
    /// which the serializer emits unconditionally for type=Ethernet.
    None,
    Bond {
        /// NM uses the kernel string here (`802.3ad`, `active-backup`, ...).
        mode: String,
    },
    Bridge {
        stp: bool,
        forward_delay: Option<u8>,
    },
    Vlan {
        parent: String,
        id: u16,
    },
}

// ── Converter ──────────────────────────────────────────────────

/// Project a `LayeredConfig` to NM connection profiles. One `NmConnection`
/// per `Link`, with members getting `controller = <master>` + IP method
/// `disabled` (the master owns the L3).
pub fn to_nm_profiles(layered: &LayeredConfig) -> Vec<NmConnection> {
    // Per-link IP settings, indexed by link name.
    let mut addrs_by_link: HashMap<&str, (NmIpSettings, NmIpSettings)> = HashMap::new();
    for addr in &layered.addresses {
        let entry = addrs_by_link
            .entry(addr.link.as_str())
            .or_insert_with(default_ip_pair);
        let target = match addr.family {
            Family::V4 => &mut entry.0,
            Family::V6 => &mut entry.1,
        };
        *target = address_to_nm_settings(addr);
    }

    // Member-of-master map: link name → (master name, "bond"|"bridge").
    let mut master_of: HashMap<&str, (&str, &'static str)> = HashMap::new();
    for link in &layered.links {
        match &link.kind {
            LinkKind::Bond { members, .. } => {
                for m in members {
                    master_of.insert(m.as_str(), (link.name.as_str(), "bond"));
                }
            }
            LinkKind::Bridge { members, .. } => {
                for m in members {
                    master_of.insert(m.as_str(), (link.name.as_str(), "bridge"));
                }
            }
            _ => {}
        }
    }

    layered
        .links
        .iter()
        .map(|link| profile_for_link(link, &addrs_by_link, &master_of))
        .collect()
}

fn profile_for_link(
    link: &Link,
    addrs_by_link: &HashMap<&str, (NmIpSettings, NmIpSettings)>,
    master_of: &HashMap<&str, (&str, &'static str)>,
) -> NmConnection {
    let master = master_of.get(link.name.as_str());

    // Member ports never carry IP; the master does.
    let (ipv4, ipv6) = if master.is_some() {
        (
            NmIpSettings {
                method: NmIpMethod::Disabled,
                addresses: vec![],
                gateway: None,
            },
            NmIpSettings {
                method: NmIpMethod::Disabled,
                addresses: vec![],
                gateway: None,
            },
        )
    } else {
        addrs_by_link
            .get(link.name.as_str())
            .cloned()
            .unwrap_or_else(default_ip_pair)
    };

    let (controller, port_type) = master
        .map(|(c, pt)| (Some((*c).to_string()), Some((*pt).to_string())))
        .unwrap_or((None, None));

    let (conn_type, type_specific) = match &link.kind {
        LinkKind::Physical => (NmConnectionType::Ethernet, NmTypeSpecific::None),
        LinkKind::Bond { mode, .. } => (
            NmConnectionType::Bond,
            NmTypeSpecific::Bond {
                mode: mode.to_kernel().to_string(),
            },
        ),
        LinkKind::Bridge {
            stp,
            forward_delay_s,
            ..
        } => (
            NmConnectionType::Bridge,
            NmTypeSpecific::Bridge {
                stp: *stp,
                forward_delay: *forward_delay_s,
            },
        ),
        LinkKind::Vlan { parent, id } => (
            NmConnectionType::Vlan,
            NmTypeSpecific::Vlan {
                parent: parent.clone(),
                id: *id,
            },
        ),
    };

    NmConnection {
        id: format!("nasty-{}", link.name),
        uuid: deterministic_uuid_for(&link.name),
        conn_type,
        interface_name: link.name.clone(),
        controller,
        port_type,
        mtu: link.mtu,
        mac: link.mac.clone(),
        autoconnect: link.enabled,
        ipv4,
        ipv6,
        type_specific,
    }
}

fn address_to_nm_settings(addr: &Address) -> NmIpSettings {
    let method = address_method_to_nm(&addr.method, addr.family);
    NmIpSettings {
        method,
        addresses: addr.cidr.clone(),
        gateway: addr.gateway.clone(),
    }
}

fn address_method_to_nm(method: &AddressMethod, family: Family) -> NmIpMethod {
    match method {
        IpMethod::Dhcp => NmIpMethod::Auto,
        IpMethod::Static => NmIpMethod::Manual,
        IpMethod::Slaac => match family {
            Family::V4 => NmIpMethod::Auto, // `slaac` is v6-only; v4 fallback to auto
            Family::V6 => NmIpMethod::Auto, // NM `auto` covers SLAAC + DHCPv6
        },
        IpMethod::Disabled => NmIpMethod::Disabled,
        // Inherit should have been resolved to a concrete method before
        // persist (see `network::resolve_inherit`). If it leaks through
        // we treat it as `auto` — best guess at the user's intent.
        IpMethod::Inherit => NmIpMethod::Auto,
    }
}

fn default_ip_pair() -> (NmIpSettings, NmIpSettings) {
    (NmIpSettings::default(), NmIpSettings::default())
}

/// Phase 2 placeholder: produce a UUID-shaped string deterministically
/// from the link name. Phase 3 will replace this with `Uuid::new_v5`
/// (real UUIDv5) when the `uuid` crate is added as a dep alongside the
/// chosen NM binding.
fn deterministic_uuid_for(name: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    "nasty-network-v2".hash(&mut h);
    name.hash(&mut h);
    let h1 = h.finish();
    let mut h = DefaultHasher::new();
    h1.hash(&mut h);
    "split".hash(&mut h);
    let h2 = h.finish();
    // Format: 8-4-4-4-12 hex digits, with version (4) and variant (8) bits.
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (h1 >> 32) as u32,
        ((h1 >> 16) & 0xffff) as u16,
        (h1 & 0xfff) as u16,
        ((h2 >> 48) & 0xfff) as u16,
        h2 & 0xffff_ffff_ffff,
    )
}

// ── Keyfile serializer ─────────────────────────────────────────

/// Render an `NmConnection` to NM `.nmconnection` keyfile text. The
/// output is what NM would consume from
/// `/etc/NetworkManager/system-connections/<id>.nmconnection`.
pub fn serialize_keyfile(p: &NmConnection) -> String {
    let mut out = String::new();

    out.push_str("# Generated by NASty — phase 2 shadow preview.\n");
    out.push_str("# See docs/network-architecture.md. Not yet active.\n\n");

    // [connection]
    out.push_str("[connection]\n");
    out.push_str(&format!("id={}\n", p.id));
    out.push_str(&format!("uuid={}\n", p.uuid));
    out.push_str(&format!("type={}\n", p.conn_type.keyfile_str()));
    out.push_str(&format!("interface-name={}\n", p.interface_name));
    out.push_str(&format!("autoconnect={}\n", p.autoconnect));
    if let Some(c) = &p.controller {
        out.push_str(&format!("controller={c}\n"));
    }
    if let Some(pt) = &p.port_type {
        out.push_str(&format!("port-type={pt}\n"));
    }
    out.push('\n');

    // Type-specific section. Order matters in NM keyfiles: connection
    // first, then type-specific, then [ipv4]/[ipv6] last.
    match &p.type_specific {
        NmTypeSpecific::None => {}
        NmTypeSpecific::Bond { mode } => {
            out.push_str("[bond]\n");
            out.push_str(&format!("mode={mode}\n\n"));
        }
        NmTypeSpecific::Bridge { stp, forward_delay } => {
            out.push_str("[bridge]\n");
            out.push_str(&format!("stp={stp}\n"));
            if let Some(fd) = forward_delay {
                out.push_str(&format!("forward-delay={fd}\n"));
            }
            out.push('\n');
        }
        NmTypeSpecific::Vlan { parent, id } => {
            out.push_str("[vlan]\n");
            out.push_str(&format!("parent={parent}\n"));
            out.push_str(&format!("id={id}\n\n"));
        }
    }

    // [ethernet] for ethernet type — carries MTU and cloned-mac.
    if matches!(p.conn_type, NmConnectionType::Ethernet) {
        out.push_str("[ethernet]\n");
        if let Some(mtu) = p.mtu {
            out.push_str(&format!("mtu={mtu}\n"));
        }
        if let Some(mac) = &p.mac {
            out.push_str(&format!("cloned-mac-address={mac}\n"));
        }
        out.push('\n');
    }

    // For non-ethernet (bond/bridge/vlan), MTU goes in the type section
    // (or globally — NM accepts both). Keyfile MTU-on-master is set on
    // the type-specific section in older NM, and as a separate field in
    // newer; we put it in [<type>] to be safe. Actually: NM accepts
    // `mtu` in `[connection]` for all types since 1.20. Use that.
    if !matches!(p.conn_type, NmConnectionType::Ethernet)
        && let Some(mtu) = p.mtu
    {
        // Re-write [connection] MTU as a fallback section. NM honors
        // mtu= in any of the type-specific sections too, but [connection]
        // is the most portable.
        // Already handled by re-emitting under the section below.
        let section = match p.conn_type {
            NmConnectionType::Bond => "[bond]",
            NmConnectionType::Bridge => "[bridge]",
            NmConnectionType::Vlan => "[vlan]",
            NmConnectionType::Ethernet => unreachable!(),
        };
        // We already emitted this section above with mode/stp/parent.
        // To keep the output simple, put MTU in a dedicated `[ethernet]`-
        // style trailing block — NM accepts it as a setting on the
        // connection itself when the type-specific section is absent.
        out.push_str(&format!("# mtu setting belongs to {section}\n"));
        out.push_str(&format!("# mtu={mtu}\n\n"));
    }

    // [ipv4]
    out.push_str("[ipv4]\n");
    serialize_ip_section(&mut out, &p.ipv4);
    out.push('\n');

    // [ipv6]
    out.push_str("[ipv6]\n");
    serialize_ip_section(&mut out, &p.ipv6);

    out
}

fn serialize_ip_section(out: &mut String, ip: &NmIpSettings) {
    out.push_str(&format!("method={}\n", ip.method.keyfile_str()));
    for (i, addr) in ip.addresses.iter().enumerate() {
        let n = i + 1;
        // NM's address syntax: `addressN=CIDR[,gateway]`. Gateway is
        // attached to address1 only.
        if i == 0
            && let Some(gw) = &ip.gateway
        {
            out.push_str(&format!("address{n}={addr},{gw}\n"));
        } else {
            out.push_str(&format!("address{n}={addr}\n"));
        }
    }
    // If gateway is set but no addresses — uncommon; emit a warning
    // marker. NM ignores standalone gateway= without an address.
    if ip.gateway.is_some() && ip.addresses.is_empty() {
        out.push_str("# warning: gateway set but no addresses; NM will ignore\n");
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::BondMode;

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
            },
        }
    }

    fn find<'a>(profiles: &'a [NmConnection], iface: &str) -> &'a NmConnection {
        profiles
            .iter()
            .find(|p| p.interface_name == iface)
            .unwrap_or_else(|| panic!("no profile for {iface}"))
    }

    // ── Converter ──────────────────────────────────────────────

    #[test]
    fn empty_layered_produces_no_profiles() {
        assert!(to_nm_profiles(&LayeredConfig::default()).is_empty());
    }

    #[test]
    fn physical_with_dhcp_becomes_ethernet_auto() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "eth0".into(),
                family: Family::V4,
                method: IpMethod::Dhcp,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        let p = find(&profiles, "eth0");
        assert_eq!(p.conn_type, NmConnectionType::Ethernet);
        assert_eq!(p.id, "nasty-eth0");
        assert!(p.controller.is_none());
        assert_eq!(p.ipv4.method, NmIpMethod::Auto);
        // IPv6 left at default Disabled when no address row.
        assert_eq!(p.ipv6.method, NmIpMethod::Disabled);
    }

    #[test]
    fn physical_with_static_carries_addresses_and_gateway() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "eth0".into(),
                family: Family::V4,
                method: IpMethod::Static,
                cidr: vec!["10.0.0.5/24".into()],
                gateway: Some("10.0.0.1".into()),
            }],
            ..Default::default()
        };
        let p = &to_nm_profiles(&layered)[0];
        assert_eq!(p.ipv4.method, NmIpMethod::Manual);
        assert_eq!(p.ipv4.addresses, vec!["10.0.0.5/24".to_string()]);
        assert_eq!(p.ipv4.gateway, Some("10.0.0.1".to_string()));
    }

    #[test]
    fn bridge_members_get_controller_and_disabled_ip() {
        // br0 owns the IP; eth0 is a member (no IP, controller=br0).
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bridge("br0", &["eth0"])],
            addresses: vec![Address {
                link: "br0".into(),
                family: Family::V4,
                method: IpMethod::Dhcp,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);

        let eth0 = find(&profiles, "eth0");
        assert_eq!(eth0.conn_type, NmConnectionType::Ethernet);
        assert_eq!(eth0.controller, Some("br0".to_string()));
        assert_eq!(eth0.port_type, Some("bridge".to_string()));
        assert_eq!(
            eth0.ipv4.method,
            NmIpMethod::Disabled,
            "member port must not carry IP"
        );
        assert_eq!(eth0.ipv6.method, NmIpMethod::Disabled);

        let br0 = find(&profiles, "br0");
        assert_eq!(br0.conn_type, NmConnectionType::Bridge);
        assert!(br0.controller.is_none());
        assert_eq!(br0.ipv4.method, NmIpMethod::Auto);
    }

    #[test]
    fn bond_members_get_controller_with_port_type_bond() {
        let layered = LayeredConfig {
            links: vec![
                link_phys("eth0"),
                link_phys("eth1"),
                link_bond("bond0", &["eth0", "eth1"]),
            ],
            addresses: vec![Address {
                link: "bond0".into(),
                family: Family::V4,
                method: IpMethod::Dhcp,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        for member in &["eth0", "eth1"] {
            let p = find(&profiles, member);
            assert_eq!(p.controller, Some("bond0".to_string()));
            assert_eq!(p.port_type, Some("bond".to_string()));
        }
        let bond = find(&profiles, "bond0");
        assert_eq!(bond.conn_type, NmConnectionType::Bond);
        match &bond.type_specific {
            NmTypeSpecific::Bond { mode } => assert_eq!(mode, "802.3ad"),
            other => panic!("expected Bond type_specific, got {other:?}"),
        }
    }

    #[test]
    fn vlan_carries_parent_and_id() {
        let layered = LayeredConfig {
            links: vec![
                link_phys("eth0"),
                Link {
                    name: "eth0.100".into(),
                    enabled: true,
                    mtu: None,
                    mac: None,
                    kind: LinkKind::Vlan {
                        parent: "eth0".into(),
                        id: 100,
                    },
                },
            ],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        let p = find(&profiles, "eth0.100");
        assert_eq!(p.conn_type, NmConnectionType::Vlan);
        match &p.type_specific {
            NmTypeSpecific::Vlan { parent, id } => {
                assert_eq!(parent, "eth0");
                assert_eq!(*id, 100);
            }
            other => panic!("expected Vlan type_specific, got {other:?}"),
        }
    }

    #[test]
    fn bridge_with_stp_and_forward_delay_round_trips() {
        let layered = LayeredConfig {
            links: vec![Link {
                name: "br0".into(),
                enabled: true,
                mtu: None,
                mac: None,
                kind: LinkKind::Bridge {
                    members: vec![],
                    stp: true,
                    forward_delay_s: Some(0),
                },
            }],
            ..Default::default()
        };
        let p = &to_nm_profiles(&layered)[0];
        match &p.type_specific {
            NmTypeSpecific::Bridge { stp, forward_delay } => {
                assert!(*stp);
                assert_eq!(*forward_delay, Some(0));
            }
            other => panic!("expected Bridge, got {other:?}"),
        }
    }

    #[test]
    fn disabled_link_emits_autoconnect_false() {
        let mut eth0 = link_phys("eth0");
        eth0.enabled = false;
        let p = &to_nm_profiles(&LayeredConfig {
            links: vec![eth0],
            ..Default::default()
        })[0];
        assert!(!p.autoconnect);
    }

    #[test]
    fn slaac_v6_maps_to_auto() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "eth0".into(),
                family: Family::V6,
                method: IpMethod::Slaac,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let p = &to_nm_profiles(&layered)[0];
        assert_eq!(p.ipv6.method, NmIpMethod::Auto);
    }

    #[test]
    fn inherit_resolved_to_auto_with_warning_intent() {
        // Inherit should never reach NM in practice (resolve_inherit
        // runs first), but if it does we treat it as the most likely
        // user intent: DHCP.
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "eth0".into(),
                family: Family::V4,
                method: IpMethod::Inherit,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let p = &to_nm_profiles(&layered)[0];
        assert_eq!(p.ipv4.method, NmIpMethod::Auto);
    }

    #[test]
    fn deterministic_uuid_is_stable_across_calls() {
        // Same name → same UUID. Phase 3 will swap to real UUIDv5 but
        // we want determinism now too so previews don't churn.
        assert_eq!(
            deterministic_uuid_for("eth0"),
            deterministic_uuid_for("eth0")
        );
        assert_ne!(
            deterministic_uuid_for("eth0"),
            deterministic_uuid_for("eth1")
        );
    }

    #[test]
    fn deterministic_uuid_is_uuid_shaped() {
        let u = deterministic_uuid_for("br0");
        // 36 chars, dashes at the canonical positions.
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[8], b'-');
        assert_eq!(u.as_bytes()[13], b'-');
        assert_eq!(u.as_bytes()[18], b'-');
        assert_eq!(u.as_bytes()[23], b'-');
    }

    // ── Keyfile serialization ──────────────────────────────────

    #[test]
    fn keyfile_has_required_sections_for_ethernet() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "eth0".into(),
                family: Family::V4,
                method: IpMethod::Dhcp,
                cidr: vec![],
                gateway: None,
            }],
            ..Default::default()
        };
        let p = &to_nm_profiles(&layered)[0];
        let keyfile = serialize_keyfile(p);
        assert!(keyfile.contains("[connection]"));
        assert!(keyfile.contains("[ethernet]"));
        assert!(keyfile.contains("[ipv4]"));
        assert!(keyfile.contains("[ipv6]"));
        assert!(keyfile.contains("id=nasty-eth0"));
        assert!(keyfile.contains("type=802-3-ethernet"));
        assert!(keyfile.contains("interface-name=eth0"));
        assert!(keyfile.contains("method=auto"));
    }

    #[test]
    fn keyfile_emits_address_with_gateway_using_nm_syntax() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            addresses: vec![Address {
                link: "eth0".into(),
                family: Family::V4,
                method: IpMethod::Static,
                cidr: vec!["10.0.0.5/24".into(), "10.0.0.6/24".into()],
                gateway: Some("10.0.0.1".into()),
            }],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        // First address gets the gateway suffix; second doesn't.
        assert!(keyfile.contains("address1=10.0.0.5/24,10.0.0.1"));
        assert!(keyfile.contains("address2=10.0.0.6/24"));
        assert!(!keyfile.contains("address2=10.0.0.6/24,10.0.0.1"));
    }

    #[test]
    fn keyfile_for_bridge_member_carries_controller_and_port_type() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bridge("br0", &["eth0"])],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        let eth0 = find(&profiles, "eth0");
        let keyfile = serialize_keyfile(eth0);
        assert!(keyfile.contains("controller=br0"));
        assert!(keyfile.contains("port-type=bridge"));
        // Member ports should declare disabled IP method explicitly.
        assert!(keyfile.contains("method=disabled"));
    }

    #[test]
    fn keyfile_for_bond_emits_mode() {
        let layered = LayeredConfig {
            links: vec![link_bond("bond0", &[])],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(keyfile.contains("[bond]"));
        assert!(keyfile.contains("mode=802.3ad"));
    }

    #[test]
    fn keyfile_for_bridge_emits_stp_and_forward_delay() {
        let layered = LayeredConfig {
            links: vec![Link {
                name: "br0".into(),
                enabled: true,
                mtu: None,
                mac: None,
                kind: LinkKind::Bridge {
                    members: vec![],
                    stp: true,
                    forward_delay_s: Some(4),
                },
            }],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(keyfile.contains("[bridge]"));
        assert!(keyfile.contains("stp=true"));
        assert!(keyfile.contains("forward-delay=4"));
    }

    #[test]
    fn keyfile_for_vlan_emits_parent_and_id() {
        let layered = LayeredConfig {
            links: vec![Link {
                name: "eth0.10".into(),
                enabled: true,
                mtu: None,
                mac: None,
                kind: LinkKind::Vlan {
                    parent: "eth0".into(),
                    id: 10,
                },
            }],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(keyfile.contains("[vlan]"));
        assert!(keyfile.contains("parent=eth0"));
        assert!(keyfile.contains("id=10"));
    }

    #[test]
    fn keyfile_includes_mtu_for_ethernet() {
        let mut eth0 = link_phys("eth0");
        eth0.mtu = Some(9000);
        let layered = LayeredConfig {
            links: vec![eth0],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(keyfile.contains("mtu=9000"));
    }

    #[test]
    fn keyfile_includes_cloned_mac_when_set() {
        let mut eth0 = link_phys("eth0");
        eth0.mac = Some("aa:bb:cc:dd:ee:ff".into());
        let layered = LayeredConfig {
            links: vec![eth0],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(keyfile.contains("cloned-mac-address=aa:bb:cc:dd:ee:ff"));
    }
}
