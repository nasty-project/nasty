use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/settings.json";
const STATE_DIR: &str = "/var/lib/nasty";
const TLS_NIX_PATH: &str = "/etc/nixos/nixos/tls.nix";

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
        save(&settings).await.map_err(|e| e.to_string())?;
        if tls_changed {
            write_tls_nix(&settings).await;
            if settings.tls_challenge_type == "dns" {
                write_dns_credentials(&settings).await;
            }
            // Apply TLS changes by rebuilding the NixOS config in the background.
            // This activates the ACME service and nginx certificate changes.
            info!("TLS settings changed — triggering nixos-rebuild switch...");
            tokio::spawn(async {
                match tokio::process::Command::new("nixos-rebuild")
                    .args(["switch", "--flake", "/etc/nixos/nixos"])
                    .output()
                    .await
                {
                    Ok(o) if o.status.success() => info!("TLS rebuild completed successfully"),
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        tracing::warn!("TLS rebuild failed: {stderr}");
                    }
                    Err(e) => tracing::warn!("TLS rebuild failed to start: {e}"),
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

/// Write /etc/nixos/nixos/tls.nix based on current TLS settings.
/// When ACME is enabled with a domain and email, generates the Let's Encrypt config.
/// Otherwise generates a no-op module (self-signed cert is the default in nasty.nix).
async fn write_tls_nix(settings: &Settings) {
    let nix = generate_tls_nix(settings);
    if let Err(e) = tokio::fs::write(TLS_NIX_PATH, &nix).await {
        warn!("Failed to write {TLS_NIX_PATH}: {e}");
    }
}

const DNS_CREDS_PATH: &str = "/var/lib/nasty/acme-dns-credentials";

fn generate_tls_nix(settings: &Settings) -> String {
    let mut out = String::from(
        "# Managed by NASty — edit via WebUI Settings > TLS\n{ ... }:\n{\n",
    );

    if settings.tls_acme_enabled {
        if let (Some(domain), Some(email)) = (&settings.tls_domain, &settings.tls_acme_email) {
            out.push_str("  security.acme.acceptTerms = true;\n");
            out.push_str(&format!("  security.acme.defaults.email = \"{email}\";\n"));
            out.push_str(&format!("  security.acme.certs.\"{domain}\" = {{\n"));

            if settings.tls_challenge_type == "dns" {
                if let Some(provider) = &settings.tls_dns_provider {
                    out.push_str(&format!("    dnsProvider = \"{provider}\";\n"));
                    out.push_str(&format!("    credentialsFile = \"{DNS_CREDS_PATH}\";\n"));
                }
            } else {
                out.push_str("    tlsChallenge = true;\n");
            }

            out.push_str("  };\n");
            out.push_str(&format!("  services.nasty.tls.certFile = \"/var/lib/acme/{domain}/fullchain.pem\";\n"));
            out.push_str(&format!("  services.nasty.tls.keyFile = \"/var/lib/acme/{domain}/key.pem\";\n"));
            out.push_str("  services.nasty.tls.selfSigned = false;\n");
        }
    }

    out.push_str("}\n");
    out
}

/// Write DNS credentials to a file readable by the ACME service.
async fn write_dns_credentials(settings: &Settings) {
    if let Some(creds) = &settings.tls_dns_credentials {
        if let Err(e) = tokio::fs::write(DNS_CREDS_PATH, creds).await {
            warn!("Failed to write DNS credentials: {e}");
            return;
        }
        // Restrict permissions — contains API keys
        let _ = tokio::fs::set_permissions(
            DNS_CREDS_PATH,
            std::fs::Permissions::from_mode(0o600),
        ).await;
    }
}
