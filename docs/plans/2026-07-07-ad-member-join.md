# AD Member Join Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** NASty joins an existing Active Directory domain; domain users and groups become usable in SMB share permissions, resolved live through winbind.

**Architecture:** A new `domain` module in `nasty-system` owns join state, krb5/winbind config rendering, preflight checks, and live principal search (via `wbinfo`). The NixOS module ships winbindd + the winbind NSS module (inert until joined) and rebuilds Samba with LDAP/ADS support. `nasty-sharing::smb` renders an engine-managed global include (`/etc/samba/nasty-domain.conf`) that carries the ADS block when joined and is empty otherwise, and its `valid_users` validation gains a `DOMAIN\name` carve-out. Engine methods follow the `smb.user.list` naming precedent (`domain.join`, `domain.leave`, `domain.status`, `domain.user.list`, `domain.group.list`).

**Tech Stack:** Rust (engine workspace), Nix/NixOS module, Samba (`net ads`, `wbinfo`, winbindd), Svelte 5 WebUI.

**Spec:** `docs/active-directory-support.md` (phase 1). Read it before starting.

## Global Constraints

- Per-box opt-in: an unjoined box renders **byte-identical** Samba config to today; winbindd installed but never started; no new open ports (member mode is outbound-only).
- The AD admin credential is used once for `net ads join`, passed via **stdin**, never argv, never persisted, never logged.
- `idmap_base` default **100000**, minimum **65536**, immutable once joined (changing it re-maps file ownership).
- Domain principals render as `DOMAIN\name` — never set `winbind use default domain`.
- No new Rust dependencies. No `-sys` crates. (`nasty-system` already has everything needed.)
- Verification before every commit: `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test --workspace` from `engine/`; `npm run check` from `webui/` for WebUI tasks.
- All engine work in `engine/` (workspace root for cargo commands). Branch: `ad-member-join`.

---

### Task 1: NixOS substrate — ADS-capable Samba, winbindd, NSS, writable config files

**Files:**
- Modify: `nixos/modules/nasty.nix` (samba block ~lines 1731–1780, tmpfiles ~line 1471, systemPackages ~line 1435)

**Interfaces:**
- Produces: `samba-winbindd.service` (present, `wantedBy = []`, engine-started), `/etc/samba/nasty-domain.conf` + `/etc/samba/nasty-krb5.conf` (engine-writable, empty by default), winbind NSS module active, `security.wrappers`-free ADS-capable samba binaries on PATH.

This task has no unit test; its gate is `nix flake check` evaluation plus the existing appliance-smoke test in CI (SMB must keep working with the rebuilt package). The Integration workflow is the real proof — a local `cargo` run can't validate it.

- [ ] **Step 1: Define the ADS-capable samba package once**

In `nixos/modules/nasty.nix`, find the `let` block near the top of the module (or the config section where `pkgs` is in scope) and the existing samba references. Add a shared binding:

```nix
  # ADS member mode needs LDAP support; nixpkgs' default samba is built
  # --without-ldap --without-ads. One binding so smbd, the CLI tools, and
  # winbindd all come from the same build. This changes the samba binary
  # on EVERY box (joined or not) — the appliance-smoke test gates it.
  sambaAds = pkgs.samba.override { enableLDAP = true; };
```

Then replace the two use sites:
- `services.samba` (~line 1734): add `package = sambaAds;` inside the `mkIf cfg.smb.enable { ... }` attrset.
- systemPackages (~line 1435): change `lib.optionals cfg.smb.enable [ samba ]` to `lib.optionals cfg.smb.enable [ sambaAds ]` (and the other `[ samba shadow.out ]` occurrence at ~line 1616 similarly).

- [ ] **Step 2: Enable winbindd, engine-started**

Inside the same `services.samba` attrset add:

```nix
      winbindd.enable = true;
```

The existing `systemd.targets.samba.wantedBy = mkIf cfg.smb.enable (lib.mkForce []);` (~line 1770) already breaks auto-start for all three daemons — verify the comment there mentions winbindd now, and update it:

```nix
    # Prevent Samba from auto-starting at boot. NixOS enables samba.target in
    # multi-user.target, which then pulls in all four daemons (smbd, nmbd,
    # wsdd, winbindd) via samba.target.wants. Override the target's wantedBy
    # to break that chain; the engine starts smbd/nmbd/wsdd via the protocol
    # toggle and winbindd only while joined to a domain.
```

- [ ] **Step 3: Point winbindd and the Samba tools at the engine-rendered krb5 config**

Add next to the samba service config:

```nix
    # The engine renders Kerberos config at domain-join time (a runtime
    # operation, not a rebuild) into an engine-owned path. Route winbindd
    # and everything the engine execs (net, wbinfo) through it instead of
    # owning the global /etc/krb5.conf.
    systemd.services.samba-winbindd.environment = mkIf cfg.smb.enable {
      KRB5_CONFIG = "/etc/samba/nasty-krb5.conf";
    };
```

- [ ] **Step 4: Winbind NSS module (permanently present, inert when winbindd is down)**

```nix
    # Winbind NSS: domain users/groups resolve through nsswitch while
    # joined. Present on every box; instant not-found while winbindd
    # isn't running, so unjoined boxes are unaffected.
    system.nssModules = mkIf cfg.smb.enable [ sambaAds ];
    system.nssDatabases.passwd = mkIf cfg.smb.enable (lib.mkAfter [ "winbind" ]);
    system.nssDatabases.group = mkIf cfg.smb.enable (lib.mkAfter [ "winbind" ]);
```

- [ ] **Step 5: Engine-writable config files**

In the tmpfiles rules block (~line 1471, next to `"f /etc/samba/smb.nasty.conf 0644 root root -"`), add:

```nix
      "f /etc/samba/nasty-domain.conf 0644 root root -"
      "f /etc/samba/nasty-krb5.conf 0644 root root -"
```

(Do NOT use `environment.etc` for these — that creates read-only store symlinks; the engine writes them at runtime.)

- [ ] **Step 6: Evaluate**

Run: `nix flake check --no-build 2>&1 | tail -5`
Expected: evaluation succeeds (build failures would only surface in CI; evaluation errors surface here).

- [ ] **Step 7: Commit**

```bash
git add nixos/modules/nasty.nix
git commit -m "nixos: ADS-capable samba + engine-started winbindd + winbind NSS

Substrate for AD member join. sambaAds (enableLDAP) replaces the stock
samba everywhere so smbd, the CLI tools, and winbindd share one build;
winbindd follows the engine-started wantedBy=[] pattern and reads the
engine-rendered nasty-krb5.conf; the winbind NSS module is present but
inert until winbindd runs. Unjoined boxes render byte-identical Samba
config."
```

---

### Task 2: Domain module skeleton — types, state, realm validation (TDD)

**Files:**
- Create: `engine/nasty-system/src/domain.rs`
- Modify: `engine/nasty-system/src/lib.rs` (add `pub mod domain;` next to the other module declarations)

**Interfaces:**
- Produces:
  - `DomainConfig { realm: String, workgroup: String, idmap_base: u32 }` (Serialize/Deserialize/JsonSchema/Clone/Debug) — persisted at `/var/lib/nasty/domain/config.json`; file presence == joined.
  - `DomainError` (thiserror) with variants `Validation(String)`, `Preflight(String)`, `AlreadyJoined`, `NotJoined`, `CommandFailed(String)`, `Io(std::io::Error)`.
  - `fn validate_realm(raw: &str) -> Result<String, DomainError>` — returns the normalized (uppercased, trimmed) realm.
  - `fn derive_workgroup(realm: &str) -> String` — first DNS label, uppercased, truncated to 15 chars (NetBIOS limit).
  - `const DEFAULT_IDMAP_BASE: u32 = 100_000;` / `const IDMAP_RANGE_SPAN: u32 = 900_000;` / `fn validate_idmap_base(base: u32) -> Result<(), DomainError>` (rejects `< 65_536`).
  - `pub struct DomainService;` with `pub fn new() -> Self` and `async fn load_config() -> Option<DomainConfig>` / `async fn save_config(&DomainConfig)` / `async fn clear_config()` using `tokio::fs` on `/var/lib/nasty/domain/config.json` (create dir with `create_dir_all`).

- [ ] **Step 1: Write the failing tests**

Create `engine/nasty-system/src/domain.rs` with module docs, the types above as **stubs** (`validate_realm` returns `Err(DomainError::Validation("stub".into()))`, `derive_workgroup` returns `String::new()`, `validate_idmap_base` returns `Ok(())`), and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_realm_normalizes_and_accepts_dns_names() {
        assert_eq!(validate_realm("corp.example.com").unwrap(), "CORP.EXAMPLE.COM");
        assert_eq!(validate_realm("  ad.lan ").unwrap(), "AD.LAN");
    }

    #[test]
    fn validate_realm_rejects_garbage() {
        // Single label: not a resolvable AD realm.
        assert!(validate_realm("WORKGROUP").is_err());
        assert!(validate_realm("").is_err());
        // Characters that could smuggle config or shell content.
        assert!(validate_realm("corp.example.com\ninclude=/etc/passwd").is_err());
        assert!(validate_realm("corp;rm -rf /.com").is_err());
        assert!(validate_realm("corp .example.com").is_err());
    }

    #[test]
    fn derive_workgroup_takes_first_label_netbios_truncated() {
        assert_eq!(derive_workgroup("CORP.EXAMPLE.COM"), "CORP");
        // NetBIOS names cap at 15 chars.
        assert_eq!(derive_workgroup("VERYLONGCOMPANYNAME.LAN"), "VERYLONGCOMPANY");
    }

    #[test]
    fn validate_idmap_base_rejects_low_ranges() {
        // Must clear every local UID the engine can allocate.
        assert!(validate_idmap_base(3000).is_err());
        assert!(validate_idmap_base(65_535).is_err());
        assert!(validate_idmap_base(65_536).is_ok());
        assert!(validate_idmap_base(DEFAULT_IDMAP_BASE).is_ok());
    }
}
```

Realm validation rules to implement in Step 3: trim, reject empty, require ≥ 2 dot-separated labels, each label 1–63 chars of `[A-Za-z0-9-]` not starting/ending with `-`, then uppercase.

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p nasty-system --lib domain 2>&1 | tail -5`
Expected: FAIL — the three validation tests panic against the stubs (the reject-tests may trivially pass against the `Err` stub; the accept-tests must fail).

- [ ] **Step 3: Implement the real functions**

```rust
fn validate_realm(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DomainError::Validation("realm is empty".into()));
    }
    let labels: Vec<&str> = trimmed.split('.').collect();
    if labels.len() < 2 {
        return Err(DomainError::Validation(format!(
            "'{trimmed}' is not a DNS realm (expected e.g. CORP.EXAMPLE.COM)"
        )));
    }
    for label in &labels {
        let ok = !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(DomainError::Validation(format!(
                "realm label '{label}' contains invalid characters"
            )));
        }
    }
    Ok(trimmed.to_ascii_uppercase())
}

fn derive_workgroup(realm: &str) -> String {
    let first = realm.split('.').next().unwrap_or(realm);
    first.chars().take(15).collect::<String>().to_ascii_uppercase()
}

fn validate_idmap_base(base: u32) -> Result<(), DomainError> {
    if base < 65_536 {
        return Err(DomainError::Validation(format!(
            "idmap base {base} is too low — must be at least 65536 so domain \
             UIDs can never collide with local accounts"
        )));
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p nasty-system --lib domain 2>&1 | tail -3`
Expected: `test result: ok.`

- [ ] **Step 5: Full verification + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-system
git add engine/nasty-system/src/domain.rs engine/nasty-system/src/lib.rs
git commit -m "system: domain module skeleton — realm/idmap validation, join state"
```

---

### Task 3: Config renderers — ADS smb.conf block and krb5.conf (TDD)

**Files:**
- Modify: `engine/nasty-system/src/domain.rs`

**Interfaces:**
- Produces:
  - `pub fn render_domain_smb_conf(cfg: &DomainConfig) -> String` — the `[global]`-scope ADS block written to `/etc/samba/nasty-domain.conf`.
  - `pub fn render_krb5_conf(realm: &str) -> String` — written to `/etc/samba/nasty-krb5.conf`.
  - `pub const DOMAIN_SMB_CONF_PATH: &str = "/etc/samba/nasty-domain.conf";`
  - `pub const KRB5_CONF_PATH: &str = "/etc/samba/nasty-krb5.conf";`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn render_domain_smb_conf_emits_ads_block() {
        let cfg = DomainConfig {
            realm: "CORP.EXAMPLE.COM".into(),
            workgroup: "CORP".into(),
            idmap_base: 100_000,
        };
        let conf = render_domain_smb_conf(&cfg);
        assert!(conf.contains("security = ADS"), "{conf}");
        assert!(conf.contains("realm = CORP.EXAMPLE.COM"), "{conf}");
        assert!(conf.contains("workgroup = CORP"), "{conf}");
        // Deterministic algorithmic mapping — same user, same UID, forever.
        assert!(conf.contains("idmap config CORP : backend = rid"), "{conf}");
        assert!(conf.contains("idmap config CORP : range = 100000-999999"), "{conf}");
        // The default (*) range must not overlap the domain range.
        assert!(conf.contains("idmap config * : range = 65000-65535"), "{conf}");
        // DC outage tolerance for recently-seen users.
        assert!(conf.contains("winbind offline logon = yes"), "{conf}");
        // Explicit namespaces — never ambiguous with local users.
        assert!(!conf.contains("winbind use default domain"), "{conf}");
        assert!(conf.contains("kerberos method = secrets and keytab"), "{conf}");
    }

    #[test]
    fn render_krb5_conf_pins_realm_and_dns_lookup() {
        let conf = render_krb5_conf("CORP.EXAMPLE.COM");
        assert!(conf.contains("default_realm = CORP.EXAMPLE.COM"), "{conf}");
        // DCs are found via DNS SRV — no static kdc lines to go stale.
        assert!(conf.contains("dns_lookup_kdc = true"), "{conf}");
        assert!(conf.contains("rdns = false"), "{conf}");
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p nasty-system --lib domain::tests::render 2>&1 | tail -4`
Expected: FAIL (functions don't exist yet — add empty-string stubs first so it compiles, then watch the asserts fail).

- [ ] **Step 3: Implement the renderers**

```rust
pub fn render_domain_smb_conf(cfg: &DomainConfig) -> String {
    let base = cfg.idmap_base;
    let end = base + IDMAP_RANGE_SPAN - 1;
    format!(
        "# Managed by NASty — Active Directory member configuration.\n\
         # Rendered at domain join; emptied at leave. Do not edit manually.\n\
         security = ADS\n\
         realm = {realm}\n\
         workgroup = {wg}\n\
         kerberos method = secrets and keytab\n\
         winbind refresh tickets = yes\n\
         winbind offline logon = yes\n\
         winbind enum users = no\n\
         winbind enum groups = no\n\
         idmap config * : backend = tdb\n\
         idmap config * : range = 65000-65535\n\
         idmap config {wg} : backend = rid\n\
         idmap config {wg} : range = {base}-{end}\n\
         template shell = /run/current-system/sw/bin/nologin\n\
         template homedir = /var/empty\n",
        realm = cfg.realm,
        wg = cfg.workgroup,
    )
}

pub fn render_krb5_conf(realm: &str) -> String {
    format!(
        "# Managed by NASty — rendered at domain join.\n\
         [libdefaults]\n\
         \tdefault_realm = {realm}\n\
         \tdns_lookup_realm = false\n\
         \tdns_lookup_kdc = true\n\
         \trdns = false\n",
    )
}
```

(Realm/workgroup are safe to interpolate — `validate_realm` rejected every config-injection character before a `DomainConfig` can exist. Note this invariant in a comment.)

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p nasty-system --lib domain 2>&1 | tail -3`
Expected: `test result: ok.`

- [ ] **Step 5: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-system
git add engine/nasty-system/src/domain.rs
git commit -m "system: render ADS smb.conf block and krb5.conf for domain join"
```

---

### Task 4: Wire the domain include into the Samba config chain (TDD)

**Files:**
- Modify: `engine/nasty-sharing/src/smb.rs` (`rebuild_include_list`, ~line 471, and its tests)

**Interfaces:**
- Consumes: `/etc/samba/nasty-domain.conf` existing on disk (tmpfiles from Task 1; content from Task 3).
- Produces: `smb.nasty.conf` includes the domain conf **before** the tuning include and share includes (global params must precede section headers).

- [ ] **Step 1: Extract the include-list body into a pure function and write the failing test**

`rebuild_include_list` builds a string then writes it. Refactor the string-building into `fn render_include_list(share_conf_names: &[String]) -> String` (same output as today: header, tuning include, share includes) so it's testable, then write the failing test:

```rust
    #[test]
    fn include_list_puts_domain_conf_first() {
        let rendered = render_include_list(&["abc.conf".to_string()]);
        let domain_pos = rendered
            .find("include = /etc/samba/nasty-domain.conf")
            .expect("domain include present");
        let tuning_pos = rendered.find("include = /etc/samba/nasty-tuning.conf").unwrap();
        let share_pos = rendered.find("include = /etc/samba/nasty.d/abc.conf").unwrap();
        // Global-scope ADS params must land before any share section opens.
        assert!(domain_pos < tuning_pos && tuning_pos < share_pos, "{rendered}");
    }
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p nasty-sharing --lib smb::tests::include_list 2>&1 | tail -4`
Expected: FAIL — no domain include emitted.

- [ ] **Step 3: Implement**

In `render_include_list`, after the header lines and before the tuning include:

```rust
    // Domain (AD member) global parameters. The file exists on every box
    // (tmpfiles) and is empty until a join renders the ADS block into it,
    // so unjoined boxes get byte-identical effective config.
    includes.push_str("include = /etc/samba/nasty-domain.conf\n\n");
```

`rebuild_include_list` becomes: collect `.conf` names from the dir as before, call `render_include_list(&names)`, write.

- [ ] **Step 4: Run the full smb test module, verify green**

Run: `cargo test -p nasty-sharing --lib smb 2>&1 | tail -3`
Expected: `test result: ok.` (existing include/render tests unchanged).

- [ ] **Step 5: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-sharing
git add engine/nasty-sharing/src/smb.rs
git commit -m "sharing: include engine-managed domain conf in smb.nasty.conf"
```

---

### Task 5: Preflight — SRV discovery and clock-skew parsing (TDD)

**Files:**
- Modify: `engine/nasty-system/src/domain.rs`

**Interfaces:**
- Produces:
  - `fn parse_resolvectl_srv(output: &str) -> Vec<String>` — DC hostnames from `resolvectl query --type=SRV _ldap._tcp.<realm>`.
  - `fn parse_net_ads_server_time(output: &str) -> Option<i64>` — unix seconds from `net ads info`'s `Server time:` line (hand-rolled RFC-2822-style parse; no new deps).
  - `fn parse_resolvectl_addresses(output: &str) -> Vec<String>` — IPs from `resolvectl query <host>` (lines like `dc1.corp.example.com: 10.0.0.5`).
  - `pub fn render_resolved_dropin(realm: &str, dc_ips: &[String]) -> String` + `pub const RESOLVED_DROPIN_PATH: &str = "/etc/systemd/resolved.conf.d/nasty-ad.conf";` — per-domain DNS routing (`Domains=~<realm>` + `DNS=` the DCs) so the box resolves the AD zone through AD DNS without replacing its resolvers (spec join-flow step 6).
  - `async fn preflight(realm: &str) -> Result<Vec<String>, DomainError>` — returns the discovered DC hostnames; SRV lookup non-empty, `net ads info` reachable, `|skew| <= 240s` (comfortably under Kerberos's 300s).

- [ ] **Step 1: Write the failing parser tests**

```rust
    #[test]
    fn parse_resolvectl_srv_extracts_targets() {
        // resolvectl output shape: "_ldap._tcp.corp.example.com IN SRV 0 100 389 dc1.corp.example.com"
        let out = "\
_ldap._tcp.corp.example.com IN SRV 0 100 389 dc1.corp.example.com\n\
_ldap._tcp.corp.example.com IN SRV 0 100 389 dc2.corp.example.com\n\n\
-- Information acquired via protocol DNS in 2.1ms.\n";
        assert_eq!(
            parse_resolvectl_srv(out),
            vec!["dc1.corp.example.com".to_string(), "dc2.corp.example.com".to_string()]
        );
        assert!(parse_resolvectl_srv("-- no data --\n").is_empty());
    }

    #[test]
    fn parse_resolvectl_addresses_extracts_ips() {
        let out = "dc1.corp.example.com: 10.0.0.5\n\n-- Information acquired via protocol DNS in 1.2ms.\n";
        assert_eq!(parse_resolvectl_addresses(out), vec!["10.0.0.5".to_string()]);
    }

    #[test]
    fn render_resolved_dropin_routes_realm_to_dcs() {
        let conf = render_resolved_dropin("CORP.EXAMPLE.COM", &["10.0.0.5".into(), "10.0.0.6".into()]);
        assert!(conf.contains("[Resolve]"), "{conf}");
        assert!(conf.contains("DNS=10.0.0.5 10.0.0.6"), "{conf}");
        // Routing domain (~) — only AD-zone queries go to the DCs.
        assert!(conf.contains("Domains=~corp.example.com"), "{conf}");
    }

    #[test]
    fn parse_net_ads_server_time_reads_rfc2822_style() {
        // `net ads info` prints e.g. "Server time: Tue, 07 Jul 2026 12:34:56 UTC"
        let out = "LDAP server: 10.0.0.5\nServer time: Tue, 07 Jul 2026 12:34:56 UTC\n";
        // 2026-07-07T12:34:56Z
        assert_eq!(parse_net_ads_server_time(out), Some(1783514096));
        assert_eq!(parse_net_ads_server_time("no time here"), None);
    }
```

(Compute the expected epoch with `date -u -d "2026-07-07 12:34:56" +%s` before pinning it; correct the constant if it differs.)

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p nasty-system --lib domain::tests::parse 2>&1 | tail -4`
Expected: FAIL against stubs.

- [ ] **Step 3: Implement the parsers**

```rust
fn parse_resolvectl_srv(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|l| l.contains(" SRV "))
        .filter_map(|l| l.split_whitespace().last())
        .map(|t| t.trim_end_matches('.').to_string())
        .collect()
}

fn parse_resolvectl_addresses(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.split_once(": "))
        .map(|(_, addr)| addr.trim().to_string())
        .filter(|a| a.parse::<std::net::IpAddr>().is_ok())
        .collect()
}

pub fn render_resolved_dropin(realm: &str, dc_ips: &[String]) -> String {
    format!(
        "# Managed by NASty — routes AD-zone DNS queries to the domain\n\
         # controllers without replacing the box's resolvers. Removed at leave.\n\
         [Resolve]\n\
         DNS={}\n\
         Domains=~{}\n",
        dc_ips.join(" "),
        realm.to_ascii_lowercase(),
    )
}

/// Parse `net ads info`'s "Server time:" line into unix seconds.
/// Format observed: "Server time: Tue, 07 Jul 2026 12:34:56 UTC".
/// Hand-rolled (days-since-epoch arithmetic) to avoid a chrono dep for
/// one line of output; only UTC/GMT zones are accepted — anything else
/// returns None and preflight skips the skew check rather than
/// mis-judging it.
fn parse_net_ads_server_time(output: &str) -> Option<i64> {
    let line = output.lines().find(|l| l.trim_start().starts_with("Server time:"))?;
    let rest = line.split_once(':')?.1.trim(); // "Tue, 07 Jul 2026 12:34:56 UTC"
    let rest = rest.split_once(',').map(|(_, r)| r.trim()).unwrap_or(rest);
    let mut parts = rest.split_whitespace(); // 07 Jul 2026 12:34:56 UTC
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let (h, m, s): (i64, i64, i64) = (
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
    );
    if !matches!(parts.next(), Some("UTC") | Some("GMT")) {
        return None;
    }
    // Days since 1970-01-01 (civil-from-days, Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + m * 60 + s)
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p nasty-system --lib domain 2>&1 | tail -3`
Expected: `test result: ok.`

- [ ] **Step 5: Implement `preflight` (async glue — no unit test, exercised by the VM test)**

```rust
/// Fail fast with actionable errors before Kerberos gets a chance to
/// produce cryptic ones. Checks, in order: the realm's LDAP SRV records
/// resolve; a DC answers `net ads info`; clock skew is within bounds.
async fn preflight(realm: &str) -> Result<Vec<String>, DomainError> {
    let srv_name = format!("_ldap._tcp.{}", realm.to_ascii_lowercase());
    let out = run_cmd("resolvectl", &["query", "--type=SRV", &srv_name], &[]).await?;
    let dcs = parse_resolvectl_srv(&out);
    if dcs.is_empty() {
        return Err(DomainError::Preflight(format!(
            "no domain controllers found: DNS SRV lookup for {srv_name} returned \
             nothing. The box's DNS must be able to resolve the AD zone — point \
             it at (or forward to) the domain's DNS server."
        )));
    }
    let info = run_cmd(
        "net",
        &["ads", "info", "-S", &dcs[0], "--realm", realm],
        &[("KRB5_CONFIG", KRB5_CONF_PATH)],
    )
    .await
    .map_err(|e| DomainError::Preflight(format!(
        "domain controller {} did not answer: {e}", dcs[0]
    )))?;
    if let Some(server_time) = parse_net_ads_server_time(&info) {
        let local = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let skew = (server_time - local).abs();
        if skew > 240 {
            return Err(DomainError::Preflight(format!(
                "clock skew vs domain controller is {skew}s — Kerberos tolerates \
                 ~300s. Fix NTP before joining."
            )));
        }
    }
    Ok(dcs)
}
```

Add the small `run_cmd(program, args, envs) -> Result<String, DomainError>` helper wrapping `tokio::process::Command` (capture stdout+stderr, `CommandFailed` with stderr on non-zero exit) — model it on `nasty_sharing::smb`'s command usage.

- [ ] **Step 6: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-system
git add engine/nasty-system/src/domain.rs
git commit -m "system: domain join preflight — SRV discovery and clock-skew check"
```

---

### Task 6: Join, leave, status — the service methods

**Files:**
- Modify: `engine/nasty-system/src/domain.rs`

**Interfaces:**
- Consumes: Tasks 2/3/5 (`validate_realm`, renderers, `preflight`, config load/save).
- Produces (used verbatim by Task 8's router arms):
  - `pub struct JoinDomainRequest { pub realm: String, pub username: String, pub password: String, pub ou: Option<String>, pub idmap_base: Option<u32> }` (Deserialize/JsonSchema)
  - `pub struct LeaveDomainRequest { pub username: Option<String>, pub password: Option<String>, #[serde(default)] pub force: bool }`
  - `pub struct DomainStatus { pub joined: bool, pub realm: Option<String>, pub workgroup: Option<String>, pub idmap_base: Option<u32>, pub trust_ok: Option<bool>, pub dc_reachable: Option<bool>, pub clock_skew_seconds: Option<i64> }` (Serialize/JsonSchema)
  - `impl DomainService`: `pub async fn status(&self) -> DomainStatus`, `pub async fn join(&self, req: JoinDomainRequest) -> Result<DomainStatus, DomainError>`, `pub async fn leave(&self, req: LeaveDomainRequest) -> Result<(), DomainError>`.

This task is orchestration glue over external commands — its unit surface is already tested (validators, renderers, parsers); the flow itself is exercised end-to-end by Task 10's VM test. Keep each step small and mirror the spec's join sequence exactly.

- [ ] **Step 1: Implement `join`**

Sequence (each failure before the state save must restore the previous on-disk config — snapshot the two rendered files' prior contents first, write them back in the error path):

```rust
pub async fn join(&self, req: JoinDomainRequest) -> Result<DomainStatus, DomainError> {
    if Self::load_config().await.is_some() {
        return Err(DomainError::AlreadyJoined);
    }
    let realm = validate_realm(&req.realm)?;
    let idmap_base = req.idmap_base.unwrap_or(DEFAULT_IDMAP_BASE);
    validate_idmap_base(idmap_base)?;
    let cfg = DomainConfig {
        workgroup: derive_workgroup(&realm),
        realm,
        idmap_base,
    };

    // Render krb5 first — preflight's `net ads info` reads it.
    tokio::fs::write(KRB5_CONF_PATH, render_krb5_conf(&cfg.realm)).await?;
    let dcs = match preflight(&cfg.realm).await {
        Ok(dcs) => dcs,
        Err(e) => {
            let _ = tokio::fs::write(KRB5_CONF_PATH, "").await;
            return Err(e);
        }
    };

    // Per-domain DNS routing: AD-zone queries go to the DCs from here on
    // (spec join-flow step 6). Resolve the discovered DC hostnames to IPs
    // through the still-working current resolvers.
    let mut dc_ips = Vec::new();
    for dc in dcs.iter().take(3) {
        if let Ok(out) = run_cmd("resolvectl", &["query", dc], &[]).await {
            dc_ips.extend(parse_resolvectl_addresses(&out));
        }
    }
    if !dc_ips.is_empty() {
        tokio::fs::create_dir_all("/etc/systemd/resolved.conf.d").await?;
        tokio::fs::write(RESOLVED_DROPIN_PATH, render_resolved_dropin(&cfg.realm, &dc_ips)).await?;
        let _ = systemctl("restart", "systemd-resolved.service").await;
    }

    tokio::fs::write(DOMAIN_SMB_CONF_PATH, render_domain_smb_conf(&cfg)).await?;
    systemctl("restart", "samba-winbindd.service").await?;

    // Credential via stdin (`net ads join -U user` prompts on stdin when
    // no %password is attached). NEVER put the password in argv.
    let mut join_args = vec!["ads", "join", "-U", req.username.as_str(), "--no-dns-updates"];
    let ou_arg;
    if let Some(ou) = &req.ou {
        ou_arg = format!("createcomputer={ou}");
        join_args.push(&ou_arg);
    }
    let join_out = run_cmd_stdin("net", &join_args, &[("KRB5_CONFIG", KRB5_CONF_PATH)], &req.password).await;

    match join_out {
        Ok(_) => {}
        Err(e) => {
            // Roll back: empty the rendered configs, drop the DNS routing,
            // stop winbindd.
            let _ = tokio::fs::write(DOMAIN_SMB_CONF_PATH, "").await;
            let _ = tokio::fs::write(KRB5_CONF_PATH, "").await;
            let _ = tokio::fs::remove_file(RESOLVED_DROPIN_PATH).await;
            let _ = systemctl("restart", "systemd-resolved.service").await;
            let _ = systemctl("stop", "samba-winbindd.service").await;
            return Err(e);
        }
    }

    // Verify the trust before declaring success.
    if let Err(e) = run_cmd("wbinfo", &["-t"], &[]).await {
        let _ = run_cmd_stdin("net", &["ads", "leave", "-U", &req.username], &[("KRB5_CONFIG", KRB5_CONF_PATH)], &req.password).await;
        let _ = tokio::fs::write(DOMAIN_SMB_CONF_PATH, "").await;
        let _ = tokio::fs::write(KRB5_CONF_PATH, "").await;
        let _ = systemctl("stop", "samba-winbindd.service").await;
        return Err(DomainError::CommandFailed(format!(
            "joined but the trust check failed (wbinfo -t): {e}"
        )));
    }

    // Register our A record in AD DNS (best-effort — some sites restrict it).
    if let Err(e) = run_cmd("net", &["ads", "dns", "register"], &[("KRB5_CONFIG", KRB5_CONF_PATH)]).await {
        tracing::warn!("AD DNS register failed (non-fatal): {e}");
    }

    Self::save_config(&cfg).await?;
    // smbd reloads pick up the ADS block via the include chain.
    let _ = run_cmd("smbcontrol", &["all", "reload-config"], &[]).await;
    tracing::info!("Joined AD domain {} (workgroup {})", cfg.realm, cfg.workgroup);
    Ok(self.status().await)
}
```

Add `run_cmd_stdin` (like `run_cmd` but writes the final arg to the child's stdin and appends `\n`). Add `systemctl` — reuse the pattern from `nasty-system/src/protocol.rs` (there's an existing `systemctl` helper there; make it `pub(crate)` and import it instead of duplicating).

- [ ] **Step 2: Implement `leave` and `status`**

```rust
pub async fn leave(&self, req: LeaveDomainRequest) -> Result<(), DomainError> {
    let Some(_cfg) = Self::load_config().await else {
        return Err(DomainError::NotJoined);
    };
    match (&req.username, &req.password, req.force) {
        (Some(user), Some(pass), _) => {
            run_cmd_stdin("net", &["ads", "leave", "-U", user], &[("KRB5_CONFIG", KRB5_CONF_PATH)], pass).await?;
        }
        (_, _, true) => {
            // Forced local leave: no DC contact; the computer account
            // goes stale in AD (documented, matches `net ads leave` docs).
            tracing::warn!("forced local domain leave — computer account left behind in AD");
        }
        _ => {
            return Err(DomainError::Validation(
                "leave needs AD credentials, or force=true for a local-only leave".into(),
            ));
        }
    }
    tokio::fs::write(DOMAIN_SMB_CONF_PATH, "").await?;
    tokio::fs::write(KRB5_CONF_PATH, "").await?;
    let _ = tokio::fs::remove_file(RESOLVED_DROPIN_PATH).await;
    let _ = systemctl("restart", "systemd-resolved.service").await;
    let _ = systemctl("stop", "samba-winbindd.service").await;
    Self::clear_config().await;
    let _ = run_cmd("smbcontrol", &["all", "reload-config"], &[]).await;
    tracing::info!("Left AD domain");
    Ok(())
}

pub async fn status(&self) -> DomainStatus {
    let Some(cfg) = Self::load_config().await else {
        return DomainStatus { joined: false, realm: None, workgroup: None, idmap_base: None, trust_ok: None, dc_reachable: None, clock_skew_seconds: None };
    };
    let trust_ok = Some(run_cmd("wbinfo", &["-t"], &[]).await.is_ok());
    let (dc_reachable, clock_skew_seconds) =
        match run_cmd("net", &["ads", "info"], &[("KRB5_CONFIG", KRB5_CONF_PATH)]).await {
            Ok(out) => {
                let skew = parse_net_ads_server_time(&out).map(|t| {
                    let local = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    t - local
                });
                (Some(true), skew)
            }
            Err(_) => (Some(false), None),
        };
    DomainStatus {
        joined: true,
        realm: Some(cfg.realm),
        workgroup: Some(cfg.workgroup),
        idmap_base: Some(cfg.idmap_base),
        trust_ok,
        dc_reachable,
        clock_skew_seconds,
    }
}
```

- [ ] **Step 3: Startup restore — start winbindd on boot when joined**

In `engine/nasty-engine/src/main.rs`, find the boot phases (`run_phase(...)` calls, e.g. `"filesystems.restore_mounts"`). Add after the protocol restore phase:

```rust
    state
        .boot_status
        .run_phase("domain.restore", secs(15), {
            let state = state.clone();
            async move {
                if state.domain.is_joined().await {
                    state.domain.ensure_winbindd().await;
                }
            }
        })
        .await;
```

with, in `domain.rs`:

```rust
    pub async fn is_joined(&self) -> bool {
        Self::load_config().await.is_some()
    }
    /// Start winbindd if it isn't running (boot restore; join already
    /// restarted it explicitly).
    pub async fn ensure_winbindd(&self) {
        if let Err(e) = systemctl("start", "samba-winbindd.service").await {
            tracing::error!("winbindd start failed on domain restore: {e}");
        }
    }
```

(AppState gains the field in Task 8 — if executing tasks in order, add a `// wired in the router task` TODO-free note in the commit message instead of a dangling reference: do this step LAST in this task and only compile-check it together with Task 8 if needed. Alternative: move this step into Task 8. Executor: if `state.domain` doesn't exist yet, defer just this step to Task 8's checklist — it's listed there too as Step 4.)

- [ ] **Step 4: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test --workspace
git add engine/nasty-system/src/domain.rs engine/nasty-system/src/protocol.rs
git commit -m "system: domain join/leave/status orchestration

Join: preflight → render configs → winbindd → net ads join (credential
via stdin, used once, never persisted) → wbinfo -t trust verification →
best-effort AD DNS register → persist state. Any failure rolls the
rendered configs back and stops winbindd. Leave reverses the flow;
force=true documents the stale computer account rather than pretending
to clean it up."
```

---

### Task 7: Live principal search via wbinfo (TDD)

**Files:**
- Modify: `engine/nasty-system/src/domain.rs`

**Interfaces:**
- Produces:
  - `pub struct DomainPrincipal { pub name: String }` (Serialize/JsonSchema) — `name` is `DOMAIN\user`.
  - `fn filter_principals(raw: &str, prefix: &str, limit: usize) -> Vec<DomainPrincipal>` (pure).
  - `pub async fn search_users(&self, prefix: &str) -> Result<Vec<DomainPrincipal>, DomainError>` / `pub async fn search_groups(&self, prefix: &str) -> Result<Vec<DomainPrincipal>, DomainError>` — run `wbinfo -u` / `wbinfo -g`, filter with `filter_principals`, `limit = 50`. Empty/short prefix (< 2 chars) → `Validation` error (no wholesale enumeration reaches the API).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn filter_principals_matches_prefix_case_insensitively_and_caps() {
        let raw = "CORP\\alice\nCORP\\albert\nCORP\\bob\nCORP\\Alfred\n";
        let hits = filter_principals(raw, "al", 50);
        let names: Vec<&str> = hits.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["CORP\\alice", "CORP\\albert", "CORP\\Alfred"]);
        // Prefix matches the account part, not the domain part.
        assert!(filter_principals(raw, "corp", 50).is_empty());
        // Limit is a hard cap.
        assert_eq!(filter_principals(raw, "al", 2).len(), 2);
    }
```

- [ ] **Step 2: Run test, verify it fails** — `cargo test -p nasty-system --lib domain::tests::filter 2>&1 | tail -3`

- [ ] **Step 3: Implement**

```rust
fn filter_principals(raw: &str, prefix: &str, limit: usize) -> Vec<DomainPrincipal> {
    let prefix = prefix.to_lowercase();
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            l.rsplit_once('\\')
                .map(|(_, account)| account.to_lowercase().starts_with(&prefix))
                .unwrap_or(false)
        })
        .take(limit)
        .map(|l| DomainPrincipal { name: l.to_string() })
        .collect()
}
```

and the async wrappers:

```rust
pub async fn search_users(&self, prefix: &str) -> Result<Vec<DomainPrincipal>, DomainError> {
    self.search(prefix, "-u").await
}
pub async fn search_groups(&self, prefix: &str) -> Result<Vec<DomainPrincipal>, DomainError> {
    self.search(prefix, "-g").await
}
async fn search(&self, prefix: &str, flag: &str) -> Result<Vec<DomainPrincipal>, DomainError> {
    if Self::load_config().await.is_none() {
        return Err(DomainError::NotJoined);
    }
    if prefix.trim().len() < 2 {
        return Err(DomainError::Validation(
            "search prefix must be at least 2 characters".into(),
        ));
    }
    let raw = run_cmd("wbinfo", &[flag], &[]).await?;
    Ok(filter_principals(&raw, prefix.trim(), 50))
}
```

- [ ] **Step 4: Run tests, verify green** — `cargo test -p nasty-system --lib domain 2>&1 | tail -3`

- [ ] **Step 5: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-system
git add engine/nasty-system/src/domain.rs
git commit -m "system: prefix-filtered domain principal search via wbinfo"
```

---

### Task 8: Engine wiring — AppState, router arms, method registry

**Files:**
- Modify: `engine/nasty-engine/src/main.rs` (AppState struct at line ~50; construction site; boot phase from Task 6 Step 3)
- Create: `engine/nasty-engine/src/router/domain.rs`
- Modify: `engine/nasty-engine/src/router/mod.rs` (module decl ~line 10; prefix match ~line 458)
- Modify: `engine/nasty-engine/src/registry/methods.rs`

**Interfaces:**
- Consumes: `DomainService` API from Tasks 6–7 exactly as declared there.
- Produces: JSON-RPC methods `domain.status` (Any), `domain.join`, `domain.leave`, `domain.user.list`, `domain.group.list` (Admin).

- [ ] **Step 1: AppState field**

In `engine/nasty-engine/src/main.rs`: add to `AppState` (next to `pub smb: ...`):

```rust
    pub domain: nasty_system::domain::DomainService,
```

and at the AppState construction site: `domain: nasty_system::domain::DomainService::new(),`. Now also apply Task 6 Step 3 (the `domain.restore` boot phase) if it was deferred.

- [ ] **Step 2: Router module**

Create `engine/nasty-engine/src/router/domain.rs`, modeled exactly on `router/share.rs`'s arm style:

```rust
//! RPC arms in the `domain.*` namespace (Active Directory member mode).

use nasty_common::{Request, Response};

use super::*;
use crate::AppState;
use crate::auth::Session;

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    _session: &Session,
) -> Option<Response> {
    Some(match req.method.as_str() {
        "domain.status" => ok(req, state.domain.status().await),
        "domain.join" => match parse_params::<nasty_system::domain::JoinDomainRequest>(req) {
            Ok(p) => match state.domain.join(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "domain.leave" => match parse_params::<nasty_system::domain::LeaveDomainRequest>(req) {
            Ok(p) => match state.domain.leave(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "domain.user.list" => match require_str(req, "prefix") {
            Ok(prefix) => match state.domain.search_users(prefix).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "domain.group.list" => match require_str(req, "prefix") {
            Ok(prefix) => match state.domain.search_groups(prefix).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}
```

(Check `err(...)`'s bound in `router/mod.rs` — if it requires `std::error::Error`, `DomainError` already satisfies it via thiserror. `parse_params`, `require_str`, `ok`, `invalid` come from `super::*` like the other domain modules.)

In `router/mod.rs`: add `mod domain;` to the module list and a prefix arm next to the others:

```rust
        "domain" => domain::try_route(req, state, session).await,
```

- [ ] **Step 3: Registry entries**

In `engine/nasty-engine/src/registry/methods.rs`, add imports for `JoinDomainRequest, LeaveDomainRequest, DomainStatus, DomainPrincipal` from `nasty_system::domain`, then a new group after the SMB user/group section (follow the exact `Method { ... }` shape at ~line 2163):

```rust
        (
            "Active Directory",
            vec![
                Method {
                    name: "domain.status",
                    desc: "Report AD membership: joined state, realm, trust health (wbinfo -t), DC reachability, and clock skew.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<DomainStatus>(generator)),
                },
                Method {
                    name: "domain.join",
                    desc: "Join an Active Directory domain. Runs preflight (DNS SRV, DC reachability, clock skew) before touching Kerberos; the admin credential is used once over stdin and never stored. Configuration rolls back on any failure.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<JoinDomainRequest>(generator)),
                    result: Some(gen_schema::<DomainStatus>(generator)),
                },
                Method {
                    name: "domain.leave",
                    desc: "Leave the AD domain. With credentials the computer account is removed from AD; with force=true the leave is local-only and the account goes stale.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<LeaveDomainRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "domain.user.list",
                    desc: "Search domain users by account-name prefix (min 2 chars, capped at 50). Live winbind query — domain users are never copied into NASty.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("prefix", "Account name prefix to search for.")),
                    result: Some(gen_schema::<Vec<DomainPrincipal>>(generator)),
                },
                Method {
                    name: "domain.group.list",
                    desc: "Search domain groups by name prefix (min 2 chars, capped at 50).",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("prefix", "Group name prefix to search for.")),
                    result: Some(gen_schema::<Vec<DomainPrincipal>>(generator)),
                },
            ],
        ),
```

- [ ] **Step 4: Build + full test run**

Run: `cargo build -p nasty-engine && cargo test --workspace 2>&1 | tail -3`
Expected: builds; all tests pass (the registry has a coverage test that fails if a registered method has no router arm — if it fires, the router arm names don't match the registry names; fix the typo).

- [ ] **Step 5: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings
git add engine/nasty-engine/src/
git commit -m "engine: domain.* methods — join/leave/status and principal search"
```

---

### Task 9: `valid_users` carve-out for DOMAIN\name entries (TDD)

**Files:**
- Modify: `engine/nasty-sharing/src/smb.rs`

**Interfaces:**
- Produces: `fn validate_valid_users(entries: &[String]) -> Result<(), SmbError>`, called from `SmbService::create` and `SmbService::update` before persisting `valid_users`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn validate_valid_users_accepts_local_group_and_domain_forms() {
        assert!(validate_valid_users(&["alice".into()]).is_ok());
        assert!(validate_valid_users(&["@staff".into()]).is_ok());
        assert!(validate_valid_users(&["CORP\\alice".into()]).is_ok());
        assert!(validate_valid_users(&["@CORP\\domain admins".into()]).is_ok());
        // AD account names may contain dots and spaces.
        assert!(validate_valid_users(&["CORP\\svc account.backup".into()]).is_ok());
    }

    #[test]
    fn validate_valid_users_rejects_injection_shapes() {
        // Same smuggling shapes validate_share_path pins (newline/config
        // injection), plus structural garbage.
        assert!(validate_valid_users(&["alice\ninclude = /etc/passwd".into()]).is_err());
        assert!(validate_valid_users(&["alice;rm".into()]).is_err());
        assert!(validate_valid_users(&["CORP\\\\alice".into()]).is_err(), "double backslash");
        assert!(validate_valid_users(&["\\alice".into()]).is_err(), "empty domain part");
        assert!(validate_valid_users(&["CORP\\".into()]).is_err(), "empty account part");
        assert!(validate_valid_users(&["".into()]).is_err());
        assert!(validate_valid_users(&["THISNETBIOSNAMEISTOOLONG\\alice".into()]).is_err());
    }
```

- [ ] **Step 2: Run tests, verify they fail** — `cargo test -p nasty-sharing --lib smb::tests::validate_valid 2>&1 | tail -4` (stub returning `Ok(())` makes the reject-test fail).

- [ ] **Step 3: Implement**

```rust
/// Validate share `valid_users` entries. Three accepted shapes:
/// local `name`, local `@group`, and domain `DOMAIN\name` (optionally
/// `@DOMAIN\group`) — the backslash carve-out for AD member mode.
/// Everything is checked against config-injection characters the same
/// way validate_share_path is; the domain part follows NetBIOS rules
/// (≤15 chars, alphanumeric + hyphen).
fn validate_valid_users(entries: &[String]) -> Result<(), SmbError> {
    for raw in entries {
        let entry = raw.strip_prefix('@').unwrap_or(raw);
        if entry.is_empty() || entry.len() > 256 {
            return Err(SmbError::InvalidName(format!(
                "invalid valid_users entry '{raw}'"
            )));
        }
        if entry.chars().any(|c| c.is_control() || matches!(c, ';' | '"' | '#' | '=' | '\n' | '\r')) {
            return Err(SmbError::InvalidName(format!(
                "valid_users entry '{raw}' contains forbidden characters"
            )));
        }
        match entry.split('\\').collect::<Vec<_>>().as_slice() {
            // Local user or group: existing character policy.
            [name] => {
                if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                    return Err(SmbError::InvalidName(format!(
                        "invalid user/group name '{raw}'"
                    )));
                }
            }
            // DOMAIN\name: NetBIOS domain + AD account (spaces/dots legal).
            [domain, name] => {
                let domain_ok = !domain.is_empty()
                    && domain.len() <= 15
                    && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                let name_ok = !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
                if !domain_ok || !name_ok {
                    return Err(SmbError::InvalidName(format!(
                        "invalid domain principal '{raw}'"
                    )));
                }
            }
            _ => {
                return Err(SmbError::InvalidName(format!(
                    "invalid valid_users entry '{raw}'"
                )));
            }
        }
    }
    Ok(())
}
```

Call it in `create` (where `valid_users: req.valid_users.unwrap_or_default()` is built, ~line 195 — validate before constructing the share) and in `update` (~line 247, before `share.valid_users = valid_users`).

- [ ] **Step 4: Run the whole smb module** — `cargo test -p nasty-sharing --lib smb 2>&1 | tail -3` — expected green. If any existing test created shares with entries the new validation rejects, the validation is too strict — fix the validator, not the test.

- [ ] **Step 5: Verify + commit**

```bash
cargo fmt && cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-sharing
git add engine/nasty-sharing/src/smb.rs
git commit -m "sharing: validate valid_users entries, DOMAIN\\name carve-out"
```

---

### Task 10: Two-node NixOS VM test — DC + joining appliance

**Files:**
- Create: `nixos/tests/ad-member.nix`
- Modify: `flake.nix` (checks attrset, ~line 406–418, next to `appliance-smoke`)
- Modify: `.github/workflows/` — add the check to whichever workflow runs `bcachefs-smoke`/`appliance-smoke` (grep for `appliance-smoke` in `.github/workflows/*.yml` and mirror its invocation)

**Interfaces:**
- Consumes: the full engine + module stack from Tasks 1–9.

- [ ] **Step 1: Write the test**

Create `nixos/tests/ad-member.nix`. Model the harness plumbing (arguments, `pkgs.testers.runNixOSTest` vs the shape `appliance-smoke.nix` uses — copy its exact wrapper) and use this structure:

```nix
# Two-node AD member-join test: a throwaway Samba AD DC and a NASty
# appliance that joins it, resolves domain users through winbind, and
# serves an SMB share restricted to a domain user. This is the only
# place the whole join flow (preflight → net ads join → wbinfo trust →
# idmap) runs against a real KDC.
{ pkgs, nasty-engine, nasty-webui, nasty-bcachefs-tools }:

let
  realm = "NASTY.TEST";
  domain = "NASTYAD";
  adminPass = "Passw0rd.123";
  sambaDc = pkgs.samba.override {
    enableLDAP = true;
    enableDomainController = true;
  };
in
# ... same test-framework wrapper as appliance-smoke.nix ...
{
  name = "ad-member";

  nodes.dc = { config, ... }: {
    networking.firewall.enable = false;
    environment.systemPackages = [ sambaDc ];
    # Provision at boot; the DC's own DNS serves the AD zone.
    systemd.services.samba-dc = {
      wantedBy = [ "multi-user.target" ];
      path = [ sambaDc ];
      serviceConfig.Type = "notify";
      serviceConfig.NotifyAccess = "all";
      script = ''
        if [ ! -f /var/lib/samba/private/krb5.conf ]; then
          rm -f /etc/samba/smb.conf
          samba-tool domain provision \
            --realm=${realm} --domain=${domain} \
            --server-role=dc --dns-backend=SAMBA_INTERNAL \
            --adminpass='${adminPass}'
          samba-tool user create alice 'UserPass.123'
        fi
        systemd-notify --ready &
        exec samba --foreground --no-process-group
      '';
    };
    # The test framework gives static IPs; disable resolved so the DC's
    # internal DNS owns port 53.
    services.resolved.enable = false;
  };

  nodes.nasty = { ... }: {
    imports = [ /* same appliance module import as appliance-smoke.nix */ ];
    # Point the box's DNS at the DC — AD join requires resolving the
    # realm's SRV records through AD DNS.
    networking.nameservers = pkgs.lib.mkForce [ "192.168.1.1" ]; # dc's test-net IP
  };

  testScript = ''
    dc.start()
    nasty.start()
    dc.wait_for_unit("samba-dc.service")
    dc.wait_until_succeeds("host -t SRV _ldap._tcp.${pkgs.lib.toLower realm} 127.0.0.1", timeout=120)
    nasty.wait_for_unit("nasty-engine.service")

    # Join via the engine API (same path the WebUI uses).
    token = ...  # login helper — copy from appliance-smoke.nix
    join = rpc(token, "domain.join", {
        "realm": "${realm}",
        "username": "Administrator",
        "password": "${adminPass}",
    })
    assert join["joined"] is True, join
    assert join["trust_ok"] is True, join

    # Domain user resolves through winbind/NSS with a UID in the idmap range.
    uid = int(nasty.succeed("id -u '${domain}\\\\alice'").strip())
    assert 100000 <= uid < 1000000, f"idmap uid out of range: {uid}"

    # Principal search returns the user.
    users = rpc(token, "domain.user.list", {"prefix": "al"})
    assert any(u["name"].endswith("\\\\alice") for u in users), users

    # status stays healthy.
    st = rpc(token, "domain.status", {})
    assert st["trust_ok"] and st["dc_reachable"], st

    # Leave (forced local) unwinds cleanly.
    rpc(token, "domain.leave", {"force": True})
    st = rpc(token, "domain.status", {})
    assert st["joined"] is False, st
  '';
}
```

The `rpc(...)`/login helper: copy the exact WebSocket helper from `appliance-smoke.nix`'s `rpcSmoke` script — same login, same call shape. The exact node IPs come from the test framework's default net (check what `appliance-smoke.nix` assumes). This step is expected to need iteration in CI — keep the testScript assertions few and sharp.

- [ ] **Step 2: Register the check**

In `flake.nix` next to `appliance-smoke` (~line 417):

```nix
      ad-member = import ./nixos/tests/ad-member.nix {
        inherit pkgs nasty-engine nasty-webui nasty-bcachefs-tools;
      };
```

(match the exact argument set `appliance-smoke` receives), and add the check to the CI workflow that runs the other NixOS tests.

- [ ] **Step 3: Build locally if on Linux, otherwise push and watch CI**

Run (Linux): `nix build .#checks.x86_64-linux.ad-member -L 2>&1 | tail -20`
On macOS: push the branch and watch the Integration workflow — this test can only run there.
Expected: test passes; on failure, the testScript's assertion output names the failing stage.

- [ ] **Step 4: Commit**

```bash
git add nixos/tests/ad-member.nix flake.nix .github/workflows/
git commit -m "tests: two-node AD member-join VM test (throwaway DC + appliance)"
```

---

### Task 11: WebUI — Directory settings card and domain-user share picker

**Files:**
- Create: `webui/src/lib/domain.svelte.ts`
- Modify: `webui/src/lib/types.ts` (DomainStatus/DomainPrincipal types)
- Modify: `webui/src/routes/settings/+page.svelte` (new "Directory" card)
- Modify: `webui/src/routes/sharing/SmbPanel.svelte` (~line 208–230, the valid_users editor)

**Interfaces:**
- Consumes: `domain.status`, `domain.join`, `domain.leave`, `domain.user.list`, `domain.group.list` exactly as registered in Task 8.

- [ ] **Step 1: Types**

In `webui/src/lib/types.ts`:

```ts
/** Wire shape of `domain.status`. */
export interface DomainStatus {
	joined: boolean;
	realm: string | null;
	workgroup: string | null;
	idmap_base: number | null;
	trust_ok: boolean | null;
	dc_reachable: boolean | null;
	clock_skew_seconds: number | null;
}

/** One AD principal from `domain.user.list` / `domain.group.list` — name is `DOMAIN\name`. */
export interface DomainPrincipal {
	name: string;
}
```

- [ ] **Step 2: Store**

Create `webui/src/lib/domain.svelte.ts`, following the shape of `webui/src/lib/sharing/iscsi.svelte.ts` (single `$state` object + exported async handlers):

```ts
/** AD membership state + handlers (Settings → Directory card). */
import { getClient } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import { confirm } from '$lib/confirm.svelte';
import type { DomainStatus, DomainPrincipal } from '$lib/types';

const client = getClient();

export const domain = $state({
	status: null as DomainStatus | null,
	loading: true,
	joining: false,
	// join form
	realm: '',
	username: '',
	password: '',
	ou: '',
});

export async function domainRefresh() {
	try {
		domain.status = await client.call<DomainStatus>('domain.status');
	} catch { /* engine without domain support */ }
	domain.loading = false;
}

export async function domainJoin() {
	if (!domain.realm || !domain.username || !domain.password) return;
	domain.joining = true;
	const params: Record<string, unknown> = {
		realm: domain.realm.trim(),
		username: domain.username.trim(),
		password: domain.password,
	};
	if (domain.ou.trim()) params.ou = domain.ou.trim();
	const ok = await withToast(() => client.call<DomainStatus>('domain.join', params), 'Joined domain');
	domain.password = '';
	if (ok !== undefined) {
		domain.status = ok;
		domain.realm = ''; domain.username = ''; domain.ou = '';
	}
	domain.joining = false;
}

export async function domainLeave(force: boolean, username?: string, password?: string) {
	if (!await confirm(
		'Leave the domain?',
		force
			? 'Local-only leave: the computer account stays behind in AD. Shares referencing domain users keep their entries but domain logons stop working.'
			: 'The computer account will be removed from AD. Shares referencing domain users keep their entries but domain logons stop working.'
	)) return;
	const params: Record<string, unknown> = force ? { force: true } : { username, password };
	await withToast(() => client.call('domain.leave', params), 'Left domain');
	await domainRefresh();
}

/** Prefix search for the share permissions picker (2+ chars). */
export async function domainSearchUsers(prefix: string): Promise<DomainPrincipal[]> {
	if (prefix.trim().length < 2 || !domain.status?.joined) return [];
	try {
		return await client.call<DomainPrincipal[]>('domain.user.list', { prefix: prefix.trim() });
	} catch {
		return [];
	}
}
```

- [ ] **Step 3: Settings card**

In `webui/src/routes/settings/+page.svelte`, add a "Directory (Active Directory)" card following the visual pattern of the existing cards on that page:

- Not joined: inputs bound to `domain.realm` (placeholder `corp.example.com`), `domain.username` (placeholder `Administrator`), `domain.password` (type=password), collapsible "advanced" with `domain.ou`; a Join button calling `domainJoin` (disabled while `domain.joining`, label "Joining… (this contacts the domain controller)").
- Joined: realm + workgroup as read-only rows; three status pills from `domain.status` — trust (`trust_ok`), DC reachability (`dc_reachable`), clock skew (amber when `Math.abs(clock_skew_seconds ?? 0) > 120`); a Leave button opening a small choice (with credentials / force local) driving `domainLeave`.
- Call `domainRefresh()` from the page's existing `onMount` load sequence.

- [ ] **Step 4: Domain users in the SMB share permissions editor**

In `webui/src/routes/sharing/SmbPanel.svelte` (~line 208–230), the editor lists `smb.systemUsers` to add to `share.valid_users`. Below that list, when `domain.status?.joined`, add a search input:

```svelte
{#if domain.status?.joined}
	<div class="mt-2">
		<Input bind:value={domainSearch} placeholder="Search domain users (2+ chars)…" class="h-8 text-xs"
			oninput={async () => { domainHits = await domainSearchUsers(domainSearch); }} />
		{#each domainHits.filter(p => !share.valid_users.includes(p.name)) as p}
			<button class="block w-full rounded px-2 py-1 text-left font-mono text-xs hover:bg-secondary/50"
				onclick={() => addValidUser(share, p.name)}>
				{p.name}
			</button>
		{/each}
	</div>
{/if}
```

with `let domainSearch = $state(''); let domainHits: DomainPrincipal[] = $state([]);` in the script block, imports for `domain`, `domainSearchUsers` from `$lib/domain.svelte`, and `addValidUser` being whatever existing handler the panel uses to append an entry to `share.valid_users` and call `share.smb.update` (find the handler the `smb.systemUsers` buttons call at ~line 226 and reuse it verbatim; if it's inline, extract it to a local function first).

- [ ] **Step 5: Check + build**

Run: `cd webui && npm run check && npm run build`
Expected: 0 errors / 0 warnings; build succeeds.

- [ ] **Step 6: Commit**

```bash
git add webui/src
git commit -m "webui: Directory card (AD join/status/leave) + domain-user share picker"
```

---

### Task 12: Verify-during-implementation item — machine secret vs rollback

**Files:**
- Modify: `docs/active-directory-support.md` (resolve the "verify during implementation" risk note with findings)
- Possibly modify: `engine/nasty-system/src/domain.rs` (one rendered line)

**Interfaces:** none (investigation + one-line config decision).

- [ ] **Step 1: Establish where `/var/lib/samba` lives relative to version switches and subvolume rollbacks**

Read `nixos/modules/nasty.nix` for any `/var/lib` bind/subvolume handling and `engine/nasty-engine/src/subvol_rollback.rs` for what a rollback restores. Answer two questions in writing: (a) does `/var/lib/samba` survive a NASty version switch (it should — version switches don't touch `/var/lib`); (b) can a subvolume rollback restore an old `secrets.tdb` (only if `/var/lib` lives on a rolled-back subvolume — determine this from the module's filesystem layout).

- [ ] **Step 2: Apply the decision**

Per the spec's risk section: if (b) is possible, add one line to `render_domain_smb_conf`:

```rust
         machine password timeout = 0\n\
```

with a comment citing the rollback finding (no rotation → a restored `secrets.tdb` is never stale). If (b) is impossible, leave rotation on and document why in the spec.

- [ ] **Step 3: Update the spec + commit**

Replace the spec's "**Verify during implementation:**" paragraph with the finding and the decision taken.

```bash
git add docs/active-directory-support.md engine/nasty-system/src/domain.rs
git commit -m "docs: resolve machine-password-rotation question with rollback findings"
```

---

### Task 13: Final verification sweep

- [ ] **Step 1: Engine** — from `engine/`: `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test --workspace` — all green.
- [ ] **Step 2: WebUI** — from `webui/`: `npm run check && npm run build` — 0/0 and clean build.
- [ ] **Step 3: Nix** — `nix flake check --no-build` evaluates; push and confirm the Integration workflow (including the new `ad-member` check and the existing `appliance-smoke` against the rebuilt Samba) is green before requesting review.
- [ ] **Step 4: Footprint audit** — on the built system config, confirm the spec's "footprint on boxes that never join" section: `smb.nasty.conf` render for an unjoined box contains only the (empty) domain include as a delta, no new listening ports in the firewall rules, winbindd not running.
