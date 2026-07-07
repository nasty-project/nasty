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
        match tokio::fs::read_to_string(CONFIG_PATH).await {
            Ok(content) => serde_json::from_str(&content).ok(),
            Err(_) => None,
        }
    }

    /// Persist domain configuration to disk.
    pub async fn save_config(config: &DomainConfig) -> Result<(), DomainError> {
        let dir = Path::new(CONFIG_PATH).parent().unwrap();
        tokio::fs::create_dir_all(dir).await?;
        let json =
            serde_json::to_string(config).map_err(|e| DomainError::Io(std::io::Error::other(e)))?;
        tokio::fs::write(CONFIG_PATH, json).await?;
        Ok(())
    }

    /// Clear domain configuration (leave domain).
    pub async fn clear_config() -> Result<(), DomainError> {
        match tokio::fs::remove_file(CONFIG_PATH).await {
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
}
