# Firewall Custom Port Rules Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator open a TCP/UDP port or contiguous range on the host firewall from the Firewall page — persisted, rendered into `table inet nasty` alongside the service rules, surviving reboots.

**Architecture:** A new persisted `CustomRule` type in `nasty-system`'s firewall module, materialized into the existing atomic nftables rebuild via its own render block; Admin-role `system.firewall.custom.{add,update,remove}` RPCs with server-side validation (refuse service-owned ports, warn on Docker-published overlap, sanitize all interpolated input); a Custom-rules section on the WebUI Firewall page.

**Tech Stack:** Rust (`nasty-system`, `nasty-engine`), nftables (`nft`), tokio, `uuid`, Svelte 5 (`webui`).

## Global Constraints

- A custom rule is **one port or one contiguous range** (`from ≤ to`, `1 ≤ from`, `to ≤ 65535`), single transport (TCP or UDP). No port sets / disjoint ranges in v1.
- **Refuse** a custom rule whose `[from,to]` interval intersects (same transport) any port a NASty service owns — checked against live `state.rules`, which already contains disabled protocols and iSCSI/NVMe-oF portal ports. Error names the owner.
- **Refuse** an exact-duplicate custom rule (same transport + from + to + source + iface); **allow** overlapping (non-identical) custom ranges.
- **Allow + warn** when a custom rule overlaps a Docker-published host port — computed at the router layer (which has `apps.list`); the firewall module stays Docker-agnostic.
- All interpolated fields are sanitized before reaching the nft ruleset: `source` must be a valid IP or CIDR, `iface` must match `^[A-Za-z0-9._@-]+$` (≤ 15 chars, Linux `IFNAMSIZ`), `label` must be non-empty, ≤ 64 chars, and free of control characters. The nft comment uses the opaque `id`, never the free-text label.
- Custom-rule methods are **Admin**-role (matching `system.firewall.restrict`); they are not added to `is_operator_allowed`.
- A custom rule pinned to a since-removed interface needs no special handling: the renderer emits `iifname "<iface>"` (string match), which matches nothing when the interface is absent and does not break the `nft -f` load — naturally fail-closed.
- Lock acquisition order in `FirewallService` is fixed **state → restrictions → custom** everywhere, to avoid deadlock.
- Verification before every Rust commit (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`. For WebUI: `npm run check` and `npm test` (from `webui/`).
- `webui/src/lib/types.ts` is hand-maintained ("Mirrors engine Rust types") — update by hand.

---

## File Structure

- `engine/nasty-system/src/firewall.rs` — **modify**: add `CustomRule` + `CustomRuleInput`, persistence (`firewall-custom.json`), a `custom` mutex on `FirewallService`, extract a pure `render_ruleset`, add the custom render block, thread `custom` through `apply_nftables` and every caller, add validation helpers and the three CRUD methods, extend `FirewallStatus`.
- `engine/nasty-engine/src/router/system.rs` — **modify**: `system.firewall.custom.{add,update,remove}` arms + the Docker-overlap warning helper.
- `engine/nasty-engine/src/registry/methods.rs` — **modify**: register the three methods (Admin).
- `webui/src/lib/types.ts` — **modify**: `CustomRule` interface + `FirewallStatus.custom_rules`.
- `webui/src/routes/firewall/+page.svelte` — **modify**: Custom port rules section (list, add form, toggle, delete, warnings, guidance copy).

---

## Task 1: CustomRule type, persistence, and nft rendering

**Files:**
- Modify: `engine/nasty-system/src/firewall.rs`
- Test: `engine/nasty-system/src/firewall.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub struct CustomRule { pub id: String, pub label: String, pub transport: Transport, pub from: u16, pub to: u16, pub source: Option<String>, pub iface: Option<String>, pub enabled: bool }` (derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema`).
  - `pub fn render_ruleset(state: &FirewallState, custom: &[CustomRule]) -> String` — the pure nft-ruleset string (extracted from `apply_nftables`), including the custom block.
  - `apply_nftables(state: &FirewallState, custom: &[CustomRule])` — signature gains `custom`.
  - `FirewallStatus.custom_rules: Vec<CustomRule>`.
  - `FirewallService.custom: tokio::sync::Mutex<Vec<CustomRule>>`.

- [ ] **Step 1: Write failing tests for rendering**

Add to the `tests` module in `engine/nasty-system/src/firewall.rs`:

```rust
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
    assert!(out.contains("tcp dport 32400 accept # custom:id1"), "got:\n{out}");
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
        out.contains("iifname \"eth0\" ip saddr 10.0.0.0/8 udp dport 8000-8010 accept # custom:id2"),
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
    assert!(!out.contains("9999"), "disabled rule must not render; got:\n{out}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run (from `engine/`): `cargo test -p nasty-system firewall::tests::render`
Expected: FAIL — `CustomRule` and `render_ruleset` don't exist.

- [ ] **Step 3: Add the `CustomRule` type and persistence**

After the `FirewallRule` struct, add:

```rust
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
```

- [ ] **Step 4: Add the `custom` mutex to `FirewallService` and load it in `init`**

In `struct FirewallService`, add the field:

```rust
pub struct FirewallService {
    state: tokio::sync::Mutex<FirewallState>,
    restrictions: tokio::sync::Mutex<FirewallRestrictions>,
    custom: tokio::sync::Mutex<Vec<CustomRule>>,
}
```

In `FirewallService::new()`, initialize it:

```rust
        Self {
            state: tokio::sync::Mutex::new(FirewallState::default()),
            restrictions: tokio::sync::Mutex::new(FirewallRestrictions::default()),
            custom: tokio::sync::Mutex::new(Vec::new()),
        }
```

In `init()`, after `*restrictions = FirewallRestrictions::load();`, load custom rules (respecting lock order state → restrictions → custom):

```rust
        let mut custom = self.custom.lock().await;
        *custom = load_custom_rules();
```

- [ ] **Step 5: Extract `render_ruleset` and add the custom block; thread `custom` through `apply_nftables`**

Replace the body of `apply_nftables` so the string-building becomes a pure function. Change the signature and split:

```rust
/// Generate and apply the full nftables ruleset atomically.
async fn apply_nftables(state: &FirewallState, custom: &[CustomRule]) -> Result<(), String> {
    let rules = render_ruleset(state, custom);

    // Apply atomically: flush + load  (unchanged below)
    let flush = Command::new("nft")
        .args(["delete", "table", "inet", "nasty"])
        .output()
        .await;
    if let Ok(o) = &flush
        && !o.status.success()
    {
        let stderr = String::from_utf8_lossy(&o.stderr);
        if !stderr.contains("No such file") && !stderr.contains("does not exist") {
            warn!("nft delete table warning: {stderr}");
        }
    }

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
```

- [ ] **Step 6: Update every `apply_nftables` caller to pass the custom slice**

Each existing caller currently locks `state` (and some `restrictions`). Add a `custom` lock (acquired last) and pass `&custom`. The seven call sites and how to fix each:

- `init` (near line 262): it already holds `state` and `restrictions` and (from Step 4) `custom`. Change the call to `apply_nftables(&state, &custom).await`.
- `open` (near line 289), `close` (near 306), `open_rdma` (near 328), `close_rdma` (near 344): each holds only `state`. Before the `apply_nftables` call add `let custom = self.custom.lock().await;` and change the call to `apply_nftables(&state, &custom).await`.
- `set_restriction` (near line 408): holds `state` (and earlier `restrictions`, already dropped via its own block). Add `let custom = self.custom.lock().await;` before the call and change it to `apply_nftables(&state, &custom).await?`.
- `set_service_ports` (near line 430): holds `state` and `restrictions`. Add `let custom = self.custom.lock().await;` before the call and change it to `apply_nftables(&state, &custom).await`.

In every case acquire `custom` only after `state` (and `restrictions` where held) to preserve the state → restrictions → custom order.

- [ ] **Step 7: Extend `FirewallStatus` and `status()`**

Add the field to `FirewallStatus` (after `published_app_ports`):

```rust
    /// User-managed custom port rules (issue #620). Rendered into the
    /// firewall alongside service rules; editable on the Firewall page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_rules: Vec<CustomRule>,
```

In `status()`, lock `custom` (after `state` and `restrictions`) and fill it:

```rust
    pub async fn status(&self) -> FirewallStatus {
        let state = self.state.lock().await;
        let restrictions = self.restrictions.lock().await;
        let custom = self.custom.lock().await;
        FirewallStatus {
            active: true,
            rules: state.rules.clone(),
            restrictions: restrictions.services.clone(),
            interface_restrictions: restrictions.interfaces.clone(),
            published_app_ports: Vec::new(),
            custom_rules: custom.clone(),
        }
    }
```

- [ ] **Step 8: Run the render tests + full crate suite**

Run (from `engine/`):
```
cargo test -p nasty-system firewall::tests::render
cargo test -p nasty-system
```
Expected: the three new render tests pass; all existing `nasty-system` tests still pass (the `apply_nftables` signature change compiles across callers).

- [ ] **Step 9: Full verification + commit**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`
Expected: clean.

```bash
cd engine && cargo fmt
git add nasty-system/src/firewall.rs
git commit -m "firewall: CustomRule type, persistence, and nft rendering"
```

---

## Task 2: Validation + CRUD methods

**Files:**
- Modify: `engine/nasty-system/src/firewall.rs`
- Test: `engine/nasty-system/src/firewall.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `CustomRule`, `FirewallState`, `Transport`, `render_ruleset`/`apply_nftables` (Task 1).
- Produces:
  - `pub struct CustomRuleInput { pub label: String, pub transport: Transport, pub from: u16, pub to: u16, pub source: Option<String>, pub iface: Option<String>, pub enabled: bool }` (derives `Debug, Clone, Deserialize, JsonSchema`; `enabled` uses `#[serde(default = "default_true")]`).
  - `pub fn validate_custom_input(input: &CustomRuleInput) -> Result<(), String>` — range + label + source + iface sanitization (no state needed).
  - `pub fn service_port_conflict(state: &FirewallState, transport: Transport, from: u16, to: u16) -> Option<String>` — returns the owning service name if the interval collides.
  - `pub async fn add_custom_rule(&self, input: CustomRuleInput) -> Result<CustomRule, String>`
  - `pub async fn update_custom_rule(&self, id: &str, input: CustomRuleInput) -> Result<CustomRule, String>`
  - `pub async fn remove_custom_rule(&self, id: &str) -> Result<(), String>`

- [ ] **Step 1: Write failing tests for the pure validators**

Add to the `tests` module:

```rust
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
        ports: vec![PortSpec { port: 445, transport: Transport::Tcp, source: None, iface: None }],
        active: true,
    });
    state.rules.push(FirewallRule {
        service: "nfs".into(),
        ports: vec![PortSpec { port: 2049, transport: Transport::Tcp, source: None, iface: None }],
        active: false, // disabled service still owns its port
    });

    assert_eq!(service_port_conflict(&state, Transport::Tcp, 445, 445).as_deref(), Some("smb"));
    // range spanning the port
    assert_eq!(service_port_conflict(&state, Transport::Tcp, 440, 450).as_deref(), Some("smb"));
    // disabled service still conflicts
    assert_eq!(service_port_conflict(&state, Transport::Tcp, 2049, 2049).as_deref(), Some("nfs"));
    // transport mismatch → no conflict
    assert_eq!(service_port_conflict(&state, Transport::Udp, 445, 445), None);
    // free port → no conflict
    assert_eq!(service_port_conflict(&state, Transport::Tcp, 32400, 32400), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run (from `engine/`): `cargo test -p nasty-system firewall::tests::validate firewall::tests::service_conflict`
Expected: FAIL — `CustomRuleInput`, `validate_custom_input`, `service_port_conflict` don't exist.

- [ ] **Step 3: Add `CustomRuleInput`, `default_true`, and the pure validators**

```rust
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
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
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
```

Note: `Transport` needs `PartialEq` (it already derives `PartialEq, Eq`). `FirewallRule`/`PortSpec` fields are used directly in the test — they are already `pub`.

- [ ] **Step 4: Run the validator tests to verify they pass**

Run (from `engine/`): `cargo test -p nasty-system firewall::tests::validate firewall::tests::service_conflict`
Expected: PASS.

- [ ] **Step 5: Add the three CRUD methods to `FirewallService`**

Add inside `impl FirewallService` (e.g. after `set_service_ports`):

```rust
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
```

Add the two small helpers near the other free functions:

```rust
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
```

- [ ] **Step 6: Full verification + commit**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-system`
Expected: clean; all validator tests pass.

```bash
cd engine && cargo fmt
git add nasty-system/src/firewall.rs
git commit -m "firewall: custom-rule validation and add/update/remove"
```

---

## Task 3: Router arms + registry + Docker-overlap warning

**Files:**
- Modify: `engine/nasty-engine/src/router/system.rs`
- Modify: `engine/nasty-engine/src/registry/methods.rs`
- Test: `engine/nasty-engine/src/router/system.rs` (`#[cfg(test)] mod tests`) — for the pure warning helper.

**Interfaces:**
- Consumes: `FirewallService::{add_custom_rule, update_custom_rule, remove_custom_rule}` and `CustomRuleInput`/`CustomRule` (Task 2); `PublishedAppPort` (existing); router helpers `parse_params`, `ok`, `err`, `require_str`.
- Produces: `system.firewall.custom.{add,update,remove}` RPCs; `fn docker_overlap_warnings(rule: &CustomRule, published: &[PublishedAppPort]) -> Vec<String>`.

- [ ] **Step 1: Write a failing test for the warning helper**

Add to a `#[cfg(test)] mod tests` at the bottom of `engine/nasty-engine/src/router/system.rs` (create the module if absent):

```rust
#[cfg(test)]
mod tests {
    use super::docker_overlap_warnings;
    use nasty_system::firewall::{CustomRule, PublishedAppPort, Transport};

    fn rule(from: u16, to: u16, transport: Transport) -> CustomRule {
        CustomRule {
            id: "id".into(),
            label: "l".into(),
            transport,
            from,
            to,
            source: None,
            iface: None,
            enabled: true,
        }
    }

    fn pub_port(host: u16, transport: &str, app: &str) -> PublishedAppPort {
        PublishedAppPort {
            app: app.into(),
            host_port: host,
            container_port: host,
            transport: transport.into(),
        }
    }

    #[test]
    fn warns_on_tcp_overlap() {
        let w = docker_overlap_warnings(&rule(8080, 8080, Transport::Tcp), &[pub_port(8080, "tcp", "nginx")]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("nginx"), "got: {:?}", w);
    }

    #[test]
    fn no_warn_on_transport_or_port_mismatch() {
        assert!(docker_overlap_warnings(&rule(8080, 8080, Transport::Tcp), &[pub_port(8080, "udp", "x")]).is_empty());
        assert!(docker_overlap_warnings(&rule(8080, 8080, Transport::Tcp), &[pub_port(9090, "tcp", "x")]).is_empty());
    }

    #[test]
    fn warns_for_each_app_in_range() {
        let w = docker_overlap_warnings(
            &rule(8000, 8100, Transport::Tcp),
            &[pub_port(8080, "tcp", "a"), pub_port(8090, "tcp", "b")],
        );
        assert_eq!(w.len(), 2);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run (from `engine/`): `cargo test -p nasty-engine docker_overlap`
Expected: FAIL — `docker_overlap_warnings` doesn't exist.

- [ ] **Step 3: Add the warning helper**

Add near the other helpers in `engine/nasty-engine/src/router/system.rs` (module-level `pub(crate) fn` so the test can reach it):

```rust
/// Warnings (never errors) for a custom rule that overlaps a Docker-published
/// host port. Bridge apps publish past the firewall, so the rule is redundant
/// — surfaced so the operator doesn't think the port was closed.
pub(crate) fn docker_overlap_warnings(
    rule: &nasty_system::firewall::CustomRule,
    published: &[nasty_system::firewall::PublishedAppPort],
) -> Vec<String> {
    use nasty_system::firewall::Transport;
    let proto = match rule.transport {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    };
    published
        .iter()
        .filter(|p| p.transport == proto && p.host_port >= rule.from && p.host_port <= rule.to)
        .map(|p| {
            format!(
                "{}/{} is already reachable via app '{}' (bridge apps publish past the firewall)",
                proto, p.host_port, p.app
            )
        })
        .collect()
}
```

- [ ] **Step 4: Run to verify the helper tests pass**

Run (from `engine/`): `cargo test -p nasty-engine docker_overlap`
Expected: PASS.

- [ ] **Step 5: Add the router arms**

First add a params struct near the top of `system.rs` (next to other `Deserialize` param structs):

```rust
#[derive(serde::Deserialize)]
struct CustomRuleUpdateParams {
    id: String,
    #[serde(flatten)]
    input: nasty_system::firewall::CustomRuleInput,
}
```

Add a small helper to gather current published ports (mirrors the `system.firewall.status` arm) so `add`/`update` can compute warnings:

```rust
async fn published_ports(state: &AppState) -> Vec<nasty_system::firewall::PublishedAppPort> {
    match state.apps.list().await {
        Ok(apps) => apps
            .iter()
            .flat_map(|app| {
                app.ports.iter().map(move |p| nasty_system::firewall::PublishedAppPort {
                    app: app.name.clone(),
                    host_port: p.host_port,
                    container_port: p.container_port,
                    transport: p.protocol.clone(),
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
```

Add the three arms alongside `system.firewall.restrict`:

```rust
        "system.firewall.custom.add" => {
            match parse_params::<nasty_system::firewall::CustomRuleInput>(req) {
                Ok(input) => match state.firewall.add_custom_rule(input).await {
                    Ok(rule) => {
                        let warnings = docker_overlap_warnings(&rule, &published_ports(state).await);
                        ok(req, serde_json::json!({ "rule": rule, "warnings": warnings }))
                    }
                    Err(e) => err(req, e),
                },
                Err(e) => err(req, e),
            }
        }
        "system.firewall.custom.update" => match parse_params::<CustomRuleUpdateParams>(req) {
            Ok(p) => match state.firewall.update_custom_rule(&p.id, p.input).await {
                Ok(rule) => {
                    let warnings = docker_overlap_warnings(&rule, &published_ports(state).await);
                    ok(req, serde_json::json!({ "rule": rule, "warnings": warnings }))
                }
                Err(e) => err(req, e),
            },
            Err(e) => err(req, e),
        },
        "system.firewall.custom.remove" => match require_str(req, "id") {
            Ok(id) => match state.firewall.remove_custom_rule(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
```

Note: confirm `parse_params`, `ok`, `err`, `require_str` are in scope in `system.rs` (they are used by the existing `system.firewall.*` arms — reuse the same imports).

- [ ] **Step 6: Register the three methods (Admin)**

In `engine/nasty-engine/src/registry/methods.rs`, after the `system.firewall.restrict` entry, add:

```rust
                Method {
                    name: "system.firewall.custom.add",
                    desc: "Open a user-managed TCP/UDP port or contiguous range on the host firewall (issue #620). Rejects ports a NASty service owns; returns { rule, warnings } where warnings flag overlap with a Docker-published port (allowed but redundant).",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<nasty_system::firewall::CustomRuleInput>(generator)),
                    result: None,
                },
                Method {
                    name: "system.firewall.custom.update",
                    desc: "Update a custom firewall port rule by id (full replace of its fields, including the enable/disable toggle). Same validation and { rule, warnings } response as add.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "required": ["id", "label", "transport", "from", "to"],
                        "properties": {
                            "id": { "type": "string", "description": "Custom rule id." },
                            "label": { "type": "string", "description": "Required human label." },
                            "transport": { "type": "string", "enum": ["tcp", "udp"] },
                            "from": { "type": "integer", "description": "Low port (1–65535)." },
                            "to": { "type": "integer", "description": "High port; equals from for a single port." },
                            "source": { "type": "string", "description": "Optional source IP/CIDR." },
                            "iface": { "type": "string", "description": "Optional interface name." },
                            "enabled": { "type": "boolean", "description": "Whether the rule is rendered into nft." }
                        }
                    })),
                    result: None,
                },
                Method {
                    name: "system.firewall.custom.remove",
                    desc: "Remove a user-managed custom firewall port rule by id and rebuild the nftables ruleset.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Custom rule id.")),
                    result: None,
                },
```

Confirm `ad_hoc_one` and `gen_schema` are already used in this file (they are, by neighboring entries).

- [ ] **Step 7: Build + registry tests**

Run (from `engine/`):
```
cargo build -p nasty-engine
cargo test -p nasty-engine
```
Expected: builds; registry tests (translation/no-collision, builds-without-panic) pass. `system.firewall.custom.*` default to POST verb via the suffix translator (no `.get`/`.list` suffix) — no paths.rs change needed. The registry-role↔allowlist guard test (`operator_role_methods_are_operator_allowed`) is unaffected: these are Admin-role.

- [ ] **Step 8: Full verification + commit**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`
Expected: clean.

```bash
cd engine && cargo fmt
git add nasty-engine/src/router/system.rs nasty-engine/src/registry/methods.rs
git commit -m "firewall: wire system.firewall.custom.* RPCs with Docker-overlap warnings"
```

---

## Task 4: WebUI — custom port rules section

**Files:**
- Modify: `webui/src/lib/types.ts`
- Modify: `webui/src/routes/firewall/+page.svelte`

**Interfaces:**
- Consumes: `system.firewall.status` (now returns `custom_rules`); `system.firewall.custom.{add,update,remove}` (`{ rule, warnings }` for add/update); `networkState.interfaces` (existing iface source).
- Produces: the Custom port rules UI.

- [ ] **Step 1: Update the TypeScript types**

In `webui/src/lib/types.ts`, add the `CustomRule` interface near `FirewallStatus`:

```ts
export interface CustomRule {
	id: string;
	label: string;
	transport: 'tcp' | 'udp';
	from: number;
	to: number;
	source?: string | null;
	iface?: string | null;
	enabled: boolean;
}
```

Add the field to `FirewallStatus` (after `published_app_ports`):

```ts
	custom_rules?: CustomRule[];
```

- [ ] **Step 2: Type-check the WebUI (types compile before UI wiring)**

Run (from `webui/`): `npm run check`
Expected: no new type errors.

- [ ] **Step 3: Add the custom-rules script state + handlers**

In `webui/src/routes/firewall/+page.svelte`'s `<script>`, extend the import and add state + functions:

```ts
	import type { FirewallStatus, NetworkState, PublishedAppPort, CustomRule } from '$lib/types';
```

```ts
	// Custom port rules (#620)
	let showAddCustom = $state(false);
	let editCustomId: string | null = $state(null);
	let cLabel = $state('');
	let cTransport: 'tcp' | 'udp' = $state('tcp');
	let cFrom = $state('');
	let cTo = $state('');
	let cSource = $state('');
	let cIface = $state('');
	let cEnabled = $state(true);

	function resetCustomForm() {
		showAddCustom = false;
		editCustomId = null;
		cLabel = ''; cTransport = 'tcp'; cFrom = ''; cTo = '';
		cSource = ''; cIface = ''; cEnabled = true;
	}

	function startAddCustom() {
		resetCustomForm();
		showAddCustom = true;
	}

	function startEditCustom(r: CustomRule) {
		editCustomId = r.id;
		showAddCustom = true;
		cLabel = r.label;
		cTransport = r.transport;
		cFrom = String(r.from);
		cTo = r.to === r.from ? '' : String(r.to);
		cSource = r.source ?? '';
		cIface = r.iface ?? '';
		cEnabled = r.enabled;
	}

	async function saveCustom() {
		const from = parseInt(cFrom, 10);
		const to = cTo.trim() ? parseInt(cTo, 10) : from;
		const params: Record<string, unknown> = {
			label: cLabel.trim(),
			transport: cTransport,
			from,
			to,
			enabled: cEnabled,
		};
		if (cSource.trim()) params.source = cSource.trim();
		if (cIface.trim()) params.iface = cIface.trim();
		if (editCustomId) params.id = editCustomId;

		const method = editCustomId ? 'system.firewall.custom.update' : 'system.firewall.custom.add';
		const res = await withToast(
			() => client.call<{ rule: CustomRule; warnings: string[] }>(method, params),
			editCustomId ? 'Custom rule updated' : 'Custom rule added',
		);
		if (!res) return;
		for (const w of res.warnings ?? []) {
			await withToast(() => Promise.resolve(null), w);
		}
		resetCustomForm();
		await loadFirewall();
	}

	async function toggleCustom(r: CustomRule) {
		await withToast(
			() => client.call('system.firewall.custom.update', {
				id: r.id, label: r.label, transport: r.transport,
				from: r.from, to: r.to,
				...(r.source ? { source: r.source } : {}),
				...(r.iface ? { iface: r.iface } : {}),
				enabled: !r.enabled,
			}),
			r.enabled ? 'Rule disabled' : 'Rule enabled',
		);
		await loadFirewall();
	}

	async function removeCustom(r: CustomRule) {
		await withToast(
			() => client.call('system.firewall.custom.remove', { id: r.id }),
			'Custom rule removed',
		);
		await loadFirewall();
	}
```

Note: match the page's actual `withToast` usage for surfacing warnings — if the codebase has a dedicated non-error notice/toast API, use that instead of the `Promise.resolve` shim above; the requirement is only that each warning string is shown non-blocking. Confirm the exact `withToast` signature in this file before finalizing.

- [ ] **Step 4: Add the Custom port rules section markup**

After the existing service-rules `<section>` (and any published-app-ports section) in `webui/src/routes/firewall/+page.svelte`, add a section. Match the page's existing styling/classes (border/section pattern seen in the service-rules block):

```svelte
	<section class="mt-4 rounded-lg border border-border p-5">
		<div class="flex items-center justify-between">
			<div>
				<h2 class="text-sm font-semibold">Custom port rules</h2>
				<p class="text-xs text-muted-foreground mt-0.5">
					Open a port for something running directly on the host (e.g. a
					<code>network_mode: host</code> app). Bridge-networked apps don't need a rule —
					their published ports (listed above) already bypass the firewall.
				</p>
			</div>
			<Button size="xs" onclick={startAddCustom}>Add rule</Button>
		</div>

		{#if firewallStatus?.custom_rules?.length}
			<div class="mt-3 space-y-1">
				{#each firewallStatus.custom_rules as r}
					<div class="flex items-center gap-3 rounded px-3 py-2 text-sm hover:bg-muted/30 {r.enabled ? '' : 'opacity-40'}">
						<span class="h-2 w-2 rounded-full shrink-0 {r.enabled ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
						<span class="font-medium w-40 truncate">{r.label}</span>
						<span class="font-mono text-xs text-muted-foreground">
							{r.from === r.to ? r.from : `${r.from}-${r.to}`}/{r.transport}
						</span>
						{#if r.source}<span class="text-xs text-amber-400">{r.source}</span>{/if}
						{#if r.iface}<span class="text-xs text-blue-400">{r.iface}</span>{/if}
						<div class="ml-auto flex items-center gap-2">
							<Button size="xs" variant="secondary" onclick={() => toggleCustom(r)}>{r.enabled ? 'Disable' : 'Enable'}</Button>
							<Button size="xs" variant="secondary" onclick={() => startEditCustom(r)}>Edit</Button>
							<Button size="xs" variant="secondary" onclick={() => removeCustom(r)}>Delete</Button>
						</div>
					</div>
				{/each}
			</div>
		{:else}
			<p class="mt-3 text-xs text-muted-foreground">No custom rules.</p>
		{/if}

		{#if showAddCustom}
			<div class="mt-3 rounded-lg border border-border bg-secondary/20 p-3 space-y-3">
				<div class="text-xs font-medium">{editCustomId ? 'Edit rule' : 'New rule'}</div>
				<input class="w-full rounded border border-border bg-background px-2 py-1 text-sm" placeholder="Label (e.g. Plex host mode)" bind:value={cLabel} />
				<div class="flex gap-2">
					<select class="rounded border border-border bg-background px-2 py-1 text-sm" bind:value={cTransport}>
						<option value="tcp">TCP</option>
						<option value="udp">UDP</option>
					</select>
					<input class="w-24 rounded border border-border bg-background px-2 py-1 text-sm" placeholder="Port" bind:value={cFrom} />
					<input class="w-24 rounded border border-border bg-background px-2 py-1 text-sm" placeholder="to (opt)" bind:value={cTo} />
				</div>
				<input class="w-full rounded border border-border bg-background px-2 py-1 text-sm" placeholder="Source IP/CIDR (optional)" bind:value={cSource} />
				{#if networkState}
					<select class="w-full rounded border border-border bg-background px-2 py-1 text-sm" bind:value={cIface}>
						<option value="">Any interface</option>
						{#each networkState.interfaces as iface}
							<option value={iface.name}>{iface.name}</option>
						{/each}
					</select>
				{/if}
				<label class="flex items-center gap-2 text-sm">
					<input type="checkbox" bind:checked={cEnabled} /> Enabled
				</label>
				<div class="flex justify-end gap-2">
					<Button size="sm" variant="secondary" onclick={resetCustomForm}>Cancel</Button>
					<Button size="sm" onclick={saveCustom}>{editCustomId ? 'Save' : 'Add'}</Button>
				</div>
			</div>
		{/if}
	</section>
```

(Match the actual component imports/classes the page already uses — if `Button` variants/sizes differ, mirror the service-rules block. Use the same interface list source `networkState.interfaces` the restrict editor uses.)

- [ ] **Step 5: Type-check and test the WebUI**

Run (from `webui/`):
```
npm run check
npm test
```
Expected: no type errors; existing tests pass. (No new WebUI unit test is required — the Firewall page has no component-test harness for these sections; the flow is validated end-to-end against a real engine.)

- [ ] **Step 6: Commit**

```bash
git add webui/src/lib/types.ts webui/src/routes/firewall/+page.svelte
git commit -m "webui: custom port rules section on the Firewall page"
```

---

## Self-Review

**Spec coverage** (checked against `docs/firewall-custom-rules.md`):
- Single port or contiguous range, persisted, rendered into `table inet nasty` → Task 1.
- Source/interface restrictions → carried on `CustomRule`, rendered in Task 1, validated in Task 2.
- Enable/disable toggle + required label → `enabled`/`label` fields (Task 1), toggle via update (Tasks 2–4).
- Refuse service-owned ports (active + inactive + portal, transport-matched) → `service_port_conflict` (Task 2).
- Refuse exact duplicate, allow overlapping custom ranges → `same_rule` (Task 2).
- Allow + warn on Docker overlap, computed at router → `docker_overlap_warnings` (Task 3).
- Input sanitization (CIDR/IP, iface charset, control-char-free label; id in nft comment) → Task 2 validators + Task 1 render uses `id`.
- Admin role, no allowlist entry, guard test unaffected → Task 3.
- Interface disappearance naturally fail-closed via `iifname` → no task needed; the render test asserts `iifname "<iface>"` is emitted (Task 1 range test).
- Status exposes `custom_rules` → Task 1.
- WebUI section + guidance copy heading off the #591 redundant-rule trap → Task 4.
- Lock order state → restrictions → custom → Tasks 1–2.

**Placeholder scan:** No TBD/TODO. Two "confirm the exact signature/classes in this file" notes (Task 3 Step 5 helper imports; Task 4 `withToast`/`Button` conventions) are guardrails against drift from real code, each with the concrete thing to check — not deferred work.

**Type consistency:** `CustomRule`, `CustomRuleInput`, `render_ruleset`, `service_port_conflict`, `validate_custom_input`, `same_rule`, `transport_str`, `add_custom_rule`/`update_custom_rule`/`remove_custom_rule`, `docker_overlap_warnings` are named identically across defining and consuming tasks. `{ rule, warnings }` response shape matches between Task 3 (engine) and Task 4 (WebUI call sites). `CustomRule` fields match between Rust (Task 1) and TS (Task 4): `id, label, transport('tcp'|'udp'), from, to, source?, iface?, enabled`. The `enabled` default (`true`) is in `CustomRuleInput` (Rust) and defaulted by the WebUI form state.
