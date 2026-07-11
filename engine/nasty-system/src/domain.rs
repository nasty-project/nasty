//! Active Directory domain join state and configuration.
//!
//! This module manages the lifecycle of NASty membership in an AD realm:
//! - Realm validation (DNS names only, no local workgroups)
//! - NetBIOS workgroup derivation from the realm's first label
//! - UID range allocation for domain users (must avoid local account collision)
//! - Persistent storage of join configuration in `/var/lib/nasty/domain/config.json`

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

use crate::protocol::systemctl;

/// Errors returned by domain operations.
#[derive(Debug, Error)]
pub enum DomainError {
    /// Validation failed (bad realm format, out-of-range idmap, etc.).
    #[error("validation error: {0}")]
    Validation(String),
    /// Preflight check failed (domain tools missing, network unreachable, etc.).
    #[error("preflight check failed: {0}")]
    Preflight(String),
    /// Already joined to a domain.
    #[error("already joined to a domain")]
    AlreadyJoined,
    /// Not currently joined to a domain.
    #[error("not joined to a domain")]
    NotJoined,
    /// A domain command (kinit, net ads, etc.) failed.
    #[error("domain command failed: {0}")]
    CommandFailed(String),
    /// I/O error (file operations, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Persisted domain join configuration.
///
/// Presence of the config file (`/var/lib/nasty/domain/config.json`) indicates
/// the system is AD-joined.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DomainConfig {
    /// Active Directory realm (DNS name, uppercase; e.g., "CORP.EXAMPLE.COM").
    pub realm: String,
    /// NetBIOS workgroup name derived from realm (≤ 15 chars, uppercase).
    pub workgroup: String,
    /// Base UID for domain user mappings (must be ≥ 65536 to avoid local collisions).
    pub idmap_base: u32,
}

/// Default base UID for domain user mappings.
/// UIDs below this are reserved for local system accounts.
pub const DEFAULT_IDMAP_BASE: u32 = 100_000;

/// UID range span for domain users (DEFAULT_IDMAP_BASE to DEFAULT_IDMAP_BASE + IDMAP_RANGE_SPAN).
pub const IDMAP_RANGE_SPAN: u32 = 900_000;

/// Validate and normalize an Active Directory realm name.
///
/// Returns the normalized (uppercase) realm on success.
/// Rejects: empty strings, single-label names (not resolvable AD realms),
/// invalid DNS characters, or trailing/leading hyphens per label.
pub fn validate_realm(raw: &str) -> Result<String, DomainError> {
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

/// Derive a NetBIOS workgroup name from an AD realm.
///
/// Takes the first DNS label and uppercases it, truncating to 15 chars
/// (NetBIOS limit). The realm is assumed already validated.
pub fn derive_workgroup(realm: &str) -> String {
    let first = realm.split('.').next().unwrap_or(realm);
    first
        .chars()
        .take(15)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Validate an idmap base UID.
///
/// Rejects values below 65536 to ensure domain UIDs never collide
/// with local system accounts (which typically occupy 0–65535).
pub fn validate_idmap_base(base: u32) -> Result<(), DomainError> {
    if base < 65_536 {
        return Err(DomainError::Validation(format!(
            "idmap base {base} is too low — must be at least 65536 so domain \
             UIDs can never collide with local accounts"
        )));
    }
    if base > u32::MAX - IDMAP_RANGE_SPAN {
        return Err(DomainError::Validation(format!(
            "idmap base {base} is too high — base + range span ({IDMAP_RANGE_SPAN}) \
             would overflow"
        )));
    }
    Ok(())
}

/// Path to the Samba ADS configuration fragment.
pub const DOMAIN_SMB_CONF_PATH: &str = "/etc/samba/nasty-domain.conf";

/// Path to the Kerberos configuration.
pub const KRB5_CONF_PATH: &str = "/etc/samba/nasty-krb5.conf";

/// Render the `[global]`-scope Samba configuration block for Active Directory.
///
/// Produces configuration suitable for `/etc/samba/nasty-domain.conf`.
/// Realm and workgroup are safe to interpolate — `validate_realm` guarantees
/// they contain no shell/config-injection characters before a `DomainConfig` can exist.
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

/// Render the Kerberos configuration.
///
/// Produces configuration suitable for `/etc/samba/nasty-krb5.conf`.
/// Realm is safe to interpolate — `validate_realm` guarantees it contains
/// no shell/config-injection characters.
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

/// Path to the resolved(1) drop-in that routes AD-zone DNS queries to the
/// domain controllers without replacing the box's own resolvers.
///
/// Lives under `/run` (not `/etc`) because `/etc/systemd/resolved.conf.d`
/// is read-only on NixOS — `systemd-resolved` reads runtime drop-ins from
/// `/run/systemd/resolved.conf.d` just as well, and `/run` is always
/// writable. This means the drop-in does not survive a reboot, but that's
/// fine: `join` is not persistent across reboots for this routing either
/// (it's re-created here on each join), and a persistent setup should
/// point the box's own DNS at the AD DNS servers instead.
pub const RESOLVED_DROPIN_PATH: &str = "/run/systemd/resolved.conf.d/nasty-ad.conf";

/// Extract domain controller hostnames from `resolvectl query --type=SRV
/// _ldap._tcp.<realm>` output (e.g. lines like
/// "_ldap._tcp.corp.example.com IN SRV 0 100 389 dc1.corp.example.com").
fn parse_resolvectl_srv(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| {
            let tokens: Vec<&str> = l.split_whitespace().collect();
            let srv = tokens.iter().position(|t| *t == "SRV")?;
            // SRV RDATA: priority weight port target
            tokens
                .get(srv + 4)
                .map(|t| t.trim_end_matches('.').to_string())
        })
        .collect()
}

/// Extract IPs from `resolvectl query <host>` output (lines like
/// "dc1.corp.example.com: 10.0.0.5").
fn parse_resolvectl_addresses(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.split_once(": "))
        .filter_map(|(_, rest)| {
            rest.split_whitespace()
                .next()
                .filter(|a| a.parse::<std::net::IpAddr>().is_ok())
                .map(str::to_string)
        })
        .collect()
}

/// Render the systemd-resolved drop-in that routes AD-zone DNS queries to
/// the domain controllers, per-domain (`Domains=~<realm>`) — the box's own
/// resolvers are left untouched for everything else (spec join-flow step 6).
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
    let line = output
        .lines()
        .find(|l| l.trim_start().starts_with("Server time:"))?;
    let rest = line.split_once(':')?.1.trim(); // "Tue, 07 Jul 2026 12:34:56 UTC"
    let rest = rest.split_once(',').map(|(_, r)| r.trim()).unwrap_or(rest);
    let mut parts = rest.split_whitespace(); // 07 Jul 2026 12:34:56 UTC
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
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

/// Parse `net ads workgroup` output: "Workgroup: NASTYAD".
fn parse_net_ads_workgroup(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Workgroup:"))
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
}

/// Kerberos-relevant skew between DC and local time, or None when the
/// server time is unusable. DCs answering anonymous `net ads info`
/// queries may report epoch zero for the time field — a sentinel, not
/// a clock reading; treating it as real skew blocked every join with
/// a ~56-year skew error (caught live by the ad-member VM test).
fn effective_skew(server_time: i64, local: i64) -> Option<i64> {
    // Anything before ~2001 is a sentinel or a badly broken DC clock;
    // either way, Kerberos itself will produce the authoritative error
    // if real skew exists — don't fabricate one from a sentinel.
    const SANE_SERVER_TIME_FLOOR: i64 = 1_000_000_000;
    (server_time >= SANE_SERVER_TIME_FLOOR).then(|| server_time - local)
}

/// Construct a command error message from output, including both stdout and
/// stderr. Prefers stderr if non-empty, otherwise stdout, otherwise "(no output)".
fn command_error(program: &str, output: &std::process::Output) -> DomainError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = match (stderr.trim(), stdout.trim()) {
        (e, o) if !e.is_empty() && !o.is_empty() => format!("{e} / {o}"),
        (e, _) if !e.is_empty() => e.to_string(),
        (_, o) if !o.is_empty() => o.to_string(),
        _ => "(no output)".to_string(),
    };
    DomainError::CommandFailed(format!("{program} exited with {}: {detail}", output.status,))
}

/// Run a command, capturing stdout+stderr. Returns stdout on success;
/// on non-zero exit (or a spawn failure) returns `CommandFailed` carrying
/// both stdout and stderr (or the spawn error).
pub(crate) async fn run_cmd(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, DomainError> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .await
        .map_err(|e| DomainError::CommandFailed(format!("failed to run {program}: {e}")))?;
    if !output.status.success() {
        return Err(command_error(program, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run a command, writing `stdin_input` (plus a trailing newline) to its
/// stdin — used to hand credentials to `net ads join`/`net ads leave`
/// without ever putting the password in argv (visible via `/proc/*/cmdline`).
/// Captures stdout+stderr the same way `run_cmd` does.
pub(crate) async fn run_cmd_stdin(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin_input: &str,
) -> Result<String, DomainError> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| DomainError::CommandFailed(format!("failed to run {program}: {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        let input = format!("{stdin_input}\n");
        let _ = stdin.write_all(input.as_bytes()).await;
        // Drop closes stdin so the child sees EOF after the password line.
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| DomainError::CommandFailed(format!("failed to run {program}: {e}")))?;
    if !output.status.success() {
        return Err(command_error(program, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Filter principals by prefix of the account name (case-insensitive),
/// respecting a limit. The input is the raw output of `wbinfo -u` or `wbinfo -g`.
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
        .map(|l| DomainPrincipal {
            name: l.to_string(),
        })
        .collect()
}

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
    .map_err(|e| {
        DomainError::Preflight(format!("domain controller {} did not answer: {e}", dcs[0]))
    })?;
    if let Some(server_time) = parse_net_ads_server_time(&info) {
        let local = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(skew) = effective_skew(server_time, local) {
            if skew.abs() > 240 {
                return Err(DomainError::Preflight(format!(
                    "clock skew vs domain controller is {}s — Kerberos tolerates \
                     ~300s. Fix NTP before joining.",
                    skew.abs()
                )));
            }
        } else {
            tracing::warn!(
                "DC did not report a usable server time; skipping preflight \
                 clock-skew check (Kerberos will enforce it)"
            );
        }
    }
    Ok(dcs)
}

/// Best-effort teardown of everything `join` may have touched, in
/// reverse order. When `leave_ad` carries credentials, the machine
/// account was already created — attempt a `net ads leave` so AD and
/// local state don't diverge. Every step is best-effort: unwind runs
/// on an already-failing path, and a secondary failure must not mask
/// the original error (log it instead).
async fn unwind_join_artifacts(leave_ad: Option<(&str, &str)>) {
    if let Some((user, pass)) = leave_ad
        && let Err(e) = run_cmd_stdin(
            "net",
            &["ads", "leave", "-U", user],
            &[("KRB5_CONFIG", KRB5_CONF_PATH)],
            pass,
        )
        .await
    {
        tracing::warn!(
            "join unwind: net ads leave failed (stale computer account left in AD): {e}"
        );
    }
    let _ = tokio::fs::write(DOMAIN_SMB_CONF_PATH, "").await;
    let _ = tokio::fs::write(KRB5_CONF_PATH, "").await;
    if tokio::fs::remove_file(RESOLVED_DROPIN_PATH).await.is_ok() {
        let _ = systemctl("restart", "systemd-resolved.service").await;
    }
    let _ = systemctl("stop", "samba-winbindd.service").await;
}

/// Request to join an Active Directory domain.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JoinDomainRequest {
    /// Active Directory realm to join (e.g. "CORP.EXAMPLE.COM").
    pub realm: String,
    /// AD account used to authorize the join (needs computer-object create rights).
    pub username: String,
    /// Password for `username`. Sent to `net ads join` via stdin only —
    /// never persisted, never placed in argv.
    pub password: String,
    /// Optional AD organizational unit to create the computer object in
    /// (`net ads join`'s `createcomputer=` option).
    pub ou: Option<String>,
    /// Optional base UID for domain user mappings; defaults to `DEFAULT_IDMAP_BASE`.
    pub idmap_base: Option<u32>,
}

/// Request to leave the currently-joined Active Directory domain.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LeaveDomainRequest {
    /// AD account used to authorize the leave (computer-object delete rights).
    pub username: Option<String>,
    /// Password for `username`. Sent to `net ads leave` via stdin only.
    pub password: Option<String>,
    /// Leave locally without contacting a DC, when credentials aren't
    /// available. The computer account is left behind (stale) in AD.
    #[serde(default)]
    pub force: bool,
}

/// Current Active Directory domain membership status.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DomainStatus {
    /// Whether the system is currently joined to a domain.
    pub joined: bool,
    /// The joined realm, if any.
    pub realm: Option<String>,
    /// The derived NetBIOS workgroup, if joined.
    pub workgroup: Option<String>,
    /// The configured idmap base UID, if joined.
    pub idmap_base: Option<u32>,
    /// Whether `wbinfo -t` (the domain trust secret) currently checks out.
    pub trust_ok: Option<bool>,
    /// Whether a domain controller answered `net ads info` just now.
    pub dc_reachable: Option<bool>,
    /// Clock skew (seconds, DC minus local) observed via `net ads info`.
    pub clock_skew_seconds: Option<i64>,
}

/// A principal (user or group) from the domain.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DomainPrincipal {
    /// The principal name in `DOMAIN\account` format.
    pub name: String,
}

/// Service for managing domain join state.
pub struct DomainService;

const CONFIG_PATH: &str = "/var/lib/nasty/domain/config.json";

impl Default for DomainService {
    fn default() -> Self {
        Self::new()
    }
}

impl DomainService {
    /// Create a new domain service instance.
    pub fn new() -> Self {
        Self
    }

    /// Load domain configuration from disk if it exists.
    pub async fn load_config() -> Option<DomainConfig> {
        Self::load_config_at(Path::new(CONFIG_PATH)).await
    }

    /// Persist domain configuration to disk.
    pub async fn save_config(config: &DomainConfig) -> Result<(), DomainError> {
        Self::save_config_at(Path::new(CONFIG_PATH), config).await
    }

    /// Clear domain configuration (leave domain).
    pub async fn clear_config() -> Result<(), DomainError> {
        Self::clear_config_at(Path::new(CONFIG_PATH)).await
    }

    /// Whether the system is currently joined to a domain.
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

    /// Join an Active Directory domain.
    ///
    /// Sequence: preflight (DNS SRV + DC reachability + clock skew) →
    /// render krb5 → ask a DC for the real NetBIOS domain (`net ads
    /// workgroup`, anonymous and pre-join) since the realm-derived guess can
    /// be wrong → render the smb config with the correct workgroup →
    /// `net ads join` (credential via stdin, used once, never persisted;
    /// it validates the configured workgroup against the DC, so it must
    /// already be right; `--realm` is passed explicitly so DC discovery
    /// never falls back to NetBIOS) → per-domain DNS routing to the
    /// discovered DCs (deferred until after the join succeeds — restarting
    /// systemd-resolved any earlier leaves DNS briefly unavailable, which
    /// makes `net ads join` itself fail to find a DC) → start winbindd
    /// (only now does a machine secret exist) → verify the trust with
    /// `wbinfo -t` → warm winbind by polling identity resolution for the
    /// domain Administrator (best-effort; a cold cache otherwise fails the
    /// first domain-user SMB access) → best-effort AD DNS registration →
    /// persist state. Every fallible step funnels through a single unwind
    /// path (`unwind_join_artifacts`) on failure, restoring the pre-join
    /// state; once `net ads join` has succeeded, unwind also attempts
    /// `net ads leave` (including on a later `save_config` failure) so AD
    /// and local state don't diverge.
    pub async fn join(&self, req: JoinDomainRequest) -> Result<DomainStatus, DomainError> {
        if Self::load_config().await.is_some() {
            return Err(DomainError::AlreadyJoined);
        }
        let realm = validate_realm(&req.realm)?;
        let idmap_base = req.idmap_base.unwrap_or(DEFAULT_IDMAP_BASE);
        validate_idmap_base(idmap_base)?;
        let mut cfg = DomainConfig {
            workgroup: derive_workgroup(&realm),
            realm,
            idmap_base,
        };

        let mut joined_in_ad = false;
        let result: Result<(), DomainError> = async {
            // Render krb5 first — preflight's `net ads info` reads it.
            tokio::fs::write(KRB5_CONF_PATH, render_krb5_conf(&cfg.realm)).await?;
            let dcs = preflight(&cfg.realm).await?;

            // The NetBIOS domain name is set at provision and is NOT derivable
            // from the realm (realm ADTEST.LAN can have NetBIOS domain NASTYAD).
            // net ads join validates the configured workgroup against the DC
            // and refuses on mismatch, so we must learn the real name BEFORE
            // rendering the config and joining — query a DC directly (anonymous,
            // pre-join). Fall back to the realm-derived guess if the query fails.
            let workgroup = match run_cmd(
                "net",
                &["ads", "workgroup", "-S", &dcs[0], "--realm", &cfg.realm],
                &[("KRB5_CONFIG", KRB5_CONF_PATH)],
            )
            .await
            {
                Ok(out) => match parse_net_ads_workgroup(&out) {
                    Some(wg) => {
                        if wg != cfg.workgroup {
                            tracing::info!(
                                "NetBIOS domain is {wg} (realm suggested {}); using the domain's real name",
                                cfg.workgroup
                            );
                        }
                        wg
                    }
                    None => {
                        tracing::warn!("could not parse NetBIOS domain from the DC; using derived guess {}", cfg.workgroup);
                        cfg.workgroup.clone()
                    }
                },
                Err(e) => {
                    tracing::warn!("NetBIOS domain query failed ({e}); using derived guess {}", cfg.workgroup);
                    cfg.workgroup.clone()
                }
            };
            cfg.workgroup = workgroup;

            tokio::fs::write(DOMAIN_SMB_CONF_PATH, render_domain_smb_conf(&cfg)).await?;

            // Credential via stdin (`net ads join -U user` prompts on stdin
            // when no %password is attached). NEVER put the password in argv.
            let mut join_args = vec![
                "ads",
                "join",
                "-U",
                req.username.as_str(),
                "--no-dns-updates",
                "--realm",
                cfg.realm.as_str(),
            ];
            let ou_arg;
            if let Some(ou) = &req.ou {
                ou_arg = format!("createcomputer={ou}");
                join_args.push(&ou_arg);
            }
            run_cmd_stdin(
                "net",
                &join_args,
                &[("KRB5_CONFIG", KRB5_CONF_PATH)],
                &req.password,
            )
            .await?;
            // The machine account now exists in AD — from here on, any
            // failure must attempt `net ads leave` during unwind.
            joined_in_ad = true;

            // Per-domain DNS routing: AD-zone queries go to the DCs from here
            // on (spec join-flow step 6). Resolve the discovered DC
            // hostnames to IPs through the still-working current resolvers.
            // Done only now (after the join has already succeeded) because
            // restarting systemd-resolved mid-join left DNS briefly
            // unavailable, causing `net ads join` to fall back to NetBIOS
            // discovery and fail. Preflight/join rely on the operator's DNS
            // already resolving the realm; this routing is for ongoing
            // operation (winbind resolving the DC by name).
            let mut dc_ips = Vec::new();
            for dc in dcs.iter().take(3) {
                if let Ok(out) = run_cmd("resolvectl", &["query", dc], &[]).await {
                    dc_ips.extend(parse_resolvectl_addresses(&out));
                }
            }
            if !dc_ips.is_empty() {
                tokio::fs::create_dir_all("/run/systemd/resolved.conf.d").await?;
                tokio::fs::write(
                    RESOLVED_DROPIN_PATH,
                    render_resolved_dropin(&cfg.realm, &dc_ips),
                )
                .await?;
                let _ = systemctl("restart", "systemd-resolved.service").await;
            } else {
                // Resolving the DC hostnames to IPs failed, so we can't write
                // the per-domain routing drop-in. The join has already
                // succeeded, but AD-zone resolution afterwards (winbind,
                // Kerberos referrals) depends entirely on the existing
                // resolvers continuing to answer for the realm — if they
                // don't, name lookups into the domain will fail.
                tracing::warn!(
                    realm = %cfg.realm,
                    "Domain joined without AD DNS routing: could not resolve \
                     any DC address; AD-zone resolution now depends on the \
                     box's existing resolvers"
                );
            }

            // Start winbindd now that a machine secret exists — pre-join it
            // has no job and can wedge on name collisions. Restart is
            // idempotent (a no-op if it was never started).
            systemctl("restart", "samba-winbindd.service")
                .await
                .map_err(DomainError::CommandFailed)?;

            // Verify the trust before declaring success.
            run_cmd("wbinfo", &["-t"], &[]).await.map_err(|e| {
                DomainError::CommandFailed(format!(
                    "joined but the trust check failed (wbinfo -t): {e}"
                ))
            })?;

            // Warm winbind before returning: `wbinfo -t` passing means the trust
            // secret is good, but the first identity lookup after join can still
            // fail until winbind has established its DC connection and idmap — a
            // domain user's first SMB access would hit LOGON_FAILURE (smbd can't
            // map the UID). Poll a guaranteed-present principal (the domain
            // Administrator) until full name→uid resolution works, so the first
            // real access doesn't land on a cold cache. Best-effort: a timeout
            // here doesn't fail the join (winbind warms within seconds in
            // practice), it just logs.
            {
                let probe = format!("{}\\administrator", cfg.workgroup);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                loop {
                    if run_cmd("wbinfo", &["-i", &probe], &[]).await.is_ok() {
                        tracing::info!("winbind identity resolution is live");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            "winbind not yet resolving domain identities after join; \
                             the first domain-user access may need a retry"
                        );
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }

            // Register our A record in AD DNS (best-effort — some sites
            // restrict it). `-P` authenticates with the machine account
            // from secrets.tdb (just created by the join); without it net
            // falls back to the ambient user (`<WORKGROUP>\root`) and
            // fails with "Invalid credentials".
            if let Err(e) = run_cmd(
                "net",
                &["ads", "dns", "register", "-P"],
                &[("KRB5_CONFIG", KRB5_CONF_PATH)],
            )
            .await
            {
                tracing::warn!("AD DNS register failed (non-fatal): {e}");
            }

            Self::save_config(&cfg).await?;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            unwind_join_artifacts(
                joined_in_ad.then_some((req.username.as_str(), req.password.as_str())),
            )
            .await;
            return Err(e);
        }

        // smbd reloads pick up the ADS block via the include chain.
        let _ = run_cmd("smbcontrol", &["all", "reload-config"], &[]).await;
        tracing::info!(
            "Joined AD domain {} (workgroup {})",
            cfg.realm,
            cfg.workgroup
        );
        Ok(self.status().await)
    }

    /// Leave the currently-joined Active Directory domain.
    ///
    /// With credentials, contacts a DC to remove the computer object via
    /// `net ads leave`. With `force=true` and no credentials, leaves
    /// locally only — the computer account goes stale in AD (matches
    /// `net ads leave`'s own documented behavior for that case).
    pub async fn leave(&self, req: LeaveDomainRequest) -> Result<(), DomainError> {
        let Some(_cfg) = Self::load_config().await else {
            return Err(DomainError::NotJoined);
        };
        match (&req.username, &req.password, req.force) {
            (Some(user), Some(pass), _) => {
                run_cmd_stdin(
                    "net",
                    &["ads", "leave", "-U", user],
                    &[("KRB5_CONFIG", KRB5_CONF_PATH)],
                    pass,
                )
                .await?;
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
        Self::clear_config().await?;
        let _ = run_cmd("smbcontrol", &["all", "reload-config"], &[]).await;
        tracing::info!("Left AD domain");
        Ok(())
    }

    /// Report current Active Directory domain membership status.
    pub async fn status(&self) -> DomainStatus {
        let Some(cfg) = Self::load_config().await else {
            return DomainStatus {
                joined: false,
                realm: None,
                workgroup: None,
                idmap_base: None,
                trust_ok: None,
                dc_reachable: None,
                clock_skew_seconds: None,
            };
        };
        let trust_ok = Some(run_cmd("wbinfo", &["-t"], &[]).await.is_ok());
        let (dc_reachable, clock_skew_seconds) =
            match run_cmd("net", &["ads", "info"], &[("KRB5_CONFIG", KRB5_CONF_PATH)]).await {
                Ok(out) => {
                    let local = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let skew =
                        parse_net_ads_server_time(&out).and_then(|t| effective_skew(t, local));
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

    /// Search for domain users by name prefix.
    ///
    /// Runs `wbinfo -u`, filters results to match the prefix (case-insensitive),
    /// and returns up to 50 results. The prefix must be at least 2 characters
    /// to prevent bulk enumeration.
    pub async fn search_users(&self, prefix: &str) -> Result<Vec<DomainPrincipal>, DomainError> {
        self.search(prefix, "-u").await
    }

    /// Search for domain groups by name prefix.
    ///
    /// Runs `wbinfo -g`, filters results to match the prefix (case-insensitive),
    /// and returns up to 50 results. The prefix must be at least 2 characters
    /// to prevent bulk enumeration.
    pub async fn search_groups(&self, prefix: &str) -> Result<Vec<DomainPrincipal>, DomainError> {
        self.search(prefix, "-g").await
    }

    /// Perform a principal search with the given wbinfo flag.
    ///
    /// Common code for `search_users` and `search_groups`.
    /// Validates that the system is joined, the prefix is at least 2 characters,
    /// then runs `wbinfo <flag>` and filters the results.
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

    /// Load domain configuration from an arbitrary path if it exists.
    ///
    /// Absence of the file, or unparseable contents, both mean "not joined" —
    /// this never panics on corrupt state.
    pub(crate) async fn load_config_at(path: &Path) -> Option<DomainConfig> {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => serde_json::from_str(&content).ok(),
            Err(_) => None,
        }
    }

    /// Persist domain configuration to an arbitrary path, creating parent dirs.
    pub(crate) async fn save_config_at(
        path: &Path,
        config: &DomainConfig,
    ) -> Result<(), DomainError> {
        let dir = path.parent().unwrap();
        tokio::fs::create_dir_all(dir).await?;
        let json =
            serde_json::to_string(config).map_err(|e| DomainError::Io(std::io::Error::other(e)))?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }

    /// Clear domain configuration at an arbitrary path (leave domain).
    ///
    /// Idempotent: clearing an already-absent config is not an error.
    pub(crate) async fn clear_config_at(path: &Path) -> Result<(), DomainError> {
        match tokio::fs::remove_file(path).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_realm_normalizes_and_accepts_dns_names() {
        assert_eq!(
            validate_realm("corp.example.com").unwrap(),
            "CORP.EXAMPLE.COM"
        );
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
        assert_eq!(
            derive_workgroup("VERYLONGCOMPANYNAME.LAN"),
            "VERYLONGCOMPANY"
        );
    }

    #[test]
    fn validate_idmap_base_rejects_out_of_range() {
        // Must clear every local UID the engine can allocate.
        assert!(validate_idmap_base(3000).is_err());
        assert!(validate_idmap_base(65_535).is_err());
        assert!(validate_idmap_base(65_536).is_ok());
        assert!(validate_idmap_base(DEFAULT_IDMAP_BASE).is_ok());
        // Must not let `base + IDMAP_RANGE_SPAN - 1` overflow u32 in the renderer.
        assert!(validate_idmap_base(u32::MAX - 100).is_err());
    }

    #[tokio::test]
    async fn config_round_trips_and_clear_means_not_joined() {
        let dir = std::env::temp_dir().join(format!("nasty-domain-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.json");
        // Absent file == not joined.
        assert!(DomainService::load_config_at(&path).await.is_none());
        let cfg = DomainConfig {
            realm: "CORP.EXAMPLE.COM".into(),
            workgroup: "CORP".into(),
            idmap_base: 100_000,
        };
        // Save creates the parent dir and the file.
        DomainService::save_config_at(&path, &cfg)
            .await
            .expect("save");
        let loaded = DomainService::load_config_at(&path).await.expect("loaded");
        assert_eq!(loaded.realm, "CORP.EXAMPLE.COM");
        assert_eq!(loaded.workgroup, "CORP");
        assert_eq!(loaded.idmap_base, 100_000);
        // Corrupt JSON degrades to "not joined", never panics.
        tokio::fs::write(&path, b"{not json").await.unwrap();
        assert!(DomainService::load_config_at(&path).await.is_none());
        // Clear is idempotent.
        DomainService::save_config_at(&path, &cfg)
            .await
            .expect("save again");
        DomainService::clear_config_at(&path).await.expect("clear");
        assert!(DomainService::load_config_at(&path).await.is_none());
        DomainService::clear_config_at(&path)
            .await
            .expect("second clear: no panic"); // idempotent
        std::fs::remove_dir_all(&dir).ok();
    }

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
        assert!(
            conf.contains("idmap config CORP : range = 100000-999999"),
            "{conf}"
        );
        // The default (*) range must not overlap the domain range.
        assert!(
            conf.contains("idmap config * : range = 65000-65535"),
            "{conf}"
        );
        // DC outage tolerance for recently-seen users.
        assert!(conf.contains("winbind offline logon = yes"), "{conf}");
        // Explicit namespaces — never ambiguous with local users.
        assert!(!conf.contains("winbind use default domain"), "{conf}");
        assert!(
            conf.contains("kerberos method = secrets and keytab"),
            "{conf}"
        );
    }

    #[test]
    fn render_krb5_conf_pins_realm_and_dns_lookup() {
        let conf = render_krb5_conf("CORP.EXAMPLE.COM");
        assert!(conf.contains("default_realm = CORP.EXAMPLE.COM"), "{conf}");
        // DCs are found via DNS SRV — no static kdc lines to go stale.
        assert!(conf.contains("dns_lookup_kdc = true"), "{conf}");
        assert!(conf.contains("rdns = false"), "{conf}");
    }

    #[test]
    fn parse_resolvectl_srv_extracts_targets() {
        // resolvectl output shape: "_ldap._tcp.corp.example.com IN SRV 0 100 389 dc1.corp.example.com"
        let out = "\
_ldap._tcp.corp.example.com IN SRV 0 100 389 dc1.corp.example.com\n\
_ldap._tcp.corp.example.com IN SRV 0 100 389 dc2.corp.example.com\n\n\
-- Information acquired via protocol DNS in 2.1ms.\n";
        assert_eq!(
            parse_resolvectl_srv(out),
            vec![
                "dc1.corp.example.com".to_string(),
                "dc2.corp.example.com".to_string()
            ]
        );
        assert!(parse_resolvectl_srv("-- no data --\n").is_empty());

        // Real resolvectl appends link annotations — the target is the
        // 4th RDATA token after "SRV" (prio weight port target), NOT
        // the last token on the line (caught live by the ad-member VM
        // test: the old parser returned "eth1" as a DC hostname).
        let annotated = "\
_ldap._tcp.nasty.test IN SRV 0 100 389 dc.nasty.test. -- link: eth1\n\n\
-- Information acquired via protocol DNS in 1.2ms.\n";
        assert_eq!(
            parse_resolvectl_srv(annotated),
            vec!["dc.nasty.test".to_string()]
        );
    }

    #[test]
    fn parse_resolvectl_addresses_extracts_ips() {
        let out = "dc1.corp.example.com: 10.0.0.5\n\n-- Information acquired via protocol DNS in 1.2ms.\n";
        assert_eq!(
            parse_resolvectl_addresses(out),
            vec!["10.0.0.5".to_string()]
        );

        let annotated = "dc.nasty.test: 192.168.1.1                  -- link: eth1\n";
        assert_eq!(
            parse_resolvectl_addresses(annotated),
            vec!["192.168.1.1".to_string()]
        );
    }

    #[test]
    fn render_resolved_dropin_routes_realm_to_dcs() {
        let conf =
            render_resolved_dropin("CORP.EXAMPLE.COM", &["10.0.0.5".into(), "10.0.0.6".into()]);
        assert!(conf.contains("[Resolve]"), "{conf}");
        assert!(conf.contains("DNS=10.0.0.5 10.0.0.6"), "{conf}");
        // Routing domain (~) — only AD-zone queries go to the DCs.
        assert!(conf.contains("Domains=~corp.example.com"), "{conf}");
    }

    #[test]
    fn parse_net_ads_server_time_reads_rfc2822_style() {
        // `net ads info` prints e.g. "Server time: Tue, 07 Jul 2026 12:34:56 UTC"
        let out = "LDAP server: 10.0.0.5\nServer time: Tue, 07 Jul 2026 12:34:56 UTC\n";
        // 2026-07-07T12:34:56Z — verified via:
        // `date -u -j -f "%Y-%m-%d %H:%M:%S" "2026-07-07 12:34:56" +%s` => 1783427696
        assert_eq!(parse_net_ads_server_time(out), Some(1783427696));
        assert_eq!(parse_net_ads_server_time("no time here"), None);
    }

    #[test]
    fn parse_net_ads_workgroup_reads_name() {
        assert_eq!(
            parse_net_ads_workgroup("Workgroup: NASTYAD\n").as_deref(),
            Some("NASTYAD")
        );
        assert_eq!(parse_net_ads_workgroup("no workgroup line"), None);
    }

    #[test]
    fn effective_skew_ignores_sentinel_server_times() {
        let now = 1_783_517_309;
        // Epoch-zero sentinel from an anonymous net ads info query.
        assert_eq!(effective_skew(0, now), None);
        // Real times produce signed skew.
        assert_eq!(effective_skew(now + 120, now), Some(120));
        assert_eq!(effective_skew(now - 300, now), Some(-300));
    }

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
}
