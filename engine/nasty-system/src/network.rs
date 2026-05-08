//! Network configuration management — multi-interface, IPv4/IPv6, bonds, VLANs.
//!
//! Persists to `/var/lib/nasty/networking.json` and generates `/etc/nixos/networking.nix`.
//! Changes are applied immediately via `ip` commands without a full nixos-rebuild.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const JSON_PATH: &str = "/var/lib/nasty/networking.json";
const NIX_PATH: &str = "/etc/nixos/networking.nix";
const HISTORY_DIR: &str = "/var/lib/nasty/networking.history";
const HISTORY_KEEP: usize = 10;

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum IpMethod {
    Dhcp,
    Static,
    Slaac,
    #[default]
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct IpConfig {
    pub method: IpMethod,
    /// Addresses in CIDR notation, e.g. "192.168.1.100/24" or "fd00::1/64".
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InterfaceConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BondMode {
    Lacp,
    ActiveBackup,
    BalanceRr,
    BalanceXor,
}

impl BondMode {
    fn to_kernel(&self) -> &'static str {
        match self {
            BondMode::Lacp => "802.3ad",
            BondMode::ActiveBackup => "active-backup",
            BondMode::BalanceRr => "balance-rr",
            BondMode::BalanceXor => "balance-xor",
        }
    }

    fn to_nix(&self) -> &'static str {
        self.to_kernel()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BondConfig {
    pub name: String,
    pub members: Vec<String>,
    pub mode: BondMode,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VlanConfig {
    pub parent: String,
    pub vlan_id: u16,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

/// A Linux bridge (e.g. `br0`) used as a virtual switch — typically for VMs
/// to share the host's network. Members can be empty (a host-internal bridge
/// that VMs attach to via veth pairs at runtime) or one or more physical /
/// bond interfaces (bridge to the LAN).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BridgeConfig {
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct NetworkConfig {
    #[serde(default)]
    pub interfaces: Vec<InterfaceConfig>,
    #[serde(default)]
    pub dns: Vec<String>,
    #[serde(default)]
    pub bonds: Vec<BondConfig>,
    #[serde(default)]
    pub vlans: Vec<VlanConfig>,
    #[serde(default)]
    pub bridges: Vec<BridgeConfig>,
}

/// Live interface state — read-only, populated at query time.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveInterface {
    pub name: String,
    pub mac: String,
    pub up: bool,
    pub speed_mbps: Option<u32>,
    pub carrier: bool,
    pub ipv4_addresses: Vec<String>,
    pub ipv6_addresses: Vec<String>,
    pub mtu: u32,
    /// "physical", "bond", "vlan", "bridge", "virtual"
    pub kind: String,
}

/// Full network state returned by `system.network.get`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkState {
    pub config: NetworkConfig,
    pub interfaces: Vec<LiveInterface>,
}

// ── Service ────────────────────────────────────────────────────

pub struct NetworkService;

impl Default for NetworkService {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkService {
    pub fn new() -> Self {
        Self
    }

    pub async fn get(&self) -> NetworkState {
        let config = load_config().await;
        let interfaces = enumerate_interfaces().await;
        NetworkState { config, interfaces }
    }

    pub async fn update(&self, config: NetworkConfig) -> Result<(), String> {
        // Validate
        for iface in &config.interfaces {
            validate_ip_config(&iface.ipv4, "IPv4")?;
            validate_ip_config(&iface.ipv6, "IPv6")?;
        }
        for bond in &config.bonds {
            if bond.members.is_empty() {
                return Err(format!("Bond '{}' has no members", bond.name));
            }
            validate_ip_config(&bond.ipv4, "IPv4")?;
            validate_ip_config(&bond.ipv6, "IPv6")?;
        }
        for vlan in &config.vlans {
            if vlan.vlan_id == 0 || vlan.vlan_id > 4094 {
                return Err(format!("VLAN ID {} is invalid (1-4094)", vlan.vlan_id));
            }
            validate_ip_config(&vlan.ipv4, "IPv4")?;
            validate_ip_config(&vlan.ipv6, "IPv6")?;
        }
        for bridge in &config.bridges {
            if bridge.name.is_empty() {
                return Err("Bridge name is required".to_string());
            }
            validate_ip_config(&bridge.ipv4, "IPv4")?;
            validate_ip_config(&bridge.ipv6, "IPv6")?;
        }

        // Persist (atomic write + history snapshot of prior config)
        persist_config(&config).await?;

        // Apply immediately
        apply_config(&config).await?;

        info!(
            "Network config updated ({} interfaces, {} bonds, {} bridges, {} VLANs)",
            config.interfaces.len(),
            config.bonds.len(),
            config.bridges.len(),
            config.vlans.len()
        );
        Ok(())
    }

    /// List physical interfaces (for UI to show available interfaces).
    pub async fn list_interfaces(&self) -> Vec<LiveInterface> {
        enumerate_interfaces().await
    }
}

fn validate_ip_config(ip: &IpConfig, label: &str) -> Result<(), String> {
    if let IpMethod::Static = ip.method
        && ip.addresses.is_empty()
    {
        return Err(format!("{label} static mode requires at least one address"));
    }
    Ok(())
}

// ── Config persistence ─────────────────────────────────────────

async fn persist_config(config: &NetworkConfig) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(config).map_err(|e| format!("serialization error: {e}"))?;

    // Snapshot the existing config to history before overwriting, so a bad
    // apply can be rolled back. Best-effort: a missing/unreadable prior file
    // is fine (first-run case).
    if let Ok(prev) = tokio::fs::read(JSON_PATH).await
        && let Err(e) = snapshot_history(&prev).await
    {
        warn!("failed to snapshot prior network config: {e}");
    }

    atomic_write(JSON_PATH, json.as_bytes())
        .await
        .map_err(|e| format!("failed to write {JSON_PATH}: {e}"))?;

    let nix = generate_nix(config);
    if let Err(e) = atomic_write(NIX_PATH, nix.as_bytes()).await {
        warn!("failed to write {NIX_PATH}: {e}");
    }

    Ok(())
}

/// Write `contents` to `path` atomically: write to `path.tmp`, fsync, rename.
/// Eliminates the half-written-config window if the engine is killed mid-write.
async fn atomic_write(path: &str, contents: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = format!("{path}.tmp");
    let mut f = tokio::fs::File::create(&tmp).await?;
    f.write_all(contents).await?;
    f.sync_all().await?;
    drop(f);
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn snapshot_history(prev: &[u8]) -> std::io::Result<()> {
    tokio::fs::create_dir_all(HISTORY_DIR).await?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = format!("{HISTORY_DIR}/{ts}.json");
    tokio::fs::write(&path, prev).await?;
    prune_history().await;
    Ok(())
}

async fn prune_history() {
    let Ok(mut entries) = tokio::fs::read_dir(HISTORY_DIR).await else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((mtime, path));
    }
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest first
    for (_, path) in files.into_iter().skip(HISTORY_KEEP) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

async fn load_config() -> NetworkConfig {
    match tokio::fs::read_to_string(JSON_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            warn!("Failed to parse {JSON_PATH}: {e}, using defaults");
            NetworkConfig::default()
        }),
        Err(_) => NetworkConfig::default(),
    }
}

// ── Interface enumeration ──────────────────────────────────────

async fn enumerate_interfaces() -> Vec<LiveInterface> {
    let mut result = Vec::new();
    let sys_net = std::path::Path::new("/sys/class/net");
    let Ok(entries) = std::fs::read_dir(sys_net) else {
        return result;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip loopback and Docker/container interfaces
        if name == "lo"
            || name.starts_with("docker")
            || name.starts_with("veth")
            || name.starts_with("br-")
            || name.starts_with("cni")
        {
            continue;
        }

        let path = entry.path();
        let read_file = |f: &str| -> String {
            std::fs::read_to_string(path.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };

        let mac = read_file("address");
        let operstate = read_file("operstate");
        // TUN/TAP and some virtual interfaces report "unknown" when they're working
        let up = operstate == "up" || operstate == "unknown";
        let carrier = read_file("carrier") == "1";
        let mtu: u32 = read_file("mtu").parse().unwrap_or(1500);
        let speed: Option<u32> = read_file("speed")
            .parse()
            .ok()
            .filter(|&s: &u32| s > 0 && s < 100_000);

        // Detect interface type from sysfs
        let tun_flags = read_file("tun_flags");
        let dev_type = read_file("type");

        let kind = if path.join("bonding").is_dir() {
            "bond"
        } else if !tun_flags.is_empty()
            || name.starts_with("tun")
            || name.starts_with("tap")
            || name.starts_with("tailscale")
            || name.starts_with("wg")
        {
            "tunnel"
        } else if dev_type == "772" {
            "vlan"
        } else if path.join("bridge").is_dir() {
            "bridge"
        } else {
            "physical"
        };

        let ipv4_addresses = get_addresses(&name, false).await;
        let ipv6_addresses = get_addresses(&name, true).await;

        result.push(LiveInterface {
            name,
            mac,
            up,
            speed_mbps: speed,
            carrier,
            ipv4_addresses,
            ipv6_addresses,
            mtu,
            kind: kind.to_string(),
        });
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

async fn get_addresses(iface: &str, ipv6: bool) -> Vec<String> {
    let flag = if ipv6 { "-6" } else { "-4" };
    let inet = if ipv6 { "inet6" } else { "inet" };
    let Ok(output) = tokio::process::Command::new("ip")
        .args([flag, "addr", "show", iface])
        .output()
        .await
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with(inet) {
                let addr = line.split_whitespace().nth(1)?;
                // Skip link-local for IPv6 unless it's the only one
                if ipv6 && addr.starts_with("fe80:") {
                    return None;
                }
                Some(addr.to_string())
            } else {
                None
            }
        })
        .collect()
}

// ── Apply config ───────────────────────────────────────────────

async fn apply_config(config: &NetworkConfig) -> Result<(), String> {
    // Apply bonds first (they need to exist before members are enslaved)
    for bond in &config.bonds {
        // Create bond if it doesn't exist
        if !std::path::Path::new(&format!("/sys/class/net/{}", bond.name)).exists() {
            run_ip(&[
                "link",
                "add",
                &bond.name,
                "type",
                "bond",
                "mode",
                bond.mode.to_kernel(),
            ])
            .await
            .map_err(|e| format!("create bond {}: {e}", bond.name))?;
        }
        // Enslave members
        for member in &bond.members {
            let _ = run_ip(&["link", "set", member, "down"]).await;
            let _ = run_ip(&["link", "set", member, "master", &bond.name]).await;
        }
        run_ip(&["link", "set", &bond.name, "up"])
            .await
            .map_err(|e| format!("bring up bond {}: {e}", bond.name))?;
        if let Some(mtu) = bond.mtu {
            let _ = run_ip(&["link", "set", &bond.name, "mtu", &mtu.to_string()]).await;
        }
        apply_ip_config(&bond.name, &bond.ipv4, false).await?;
        apply_ip_config(&bond.name, &bond.ipv6, true).await?;
    }

    // Apply bridges (after bonds so bonds can be members; before VLANs so
    // VLANs can sit on top of a bridge).
    for bridge in &config.bridges {
        if !std::path::Path::new(&format!("/sys/class/net/{}", bridge.name)).exists() {
            run_ip(&["link", "add", &bridge.name, "type", "bridge"])
                .await
                .map_err(|e| format!("create bridge {}: {e}", bridge.name))?;
        }
        for member in &bridge.members {
            let _ = run_ip(&["link", "set", member, "down"]).await;
            let _ = run_ip(&["link", "set", member, "master", &bridge.name]).await;
            let _ = run_ip(&["link", "set", member, "up"]).await;
        }
        run_ip(&["link", "set", &bridge.name, "up"])
            .await
            .map_err(|e| format!("bring up bridge {}: {e}", bridge.name))?;
        if let Some(mtu) = bridge.mtu {
            let _ = run_ip(&["link", "set", &bridge.name, "mtu", &mtu.to_string()]).await;
        }
        apply_ip_config(&bridge.name, &bridge.ipv4, false).await?;
        apply_ip_config(&bridge.name, &bridge.ipv6, true).await?;
    }

    // Apply VLANs
    for vlan in &config.vlans {
        let vlan_name = format!("{}.{}", vlan.parent, vlan.vlan_id);
        if !std::path::Path::new(&format!("/sys/class/net/{vlan_name}")).exists() {
            run_ip(&[
                "link",
                "add",
                "link",
                &vlan.parent,
                "name",
                &vlan_name,
                "type",
                "vlan",
                "id",
                &vlan.vlan_id.to_string(),
            ])
            .await
            .map_err(|e| format!("create vlan {vlan_name}: {e}"))?;
        }
        run_ip(&["link", "set", &vlan_name, "up"])
            .await
            .map_err(|e| format!("bring up vlan {vlan_name}: {e}"))?;
        if let Some(mtu) = vlan.mtu {
            let _ = run_ip(&["link", "set", &vlan_name, "mtu", &mtu.to_string()]).await;
        }
        apply_ip_config(&vlan_name, &vlan.ipv4, false).await?;
        apply_ip_config(&vlan_name, &vlan.ipv6, true).await?;
    }

    // Apply interface configs
    for iface in &config.interfaces {
        if !iface.enabled {
            let _ = run_ip(&["link", "set", &iface.name, "down"]).await;
            continue;
        }
        run_ip(&["link", "set", &iface.name, "up"])
            .await
            .map_err(|e| format!("bring up {}: {e}", iface.name))?;
        if let Some(mtu) = iface.mtu {
            let _ = run_ip(&["link", "set", &iface.name, "mtu", &mtu.to_string()]).await;
        }
        apply_ip_config(&iface.name, &iface.ipv4, false).await?;
        apply_ip_config(&iface.name, &iface.ipv6, true).await?;
    }

    // Apply DNS via resolvconf (NixOS manages /etc/resolv.conf — writing it
    // directly causes "signature mismatch" errors on the next rebuild).
    if !config.dns.is_empty() {
        let resolv: String = config
            .dns
            .iter()
            .map(|ns| format!("nameserver {ns}\n"))
            .collect();
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("resolvconf")
            .args(["-a", "nasty", "-m", "0"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("resolvconf: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(resolv.as_bytes())
                .await
                .map_err(|e| format!("resolvconf stdin: {e}"))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("resolvconf wait: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("resolvconf failed: {stderr}");
        }
    }

    Ok(())
}

async fn apply_ip_config(iface: &str, ip: &IpConfig, v6: bool) -> Result<(), String> {
    let flag = if v6 { "-6" } else { "-4" };
    match ip.method {
        IpMethod::Dhcp => {
            if !v6 {
                // Per-iface rebind. Replaces the previous global `systemctl
                // restart dhcpcd`, which dropped leases on every other iface.
                dhcp::start(iface).await;
            }
            // DHCPv6 is typically handled by dhcpcd or systemd-networkd.
        }
        IpMethod::Static => {
            if !v6 {
                // Release any prior dhcpcd lease so it can't fight the static
                // config (re-add a leased address, replace our default route).
                dhcp::stop(iface).await;
            }
            let _ = run_ip(&[flag, "addr", "flush", "dev", iface]).await;
            for addr in &ip.addresses {
                run_ip(&[flag, "addr", "add", addr, "dev", iface])
                    .await
                    .map_err(|e| format!("ip addr add {addr} on {iface}: {e}"))?;
            }
            if let Some(ref gw) = ip.gateway {
                let _ =
                    run_ip(&[flag, "route", "replace", "default", "via", gw, "dev", iface]).await;
            }
        }
        IpMethod::Slaac => {
            if v6 {
                let sysctl_path = format!("/proc/sys/net/ipv6/conf/{iface}/autoconf");
                let _ = tokio::fs::write(&sysctl_path, "1").await;
                let accept_ra = format!("/proc/sys/net/ipv6/conf/{iface}/accept_ra");
                let _ = tokio::fs::write(&accept_ra, "1").await;
            }
        }
        IpMethod::Disabled => {
            if !v6 {
                dhcp::stop(iface).await;
            }
            let _ = run_ip(&[flag, "addr", "flush", "dev", iface]).await;
            if v6 {
                let sysctl_path = format!("/proc/sys/net/ipv6/conf/{iface}/disable_ipv6");
                let _ = tokio::fs::write(&sysctl_path, "1").await;
            }
        }
    }
    Ok(())
}

/// Per-interface DHCP control. Behind a small module seam so the underlying
/// client (currently dhcpcd) can be swapped — e.g. for systemd-networkd —
/// without touching call sites.
mod dhcp {
    use tracing::warn;

    /// Start or rebind dhcpcd for `iface`.
    pub async fn start(iface: &str) {
        run(&["-n", iface], "rebind", iface).await;
    }

    /// Release the lease and stop dhcpcd for `iface`. No-op if not running
    /// — that's the common case when switching from Static to Static.
    pub async fn stop(iface: &str) {
        run(&["-k", iface], "release", iface).await;
    }

    async fn run(args: &[&str], action: &str, iface: &str) {
        match tokio::process::Command::new("dhcpcd")
            .args(args)
            .output()
            .await
        {
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let stderr = stderr.trim();
                if stderr.is_empty() {
                    warn!("dhcpcd {action} {iface} exited with non-zero status");
                } else {
                    warn!("dhcpcd {action} {iface} failed: {stderr}");
                }
            }
            Err(e) => warn!("dhcpcd {action} {iface}: spawn failed: {e}"),
            _ => {}
        }
    }
}

// ── NixOS config generation ────────────────────────────────────

fn generate_nix(config: &NetworkConfig) -> String {
    let mut out =
        String::from("# Managed by NASty — edit via WebUI Settings > Network\n{ ... }:\n{\n");

    out.push_str("  networking.useDHCP = false;\n\n");

    // Interfaces
    for iface in &config.interfaces {
        if !iface.enabled {
            continue;
        }
        generate_iface_nix(&mut out, &iface.name, &iface.ipv4, &iface.ipv6, iface.mtu);
    }

    // Bonds
    for bond in &config.bonds {
        let members: Vec<String> = bond.members.iter().map(|m| format!("\"{m}\"")).collect();
        out.push_str(&format!(
            "  networking.bonds.{} = {{\n    interfaces = [ {} ];\n    driverOptions.mode = \"{}\";\n  }};\n",
            bond.name, members.join(" "), bond.mode.to_nix()
        ));
        generate_iface_nix(&mut out, &bond.name, &bond.ipv4, &bond.ipv6, bond.mtu);
    }

    // Bridges
    for bridge in &config.bridges {
        let members: Vec<String> = bridge.members.iter().map(|m| format!("\"{m}\"")).collect();
        out.push_str(&format!(
            "  networking.bridges.{} = {{ interfaces = [ {} ]; }};\n",
            bridge.name,
            members.join(" ")
        ));
        generate_iface_nix(
            &mut out,
            &bridge.name,
            &bridge.ipv4,
            &bridge.ipv6,
            bridge.mtu,
        );
    }

    // VLANs
    for vlan in &config.vlans {
        let vlan_name = format!("{}-{}", vlan.parent, vlan.vlan_id);
        out.push_str(&format!(
            "  networking.vlans.{vlan_name} = {{ id = {}; interface = \"{}\"; }};\n",
            vlan.vlan_id, vlan.parent
        ));
        let iface_name = format!("{}.{}", vlan.parent, vlan.vlan_id);
        generate_iface_nix(&mut out, &iface_name, &vlan.ipv4, &vlan.ipv6, vlan.mtu);
    }

    // DNS
    if !config.dns.is_empty() {
        let items: Vec<String> = config.dns.iter().map(|ns| format!("\"{ns}\"")).collect();
        out.push_str(&format!(
            "  networking.nameservers = [ {} ];\n",
            items.join(" ")
        ));
    }

    out.push_str("}\n");
    out
}

fn generate_iface_nix(
    out: &mut String,
    name: &str,
    ipv4: &IpConfig,
    ipv6: &IpConfig,
    mtu: Option<u16>,
) {
    match ipv4.method {
        IpMethod::Dhcp => {
            out.push_str(&format!("  networking.interfaces.{name}.useDHCP = true;\n"));
        }
        IpMethod::Static => {
            let addrs: Vec<String> = ipv4
                .addresses
                .iter()
                .map(|a| {
                    let parts: Vec<&str> = a.split('/').collect();
                    let addr = parts[0];
                    let prefix: u8 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
                    format!("{{ address = \"{addr}\"; prefixLength = {prefix}; }}")
                })
                .collect();
            out.push_str(&format!(
                "  networking.interfaces.{name}.ipv4.addresses = [ {} ];\n",
                addrs.join(" ")
            ));
            if let Some(ref gw) = ipv4.gateway {
                out.push_str(&format!("  networking.defaultGateway = \"{gw}\";\n"));
            }
        }
        _ => {}
    }

    match ipv6.method {
        IpMethod::Slaac => {
            // NixOS enables SLAAC by default when IPv6 is not disabled
        }
        IpMethod::Static => {
            let addrs: Vec<String> = ipv6
                .addresses
                .iter()
                .map(|a| {
                    let parts: Vec<&str> = a.split('/').collect();
                    let addr = parts[0];
                    let prefix: u8 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(64);
                    format!("{{ address = \"{addr}\"; prefixLength = {prefix}; }}")
                })
                .collect();
            out.push_str(&format!(
                "  networking.interfaces.{name}.ipv6.addresses = [ {} ];\n",
                addrs.join(" ")
            ));
            if let Some(ref gw) = ipv6.gateway {
                out.push_str(&format!("  networking.defaultGateway6 = \"{gw}\";\n"));
            }
        }
        IpMethod::Disabled => {
            out.push_str(&format!(
                "  networking.interfaces.{name}.ipv6.addresses = [];\n"
            ));
        }
        _ => {}
    }

    if let Some(mtu) = mtu {
        out.push_str(&format!("  networking.interfaces.{name}.mtu = {mtu};\n"));
    }
}

// ── Helpers ────────────────────────────────────────────────────

pub async fn detect_primary_interface() -> Option<String> {
    // Try IPv4 first, then IPv6
    for flag in &["-4", "-6"] {
        let target = if *flag == "-4" {
            "1.1.1.1"
        } else {
            "2001:4860:4860::8888"
        };
        let output = tokio::process::Command::new("ip")
            .args([flag, "route", "get", target])
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
    }
    None
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
