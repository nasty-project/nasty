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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DcPrincipal {
    pub name: String,
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

/// Sanity check that samba-tool's provision actually wrote its generated
/// configuration back to the --configfile path (rather than leaving the
/// empty file we seeded for lp.load()). A provisioned config always
/// carries a [global] section with the realm.
fn looks_provisioned(conf: &str) -> bool {
    let lower = conf.to_lowercase();
    lower.contains("[global]") && lower.contains("realm")
}

/// Remove every existing `dns forwarder = ...` line (samba-tool provision
/// writes one pointing at resolv.conf's value — typically the resolved stub
/// we've just disabled; smb.conf is last-value-wins, so a leftover line
/// would override the operator's forwarder).
fn strip_dns_forwarder_lines(conf: &str) -> String {
    conf.lines()
        .filter(|l| {
            let t = l.trim();
            !(t.starts_with("dns forwarder") && t.contains('='))
        })
        .map(|l| format!("{l}\n"))
        .collect()
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

/// Static-IP precondition (spec-amended). NASty fails this check only
/// when it itself is managing this box's addressing dynamically — that's
/// fixable by the operator on the Network page. When NASty has no L3
/// opinion at all, the box is externally managed and we can only warn.
///
/// - **Pass**: any interface/bond/vlan/bridge has `ipv4` or `ipv6`
///   `Static`.
/// - **Fail**: no `Static` anywhere, but at least one `Dhcp`/`Slaac` —
///   NASty itself is assigning a dynamic address.
/// - **Warn**: no network config, or nothing but `Disabled`/`Inherit` —
///   NASty has no opinion on this box's addressing (externally managed
///   box / fresh VM). This covers the all-`Disabled` case, which used to
///   Fail: a box NASty was never told to address isn't the same failure
///   as one NASty is actively DHCP-addressing.
///
/// `macvlans` are excluded from all of the above: they're engine-managed
/// app shims (#448), never the box's own address.
#[derive(Debug)]
pub enum StaticIpCheck {
    Pass,
    Warn(String),
    Fail(String),
}

/// Warn text shared by both "NASty has no L3 opinion" cases below (no
/// persisted config at all, or one that exists but assigns nothing
/// dynamically itself) — a single `const` so the two call sites can't
/// drift apart.
const STATIC_IP_EXTERNALLY_MANAGED_WARN: &str = "NASty does not manage this box's network config — make sure the DC's IP address is static (a DHCP-addressed DC breaks the domain when the lease changes)";

/// True if any interface/bond/vlan/bridge has an `ipv4` or `ipv6` method
/// matching `pred`. `macvlans` are deliberately excluded — they're
/// engine-managed app shims (#448), never the box's own address.
fn any_managed_ip_method(
    cfg: &crate::network::NetworkConfig,
    pred: impl Fn(&crate::network::IpMethod) -> bool,
) -> bool {
    cfg.interfaces
        .iter()
        .any(|i| pred(&i.ipv4.method) || pred(&i.ipv6.method))
        || cfg
            .bonds
            .iter()
            .any(|b| pred(&b.ipv4.method) || pred(&b.ipv6.method))
        || cfg
            .vlans
            .iter()
            .any(|v| pred(&v.ipv4.method) || pred(&v.ipv6.method))
        || cfg
            .bridges
            .iter()
            .any(|b| pred(&b.ipv4.method) || pred(&b.ipv6.method))
}

pub fn static_ip_check(cfg: Option<&crate::network::NetworkConfig>) -> StaticIpCheck {
    use crate::network::IpMethod;

    let Some(cfg) = cfg else {
        return StaticIpCheck::Warn(STATIC_IP_EXTERNALLY_MANAGED_WARN.into());
    };

    if any_managed_ip_method(cfg, |m| matches!(m, IpMethod::Static)) {
        return StaticIpCheck::Pass;
    }
    if any_managed_ip_method(cfg, |m| matches!(m, IpMethod::Dhcp | IpMethod::Slaac)) {
        return StaticIpCheck::Fail(
            "a domain controller needs a static IP address, but this box's NASty-managed addressing is dynamic (DHCP/SLAAC) — set a static address on the Network page first".into(),
        );
    }
    StaticIpCheck::Warn(STATIC_IP_EXTERNALLY_MANAGED_WARN.into())
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

fn parse_principal_lines(raw: &str) -> Vec<DcPrincipal> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| DcPrincipal {
            name: l.to_string(),
        })
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
                "this box is joined to a domain as a member — leave the domain before hosting one"
                    .into(),
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

        // A stale DC config or half-written domain databases (from an
        // earlier failed provision) would make samba-tool refuse to run.
        // Safe here: preflight already ruled out hosting AND member mode.
        let _ = tokio::fs::remove_file(DC_CONF_PATH).await;
        let _ = tokio::fs::remove_dir_all("/var/lib/samba/private").await;
        let _ = tokio::fs::remove_dir_all("/var/lib/samba/sysvol").await;
        let _ = tokio::fs::remove_dir_all("/var/lib/samba/bind-dns").await;

        // samba-tool with an explicit --configfile requires the file to
        // EXIST at load time (lp.load() errors on a missing path — first
        // live CI run proved it); provision then writes the generated
        // config back to the same path. Hand it an empty one to load.
        tokio::fs::write(DC_CONF_PATH, "").await?;

        // ── Provision (throwaway password on argv, rotated below) ──
        let throwaway = uuid::Uuid::new_v4().to_string();
        info!("dc: provisioning domain {realm} (workgroup {workgroup})");
        samba_tool(&provision_args(&realm, &workgroup, &throwaway)).await?;

        // Everything past this point unwinds on failure.
        let result: Result<(), DcError> = async {
            // NASty [global] additions inside the generated config. Strip
            // samba-tool's own `dns forwarder` line first — smb.conf is
            // last-value-wins within a section, so a leftover line (pointing
            // at the resolved stub we've just disabled) would silently win
            // over the operator's forwarder if it came after ours.
            let conf = tokio::fs::read_to_string(DC_CONF_PATH).await?;
            if !looks_provisioned(&conf) {
                return Err(DcError::CommandFailed(
                    "samba-tool provision succeeded but did not write the expected configuration to smb.dc.conf".into(),
                ));
            }
            let conf = insert_into_global(
                &strip_dns_forwarder_lines(&conf),
                &nasty_global_additions(&dns_forwarder),
            );
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
        if tokio::fs::remove_file(DC_RESOLVED_DROPIN_PATH)
            .await
            .is_ok()
        {
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
        let resolved = validate_backup_dest(Path::new(dest), Path::new(FS_ROOT))?;
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
            DcError::CommandFailed(
                "samba-tool reported success but no backup tarball was found".into(),
            )
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
            if tokio::fs::write(DC_RESOLVED_DROPIN_PATH, dropin)
                .await
                .is_ok()
            {
                let _ = run_cmd("systemctl", &["restart", "systemd-resolved"], &[]).await;
            }
        }
        if let Err(e) = run_cmd("systemctl", &["start", "samba-dc.service"], &[]).await {
            warn!("dc: failed to start samba-dc at boot restore: {e}");
        }
        true
    }

    // ── Domain principal management (all samba-tool; passwords via stdin) ──

    async fn require_hosting(&self) -> Result<(), DcError> {
        if Self::load_config().await.is_none() {
            return Err(DcError::NotHosting);
        }
        Ok(())
    }

    pub async fn user_list(&self) -> Result<Vec<DcPrincipal>, DcError> {
        self.require_hosting().await?;
        Ok(parse_principal_lines(
            &samba_tool(&list_args("user")).await?,
        ))
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
        Ok(parse_principal_lines(
            &samba_tool(&list_args("group")).await?,
        ))
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
        Ok(parse_principal_lines(
            &samba_tool(&list_args("computer")).await?,
        ))
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
    let out = run_cmd("resolvectl", &["dns"], &[])
        .await
        .unwrap_or_default();
    for token in out.split_whitespace() {
        if let Ok(ip) = token.parse::<std::net::IpAddr>()
            && !ip.is_loopback()
        {
            return Ok(ip.to_string());
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

    #[test]
    fn strip_dns_forwarder_lines_removes_samba_provisioned_forwarder() {
        // provision-shaped config: samba-tool wrote its own `dns forwarder`
        // line (pointing at resolv.conf's value — typically the resolved
        // stub) into [global].
        let conf = "# Global parameters\n[global]\n\tdns forwarder = 127.0.0.53\n\trealm = AD.EXAMPLE.COM\n\n[sysvol]\n\tpath = /var/lib/samba/sysvol\n\n[netlogon]\n\tpath = /var/lib/samba/sysvol/ad.example.com/scripts\n";
        let out = insert_into_global(
            &strip_dns_forwarder_lines(conf),
            &nasty_global_additions("192.168.1.1"),
        );
        assert!(out.contains("dns forwarder = 192.168.1.1"), "got:\n{out}");
        assert!(!out.contains("127.0.0.53"), "got:\n{out}");
        assert_eq!(
            out.matches("dns forwarder").count(),
            1,
            "expected exactly one dns forwarder line, got:\n{out}"
        );
    }

    // ── looks_provisioned ─────────────────────────────────────
    #[test]
    fn looks_provisioned_accepts_generated_config() {
        // Same provision-shaped fixture used above to model samba-tool's
        // generated smb.dc.conf.
        let conf = "# Global parameters\n[global]\n\tdns forwarder = 127.0.0.53\n\trealm = AD.EXAMPLE.COM\n\n[sysvol]\n\tpath = /var/lib/samba/sysvol\n\n[netlogon]\n\tpath = /var/lib/samba/sysvol/ad.example.com/scripts\n";
        assert!(looks_provisioned(conf));
    }

    #[test]
    fn looks_provisioned_rejects_empty_seed_file() {
        // The empty file we seed for samba-tool's lp.load() before
        // provision runs — must not be mistaken for a provisioned config.
        assert!(!looks_provisioned(""));
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

    /// Same shape as `iface`, but drives `ipv6.method` instead of
    /// `ipv4.method` — needed for the "ipv4 disabled, ipv6 static" case
    /// (either family counts, see `static_ip_check`).
    fn iface_ipv6(name: &str, method: crate::network::IpMethod) -> crate::network::InterfaceConfig {
        crate::network::InterfaceConfig {
            name: name.to_string(),
            enabled: true,
            ipv4: crate::network::IpConfig::default(),
            ipv6: crate::network::IpConfig {
                method,
                addresses: Vec::new(),
                gateway: None,
            },
            mtu: None,
            sriov_num_vfs: None,
            vfs: Vec::new(),
        }
    }

    /// Minimal `BondConfig` driving just `ipv4.method` — mirrors `iface`.
    /// `mode` is irrelevant to the static-IP check; `ActiveBackup` is an
    /// arbitrary valid value.
    fn bond(name: &str, method: crate::network::IpMethod) -> crate::network::BondConfig {
        crate::network::BondConfig {
            name: name.to_string(),
            members: Vec::new(),
            mode: crate::network::BondMode::ActiveBackup,
            ipv4: crate::network::IpConfig {
                method,
                addresses: Vec::new(),
                gateway: None,
            },
            ipv6: crate::network::IpConfig::default(),
            mtu: None,
            inherit_member_mac: true,
        }
    }

    /// Minimal `BridgeConfig` driving just `ipv4.method` — mirrors `iface`.
    fn bridge(name: &str, method: crate::network::IpMethod) -> crate::network::BridgeConfig {
        crate::network::BridgeConfig {
            name: name.to_string(),
            members: Vec::new(),
            ipv4: crate::network::IpConfig {
                method,
                addresses: Vec::new(),
                gateway: None,
            },
            ipv6: crate::network::IpConfig::default(),
            mtu: None,
            stp: false,
            forward_delay_s: None,
            inherit_member_mac: true,
        }
    }

    #[test]
    fn static_ip_check_matrix() {
        use crate::network::{IpMethod, NetworkConfig};

        // No config at all → externally managed → warn, not fail.
        assert!(matches!(static_ip_check(None), StaticIpCheck::Warn(_)));

        // Empty config (every Vec empty) → same.
        let empty = NetworkConfig::default();
        assert!(matches!(
            static_ip_check(Some(&empty)),
            StaticIpCheck::Warn(_)
        ));

        // Single interface, all-Disabled → NASty has no L3 opinion → warn,
        // NOT fail. Changed behavior: the old predicate treated "managed
        // but not Static" as Fail, which punished boxes NASty was never
        // told to address at all.
        let mut all_disabled = NetworkConfig::default();
        all_disabled
            .interfaces
            .push(iface("eth0", IpMethod::Disabled));
        assert!(matches!(
            static_ip_check(Some(&all_disabled)),
            StaticIpCheck::Warn(_)
        ));

        // ipv4 Dhcp → NASty itself assigns a dynamic address → fail,
        // message points the operator at the Network page.
        let mut dhcp = NetworkConfig::default();
        dhcp.interfaces.push(iface("eth0", IpMethod::Dhcp));
        match static_ip_check(Some(&dhcp)) {
            StaticIpCheck::Fail(msg) => assert!(msg.contains("Network page"), "msg: {msg}"),
            other => panic!("expected Fail, got {other:?}"),
        }

        // ipv4 Slaac → same dynamic-addressing fail.
        let mut slaac = NetworkConfig::default();
        slaac.interfaces.push(iface("eth0", IpMethod::Slaac));
        assert!(matches!(
            static_ip_check(Some(&slaac)),
            StaticIpCheck::Fail(_)
        ));

        // ipv4 Disabled + ipv6 Static on the same interface → pass;
        // either family counts.
        let mut ipv6_static = NetworkConfig::default();
        ipv6_static
            .interfaces
            .push(iface_ipv6("eth0", IpMethod::Static));
        assert!(matches!(
            static_ip_check(Some(&ipv6_static)),
            StaticIpCheck::Pass
        ));

        // Static on a bridge only, no interfaces at all → pass; bridges
        // are collected too, not just top-level interfaces.
        let mut bridge_static = NetworkConfig::default();
        bridge_static.bridges.push(bridge("br0", IpMethod::Static));
        assert!(matches!(
            static_ip_check(Some(&bridge_static)),
            StaticIpCheck::Pass
        ));

        // Same, but with a Disabled interface also present → still pass.
        let mut bridge_static_disabled_iface = NetworkConfig::default();
        bridge_static_disabled_iface
            .interfaces
            .push(iface("eth0", IpMethod::Disabled));
        bridge_static_disabled_iface
            .bridges
            .push(bridge("br0", IpMethod::Static));
        assert!(matches!(
            static_ip_check(Some(&bridge_static_disabled_iface)),
            StaticIpCheck::Pass
        ));

        // Dhcp on a bond only → fail; bonds are collected too.
        let mut bond_dhcp = NetworkConfig::default();
        bond_dhcp.bonds.push(bond("bond0", IpMethod::Dhcp));
        match static_ip_check(Some(&bond_dhcp)) {
            StaticIpCheck::Fail(msg) => assert!(msg.contains("Network page"), "msg: {msg}"),
            other => panic!("expected Fail, got {other:?}"),
        }

        // Inherit-only (nothing Static/Dhcp/Slaac anywhere) → warn, not
        // fail — Inherit is NASty deferring to a member's L3, not NASty
        // itself assigning a dynamic address.
        let mut inherit_only = NetworkConfig::default();
        inherit_only.bridges.push(bridge("br0", IpMethod::Inherit));
        assert!(matches!(
            static_ip_check(Some(&inherit_only)),
            StaticIpCheck::Warn(_)
        ));
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

    // ── backup tarball discovery ──────────────────────────────
    #[test]
    fn find_backup_tarball_picks_the_tarball() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("samba-backup-2026-07-10.tar.bz2"), b"x").unwrap();
        let found = find_backup_tarball(dir.path()).unwrap();
        assert!(found.ends_with("samba-backup-2026-07-10.tar.bz2"));
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
        // ...and pins the DC config, same as every other samba-tool call.
        assert!(
            prov.windows(2)
                .any(|w| w[0] == "--configfile" && w[1] == DC_CONF_PATH)
        );
    }

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
            assert!(
                argv.windows(2)
                    .any(|w| w[0] == "--configfile" && w[1] == DC_CONF_PATH)
            );
        }
    }
}
