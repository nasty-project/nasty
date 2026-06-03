//! IPv6 host-detection helpers shared by the share-protocol services.
//!
//! Used to drive dual-stack auto-portal defaults: on a host with a
//! global v6 address, target/subsystem creation can add a second
//! listen socket for free; on a v4-only host the same calls stay v4.
//!
//! Both helpers shell out to `ip` rather than reading `/proc/net/if_inet6`
//! directly — `ip` already filters out tentative addresses, masks scope
//! correctly, and matches what every other "is v6 working here?" check
//! in NASty uses (see `network::get_addresses`).

/// True if the host has at least one configured IPv6 address with
/// global scope (excluding link-local `fe80::/10` and loopback `::1`).
///
/// Used to decide whether to add a `[::]:port` companion to a fresh
/// `0.0.0.0:port` portal. Returns `false` on v4-only hosts, and also
/// on errors invoking `ip` — better to silently stay v4 than to fail
/// the user's create call because `ip` was missing.
pub(crate) async fn host_has_global_ipv6() -> bool {
    let Ok(output) = tokio::process::Command::new("ip")
        .args(["-6", "addr", "show", "scope", "global"])
        .output()
        .await
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .any(|line| line.trim_start().starts_with("inet6 "))
}

/// Best-effort detect a routable IPv6 source address. Used by NVMe-oF
/// where configfs requires a concrete address per port (the `::`
/// wildcard rejects with EINVAL during subsystem linking). Mirrors
/// `detect_primary_ip()` in `nvmeof.rs` but for v6 — picks the
/// address the kernel would use to reach a global v6 target.
///
/// Returns `None` on v4-only hosts, on missing `ip` binary, or when
/// no v6 default route is configured.
pub(crate) async fn detect_primary_ipv6() -> Option<String> {
    // 2001:4860:4860::8888 is Google Public DNS over v6 — same role as
    // 1.1.1.1 in the v4 path: a stable globally-routable address used
    // only as a routing-table probe target. No packet is actually sent.
    let output = tokio::process::Command::new("ip")
        .args(["-6", "route", "get", "2001:4860:4860::8888"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Output: "2001:4860:4860::8888 from :: via fe80::1 dev eth0 src 2001:db8::5 metric 100 pref medium"
    let mut iter = text.split_whitespace();
    while let Some(token) = iter.next() {
        if token == "src" {
            return iter.next().map(|s| s.to_string());
        }
    }
    None
}
