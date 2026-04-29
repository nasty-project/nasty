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
        Self { state: "idle".into(), message: String::new(), domain: None, expires: None, issued: None, issuer: None, last_attempt: None }
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
        if let Some(d) = domain { s.domain = Some(d.to_string()); }
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

/// Retry ACME provisioning with current settings.
pub async fn retry_acme() -> Result<(), String> {
    let settings = load().await;
    if !settings.tls_acme_enabled {
        return Err("ACME is not enabled".into());
    }
    if settings.tls_challenge_type == "dns" {
        write_dns_credentials(&settings).await;
    }
    tokio::spawn(async move {
        match run_lego(&settings).await {
            Ok(()) => info!("ACME certificate provisioned successfully"),
            Err(e) => warn!("ACME certificate provisioning failed: {e}"),
        }
    });
    Ok(())
}

const STATE_PATH: &str = "/var/lib/nasty/settings.json";
const STATE_DIR: &str = "/var/lib/nasty";
const TLS_CERT_PATH: &str = "/var/lib/nasty/tls/cert.pem";
const TLS_KEY_PATH: &str = "/var/lib/nasty/tls/key.pem";
const LEGO_DATA_DIR: &str = "/var/lib/nasty/lego";

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
    /// Domain name for Let's Encrypt TLS (e.g. "nasty.example.com"). Empty = self-signed.
    #[serde(default)]
    pub tls_domain: Option<String>,
    /// Email address for Let's Encrypt ACME notifications.
    #[serde(default)]
    pub tls_acme_email: Option<String>,
    /// Whether Let's Encrypt is enabled. Requires tls_domain and tls_acme_email.
    #[serde(default)]
    pub tls_acme_enabled: bool,
    /// ACME challenge type: "tls-alpn" (port 443) or "dns" (DNS provider API).
    #[serde(default = "default_challenge_type")]
    pub tls_challenge_type: String,
    /// DNS provider code for DNS-01 challenge (e.g. "cloudflare", "route53").
    #[serde(default)]
    pub tls_dns_provider: Option<String>,
    /// DNS provider API credentials as KEY=VALUE lines.
    #[serde(default)]
    pub tls_dns_credentials: Option<String>,
    /// Use Let's Encrypt staging environment (for testing, avoids rate limits).
    #[serde(default)]
    pub tls_acme_staging: bool,
    /// Custom DNS resolver for ACME propagation checks (e.g. "1.1.1.1:53").
    /// Default: use system resolver. Useful when local DNS doesn't see public records.
    #[serde(default)]
    pub tls_dns_resolver: Option<String>,
    /// Disable authoritative NS propagation check. Useful when the parent domain
    /// has no A record and the authoritative NS returns NXDOMAIN for the TXT record.
    #[serde(default)]
    pub tls_dns_disable_propagation_check: bool,
    /// Whether anonymous telemetry is enabled (drive count, storage capacity).
    #[serde(default = "default_telemetry_enabled")]
    pub telemetry_enabled: bool,
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
            tls_domain: None,
            tls_acme_email: None,
            tls_acme_enabled: false,
            tls_challenge_type: default_challenge_type(),
            tls_dns_provider: None,
            tls_dns_credentials: None,
            tls_acme_staging: false,
            tls_dns_resolver: None,
            tls_dns_disable_propagation_check: false,
            telemetry_enabled: default_telemetry_enabled(),
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
    /// Custom DNS resolver for propagation checks.
    pub tls_dns_resolver: Option<String>,
    /// Disable authoritative NS propagation check.
    pub tls_dns_disable_propagation_check: Option<bool>,
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
        if settings.hostname.is_none() {
            if let Ok(name) = tokio::fs::read_to_string("/proc/sys/kernel/hostname").await {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    settings.hostname = Some(name);
                    let _ = save(&settings).await;
                }
            }
        }
        Self {
            state: Arc::new(RwLock::new(settings)),
        }
    }

    pub async fn get(&self) -> Settings {
        self.state.read().await.clone()
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
        let mut tls_changed = false;
        if let Some(domain) = update.tls_domain {
            let domain = if domain.trim().is_empty() { None } else { Some(domain.trim().to_string()) };
            if settings.tls_domain != domain {
                settings.tls_domain = domain;
                tls_changed = true;
            }
        }
        if let Some(email) = update.tls_acme_email {
            let email = if email.trim().is_empty() { None } else { Some(email.trim().to_string()) };
            if settings.tls_acme_email != email {
                settings.tls_acme_email = email;
                tls_changed = true;
            }
        }
        if let Some(enabled) = update.tls_acme_enabled {
            if settings.tls_acme_enabled != enabled {
                settings.tls_acme_enabled = enabled;
                tls_changed = true;
            }
        }
        if let Some(ct) = update.tls_challenge_type {
            if settings.tls_challenge_type != ct {
                settings.tls_challenge_type = ct;
                tls_changed = true;
            }
        }
        if let Some(provider) = update.tls_dns_provider {
            let provider = if provider.trim().is_empty() { None } else { Some(provider.trim().to_string()) };
            if settings.tls_dns_provider != provider {
                settings.tls_dns_provider = provider;
                tls_changed = true;
            }
        }
        if let Some(creds) = update.tls_dns_credentials {
            let creds = if creds.trim().is_empty() { None } else { Some(creds.trim().to_string()) };
            if settings.tls_dns_credentials != creds {
                settings.tls_dns_credentials = creds;
                tls_changed = true;
            }
        }
        if let Some(staging) = update.tls_acme_staging {
            if settings.tls_acme_staging != staging {
                settings.tls_acme_staging = staging;
                tls_changed = true;
            }
        }
        if let Some(resolver) = update.tls_dns_resolver {
            if settings.tls_dns_resolver != Some(resolver.clone()) {
                settings.tls_dns_resolver = if resolver.is_empty() { None } else { Some(resolver) };
                tls_changed = true;
            }
        }
        if let Some(disable_cp) = update.tls_dns_disable_propagation_check {
            if settings.tls_dns_disable_propagation_check != disable_cp {
                settings.tls_dns_disable_propagation_check = disable_cp;
                tls_changed = true;
            }
        }
        if let Some(telemetry) = update.telemetry_enabled {
            settings.telemetry_enabled = telemetry;
        }
        save(&settings).await.map_err(|e| e.to_string())?;
        if tls_changed {
            if settings.tls_acme_enabled {
                if settings.tls_challenge_type == "dns" {
                    write_dns_credentials(&settings).await;
                }
                // Run ACME cert provisioning in the background
                let s = settings.clone();
                tokio::spawn(async move {
                    match run_lego(&s).await {
                        Ok(()) => info!("ACME certificate provisioned successfully"),
                        Err(e) => warn!("ACME certificate provisioning failed: {e}"),
                    }
                });
            }
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
    // NixOS has /etc as read-only — set hostname via kernel proc only.
    // Persistence is via /var/lib/nasty/settings.json, read at boot.
    tokio::fs::write("/proc/sys/kernel/hostname", name.as_bytes())
        .await
        .map_err(|e| format!("failed to set kernel hostname: {e}"))?;
    Ok(())
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
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

async fn save(settings: &Settings) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(settings).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}

const DNS_CREDS_PATH: &str = "/var/lib/nasty/acme-dns-credentials";

/// Run lego ACME client to obtain or renew a certificate.
/// Writes cert and key to /var/lib/nasty/tls/ and reloads nginx.
async fn run_lego(settings: &Settings) -> Result<(), String> {
    let domain = settings.tls_domain.as_deref()
        .ok_or("TLS domain not set")?;
    let email = settings.tls_acme_email.as_deref()
        .ok_or("ACME email not set")?;

    set_acme_status("running", &format!("Preparing certificate for {domain}..."), Some(domain));

    // Create lego data directory
    // Use separate lego directories per ACME server so staging/production data coexist
    let lego_dir = if settings.tls_acme_staging {
        format!("{LEGO_DATA_DIR}/staging")
    } else {
        format!("{LEGO_DATA_DIR}/production")
    };
    let _ = tokio::fs::create_dir_all(&lego_dir).await;

    // Determine if this is a new cert or renewal
    let lego_cert_path = format!("{lego_dir}/certificates/{domain}.crt");
    let action = if std::path::Path::new(&lego_cert_path).exists() {
        "renew"
    } else {
        "run"
    };

    let mut args = vec![
        "--accept-tos".to_string(),
        "--email".to_string(), email.to_string(),
        "--domains".to_string(), domain.to_string(),
        "--path".to_string(), lego_dir.clone(),
    ];

    if settings.tls_acme_staging {
        args.push("--server".to_string());
        args.push("https://acme-staging-v02.api.letsencrypt.org/directory".to_string());
    }

    // Challenge type
    if settings.tls_challenge_type == "dns" {
        if let Some(ref provider) = settings.tls_dns_provider {
            args.push("--dns".to_string());
            args.push(provider.clone());
            // Custom resolver for propagation checks (default: 1.1.1.1 to avoid
            // issues with local DNS not seeing public ACME TXT records).
            let resolver = settings.tls_dns_resolver.as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("1.1.1.1:53");
            args.push("--dns.resolvers".to_string());
            args.push(resolver.to_string());
            args.push("--dns.propagation-rns".to_string());
            if settings.tls_dns_disable_propagation_check {
                args.push("--dns.propagation-disable-ans".to_string());
            }
        } else {
            return Err("DNS challenge selected but no provider configured".to_string());
        }
    } else {
        // TLS-ALPN-01: lego listens on :443 temporarily
        // nginx must be stopped briefly for this to work
        args.push("--tls".to_string());
        args.push("--tls.port".to_string());
        args.push(":443".to_string());
    }

    args.push(action.to_string());

    info!("Running lego {action} for {domain} (challenge: {})", settings.tls_challenge_type);

    // For TLS-ALPN challenge, stop nginx briefly so lego can bind to :443
    let need_nginx_stop = settings.tls_challenge_type != "dns";
    if need_nginx_stop {
        set_acme_status("running", "Stopping web server for TLS challenge...", Some(domain));
        let _ = tokio::process::Command::new("systemctl")
            .args(["stop", "nginx"])
            .output().await;
    }

    if settings.tls_challenge_type == "dns" {
        set_acme_status("running", &format!("Running ACME {action} — creating DNS records and waiting for propagation (this can take 1-2 minutes)..."), Some(domain));
    } else {
        set_acme_status("running", &format!("Running ACME {action} — waiting for Let's Encrypt verification..."), Some(domain));
    }

    // Run lego — stream stderr to status updates, ensure nginx is ALWAYS restarted
    let lego_result = async {
        let mut cmd = tokio::process::Command::new("lego");
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        cmd.args(&arg_refs);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        if settings.tls_challenge_type == "dns" {
            if let Some(ref creds) = settings.tls_dns_credentials {
                for line in creds.lines() {
                    if let Some((key, value)) = line.split_once('=') {
                        cmd.env(key.trim(), value.trim());
                    }
                }
            }
        }

        let mut child = cmd.spawn()
            .map_err(|e| format!("failed to run lego: {e}"))?;

        // Stream stderr lines to ACME status so the UI shows real-time progress
        let domain_owned = domain.to_string();
        let stderr_handle = if let Some(stderr) = child.stderr.take() {
            let handle = tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                let mut collected = Vec::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        set_acme_status("running", &line, Some(&domain_owned));
                        collected.push(line);
                    }
                }
                collected
            });
            Some(handle)
        } else {
            None
        };

        let status = child.wait().await
            .map_err(|e| format!("lego wait: {e}"))?;

        let stderr_lines = if let Some(h) = stderr_handle {
            h.await.unwrap_or_default()
        } else {
            vec![]
        };

        Ok::<_, String>((status, stderr_lines))
    }.await;

    // ALWAYS restart nginx, regardless of lego success/failure
    if need_nginx_stop {
        set_acme_status("running", "Restarting web server...", Some(domain));
        let _ = tokio::process::Command::new("systemctl")
            .args(["start", "nginx"])
            .output().await;
    }

    let (exit_status, stderr_lines) = lego_result?;

    if !exit_status.success() {
        let stderr = stderr_lines.join("\n");
        let msg = format!("lego {action} failed: {stderr}");
        set_acme_status("error", &msg, Some(domain));
        return Err(msg);
    }

    set_acme_status("running", "Installing certificate...", Some(domain));

    // Copy lego certs to NASty's TLS paths
    let lego_cert = format!("{lego_dir}/certificates/{domain}.crt");
    let lego_key = format!("{lego_dir}/certificates/{domain}.key");

    tokio::fs::copy(&lego_cert, TLS_CERT_PATH).await
        .map_err(|e| { let m = format!("failed to copy cert: {e}"); set_acme_status("error", &m, Some(domain)); m })?;
    tokio::fs::copy(&lego_key, TLS_KEY_PATH).await
        .map_err(|e| { let m = format!("failed to copy key: {e}"); set_acme_status("error", &m, Some(domain)); m })?;

    // Set permissions so nginx (running as nginx user) can read the cert
    let _ = tokio::fs::set_permissions(TLS_CERT_PATH, std::fs::Permissions::from_mode(0o644)).await;
    let _ = tokio::fs::set_permissions(TLS_KEY_PATH, std::fs::Permissions::from_mode(0o640)).await;
    // Set key group to nginx so it can read it
    let _ = tokio::process::Command::new("chown")
        .args(["root:nginx", TLS_KEY_PATH])
        .output().await;

    // Reload nginx to pick up the new certificate
    let reload = tokio::process::Command::new("systemctl")
        .args(["reload", "nginx"])
        .output().await;
    match reload {
        Ok(r) if r.status.success() => info!("nginx reloaded with new certificate"),
        Ok(r) => {
            let stderr = String::from_utf8_lossy(&r.stderr);
            warn!("nginx reload failed after cert install: {stderr}");
        }
        Err(e) => warn!("Failed to reload nginx: {e}"),
    }

    // Read cert details and populate status
    let cert_info = read_cert_info(TLS_CERT_PATH).await;
    {
        let status = ACME_STATUS.get_or_init(|| std::sync::Mutex::new(AcmeStatus::default()));
        if let Ok(mut s) = status.lock() {
            s.state = "success".to_string();
            s.message = format!("Certificate installed for {domain}");
            s.domain = Some(domain.to_string());
            s.expires = cert_info.expires;
            s.issued = cert_info.issued;
            s.issuer = cert_info.issuer;
            s.last_attempt = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs().to_string())
                    .unwrap_or_default()
            );
        }
    }
    info!("ACME certificate installed for {domain}");
    Ok(())
}

struct CertInfo {
    expires: Option<String>,
    issued: Option<String>,
    issuer: Option<String>,
}

/// Read certificate details from a PEM file.
async fn read_cert_info(cert_path: &str) -> CertInfo {
    let mut info = CertInfo { expires: None, issued: None, issuer: None };
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
    info.issued = Some(validity.not_before.to_rfc2822().unwrap_or_else(|_| validity.not_before.to_string()));
    info.expires = Some(validity.not_after.to_rfc2822().unwrap_or_else(|_| validity.not_after.to_string()));
    // Extract CN or O from issuer
    for rdn in cert.issuer().iter() {
        for attr in rdn.iter() {
            let val = attr.as_str().unwrap_or_default();
            let oid = attr.attr_type();
            // OID 2.5.4.3 = CN, 2.5.4.10 = O
            if *oid == x509_parser::oid_registry::OID_X509_COMMON_NAME {
                info.issuer = Some(val.to_string());
                break;
            } else if *oid == x509_parser::oid_registry::OID_X509_ORGANIZATION_NAME && info.issuer.is_none() {
                info.issuer = Some(val.to_string());
            }
        }
    }
    info
}

/// Write DNS credentials to a file readable by the ACME client.
async fn write_dns_credentials(settings: &Settings) {
    if let Some(creds) = &settings.tls_dns_credentials {
        if let Err(e) = tokio::fs::write(DNS_CREDS_PATH, creds).await {
            warn!("Failed to write DNS credentials: {e}");
            return;
        }
        let _ = tokio::fs::set_permissions(
            DNS_CREDS_PATH,
            std::fs::Permissions::from_mode(0o600),
        ).await;
    }
}

/// Check if ACME cert needs renewal (runs on engine startup).
pub async fn check_acme_renewal() {
    let settings = load().await;
    if !settings.tls_acme_enabled {
        return;
    }
    if settings.tls_domain.is_none() || settings.tls_acme_email.is_none() {
        return;
    }

    // Populate cert info on startup so the UI shows details immediately
    if std::path::Path::new(TLS_CERT_PATH).exists() {
        let cert_info = read_cert_info(TLS_CERT_PATH).await;
        let domain = settings.tls_domain.as_deref().unwrap_or("");
        let status = ACME_STATUS.get_or_init(|| std::sync::Mutex::new(AcmeStatus::default()));
        if let Ok(mut s) = status.lock() {
            s.state = "success".to_string();
            s.message = format!("Certificate installed for {domain}");
            s.domain = Some(domain.to_string());
            s.expires = cert_info.expires;
            s.issued = cert_info.issued;
            s.issuer = cert_info.issuer;
        }
    }

    // Check if cert exists and is near expiry (within 30 days)
    let lego_subdir = if settings.tls_acme_staging { "staging" } else { "production" };
    let cert_path = format!("{LEGO_DATA_DIR}/{lego_subdir}/certificates/{}.crt",
        settings.tls_domain.as_deref().unwrap_or(""));
    if !std::path::Path::new(&cert_path).exists() {
        info!("No ACME cert found, running initial provisioning...");
        if let Err(e) = run_lego(&settings).await {
            warn!("ACME provisioning on startup failed: {e}");
        }
        return;
    }

    // Try renewal (lego handles expiry check internally)
    info!("Checking ACME certificate renewal...");
    if let Err(e) = run_lego(&settings).await {
        warn!("ACME renewal check failed: {e}");
    }
}
