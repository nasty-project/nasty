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
    fn validate_idmap_base_rejects_low_ranges() {
        // Must clear every local UID the engine can allocate.
        assert!(validate_idmap_base(3000).is_err());
        assert!(validate_idmap_base(65_535).is_err());
        assert!(validate_idmap_base(65_536).is_ok());
        assert!(validate_idmap_base(DEFAULT_IDMAP_BASE).is_ok());
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
}
