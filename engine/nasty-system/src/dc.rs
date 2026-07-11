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

use crate::domain::{run_cmd, run_cmd_stdin};

pub const DC_CONF_PATH: &str = "/etc/samba/smb.dc.conf";
pub const DC_STATE_PATH: &str = "/var/lib/nasty/dc.json";
pub const DC_RESOLVED_DROPIN_PATH: &str = "/run/systemd/resolved.conf.d/nasty-dc.conf";
/// Root every domain-backup destination must resolve under.
#[allow(dead_code)] // consumed by Task 3 lifecycle
const FS_ROOT: &str = "/fs";
/// NASty's own network config — consulted (read-only) by the static-IP
/// precondition. Path mirrors `network.rs`'s private `JSON_PATH`.
#[allow(dead_code)] // consumed by Task 3 lifecycle
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
    // NOTE: `InterfaceConfig` has no top-level `method` — it's per-family,
    // at `ipv4.method` / `ipv6.method`. Either family being Static counts.
    if cfg.interfaces.iter().any(|i| {
        i.ipv4.method == crate::network::IpMethod::Static
            || i.ipv6.method == crate::network::IpMethod::Static
    }) {
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
        return Err(DcError::Validation(
            "destination must be an absolute path".into(),
        ));
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
                "destination directory is not empty — samba-tool requires an empty backup target"
                    .into(),
            ));
        }
    } else if resolved.exists() {
        return Err(DcError::Validation(
            "destination exists and is not a directory".into(),
        ));
    }
    Ok(resolved)
}

// ── samba-tool argv builders (pure; unit-tested for argv hygiene) ──
//
// These are `pub(crate)` and called from `#[cfg(test)] mod tests` below
// (so `cargo test` sees them as live), but Task 3 hasn't wired a
// non-test caller yet. `cargo clippy --all-targets` also builds the
// plain `--lib` target (no `--cfg test`), where the test module doesn't
// exist and these are genuinely unreferenced — hence the explicit
// allows, matching `samba_tool`/`samba_tool_stdin` below.

#[allow(dead_code)] // consumed by Task 3 lifecycle
fn with_conf(mut argv: Vec<String>) -> Vec<String> {
    argv.push("--configfile".into());
    argv.push(DC_CONF_PATH.into());
    argv
}

#[allow(dead_code)] // consumed by Task 3 lifecycle
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

#[allow(dead_code)] // consumed by Task 3 lifecycle
pub(crate) fn setpassword_args(user: &str) -> Vec<String> {
    with_conf(vec!["user".into(), "setpassword".into(), user.into()])
}

#[allow(dead_code)] // consumed by Task 3 lifecycle
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
///
/// Unused until Task 3 wires the provision/demote lifecycle on top of the
/// argv builders above.
#[allow(dead_code)] // consumed by Task 3 lifecycle
async fn samba_tool(argv: &[String]) -> Result<String, DcError> {
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    Ok(run_cmd("samba-tool", &args, &[]).await?)
}

/// Run samba-tool feeding `stdin_input` (the only way secrets travel).
///
/// Unused until Task 3 wires the provision/demote lifecycle on top of the
/// argv builders above.
#[allow(dead_code)] // consumed by Task 3 lifecycle
async fn samba_tool_stdin(argv: &[String], stdin_input: &str) -> Result<String, DcError> {
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    Ok(run_cmd_stdin("samba-tool", &args, &[], stdin_input).await?)
}

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
        assert!(
            global_pos < include_pos && include_pos < sysvol_pos,
            "got:\n{out}"
        );
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
        // NOTE (deviation from brief): `InterfaceConfig` has no top-level
        // `method` field — it's per-family at `ipv4.method` / `ipv6.method`
        // (see network.rs). The brief's flat {"name","method"} JSON doesn't
        // error on that mismatch — serde silently ignores the unmatched
        // "method" key rather than failing loudly, since InterfaceConfig
        // doesn't `deny_unknown_fields`, leaving ipv4.method at its
        // `IpMethod::default()` (Disabled) regardless of the argument. That
        // would make this helper silently useless rather than "fail to
        // deserialize", so — per the brief's own fallback instruction —
        // the struct is built literally instead, driving `ipv4.method`.
        crate::network::InterfaceConfig {
            name: name.to_string(),
            enabled: true,
            ipv4: crate::network::IpConfig {
                method,
                addresses: Vec::new(),
                gateway: None,
            },
            ipv6: crate::network::IpConfig::default(),
            mtu: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
        }
    }

    #[test]
    fn static_ip_check_matrix() {
        use crate::network::{IpMethod, NetworkConfig};
        // No config at all → externally managed → warn, not fail.
        assert!(matches!(static_ip_check(None), StaticIpCheck::Warn(_)));
        // Empty config → same.
        let empty = NetworkConfig::default();
        assert!(matches!(
            static_ip_check(Some(&empty)),
            StaticIpCheck::Warn(_)
        ));
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
            assert!(
                argv.windows(2)
                    .any(|w| w[0] == "--configfile" && w[1] == DC_CONF_PATH)
            );
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
