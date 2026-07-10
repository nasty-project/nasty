# AD Domain Controller Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a NASty box host an Active Directory domain (Samba AD DC, single-DC, SAMBA_INTERNAL DNS) provisioned and managed from the WebUI — the second half of issue #20.

**Architecture:** A new `dc.rs` module in `nasty-system` (sibling to member mode's `domain.rs`, which stays untouched except a mutual-exclusion check and two helper visibility bumps) owns provision/demote/status, user/group/computer management, and domain backup. The DC runs as a nix-defined, engine-toggled `samba-dc.service` with an engine-owned config (`--configfile=/etc/samba/smb.dc.conf`) that includes the existing share chain, so the DC's integrated smbd serves the box's shares. systemd `Conflicts=` swaps member-mode daemons out atomically. The firewall gains a `dc` service-rule set (and its first ranged service port). WebUI: the Settings "Directory" card becomes three-state with a `DcPanel` component.

**Tech Stack:** Rust (`nasty-system`, `nasty-engine`), Samba (`samba-tool`, `samba` AD DC daemon), NixOS module + VM test, Svelte 5.

## Global Constraints

- **Secrets never ride argv** (visible in `/proc/*/cmdline`). Provision uses a random throwaway `--adminpass`, then sets the real Administrator password via `samba-tool user setpassword` **fed over stdin**. Every user-password operation feeds stdin. There is a unit test pinning this invariant.
- **The nix-managed `/etc/samba/smb.conf` is never touched.** All DC samba-tool/daemon invocations pass `--configfile=/etc/samba/smb.dc.conf` (engine-owned).
- **Exactly one DC per domain** (new-domain provision only). Mutual exclusion with member mode enforced both ways (`dc.provision` refuses when joined; `domain.join` refuses when hosting).
- **Domain state stays on root ext4** at `/var/lib/samba` — no relocation to `/fs`.
- **`dc.backup` destination is jailed under `/fs`** (canonicalize + prefix check; must not exist or be an empty dir).
- **Demote destroys the domain**: typed-realm confirmation required; a final backup parachute is taken into `/fs` first when a filesystem exists.
- **Service switchover via `Conflicts=`**: `samba-dc.service` conflicts with `samba-smbd`/`samba-nmbd`/`samba-winbindd`. It is NOT `wantedBy` anything — the engine starts it (provision + boot-restore phase `dc.restore`).
- **One samba build**: nasty.nix's `sambaAds` becomes the DC-capable superset (`enableLDAP = true; enableDomainController = true;` + `python3Packages.cryptography` on `pythonPath`).
- **Roles:** `dc.status` = `Any`; every other `dc.*` method = `Admin`. `dc.user.list`, `dc.group.list`, `dc.computer.list` MUST be added to the `is_read_only` carve-out in `router/mod.rs` (their `.list` suffix would otherwise leak them to ReadOnly sessions — same trap `domain.user.list` already documents).
- **Static-IP precondition (amended from spec, approved):** hard-fail only when NASty's own network config (`networking.json`) manages interfaces and none is `Static` (all-DHCP → error naming the Network page); when NASty has no opinion (no managed interfaces — externally configured box, CI VM), provision proceeds and the response carries a **warning**.
- Firewall `dc` port set: tcp+udp 53, 88, 464; tcp 135, 139, 389, 445, 636, 3268, 3269; udp 137, 138; tcp range 49152–65535. **No NTP port** (the box does not serve time).
- Verification before every Rust commit (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`. WebUI (from `webui/`): `npm run check && npm test`. Nix changes are gated by the Integration workflow (VM tests), not local builds.
- No new Rust dependencies (so no nix derivation input changes for the engine build).

---

## File Structure

- `engine/nasty-system/src/firewall.rs` — **modify**: `PortSpec.to: Option<u16>` (additive), ranged render + ranged `service_port_conflict`, `dc_ports()`, `open_dc()`/`close_dc()`.
- `engine/nasty-system/src/domain.rs` — **modify (minimal)**: `run_cmd`/`run_cmd_stdin` become `pub(crate)`; `join()` gains the "not while hosting" check.
- `engine/nasty-system/src/dc.rs` — **create**: everything DC — types, persistence, validation, renders, provision/demote/status/backup, users/groups/computers.
- `engine/nasty-system/src/lib.rs` — **modify**: `pub mod dc;`.
- `engine/nasty-engine/src/main.rs` — **modify**: `AppState.dc`, `dc.restore` boot phase.
- `engine/nasty-engine/src/router/dc.rs` — **create**: `dc.*` arms. `router/mod.rs` — **modify**: chain the new router, extend the `is_read_only` carve-out + its test.
- `engine/nasty-engine/src/registry/methods.rs` — **modify**: `dc.*` entries.
- `nixos/modules/nasty.nix` — **modify**: DC-capable samba build, `samba-dc.service`.
- `webui/src/lib/types.ts`, `webui/src/lib/dc.svelte.ts` (**create**), `webui/src/lib/directory/DcPanel.svelte` (**create**), `webui/src/routes/settings/+page.svelte`, `webui/src/routes/firewall/+page.svelte` — WebUI.
- `nixos/tests/ad-dc.nix` — **create**; `flake.nix` + `.github/workflows/integration.yml` — **modify**: register the check.

---

## Task 1: Firewall — ranged PortSpec + the `dc` service rule

**Files:**
- Modify: `engine/nasty-system/src/firewall.rs`
- Test: `engine/nasty-system/src/firewall.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `PortSpec`, `render_ruleset`, `service_port_conflict`, `open_rdma`/`close_rdma` (the shape to mirror).
- Produces:
  - `PortSpec.to: Option<u16>` — `#[serde(default, skip_serializing_if = "Option::is_none")]`; `None` = single port (all existing behavior unchanged).
  - `pub fn dc_ports() -> Vec<PortSpec>`
  - `pub async fn open_dc(&self)` / `pub async fn close_dc(&self)` on `FirewallService` — the `"dc"` named rule, exact mirrors of `open_rdma`/`close_rdma`.

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `firewall.rs`:

```rust
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
    assert!(out.contains("tcp dport 49152-65535 accept # dc"), "got:\n{out}");
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
    assert_eq!(service_port_conflict(&state, Transport::Tcp, 40000, 49151), None);
}

#[test]
fn dc_ports_cover_ad_services() {
    let ports = dc_ports();
    let has = |t: Transport, p: u16| ports.iter().any(|s| s.transport == t && s.port == p && s.to.is_none());
    for p in [53u16, 88, 464] {
        assert!(has(Transport::Tcp, p) && has(Transport::Udp, p), "missing tcp+udp {p}");
    }
    for p in [135u16, 139, 389, 445, 636, 3268, 3269] {
        assert!(has(Transport::Tcp, p), "missing tcp {p}");
    }
    for p in [137u16, 138] {
        assert!(has(Transport::Udp, p), "missing udp {p}");
    }
    assert!(
        ports.iter().any(|s| s.transport == Transport::Tcp && s.port == 49152 && s.to == Some(65535)),
        "missing RPC range"
    );
    // No NTP — the box does not serve time.
    assert!(!ports.iter().any(|s| s.port == 123));
}
```

- [ ] **Step 2: Run to verify they fail**

Run (from `engine/`): `cargo test -p nasty-system firewall::tests::render_emits_ranged firewall::tests::service_conflict_matches_ranged firewall::tests::dc_ports_cover`
Expected: FAIL — no field `to`, no `dc_ports`.

- [ ] **Step 3: Add `to` to `PortSpec` and fix every constructor**

In `struct PortSpec`, after `port`:

```rust
    /// Optional end of a contiguous port range (`port`..=`to`). `None`
    /// means a single port. First used by the DC role's dynamic-RPC range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<u16>,
```

Every `PortSpec { ... }` literal in the file gains `to: None` **except** where propagating an existing spec. The touch points: `tcp()` and `udp()` helpers, and the three literals inside `apply_restrictions` (those propagate: `to: port.to,`). Add a ranged helper next to `tcp()`/`udp()`:

```rust
fn tcp_range(from: u16, to: u16) -> PortSpec {
    PortSpec {
        port: from,
        to: Some(to),
        transport: Transport::Tcp,
        source: None,
        iface: None,
    }
}
```

- [ ] **Step 4: Teach the service render loop and the conflict check about ranges**

In `render_ruleset`'s **service** loop, replace the dport condition line:

```rust
            match port.to {
                Some(to) => conditions.push(format!("{proto} dport {}-{to}", port.port)),
                None => conditions.push(format!("{proto} dport {}", port.port)),
            }
```

In `service_port_conflict`, replace the containment test with interval intersection:

```rust
            let hi = port.to.unwrap_or(port.port);
            if port.transport == transport && port.port <= to && hi >= from {
                return Some(rule.service.clone());
            }
```

- [ ] **Step 5: Add `dc_ports()` and the open/close methods**

Next to `rdma_ports()`:

```rust
/// Ports for the Active Directory DC role (#20): DNS, Kerberos +
/// kpasswd, RPC endpoint mapper, NetBIOS, LDAP(S), SMB, Global Catalog,
/// and the dynamic RPC range. Deliberately no NTP — the box does not
/// serve time to domain clients (documented limitation).
pub fn dc_ports() -> Vec<PortSpec> {
    let mut ports = Vec::new();
    for p in [53u16, 88, 464] {
        ports.push(tcp(p));
        ports.push(udp(p));
    }
    for p in [135u16, 139, 389, 445, 636, 3268, 3269] {
        ports.push(tcp(p));
    }
    for p in [137u16, 138] {
        ports.push(udp(p));
    }
    ports.push(tcp_range(49152, 65535));
    ports
}
```

In `impl FirewallService`, after `close_rdma`, add `open_dc`/`close_dc` — copy `open_rdma`/`close_rdma` verbatim, replacing `"rdma"` with `"dc"` and `rdma_ports()` with `dc_ports()` (including the custom-lock acquisition and `apply_nftables(&state, &custom)` call those methods already carry).

- [ ] **Step 6: Run the tests + full crate**

Run (from `engine/`): `cargo test -p nasty-system`
Expected: new tests PASS; all existing firewall tests (renders, custom rules, validation) still pass — single-port behavior is unchanged because `to: None` renders exactly as before.

- [ ] **Step 7: Full verification + commit**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`

```bash
cd engine && cargo fmt
git add nasty-system/src/firewall.rs
git commit -m "firewall: ranged service ports + dc rule set (#20)"
```

---

## Task 2: dc.rs — types, persistence, validation, renders, command hygiene

**Files:**
- Create: `engine/nasty-system/src/dc.rs`
- Modify: `engine/nasty-system/src/lib.rs` (add `pub mod dc;`)
- Modify: `engine/nasty-system/src/domain.rs` (visibility: `run_cmd` and `run_cmd_stdin` become `pub(crate) async fn`; doc comments unchanged)
- Test: `engine/nasty-system/src/dc.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `domain::{validate_realm, derive_workgroup}` (already `pub`), `domain::{run_cmd, run_cmd_stdin}` (made `pub(crate)` here), `network::NetworkConfig`/`IpMethod` (already `pub`).
- Produces (Tasks 3–5 rely on these exact names):
  - `pub const DC_CONF_PATH: &str = "/etc/samba/smb.dc.conf";`
  - `pub const DC_STATE_PATH: &str = "/var/lib/nasty/dc.json";`
  - `pub const DC_RESOLVED_DROPIN_PATH: &str = "/run/systemd/resolved.conf.d/nasty-dc.conf";`
  - `pub enum DcError { Validation(String), Precondition(String), CommandFailed(String), NotHosting, AlreadyHosting, Io(std::io::Error) }` (thiserror, operator-facing messages).
  - `pub struct DcConfig { pub realm: String, pub workgroup: String, pub dns_forwarder: String }` (Serialize/Deserialize/Clone/Debug/JsonSchema).
  - `pub struct ProvisionRequest { pub realm: String, pub admin_password: String, pub dns_forwarder: Option<String> }`, `pub struct DemoteRequest { pub realm_confirmation: String }` (Deserialize/JsonSchema; passwords redacted from Debug or Debug not derived).
  - `pub struct DcStatus { pub hosting: bool, pub realm: Option<String>, pub workgroup: Option<String>, pub dns_forwarder: Option<String>, pub service_healthy: bool }` (Serialize/JsonSchema).
  - `pub fn render_dc_resolved_dropin() -> String`
  - `pub fn insert_into_global(conf: &str, extra: &str) -> String`
  - `pub fn nasty_global_additions(dns_forwarder: &str) -> String`
  - `pub enum StaticIpCheck { Pass, Warn(String), Fail(String) }` + `pub fn static_ip_check(cfg: Option<&crate::network::NetworkConfig>) -> StaticIpCheck`
  - `pub fn validate_backup_dest(dest: &Path, root: &Path) -> Result<PathBuf, DcError>`
  - `fn samba_tool_args<'a>(sub: &[&'a str]) -> Vec<&'a str>` — prefixes every call with the subcommand and appends `["--configfile", DC_CONF_PATH]`.
  - `DcService::{new, load_config, save_config, clear_config}` (assoc fns mirroring `DomainService`'s, own path).

- [ ] **Step 1: Write the failing tests**

Create `engine/nasty-system/src/dc.rs` with the module doc, the constants/types above as stubs where needed, and this test module (the tests define the contracts; implement to satisfy them):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // ── insert_into_global ────────────────────────────────────
    #[test]
    fn insert_into_global_lands_inside_global_section() {
        let conf = "# Global parameters\n[global]\n\tdns forwarder = 127.0.0.53\n\trealm = AD.EXAMPLE.COM\n\n[sysvol]\n\tpath = /var/lib/samba/sysvol\n\n[netlogon]\n\tpath = /var/lib/samba/sysvol/ad.example.com/scripts\n";
        let out = insert_into_global(conf, "\tinclude = /etc/samba/smb.nasty.conf\n");
        let global_pos = out.find("[global]").unwrap();
        let include_pos = out.find("include = /etc/samba/smb.nasty.conf").unwrap();
        let sysvol_pos = out.find("[sysvol]").unwrap();
        assert!(global_pos < include_pos && include_pos < sysvol_pos, "got:\n{out}");
        // Original content preserved
        assert!(out.contains("[netlogon]"));
    }

    #[test]
    fn nasty_global_additions_carry_include_and_forwarder() {
        let extra = nasty_global_additions("192.168.1.1");
        assert!(extra.contains("include = /etc/samba/smb.nasty.conf"));
        assert!(extra.contains("dns forwarder = 192.168.1.1"));
    }

    // ── resolved drop-in ──────────────────────────────────────
    #[test]
    fn dc_resolved_dropin_points_box_at_samba_dns() {
        let out = render_dc_resolved_dropin();
        assert!(out.contains("DNSStubListener=no"));
        assert!(out.contains("DNS=127.0.0.1"));
    }

    // ── static-IP precondition ────────────────────────────────
    fn iface(name: &str, method: crate::network::IpMethod) -> crate::network::InterfaceConfig {
        // Build via serde so the test doesn't chase struct-field churn:
        serde_json::from_value(serde_json::json!({
            "name": name,
            "method": method,
        }))
        .expect("InterfaceConfig from minimal json (all other fields serde-default)")
    }

    #[test]
    fn static_ip_check_matrix() {
        use crate::network::{IpMethod, NetworkConfig};
        // No config at all → externally managed → warn, not fail.
        assert!(matches!(static_ip_check(None), StaticIpCheck::Warn(_)));
        // Empty config → same.
        let empty = NetworkConfig::default();
        assert!(matches!(static_ip_check(Some(&empty)), StaticIpCheck::Warn(_)));
        // A static interface → pass.
        let mut ok = NetworkConfig::default();
        ok.interfaces.push(iface("eth0", IpMethod::Static));
        assert!(matches!(static_ip_check(Some(&ok)), StaticIpCheck::Pass));
        // Managed but all-DHCP → fail, message names the Network page.
        let mut bad = NetworkConfig::default();
        bad.interfaces.push(iface("eth0", IpMethod::Dhcp));
        match static_ip_check(Some(&bad)) {
            StaticIpCheck::Fail(msg) => assert!(msg.contains("Network"), "msg: {msg}"),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    // ── backup-dest jail ──────────────────────────────────────
    #[test]
    fn backup_dest_jail() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tank")).unwrap();
        // ok: not-yet-existing dir under root
        let dest = root.path().join("tank").join("dc-backup");
        assert!(validate_backup_dest(&dest, root.path()).is_ok());
        // ok: existing but empty
        std::fs::create_dir(&dest).unwrap();
        assert!(validate_backup_dest(&dest, root.path()).is_ok());
        // reject: non-empty
        std::fs::write(dest.join("x"), b"x").unwrap();
        assert!(validate_backup_dest(&dest, root.path()).is_err());
        // reject: traversal escape
        let esc = root.path().join("tank").join("..").join("..").join("etc");
        assert!(validate_backup_dest(&esc, root.path()).is_err());
        // reject: relative
        assert!(validate_backup_dest(std::path::Path::new("x/y"), root.path()).is_err());
    }

    // ── argv hygiene: THE invariant ───────────────────────────
    #[test]
    fn no_secret_ever_in_samba_tool_argv() {
        // Every argv builder used for password-carrying operations must
        // keep the password out. The builders return the argv; the secret
        // travels via run_cmd_stdin.
        let secret = "Sup3r.Secret!";
        for argv in [
            setpassword_args("Administrator"),
            user_create_args("alice", None, None),
        ] {
            assert!(
                !argv.iter().any(|a| a.contains(secret)),
                "secret leaked into argv: {argv:?}"
            );
            // and every samba-tool call pins the DC config
            assert!(argv.windows(2).any(|w| w[0] == "--configfile" && w[1] == DC_CONF_PATH));
        }
        // Provision's argv carries ONLY the throwaway password, never the
        // operator's. (The throwaway is random per call; assert the real
        // secret isn't there.)
        let prov = provision_args("AD.EXAMPLE.COM", "ADEXAMPLE", "throwaway-x");
        assert!(!prov.iter().any(|a| a.contains(secret)));
        assert!(prov.iter().any(|a| a == "--dns-backend=SAMBA_INTERNAL"));
        assert!(prov.iter().any(|a| a == "--server-role=dc"));
    }
}
```

Notes for the implementer: `tempfile` is already a dev-dependency of `nasty-backup`, not necessarily of `nasty-system` — check `grep tempfile engine/nasty-system/Cargo.toml` and add `tempfile = "3"` under `[dev-dependencies]` if absent. The test helper builds `InterfaceConfig` through serde on purpose — only `name` and `method` matter and every other field is `#[serde(default)]`; if deserialization fails because some field lacks a default, construct the struct literally instead and note it.

- [ ] **Step 2: Run to verify they fail**

Run (from `engine/`): `cargo test -p nasty-system dc::tests`
Expected: FAIL — none of the functions exist yet.

- [ ] **Step 3: Implement the module core**

The full implementation (top of `dc.rs`, above the tests):

```rust
//! Active Directory Domain Controller role (#20, second half).
//!
//! NASty *hosts* a domain: `samba-tool domain provision` with the
//! SAMBA_INTERNAL DNS backend, run against an engine-owned config
//! (`smb.dc.conf`) so the nix-managed /etc/samba/smb.conf is never
//! touched. The DC's integrated smbd serves the box's shares via the
//! existing `smb.nasty.conf` include chain. Exactly one DC per domain;
//! mutual exclusion with member mode (`domain.rs`) both ways.
//!
//! Credential rule (same as member join): secrets NEVER ride argv —
//! provision uses a random throwaway password, the real Administrator
//! password is set via `samba-tool user setpassword` over stdin.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::domain::{run_cmd, run_cmd_stdin};

pub const DC_CONF_PATH: &str = "/etc/samba/smb.dc.conf";
pub const DC_STATE_PATH: &str = "/var/lib/nasty/dc.json";
pub const DC_RESOLVED_DROPIN_PATH: &str = "/run/systemd/resolved.conf.d/nasty-dc.conf";
/// Root every domain-backup destination must resolve under.
const FS_ROOT: &str = "/fs";
/// NASty's own network config — consulted (read-only) by the static-IP
/// precondition. Path mirrors `network.rs`'s private `JSON_PATH`.
const NETWORKING_JSON_PATH: &str = "/var/lib/nasty/networking.json";

#[derive(Debug, thiserror::Error)]
pub enum DcError {
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Precondition(String),
    #[error("{0}")]
    CommandFailed(String),
    #[error("this box is not hosting a domain")]
    NotHosting,
    #[error("this box is already hosting a domain — demote it first")]
    AlreadyHosting,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<crate::domain::DomainError> for DcError {
    fn from(e: crate::domain::DomainError) -> Self {
        DcError::CommandFailed(e.to_string())
    }
}

/// Persisted DC role state. Presence of the file == this box hosts a domain.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DcConfig {
    pub realm: String,
    pub workgroup: String,
    pub dns_forwarder: String,
}

#[derive(Clone, Deserialize, JsonSchema)]
pub struct ProvisionRequest {
    /// Kerberos realm / DNS zone for the new domain, e.g. "ad.example.lan".
    pub realm: String,
    /// Administrator password. Set via samba-tool over stdin; never argv,
    /// never logged, never persisted.
    pub admin_password: String,
    /// Upstream DNS the DC forwards non-domain queries to. Defaults to the
    /// box's current upstream resolver.
    #[serde(default)]
    pub dns_forwarder: Option<String>,
}

#[derive(Clone, Deserialize, JsonSchema)]
pub struct DemoteRequest {
    /// Must exactly match the hosted realm — demoting DESTROYS the domain.
    pub realm_confirmation: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DcStatus {
    pub hosting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workgroup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_forwarder: Option<String>,
    /// Whether samba-dc.service is active. Meaningful only when hosting.
    pub service_healthy: bool,
}

// ── Pure renders / helpers ─────────────────────────────────────

/// resolved drop-in for DC mode: samba's SAMBA_INTERNAL DNS owns :53,
/// so resolved must release the stub listener and the box resolves
/// through samba (which forwards non-domain queries upstream).
pub fn render_dc_resolved_dropin() -> String {
    "# Managed by NASty — this box hosts an AD domain; samba's internal\n\
     # DNS owns port 53. Removed at demote.\n\
     [Resolve]\n\
     DNS=127.0.0.1\n\
     DNSStubListener=no\n"
        .to_string()
}

/// The [global] lines NASty adds to the provision-generated smb.dc.conf:
/// the share include chain and the upstream DNS forwarder.
pub fn nasty_global_additions(dns_forwarder: &str) -> String {
    format!(
        "\t# NASty-managed additions (share include chain + upstream DNS)\n\
         \tinclude = /etc/samba/smb.nasty.conf\n\
         \tdns forwarder = {dns_forwarder}\n"
    )
}

/// Insert `extra` immediately after the `[global]` section header. A blind
/// append would land inside the last share section ([netlogon]) instead.
pub fn insert_into_global(conf: &str, extra: &str) -> String {
    let mut out = String::with_capacity(conf.len() + extra.len());
    let mut inserted = false;
    for line in conf.lines() {
        out.push_str(line);
        out.push('\n');
        if !inserted && line.trim() == "[global]" {
            out.push_str(extra);
            inserted = true;
        }
    }
    if !inserted {
        // No [global] header at all (unexpected) — prepend one.
        return format!("[global]\n{extra}{out}");
    }
    out
}

/// Static-IP precondition (spec-amended): Fail only when NASty's own
/// network config manages interfaces and none is Static; Warn when NASty
/// has no opinion (externally managed box / fresh VM); Pass when at least
/// one managed interface is Static.
#[derive(Debug)]
pub enum StaticIpCheck {
    Pass,
    Warn(String),
    Fail(String),
}

pub fn static_ip_check(cfg: Option<&crate::network::NetworkConfig>) -> StaticIpCheck {
    let Some(cfg) = cfg else {
        return StaticIpCheck::Warn(
            "NASty does not manage this box's network config — make sure the DC's IP address is static (a DHCP-addressed DC breaks the domain when the lease changes)".into(),
        );
    };
    if cfg.interfaces.is_empty() {
        return StaticIpCheck::Warn(
            "NASty does not manage this box's network config — make sure the DC's IP address is static (a DHCP-addressed DC breaks the domain when the lease changes)".into(),
        );
    }
    if cfg
        .interfaces
        .iter()
        .any(|i| i.method == crate::network::IpMethod::Static)
    {
        return StaticIpCheck::Pass;
    }
    StaticIpCheck::Fail(
        "a domain controller needs a static IP address, but every NASty-managed interface uses DHCP — set a static address on the Network page first".into(),
    )
}

/// Jail a domain-backup destination under `/fs` (root parameterized for
/// tests). Must be absolute, resolve under root after canonicalizing the
/// existing prefix, and be either absent or an empty directory —
/// `samba-tool domain backup` requires an empty target dir. Local sibling
/// of nasty-backup's restore jail; duplicated across the crate boundary
/// on purpose (nasty-system must not depend on nasty-backup).
pub fn validate_backup_dest(dest: &Path, root: &Path) -> Result<PathBuf, DcError> {
    if !dest.is_absolute() {
        return Err(DcError::Validation("destination must be an absolute path".into()));
    }
    let mut existing = dest;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let canonical_prefix = loop {
        match existing.canonicalize() {
            Ok(p) => break p,
            Err(_) => {
                let file = existing
                    .file_name()
                    .ok_or_else(|| DcError::Validation("destination escapes /fs".into()))?;
                tail.push(file);
                existing = existing
                    .parent()
                    .ok_or_else(|| DcError::Validation("destination escapes /fs".into()))?;
            }
        }
    };
    let mut resolved = canonical_prefix;
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    let canonical_root = root.canonicalize().map_err(DcError::Io)?;
    if !resolved.starts_with(&canonical_root) || resolved == canonical_root {
        return Err(DcError::Validation(
            "destination must be inside a NASty filesystem under /fs".into(),
        ));
    }
    if resolved.is_dir() {
        let mut entries = std::fs::read_dir(&resolved)?;
        if entries.next().is_some() {
            return Err(DcError::Validation(
                "destination directory is not empty — samba-tool requires an empty backup target".into(),
            ));
        }
    } else if resolved.exists() {
        return Err(DcError::Validation("destination exists and is not a directory".into()));
    }
    Ok(resolved)
}

// ── samba-tool argv builders (pure; unit-tested for argv hygiene) ──

fn with_conf(mut argv: Vec<String>) -> Vec<String> {
    argv.push("--configfile".into());
    argv.push(DC_CONF_PATH.into());
    argv
}

pub(crate) fn provision_args(realm: &str, workgroup: &str, throwaway_pass: &str) -> Vec<String> {
    with_conf(vec![
        "domain".into(),
        "provision".into(),
        format!("--realm={realm}"),
        format!("--domain={workgroup}"),
        "--server-role=dc".into(),
        "--dns-backend=SAMBA_INTERNAL".into(),
        format!("--adminpass={throwaway_pass}"),
    ])
}

pub(crate) fn setpassword_args(user: &str) -> Vec<String> {
    with_conf(vec!["user".into(), "setpassword".into(), user.into()])
}

pub(crate) fn user_create_args(
    name: &str,
    given_name: Option<&str>,
    surname: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["user".into(), "create".into(), name.into()];
    if let Some(g) = given_name {
        argv.push(format!("--given-name={g}"));
    }
    if let Some(s) = surname {
        argv.push(format!("--surname={s}"));
    }
    with_conf(argv)
}
```

Then the service skeleton + persistence (the lifecycle methods land in Task 3):

```rust
// ── Service ────────────────────────────────────────────────────

pub struct DcService;

impl Default for DcService {
    fn default() -> Self {
        Self::new()
    }
}

impl DcService {
    pub fn new() -> Self {
        Self
    }

    pub async fn load_config() -> Option<DcConfig> {
        let raw = tokio::fs::read_to_string(DC_STATE_PATH).await.ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub async fn save_config(config: &DcConfig) -> Result<(), DcError> {
        let json = serde_json::to_string_pretty(config)
            .map_err(|e| DcError::Validation(format!("serialize dc config: {e}")))?;
        tokio::fs::write(DC_STATE_PATH, json).await?;
        Ok(())
    }

    pub async fn clear_config() -> Result<(), DcError> {
        match tokio::fs::remove_file(DC_STATE_PATH).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Run samba-tool with the given argv (already carrying --configfile).
async fn samba_tool(argv: &[String]) -> Result<String, DcError> {
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    Ok(run_cmd("samba-tool", &args, &[]).await?)
}

/// Run samba-tool feeding `stdin_input` (the only way secrets travel).
async fn samba_tool_stdin(argv: &[String], stdin_input: &str) -> Result<String, DcError> {
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    Ok(run_cmd_stdin("samba-tool", &args, &[], stdin_input).await?)
}
```

In `domain.rs`, change the two helper signatures (nothing else):

```rust
pub(crate) async fn run_cmd(
pub(crate) async fn run_cmd_stdin(
```

In `lib.rs`, add `pub mod dc;` next to `pub mod domain;`.

Note: `provision_args`/`setpassword_args`/`user_create_args`/`samba_tool`/`samba_tool_stdin` will be `dead_code` until Task 3 wires them — add `#[allow(dead_code)]` on `samba_tool`/`samba_tool_stdin` ONLY if clippy demands it, with a `// consumed by Task 3 lifecycle` comment, and Task 3 MUST remove it. The `pub(crate)` argv builders are exercised by the tests, so they're live already.

- [ ] **Step 4: Run to verify the tests pass**

Run (from `engine/`): `cargo test -p nasty-system dc::tests`
Expected: PASS (all six).

- [ ] **Step 5: Full verification + commit**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`

```bash
cd engine && cargo fmt
git add nasty-system/src/dc.rs nasty-system/src/lib.rs nasty-system/src/domain.rs nasty-system/Cargo.toml
git commit -m "dc: types, persistence, validation, renders, samba-tool argv hygiene (#20)"
```

---

## Task 3: dc.rs — provision / demote / status / backup lifecycle

**Files:**
- Modify: `engine/nasty-system/src/dc.rs`
- Modify: `engine/nasty-system/src/domain.rs` (join gains the hosting check)
- Test: `engine/nasty-system/src/dc.rs` (one new pure test), plus compile/clippy/suite — the lifecycle itself is VM-test territory (Task 8), same convention as member join.

**Interfaces:**
- Consumes: everything Task 2 produced; `domain::{validate_realm, derive_workgroup}`; `DomainService::load_config`.
- Produces (Tasks 5/8 rely on):
  - `pub async fn provision(&self, req: ProvisionRequest) -> Result<(DcStatus, Vec<String>), DcError>` — the `Vec<String>` is warnings (static-IP warn).
  - `pub async fn demote(&self, req: DemoteRequest) -> Result<(), DcError>`
  - `pub async fn status(&self) -> DcStatus`
  - `pub async fn backup(&self, dest: &str) -> Result<String, DcError>` — returns the tarball path.
  - `pub async fn ensure_running(&self) -> bool` — boot-restore: true when hosting (caller opens the firewall).

- [ ] **Step 1: Write the one new pure failing test**

```rust
    #[test]
    fn find_backup_tarball_picks_the_tarball() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("samba-backup-2026-07-10.tar.bz2"), b"x").unwrap();
        let found = find_backup_tarball(dir.path()).unwrap();
        assert!(found.ends_with("samba-backup-2026-07-10.tar.bz2"));
    }
```

Run (from `engine/`): `cargo test -p nasty-system dc::tests::find_backup_tarball` — Expected: FAIL (missing fn).

- [ ] **Step 2: Implement the lifecycle**

Add to `dc.rs` (inside `impl DcService`, plus the free helpers). Complete code:

```rust
    /// Provision a brand-new AD domain on this box. Returns the status plus
    /// operator-facing warnings (e.g. externally-managed network config).
    ///
    /// Sequence: preflight → samba-tool provision (throwaway --adminpass,
    /// engine-owned smb.dc.conf) → insert NASty [global] additions → set the
    /// real Administrator password via stdin → resolved drop-in (release
    /// :53) → start samba-dc.service → persist dc.json. Any failure after
    /// provision unwinds through the demote teardown (minus the backup).
    pub async fn provision(
        &self,
        req: ProvisionRequest,
    ) -> Result<(DcStatus, Vec<String>), DcError> {
        // ── Preflight ──────────────────────────────────────────
        if Self::load_config().await.is_some() {
            return Err(DcError::AlreadyHosting);
        }
        if crate::domain::DomainService::load_config().await.is_some() {
            return Err(DcError::Precondition(
                "this box is joined to a domain as a member — leave the domain before hosting one".into(),
            ));
        }
        let realm = crate::domain::validate_realm(&req.realm)
            .map_err(|e| DcError::Validation(e.to_string()))?;
        let workgroup = crate::domain::derive_workgroup(&realm);

        let mut warnings = Vec::new();
        let net_cfg: Option<crate::network::NetworkConfig> =
            tokio::fs::read_to_string(NETWORKING_JSON_PATH)
                .await
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
        match static_ip_check(net_cfg.as_ref()) {
            StaticIpCheck::Pass => {}
            StaticIpCheck::Warn(w) => warnings.push(w),
            StaticIpCheck::Fail(msg) => return Err(DcError::Precondition(msg)),
        }

        let dns_forwarder = resolve_dns_forwarder(req.dns_forwarder.as_deref()).await?;

        // A stale DC config would make samba-tool refuse to provision.
        let _ = tokio::fs::remove_file(DC_CONF_PATH).await;

        // ── Provision (throwaway password on argv, rotated below) ──
        let throwaway = uuid::Uuid::new_v4().to_string();
        info!("dc: provisioning domain {realm} (workgroup {workgroup})");
        samba_tool(&provision_args(&realm, &workgroup, &throwaway)).await?;

        // Everything past this point unwinds on failure.
        let result: Result<(), DcError> = async {
            // NASty [global] additions inside the generated config.
            let conf = tokio::fs::read_to_string(DC_CONF_PATH).await?;
            let conf = insert_into_global(&conf, &nasty_global_additions(&dns_forwarder));
            tokio::fs::write(DC_CONF_PATH, conf).await?;

            // Real Administrator password — over stdin, twice (prompt +
            // confirmation). Never argv, never logged.
            samba_tool_stdin(
                &setpassword_args("Administrator"),
                &format!("{}\n{}", req.admin_password, req.admin_password),
            )
            .await?;

            // Release :53 to samba, route the box's own lookups through it.
            tokio::fs::create_dir_all("/run/systemd/resolved.conf.d").await?;
            tokio::fs::write(DC_RESOLVED_DROPIN_PATH, render_dc_resolved_dropin()).await?;
            run_cmd("systemctl", &["restart", "systemd-resolved"], &[]).await?;

            // samba-dc conflicts with smbd/nmbd/winbindd — systemd swaps
            // them atomically.
            run_cmd("systemctl", &["start", "samba-dc.service"], &[]).await?;

            Self::save_config(&DcConfig {
                realm: realm.clone(),
                workgroup: workgroup.clone(),
                dns_forwarder: dns_forwarder.clone(),
            })
            .await?;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            warn!("dc: provision failed after samba-tool provision — unwinding: {e}");
            self.teardown().await;
            return Err(e);
        }

        info!("dc: domain {realm} provisioned");
        Ok((self.status().await, warnings))
    }

    /// Demote — DESTROYS the domain. Typed-realm confirmation; takes a
    /// final backup parachute into /fs when a filesystem exists.
    pub async fn demote(&self, req: DemoteRequest) -> Result<(), DcError> {
        let cfg = Self::load_config().await.ok_or(DcError::NotHosting)?;
        if req.realm_confirmation.trim() != cfg.realm {
            return Err(DcError::Validation(format!(
                "confirmation does not match the hosted realm ({})",
                cfg.realm
            )));
        }
        // Parachute: best-effort final backup into /fs.
        match first_fs_dir().await {
            Some(fs_dir) => {
                let dest = format!(
                    "{fs_dir}/dc-final-backup-{}",
                    chrono::Utc::now().format("%Y%m%d-%H%M%S")
                );
                match self.backup(&dest).await {
                    Ok(path) => info!("dc: final domain backup written to {path}"),
                    Err(e) => warn!("dc: final backup failed (continuing demote): {e}"),
                }
            }
            None => warn!("dc: no /fs filesystem mounted — demoting WITHOUT a final backup"),
        }
        info!("dc: demoting — destroying domain {}", cfg.realm);
        self.teardown().await;
        Ok(())
    }

    /// Shared teardown for demote and provision-unwind: stop the DC, remove
    /// its config + domain state, restore resolved, clear dc.json. Every
    /// step best-effort — teardown must never strand the box half-torn.
    async fn teardown(&self) {
        let _ = run_cmd("systemctl", &["stop", "samba-dc.service"], &[]).await;
        let _ = tokio::fs::remove_file(DC_CONF_PATH).await;
        // Domain databases + sysvol. Root-resident by design.
        let _ = tokio::fs::remove_dir_all("/var/lib/samba/private").await;
        let _ = tokio::fs::remove_dir_all("/var/lib/samba/sysvol").await;
        let _ = tokio::fs::remove_dir_all("/var/lib/samba/bind-dns").await;
        if tokio::fs::remove_file(DC_RESOLVED_DROPIN_PATH).await.is_ok() {
            let _ = run_cmd("systemctl", &["restart", "systemd-resolved"], &[]).await;
        }
        let _ = Self::clear_config().await;
    }

    pub async fn status(&self) -> DcStatus {
        match Self::load_config().await {
            Some(cfg) => {
                let healthy = run_cmd("systemctl", &["is-active", "samba-dc.service"], &[])
                    .await
                    .is_ok();
                DcStatus {
                    hosting: true,
                    realm: Some(cfg.realm),
                    workgroup: Some(cfg.workgroup),
                    dns_forwarder: Some(cfg.dns_forwarder),
                    service_healthy: healthy,
                }
            }
            None => DcStatus {
                hosting: false,
                realm: None,
                workgroup: None,
                dns_forwarder: None,
                service_healthy: false,
            },
        }
    }

    /// Domain backup: `samba-tool domain backup offline` into a /fs-jailed,
    /// empty target dir. Returns the tarball path.
    pub async fn backup(&self, dest: &str) -> Result<String, DcError> {
        if Self::load_config().await.is_none() {
            return Err(DcError::NotHosting);
        }
        let resolved =
            validate_backup_dest(Path::new(dest), Path::new(FS_ROOT))?;
        tokio::fs::create_dir_all(&resolved).await?;
        let target = resolved.to_string_lossy().to_string();
        samba_tool(&with_conf(vec![
            "domain".into(),
            "backup".into(),
            "offline".into(),
            format!("--targetdir={target}"),
        ]))
        .await?;
        find_backup_tarball(&resolved).ok_or_else(|| {
            DcError::CommandFailed("samba-tool reported success but no backup tarball was found".into())
        })
    }

    /// Boot restore (`dc.restore` phase): when hosting, rewrite the /run
    /// resolved drop-in (tmpfs — gone after reboot), restart resolved if it
    /// changed, and start the DC. Returns whether the box hosts a domain so
    /// the caller can open the firewall.
    pub async fn ensure_running(&self) -> bool {
        if Self::load_config().await.is_none() {
            return false;
        }
        let dropin = render_dc_resolved_dropin();
        let current = tokio::fs::read_to_string(DC_RESOLVED_DROPIN_PATH)
            .await
            .unwrap_or_default();
        if current != dropin {
            let _ = tokio::fs::create_dir_all("/run/systemd/resolved.conf.d").await;
            if tokio::fs::write(DC_RESOLVED_DROPIN_PATH, dropin).await.is_ok() {
                let _ = run_cmd("systemctl", &["restart", "systemd-resolved"], &[]).await;
            }
        }
        if let Err(e) = run_cmd("systemctl", &["start", "samba-dc.service"], &[]).await {
            warn!("dc: failed to start samba-dc at boot restore: {e}");
        }
        true
    }
```

Free helpers (module level):

```rust
/// The upstream DNS forwarder: explicit wins; else the first global server
/// from `resolvectl dns`; else an error asking the operator to supply one
/// (once samba owns :53 there is no other upstream path).
async fn resolve_dns_forwarder(explicit: Option<&str>) -> Result<String, DcError> {
    if let Some(f) = explicit {
        let f = f.trim();
        if f.parse::<std::net::IpAddr>().is_err() {
            return Err(DcError::Validation(format!(
                "dns_forwarder must be an IP address, got: {f}"
            )));
        }
        return Ok(f.to_string());
    }
    let out = run_cmd("resolvectl", &["dns"], &[]).await.unwrap_or_default();
    for token in out.split_whitespace() {
        if let Ok(ip) = token.parse::<std::net::IpAddr>() {
            if !ip.is_loopback() {
                return Ok(ip.to_string());
            }
        }
    }
    Err(DcError::Precondition(
        "could not determine an upstream DNS forwarder — supply one in the provision form".into(),
    ))
}

/// First mounted filesystem under /fs (for the demote parachute).
async fn first_fs_dir() -> Option<String> {
    let mut rd = tokio::fs::read_dir(FS_ROOT).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            return Some(entry.path().to_string_lossy().to_string());
        }
    }
    None
}

/// The tarball samba-tool wrote into the backup target dir.
fn find_backup_tarball(dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".tar.bz2") {
            return Some(entry.path().to_string_lossy().to_string());
        }
    }
    None
}
```

Check `nasty-system/Cargo.toml` has `uuid` and `chrono` (both confirmed present: `uuid.workspace = true`, `chrono` with `clock` feature). Remove any `#[allow(dead_code)]` Task 2 left on `samba_tool`/`samba_tool_stdin`.

In `domain.rs`'s `join()`, at the top of the preflight (right after its existing validation begins), add:

```rust
        if crate::dc::DcService::load_config().await.is_some() {
            return Err(DomainError::Validation(
                "this box hosts an Active Directory domain — demote it before joining another domain".into(),
            ));
        }
```

- [ ] **Step 3: Run tests + full verification**

Run (from `engine/`): `cargo test -p nasty-system dc::tests` — all pass (including `find_backup_tarball_picks_the_tarball`).
Then: `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`

- [ ] **Step 4: Commit**

```bash
cd engine && cargo fmt
git add nasty-system/src/dc.rs nasty-system/src/domain.rs
git commit -m "dc: provision/demote/status/backup lifecycle with unwind (#20)"
```

---

## Task 4: dc.rs — users, groups, computers

**Files:**
- Modify: `engine/nasty-system/src/dc.rs`
- Test: `engine/nasty-system/src/dc.rs`

**Interfaces:**
- Consumes: `samba_tool`, `samba_tool_stdin`, `with_conf`, `user_create_args` (Task 2/3).
- Produces (Task 5 relies on):
  - `pub struct DcPrincipal { pub name: String }` (Serialize/JsonSchema)
  - `pub async fn user_list(&self) -> Result<Vec<DcPrincipal>, DcError>`
  - `pub async fn user_create(&self, name: &str, password: &str, given_name: Option<&str>, surname: Option<&str>) -> Result<(), DcError>`
  - `pub async fn user_delete(&self, name: &str) -> Result<(), DcError>`
  - `pub async fn user_set_password(&self, name: &str, password: &str) -> Result<(), DcError>`
  - `pub async fn user_enable(&self, name: &str)` / `user_disable` — `Result<(), DcError>`
  - `pub async fn group_list(&self) -> Result<Vec<DcPrincipal>, DcError>`
  - `pub async fn group_create(&self, name: &str)` / `group_delete` — `Result<(), DcError>`
  - `pub async fn group_add_member(&self, group: &str, member: &str)` / `group_remove_member` — `Result<(), DcError>`
  - `pub async fn computer_list(&self) -> Result<Vec<DcPrincipal>, DcError>`

- [ ] **Step 1: Write failing tests (parser + argv hygiene extension)**

```rust
    #[test]
    fn parse_principal_lines_skips_noise() {
        let raw = "Administrator\nGuest\n\nalice\n  bob  \n";
        let out = parse_principal_lines(raw);
        let names: Vec<&str> = out.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Administrator", "Guest", "alice", "bob"]);
    }

    #[test]
    fn management_argv_never_carries_secrets_and_pins_config() {
        let secret = "Sup3r.Secret!";
        for argv in [
            user_delete_args("alice"),
            user_enable_args("alice"),
            user_disable_args("alice"),
            group_add_args("staff"),
            group_delete_args("staff"),
            group_members_args("addmembers", "staff", "alice"),
            list_args("user"),
            list_args("group"),
            list_args("computer"),
        ] {
            assert!(!argv.iter().any(|a| a.contains(secret)), "argv: {argv:?}");
            assert!(argv.windows(2).any(|w| w[0] == "--configfile" && w[1] == DC_CONF_PATH));
        }
    }
```

Run: `cargo test -p nasty-system dc::tests::parse_principal dc::tests::management_argv` — Expected: FAIL.

- [ ] **Step 2: Implement**

Argv builders + parser (module level):

```rust
fn parse_principal_lines(raw: &str) -> Vec<DcPrincipal> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| DcPrincipal { name: l.to_string() })
        .collect()
}

pub(crate) fn list_args(kind: &str) -> Vec<String> {
    with_conf(vec![kind.into(), "list".into()])
}
pub(crate) fn user_delete_args(name: &str) -> Vec<String> {
    with_conf(vec!["user".into(), "delete".into(), name.into()])
}
pub(crate) fn user_enable_args(name: &str) -> Vec<String> {
    with_conf(vec!["user".into(), "enable".into(), name.into()])
}
pub(crate) fn user_disable_args(name: &str) -> Vec<String> {
    with_conf(vec!["user".into(), "disable".into(), name.into()])
}
pub(crate) fn group_add_args(name: &str) -> Vec<String> {
    with_conf(vec!["group".into(), "add".into(), name.into()])
}
pub(crate) fn group_delete_args(name: &str) -> Vec<String> {
    with_conf(vec!["group".into(), "delete".into(), name.into()])
}
pub(crate) fn group_members_args(op: &str, group: &str, member: &str) -> Vec<String> {
    with_conf(vec!["group".into(), op.into(), group.into(), member.into()])
}
```

The type + methods:

```rust
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DcPrincipal {
    pub name: String,
}
```

```rust
    // ── Domain principal management (all samba-tool; passwords via stdin) ──

    async fn require_hosting(&self) -> Result<(), DcError> {
        if Self::load_config().await.is_none() {
            return Err(DcError::NotHosting);
        }
        Ok(())
    }

    pub async fn user_list(&self) -> Result<Vec<DcPrincipal>, DcError> {
        self.require_hosting().await?;
        Ok(parse_principal_lines(&samba_tool(&list_args("user")).await?))
    }

    pub async fn user_create(
        &self,
        name: &str,
        password: &str,
        given_name: Option<&str>,
        surname: Option<&str>,
    ) -> Result<(), DcError> {
        self.require_hosting().await?;
        // `samba-tool user create` prompts "New Password:" + "Retype
        // Password:" — both fed over stdin (never argv).
        samba_tool_stdin(
            &user_create_args(name, given_name, surname),
            &format!("{password}\n{password}"),
        )
        .await?;
        info!("dc: created domain user {name}");
        Ok(())
    }

    pub async fn user_delete(&self, name: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&user_delete_args(name)).await?;
        info!("dc: deleted domain user {name}");
        Ok(())
    }

    pub async fn user_set_password(&self, name: &str, password: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool_stdin(&setpassword_args(name), &format!("{password}\n{password}")).await?;
        info!("dc: reset password for domain user {name}");
        Ok(())
    }

    pub async fn user_enable(&self, name: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&user_enable_args(name)).await?;
        Ok(())
    }

    pub async fn user_disable(&self, name: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&user_disable_args(name)).await?;
        Ok(())
    }

    pub async fn group_list(&self) -> Result<Vec<DcPrincipal>, DcError> {
        self.require_hosting().await?;
        Ok(parse_principal_lines(&samba_tool(&list_args("group")).await?))
    }

    pub async fn group_create(&self, name: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&group_add_args(name)).await?;
        Ok(())
    }

    pub async fn group_delete(&self, name: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&group_delete_args(name)).await?;
        Ok(())
    }

    pub async fn group_add_member(&self, group: &str, member: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&group_members_args("addmembers", group, member)).await?;
        Ok(())
    }

    pub async fn group_remove_member(&self, group: &str, member: &str) -> Result<(), DcError> {
        self.require_hosting().await?;
        samba_tool(&group_members_args("removemembers", group, member)).await?;
        Ok(())
    }

    pub async fn computer_list(&self) -> Result<Vec<DcPrincipal>, DcError> {
        self.require_hosting().await?;
        Ok(parse_principal_lines(&samba_tool(&list_args("computer")).await?))
    }
```

- [ ] **Step 3: Run tests + full verification, commit**

`cargo test -p nasty-system dc::tests` → PASS; then the full three-step verification.

```bash
cd engine && cargo fmt
git add nasty-system/src/dc.rs
git commit -m "dc: domain user/group/computer management via samba-tool (#20)"
```

---

## Task 5: Engine wiring — AppState, router, registry, carve-outs, boot restore

**Files:**
- Modify: `engine/nasty-engine/src/main.rs`
- Create: `engine/nasty-engine/src/router/dc.rs`
- Modify: `engine/nasty-engine/src/router/mod.rs` (chain router; extend `is_read_only` carve-out + test)
- Modify: `engine/nasty-engine/src/registry/methods.rs`

**Interfaces:**
- Consumes: `DcService` + every Task 3/4 method; `FirewallService::{open_dc, close_dc}` (Task 1); router helpers (`ok`, `err`, `invalid`, `parse_params`, `require_str`).
- Produces: RPC methods `dc.status`, `dc.provision`, `dc.demote`, `dc.backup`, `dc.user.{list,create,delete,set_password,enable,disable}`, `dc.group.{list,create,delete,add_member,remove_member}`, `dc.computer.list`.

- [ ] **Step 1: Failing test — the read-only carve-out**

In `router/mod.rs`'s existing tests (next to the `domain.user.list` assertions at ~line 1381):

```rust
        assert!(!is_read_only("dc.user.list"));
        assert!(!is_read_only("dc.group.list"));
        assert!(!is_read_only("dc.computer.list"));
        assert!(is_read_only("dc.status")); // status is a safe read
```

Run: `cargo test -p nasty-engine read_only` — Expected: FAIL (the three `.list` methods currently match the suffix heuristic).

- [ ] **Step 2: Extend the carve-out**

In `is_read_only`, extend the existing `matches!`:

```rust
    if matches!(
        method,
        "domain.user.list"
            | "domain.group.list"
            // Same trap for DC mode: these enumerate the hosted directory
            // and are Admin-gated in the registry — the `.list` suffix must
            // not slip them into the ReadOnly set.
            | "dc.user.list"
            | "dc.group.list"
            | "dc.computer.list"
    ) {
        return false;
    }
```

Run the test again → PASS.

- [ ] **Step 3: AppState + boot restore**

In `main.rs`: add the field and constructor entry next to `domain` (lines ~82/~226):

```rust
    pub dc: nasty_system::dc::DcService,
```
```rust
            dc: nasty_system::dc::DcService::new(),
```

Register the phase name next to `"domain.restore"` (~line 248): add `"dc.restore",`.

Add the phase right after the existing `domain.restore` block (~line 373), same shape:

```rust
    // If this box hosts an AD domain, bring the DC back up: rewrite the
    // /run resolved drop-in (tmpfs — empty after reboot), start samba-dc
    // (Conflicts= swaps member-mode samba out), and open the DC firewall
    // ports. Must run after the smb.nasty.conf reconcile above — the DC
    // config includes it.
    .run_phase("dc.restore", secs(30), {
        let state = state.clone();
        async move {
            if state.dc.ensure_running().await {
                state.firewall.open_dc().await;
            }
        }
    })
```

(Match the surrounding `run_phase` closure style exactly — clone pattern, `secs(...)` helper.)

- [ ] **Step 4: Router arms**

Create `engine/nasty-engine/src/router/dc.rs`:

```rust
//! RPC arms in the `dc.*` namespace (Active Directory Domain Controller
//! role — this box HOSTS the domain). Member mode lives in domain.rs.

use nasty_common::{Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::Session;

#[derive(Deserialize)]
struct UserCreateParams {
    name: String,
    password: String,
    #[serde(default)]
    given_name: Option<String>,
    #[serde(default)]
    surname: Option<String>,
}

#[derive(Deserialize)]
struct SetPasswordParams {
    name: String,
    password: String,
}

#[derive(Deserialize)]
struct GroupMemberParams {
    group: String,
    member: String,
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    _session: &Session,
) -> Option<Response> {
    Some(match req.method.as_str() {
        "dc.status" => ok(req, state.dc.status().await),
        "dc.provision" => match parse_params::<nasty_system::dc::ProvisionRequest>(req) {
            Ok(p) => match state.dc.provision(p).await {
                Ok((status, warnings)) => {
                    // DC ports open only after a successful provision.
                    state.firewall.open_dc().await;
                    ok(req, serde_json::json!({ "status": status, "warnings": warnings }))
                }
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.demote" => match parse_params::<nasty_system::dc::DemoteRequest>(req) {
            Ok(p) => match state.dc.demote(p).await {
                Ok(()) => {
                    state.firewall.close_dc().await;
                    ok(req, "ok")
                }
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.backup" => match require_str(req, "dest") {
            Ok(dest) => match state.dc.backup(dest).await {
                Ok(path) => ok(req, serde_json::json!({ "path": path })),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.user.list" => match state.dc.user_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "dc.user.create" => match parse_params::<UserCreateParams>(req) {
            Ok(p) => match state
                .dc
                .user_create(&p.name, &p.password, p.given_name.as_deref(), p.surname.as_deref())
                .await
            {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.user.delete" => match require_str(req, "name") {
            Ok(name) => match state.dc.user_delete(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.user.set_password" => match parse_params::<SetPasswordParams>(req) {
            Ok(p) => match state.dc.user_set_password(&p.name, &p.password).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.user.enable" => match require_str(req, "name") {
            Ok(name) => match state.dc.user_enable(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.user.disable" => match require_str(req, "name") {
            Ok(name) => match state.dc.user_disable(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.group.list" => match state.dc.group_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "dc.group.create" => match require_str(req, "name") {
            Ok(name) => match state.dc.group_create(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.group.delete" => match require_str(req, "name") {
            Ok(name) => match state.dc.group_delete(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.group.add_member" => match parse_params::<GroupMemberParams>(req) {
            Ok(p) => match state.dc.group_add_member(&p.group, &p.member).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.group.remove_member" => match parse_params::<GroupMemberParams>(req) {
            Ok(p) => match state.dc.group_remove_member(&p.group, &p.member).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.computer.list" => match state.dc.computer_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        _ => return None,
    })
}
```

Register the module and chain it in `router/mod.rs`: add `mod dc;` next to `mod domain;`, and chain `dc::try_route(...)` immediately after the `domain::try_route(...)` call site (grep `domain::try_route` for the exact chaining style — an `if let Some(r) = ... { return r; }` or `.or()` chain; mirror it).

- [ ] **Step 5: Registry entries**

In `registry/methods.rs`, after the `domain.*` group, add the `dc.*` methods. `dc.status` → `role: MethodRole::Any`, `result: Some(gen_schema::<nasty_system::dc::DcStatus>(generator))`, `params: MethodParams::None`. All others → `role: MethodRole::Admin`:

- `dc.provision`: `MethodParams::Schema(gen_schema::<nasty_system::dc::ProvisionRequest>(generator))`; desc: "Provision a brand-new Active Directory domain on this box (Samba AD DC, internal DNS). Exactly one DC per domain; refuses when the box is domain-joined. The Administrator password is set over stdin and never logged. Returns { status, warnings }."
- `dc.demote`: `MethodParams::Schema(gen_schema::<nasty_system::dc::DemoteRequest>(generator))`; desc: "Demote the DC — DESTROYS the hosted domain (typed-realm confirmation required). Takes a final domain backup into /fs when a filesystem exists, then tears the role down."
- `dc.backup`: `MethodParams::AdHoc(ad_hoc_one("dest", "Backup target directory; must resolve under /fs and be empty or absent."))`; desc: "Run `samba-tool domain backup offline` into a /fs-jailed directory and return the tarball path. Ship it offsite with a backup profile."
- `dc.user.list` / `dc.group.list` / `dc.computer.list`: `MethodParams::None`, result `Some(gen_schema::<Vec<nasty_system::dc::DcPrincipal>>(generator))`; descs: "List domain users / groups / joined computers (Admin — enumerates the hosted directory)."
- `dc.user.create`: AdHoc schema `{ name (req), password (req), given_name?, surname? }`; desc mentions password-over-stdin.
- `dc.user.delete` / `dc.user.enable` / `dc.user.disable` / `dc.group.create` / `dc.group.delete`: `ad_hoc_one("name", ...)`.
- `dc.user.set_password`: AdHoc `{ name (req), password (req) }`.
- `dc.group.add_member` / `dc.group.remove_member`: AdHoc `{ group (req), member (req) }`.

Follow the exact `Method { name, desc, role, params, result }` shape of the neighboring `domain.*` entries; for AdHoc multi-field schemas copy the `backup.restore` entry's `serde_json::json!({...})` style.

- [ ] **Step 6: Build + tests + verification, commit**

Run (from `engine/`):
```
cargo build -p nasty-engine
cargo test -p nasty-engine
cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test
```
Expected: registry no-collision + builds-without-panic pass; the new carve-out assertions pass; `operator_role_methods_are_operator_allowed` passes untouched (every new mutating method is Admin, correctly absent from `is_operator_allowed`).

```bash
cd engine && cargo fmt
git add nasty-engine/src/main.rs nasty-engine/src/router/dc.rs nasty-engine/src/router/mod.rs nasty-engine/src/registry/methods.rs
git commit -m "dc: wire dc.* RPCs, boot restore phase, read-only carve-outs (#20)"
```

---

## Task 6: nasty.nix — DC-capable samba + samba-dc.service

**Files:**
- Modify: `nixos/modules/nasty.nix`

**Interfaces:**
- Consumes: the existing `sambaAds` binding (line ~59) and the samba service wiring.
- Produces: a DC-capable samba on every SMB-enabled box; a `samba-dc.service` the engine starts/stops.

No Rust tests — the gate is the Integration workflow's VM tests (Task 8 adds `ad-dc`; the existing `appliance-smoke`/`ad-member` catch member-mode regressions from the samba build change). Locally verify nix parses: `nix --extra-experimental-features 'nix-command flakes' flake check --no-build 2>&1 | head` is acceptable if fast; otherwise rely on CI (per project convention: only the Integration workflow proves nix changes).

- [ ] **Step 1: Upgrade the samba build**

Replace the `sambaAds` binding (keep the name — every existing reference keeps working):

```nix
  # ADS member mode needs LDAP; the DC role (#20) needs the full domain-
  # controller build. One superset build serves both roles — member boxes
  # simply never run the DC bits; two parallel samba store paths would
  # invite version skew. samba-tool's provision path imports python
  # `cryptography` (samba.gkdi) — keep it on pythonPath (harmless when the
  # pinned nixpkgs already carries it; required when it doesn't).
  sambaAds =
    (pkgs.samba.override {
      enableLDAP = true;
      enableDomainController = true;
    }).overrideAttrs
      (old: {
        pythonPath = (old.pythonPath or [ ]) ++ [ pkgs.python3Packages.cryptography ];
      });
```

- [ ] **Step 2: Add the samba-dc unit**

Next to the samba service configuration (`services.samba = mkIf cfg.smb.enable ...`, ~line 1742), add:

```nix
    # ── AD Domain Controller role (#20) ──────────────────────────
    # Present on every SMB-enabled box, disabled by default; the ENGINE
    # starts it (dc.provision / the dc.restore boot phase) — per-box
    # runtime opt-in, like every other role. Conflicts= makes systemd swap
    # the member-mode daemons out atomically: starting samba-dc stops
    # smbd/nmbd/winbindd, stopping it lets them return under their normal
    # toggles. The AD DC `samba` daemon runs its own smbd internally
    # (serving sysvol + the NASty shares via the include chain in
    # /etc/samba/smb.dc.conf, which `dc.provision` generates — never the
    # nix-managed smb.conf).
    systemd.services.samba-dc = mkIf cfg.smb.enable {
      description = "Samba Active Directory Domain Controller (NASty-managed)";
      # Lesson from the ad-member CI DC: without network-online ordering
      # the DC races the interface and provision-time DNS registration
      # fails in ways that look like samba bugs.
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      conflicts = [ "samba-smbd.service" "samba-nmbd.service" "samba-winbindd.service" ];
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "all";
        ExecStart = "${sambaAds}/bin/samba --foreground --no-process-group --configfile=/etc/samba/smb.dc.conf";
        LimitNOFILE = 16384;
        Restart = "on-failure";
        RestartSec = "5s";
      };
      # NOT wantedBy multi-user.target: the engine owns the role.
    };
```

(If the module's option style requires `systemd.services.samba-dc = mkIf ...` to live inside the module's existing `config = mkMerge [...]`/attrset structure, place it beside the other `systemd.services.*` entries the module already defines — match whatever wrapping the neighbors use.)

- [ ] **Step 3: Sanity + commit**

`git diff` review: only the two blocks above changed; `sambaAds` name untouched elsewhere (engine PATH at ~1625, `services.samba.package`, `system.nssModules` all keep working).

```bash
git add nixos/modules/nasty.nix
git commit -m "nixos: DC-capable samba build + engine-managed samba-dc unit (#20)"
```

---

## Task 7: WebUI — three-state Directory card + DcPanel

**Files:**
- Modify: `webui/src/lib/types.ts`
- Create: `webui/src/lib/dc.svelte.ts`
- Create: `webui/src/lib/directory/DcPanel.svelte`
- Modify: `webui/src/routes/settings/+page.svelte`
- Modify: `webui/src/routes/firewall/+page.svelte` (range-aware port chip)

**Interfaces:**
- Consumes: `dc.*` RPCs (Task 5 shapes: `dc.provision → { status, warnings }`, `dc.status → DcStatus`, lists → `DcPrincipal[]`, `dc.backup → { path }`); existing `domain.svelte.ts` (the pattern to mirror) and the Settings Directory card.
- Produces: the three-state Directory UI.

- [ ] **Step 1: Types**

In `webui/src/lib/types.ts`, near `DomainStatus`:

```ts
/** Returned by `dc.status` — Active Directory Domain Controller role. */
export interface DcStatus {
	hosting: boolean;
	realm?: string | null;
	workgroup?: string | null;
	dns_forwarder?: string | null;
	service_healthy: boolean;
}

export interface DcPrincipal {
	name: string;
}
```

And in the `PortSpec`-mirroring interface used by the Firewall page (find the firewall rule/port type in `types.ts`), add:

```ts
	/** End of a contiguous port range; absent = single port. */
	to?: number | null;
```

- [ ] **Step 2: State module**

Create `webui/src/lib/dc.svelte.ts`, mirroring `domain.svelte.ts`'s conventions (same `$state` object + exported async handlers, `withToast`, password cleared after use):

```ts
/** AD Domain Controller role state + handlers (Settings → Directory card). */
import { getClient } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import type { DcStatus, DcPrincipal } from '$lib/types';

const client = getClient();

export const dc = $state({
	status: null as DcStatus | null,
	loading: true,
	provisioning: false,
	// provision form
	realm: '',
	adminPassword: '',
	dnsForwarder: '',
	// panel data
	users: [] as DcPrincipal[],
	groups: [] as DcPrincipal[],
	computers: [] as DcPrincipal[],
});

export async function dcRefresh() {
	try {
		dc.status = await client.call<DcStatus>('dc.status');
	} catch { /* engine without dc support */ }
	dc.loading = false;
}

export async function dcProvision(): Promise<boolean> {
	if (!dc.realm || !dc.adminPassword) return false;
	dc.provisioning = true;
	const params: Record<string, unknown> = {
		realm: dc.realm.trim(),
		admin_password: dc.adminPassword,
	};
	if (dc.dnsForwarder.trim()) params.dns_forwarder = dc.dnsForwarder.trim();
	const res = await withToast(
		() => client.call<{ status: DcStatus; warnings: string[] }>('dc.provision', params),
		'Domain provisioned',
	);
	dc.adminPassword = '';
	dc.provisioning = false;
	if (!res) return false;
	dc.status = res.status;
	dc.realm = '';
	dc.dnsForwarder = '';
	return res.warnings?.length ? (res.warnings.forEach(w => notifyWarning(w)), true) : true;
}

export async function dcDemote(realmConfirmation: string): Promise<boolean> {
	const ok = await withToast(
		() => client.call('dc.demote', { realm_confirmation: realmConfirmation }),
		'Domain demoted',
	);
	if (ok !== undefined) await dcRefresh();
	return ok !== undefined;
}

export async function dcLoadPrincipals() {
	try {
		[dc.users, dc.groups, dc.computers] = await Promise.all([
			client.call<DcPrincipal[]>('dc.user.list'),
			client.call<DcPrincipal[]>('dc.group.list'),
			client.call<DcPrincipal[]>('dc.computer.list'),
		]);
	} catch { /* surfaced by panel */ }
}
```

`notifyWarning`: use the toast module's non-error notice API — the Firewall page's custom-rule warnings (from #637) established the convention (`info(...)` in `toast.svelte.ts`); import and use exactly that. If the helper is named differently, match the real name and note it.

- [ ] **Step 3: DcPanel component**

Create `webui/src/lib/directory/DcPanel.svelte` — the hosting-state dashboard rendered inside the Directory card. Structure (match the Settings page's section/input/button classes — read neighboring markup and reuse it):

- Header row: realm + workgroup, a green/amber dot for `service_healthy` ("Running" / "Not running — check `journalctl -u samba-dc`").
- Three tabs (simple `$state` tab switch, same pattern as other in-page tabs — if none exists on the Settings page, use a small button-group): **Users**, **Groups**, **Computers**.
  - Users tab: list rows (name + Enable/Disable + Reset password + Delete buttons); an add form (name, password, optional given/surname) calling `dc.user.create` then `dcLoadPrincipals()`. Reset password opens a small inline form (password field) → `dc.user.set_password`.
  - Groups tab: list rows (name + Delete); add form (name); a membership row (group + member inputs with Add/Remove buttons → `dc.group.add_member`/`remove_member`).
  - Computers tab: read-only name list.
- **Back up domain**: destination input (placeholder `/fs/tank/dc-backups/2026-07-10`) → `dc.backup { dest }`; on success toast the returned `path`.
- **Danger zone**: Demote — expandable section with red framing, explanatory copy ("Destroys the domain: every user, group, and joined machine's trust. A final backup is written to /fs first when a filesystem exists."), a text input for the realm, and a Demote button `disabled={typed !== dc.status?.realm}` calling `dcDemote(typed)`.
- All mutations `await` then `dcLoadPrincipals()`; every call routes through `withToast`.

- [ ] **Step 4: Three-state Directory card**

In `webui/src/routes/settings/+page.svelte`'s Directory section (~line 940):

- Import `dc`, `dcRefresh`, `dcProvision` from `$lib/dc.svelte` and `DcPanel` from `$lib/directory/DcPanel.svelte`; call `dcRefresh()` alongside the existing `domainRefresh()` in `onMount`.
- Restructure the card body:

```svelte
{#if domain.loading || dc.loading}
	<p class="text-sm text-muted-foreground">Loading...</p>
{:else if dc.status?.hosting}
	<DcPanel />
{:else if domain.status?.joined}
	<!-- existing joined-state markup, unchanged -->
{:else}
	<!-- standalone: two paths -->
	<!-- (1) existing join form, unchanged, under a subheading "Join an existing domain" -->
	<!-- (2) new subheading "Host a new domain" with copy:
	     "This NASty becomes the Active Directory domain controller —
	      clients and other NASty boxes join the domain it hosts. One DC
	      per domain; back it up from this panel. Clients should use this
	      box as their DNS server. Advanced administration (OUs, GPOs)
	      works with Windows RSAT."
	     Fields: Realm (placeholder ad.example.lan), Administrator password,
	     optional DNS forwarder (placeholder "auto — current upstream").
	     Provision button (disabled while dc.provisioning) → dcProvision(). -->
{/if}
```

The join form and the host form are mutually exclusive paths out of standalone — after either succeeds the card re-renders into the corresponding state.

- [ ] **Step 5: Firewall page range chip**

In `webui/src/routes/firewall/+page.svelte`, the service-rule port chips render `` `${p.port}/${p.transport}` `` — make it range-aware:

```ts
${p.to != null ? `${p.port}-${p.to}` : p.port}/${p.transport}
```

(Adjust to the actual expression at that site — it's inside a `new Set(rule.ports.map(...))`.)

- [ ] **Step 6: Check, test, commit**

Run (from `webui/`): `npm run check && npm test`
Expected: 0 errors/warnings; suite passes.

```bash
git add webui/src/lib/types.ts webui/src/lib/dc.svelte.ts webui/src/lib/directory/DcPanel.svelte webui/src/routes/settings/+page.svelte webui/src/routes/firewall/+page.svelte
git commit -m "webui: three-state Directory card + DC panel (#20)"
```

---

## Task 8: VM test — NASty DC + NASty member (the fleet money-shot)

**Files:**
- Create: `nixos/tests/ad-dc.nix`
- Modify: `flake.nix` (register the check next to `ad-member`, ~line 426)
- Modify: `.github/workflows/integration.yml` (add `.#checks.x86_64-linux.ad-dc \` next to the `ad-member` line, ~line 93)

**Interfaces:**
- Consumes: the full stack from Tasks 1–7; `nixos/tests/ad-member.nix` as the harness template (module imports, `_module.args`, the JSON-RPC websocket driver script pattern, login/bearer flow).

**Test topology & flow (write the test to this contract):**

- **Node `dcbox`** — full NASty appliance (same imports/`_module.args` as ad-member's `nasty` node). Test driver script (same websocket JSON-RPC pattern, own file like ad-member's):
  1. login → bearer;
  2. `dc.provision { realm: "NASTYDC.LAN", admin_password: "Passw0rd.123" }` → expect `status.hosting == true` (a static-IP *warning* is expected — the VM's network is externally managed; assert warnings array may be non-empty but provision succeeded);
  3. poll `dc.status` until `service_healthy == true` (timeout ~60s);
  4. `dc.user.create { name: "alice", password: "UserPass.123" }`;
  5. `dc.user.list` → contains `alice`;
  6. `dc.backup { dest: "/fs/<fs>/dc-backup" }` — only if the test harness sets up a filesystem (ad-member's does not; if no `/fs` in this harness, SKIP the backup step and leave a comment saying the jail is unit-tested and backup needs a pool — do not fabricate a pool just for this).
- **Node `member`** — second full NASty appliance with `networking.nameservers` pointed at `dcbox`'s test IP (the DC owns DNS for the realm). Driver:
  1. login → bearer;
  2. `domain.join { realm: "NASTYDC.LAN", username: "Administrator", password: "Passw0rd.123" }` — the SHIPPED member flow, unchanged;
  3. `domain.status` → joined + trust ok;
  4. `wbinfo -i 'NASTYDC\alice'` resolves (uid in the idmap range);
  5. from `dcbox`: `smbclient //member/<share-or-ipc> -U 'NASTYDC\alice%UserPass.123' -c 'exit'` succeeds (out-of-band auth exactly like ad-member's phase-2 check — crib it; IPC$ suffices if the harness creates no share).
- Test driver ordering: `dcbox.wait_for_unit("nasty-engine")` → provision phase → `member.wait_for_unit(...)` → join phase → assertions. Reuse ad-member's timeout/retry idioms.

- [ ] **Step 1: Write `nixos/tests/ad-dc.nix`** following the contract above, cribbing structure (nodes, driver script file, python test script) from `ad-member.nix`. The DC node does NOT use ad-member's throwaway `samba-dc` script-unit — it exercises the real `nasty.nix` `samba-dc.service` + engine provisioning path end to end.

- [ ] **Step 2: Register the check**

In `flake.nix` next to `ad-member` (~line 426):

```nix
      ad-dc = import ./nixos/tests/ad-dc.nix {
        inherit pkgs nasty-engine nasty-webui nasty-bcachefs-tools;
      };
```

(Copy the `ad-member` entry's exact argument set — pass whatever it passes.)

In `.github/workflows/integration.yml` (~line 93), add to the checks list:

```
            .#checks.x86_64-linux.ad-dc \
```

- [ ] **Step 3: Local syntax gate, commit, CI**

Local (macOS) can't run the VM test; gate what's gateable: `nix --extra-experimental-features 'nix-command flakes' flake show 2>&1 | grep ad-dc` (the check evaluates). The real validation is the Integration workflow on push — this task's changes touch `nixos/**` + `flake.nix`, so the path filter fires it.

```bash
git add nixos/tests/ad-dc.nix flake.nix .github/workflows/integration.yml
git commit -m "tests: ad-dc VM test — NASty DC provisioned via RPC, NASty member joins it (#20)"
```

Expected CI: `Engine` green (no Rust change in this task), `Integration` runs `ad-dc` + `ad-member` + the smokes. Iterate on CI failures the way the ad-member work did — the first live run WILL find something (Type=notify readiness, provision-vs-config-path behavior, getpass-over-stdin); each fix lands as its own commit on this branch. Two named fallbacks if the VM run disproves an assumption: (a) if `samba` never signals readiness, switch the unit to `Type=simple` + poll `samba-tool` reachability in `dc.status`; (b) if `samba-tool domain provision -s <path>` refuses to write the config at the `-s` path, provision with `--targetdir` into a scratch dir and move the generated smb.conf to `DC_CONF_PATH`.

---

## Self-Review

**Spec coverage** (against `docs/ad-domain-controller.md`):
- Provision (realm/password/forwarder, SAMBA_INTERNAL, engine-owned config, include chain, credential hygiene, resolved drop-in, Conflicts= switchover, firewall, boot restore) → Tasks 1, 2, 3, 5, 6.
- Single-DC + mutual exclusion both ways → Task 3 (provision preflight + `domain.join` check).
- Static-IP precondition, spec-amended softening → Tasks 2 (pure check + matrix test), 3 (wired), 8 (VM relies on the Warn path).
- Users/groups/computers surface → Task 4 (engine), 5 (RPC), 7 (UI).
- Backup jailed under /fs → Tasks 2 (jail + tests), 3 (op), 5 (RPC), 7 (UI); VM-test backup step conditional on a pool existing.
- Demote destroys + typed realm + parachute + unwind-shared teardown → Task 3, UI danger zone → Task 7.
- Roles (status Any, rest Admin) + `.list` carve-outs + guard-test posture → Task 5.
- One samba build + samba-dc unit + network-online lesson → Task 6.
- Fleet money-shot VM test + CI registration → Task 8.
- Errors: preflight named-fix messages (Tasks 2/3), stderr-verbatim (inherited via `run_cmd`), unhealthy status + journal pointer (Tasks 3/7).
- Out-of-scope items (multi-DC, GPO UI, BIND9, signed NTP, restore automation) → no task implements them; docs already state them.

**Placeholder scan:** No TBD/TODO/"fill in". Deliberate bounded flexibility: Task 5 Step 5 describes registry entries by exact name/role/params-shape rather than pasting 15 near-identical `Method{}` blocks (each field is specified); Task 7 Steps 3–4 specify the DcPanel/card by contract with exact RPC calls and state fields (matching how Task 6 of the firewall plan handled Svelte markup, where the file's real conventions must win); Task 8 defines the VM test as a step-by-step contract over the named template. Two implementation risks carry named fallbacks (Type=notify; provision config path) rather than silent assumptions.

**Type consistency:** `DcService`, `DcConfig`, `DcStatus`, `DcPrincipal`, `ProvisionRequest { realm, admin_password, dns_forwarder }`, `DemoteRequest { realm_confirmation }`, `provision → (DcStatus, Vec<String>)` ↔ router `{ status, warnings }` ↔ WebUI `{ status: DcStatus; warnings: string[] }`; `dc.backup { dest } → { path }` ↔ UI; `PortSpec.to: Option<u16>` ↔ TS `to?: number | null`; `open_dc`/`close_dc` used in router + boot phase; snake_case wire fields (`admin_password`, `realm_confirmation`, `given_name`) consistent across Rust serde and WebUI params. All checked.
