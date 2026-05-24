use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// ── ACME cert status (global, in-memory) ─────────────────────

static ACME_STATUS: std::sync::OnceLock<std::sync::Mutex<AcmeStatus>> = std::sync::OnceLock::new();

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AcmeStatus {
    /// "idle", "running", "success", "error"
    pub state: String,
    /// Human-readable message (error details, progress info)
    pub message: String,
    /// Domain the cert is for
    pub domain: Option<String>,
    /// When the cert expires, if known
    pub expires: Option<String>,
    /// When the cert was issued, if known
    pub issued: Option<String>,
    /// Certificate issuer (e.g. "Let's Encrypt")
    pub issuer: Option<String>,
    /// When the last attempt was made
    pub last_attempt: Option<String>,
}

impl Default for AcmeStatus {
    fn default() -> Self {
        Self {
            state: "idle".into(),
            message: String::new(),
            domain: None,
            expires: None,
            issued: None,
            issuer: None,
            last_attempt: None,
        }
    }
}

fn set_acme_status(state: &str, message: &str, domain: Option<&str>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    let status = ACME_STATUS.get_or_init(|| std::sync::Mutex::new(AcmeStatus::default()));
    if let Ok(mut s) = status.lock() {
        s.state = state.to_string();
        s.message = message.to_string();
        if let Some(d) = domain {
            s.domain = Some(d.to_string());
        }
        s.last_attempt = Some(now);
    }
}

pub fn get_acme_status() -> AcmeStatus {
    let status = ACME_STATUS.get_or_init(|| std::sync::Mutex::new(AcmeStatus::default()));
    status.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn reset_acme_status() {
    set_acme_status("idle", "", None);
}

/// Re-apply the Caddy TLS automation policy. Useful after a transient
/// failure (Caddy briefly down, ACME server flaking, etc.) — the user
/// can hit "retry" in the WebUI and we push the same policies again,
/// which kicks Caddy's issuance state machine back into action.
pub async fn retry_acme() -> Result<(), String> {
    let settings = load().await;
    let app_subdomains = load_app_subdomains().await;
    tokio::spawn(async move {
        if let Err(e) = apply_caddy_tls_with_apps(&settings, &app_subdomains).await {
            warn!("Caddy TLS reapply failed: {e}");
        }
    });
    Ok(())
}

/// Fire-and-forget TLS automation reapply. Called by router-level
/// ingress mutations (set / remove with a subdomain) so a newly-added
/// hostname gets a cert immediately, and a removed one stops being
/// renewed. Cheap when nothing changed — Caddy compares the PUT body
/// against the running config and no-ops if identical.
///
/// Best-effort: failures log a warning. The next user-triggered TLS
/// action (settings change, retry button) reconverges.
pub async fn reapply_tls_from_disk() {
    // Wait for Caddy's admin API before pushing the policy set. Engine
    // startup spawns this concurrently with the rest of restore, and on
    // a fresh boot Caddy can still be initialising when we get here.
    // main.rs already does a wait_ready before apps.reconcile_app_routes,
    // but if that timed out (slow Caddy, journald wedged, etc.) calling
    // set_tls_automation here would fail silently with no retry until
    // the next engine restart — leaving the box without the policy set
    // it should have on disk-state. Re-arming the wait_ready locally
    // means a Caddy that needed more than 30 s still gets its config.
    let api = nasty_apps::caddy::CaddyApi::new();
    if let Err(e) = api.wait_ready(30).await {
        warn!("Caddy admin API not ready ({e}); skipping TLS reapply at startup");
        return;
    }
    let settings = load().await;
    let app_subdomains = load_app_subdomains().await;
    if let Err(e) = apply_caddy_tls_with_apps(&settings, &app_subdomains).await {
        warn!("Caddy TLS reapply failed: {e}");
    }
}

/// Caddy's internal-CA root certificate. Lives in Caddy's state dir
/// alongside the issued certs; we return its PEM so the WebUI can offer
/// it as a download, letting operators import the cert into their
/// OS/browser trust store and have every NASty box that uses
/// `tls internal` (the default fallback) be trusted without per-host
/// security warnings.
///
/// Returns the PEM string. None on read failure (Caddy not yet
/// started, file permissions, no internal CA bootstrapped). Caller
/// surfaces None as a 404-ish "not available yet" to the WebUI.
pub async fn read_caddy_local_ca_root() -> Option<String> {
    const ROOT_PATH: &str = "/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt";
    tokio::fs::read_to_string(ROOT_PATH).await.ok()
}

/// Walk `/var/lib/nasty/apps/*.json` and collect every non-empty
/// `ingress_subdomain` value. Used by [`retry_acme`] and the
/// settings-change path so the TLS policy set always reflects every
/// hostname Caddy needs to serve, not just the main domain.
///
/// nasty-system reads these manifests as raw JSON (rather than via
/// the nasty-apps type) to keep the dep direction one-way: apps
/// already depends on system via the AppRoute / CaddyApi flow, and
/// pulling apps's typed reader in would close that loop. Best-effort:
/// any read or parse failure is logged at debug and skipped — a
/// malformed app.json shouldn't block the main-domain cert refresh.
async fn load_app_subdomains() -> Vec<String> {
    let dir = "/var/lib/nasty/apps";
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(d) => d,
        Err(_) => return out,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(host) = v.get("ingress_subdomain").and_then(|s| s.as_str())
            && !host.trim().is_empty()
        {
            out.push(host.trim().to_string());
        }
    }
    out
}

const STATE_PATH: &str = "/var/lib/nasty/settings.json";
const STATE_DIR: &str = "/var/lib/nasty";

// Caddy reads DNS-01 provider credentials from this EnvironmentFile (the
// caddy.service unit references it). One KEY=VAL per line. We still
// write this even though the rest of the TLS pipeline moved to the
// admin API — Caddy resolves `{env.X}` references in admin-pushed
// config from the process env, which is populated from this file at
// service start. No way to push secrets directly through the admin API
// without baking them into the JSON, which would leak them into the
// Caddy storage on disk.
const CADDY_ACME_ENV_PATH: &str = "/var/lib/nasty/caddy/acme.env";

/// Display unit for temperatures rendered in the WebUI. Internal storage
/// and alert thresholds are always in Celsius; this is presentational only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TempUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Settings {
    /// IANA timezone string applied to the system (e.g. `UTC`, `America/New_York`).
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// System hostname.
    pub hostname: Option<String>,
    /// Whether to display clocks in 24-hour format.
    #[serde(default = "default_clock_24h")]
    pub clock_24h: bool,
    /// Unit for displayed temperatures (CPU, disks, alert thresholds).
    /// Storage and alert evaluation always use Celsius internally — this
    /// only affects rendering in the WebUI.
    #[serde(default)]
    pub temp_unit: TempUnit,
    /// Domain name for Let's Encrypt TLS (e.g. "nasty.example.com"). Empty = self-signed.
    #[serde(default)]
    pub tls_domain: Option<String>,
    /// Email address for Let's Encrypt ACME notifications.
    #[serde(default)]
    pub tls_acme_email: Option<String>,
    /// Whether Let's Encrypt is enabled. Requires tls_domain and tls_acme_email.
    #[serde(default)]
    pub tls_acme_enabled: bool,
    /// ACME challenge type. Caddy's built-in ACME issuer handles all three:
    ///   - "tls-alpn"  → TLS-ALPN-01 (port 443)
    ///   - "http"      → HTTP-01 (port 80)
    ///   - "dns"       → DNS-01 via a DNS-provider plugin compiled into Caddy
    #[serde(default = "default_challenge_type")]
    pub tls_challenge_type: String,
    /// DNS provider code for DNS-01 challenge (e.g. "cloudflare", "route53").
    /// Must match a DNS plugin compiled into the Caddy binary.
    #[serde(default)]
    pub tls_dns_provider: Option<String>,
    /// DNS provider API credentials as KEY=VALUE lines.
    /// Written to a Caddy `EnvironmentFile` and referenced from the
    /// generated `tls` block via `{env.KEY}` placeholders.
    #[serde(default)]
    pub tls_dns_credentials: Option<String>,
    /// Use Let's Encrypt staging environment (for testing, avoids rate limits).
    #[serde(default)]
    pub tls_acme_staging: bool,
    /// External DNS resolvers (comma-separated) to use when verifying
    /// TXT-record propagation during DNS-01. Defaults to "1.1.1.1,8.8.8.8".
    /// Set this when the box's authoritative DNS isn't reachable via
    /// public resolvers (split-horizon zones, air-gapped networks).
    /// Empty / None ⇒ use the default.
    #[serde(default)]
    pub tls_dns_resolver: Option<String>,
    /// Seconds to wait after creating the TXT record before checking
    /// propagation. Defaults to 30. The recursive resolvers we use to
    /// verify propagation often have a long negative TTL on the
    /// `_acme-challenge.X` name (cached NXDOMAIN from prior lookups);
    /// without a wait, Caddy queries immediately, sees the cached
    /// answer, and the timer-based propagation check times out before
    /// the cache flushes. Bump this when issuance still times out
    /// after several minutes (slow DNS providers, long SOA MINIMUM
    /// TTL on the parent zone).
    #[serde(default)]
    pub tls_dns_propagation_wait: Option<u32>,
    /// Whether anonymous telemetry is enabled (drive count, storage capacity).
    #[serde(default = "default_telemetry_enabled")]
    pub telemetry_enabled: bool,
    /// OpenID Connect single-sign-on configuration. Disabled by default.
    #[serde(default)]
    pub oidc: OidcSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OidcSettings {
    /// Master switch — when false, OIDC endpoints return 404 and no IdP traffic occurs.
    #[serde(default)]
    pub enabled: bool,
    /// IdP issuer URL (used for OIDC discovery, e.g. "https://auth.example.com").
    #[serde(default)]
    pub issuer_url: Option<String>,
    /// OAuth client_id registered with the IdP.
    #[serde(default)]
    pub client_id: Option<String>,
    /// OAuth client_secret. Returned as a placeholder over RPC; only the engine sees the real value.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Absolute redirect URI registered with the IdP (e.g. "https://nasty.local/api/auth/oidc/callback").
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// OAuth scopes to request. Defaults to ["openid","profile","email","groups"].
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
    /// Name of the ID-token claim that carries the user's groups.
    #[serde(default = "default_oidc_groups_claim")]
    pub groups_claim: String,
    /// Group → role mappings. Evaluated in order; first match wins.
    #[serde(default)]
    pub role_mappings: Vec<OidcRoleMapping>,
    /// Role applied when no group mapping matches. None = deny login.
    #[serde(default)]
    pub default_role: Option<String>,
    /// When true, unknown OIDC subjects are auto-provisioned as local users on first login.
    #[serde(default = "default_true")]
    pub auto_provision: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OidcRoleMapping {
    /// Group name (matched verbatim against entries in the configured groups claim).
    pub group: String,
    /// NASty role to assign: "admin", "operator", or "readonly".
    pub role: String,
}

impl Default for OidcSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer_url: None,
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: default_oidc_scopes(),
            groups_claim: default_oidc_groups_claim(),
            role_mappings: Vec::new(),
            default_role: None,
            auto_provision: true,
        }
    }
}

/// Sentinel returned in place of the OIDC client_secret over the API. When a
/// caller sends this back unchanged, the engine keeps the stored secret.
pub const OIDC_SECRET_PLACEHOLDER: &str = "<unchanged>";

/// Replace the client_secret on a copy of OidcSettings with `<set>` / `<unset>`,
/// suitable for returning to API callers without leaking the real value.
pub fn redact_oidc_secret(mut s: OidcSettings) -> OidcSettings {
    s.client_secret = match s.client_secret.as_deref() {
        Some(v) if !v.is_empty() => Some("<set>".into()),
        _ => Some("<unset>".into()),
    };
    s
}

fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".into(),
        "profile".into(),
        "email".into(),
        "groups".into(),
    ]
}

fn default_oidc_groups_claim() -> String {
    "groups".to_string()
}

fn default_true() -> bool {
    true
}

fn default_challenge_type() -> String {
    "tls-alpn".to_string()
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_clock_24h() -> bool {
    true
}

fn default_telemetry_enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            hostname: None,
            clock_24h: default_clock_24h(),
            temp_unit: TempUnit::default(),
            tls_domain: None,
            tls_acme_email: None,
            tls_acme_enabled: false,
            tls_challenge_type: default_challenge_type(),
            tls_dns_provider: None,
            tls_dns_credentials: None,
            tls_acme_staging: false,
            tls_dns_resolver: None,
            tls_dns_propagation_wait: None,
            telemetry_enabled: default_telemetry_enabled(),
            oidc: OidcSettings::default(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SettingsUpdate {
    /// New IANA timezone to apply (optional).
    pub timezone: Option<String>,
    /// New hostname to set (optional).
    pub hostname: Option<String>,
    /// Whether to use 24-hour clock display (optional).
    pub clock_24h: Option<bool>,
    /// Display unit for temperatures (optional).
    pub temp_unit: Option<TempUnit>,
    /// Domain name for Let's Encrypt TLS (set to empty string to disable).
    pub tls_domain: Option<String>,
    /// Email address for ACME notifications.
    pub tls_acme_email: Option<String>,
    /// Enable/disable Let's Encrypt.
    pub tls_acme_enabled: Option<bool>,
    /// Challenge type: "tls-alpn" or "dns".
    pub tls_challenge_type: Option<String>,
    /// DNS provider code.
    pub tls_dns_provider: Option<String>,
    /// DNS API credentials (KEY=VALUE per line).
    pub tls_dns_credentials: Option<String>,
    /// Use staging environment.
    pub tls_acme_staging: Option<bool>,
    /// External DNS resolvers (comma-separated). Empty string clears.
    pub tls_dns_resolver: Option<String>,
    /// Propagation wait in seconds. 0 clears (engine treats as default).
    pub tls_dns_propagation_wait: Option<u32>,
    /// Enable/disable anonymous telemetry.
    pub telemetry_enabled: Option<bool>,
}

pub struct SettingsService {
    state: Arc<RwLock<Settings>>,
}

impl SettingsService {
    pub async fn new() -> Self {
        let mut settings = load().await;
        // Seed hostname from the running system if not yet persisted.
        // This picks up whatever the installer configured (networking.hostName)
        // so the settings page shows the real hostname from day one.
        if settings.hostname.is_none()
            && let Ok(name) = tokio::fs::read_to_string("/proc/sys/kernel/hostname").await
        {
            let name = name.trim().to_string();
            if !name.is_empty() {
                settings.hostname = Some(name);
                let _ = save(&settings).await;
            }
        }
        Self {
            state: Arc::new(RwLock::new(settings)),
        }
    }

    pub async fn get(&self) -> Settings {
        self.state.read().await.clone()
    }

    /// Replace the OIDC configuration. If `incoming.client_secret` is the literal
    /// placeholder `"<unchanged>"`, the previously-stored secret is preserved.
    pub async fn set_oidc(&self, mut incoming: OidcSettings) -> Result<OidcSettings, String> {
        let mut settings = self.state.write().await;
        if incoming.client_secret.as_deref() == Some(OIDC_SECRET_PLACEHOLDER) {
            incoming.client_secret = settings.oidc.client_secret.clone();
        }
        if incoming.scopes.is_empty() {
            incoming.scopes = default_oidc_scopes();
        }
        settings.oidc = incoming.clone();
        save(&settings).await.map_err(|e| e.to_string())?;
        Ok(redact_oidc_secret(incoming))
    }

    pub async fn update(&self, update: SettingsUpdate) -> Result<Settings, String> {
        let mut settings = self.state.write().await;
        if let Some(tz) = update.timezone {
            apply_timezone(&tz).await?;
            settings.timezone = tz;
        }
        if let Some(name) = update.hostname {
            apply_hostname(&name).await?;
            settings.hostname = Some(name);
        }
        if let Some(h24) = update.clock_24h {
            settings.clock_24h = h24;
        }
        if let Some(unit) = update.temp_unit {
            settings.temp_unit = unit;
        }
        let mut tls_changed = false;
        if let Some(domain) = update.tls_domain {
            let domain = if domain.trim().is_empty() {
                None
            } else {
                Some(domain.trim().to_string())
            };
            if settings.tls_domain != domain {
                settings.tls_domain = domain;
                tls_changed = true;
            }
        }
        if let Some(email) = update.tls_acme_email {
            let email = if email.trim().is_empty() {
                None
            } else {
                Some(email.trim().to_string())
            };
            if settings.tls_acme_email != email {
                settings.tls_acme_email = email;
                tls_changed = true;
            }
        }
        if let Some(enabled) = update.tls_acme_enabled
            && settings.tls_acme_enabled != enabled
        {
            settings.tls_acme_enabled = enabled;
            tls_changed = true;
        }
        if let Some(ct) = update.tls_challenge_type
            && settings.tls_challenge_type != ct
        {
            settings.tls_challenge_type = ct;
            tls_changed = true;
        }
        if let Some(provider) = update.tls_dns_provider {
            let provider = if provider.trim().is_empty() {
                None
            } else {
                Some(provider.trim().to_string())
            };
            if settings.tls_dns_provider != provider {
                settings.tls_dns_provider = provider;
                tls_changed = true;
            }
        }
        if let Some(creds) = update.tls_dns_credentials {
            let creds = if creds.trim().is_empty() {
                None
            } else {
                Some(creds.trim().to_string())
            };
            if settings.tls_dns_credentials != creds {
                settings.tls_dns_credentials = creds;
                tls_changed = true;
            }
        }
        if let Some(staging) = update.tls_acme_staging
            && settings.tls_acme_staging != staging
        {
            settings.tls_acme_staging = staging;
            tls_changed = true;
        }
        if let Some(resolver) = update.tls_dns_resolver {
            let resolver = if resolver.trim().is_empty() {
                None
            } else {
                Some(resolver.trim().to_string())
            };
            if settings.tls_dns_resolver != resolver {
                settings.tls_dns_resolver = resolver;
                tls_changed = true;
            }
        }
        if let Some(wait) = update.tls_dns_propagation_wait {
            // 0 ⇒ clear (engine falls back to the default 30s). Allows
            // the WebUI form to express "use default" without needing a
            // separate null/clear field.
            let wait = if wait == 0 { None } else { Some(wait) };
            if settings.tls_dns_propagation_wait != wait {
                settings.tls_dns_propagation_wait = wait;
                tls_changed = true;
            }
        }
        if let Some(telemetry) = update.telemetry_enabled {
            settings.telemetry_enabled = telemetry;
        }
        save(&settings).await.map_err(|e| e.to_string())?;
        if tls_changed {
            // Always re-apply on a TLS change — even when ACME is now
            // disabled, the automation policy must be cleared so Caddy
            // stops renewing certs we no longer want.
            let s = settings.clone();
            let app_subdomains = load_app_subdomains().await;
            tokio::spawn(async move {
                if let Err(e) = apply_caddy_tls_with_apps(&s, &app_subdomains).await {
                    warn!("Caddy TLS reapply failed: {e}");
                }
            });
        }
        Ok(settings.clone())
    }
}

pub async fn list_timezones() -> Result<Vec<String>, String> {
    let output = tokio::process::Command::new("timedatectl")
        .args(["list-timezones"])
        .output()
        .await
        .map_err(|e| format!("timedatectl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|s| s.to_string()).collect())
}

async fn apply_hostname(name: &str) -> Result<(), String> {
    // NixOS has /etc as read-only — set the kernel hostname via /proc.
    // Persistence is via /var/lib/nasty/settings.json, read at boot by
    // nasty-apply-hostname.service.
    tokio::fs::write("/proc/sys/kernel/hostname", name.as_bytes())
        .await
        .map_err(|e| format!("failed to set kernel hostname: {e}"))?;

    // Also expose the name to the wrapper flake so `nixos-rebuild
    // switch` (which defaults to looking up `nixosConfigurations.<kernel-hostname>`)
    // resolves to our system. The flake at /etc/nixos/flake.nix imports
    // ./hostname.nix when present and falls back to "nasty" otherwise,
    // so writing this file is best-effort — failures are logged but
    // don't fail the apply (e.g. fresh installs before rebootstrap, or
    // if /etc/nixos isn't writable for some reason).
    write_hostname_nix(name).await;

    Ok(())
}

/// Write `/etc/nixos/hostname.nix` with the current hostname as a Nix
/// string literal. Read by the wrapper flake to alias
/// `nixosConfigurations.<hostname>` to the same system attr as `nasty`.
async fn write_hostname_nix(name: &str) {
    let nixos_dir = std::path::Path::new("/etc/nixos");
    if !nixos_dir.exists() {
        // Fresh install before rebootstrap, or running outside a normal
        // NixOS layout (tests). Nothing to do.
        return;
    }
    let path = nixos_dir.join("hostname.nix");
    let content = format!("{}\n", to_nix_string(name));
    if let Err(e) = tokio::fs::write(&path, content).await {
        warn!("could not write {}: {e}", path.display());
    }
}

/// Render a Rust string as a Nix double-quoted string literal, escaping
/// the characters that have special meaning inside `"..."`. The hostname
/// has been validated upstream (RFC1123-ish), but escape defensively so
/// any future relaxation can't smuggle Nix syntax into the flake.
fn to_nix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

async fn apply_timezone(tz: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("timedatectl")
        .args(["set-timezone", tz])
        .output()
        .await
        .map_err(|e| format!("timedatectl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("failed to set timezone: {stderr}"));
    }
    Ok(())
}

async fn load() -> Settings {
    nasty_common::load_singleton_or_recover(STATE_PATH).await
}

async fn save(settings: &Settings) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(settings).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    // Contains OIDC client_secret and ACME/DNS provider config.
    tokio::fs::set_permissions(STATE_PATH, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

// ── Caddy-driven ACME ────────────────────────────────────────
//
// Caddy's built-in ACME issuer handles HTTP-01, TLS-ALPN-01 and
// (with the right plugin compiled in) DNS-01 — including renewals,
// retries, OCSP stapling, and the full issuance state machine.
//
// The engine's job here is reduced to:
//   1. Write the user's DNS-provider credentials to the
//      `CADDY_ACME_ENV_PATH` EnvironmentFile so Caddy can resolve
//      `{env.X}` references in admin-pushed config.
//   2. Push a `tls.automation.policies[]` block to Caddy's admin API
//      describing every hostname we want a cert for (main domain +
//      per-app subdomains). Caddy issues + renews + staples.
//   3. Read back what Caddy ends up serving so the WebUI can show
//      issuer / expiry / status.
//
// Caddy stores ACME-issued certs under
// `/var/lib/caddy/.local/share/caddy/certificates/<endpoint>/<sans>/<sans>.crt`,
// where `<endpoint>` is the ACME directory URL with `/` → `-` (e.g.
// `acme-v02.api.letsencrypt.org-directory`). We resolve the actual
// path lazily by globbing — relying on the exact URL-encoding rules
// would be brittle across Caddy versions.

const CADDY_DATA_DIR: &str = "/var/lib/caddy";

/// Per-app subdomain ingresses the engine knows about, used to extend
/// the TLS automation policy beyond just the main `tls_domain`.
/// Sourced by the engine binary (it has the apps service) and passed
/// to [`apply_caddy_tls_with_apps`]. Settings code stays decoupled
/// from the apps crate's storage layer.
pub type AppSubdomains = Vec<String>;

/// Render the Caddy `EnvironmentFile` content from `tls_dns_credentials`.
/// Empty when ACME-DNS is off — the file is recreated empty so a stale
/// value from a previous provider doesn't leak into the next run.
///
/// The env vars referenced from `dns_provider_json` in nasty-apps' caddy
/// module (e.g. `CLOUDFLARE_DNS_API_TOKEN`) must be present in this file
/// for Caddy to resolve them at request time. We trust the operator to
/// write KEY=VAL lines matching what the chosen provider expects.
fn caddy_acme_env(settings: &Settings) -> String {
    if !settings.tls_acme_enabled
        || settings.tls_challenge_type != "dns"
        || settings.tls_dns_credentials.is_none()
    {
        return String::new();
    }
    settings
        .tls_dns_credentials
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string()
        + "\n"
}

/// Apply current TLS settings to Caddy via the admin API: write the
/// EnvironmentFile (so Caddy can resolve `{env.X}` placeholders), then
/// push the automation policy set.
///
/// `app_subdomains` is the list of additional hostnames per-app ingress
/// has registered. Empty when the caller is settings-only (the engine
/// binary will pass the real list during startup reconcile and on
/// ingress mutations).
///
/// File-vs-admin-API split: secrets stay file-based because pushing
/// them through the admin API would land them in `/var/lib/caddy/...`
/// alongside the cert storage. EnvironmentFile is the standard
/// systemd-managed secrets path; mode 0640 + group caddy keeps it out
/// of unprivileged hands.
pub async fn apply_caddy_tls(settings: &Settings) -> Result<(), String> {
    apply_caddy_tls_with_apps(settings, &[]).await
}

/// Variant of [`apply_caddy_tls`] that takes the per-app subdomain list
/// explicitly. The engine binary knows about apps; settings doesn't, so
/// we keep the dep direction clean by funnelling everything through
/// this helper.
pub async fn apply_caddy_tls_with_apps(
    settings: &Settings,
    app_subdomains: &[String],
) -> Result<(), String> {
    let domain_for_status = settings.tls_domain.clone();

    if settings.tls_acme_enabled {
        set_acme_status(
            "running",
            "Applying TLS automation via Caddy admin API...",
            domain_for_status.as_deref(),
        );
    }

    // EnvironmentFile lives next to the (now-vestigial) caddy state
    // dir. Owned by root:caddy so the unit can read it.
    if let Some(parent) = std::path::Path::new(CADDY_ACME_ENV_PATH).parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        warn!("create_dir_all({}) failed: {e}", parent.display());
    }

    let env_content = caddy_acme_env(settings);
    tokio::fs::write(CADDY_ACME_ENV_PATH, &env_content)
        .await
        .map_err(|e| format!("write {CADDY_ACME_ENV_PATH}: {e}"))?;
    if let Err(e) =
        tokio::fs::set_permissions(CADDY_ACME_ENV_PATH, std::fs::Permissions::from_mode(0o640))
            .await
    {
        warn!("chmod 640 {CADDY_ACME_ENV_PATH} failed: {e}");
    }
    nasty_common::cmd::try_run("chown", &["root:caddy", CADDY_ACME_ENV_PATH]).await;

    // Decide whether to restart Caddy. The naive check (did the file
    // content change?) is wrong: an earlier deploy may have written
    // the file *after* Caddy had already started reading an empty
    // version, leaving the file content correct but Caddy's process
    // env empty. The file content is now identical to what we'd
    // write, change-detection says "skip", and every `{env.X}` in our
    // pushed config resolves to "" — exactly what was happening in
    // production.
    //
    // Direct check: read Caddy's /proc/<pid>/environ and see whether
    // every KEY from acme.env is actually loaded into the running
    // process. Missing any key ⇒ restart. Engine runs as root so it
    // can read /proc/<other-uid-pid>/environ without issue.
    let needs_restart = settings.tls_acme_enabled
        && settings.tls_challenge_type == "dns"
        && !caddy_environ_has_keys_from(&env_content).await;
    if needs_restart {
        let restart = tokio::process::Command::new("systemctl")
            .args(["restart", "caddy"])
            .output()
            .await
            .map_err(|e| {
                let m = format!("systemctl restart caddy: spawn failed: {e}");
                warn!("{m}");
                m
            })?;
        if !restart.status.success() {
            let stderr = String::from_utf8_lossy(&restart.stderr).to_string();
            let msg = format!("caddy restart failed: {stderr}");
            warn!("{msg}");
            set_acme_status("error", &msg, domain_for_status.as_deref());
            return Err(msg);
        }
        info!("caddy restarted (EnvironmentFile refreshed)");
        // Wait for admin API to come back up before pushing config.
        let api = nasty_apps::caddy::CaddyApi::new();
        if let Err(e) = api.wait_ready(30).await {
            warn!("caddy admin API not ready after restart: {e}");
            set_acme_status("error", &e, domain_for_status.as_deref());
            return Err(e);
        }
    }

    // Build the desired policy set: main domain (when ACME enabled) +
    // every app subdomain. Empty ⇒ Caddy clears automation and falls
    // back to the static-cert :443 block.
    let policies = build_policy_set(settings, app_subdomains);
    let issuer = nasty_apps::caddy::TlsIssuer {
        email: settings.tls_acme_email.clone(),
        dns_provider: if settings.tls_acme_enabled && settings.tls_challenge_type == "dns" {
            settings.tls_dns_provider.clone()
        } else {
            None
        },
        staging: settings.tls_acme_staging,
        dns_resolvers: settings
            .tls_dns_resolver
            .as_deref()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty()),
        dns_propagation_wait_secs: settings.tls_dns_propagation_wait,
    };

    // Local IP SANs for the internal-CA fallback policy. Adding the
    // box's reachable IPs to the cert means `https://10.x.x.x` and
    // `https://[fd00::1]` don't trip a browser CN-mismatch warning
    // — only the "self-signed CA" warning remains, which goes away
    // once the operator imports Caddy's root via the TLS page's
    // existing "download CA root" button. Refreshed every time TLS
    // is reapplied (engine startup, settings change, and after every
    // network apply via the hook in network.rs::apply_config).
    let extra_subjects = crate::network::local_tls_subjects().await;
    let api = nasty_apps::caddy::CaddyApi::new();
    if let Err(e) = api
        .set_tls_automation(&policies, &issuer, &extra_subjects)
        .await
    {
        warn!("caddy: set_tls_automation failed: {e}");
        set_acme_status("error", &e, domain_for_status.as_deref());
        return Err(e);
    }

    refresh_acme_status_from_disk(settings).await;

    // Caddy issues asynchronously. The status set above will be
    // "running" / "Waiting for Caddy to obtain a certificate..." until
    // the file lands on disk — which could be seconds (HTTP-01 from a
    // public IP) or a few minutes (DNS-01 with our 30s propagation
    // delay + ACME finalize round-trip). Without a follow-up poll the
    // WebUI would stay on the "Provisioning" badge forever; users
    // would have to refresh the engine to see "success".
    //
    // Spawn a polling task that re-runs refresh_acme_status_from_disk
    // until the cert appears or we hit a 5-minute cap. Cheap (a few
    // statx calls per tick) and the engine doesn't outlive Caddy's
    // issuance window in practice. Only spawn when ACME is on and a
    // managed cert is expected; otherwise there's nothing to wait for.
    if settings.tls_acme_enabled && !policies.is_empty() {
        let settings_for_poll = settings.clone();
        tokio::spawn(async move {
            poll_until_issued(&settings_for_poll).await;
        });
    }

    Ok(())
}

/// Re-run `refresh_acme_status_from_disk` on a fixed cadence until the
/// status flips to `success` or `MAX_POLL_SECS` elapses. Called from
/// `apply_caddy_tls_with_apps` after a successful policy push so the
/// WebUI's "Provisioning" badge transitions to "Active" without the
/// user having to refresh or restart the engine.
///
/// Cadence is 5s — Caddy DNS-01 takes ~30s minimum (our propagation
/// delay) so we'd never catch a sub-5s issuance anyway, and a tighter
/// poll would just spin against an empty cert dir.
async fn poll_until_issued(settings: &Settings) {
    const MAX_POLL_SECS: u64 = 300;
    const POLL_INTERVAL_SECS: u64 = 5;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(MAX_POLL_SECS);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        refresh_acme_status_from_disk(settings).await;
        if get_acme_status().state == "success" {
            return;
        }
        if std::time::Instant::now() >= deadline {
            // Don't escalate to "error" — Caddy is still retrying in the
            // background and might succeed minutes later. Leave the
            // status as-is so the operator can see whatever Caddy's
            // last reported state was; the next settings change or
            // retry_acme call will pick it up.
            warn!(
                "acme: cert not issued within {MAX_POLL_SECS}s; leaving \
                 status as-is. Caddy will keep retrying in the background."
            );
            return;
        }
    }
}

/// Verify that every `KEY=` line in `env_content` has a matching key
/// in the running Caddy process's environment. Returns `false` if any
/// expected key is missing — caller should restart Caddy to refresh
/// the EnvironmentFile.
///
/// `false` is also returned when Caddy isn't running (no MainPID, or
/// /proc/<pid>/environ unreadable) so the caller restarts and gets a
/// fresh process. Empty `env_content` returns `true` (nothing to
/// verify, no restart needed).
///
/// Read directly from /proc/<pid>/environ rather than from systemd
/// because systemd's `EnvironmentFile` only loads at unit start —
/// there's no "show me the env vars systemd thinks the unit has"
/// query that reflects updates between start and now. /proc is
/// authoritative.
async fn caddy_environ_has_keys_from(env_content: &str) -> bool {
    let expected_keys: Vec<&str> = env_content
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim()))
        .filter(|k| !k.is_empty())
        .collect();
    if expected_keys.is_empty() {
        return true;
    }
    let Some(pid) = caddy_main_pid().await else {
        return false;
    };
    let environ_path = format!("/proc/{pid}/environ");
    let environ_bytes = match tokio::fs::read(&environ_path).await {
        Ok(b) => b,
        Err(e) => {
            warn!("read {environ_path}: {e}");
            return false;
        }
    };
    // /proc/<pid>/environ is NUL-separated KEY=VAL entries.
    let entries: Vec<&[u8]> = environ_bytes.split(|&b| b == 0).collect();
    for key in expected_keys {
        let needle = format!("{key}=");
        let needle_bytes = needle.as_bytes();
        let present = entries.iter().any(|e| e.starts_with(needle_bytes));
        if !present {
            return false;
        }
    }
    true
}

/// Look up Caddy's MainPID via systemd. Returns None when the unit
/// isn't running (MainPID==0) or when systemctl can't be invoked.
async fn caddy_main_pid() -> Option<u32> {
    let out = tokio::process::Command::new("systemctl")
        .args(["show", "-p", "MainPID", "--value", "caddy.service"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if pid == 0 { None } else { Some(pid) }
}

/// Build the `Vec<TlsPolicy>` to push for the current settings + app
/// subdomain set. Returns empty when ACME is disabled — the caller
/// PUTs that empty list to clear Caddy's automation.
fn build_policy_set(
    settings: &Settings,
    app_subdomains: &[String],
) -> Vec<nasty_apps::caddy::TlsPolicy> {
    if !settings.tls_acme_enabled {
        return Vec::new();
    }
    let mut policies = Vec::new();
    if let Some(domain) = settings.tls_domain.as_deref()
        && !domain.trim().is_empty()
    {
        policies.push(nasty_apps::caddy::TlsPolicy {
            host: domain.trim().to_string(),
        });
    }
    for sub in app_subdomains {
        let sub = sub.trim();
        if sub.is_empty() {
            continue;
        }
        // Deduplicate against the main domain.
        if policies.iter().any(|p| p.host == sub) {
            continue;
        }
        policies.push(nasty_apps::caddy::TlsPolicy {
            host: sub.to_string(),
        });
    }
    policies
}

/// Locate the cert Caddy is currently serving for `domain` when ACME
/// is on. Returns None when no managed cert is on disk yet — caller
/// surfaces that as "still issuing".
///
/// We only look under Caddy's ACME-issuer subtrees
/// (`certificates/acme-v02.*` etc.) — NOT under the internal-CA
/// subtree, because a managed-cert-not-yet-issued shouldn't be masked
/// by Caddy's local-authority cert. The internal CA's cert is what
/// the `:443` fallback serves, and the WebUI's "active certificate"
/// view is about the *managed* cert.
async fn locate_serving_cert(settings: &Settings) -> Option<String> {
    if !settings.tls_acme_enabled {
        return None;
    }
    let domain = settings.tls_domain.as_deref()?;
    let base = format!("{CADDY_DATA_DIR}/.local/share/caddy/certificates");
    let mut endpoints = tokio::fs::read_dir(&base).await.ok()?;
    while let Ok(Some(ep)) = endpoints.next_entry().await {
        let name = ep.file_name().to_string_lossy().into_owned();
        // Skip the internal-CA dir — those certs are for the fallback,
        // not for the managed hostname.
        if name.starts_with("local") {
            continue;
        }
        let candidate = format!("{}/{domain}/{domain}.crt", ep.path().display());
        if tokio::fs::metadata(&candidate).await.is_ok() {
            return Some(candidate);
        }
    }
    None
}

/// Repopulate the cached ACME status struct from whatever cert Caddy is
/// (or isn't yet) serving. Cheap to call repeatedly — does at most one
/// directory listing + one cert parse.
pub async fn refresh_acme_status_from_disk(settings: &Settings) {
    let domain = settings.tls_domain.clone();
    if !settings.tls_acme_enabled {
        // No ACME. The :443 fallback is Caddy's internal-CA cert,
        // managed entirely by Caddy. Surface that as `idle` rather
        // than parsing a specific file — there's no "static cert"
        // path the operator can point at anymore.
        let status = ACME_STATUS.get_or_init(|| std::sync::Mutex::new(AcmeStatus::default()));
        if let Ok(mut s) = status.lock() {
            s.state = "idle".into();
            s.message = "ACME disabled; Caddy's internal CA is serving the :443 fallback.".into();
            s.domain = domain;
            s.expires = None;
            s.issued = None;
            s.issuer = None;
        }
        return;
    }

    let Some(path) = locate_serving_cert(settings).await else {
        // ACME enabled but no managed cert on disk yet — still issuing.
        set_acme_status(
            "running",
            "Waiting for Caddy to obtain a certificate...",
            domain.as_deref(),
        );
        return;
    };
    let cert_info = read_cert_info(&path).await;
    let status = ACME_STATUS.get_or_init(|| std::sync::Mutex::new(AcmeStatus::default()));
    if let Ok(mut s) = status.lock() {
        s.state = "success".into();
        s.message = match domain.as_deref() {
            Some(d) => format!("Certificate active for {d}"),
            None => "Certificate active".into(),
        };
        s.domain = domain;
        s.expires = cert_info.expires;
        s.issued = cert_info.issued;
        s.issuer = cert_info.issuer;
    }
}

/// Engine-startup hook. Caddy auto-renews internally — there's nothing
/// for us to *do* here, just seed the cached status so the WebUI shows
/// cert details immediately rather than after the first user-triggered
/// apply.
pub async fn check_acme_renewal() {
    let settings = load().await;
    refresh_acme_status_from_disk(&settings).await;
}

struct CertInfo {
    expires: Option<String>,
    issued: Option<String>,
    issuer: Option<String>,
}

/// Per-host certificate details for the Ingress overview page. Returned
/// by [`cert_info_for_host`] for each `host`-matching Caddy route so the
/// WebUI can render an expiry/issuer badge per row.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct HostCertInfo {
    /// Issuer CN (e.g. `R10`, `Let's Encrypt Authority X3`) or O when no
    /// CN is present. None when the PEM didn't parse — surfaced as a
    /// "cert present but unreadable" hint to the WebUI.
    pub issuer: Option<String>,
    /// `not_before` from the cert, RFC 2822 string.
    pub issued: Option<String>,
    /// `not_after`, RFC 2822 string.
    pub expires: Option<String>,
    /// Days until expiry from now (negative when expired). Lets the
    /// WebUI colour the badge — red when ≤ 7, amber when ≤ 30, green
    /// otherwise — without parsing dates client-side.
    pub expires_in_days: Option<i64>,
    /// Absolute path the cert was read from. Diagnostic; the WebUI
    /// uses it as a tooltip when the operator hovers the badge.
    pub path: String,
}

/// Locate and read the cert Caddy serves for `host`. Walks
/// `/var/lib/caddy/.local/share/caddy/certificates/<endpoint>/...`
/// across every issuer endpoint, and matches either:
///   - directly by hostname (`example.com/example.com.crt`), or
///   - by an enclosing wildcard cert
///     (`wildcard_.example.com/wildcard_.example.com.crt` covers
///     `<anything>.example.com`).
///
/// Returns `None` when no cert is on disk yet. Issuance is eager —
/// the engine pushes automation policies the moment an ingress is
/// set, so Caddy starts work immediately — but DNS-01 with our 30s
/// `propagation_delay` plus the ACME order round-trip can take
/// 30-90s before the cert lands. See [`host_tls_status`] for the
/// richer state (issuing / failed / active) including the last error
/// message when issuance is stuck.
pub async fn cert_info_for_host(host: &str) -> Option<HostCertInfo> {
    let path = locate_cert_for_host(host).await?;
    let info = read_cert_info(&path).await;
    // Convert `expires` (RFC 2822) to days-from-now. Parse failure is
    // benign — the caller still sees `expires` as a string, just no
    // colour-coding. We use chrono since it's already in the deps.
    let expires_in_days = info.expires.as_deref().and_then(|s| {
        let parsed = chrono::DateTime::parse_from_rfc2822(s).ok()?;
        let secs = parsed.timestamp() - chrono::Utc::now().timestamp();
        Some(secs / 86_400)
    });
    Some(HostCertInfo {
        issuer: info.issuer,
        issued: info.issued,
        expires: info.expires,
        expires_in_days,
        path,
    })
}

/// Per-host TLS state surfaced on the `/tls` page. Distinguishes the
/// four states a managed hostname can be in:
///
/// - `active` — cert on disk, currently being served. `issuer` /
///   `expires` / `expires_in_days` are populated.
/// - `issuing` — Caddy is actively working on getting a cert. Tail
///   of recent log lines for this host showed `obtaining
///   certificate` or `trying to solve challenge`. `message` carries
///   the most recent log line so the operator can see how far along
///   the process is.
/// - `failed` — Caddy tried and gave up (rate-limit, DNS challenge
///   timeout, provider auth error). `message` carries the failure
///   reason verbatim from the log.
/// - `pending` — policy exists but Caddy hasn't logged any activity
///   yet (sub-second window between policy push and first work).
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct HostTlsStatus {
    pub host: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<i64>,
    /// `active` ⇒ on-disk cert path. `failed` / `issuing` ⇒ last log
    /// line, verbatim. `pending` ⇒ None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Build a [`HostTlsStatus`] for one managed hostname.
///
/// Decision tree:
///   1. Cert on disk via [`cert_info_for_host`] ⇒ `active`.
///   2. Else tail Caddy's journal for the last 10 minutes, find log
///      lines matching `"identifier":"<host>"`, classify the most
///      recent one:
///        - "obtained successfully" ⇒ `active` (race: cert just
///          landed but cert_info_for_host raced ahead of disk sync)
///        - "could not get" / "error" / "timed out" ⇒ `failed`
///          with the error message as `message`
///        - any other engaged-state phrase ⇒ `issuing` with the line
///          as `message`
///   3. No matching log lines ⇒ `pending`.
pub async fn host_tls_status(host: &str) -> HostTlsStatus {
    if let Some(info) = cert_info_for_host(host).await {
        return HostTlsStatus {
            host: host.to_string(),
            state: "active".into(),
            issuer: info.issuer,
            issued: info.issued,
            expires: info.expires,
            expires_in_days: info.expires_in_days,
            message: Some(info.path),
        };
    }

    // Tail Caddy's journal for this host. JSON-formatted logs from
    // certmagic always carry `"identifier":"<host>"` for ACME events
    // — that's the field we grep for. 10-minute window covers a
    // full DNS-01 attempt + a retry, but stays cheap.
    let output = tokio::process::Command::new("journalctl")
        .args(["-u", "caddy", "--since", "10 min ago", "--no-pager"])
        .output()
        .await;
    let lines = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    };

    // Walk lines newest-last (journalctl default). Find the most
    // recent log mentioning this host; classify by content.
    let needle = format!("\"identifier\":\"{host}\"");
    let last = lines.lines().rev().find(|l| l.contains(&needle));
    let Some(line) = last else {
        return HostTlsStatus {
            host: host.to_string(),
            state: "pending".into(),
            issuer: None,
            issued: None,
            expires: None,
            expires_in_days: None,
            message: None,
        };
    };

    let lower = line.to_lowercase();
    let state = if lower.contains("could not get") || lower.contains("timed out") {
        "failed"
    } else if lower.contains("obtained successfully") {
        "active"
    } else {
        "issuing"
    };

    HostTlsStatus {
        host: host.to_string(),
        state: state.into(),
        issuer: None,
        issued: None,
        expires: None,
        expires_in_days: None,
        message: Some(extract_log_message(line)),
    }
}

/// Return the list of all managed hostnames Caddy is currently
/// tracking, with each one's current TLS status. Reads
/// `apps.tls.certificates.automate` from Caddy's admin API (the
/// authoritative "what hosts should Caddy be obtaining certs for"
/// list — set by our `set_tls_automation` flow) and resolves status
/// per host.
///
/// Skips `nasty.local` (the internal-CA fallback) — that one is
/// always active and not interesting on the /tls page.
pub async fn list_host_tls_statuses() -> Vec<HostTlsStatus> {
    let url = "http://127.0.0.1:2019/config/apps/tls/certificates/automate";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build();
    let Ok(client) = client else {
        return Vec::new();
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let hosts: Vec<String> = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for host in hosts {
        if host == "nasty.local" {
            continue;
        }
        out.push(host_tls_status(&host).await);
    }
    out
}

/// Pull the human-readable `msg` field out of a Caddy JSON log line,
/// falling back to the whole line when the format doesn't match what
/// we expect. Keeps the WebUI tooltip readable instead of dumping
/// raw JSON.
fn extract_log_message(line: &str) -> String {
    // Look for "msg":"..." (un-escaped naive parse, good enough for
    // certmagic's well-formed logs). If we find `error` field too,
    // prefer it (it carries the actual failure detail).
    if let Some(err) = extract_json_string_field(line, "error")
        && !err.is_empty()
    {
        return err;
    }
    if let Some(msg) = extract_json_string_field(line, "msg") {
        return msg;
    }
    line.to_string()
}

fn extract_json_string_field(line: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\":\"");
    let start = line.find(&key)? + key.len();
    let rest = &line[start..];
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'\\' {
            end += 2;
            continue;
        }
        if bytes[end] == b'"' {
            break;
        }
        end += 1;
    }
    Some(rest[..end].replace("\\\"", "\"").replace("\\\\", "\\"))
}

async fn locate_cert_for_host(host: &str) -> Option<String> {
    let base = format!("{CADDY_DATA_DIR}/.local/share/caddy/certificates");
    let mut endpoints = tokio::fs::read_dir(&base).await.ok()?;
    while let Ok(Some(ep)) = endpoints.next_entry().await {
        // Direct match: <endpoint>/<host>/<host>.crt
        let direct = ep.path().join(host).join(format!("{host}.crt"));
        if tokio::fs::metadata(&direct).await.is_ok() {
            return Some(direct.to_string_lossy().into_owned());
        }
        // Wildcard match: walk subdirs named `wildcard_.<suffix>` and
        // check whether `host` ends in `.<suffix>`. Cert filename
        // mirrors the dir name so we don't have to guess.
        if let Ok(mut subdirs) = tokio::fs::read_dir(ep.path()).await {
            while let Ok(Some(sd)) = subdirs.next_entry().await {
                let name = sd.file_name().to_string_lossy().into_owned();
                if let Some(suffix) = name.strip_prefix("wildcard_.")
                    && host.ends_with(&format!(".{suffix}"))
                {
                    let candidate = sd.path().join(format!("{name}.crt"));
                    if tokio::fs::metadata(&candidate).await.is_ok() {
                        return Some(candidate.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    None
}

/// Read certificate details from a PEM file.
async fn read_cert_info(cert_path: &str) -> CertInfo {
    let mut info = CertInfo {
        expires: None,
        issued: None,
        issuer: None,
    };
    let pem_data = match tokio::fs::read(cert_path).await {
        Ok(d) => d,
        Err(_) => return info,
    };
    let pem = match x509_parser::pem::parse_x509_pem(&pem_data) {
        Ok((_, pem)) => pem,
        Err(_) => return info,
    };
    let cert = match pem.parse_x509() {
        Ok(c) => c,
        Err(_) => return info,
    };
    let validity = cert.validity();
    info.issued = Some(
        validity
            .not_before
            .to_rfc2822()
            .unwrap_or_else(|_| validity.not_before.to_string()),
    );
    info.expires = Some(
        validity
            .not_after
            .to_rfc2822()
            .unwrap_or_else(|_| validity.not_after.to_string()),
    );
    for rdn in cert.issuer().iter() {
        for attr in rdn.iter() {
            let val = attr.as_str().unwrap_or_default();
            let oid = attr.attr_type();
            // OID 2.5.4.3 = CN, 2.5.4.10 = O
            if *oid == x509_parser::oid_registry::OID_X509_COMMON_NAME {
                info.issuer = Some(val.to_string());
                break;
            } else if *oid == x509_parser::oid_registry::OID_X509_ORGANIZATION_NAME
                && info.issuer.is_none()
            {
                info.issuer = Some(val.to_string());
            }
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::{Settings, build_policy_set, caddy_acme_env, to_nix_string};

    fn acme_enabled_settings(challenge: &str) -> Settings {
        Settings {
            tls_domain: Some("nas.example.com".into()),
            tls_acme_email: Some("admin@example.com".into()),
            tls_acme_enabled: true,
            tls_challenge_type: challenge.into(),
            ..Settings::default()
        }
    }

    // ── build_policy_set ──

    #[test]
    fn policy_set_empty_when_acme_disabled() {
        // ACME off ⇒ no policies ⇒ engine PUTs an empty array ⇒ Caddy
        // stops renewing. Per-app subdomains are ignored in this state
        // because the user has opted out of cert automation entirely.
        let s = Settings::default();
        assert!(
            build_policy_set(&s, &["app.example.com".into()]).is_empty(),
            "subdomains must be skipped when ACME is disabled"
        );
    }

    #[test]
    fn policy_set_includes_main_domain() {
        let s = acme_enabled_settings("dns");
        let policies = build_policy_set(&s, &[]);
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].host, "nas.example.com");
    }

    #[test]
    fn policy_set_appends_app_subdomains() {
        let s = acme_enabled_settings("dns");
        let policies = build_policy_set(&s, &["a.example.com".into(), "b.example.com".into()]);
        assert_eq!(policies.len(), 3);
        // Main domain comes first so it gets issued first on cold boot.
        assert_eq!(policies[0].host, "nas.example.com");
        assert_eq!(policies[1].host, "a.example.com");
        assert_eq!(policies[2].host, "b.example.com");
    }

    #[test]
    fn policy_set_dedupes_subdomain_matching_main_domain() {
        // Edge case where an app's subdomain happens to equal the main
        // domain (operator misconfiguration). Without the dedupe Caddy
        // would receive two policies with the same subject and reject
        // the PUT — fail open by collapsing.
        let s = acme_enabled_settings("dns");
        let policies = build_policy_set(&s, &["nas.example.com".into()]);
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].host, "nas.example.com");
    }

    #[test]
    fn policy_set_skips_blank_subdomain_entries() {
        // The on-disk app.json may carry an empty or whitespace
        // `ingress_subdomain` field when the operator cleared it.
        // load_app_subdomains skips blanks; this is the second line of
        // defence.
        let s = acme_enabled_settings("dns");
        let policies = build_policy_set(&s, &["".into(), "   ".into()]);
        assert_eq!(policies.len(), 1, "only the main domain should remain");
    }

    #[test]
    fn policy_set_skips_main_domain_when_empty_string() {
        // Operator enabled ACME but didn't (yet) fill in the domain
        // field — only the apps' subdomains end up in the policy set.
        let mut s = acme_enabled_settings("dns");
        s.tls_domain = Some("   ".into());
        let policies = build_policy_set(&s, &["only.example.com".into()]);
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].host, "only.example.com");
    }

    #[test]
    fn acme_env_empty_unless_dns_challenge_with_creds() {
        // Three negative cases that should all yield "".
        assert_eq!(caddy_acme_env(&Settings::default()), "");
        let mut s = acme_enabled_settings("tls-alpn");
        s.tls_dns_credentials = Some("CF_API_TOKEN=ignored\n".into());
        assert_eq!(
            caddy_acme_env(&s),
            "",
            "tls-alpn challenge must not leak DNS creds to env file"
        );
        let mut s = acme_enabled_settings("dns");
        s.tls_dns_credentials = None;
        assert_eq!(caddy_acme_env(&s), "");
    }

    #[test]
    fn acme_env_preserves_user_pasted_kv_lines_exactly() {
        // The user's KEY=VAL textarea is the source of truth — the
        // engine writes it verbatim (after a trim) so Caddy's
        // EnvironmentFile parser sees it as-is.
        let mut s = acme_enabled_settings("dns");
        s.tls_dns_credentials = Some("CF_API_TOKEN=abc123\nFOO=bar\n".into());
        assert_eq!(caddy_acme_env(&s), "CF_API_TOKEN=abc123\nFOO=bar\n");
    }

    #[test]
    fn nix_string_renders_a_plain_hostname_unchanged() {
        // The common case: a normal hostname has no special characters,
        // so the output is just `"name"`.
        assert_eq!(to_nix_string("nasty"), "\"nasty\"");
    }

    #[test]
    fn nix_string_renders_an_fqdn_unchanged() {
        // The motivating case from issue #95: user set hostname to
        // a dot-separated FQDN. Dots have no Nix-string semantics so
        // they just pass through.
        assert_eq!(to_nix_string("nasty.domain.xyz"), "\"nasty.domain.xyz\"",);
    }

    #[test]
    fn nix_string_escapes_quotes_backslashes_and_dollar() {
        // Defensive — hostnames have been validated upstream, but if
        // anything ever sneaks past validation we don't want it
        // smuggling Nix syntax into the flake. `${...}` interpolation
        // is the most dangerous: escaping the leading `$` is enough.
        assert_eq!(to_nix_string("a\"b"), r#""a\"b""#);
        assert_eq!(to_nix_string("a\\b"), r#""a\\b""#);
        assert_eq!(to_nix_string("a${x}b"), r#""a\${x}b""#);
    }

    #[test]
    fn nix_string_escapes_whitespace_control_chars() {
        assert_eq!(to_nix_string("a\nb"), r#""a\nb""#);
        assert_eq!(to_nix_string("a\tb"), r#""a\tb""#);
    }
}
