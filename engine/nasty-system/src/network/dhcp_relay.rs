//! Managed DHCPv4 relay. The bridge settings are the authoritative configuration;
//! daemon and firewall artifacts are regenerated on apply, rollback, and boot.

use super::{IpMethod, NetworkConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

const CONF: &str = "/var/lib/nasty/dhcp-relay.conf";
const POLICY: &str = "/var/lib/nasty/dhcp-relay-rules.nft";
const UNIT: &str = "nasty-dhcp-relay.service";
// Serialize artifact changes with full firewall replacements so an unrelated
// service toggle cannot overwrite the relay policy with an older snapshot.
pub(crate) static POLICY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DhcpRelayConfig {
    pub server: Ipv4Addr,
    pub upstream: String,
}

fn interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
}

fn unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_link_local()
        && (1..224).contains(&ip.octets()[0])
}

fn cidr_parts(cidr: &str) -> Result<(Ipv4Addr, u32, u32, u8), String> {
    let (address, prefix) = cidr
        .split_once('/')
        .ok_or("relay bridge address needs an IPv4 prefix")?;
    let ip: Ipv4Addr = address
        .parse()
        .map_err(|_| "relay bridge address must be IPv4")?;
    let prefix: u8 = prefix.parse().map_err(|_| "invalid relay bridge prefix")?;
    if prefix > 32 {
        return Err("invalid IPv4 prefix".into());
    }
    let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
    let network = u32::from(ip) & mask;
    Ok((ip, network, mask, prefix))
}

fn subnet(cidr: &str) -> Result<(Ipv4Addr, u32, u32), String> {
    let (ip, network, mask, prefix) = cidr_parts(cidr)?;
    if !(1..=30).contains(&prefix) {
        return Err("relay bridge prefix must be between 1 and 30".into());
    }
    if !unicast(ip) || u32::from(ip) == network || u32::from(ip) == network | !mask {
        return Err("relay bridge needs a usable unicast host address".into());
    }
    Ok((ip, network, mask))
}

/// Returns dnsmasq configuration and commands populating our dedicated chain.
/// All interpolated values are validated; free text never reaches either syntax.
pub fn render(config: &NetworkConfig) -> Result<(String, String), String> {
    let mut conf = String::from("port=0\nbind-dynamic\nlog-dhcp\n");
    let mut rules = String::new();
    let mut subnets = Vec::new();
    for bridge in &config.bridges {
        let Some(relay) = &bridge.dhcp_relay else {
            continue;
        };
        if !interface_name(&bridge.name)
            || !interface_name(&relay.upstream)
            || bridge.name == relay.upstream
        {
            return Err(
                "DHCP relay requires distinct valid bridge and upstream interface names".into(),
            );
        }
        if !bridge.members.is_empty() {
            return Err(format!(
                "DHCP relay bridge '{}' must have no physical members",
                bridge.name
            ));
        }
        if bridge.ipv4.method != IpMethod::Static
            || bridge.ipv4.addresses.len() != 1
            || bridge.ipv4.gateway.is_some()
        {
            return Err(
                "DHCP relay requires one static IPv4 bridge address and no host default gateway"
                    .into(),
            );
        }
        let (local, network, mask) = subnet(&bridge.ipv4.addresses[0])?;
        if !unicast(relay.server) || u32::from(relay.server) & mask == network {
            return Err(
                "DHCP server must be a unicast IPv4 address outside the guest subnet".into(),
            );
        }
        for (other_network, other_mask) in &subnets {
            if network & other_mask == *other_network || other_network & mask == network {
                return Err("DHCP relay bridge subnets must not overlap".into());
            }
        }
        // Reject overlaps with other configured static addresses as well.
        for cidr in config
            .interfaces
            .iter()
            .flat_map(|i| &i.ipv4.addresses)
            .chain(config.bonds.iter().flat_map(|b| &b.ipv4.addresses))
            .chain(config.vlans.iter().flat_map(|v| &v.ipv4.addresses))
            .chain(
                config
                    .bridges
                    .iter()
                    .filter(|b| b.name != bridge.name)
                    .flat_map(|b| &b.ipv4.addresses),
            )
        {
            if let Ok((_, other_network, other_mask, _)) = cidr_parts(cidr)
                && (network & other_mask == other_network || other_network & mask == network)
            {
                return Err(format!(
                    "DHCP relay subnet overlaps configured address {cidr}"
                ));
            }
        }
        subnets.push((network, mask));
        conf.push_str(&format!(
            "interface={}\ndhcp-relay={local},{},{}\n",
            bridge.name, relay.server, relay.upstream
        ));
        rules.push_str(&format!(
            "add rule inet nasty dhcp_relay iifname \"{}\" udp sport 68 udp dport 67 accept\nadd rule inet nasty dhcp_relay iifname \"{}\" ip saddr {} udp sport 67 udp dport 67 accept\n",
            bridge.name, relay.upstream, relay.server));
    }
    if rules.is_empty() {
        conf.clear();
    }
    Ok((conf, rules))
}

pub fn validate_upstreams(
    config: &NetworkConfig,
    live: &std::collections::HashSet<String>,
) -> Result<(), String> {
    for bridge in &config.bridges {
        if let Some(relay) = &bridge.dhcp_relay
            && (!live.contains(&relay.upstream)
                || config
                    .bridges
                    .iter()
                    .any(|b| b.name == relay.upstream || b.members.contains(&relay.upstream))
                || config
                    .bonds
                    .iter()
                    .any(|b| b.members.contains(&relay.upstream)))
        {
            return Err(format!(
                "DHCP relay upstream '{}' must be an existing standalone L3 interface",
                relay.upstream
            ));
        }
    }
    Ok(())
}

pub(crate) async fn policy() -> Result<String, String> {
    match tokio::fs::read_to_string(POLICY).await {
        Ok(rules) => Ok(rules),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("read DHCP relay policy: {e}")),
    }
}

async fn systemctl(action: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("systemctl")
        .args([action, UNIT])
        .output()
        .await
        .map_err(|e| format!("DHCP relay {action}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "DHCP relay {action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub async fn reconcile(config: &NetworkConfig) -> Result<(), String> {
    let (conf, rules) = render(config)?;
    let _lock = POLICY_LOCK.lock().await;
    let result = reconcile_locked(config, conf, rules).await;
    if result.is_err() {
        // Fail closed even when a same-config retry finds a failed daemon or
        // changed route. Keep the original error; log cleanup failures.
        if let Err(error) = systemctl("stop").await {
            tracing::warn!("DHCP relay error cleanup: {error}");
        }
        let empty = "flush chain inet nasty dhcp_relay\n";
        if let Err(error) = crate::firewall::run_nft(
            std::path::Path::new("nft"),
            &["--file", "-"],
            empty,
            "relay cleanup",
        )
        .await
        {
            tracing::warn!("DHCP relay firewall cleanup: {error}");
        }
        if let Err(error) = super::atomic_write(POLICY, b"").await {
            tracing::warn!("DHCP relay policy cleanup: {error}");
        }
    }
    result
}

async fn validate_runtime(config: &NetworkConfig) -> Result<(), String> {
    if config.bridges.iter().any(|b| b.dhcp_relay.is_some()) {
        let forwarding = tokio::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
            .await
            .map_err(|e| format!("read IPv4 forwarding state: {e}"))?;
        if forwarding.trim() != "1" {
            return Err("DHCP relay routing requires IPv4 forwarding to be enabled".into());
        }
    }
    for bridge in &config.bridges {
        let Some(relay) = &bridge.dhcp_relay else {
            continue;
        };
        let output = tokio::process::Command::new("ip")
            .args(["-4", "-j", "route", "get", &relay.server.to_string()])
            .output()
            .await
            .map_err(|e| format!("DHCP relay route lookup: {e}"))?;
        let routes: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .map_err(|_| "DHCP server route lookup failed")?;
        if !output.status.success()
            || !routes.iter().any(|r| {
                r["dev"].as_str() == Some(&relay.upstream) && r["type"].as_str() != Some("local")
            })
        {
            return Err(format!(
                "DHCP server {} must be routed via '{}'",
                relay.server, relay.upstream
            ));
        }
        let output = tokio::process::Command::new("ip")
            .args(["-4", "-j", "addr", "show", "dev", &bridge.name])
            .output()
            .await
            .map_err(|e| format!("DHCP relay bridge lookup: {e}"))?;
        let interfaces: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .map_err(|_| "DHCP relay bridge is not active")?;
        let local = bridge.ipv4.addresses[0]
            .split('/')
            .next()
            .unwrap_or_default();
        if !output.status.success()
            || !interfaces.iter().any(|i| {
                i["addr_info"].as_array().is_some_and(|addresses| {
                    addresses.iter().any(|a| a["local"].as_str() == Some(local))
                })
            })
        {
            return Err(format!(
                "DHCP relay bridge '{}' is missing address {local}",
                bridge.name
            ));
        }
    }
    Ok(())
}

async fn reconcile_locked(
    config: &NetworkConfig,
    conf: String,
    rules: String,
) -> Result<(), String> {
    let old = match tokio::fs::read_to_string(CONF).await {
        Ok(conf) => conf,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read DHCP relay configuration: {e}")),
    };
    if conf.is_empty() && old.is_empty() && policy().await?.is_empty() {
        return Ok(());
    }
    // Stop before changing the listener scope, and never start if policy fails.
    if conf != old || conf.is_empty() {
        systemctl("stop").await?;
    }
    // ActivateConnection returns before asynchronous address configuration
    // finishes. Wait briefly for the selected bridge and upstream route.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match validate_runtime(config).await {
            Ok(()) => break,
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
    let batch = format!("flush chain inet nasty dhcp_relay\n{rules}");
    crate::firewall::run_nft(
        std::path::Path::new("nft"),
        &["--check", "--file", "-"],
        &batch,
        "relay validation",
    )
    .await?;
    crate::firewall::run_nft(
        std::path::Path::new("nft"),
        &["--file", "-"],
        &batch,
        "relay apply",
    )
    .await?;
    // POLICY_LOCK excludes full firewall replacements until persistence ends.
    super::atomic_write(POLICY, rules.as_bytes())
        .await
        .map_err(|e| format!("persist DHCP relay policy: {e}"))?;
    super::atomic_write(CONF, conf.as_bytes())
        .await
        .map_err(|e| format!("persist DHCP relay configuration: {e}"))?;
    if !conf.is_empty() {
        systemctl(if conf == old { "start" } else { "restart" }).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> NetworkConfig {
        serde_json::from_value(serde_json::json!({"bridges": [{
            "name":"br-vm", "members":[],
            "ipv4":{"method":"static", "addresses":["10.10.30.1/24"]},
            "dhcp_relay":{"server":"10.10.20.97", "upstream":"enp1s0"}
        }]}))
        .unwrap()
    }
    #[test]
    fn isolated_relay_disables_dns_and_scopes_firewall() {
        let (conf, rules) = render(&config()).unwrap();
        assert!(conf.starts_with("port=0\n"));
        assert!(conf.contains("dhcp-relay=10.10.30.1,10.10.20.97,enp1s0"));
        assert!(rules.contains("iifname \"br-vm\" udp sport 68 udp dport 67"));
        assert!(rules.contains("iifname \"enp1s0\" ip saddr 10.10.20.97"));
        assert!(!conf.contains("dhcp-range"));
    }
    #[test]
    fn absent_relay_preserves_legacy_behavior() {
        assert_eq!(
            render(&NetworkConfig::default()).unwrap(),
            (String::new(), String::new())
        );
    }
    #[test]
    fn detects_overlapping_host_routes_and_subnets() {
        for cidr in [
            "10.10.30.2/32",
            "10.10.30.2/31",
            "10.10.0.1/16",
            "10.10.30.129/25",
        ] {
            let mut c = config();
            c.interfaces = serde_json::from_value(serde_json::json!([{
                "name":"other", "ipv4":{"method":"static", "addresses":[cidr]}
            }]))
            .unwrap();
            assert!(render(&c).unwrap_err().contains("overlap"), "{cidr}");
        }
    }
    #[test]
    fn validates_upstream_presence_and_membership() {
        let mut c = config();
        assert!(validate_upstreams(&c, &Default::default()).is_err());
        let live = ["enp1s0".to_string()].into_iter().collect();
        assert!(validate_upstreams(&c, &live).is_ok());
        c.bridges[0].members.push("enp1s0".into());
        assert!(validate_upstreams(&c, &live).is_err());
    }
    #[test]
    fn multiple_relays_remain_scoped_and_disjoint() {
        let mut c = config();
        let mut second = c.bridges[0].clone();
        second.name = "br-other".into();
        second.ipv4.addresses = vec!["10.10.40.1/24".into()];
        c.bridges.push(second);
        let (conf, rules) = render(&c).unwrap();
        assert_eq!(conf.matches("dhcp-relay=").count(), 2);
        assert!(rules.contains("iifname \"br-other\""));
        c.bridges[1].ipv4.addresses = vec!["10.10.30.129/25".into()];
        assert!(render(&c).is_err());
    }
    #[test]
    fn rejects_shared_lan_dynamic_address_gateway_and_injection() {
        for mutate in [0, 1, 2, 3, 4, 5] {
            let mut c = config();
            let b = &mut c.bridges[0];
            match mutate {
                0 => b.members.push("enp1s0".into()),
                1 => b.ipv4.method = IpMethod::Dhcp,
                2 => b.ipv4.gateway = Some("10.10.30.254".into()),
                3 => b.name = "bad\"; accept".into(),
                4 => b.ipv4.addresses = vec!["10.10.30.0/24".into()],
                _ => b.dhcp_relay.as_mut().unwrap().server = "10.10.30.2".parse().unwrap(),
            }
            assert!(render(&c).is_err(), "case {mutate}");
        }
    }
    #[test]
    fn relay_survives_layered_round_trip() {
        let c = config();
        assert_eq!(
            super::super::layered::from_layered(&super::super::layered::to_layered(&c)),
            c
        );
    }
}
