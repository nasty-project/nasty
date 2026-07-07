//! NetworkManager backend — typed connection-profile model.
//!
//! See `docs/network-architecture.md`.  This module turns a
//! `LayeredConfig` (from `super::layered`) into a set of NM connection
//! profiles.  The actual apply goes through `nm::dbus::apply_profiles`
//! over DBus; `serialize_keyfile` here renders the same data as
//! `.nmconnection.preview` files in a NASty-owned directory for
//! inspection / debugging.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zbus::zvariant::OwnedValue;

use super::IpMethod;
use super::layered::{Address, AddressMethod, Family, LayeredConfig, Link, LinkKind};

pub mod dbus;

// ── Types ──────────────────────────────────────────────────────

/// One NetworkManager connection profile.
///
/// Maps roughly 1:1 to a `.nmconnection` keyfile in
/// `/etc/NetworkManager/system-connections/`.  NM gets these via
/// DBus; the file form is rendered alongside for inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NmConnection {
    /// Connection ID, e.g. `nasty-br0`.  Used by `nmcli connection show`
    /// and as the basename of the keyfile.  The `nasty-` prefix is the
    /// ownership marker that discriminates NASty-owned connections from
    /// external ones (Docker, libvirt, user-created profiles).
    pub id: String,
    /// Deterministic UUID derived from `id` via UUIDv5 of the connection
    /// ID — same name → same UUID across runs, which lets `apply_profiles`
    /// match desired-vs-existing without keeping a side database.
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
    /// SR-IOV VF count to create on this PF at activation
    /// (`sriov.total-vfs`). NM owns VF lifecycle: it writes the
    /// device's `sriov_numvfs` when the profile activates, which also
    /// recreates VFs automatically at boot with no engine involvement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sriov_num_vfs: Option<u32>,
    /// SR-IOV per-VF properties, emitted as `sriov.vfs` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vfs: Vec<super::VfConfig>,
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
    /// `infiniband` — an IPoIB port. Same L3 handling as ethernet but
    /// a different link layer entirely (20-byte hardware address, no
    /// Ethernet framing): NM refuses an `802-3-ethernet` profile on an
    /// IB device, and IB ports can never be bond/bridge members here.
    Infiniband,
    Bond,
    Bridge,
    Vlan,
    Macvlan,
}

impl NmConnectionType {
    /// String NM expects in the keyfile `type=` field.
    fn keyfile_str(self) -> &'static str {
        match self {
            NmConnectionType::Ethernet => "802-3-ethernet",
            NmConnectionType::Infiniband => "infiniband",
            NmConnectionType::Bond => "bond",
            NmConnectionType::Bridge => "bridge",
            NmConnectionType::Vlan => "vlan",
            NmConnectionType::Macvlan => "macvlan",
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
    /// DNS servers for this address family. NM merges per-connection
    /// DNS into systemd-resolved at activation time, so we emit the
    /// user's globally-configured `legacy.dns` on every connection
    /// rather than guessing which one is "the gateway-bearing one".
    /// IPv4 strings go in [ipv4].dns; IPv6 strings in [ipv6].dns; the
    /// per-family split is decided in `to_nm_profiles` based on the
    /// presence of a colon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    /// On-link static routes (bare destination CIDRs) for this family.
    /// Emitted as `routeN=<dst>` (keyfile) / `route-data` (DBus) with no
    /// next-hop, i.e. scope-link via this connection's device — used by
    /// the macvlan host shim to reach container subnets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
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
    Macvlan {
        parent: String,
        /// NM macvlan mode; we only emit "bridge" (host↔child shim).
        mode: String,
    },
}

// ── Converter ──────────────────────────────────────────────────

/// Per-apply context that's not in the layered model itself. Currently
/// just for MAC inheritance — without it, NM creates bonds/bridges
/// with a random MAC, which makes DHCP servers see "a new client" and
/// hand out a different lease the moment a master comes up. If the
/// user is enslaving their management interface, that yanks the
/// session and lands them on a new IP. Inheriting the primary
/// member's MAC keeps DHCP recognising the same client.
///
/// `mgmt_iface` is the management interface name resolved from the
/// caller's socket (same one the risk classifier uses). When it's
/// among a master's members, prefer its MAC over the first member's
/// MAC — that's the one the user is currently reachable on, and
/// keeping it stable preserves their session across the enslave step.
#[derive(Debug, Default)]
pub struct MacContext {
    /// Live `name → MAC` map from `enumerate_interfaces()`. Empty
    /// for tests / migration / nm_preview where live state isn't
    /// readily available — callers fall back to NM's default
    /// (random MAC) which is fine when the bridge/bond doesn't
    /// touch the management path.
    pub live_macs: HashMap<String, String>,
    pub mgmt_iface: Option<String>,
    /// Live interfaces whose kind is `infiniband` (ARPHRD 32). A
    /// `Physical` link in this set renders as an NM `infiniband`
    /// connection instead of `802-3-ethernet` — NM rejects an
    /// ethernet profile bound to an IPoIB device. The config model
    /// deliberately doesn't persist link layers (live facts drift),
    /// so this rides in the render context like the MACs do. All
    /// production call sites must populate it consistently or the
    /// desired set flip-flops between profile types across
    /// apply/preview/reconcile.
    pub infiniband_ifaces: std::collections::HashSet<String>,
}

/// Convenience wrapper that calls into [`to_nm_profiles_with_macs`]
/// with an empty context. Used by tests and by migration code paths
/// where we don't have live MAC info to plumb through.
pub fn to_nm_profiles(layered: &LayeredConfig) -> Vec<NmConnection> {
    to_nm_profiles_with_macs(layered, &MacContext::default())
}

/// Project a `LayeredConfig` to NM connection profiles. One `NmConnection`
/// per `Link`, with members getting `controller = <master>` + IP method
/// `disabled` (the master owns the L3).
///
/// Bond/bridge masters get their `mac` populated from the primary
/// member's live MAC (preferring the management iface when it's a
/// member). The serializer then emits this as `bridge.mac-address`
/// for bridges and `802-3-ethernet.cloned-mac-address` for bonds.
pub fn to_nm_profiles_with_macs(layered: &LayeredConfig, ctx: &MacContext) -> Vec<NmConnection> {
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

    // On-link static routes grouped by device, split into v4/v6 by the
    // destination's form, so each goes in the right [ipv4]/[ipv6] section.
    let mut routes_by_dev: HashMap<&str, (Vec<String>, Vec<String>)> = HashMap::new();
    for r in &layered.routes {
        let entry = routes_by_dev.entry(r.dev.as_str()).or_default();
        if r.dst.contains(':') {
            entry.1.push(r.dst.clone());
        } else {
            entry.0.push(r.dst.clone());
        }
    }

    // Split user-configured DNS into v4/v6 buckets so we put each
    // entry in the right [ipv4]/[ipv6] section. NM accepts only
    // family-matching addresses in each section.
    let (dns_v4, dns_v6) = split_dns_by_family(&layered.dns);

    layered
        .links
        .iter()
        .map(|link| {
            let mut conn = profile_for_link(link, &addrs_by_link, &master_of, ctx);
            // DNS only meaningful on master/standalone connections —
            // member ports have ipv4/ipv6 method=disabled and NM
            // ignores DNS on disabled-method sections anyway, but
            // skip them explicitly to keep the wire dict tidy.
            if conn.controller.is_none() {
                conn.ipv4.dns.clone_from(&dns_v4);
                conn.ipv6.dns.clone_from(&dns_v6);
            }
            // Attach any on-link routes targeted at this device (macvlan shim).
            if let Some((v4, v6)) = routes_by_dev.get(link.name.as_str()) {
                conn.ipv4.routes.clone_from(v4);
                conn.ipv6.routes.clone_from(v6);
            }
            // Bond/bridge masters: inherit the primary member's MAC
            // *if the user opted in* (default for new bridges/bonds;
            // can be turned off via the WebUI's "Don't inherit member
            // MAC" checkbox). Skipping inheritance lets NM/the kernel
            // assign a random MAC, which makes DHCP servers see a new
            // client identity — that's the right call for some users
            // who want a separate identity for the master, and the
            // wrong one for users enslaving their mgmt iface, hence
            // the toggle. `pick_primary_member` prefers the mgmt iface
            // when it's a member so the user's session survives the
            // enslave step.
            if inherits_member_mac(&link.kind)
                && let Some(member) = pick_primary_member(&link.kind, ctx)
                && let Some(mac) = ctx.live_macs.get(member)
            {
                conn.mac = Some(mac.clone());
            }
            conn
        })
        .collect()
}

fn inherits_member_mac(kind: &LinkKind) -> bool {
    match kind {
        LinkKind::Bond {
            inherit_member_mac, ..
        }
        | LinkKind::Bridge {
            inherit_member_mac, ..
        } => *inherit_member_mac,
        _ => false,
    }
}

/// Pick which member's MAC the master should adopt. When the user's
/// management interface is in the member list, return that — DHCP
/// will keep handing out the same lease, so their session survives.
/// Otherwise fall back to the first declared member; that's the most
/// common bridge/bond case (one or two members, list order matches
/// user intent). Returns `None` when the link isn't a master.
fn pick_primary_member<'a>(kind: &'a LinkKind, ctx: &'a MacContext) -> Option<&'a str> {
    let members = match kind {
        LinkKind::Bond { members, .. } | LinkKind::Bridge { members, .. } => members.as_slice(),
        _ => return None,
    };
    if let Some(mgmt) = ctx.mgmt_iface.as_deref()
        && members.iter().any(|m| m == mgmt)
    {
        return Some(mgmt);
    }
    members.first().map(|s| s.as_str())
}

/// Bucket DNS server strings by family. A colon means IPv6; absence
/// means IPv4. Doesn't validate the addresses — NM will reject
/// malformed entries at activation time, with a clearer error than
/// we'd produce here.
fn split_dns_by_family(dns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for s in dns {
        if s.contains(':') {
            v6.push(s.clone());
        } else {
            v4.push(s.clone());
        }
    }
    (v4, v6)
}

fn profile_for_link(
    link: &Link,
    addrs_by_link: &HashMap<&str, (NmIpSettings, NmIpSettings)>,
    master_of: &HashMap<&str, (&str, &'static str)>,
    ctx: &MacContext,
) -> NmConnection {
    let master = master_of.get(link.name.as_str());

    // Member ports never carry IP; the master does.
    let (ipv4, ipv6) = if master.is_some() {
        (
            NmIpSettings {
                method: NmIpMethod::Disabled,
                ..Default::default()
            },
            NmIpSettings {
                method: NmIpMethod::Disabled,
                ..Default::default()
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
        LinkKind::Physical if ctx.infiniband_ifaces.contains(&link.name) => {
            (NmConnectionType::Infiniband, NmTypeSpecific::None)
        }
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
        LinkKind::Macvlan { parent, mode } => (
            NmConnectionType::Macvlan,
            NmTypeSpecific::Macvlan {
                parent: parent.clone(),
                mode: mode.clone(),
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
        sriov_num_vfs: link.sriov_num_vfs,
        vfs: link.vfs.clone(),
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
        dns: Vec::new(),
        routes: Vec::new(),
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

/// Stable namespace UUID for NASty-managed NM connections. Generated
/// once via `Uuid::new_v4`, then frozen. Same name → same connection
/// UUID across reboots and across boxes, which keeps NM's connection
/// identity stable when we re-author profiles.
const NASTY_UUID_NAMESPACE: uuid::Uuid = uuid::uuid!("8d1f3a4e-4c8b-5e9f-9a1b-7c2e3d4f5a6b");

/// Deterministic UUIDv5 from the link name.  Same name → same UUID,
/// always.  Used as the connection identity when calling
/// `Settings.AddConnection` over DBus.
fn deterministic_uuid_for(name: &str) -> String {
    uuid::Uuid::new_v5(&NASTY_UUID_NAMESPACE, name.as_bytes())
        .hyphenated()
        .to_string()
}

// ── Keyfile serializer ─────────────────────────────────────────

/// Render an `NmConnection` to NM `.nmconnection` keyfile text. The
/// output is what NM would consume from
/// `/etc/NetworkManager/system-connections/<id>.nmconnection`.
/// VF attribute string in NM's `nm_utils_sriov_vf_to_str` shape with
/// the index omitted: space-separated `attr=val` pairs in attribute
/// name order, `vlans=ID[.QOS[.PROTO]]` appended last. This is exactly
/// what NM's keyfile writer puts after `vf.<index>=`.
fn vf_attr_string(vf: &super::VfConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(mac) = &vf.mac {
        parts.push(format!("mac={mac}"));
    }
    if let Some(sc) = vf.spoof_check {
        parts.push(format!("spoof-check={sc}"));
    }
    if let Some(t) = vf.trust {
        parts.push(format!("trust={t}"));
    }
    if let Some(vlan) = vf.vlan {
        parts.push(format!("vlans={vlan}"));
    }
    parts.join(" ")
}

pub fn serialize_keyfile(p: &NmConnection) -> String {
    let mut out = String::new();

    out.push_str("# Generated by NASty for inspection. NM gets the\n");
    out.push_str("# active profile via DBus, not from this file.\n\n");

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
            // Disable bridge multicast snooping (#291). The Linux bridge
            // defaults to multicast_snooping=1 + multicast_querier=0; with
            // no querier on a typical home LAN, the bridge stops forwarding
            // multicast ~5 min after the last IGMP membership times out.
            // That silently kills mDNS (avahi/Finder) and WSD (Windows
            // Explorer) discovery once a NASty link is enslaved to a bridge:
            // the SMB data path keeps working but the box vanishes from file
            // managers. Snooping buys nothing without a querier, so turn it
            // off on every bridge we manage.
            out.push_str("multicast-snooping=false\n");
            // Bridge MTU is NOT a [bridge] property (NM rejects `bridge.mtu`
            // as unknown — #580); it's emitted in [ethernet] below like
            // every other link type.
            // Bridge MAC DOES live here (`bridge.mac-address`), not
            // in [802-3-ethernet]. Without it NM creates the bridge
            // with a kernel-random MAC and DHCP hands out a new lease.
            if let Some(mac) = &p.mac {
                out.push_str(&format!("mac-address={mac}\n"));
            }
            out.push('\n');
        }
        NmTypeSpecific::Vlan { parent, id } => {
            out.push_str("[vlan]\n");
            out.push_str(&format!("parent={parent}\n"));
            out.push_str(&format!("id={id}\n\n"));
        }
        NmTypeSpecific::Macvlan { parent, mode } => {
            out.push_str("[macvlan]\n");
            out.push_str(&format!("parent={parent}\n"));
            // NM stores macvlan mode as the numeric NMSettingMacvlanMode.
            out.push_str(&format!("mode={}\n\n", macvlan_mode_num(mode)));
        }
    }

    // [infiniband] is the IB counterpart of [ethernet]: it owns the
    // transport mode and the MTU (`infiniband.mtu`, not `ethernet.mtu`).
    // Only datagram mode is emitted — connected mode is deprecated and
    // unsupported by the in-tree mlx5_ib driver (ConnectX-4+); NM keeps
    // it solely for legacy mlx4 hardware (#602).
    if matches!(p.conn_type, NmConnectionType::Infiniband) {
        out.push_str("[infiniband]\n");
        out.push_str("transport-mode=datagram\n");
        if let Some(mtu) = p.mtu {
            out.push_str(&format!("mtu={mtu}\n"));
        }
        out.push('\n');
    }

    // [ethernet] carries MTU for ethernet-like types AND bridges, and
    // cloned-mac for the non-bridge ones. A bridge master's MTU has no
    // `bridge.mtu` (NM rejects it — #580), so it lives here. A bridge's
    // MAC is set via [bridge] mac-address, so no cloned-mac here for it.
    // Never emitted for infiniband — IB has no 802-3 section and its
    // MTU lives in [infiniband] above.
    let is_bridge = matches!(p.conn_type, NmConnectionType::Bridge);
    let eth_cloned_mac = if is_bridge { None } else { p.mac.as_ref() };
    let needs_eth_section = matches!(
        p.conn_type,
        NmConnectionType::Ethernet
            | NmConnectionType::Bond
            | NmConnectionType::Vlan
            | NmConnectionType::Bridge
    );
    if needs_eth_section
        && (p.mtu.is_some()
            || eth_cloned_mac.is_some()
            || matches!(p.conn_type, NmConnectionType::Ethernet))
    {
        out.push_str("[ethernet]\n");
        if let Some(mtu) = p.mtu {
            out.push_str(&format!("mtu={mtu}\n"));
        }
        if let Some(mac) = eth_cloned_mac {
            out.push_str(&format!("cloned-mac-address={mac}\n"));
        }
        out.push('\n');
    }

    // [sriov] — VF count on an SR-IOV PF. NM creates/destroys the VFs
    // at activation time; autoprobe is left at the global default so
    // VF host drivers bind normally (passthrough VFs get claimed
    // per-device via passthrough.nix udev rules instead).
    if let Some(n) = p.sriov_num_vfs {
        out.push_str("[sriov]\n");
        out.push_str(&format!("total-vfs={n}\n"));
        // One `vf.<index>` key per configured VF — the shape NM's own
        // keyfile writer produces (nm-keyfile.c, sriov_vfs_writer).
        for vf in &p.vfs {
            out.push_str(&format!("vf.{}={}\n", vf.index, vf_attr_string(vf)));
        }
        out.push('\n');
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
    // NM keyfile DNS syntax: `dns=8.8.8.8;1.1.1.1;` (semicolon-
    // separated, with a trailing semicolon). Only emit when set —
    // an empty `dns=` line is harmless but adds noise.
    if !ip.dns.is_empty() {
        out.push_str("dns=");
        for d in &ip.dns {
            out.push_str(d);
            out.push(';');
        }
        out.push('\n');
    }
    // On-link static routes: `routeN=<dst>` with no next-hop ⇒ scope-link
    // route via this connection's device (the macvlan shim).
    for (i, dst) in ip.routes.iter().enumerate() {
        out.push_str(&format!("route{}={dst}\n", i + 1));
    }
}

/// NM `NMSettingMacvlanMode` numeric value. We only create "bridge"
/// shims, but map the rest for completeness/forward-compat.
fn macvlan_mode_num(mode: &str) -> u32 {
    match mode {
        "vepa" => 1,
        "bridge" => 2,
        "private" => 3,
        "passthru" => 4,
        "source" => 5,
        _ => 2,
    }
}

// ── DBus settings dict converter ───────────────────────────────
//
// Convert a typed `NmConnection` into the dict shape NM expects on
// DBus (`a{sa{sv}}` — section name → key → variant).  This is the
// `Settings.AddConnection` / `Connection.Update` payload.
//
// Why this lives alongside `serialize_keyfile`: the keyfile and the
// DBus dict are two views of the same connection. Some fields are
// formatted differently (`address1=10.0.0.5/24,10.0.0.1` in keyfile
// becomes a `Vec<Vec<u32>>` of `[ip, prefix, gateway]` triples on
// DBus), but every field is in both. Keeping them next to each other
// makes drift easy to spot.

/// Build the DBus settings dict for an `NmConnection`. Compatible with
/// `org.freedesktop.NetworkManager.Settings.AddConnection`.
pub fn to_settings_dict(p: &NmConnection) -> HashMap<String, HashMap<String, OwnedValue>> {
    let mut out: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();

    // [connection]
    let mut conn = HashMap::new();
    conn.insert("id".into(), into_value(p.id.clone()));
    conn.insert("uuid".into(), into_value(p.uuid.clone()));
    conn.insert(
        "type".into(),
        into_value(p.conn_type.keyfile_str().to_string()),
    );
    conn.insert(
        "interface-name".into(),
        into_value(p.interface_name.clone()),
    );
    conn.insert("autoconnect".into(), into_value(p.autoconnect));
    if let Some(c) = &p.controller {
        conn.insert("controller".into(), into_value(c.clone()));
    }
    if let Some(pt) = &p.port_type {
        conn.insert("port-type".into(), into_value(pt.clone()));
    }
    out.insert("connection".into(), conn);

    // type-specific section
    match &p.type_specific {
        NmTypeSpecific::None => {}
        NmTypeSpecific::Bond { mode } => {
            // NM's [bond] is `options`: a string→string map.
            let mut options = HashMap::<String, String>::new();
            options.insert("mode".into(), mode.clone());
            let mut bond = HashMap::new();
            bond.insert("options".into(), into_value(options));
            out.insert("bond".into(), bond);
        }
        NmTypeSpecific::Bridge { stp, forward_delay } => {
            let mut bridge = HashMap::new();
            bridge.insert("stp".into(), into_value(*stp));
            if let Some(fd) = forward_delay {
                // NM expects forward-delay as a u32 in seconds.
                bridge.insert("forward-delay".into(), into_value(u32::from(*fd)));
            }
            // Disable multicast snooping (#291) — see the keyfile
            // serializer for the full rationale. Without a querier on the
            // LAN the bridge stops forwarding multicast after ~5 min,
            // which kills mDNS/WSD discovery while leaving SMB working.
            bridge.insert("multicast-snooping".into(), into_value(false));
            // NOTE: a bridge master's MTU is NOT a `bridge` property — NM
            // rejects `bridge.mtu` as an unknown property (verified on NM
            // 1.56; the `bridge` setting has no `mtu`). It's emitted under
            // [802-3-ethernet] below, like every other link type. This
            // reverts the #96 change that moved it into `bridge.mtu` and
            // broke bridge creation on any box with a non-default bridge
            // MTU — e.g. a bridge over a 9000-MTU bond (#580), where the
            // failed br0 then cascades to "can't find controller br0" for
            // the bond.
            // Bridge MAC: NM's `bridge.mac-address`
            // pins the bridge interface's MAC at activation time;
            // without it the kernel generates a random MAC and
            // DHCP hands out a new lease (issue from the bridge
            // creation test on 10.10.10.61).
            if let Some(mac) = &p.mac
                && let Some(bytes) = mac_to_bytes(mac)
            {
                bridge.insert("mac-address".into(), into_value(bytes));
            }
            out.insert("bridge".into(), bridge);
        }
        NmTypeSpecific::Vlan { parent, id } => {
            let mut vlan = HashMap::new();
            vlan.insert("parent".into(), into_value(parent.clone()));
            vlan.insert("id".into(), into_value(u32::from(*id)));
            out.insert("vlan".into(), vlan);
        }
        NmTypeSpecific::Macvlan { parent, mode } => {
            let mut mv = HashMap::new();
            mv.insert("parent".into(), into_value(parent.clone()));
            mv.insert("mode".into(), into_value(macvlan_mode_num(mode)));
            out.insert("macvlan".into(), mv);
        }
    }

    // [infiniband] — IB counterpart of [802-3-ethernet]; owns transport
    // mode and MTU. Datagram only: connected mode is deprecated and
    // unsupported by the in-tree mlx5_ib driver (#602).
    if matches!(p.conn_type, NmConnectionType::Infiniband) {
        let mut ib = HashMap::new();
        ib.insert("transport-mode".into(), into_value("datagram".to_string()));
        if let Some(mtu) = p.mtu {
            ib.insert("mtu".into(), into_value(u32::from(mtu)));
        }
        out.insert("infiniband".into(), ib);
    }

    // [802-3-ethernet] carries MTU for ethernet-like types AND bridges,
    // and cloned-mac for the non-bridge ones. NM accepts this section on
    // Ethernet, Bond, VLAN, and Bridge connections — and a bridge master's
    // MTU has nowhere else to go (no `bridge.mtu`), so it lives here (#580).
    // A bridge's MAC is set via `bridge.mac-address` (above), so we don't
    // emit cloned-mac here for bridges. Infiniband is excluded — its MTU
    // lives in the `infiniband` section above.
    let needs_eth_section = matches!(
        p.conn_type,
        NmConnectionType::Ethernet
            | NmConnectionType::Bond
            | NmConnectionType::Vlan
            | NmConnectionType::Bridge
    );
    if needs_eth_section {
        let mut eth = HashMap::new();
        if let Some(mtu) = p.mtu {
            eth.insert("mtu".into(), into_value(u32::from(mtu)));
        }
        if !matches!(p.conn_type, NmConnectionType::Bridge)
            && let Some(mac) = &p.mac
            && let Some(bytes) = mac_to_bytes(mac)
        {
            eth.insert("cloned-mac-address".into(), into_value(bytes));
        }
        // Always emit the section for ethernet so a profile with
        // neither MTU nor cloned-mac still has the type's primary
        // setting block. Bond/VLAN/Bridge: only emit when populated.
        if matches!(p.conn_type, NmConnectionType::Ethernet) || !eth.is_empty() {
            out.insert("802-3-ethernet".into(), eth);
        }
    }

    // [sriov] — see the keyfile serializer note.
    if let Some(n) = p.sriov_num_vfs {
        let mut sriov = HashMap::new();
        sriov.insert("total-vfs".into(), into_value(n));
        // `vfs` is aa{sv} with `vlans` nested as aa{sv} of
        // {id, qos, protocol} — NM's vfs_to_dbus shape
        // (nm-setting-sriov.c). protocol 0 = 802.1Q.
        if !p.vfs.is_empty() {
            let mut vfs: Vec<HashMap<String, OwnedValue>> = Vec::new();
            for vf in &p.vfs {
                let mut e = HashMap::new();
                e.insert("index".into(), into_value(vf.index));
                if let Some(mac) = &vf.mac {
                    e.insert("mac".into(), into_value(mac.clone()));
                }
                if let Some(sc) = vf.spoof_check {
                    e.insert("spoof-check".into(), into_value(sc));
                }
                if let Some(t) = vf.trust {
                    e.insert("trust".into(), into_value(t));
                }
                if let Some(vlan) = vf.vlan {
                    let mut v: HashMap<String, OwnedValue> = HashMap::new();
                    v.insert("id".into(), into_value(u32::from(vlan)));
                    v.insert("qos".into(), into_value(0u32));
                    v.insert("protocol".into(), into_value(0u32));
                    e.insert("vlans".into(), into_value(vec![v]));
                }
                vfs.push(e);
            }
            sriov.insert("vfs".into(), into_value(vfs));
        }
        out.insert("sriov".into(), sriov);
    }

    // [ipv4]
    out.insert("ipv4".into(), ip_section_dict(&p.ipv4, Family::V4));
    // [ipv6]
    out.insert("ipv6".into(), ip_section_dict(&p.ipv6, Family::V6));

    out
}

fn ip_section_dict(ip: &NmIpSettings, family: Family) -> HashMap<String, OwnedValue> {
    let mut s = HashMap::new();
    s.insert(
        "method".into(),
        into_value(ip.method.keyfile_str().to_string()),
    );

    // NM's `address-data` (since 1.0) is the modern shape:
    //   aa{sv} — array of dicts with keys "address" (string CIDR base)
    //   and "prefix" (u32). Optional gateway is on `[ipv4].gateway`.
    if !ip.addresses.is_empty() {
        let mut addr_data: Vec<HashMap<String, OwnedValue>> = Vec::new();
        for cidr in &ip.addresses {
            if let Some((addr, prefix)) = parse_cidr(cidr, family) {
                let mut entry = HashMap::new();
                entry.insert("address".into(), into_value(addr));
                entry.insert("prefix".into(), into_value(prefix));
                addr_data.push(entry);
            }
        }
        if !addr_data.is_empty() {
            s.insert("address-data".into(), into_value(addr_data));
        }
    }
    if let Some(gw) = &ip.gateway {
        s.insert("gateway".into(), into_value(gw.clone()));
    }
    // NM's DBus types for DNS are family-specific and not strings:
    //   ipv4.dns  → `au`   array of u32 in network byte order
    //   ipv6.dns  → `aay`  array of 16-byte arrays
    // Sending the string-array shape (as) gets rejected as
    //   "InvalidProperty: ipv4.dns: can't set property of type 'au'
    //    from value of type 'as'"
    // which then prevents the bond from being created, which
    // cascades into DependencyFailed errors on its members — the
    // shape reported in discussion #159.  Pack to the correct
    // family-specific type here.  We silently drop entries that
    // don't parse for the target family rather than fail the whole
    // apply; `split_dns_by_family` upstream is supposed to route
    // them correctly already, but a malformed string from a hand-
    // edited config shouldn't take the whole network apply down.
    //
    // Empty arrays aren't emitted at all — overwriting the DHCP-
    // supplied DNS on auto-method connections with an empty list
    // would yank name resolution.
    if !ip.dns.is_empty() {
        match family {
            Family::V4 => {
                let packed: Vec<u32> = ip
                    .dns
                    .iter()
                    .filter_map(|s| s.parse::<std::net::Ipv4Addr>().ok())
                    .map(|a| u32::from_be_bytes(a.octets()))
                    .collect();
                if !packed.is_empty() {
                    s.insert("dns".into(), into_value(packed));
                }
            }
            Family::V6 => {
                let packed: Vec<Vec<u8>> = ip
                    .dns
                    .iter()
                    .filter_map(|s| s.parse::<std::net::Ipv6Addr>().ok())
                    .map(|a| a.octets().to_vec())
                    .collect();
                if !packed.is_empty() {
                    s.insert("dns".into(), into_value(packed));
                }
            }
        }
    }
    // route-data (aa{sv}): on-link routes — dest + prefix, no next-hop.
    if !ip.routes.is_empty() {
        let mut rd: Vec<HashMap<String, OwnedValue>> = Vec::new();
        for cidr in &ip.routes {
            if let Some((dest, prefix)) = parse_cidr(cidr, family) {
                let mut e = HashMap::new();
                e.insert("dest".into(), into_value(dest));
                e.insert("prefix".into(), into_value(prefix));
                rd.push(e);
            }
        }
        if !rd.is_empty() {
            s.insert("route-data".into(), into_value(rd));
        }
    }
    s
}

/// Parse `"10.0.0.5/24"` → (`"10.0.0.5"`, 24). Returns None on
/// malformed input (we'd rather drop a bad address row than fail the
/// whole apply).
fn parse_cidr(cidr: &str, _family: Family) -> Option<(String, u32)> {
    let (addr, prefix_str) = cidr.split_once('/')?;
    let prefix: u32 = prefix_str.parse().ok()?;
    Some((addr.to_string(), prefix))
}

/// Parse a colon-separated MAC address like "aa:bb:cc:dd:ee:ff" into the
/// 6-byte form NetworkManager expects for `ay`-typed MAC fields
/// (`bridge.mac-address`, `802-3-ethernet.cloned-mac-address`).
///
/// Returns `None` for malformed input; callers skip the field so NM uses
/// its default rather than rejecting the whole connection.
fn mac_to_bytes(mac: &str) -> Option<Vec<u8>> {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = Vec::with_capacity(6);
    for p in parts {
        if p.len() != 2 {
            return None;
        }
        out.push(u8::from_str_radix(p, 16).ok()?);
    }
    Some(out)
}

/// Wrap a value in `OwnedValue`. Centralized so the unwrap handling is
/// in one place — these conversions only fail on out-of-memory, which
/// would fail loudly elsewhere first.
fn into_value<T>(v: T) -> OwnedValue
where
    T: Into<zbus::zvariant::Value<'static>>,
{
    let value: zbus::zvariant::Value<'static> = v.into();
    OwnedValue::try_from(value).expect("OwnedValue conversion")
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
            sriov_num_vfs: None,
            vfs: Vec::new(),
            kind: LinkKind::Physical,
        }
    }

    fn link_bridge(name: &str, members: &[&str]) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
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
            sriov_num_vfs: None,
            vfs: Vec::new(),
            kind: LinkKind::Bond {
                members: members.iter().map(|s| (*s).to_string()).collect(),
                mode: BondMode::Lacp,
                inherit_member_mac: false,
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
    fn infiniband_port_renders_ipoib_profile() {
        // A Physical link whose live kind is infiniband must become an
        // NM `infiniband` connection: NM refuses 802-3-ethernet on an
        // IPoIB device. MTU lives in [infiniband], not [ethernet], and
        // only datagram mode is emitted (connected is dead in mlx5_ib).
        let mut link = link_phys("ib0");
        link.mtu = Some(2044);
        let layered = LayeredConfig {
            links: vec![link, link_phys("eth0")],
            addresses: vec![Address {
                link: "ib0".into(),
                family: Family::V4,
                method: IpMethod::Static,
                cidr: vec!["10.99.0.1/24".into()],
                gateway: None,
            }],
            ..Default::default()
        };
        let ctx = MacContext {
            infiniband_ifaces: ["ib0".to_string()].into(),
            ..Default::default()
        };
        let profiles = to_nm_profiles_with_macs(&layered, &ctx);

        let ib = find(&profiles, "ib0");
        assert_eq!(ib.conn_type, NmConnectionType::Infiniband);
        let keyfile = serialize_keyfile(ib);
        assert!(keyfile.contains("type=infiniband"), "{keyfile}");
        assert!(keyfile.contains("[infiniband]"), "{keyfile}");
        assert!(keyfile.contains("transport-mode=datagram"), "{keyfile}");
        assert!(!keyfile.contains("[ethernet]"), "{keyfile}");
        // MTU under [infiniband].
        assert!(keyfile.contains("mtu=2044"), "{keyfile}");

        let dict = to_settings_dict(ib);
        assert!(dict.contains_key("infiniband"));
        assert!(!dict.contains_key("802-3-ethernet"));

        // The sibling ethernet port is unaffected by the IB context.
        let eth = find(&profiles, "eth0");
        assert_eq!(eth.conn_type, NmConnectionType::Ethernet);
    }

    #[test]
    fn sriov_pf_emits_total_vfs_in_both_serializers() {
        let mut link = link_phys("enp6s0f0");
        link.sriov_num_vfs = Some(8);
        let layered = LayeredConfig {
            links: vec![link, link_phys("eth0")],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);

        let pf = find(&profiles, "enp6s0f0");
        let keyfile = serialize_keyfile(pf);
        assert!(keyfile.contains("[sriov]"), "{keyfile}");
        assert!(keyfile.contains("total-vfs=8"), "{keyfile}");
        let dict = to_settings_dict(pf);
        assert!(dict.contains_key("sriov"));

        // Plain NIC: no [sriov] anywhere.
        let eth = find(&profiles, "eth0");
        assert!(!serialize_keyfile(eth).contains("[sriov]"));
        assert!(!to_settings_dict(eth).contains_key("sriov"));

        // Count-only PF: no per-VF keys.
        assert!(!keyfile.contains("vf."), "{keyfile}");
    }

    fn pf_with_vf_properties() -> LayeredConfig {
        let mut link = link_phys("enp6s0f0");
        link.sriov_num_vfs = Some(8);
        link.vfs = vec![
            crate::network::VfConfig {
                index: 0,
                vlan: Some(100),
                mac: Some("00:11:22:33:44:55".into()),
                trust: Some(true),
                spoof_check: Some(false),
            },
            crate::network::VfConfig {
                index: 3,
                vlan: Some(200),
                mac: None,
                trust: None,
                spoof_check: None,
            },
        ];
        LayeredConfig {
            links: vec![link],
            ..Default::default()
        }
    }

    #[test]
    fn sriov_vf_properties_emit_as_keyfile_vf_keys() {
        // NM's keyfile writer (nm-keyfile.c, sriov_vfs_writer) stores
        // one `vf.<index>` key per VF whose value is the attribute
        // string without the index: space-separated `attr=val` pairs
        // in name order, `vlans=ID[.QOS[.PROTO]]` last. Pin the exact
        // shape so NM parses what we write.
        let profiles = to_nm_profiles(&pf_with_vf_properties());
        let keyfile = serialize_keyfile(find(&profiles, "enp6s0f0"));
        assert!(
            keyfile.contains("vf.0=mac=00:11:22:33:44:55 spoof-check=false trust=true vlans=100"),
            "{keyfile}"
        );
        assert!(keyfile.contains("vf.3=vlans=200"), "{keyfile}");
    }

    #[test]
    fn sriov_vf_properties_emit_as_dbus_vardicts() {
        // NM's D-Bus shape (nm-setting-sriov.c, vfs_to_dbus): `vfs` is
        // aa{sv}, each VF vardict carrying `index` (u) plus attributes,
        // with `vlans` nested as aa{sv} of {id, qos, protocol}.
        let profiles = to_nm_profiles(&pf_with_vf_properties());
        let dict = to_settings_dict(find(&profiles, "enp6s0f0"));
        let sriov = dict.get("sriov").expect("sriov section");

        let outer = sriov.get("vfs").expect("vfs key").try_clone().unwrap();
        let arr: zbus::zvariant::Array = outer.try_into().expect("vfs is an array");
        let first: zbus::zvariant::Value = arr.iter().next().unwrap().try_clone().unwrap();
        let entry: zbus::zvariant::Dict = first.try_into().expect("vf entry is a dict");
        let vf: HashMap<String, OwnedValue> = entry.try_into().unwrap();

        let index: u32 = vf["index"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(index, 0);
        let mac: String = vf["mac"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(mac, "00:11:22:33:44:55");
        let trust: bool = vf["trust"].try_clone().unwrap().try_into().unwrap();
        assert!(trust);
        let spoof: bool = vf["spoof-check"].try_clone().unwrap().try_into().unwrap();
        assert!(!spoof);

        let vlans = vf["vlans"].try_clone().unwrap();
        let vlans_arr: zbus::zvariant::Array = vlans.try_into().expect("vlans is an array");
        let vlan_entry: zbus::zvariant::Value =
            vlans_arr.iter().next().unwrap().try_clone().unwrap();
        let vlan_dict: zbus::zvariant::Dict = vlan_entry.try_into().unwrap();
        let vlan: HashMap<String, OwnedValue> = vlan_dict.try_into().unwrap();
        let id: u32 = vlan["id"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(id, 100);
        let qos: u32 = vlan["qos"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(qos, 0);
        let protocol: u32 = vlan["protocol"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(protocol, 0, "0 = 802.1Q");
    }

    #[test]
    fn physical_link_without_ib_context_stays_ethernet() {
        // Same link name, empty context → ethernet. Guards against the
        // context being accidentally required for ordinary NICs.
        let layered = LayeredConfig {
            links: vec![link_phys("ib0")],
            ..Default::default()
        };
        let p = to_nm_profiles(&layered);
        assert_eq!(p[0].conn_type, NmConnectionType::Ethernet);
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
    fn macvlan_shim_serializes_with_parent_mode_and_route() {
        let layered = LayeredConfig {
            links: vec![
                link_phys("eth1"),
                Link {
                    name: "shim-lan".into(),
                    enabled: true,
                    mtu: None,
                    mac: None,
                    sriov_num_vfs: None,
                    vfs: Vec::new(),
                    kind: LinkKind::Macvlan {
                        parent: "eth1".into(),
                        mode: "bridge".into(),
                    },
                },
            ],
            addresses: vec![Address {
                link: "shim-lan".into(),
                family: Family::V4,
                method: IpMethod::Static,
                cidr: vec!["192.168.1.2/24".into()],
                gateway: None,
            }],
            routes: vec![crate::network::layered::Route {
                table: 254,
                dst: "192.168.1.0/24".into(),
                via: None,
                dev: "shim-lan".into(),
                metric: None,
            }],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        let shim = find(&profiles, "shim-lan");
        assert_eq!(shim.conn_type, NmConnectionType::Macvlan);
        match &shim.type_specific {
            NmTypeSpecific::Macvlan { parent, mode } => {
                assert_eq!(parent, "eth1");
                assert_eq!(mode, "bridge");
            }
            other => panic!("expected Macvlan, got {other:?}"),
        }
        assert_eq!(shim.ipv4.routes, vec!["192.168.1.0/24".to_string()]);

        let keyfile = serialize_keyfile(shim);
        assert!(keyfile.contains("[macvlan]"), "{keyfile}");
        assert!(keyfile.contains("parent=eth1"));
        assert!(keyfile.contains("mode=2")); // NMSettingMacvlanMode bridge
        assert!(keyfile.contains("route1=192.168.1.0/24"));
        // On-link: no next-hop appended to the route.
        assert!(!keyfile.contains("route1=192.168.1.0/24,"));

        let dict = to_settings_dict(shim);
        assert!(dict.contains_key("macvlan"));
        assert!(dict["ipv4"].contains_key("route-data"));
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
                    sriov_num_vfs: None,
                    vfs: Vec::new(),
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
                sriov_num_vfs: None,
                vfs: Vec::new(),
                kind: LinkKind::Bridge {
                    members: vec![],
                    stp: true,
                    forward_delay_s: Some(0),
                    inherit_member_mac: false,
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
        // Same name → same UUID across calls so `apply_profiles` can
        // match desired-vs-existing without a side table.
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
                sriov_num_vfs: None,
                vfs: Vec::new(),
                kind: LinkKind::Bridge {
                    members: vec![],
                    stp: true,
                    forward_delay_s: Some(4),
                    inherit_member_mac: false,
                },
            }],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(keyfile.contains("[bridge]"));
        assert!(keyfile.contains("stp=true"));
        assert!(keyfile.contains("forward-delay=4"));
        // #291: snooping disabled so mDNS/WSD keep flowing without a querier.
        assert!(keyfile.contains("multicast-snooping=false"));
    }

    #[test]
    fn keyfile_for_vlan_emits_parent_and_id() {
        let layered = LayeredConfig {
            links: vec![Link {
                name: "eth0.10".into(),
                enabled: true,
                mtu: None,
                mac: None,
                sriov_num_vfs: None,
                vfs: Vec::new(),
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

    #[test]
    fn keyfile_emits_bond_mtu_under_ethernet_section() {
        // Reproducer from issue #96 follow-up: setting MTU on a bond
        // produced no `mtu=` line at all in the keyfile (just a stale
        // `# mtu=...` comment), so jumbo frames silently dropped to
        // 1500 on apply. Bond MTU lives under [ethernet] in NM.
        let mut bond0 = link_bond("bond0", &["eth0"]);
        bond0.mtu = Some(9000);
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), bond0],
            ..Default::default()
        };
        let bond_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-bond0")
            .unwrap();
        let keyfile = serialize_keyfile(&bond_profile);
        assert!(
            keyfile.contains("[ethernet]"),
            "bond should emit [ethernet] section for MTU"
        );
        assert!(
            keyfile.contains("mtu=9000"),
            "MTU must be plain `mtu=`, not `# mtu=`"
        );
    }

    #[test]
    fn keyfile_emits_bridge_mtu_under_ethernet_not_bridge() {
        // #580: a bridge master's MTU must go under [ethernet], NOT
        // [bridge] — NM has no `bridge.mtu` property and rejects it as
        // "unknown property" (verified on NM 1.56), which fails bridge
        // creation and cascades to its bond member. The [bridge] block
        // must NOT carry an mtu= line.
        let mut br0 = link_bridge("br0", &["eth0"]);
        br0.mtu = Some(9000);
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), br0],
            ..Default::default()
        };
        let br_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-br0")
            .unwrap();
        let keyfile = serialize_keyfile(&br_profile);

        // The [ethernet] block carries the MTU.
        let eth_idx = keyfile
            .find("[ethernet]")
            .expect("bridge with MTU should emit an [ethernet] section");
        let after_eth = &keyfile[eth_idx..];
        let eth_end = after_eth[1..].find('[').map_or(after_eth.len(), |i| i + 1);
        assert!(
            after_eth[..eth_end].contains("mtu=9000"),
            "[ethernet] block missing mtu: {:?}",
            &after_eth[..eth_end]
        );

        // The [bridge] block must NOT carry an mtu= line.
        let bridge_idx = keyfile.find("[bridge]").expect("bridge section missing");
        let after_bridge = &keyfile[bridge_idx..];
        let br_end = after_bridge[1..]
            .find('[')
            .map_or(after_bridge.len(), |i| i + 1);
        assert!(
            !after_bridge[..br_end].contains("mtu="),
            "[bridge] block must not contain mtu= (NM rejects bridge.mtu): {:?}",
            &after_bridge[..br_end]
        );
    }

    #[test]
    fn keyfile_emits_vlan_mtu_under_ethernet_section() {
        // VLANs over ethernet/bond parents inherit the [ethernet]
        // layer — same as bonds.
        let mut eth0_100 = Link {
            name: "eth0.100".into(),
            enabled: true,
            mtu: Some(1400),
            mac: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
            kind: LinkKind::Vlan {
                parent: "eth0".into(),
                id: 100,
            },
        };
        eth0_100.mtu = Some(1400);
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), eth0_100],
            ..Default::default()
        };
        let vlan_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-eth0.100")
            .unwrap();
        let keyfile = serialize_keyfile(&vlan_profile);
        assert!(keyfile.contains("[ethernet]"));
        assert!(keyfile.contains("mtu=1400"));
    }

    // ── DBus settings dict conversion ──────────────────────────

    fn cidr_addr_data_first(dict: &HashMap<String, OwnedValue>) -> Option<(String, u32)> {
        // Pull the first {address, prefix} entry out of `address-data`.
        let addr_data: &OwnedValue = dict.get("address-data")?;
        let outer = addr_data.try_clone().ok()?;
        // `address-data` is `aa{sv}` — array of dicts.
        let arr: zbus::zvariant::Array = outer.try_into().ok()?;
        let first: zbus::zvariant::Value = arr.iter().next()?.try_clone().ok()?;
        let entry: zbus::zvariant::Dict = first.try_into().ok()?;
        let map: HashMap<String, OwnedValue> = entry.try_into().ok()?;
        let addr: String = map.get("address")?.try_clone().ok()?.try_into().ok()?;
        let prefix: u32 = map.get("prefix")?.try_clone().ok()?.try_into().ok()?;
        Some((addr, prefix))
    }

    #[test]
    fn dict_has_required_top_level_sections_for_ethernet_dhcp() {
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
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        assert!(dict.contains_key("connection"));
        assert!(dict.contains_key("802-3-ethernet"));
        assert!(dict.contains_key("ipv4"));
        assert!(dict.contains_key("ipv6"));
    }

    #[test]
    fn dict_connection_section_carries_id_uuid_type_and_interface() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let conn = &dict["connection"];
        let id: String = conn["id"].try_clone().unwrap().try_into().unwrap();
        let uuid: String = conn["uuid"].try_clone().unwrap().try_into().unwrap();
        let conn_type: String = conn["type"].try_clone().unwrap().try_into().unwrap();
        let iface: String = conn["interface-name"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(id, "nasty-eth0");
        assert_eq!(uuid.len(), 36);
        assert_eq!(conn_type, "802-3-ethernet");
        assert_eq!(iface, "eth0");
    }

    #[test]
    fn dict_static_ipv4_uses_address_data_with_prefix() {
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
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let ipv4 = &dict["ipv4"];
        let method: String = ipv4["method"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(method, "manual");

        let (addr, prefix) = cidr_addr_data_first(ipv4).expect("address-data parse");
        assert_eq!(addr, "10.0.0.5");
        assert_eq!(prefix, 24);

        let gateway: String = ipv4["gateway"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(gateway, "10.0.0.1");
    }

    #[test]
    fn dict_bridge_member_carries_controller_and_port_type_in_connection_section() {
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bridge("br0", &["eth0"])],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        let eth0 = find(&profiles, "eth0");
        let dict = to_settings_dict(eth0);
        let conn = &dict["connection"];
        let controller: String = conn["controller"].try_clone().unwrap().try_into().unwrap();
        let port_type: String = conn["port-type"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(controller, "br0");
        assert_eq!(port_type, "bridge");
        // Member port should declare disabled IP method explicitly.
        let ipv4_method: String = dict["ipv4"]["method"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(ipv4_method, "disabled");
    }

    #[test]
    fn dict_bond_emits_options_subdict_with_mode() {
        let layered = LayeredConfig {
            links: vec![link_bond("bond0", &[])],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let bond = dict.get("bond").expect("bond section");
        // `options` is `a{ss}` — string→string dict.
        let options_value = bond["options"].try_clone().unwrap();
        let options: HashMap<String, String> = options_value.try_into().unwrap();
        assert_eq!(options.get("mode").map(|s| s.as_str()), Some("802.3ad"));
    }

    #[test]
    fn dict_bridge_emits_stp_and_forward_delay() {
        let layered = LayeredConfig {
            links: vec![Link {
                name: "br0".into(),
                enabled: true,
                mtu: None,
                mac: None,
                sriov_num_vfs: None,
                vfs: Vec::new(),
                kind: LinkKind::Bridge {
                    members: vec![],
                    stp: true,
                    forward_delay_s: Some(7),
                    inherit_member_mac: false,
                },
            }],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let bridge = dict.get("bridge").expect("bridge section");
        let stp: bool = bridge["stp"].try_clone().unwrap().try_into().unwrap();
        let fd: u32 = bridge["forward-delay"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert!(stp);
        assert_eq!(fd, 7);
        // #291: multicast snooping disabled on every managed bridge.
        let snooping: bool = bridge["multicast-snooping"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert!(!snooping);
    }

    #[test]
    fn dict_vlan_emits_parent_and_id_as_u32() {
        let layered = LayeredConfig {
            links: vec![Link {
                name: "eth0.10".into(),
                enabled: true,
                mtu: None,
                mac: None,
                sriov_num_vfs: None,
                vfs: Vec::new(),
                kind: LinkKind::Vlan {
                    parent: "eth0".into(),
                    id: 10,
                },
            }],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let vlan = dict.get("vlan").expect("vlan section");
        let parent: String = vlan["parent"].try_clone().unwrap().try_into().unwrap();
        let id: u32 = vlan["id"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(parent, "eth0");
        assert_eq!(id, 10);
    }

    #[test]
    fn dict_ethernet_section_carries_mtu_and_cloned_mac() {
        // NM's `802-3-ethernet.cloned-mac-address` is type `ay`
        // (byte array), not `s`. Sending a string gets rejected
        // with "InvalidProperty: 802-3-ethernet.cloned-mac-address:
        // can't set property of type 'ay' from value of type 's'"
        // which then aborts the whole add_connection.
        let mut eth0 = link_phys("eth0");
        eth0.mtu = Some(9000);
        eth0.mac = Some("aa:bb:cc:dd:ee:ff".into());
        let layered = LayeredConfig {
            links: vec![eth0],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let eth = dict.get("802-3-ethernet").expect("ethernet section");
        let mtu: u32 = eth["mtu"].try_clone().unwrap().try_into().unwrap();
        let mac: Vec<u8> = eth["cloned-mac-address"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(mtu, 9000);
        assert_eq!(mac, vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn dict_bridge_mac_address_emitted_as_byte_array() {
        // NM's `bridge.mac-address` is type `ay` (byte array), not `s`
        // — same wire-type story as cloned-mac-address. Pin the bytes
        // so this can't drift back to string form.
        let mut br0 = link_bridge("br0", &["eth0"]);
        br0.mac = Some("11:22:33:44:55:66".into());
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), br0],
            ..Default::default()
        };
        let br_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-br0")
            .unwrap();
        let dict = to_settings_dict(&br_profile);
        let bridge = dict.get("bridge").expect("bridge section");
        let mac: Vec<u8> = bridge["mac-address"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(mac, vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn mac_to_bytes_parses_canonical_and_rejects_garbage() {
        assert_eq!(
            mac_to_bytes("aa:bb:cc:dd:ee:ff"),
            Some(vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        // Mixed case is fine — `from_str_radix` accepts both.
        assert_eq!(
            mac_to_bytes("AA:Bb:cC:DD:ee:FF"),
            Some(vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        // Wrong separator / wrong length / non-hex → None (we skip
        // the field rather than poison the whole connection).
        assert!(mac_to_bytes("aa-bb-cc-dd-ee-ff").is_none());
        assert!(mac_to_bytes("aa:bb:cc:dd:ee").is_none());
        assert!(mac_to_bytes("aa:bb:cc:dd:ee:ff:00").is_none());
        assert!(mac_to_bytes("aa:bb:cc:dd:ee:gg").is_none());
        assert!(mac_to_bytes("a:bb:cc:dd:ee:ff").is_none());
    }

    #[test]
    fn dict_bond_includes_mtu_in_ethernet_section() {
        // Issue #96 follow-up: bond MTU was being silently dropped.
        // The DBus path that sends the connection to NM is
        // `to_settings_dict`, so we assert MTU lands in the ethernet
        // section here.
        let mut bond0 = link_bond("bond0", &["eth0"]);
        bond0.mtu = Some(9000);
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), bond0],
            ..Default::default()
        };
        let bond_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-bond0")
            .unwrap();
        let dict = to_settings_dict(&bond_profile);
        let eth = dict
            .get("802-3-ethernet")
            .expect("bond should emit 802-3-ethernet for MTU");
        let mtu: u32 = eth["mtu"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(mtu, 9000);
    }

    #[test]
    fn dict_bridge_mtu_in_ethernet_not_bridge_section() {
        // #580: bridge MTU goes in [802-3-ethernet], NOT [bridge] — NM has
        // no `bridge.mtu` and rejects it as an unknown property (NM 1.56),
        // failing the connection. Same home as bond/vlan.
        let mut br0 = link_bridge("br0", &["eth0"]);
        br0.mtu = Some(9000);
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), br0],
            ..Default::default()
        };
        let br_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-br0")
            .unwrap();
        let dict = to_settings_dict(&br_profile);
        let eth = dict
            .get("802-3-ethernet")
            .expect("bridge with MTU should emit 802-3-ethernet");
        let mtu: u32 = eth["mtu"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(mtu, 9000);
        // The bridge setting must NOT carry mtu (NM would reject it).
        assert!(
            dict.get("bridge").is_none_or(|b| !b.contains_key("mtu")),
            "bridge.mtu must not be emitted — NM rejects it as unknown"
        );
    }

    #[test]
    fn dict_vlan_includes_mtu_in_ethernet_section() {
        // VLAN MTU lives under [802-3-ethernet] like ethernet/bond.
        let eth0_100 = Link {
            name: "eth0.100".into(),
            enabled: true,
            mtu: Some(1400),
            mac: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
            kind: LinkKind::Vlan {
                parent: "eth0".into(),
                id: 100,
            },
        };
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), eth0_100],
            ..Default::default()
        };
        let vlan_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-eth0.100")
            .unwrap();
        let dict = to_settings_dict(&vlan_profile);
        let eth = dict
            .get("802-3-ethernet")
            .expect("vlan should emit 802-3-ethernet for MTU");
        let mtu: u32 = eth["mtu"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(mtu, 1400);
    }

    #[test]
    fn dict_bond_without_mtu_or_mac_omits_ethernet_section() {
        // Optimization detail: when a bond/vlan has no ethernet-layer
        // properties to set, skip the empty section to keep the wire
        // dict small. (Ethernet is the exception — it always gets the
        // section since it's the type's primary settings.)
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bond("bond0", &["eth0"])],
            ..Default::default()
        };
        let bond_profile = to_nm_profiles(&layered)
            .into_iter()
            .find(|p| p.id == "nasty-bond0")
            .unwrap();
        let dict = to_settings_dict(&bond_profile);
        assert!(
            !dict.contains_key("802-3-ethernet"),
            "empty ethernet section should be omitted on bonds"
        );
    }

    // ── DNS propagation ──────────────────────────────────────────

    #[test]
    fn split_dns_routes_v4_v6_by_colon() {
        // Single-character colon-presence test mirrors what we use in
        // production — IPv4 strings have no `:`, IPv6 strings do.
        let (v4, v6) = split_dns_by_family(&[
            "10.0.0.1".into(),
            "2001:db8::1".into(),
            "1.1.1.1".into(),
            "fe80::1".into(),
        ]);
        assert_eq!(v4, vec!["10.0.0.1".to_string(), "1.1.1.1".to_string()]);
        assert_eq!(v6, vec!["2001:db8::1".to_string(), "fe80::1".to_string()]);
    }

    #[test]
    fn dict_emits_dns_on_ipv4_section_as_packed_uint32() {
        // Regression test for the discussion #159 bug: NM's
        // `ipv4.dns` is type `au` (array of u32 in network byte
        // order), NOT `as`.  Sending a string array gets rejected
        // with "InvalidProperty: ipv4.dns: can't set property of
        // type 'au' from value of type 'as'" which then prevents
        // every other connection in the apply from going through.
        //
        // 10.10.11.1 in network byte order packs to
        //   ((10<<24) | (10<<16) | (11<<8) | 1) = 0x0A0A0B01
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            dns: vec!["10.10.11.1".into()],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let ipv4 = dict.get("ipv4").expect("ipv4 section");
        let dns: Vec<u32> = ipv4["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(dns, vec![0x0A_0A_0B_01]);
        // No leakage into ipv6 (no v6 entries in input).
        let ipv6 = dict.get("ipv6").expect("ipv6 section");
        assert!(!ipv6.contains_key("dns"), "ipv6 must not carry v4 DNS");
    }

    #[test]
    fn dict_routes_dns_to_ipv6_section_as_byte_arrays() {
        // IPv6 DNS is type `aay`: each entry is a 16-byte array
        // (the address in big-endian / network byte order).
        // 2606:4700:4700::1111 octets:
        //   26 06 47 00 47 00 00 00 00 00 00 00 00 00 11 11
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            dns: vec!["2606:4700:4700::1111".into()],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let ipv6 = dict.get("ipv6").expect("ipv6 section");
        let dns: Vec<Vec<u8>> = ipv6["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(
            dns,
            vec![vec![
                0x26, 0x06, 0x47, 0x00, 0x47, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x11, 0x11,
            ]]
        );
        let ipv4 = dict.get("ipv4").expect("ipv4 section");
        assert!(!ipv4.contains_key("dns"), "ipv4 must not carry v6 DNS");
    }

    #[test]
    fn dict_does_not_emit_dns_on_member_port_connections() {
        // Member ports have ipv4/ipv6 method=disabled and don't carry
        // L3 — putting DNS on them is wasted dict bytes and could
        // confuse NM. Bond's own connection (the master) gets the
        // DNS instead.
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bond("bond0", &["eth0"])],
            dns: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        let profiles = to_nm_profiles(&layered);
        let eth0_member = profiles.iter().find(|p| p.id == "nasty-eth0").unwrap();
        let bond_master = profiles.iter().find(|p| p.id == "nasty-bond0").unwrap();
        // Member: no DNS.
        let eth_dict = to_settings_dict(eth0_member);
        assert!(!eth_dict["ipv4"].contains_key("dns"));
        // Master: DNS present.
        let bond_dict = to_settings_dict(bond_master);
        assert!(bond_dict["ipv4"].contains_key("dns"));
    }

    #[test]
    fn keyfile_emits_dns_in_semicolon_separated_form() {
        // NM keyfile uses `dns=A;B;C;` (with trailing `;`). Easy to
        // get wrong — pin it.
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            dns: vec!["10.10.11.1".into(), "1.1.1.1".into()],
            ..Default::default()
        };
        let keyfile = serialize_keyfile(&to_nm_profiles(&layered)[0]);
        assert!(
            keyfile.contains("dns=10.10.11.1;1.1.1.1;"),
            "keyfile missing semi-colon-separated dns: {keyfile}"
        );
    }

    #[test]
    fn dict_omits_dns_when_unset() {
        // Empty dns must not leak an empty `dns=[]` into the dict —
        // NM treats that as "explicitly no DNS" which would replace
        // the DHCP-supplied servers on auto-method connections.
        let layered = LayeredConfig {
            links: vec![link_phys("eth0")],
            ..Default::default()
        };
        let dict = to_settings_dict(&to_nm_profiles(&layered)[0]);
        let ipv4 = dict.get("ipv4").expect("ipv4 section");
        assert!(!ipv4.contains_key("dns"));
    }

    // ── UUIDv5 sanity checks ───────────────────────────────────

    #[test]
    fn uuid_v5_is_canonical_36_chars() {
        let u = deterministic_uuid_for("br0");
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[8], b'-');
        assert_eq!(u.as_bytes()[13], b'-');
        assert_eq!(u.as_bytes()[14], b'5'); // version 5
        assert_eq!(u.as_bytes()[18], b'-');
        assert_eq!(u.as_bytes()[23], b'-');
    }

    #[test]
    fn uuid_v5_is_stable_for_same_name() {
        assert_eq!(
            deterministic_uuid_for("eth0"),
            deterministic_uuid_for("eth0")
        );
        assert_ne!(
            deterministic_uuid_for("eth0"),
            deterministic_uuid_for("eth1")
        );
    }

    // ── MAC inheritance ──────────────────────────────────────────

    fn live_macs(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(n, m)| ((*n).to_string(), (*m).to_string()))
            .collect()
    }

    fn link_bridge_inherit(name: &str, members: &[&str]) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
            kind: LinkKind::Bridge {
                members: members.iter().map(|s| (*s).to_string()).collect(),
                stp: false,
                forward_delay_s: None,
                inherit_member_mac: true,
            },
        }
    }

    fn link_bond_inherit(name: &str, members: &[&str]) -> Link {
        Link {
            name: name.into(),
            enabled: true,
            mtu: None,
            mac: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
            kind: LinkKind::Bond {
                members: members.iter().map(|s| (*s).to_string()).collect(),
                mode: BondMode::Lacp,
                inherit_member_mac: true,
            },
        }
    }

    #[test]
    fn pick_primary_prefers_mgmt_iface_when_member() {
        // Reproducer for the .61 → .62 surprise: user's mgmt iface
        // is in the bond's members. Bond should adopt mgmt's MAC so
        // DHCP keeps handing out the same lease and the session
        // survives the enslave step.
        let kind = LinkKind::Bond {
            members: vec!["eth0".into(), "eth1".into()],
            mode: BondMode::Lacp,
            inherit_member_mac: true,
        };
        let ctx = MacContext {
            live_macs: live_macs(&[("eth0", "aa:..."), ("eth1", "bb:...")]),
            mgmt_iface: Some("eth1".into()),
            infiniband_ifaces: Default::default(),
        };
        assert_eq!(pick_primary_member(&kind, &ctx), Some("eth1"));
    }

    #[test]
    fn pick_primary_falls_back_to_first_when_mgmt_not_a_member() {
        // Bond not touching the mgmt path: pick first declared
        // member as the MAC source. Order matches user intent in
        // the WebUI.
        let kind = LinkKind::Bond {
            members: vec!["eth1".into(), "eth2".into()],
            mode: BondMode::Lacp,
            inherit_member_mac: true,
        };
        let ctx = MacContext {
            live_macs: live_macs(&[]),
            mgmt_iface: Some("eth0".into()), // mgmt not in members
            infiniband_ifaces: Default::default(),
        };
        assert_eq!(pick_primary_member(&kind, &ctx), Some("eth1"));
    }

    #[test]
    fn pick_primary_returns_none_for_non_master() {
        // Physical/VLAN are not master types. The MAC inheritance
        // rule doesn't apply to them; their MAC comes from the
        // hardware directly (or kernel for VLANs).
        let physical = LinkKind::Physical;
        let vlan = LinkKind::Vlan {
            parent: "eth0".into(),
            id: 100,
        };
        let ctx = MacContext::default();
        assert_eq!(pick_primary_member(&physical, &ctx), None);
        assert_eq!(pick_primary_member(&vlan, &ctx), None);
    }

    #[test]
    fn dict_bridge_emits_mac_address_when_inherit_true() {
        // The headline fix: with inherit_member_mac=true and a known
        // member MAC, the bridge connection's [bridge] section
        // carries `mac-address`. NM uses this to pin the bridge's
        // MAC at activation, keeping DHCP recognising the same client.
        let layered = LayeredConfig {
            links: vec![link_phys("ens18"), link_bridge_inherit("br0", &["ens18"])],
            ..Default::default()
        };
        let ctx = MacContext {
            live_macs: live_macs(&[("ens18", "bc:24:11:34:a0:27")]),
            mgmt_iface: None,
            infiniband_ifaces: Default::default(),
        };
        let profiles = to_nm_profiles_with_macs(&layered, &ctx);
        let br = profiles.iter().find(|p| p.id == "nasty-br0").unwrap();
        let dict = to_settings_dict(br);
        let bridge = dict.get("bridge").expect("bridge section");
        let mac: Vec<u8> = bridge["mac-address"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(mac, vec![0xbc, 0x24, 0x11, 0x34, 0xa0, 0x27]);
    }

    #[test]
    fn dict_bridge_omits_mac_address_when_inherit_false() {
        // Opt-out: user toggled "Don't inherit member MAC" → no
        // mac-address field → NM/kernel assign a random MAC at
        // activation (the previous default). Test guards against
        // the flag being silently ignored.
        let layered = LayeredConfig {
            links: vec![link_phys("ens18"), link_bridge("br0", &["ens18"])],
            ..Default::default()
        };
        let ctx = MacContext {
            live_macs: live_macs(&[("ens18", "bc:24:11:34:a0:27")]),
            mgmt_iface: None,
            infiniband_ifaces: Default::default(),
        };
        let profiles = to_nm_profiles_with_macs(&layered, &ctx);
        let br = profiles.iter().find(|p| p.id == "nasty-br0").unwrap();
        assert!(br.mac.is_none());
        let dict = to_settings_dict(br);
        let bridge = dict.get("bridge").expect("bridge section");
        assert!(!bridge.contains_key("mac-address"));
    }

    #[test]
    fn dict_bond_emits_cloned_mac_address_when_inherit_true() {
        // Bonds put MAC in [802-3-ethernet] (not [bridge]) — NM
        // models bonds as having an underlying ethernet layer.
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bond_inherit("bond0", &["eth0"])],
            ..Default::default()
        };
        let ctx = MacContext {
            live_macs: live_macs(&[("eth0", "aa:bb:cc:dd:ee:ff")]),
            mgmt_iface: None,
            infiniband_ifaces: Default::default(),
        };
        let profiles = to_nm_profiles_with_macs(&layered, &ctx);
        let bond = profiles.iter().find(|p| p.id == "nasty-bond0").unwrap();
        let dict = to_settings_dict(bond);
        let eth = dict.get("802-3-ethernet").expect("ethernet section");
        let mac: Vec<u8> = eth["cloned-mac-address"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(mac, vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn keyfile_bridge_mac_address_in_bridge_section() {
        let layered = LayeredConfig {
            links: vec![link_phys("ens18"), link_bridge_inherit("br0", &["ens18"])],
            ..Default::default()
        };
        let ctx = MacContext {
            live_macs: live_macs(&[("ens18", "bc:24:11:34:a0:27")]),
            mgmt_iface: None,
            infiniband_ifaces: Default::default(),
        };
        let br = to_nm_profiles_with_macs(&layered, &ctx)
            .into_iter()
            .find(|p| p.id == "nasty-br0")
            .unwrap();
        let keyfile = serialize_keyfile(&br);
        // Has to land in the [bridge] section, not [ethernet].
        let bridge_idx = keyfile.find("[bridge]").expect("bridge section");
        let after = &keyfile[bridge_idx..];
        let next_section = after[1..].find('[').map_or(after.len(), |i| i + 1);
        assert!(
            after[..next_section].contains("mac-address=bc:24:11:34:a0:27"),
            "[bridge] block missing mac-address: {:?}",
            &after[..next_section]
        );
    }

    #[test]
    fn empty_mac_context_inherits_nothing() {
        // Tests / migration code paths use `to_nm_profiles` (the
        // empty-context wrapper). Bridges/bonds get no inherited
        // MAC — same as the pre-#96-followup behaviour. Guards
        // against test scaffolding accidentally pinning a member's
        // MAC because of the new code path.
        let layered = LayeredConfig {
            links: vec![link_phys("eth0"), link_bridge_inherit("br0", &["eth0"])],
            ..Default::default()
        };
        // No live_macs — even with inherit_member_mac=true, there's
        // no MAC to inherit.
        let profiles = to_nm_profiles(&layered);
        let br = profiles.iter().find(|p| p.id == "nasty-br0").unwrap();
        assert!(br.mac.is_none());
    }
}
