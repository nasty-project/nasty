use std::path::Path;

use nasty_common::{BlockDeviceMappings, BlockVolumeId, HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

const STATE_DIR: &str = "/var/lib/nasty/shares/nvmeof";
const NVMET_BASE: &str = "/sys/kernel/config/nvmet";
const DEFAULT_NQN_PREFIX: &str = "nqn.2137-04.storage.nasty";
const BASE_NQN_PATH: &str = "/var/lib/nasty/nvmeof-base-nqn";
const PREFIX_HISTORY_DIR: &str = "/var/lib/nasty/nvmeof-managed-prefixes";

#[derive(Debug, Error)]
pub enum NvmeofError {
    #[error("subsystem not found: {0}")]
    NotFound(String),
    #[error("subsystem already exists: {0}")]
    AlreadyExists(String),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("namespace backing volume cannot be repaired: {0}")]
    InvalidBackingVolume(String),
    #[error("namespace not found: nsid {0}")]
    NamespaceNotFound(u32),
    #[error("port not found: {0}")]
    PortNotFound(u16),
    #[error("configfs error: {0}")]
    ConfigFs(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported transport '{0}' (expected \"tcp\" or \"rdma\")")]
    InvalidTransport(String),
}

// ── Data types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NvmeofSubsystem {
    /// Unique subsystem identifier (UUID).
    pub id: String,
    /// NVMe Qualified Name (e.g. `nqn.2137-04.storage.nasty:tank-vol`).
    pub nqn: String,
    /// Block device namespaces exposed by this subsystem.
    pub namespaces: Vec<Namespace>,
    /// Transport ports this subsystem is reachable on.
    pub ports: Vec<Port>,
    /// NQNs of hosts explicitly allowed to connect (used when `allow_any_host` is false).
    pub allowed_hosts: Vec<String>,
    /// Whether any host NQN is permitted to connect without explicit ACL entries.
    pub allow_any_host: bool,
    /// Whether this subsystem is active in nvmet configfs.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Namespace {
    /// Namespace ID (1-based, auto-assigned).
    pub nsid: u32,
    /// Block device path backing this namespace (e.g. `/dev/loop0`).
    pub device_path: String,
    /// Whether the namespace is enabled in configfs.
    pub enabled: bool,
    /// Stable identity of a NASty-managed block subvolume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing_volume: Option<BlockVolumeId>,
    /// Prevents startup from trusting a stale legacy loop path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backing_volume_unresolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Port {
    /// nvmet configfs port ID (unique across all subsystems on this host).
    pub port_id: u16,
    /// Transport type (e.g. `tcp`, `rdma`).
    pub transport: String,
    /// Listening IP address (e.g. `0.0.0.0` for all interfaces).
    pub addr: String,
    /// TCP/RDMA port number as a string (default NVMe-oF port is `4420`).
    pub service_id: String,
    /// Address family (`ipv4` or `ipv6`).
    pub addr_family: String,
}

// ── Requests ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSubsystemRequest {
    /// Short name appended to NQN prefix
    pub name: String,
    /// Whether any host NQN is permitted to connect (default: true).
    pub allow_any_host: Option<bool>,
    /// Block device path (e.g. /dev/loop0). When provided, a namespace is
    /// automatically created.
    pub device_path: Option<String>,
    /// Filled authoritatively by the engine router; not accepted from clients.
    #[serde(skip)]
    #[schemars(skip)]
    pub backing_volume: Option<BlockVolumeId>,
    /// Resolved once by the router for both scoped authorization and creation.
    #[serde(skip)]
    #[schemars(skip)]
    pub resolved_nqn: Option<String>,
    /// Listen address (default 0.0.0.0). Only used when `device_path` is set.
    pub addr: Option<String>,
    /// Port number (default 4420). Only used when `device_path` is set.
    pub port: Option<u16>,
    /// Host NQNs to allow. When provided, `allow_any_host` is set to false
    /// and only these hosts are permitted.
    pub allowed_hosts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSubsystemRequest {
    /// ID of the NVMe-oF subsystem to delete.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddNamespaceRequest {
    /// ID of the subsystem to add the namespace to.
    pub subsystem_id: String,
    /// Block device path (e.g. /dev/sdc)
    pub device_path: String,
    /// Filled authoritatively by the engine router; not accepted from clients.
    #[serde(skip)]
    #[schemars(skip)]
    pub backing_volume: Option<BlockVolumeId>,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceRemapOutcome {
    pub safe_to_restore: bool,
    pub changed: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveNamespaceRequest {
    /// ID of the subsystem from which to remove the namespace.
    pub subsystem_id: String,
    /// Namespace ID to remove.
    pub nsid: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepairNamespaceRequest {
    /// ID of the subsystem containing the unresolved namespace.
    pub subsystem_id: String,
    /// Namespace whose managed backing identity should be repaired.
    pub nsid: u32,
    /// Currently attached managed block-subvolume device.
    pub device_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddPortRequest {
    /// ID of the subsystem to add the port to.
    pub subsystem_id: String,
    /// "tcp" or "rdma"
    pub transport: Option<String>,
    /// Listening IP address (default `0.0.0.0`).
    pub addr: Option<String>,
    /// Port number (default 4420)
    pub service_id: Option<u16>,
    /// Address family (`ipv4` or `ipv6`; default `ipv4`).
    pub addr_family: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemovePortRequest {
    /// ID of the subsystem from which to remove the port.
    pub subsystem_id: String,
    /// Port ID to remove.
    pub port_id: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddHostRequest {
    /// ID of the subsystem to which to grant access.
    pub subsystem_id: String,
    /// NQN of the host to allow.
    pub host_nqn: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveHostRequest {
    /// ID of the subsystem from which to revoke access.
    pub subsystem_id: String,
    /// NQN of the host to disallow.
    pub host_nqn: String,
}

impl HasId for NvmeofSubsystem {
    fn id(&self) -> &str {
        &self.id
    }
}

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

static PREFIX_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn normalize_nqn_prefix(value: &str) -> Result<String, NvmeofError> {
    let prefix = value.trim().to_ascii_lowercase();
    let valid = prefix
        .strip_prefix("nqn.")
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(date, domain)| {
            let (year, month) = date.split_once('-').unwrap_or(("", ""));
            year.len() == 4
                && year.bytes().all(|c| c.is_ascii_digit())
                && month.len() == 2
                && month.parse::<u8>().is_ok_and(|m| (1..=12).contains(&m))
                && domain.split('.').count() >= 2
                && domain.split('.').all(|label| {
                    !label.is_empty()
                        && label
                            .bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                })
        });
    if !valid {
        return Err(NvmeofError::ConfigFs(
            "Base NQN must look like nqn.YYYY-MM.reversed.domain (letters, digits, '-' and '.' only)"
                .to_string(),
        ));
    }
    Ok(prefix)
}

fn build_subsystem_nqn(prefix: &str, name: &str) -> Result<String, NvmeofError> {
    validate_subsystem_name(name)?;
    let nqn = format!("{}:{name}", normalize_nqn_prefix(prefix)?);
    if nqn.len() > 223 {
        return Err(NvmeofError::ConfigFs(
            "NVMe-oF subsystem NQN exceeds 223 bytes".to_string(),
        ));
    }
    Ok(nqn)
}

async fn base_nqn_prefix() -> Result<String, NvmeofError> {
    let value = match tokio::fs::read_to_string(BASE_NQN_PATH).await {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            DEFAULT_NQN_PREFIX.to_string()
        }
        Err(error) => return Err(error.into()),
    };
    normalize_nqn_prefix(&value)
}

async fn prefix_history_in(dir: &Path) -> Result<std::collections::BTreeSet<String>, NvmeofError> {
    let path = dir.join("history.json");
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_str(&text)
        .map_err(|error| NvmeofError::ConfigFs(format!("parse {}: {error}", path.display())))
}

async fn remember_prefix_in(dir: &Path, prefix: &str) -> Result<(), NvmeofError> {
    let mut prefixes = prefix_history_in(dir).await?;
    if prefixes.insert(prefix.to_string()) {
        StateDir::new(dir).save("history", &prefixes).await?;
    }
    Ok(())
}

async fn prefix_history() -> Result<std::collections::BTreeSet<String>, NvmeofError> {
    prefix_history_in(Path::new(PREFIX_HISTORY_DIR)).await
}

async fn remember_prefix(prefix: &str) -> Result<(), NvmeofError> {
    remember_prefix_in(Path::new(PREFIX_HISTORY_DIR), prefix).await
}

fn persisted_prefixes(persisted: &[NvmeofSubsystem]) -> std::collections::BTreeSet<String> {
    let mut prefixes = std::collections::BTreeSet::from([DEFAULT_NQN_PREFIX.to_string()]);
    for subsystem in persisted {
        if let Some((prefix, _)) = subsystem.nqn.split_once(':') {
            prefixes.insert(prefix.to_string());
        }
    }
    prefixes
}

async fn managed_prefixes(
    persisted: &[NvmeofSubsystem],
) -> Result<std::collections::BTreeSet<String>, NvmeofError> {
    let mut prefixes = persisted_prefixes(persisted);
    prefixes.extend(prefix_history().await?);
    // Older versions could save invalid, unused Base NQNs. Keep recovery
    // driven by persisted subsystem identities even if that setting is bad.
    if let Ok(prefix) = base_nqn_prefix().await {
        prefixes.insert(prefix);
    }
    Ok(prefixes)
}

fn is_managed_nqn(nqn: &str, prefixes: &std::collections::BTreeSet<String>) -> bool {
    prefixes.iter().any(|prefix| {
        nqn.strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with(':'))
    })
}

/// Derive next port ID by scanning existing ports in configfs.
async fn next_port_id() -> u16 {
    let ports_dir = format!("{NVMET_BASE}/ports");
    let mut max_id: u16 = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(&ports_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && let Ok(id) = name.parse::<u16>()
            {
                max_id = max_id.max(id);
            }
        }
    }
    max_id + 1
}

// ── Service ─────────────────────────────────────────────────────

pub struct NvmeofService;

impl Default for NvmeofService {
    fn default() -> Self {
        Self::new()
    }
}

impl NvmeofService {
    pub fn new() -> Self {
        Self
    }

    pub async fn update_base_nqn_prefix(value: &str) -> Result<String, NvmeofError> {
        let prefix = normalize_nqn_prefix(value)?;
        let _guard = PREFIX_LOCK.lock().await;
        // Older versions accepted arbitrary text here but never used it to
        // create subsystems. Let an operator replace an invalid saved value.
        let previous = match base_nqn_prefix().await {
            Ok(previous) => previous,
            Err(NvmeofError::ConfigFs(_)) => DEFAULT_NQN_PREFIX.to_string(),
            Err(error) => return Err(error),
        };
        remember_prefix(&previous).await?;
        remember_prefix(&prefix).await?;
        tokio::fs::write(BASE_NQN_PATH, &prefix).await?;
        Ok(prefix)
    }

    pub async fn nqn_for_subsystem(name: &str) -> Result<String, NvmeofError> {
        validate_subsystem_name(name)?;
        let _guard = PREFIX_LOCK.lock().await;
        let prefix = base_nqn_prefix().await?;
        let nqn = build_subsystem_nqn(&prefix, name)?;
        remember_prefix(&prefix).await?;
        Ok(nqn)
    }

    pub async fn repair_namespace(
        &self,
        req: RepairNamespaceRequest,
        identity: BlockVolumeId,
    ) -> Result<(NvmeofSubsystem, NvmeofSubsystem), NvmeofError> {
        let mut subsystem = state_dir()
            .load::<NvmeofSubsystem>(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;
        let previous = subsystem.clone();
        repair_subsystem_namespace(&mut subsystem, req.nsid, req.device_path, identity)?;
        state_dir().save(&subsystem.id, &subsystem).await?;
        Ok((subsystem, previous))
    }

    pub async fn restore_subsystem_state(
        &self,
        subsystem: &NvmeofSubsystem,
    ) -> Result<(), NvmeofError> {
        state_dir().save(&subsystem.id, subsystem).await?;
        Ok(())
    }

    /// Restore NVMe-oF configfs state from persisted JSON files.
    /// Called at startup — configfs is volatile and lost on reboot.
    pub async fn restore(&self) -> Result<(), NvmeofError> {
        // Only restore if the nvmeof protocol is enabled
        let proto_state: serde_json::Value =
            tokio::fs::read_to_string("/var/lib/nasty/protocols.json")
                .await
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();

        if proto_state.get("nvmeof").and_then(|v| v.as_bool()) != Some(true) {
            info!("NVMe-oF protocol disabled, skipping restore");
            return Ok(());
        }

        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all_strict().await?;
        if subsystems.is_empty() {
            info!("No NVMe-oF subsystems to restore");
            return Ok(());
        }
        let mut failures = Vec::new();

        for subsys in &subsystems {
            info!("Restoring NVMe-oF subsystem: {}", subsys.nqn);

            // Create subsystem
            let subsys_path = format!("{NVMET_BASE}/subsystems/{}", subsys.nqn);
            if let Err(e) = configfs_ensure_dir(&subsys_path).await {
                warn!("Failed to create subsystem {}: {e}", subsys.nqn);
                failures.push(format!("{} subsystem: {e}", subsys.nqn));
                continue;
            }
            if let Err(error) = configfs_write(
                &format!("{subsys_path}/attr_allow_any_host"),
                if subsys.allow_any_host { "1" } else { "0" },
            )
            .await
            {
                warn!(
                    "Failed to restore access policy for {}: {error}",
                    subsys.nqn
                );
                failures.push(format!("{} access policy: {error}", subsys.nqn));
                continue;
            }

            // Restore namespaces
            for ns in &subsys.namespaces {
                let ns_path = format!("{subsys_path}/namespaces/{}", ns.nsid);
                if let Err(e) = configfs_ensure_dir(&ns_path).await {
                    warn!("  Failed to create namespace {}: {e}", ns.nsid);
                    failures.push(format!("{} namespace {}: {e}", subsys.nqn, ns.nsid));
                    continue;
                }
                if let Err(error) = restore_namespace(&ns_path, ns).await {
                    warn!("  Failed to restore namespace {} safely: {error}", ns.nsid);
                    failures.push(format!("{} namespace {}: {error}", subsys.nqn, ns.nsid));
                    continue;
                }
                info!("  Restored namespace {} -> {}", ns.nsid, ns.device_path);
            }

            // Restore allowed hosts
            for host_nqn in &subsys.allowed_hosts {
                let host_path = format!("{NVMET_BASE}/hosts/{host_nqn}");
                if let Err(error) = configfs_ensure_dir(&host_path).await {
                    warn!("  Failed to restore allowed host {host_nqn}: {error}");
                    failures.push(format!("{} host {host_nqn}: {error}", subsys.nqn));
                    continue;
                }
                let link = format!("{subsys_path}/allowed_hosts/{host_nqn}");
                if let Err(error) = configfs_ensure_symlink(&host_path, &link).await {
                    warn!("  Failed to link allowed host {host_nqn}: {error}");
                    failures.push(format!("{} host {host_nqn}: {error}", subsys.nqn));
                }
            }

            // A port bound to an address that is not up yet (Tailscale, a late
            // DHCP lease or VLAN) is not a failure: nothing listens there, so
            // nothing is exposed, and `bind_pending_ports` binds it when the
            // address appears (#625). Failing the restore here closed the
            // firewall for every NVMe-oF port.
            for port in &subsys.ports {
                if !addr_is_local(&port.addr) {
                    info!(
                        "  NVMe-oF port {}:{} waits for its address to come up",
                        port.addr, port.service_id
                    );
                    continue;
                }
                match bind_port(subsys, port).await {
                    Ok(port_id) => info!(
                        "  Restored port {} ({}:{})",
                        port_id, port.addr, port.service_id
                    ),
                    Err(e) => {
                        warn!(
                            "  NVMe-oF port bind to {}:{} failed: {e}",
                            port.addr, port.service_id
                        );
                        failures.push(format!("{} port {}: {e}", subsys.nqn, port.port_id));
                    }
                }
            }
        }

        info!("NVMe-oF restore complete");
        if failures.is_empty() {
            Ok(())
        } else {
            Err(NvmeofError::ConfigFs(failures.join("; ")))
        }
    }

    /// Bind restored subsystems' ports whose address has come up since. Run on
    /// every address watch tick; a port that is already bound is left alone.
    pub async fn bind_pending_ports(&self) {
        let Ok(subsystems) = state_dir().load_all_strict::<NvmeofSubsystem>().await else {
            return;
        };
        for subsys in &subsystems {
            let subsys_path = format!("{NVMET_BASE}/subsystems/{}", subsys.nqn);
            // Not in configfs: not restored, nothing to bind. (A subsystem
            // quiesced as unsafe stays, but its namespaces are disabled and
            // its firewall closed, so binding it exposes nothing.)
            if tokio::fs::metadata(&subsys_path).await.is_err() {
                continue;
            }
            for port in subsys.ports.iter().filter(|p| !is_wildcard_addr(&p.addr)) {
                if !addr_is_local(&port.addr) || port_is_bound(subsys, port).await {
                    continue;
                }
                match bind_port(subsys, port).await {
                    Ok(port_id) => info!(
                        "NVMe-oF: bound port {port_id} ({}:{}) for {} now that its address is up",
                        port.addr, port.service_id, subsys.nqn
                    ),
                    Err(e) => warn!(
                        "NVMe-oF: binding {}:{} for {} failed: {e}",
                        port.addr, port.service_id, subsys.nqn
                    ),
                }
            }
        }
    }

    /// Disable every NASty-managed live namespace without changing persisted
    /// state. nvmet configfs survives an engine-process restart, so merely
    /// skipping restore is not sufficient when backing identity is unsafe.
    pub async fn quiesce(&self) -> Result<(), NvmeofError> {
        let (persisted, state_error) = match state_dir().load_all_strict().await {
            Ok(persisted) => (persisted, None),
            Err(error) => {
                warn!("Cannot load NVMe-oF state for quiesce: {error}");
                (Vec::new(), Some(NvmeofError::Io(error)))
            }
        };
        let (prefixes, history_error) = match managed_prefixes(&persisted).await {
            Ok(prefixes) => (prefixes, None),
            Err(error) => {
                // A damaged prefix history must not prevent us from disabling
                // the exports still identified by persisted subsystem state.
                warn!("Cannot load managed NVMe-oF prefix history: {error}");
                (persisted_prefixes(&persisted), Some(error))
            }
        };
        let subsystem_dir = format!("{NVMET_BASE}/subsystems");
        let mut subsystems = match tokio::fs::read_dir(&subsystem_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(NvmeofError::Io(error)),
        };
        while let Some(subsystem) = subsystems.next_entry().await? {
            let nqn = subsystem.file_name();
            let nqn = nqn.to_string_lossy();
            if !is_managed_nqn(&nqn, &prefixes) {
                continue;
            }
            let namespace_dir = subsystem.path().join("namespaces");
            let mut namespaces = match tokio::fs::read_dir(&namespace_dir).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(NvmeofError::Io(error)),
            };
            while let Some(namespace) = namespaces.next_entry().await? {
                let enable = namespace.path().join("enable");
                tokio::fs::write(&enable, "0").await.map_err(|error| {
                    NvmeofError::ConfigFs(format!("disable {}: {error}", enable.display()))
                })?;
            }
        }
        if let Some(error) = state_error.or(history_error) {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// Whether every persisted enabled namespace is currently enabled in
    /// configfs. Used to avoid disrupting healthy initiators on unrelated
    /// filesystem mounts while still recovering a previously quiesced export.
    pub async fn is_active(&self) -> Result<bool, NvmeofError> {
        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all_strict().await?;
        for subsystem in subsystems {
            for namespace in subsystem.namespaces.into_iter().filter(|ns| ns.enabled) {
                let enable = format!(
                    "{NVMET_BASE}/subsystems/{}/namespaces/{}/enable",
                    subsystem.nqn, namespace.nsid
                );
                match tokio::fs::read_to_string(&enable).await {
                    Ok(value) if value.trim() == "1" => {}
                    Ok(_) => return Ok(false),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(NvmeofError::Io(error)),
                }
            }
        }
        Ok(true)
    }

    /// Resolve every namespace independently by immutable backing identity.
    pub async fn remap_device_paths(&self, mappings: &BlockDeviceMappings) -> DeviceRemapOutcome {
        let mut subsystems: Vec<NvmeofSubsystem> = match state_dir().load_all_strict().await {
            Ok(subsystems) => subsystems,
            Err(error) => {
                warn!("Cannot safely load all NVMe-oF state: {error}");
                return DeviceRemapOutcome::default();
            }
        };
        let mut safe_to_restore = match validate_managed_configfs_inventory(&subsystems).await {
            Ok(()) => true,
            Err(error) => {
                warn!("Cannot validate live NVMe-oF inventory safely: {error}");
                false
            }
        };
        if subsystems.is_empty() {
            return DeviceRemapOutcome {
                safe_to_restore,
                changed: false,
            };
        }
        let mut any_changed = false;
        for subsys in &mut subsystems {
            let (changed, subsystem_safe) = remap_subsystem(subsys, mappings);
            any_changed |= changed;
            safe_to_restore &= subsystem_safe;
            if changed && let Err(error) = state_dir().save(&subsys.id, subsys).await {
                warn!(
                    "Failed to persist NVMe-oF block identities for '{}': {error}",
                    subsys.nqn
                );
                safe_to_restore = false;
            }
        }
        DeviceRemapOutcome {
            safe_to_restore,
            changed: any_changed,
        }
    }

    pub async fn list(&self) -> Result<Vec<NvmeofSubsystem>, NvmeofError> {
        Ok(state_dir().load_all_strict().await?)
    }

    pub async fn get(&self, id: &str) -> Result<NvmeofSubsystem, NvmeofError> {
        state_dir()
            .load::<NvmeofSubsystem>(id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(id.to_string()))
    }

    pub async fn create(
        &self,
        req: CreateSubsystemRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        validate_subsystem_name(&req.name)?;
        if let Some(ref hosts) = req.allowed_hosts {
            for host_nqn in hosts {
                validate_host_nqn(host_nqn)?;
            }
        }
        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all_strict().await?;
        let nqn = match req.resolved_nqn.as_deref() {
            Some(nqn) => nqn.to_string(),
            None => Self::nqn_for_subsystem(&req.name).await?,
        };

        if let Some(existing) = subsystems.into_iter().find(|s| s.nqn == nqn) {
            info!("NVMe-oF subsystem {nqn} already exists, returning existing (idempotent)");
            return Ok(existing);
        }

        // Explicit hosts mean a closed subsystem, as the request documents.
        let allow_any = req
            .allow_any_host
            .unwrap_or_else(|| req.allowed_hosts.as_ref().is_none_or(Vec::is_empty));

        let subsys_path = format!("{NVMET_BASE}/subsystems/{nqn}");
        configfs_mkdir(&subsys_path).await?;
        configfs_write(
            &format!("{subsys_path}/attr_allow_any_host"),
            if allow_any { "1" } else { "0" },
        )
        .await?;

        let subsystem = NvmeofSubsystem {
            id: Uuid::new_v4().to_string(),
            nqn: nqn.clone(),
            namespaces: vec![],
            ports: vec![],
            allowed_hosts: vec![],
            allow_any_host: allow_any,
            enabled: true,
        };

        state_dir()
            .save(&subsystem.id, &subsystem)
            .await
            .map_err(NvmeofError::Io)?;

        // Optional: add namespace if device_path was provided
        let mut subsystem = subsystem;
        if let Some(device_path) = req.device_path {
            if subsystem.namespaces.is_empty() {
                subsystem = self
                    .add_namespace(AddNamespaceRequest {
                        subsystem_id: subsystem.id.clone(),
                        device_path,
                        backing_volume: req.backing_volume,
                    })
                    .await?;
            } else {
                info!(
                    "Subsystem {} already has {} namespace(s), skipping",
                    subsystem.nqn,
                    subsystem.namespaces.len()
                );
            }

            // Add a port when a namespace was created
            if subsystem.ports.is_empty() {
                let svc_port = req.port.unwrap_or(4420);
                let explicit_addr = req.addr.clone();
                subsystem = self
                    .add_port(AddPortRequest {
                        subsystem_id: subsystem.id.clone(),
                        // Auto-created ports are deliberately TCP: RDMA ports are an
                        // explicit per-port choice via add_port (and Tailscale is an
                        // overlay — RDMA over it is meaningless).
                        transport: Some("tcp".to_string()),
                        addr: Some(req.addr.unwrap_or_else(|| "0.0.0.0".to_string())),
                        service_id: Some(svc_port),
                        addr_family: Some("ipv4".to_string()),
                    })
                    .await?;

                // Dual-stack auto-port: when the operator didn't pin a
                // specific address and the host has a routable v6
                // source, add a sibling port on the v6 address. Skips
                // silently on v4-only hosts and on the explicit-addr
                // path (operator chose v4 deliberately — don't surprise
                // them by adding a v6 port too).
                if explicit_addr.is_none()
                    && let Some(v6) = crate::v6::detect_primary_ipv6().await
                {
                    match self
                        .add_port(AddPortRequest {
                            subsystem_id: subsystem.id.clone(),
                            transport: Some("tcp".to_string()),
                            addr: Some(v6.clone()),
                            service_id: Some(svc_port),
                            addr_family: Some("ipv6".to_string()),
                        })
                        .await
                    {
                        Ok(s) => {
                            subsystem = s;
                            info!("Auto-added IPv6 sibling port {v6} for '{}'", subsystem.nqn);
                        }
                        Err(e) => {
                            // Don't fail the whole create — the v4 port
                            // is up and the subsystem is usable. Log
                            // and continue so the user gets a working
                            // (if v4-only) target.
                            warn!(
                                "auto-add IPv6 port for '{}' on {v6} failed: {e}",
                                subsystem.nqn
                            );
                        }
                    }
                }
            } else {
                info!(
                    "Subsystem {} already has {} port(s), skipping",
                    subsystem.nqn,
                    subsystem.ports.len()
                );
            }
        }

        // Optional: add allowed hosts if provided
        if let Some(hosts) = req.allowed_hosts {
            for host_nqn in hosts {
                subsystem = self
                    .add_host(AddHostRequest {
                        subsystem_id: subsystem.id.clone(),
                        host_nqn,
                    })
                    .await?;
            }
        }

        // Wait for readiness when a namespace was attached
        if !subsystem.namespaces.is_empty() {
            wait_for_subsystem_ready(&subsystem.nqn).await;
        }

        info!("Created NVMe-oF subsystem {nqn}");
        Ok(subsystem)
    }

    pub async fn delete(&self, req: DeleteSubsystemRequest) -> Result<(), NvmeofError> {
        let subsys: NvmeofSubsystem = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.id.clone()))?;

        // 1. Disable all namespaces first — stops new I/O from clients
        let ns_dir = format!("{NVMET_BASE}/subsystems/{}/namespaces", subsys.nqn);
        if let Ok(mut entries) = tokio::fs::read_dir(&ns_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let enable_path = entry.path().join("enable");
                let _ = configfs_write(&enable_path.to_string_lossy(), "0").await;
            }
        }

        // 2. Unlink from ports — triggers client-side disconnect
        for port in &subsys.ports {
            let link = format!(
                "{NVMET_BASE}/ports/{}/subsystems/{}",
                port.port_id, subsys.nqn
            );
            let _ = configfs_unlink(&link).await;
        }

        // 3. Brief settle for clients to process disconnect
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // 4. Remove namespaces
        if let Ok(mut entries) = tokio::fs::read_dir(&ns_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let ns_path = entry.path();
                let _ = configfs_rmdir(&ns_path.to_string_lossy()).await;
            }
        }

        // 5. Remove allowed hosts
        let hosts_dir = format!("{NVMET_BASE}/subsystems/{}/allowed_hosts", subsys.nqn);
        if let Ok(mut entries) = tokio::fs::read_dir(&hosts_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let link = entry.path();
                let _ = tokio::fs::remove_file(&link).await;
            }
        }

        // 6. Remove port directories if empty
        for port in &subsys.ports {
            let port_dir = format!("{NVMET_BASE}/ports/{}", port.port_id);
            let subsys_dir = format!("{port_dir}/subsystems");
            if dir_is_empty(&subsys_dir).await {
                let _ = configfs_rmdir(&port_dir).await;
            }
        }

        // 7. Remove subsystem
        let subsys_path = format!("{NVMET_BASE}/subsystems/{}", subsys.nqn);
        configfs_rmdir(&subsys_path).await?;

        state_dir().remove(&req.id).await.map_err(NvmeofError::Io)?;

        info!("Deleted NVMe-oF subsystem '{}'", req.id);
        Ok(())
    }

    pub async fn add_namespace(
        &self,
        req: AddNamespaceRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        if !Path::new(&req.device_path).exists() {
            return Err(NvmeofError::DeviceNotFound(req.device_path));
        }

        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let nsid = subsys
            .namespaces
            .iter()
            .map(|n| n.nsid)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);

        // Create namespace in configfs
        let ns_path = format!("{NVMET_BASE}/subsystems/{}/namespaces/{nsid}", subsys.nqn);
        configfs_mkdir(&ns_path).await?;
        configfs_write(&format!("{ns_path}/device_path"), &req.device_path).await?;
        // Direct I/O — vol.img may have nocow set (on non-encrypted filesystems),
        // so writes go in-place. On encrypted filesystems nocow is skipped to
        // preserve encryption and checksums.
        configfs_write(&format!("{ns_path}/buffered_io"), "0").await?;
        configfs_write(&format!("{ns_path}/enable"), "1").await?;

        subsys.namespaces.push(Namespace {
            nsid,
            device_path: req.device_path,
            enabled: true,
            backing_volume: req.backing_volume,
            backing_volume_unresolved: false,
        });

        state_dir()
            .save(&subsys.id, &subsys)
            .await
            .map_err(NvmeofError::Io)?;

        info!("Added namespace {nsid} to subsystem '{}'", subsys.nqn);
        Ok(subsys)
    }

    pub async fn remove_namespace(
        &self,
        req: RemoveNamespaceRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let ns_idx = subsys
            .namespaces
            .iter()
            .position(|n| n.nsid == req.nsid)
            .ok_or(NvmeofError::NamespaceNotFound(req.nsid))?;

        let ns_path = format!(
            "{NVMET_BASE}/subsystems/{}/namespaces/{}",
            subsys.nqn, req.nsid
        );
        configfs_rmdir(&ns_path).await?;

        subsys.namespaces.remove(ns_idx);

        state_dir()
            .save(&subsys.id, &subsys)
            .await
            .map_err(NvmeofError::Io)?;

        info!(
            "Removed namespace {} from subsystem '{}'",
            req.nsid, subsys.nqn
        );
        Ok(subsys)
    }

    pub async fn add_port(&self, req: AddPortRequest) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let transport = req.transport.unwrap_or_else(|| "tcp".to_string());
        validate_transport(&transport)?;
        let addr = match req.addr {
            Some(a) if !a.is_empty() && a != "0.0.0.0" => a,
            _ => {
                // NVMe-oF configfs requires a real IP — 0.0.0.0 causes EINVAL
                // when linking subsystems to ports. Detect the primary IP.
                detect_primary_ip()
                    .await
                    .unwrap_or_else(|| "0.0.0.0".to_string())
            }
        };
        let svc_id = req.service_id.unwrap_or(4420);
        let addr_family = req.addr_family.unwrap_or_else(|| "ipv4".to_string());

        // Check if an existing port already binds to the same address —
        // multiple subsystems can share a single NVMe-oF port.
        let port_id = if let Some(existing) =
            find_existing_port(&transport, &addr, svc_id, &addr_family).await
        {
            info!("Reusing existing port {existing} for {}:{svc_id}", addr);
            existing
        } else {
            let pid = next_port_id().await;
            // Create new port in configfs
            let port_path = format!("{NVMET_BASE}/ports/{pid}");
            configfs_mkdir(&port_path).await?;
            configfs_write(&format!("{port_path}/addr_trtype"), &transport).await?;
            configfs_write(&format!("{port_path}/addr_traddr"), &addr).await?;
            configfs_write(&format!("{port_path}/addr_trsvcid"), &svc_id.to_string()).await?;
            configfs_write(&format!("{port_path}/addr_adrfam"), &addr_family).await?;
            pid
        };

        // Link subsystem to port
        let port_path = format!("{NVMET_BASE}/ports/{port_id}");
        let link_path = format!("{port_path}/subsystems/{}", subsys.nqn);
        configfs_symlink(
            &format!("{NVMET_BASE}/subsystems/{}", subsys.nqn),
            &link_path,
        )
        .await?;

        let port = Port {
            port_id,
            transport,
            addr,
            service_id: svc_id.to_string(),
            addr_family,
        };

        subsys.ports.push(port);

        state_dir()
            .save(&subsys.id, &subsys)
            .await
            .map_err(NvmeofError::Io)?;

        info!("Added port {port_id} to subsystem '{}'", subsys.nqn);
        Ok(subsys)
    }

    pub async fn remove_port(
        &self,
        req: RemovePortRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let port_idx = subsys
            .ports
            .iter()
            .position(|p| p.port_id == req.port_id)
            .ok_or(NvmeofError::PortNotFound(req.port_id))?;

        // Unlink subsystem from port
        let link_path = format!(
            "{NVMET_BASE}/ports/{}/subsystems/{}",
            req.port_id, subsys.nqn
        );
        let _ = configfs_unlink(&link_path).await;

        // Remove port dir if no other subsystems use it
        let port_subsys_dir = format!("{NVMET_BASE}/ports/{}/subsystems", req.port_id);
        if dir_is_empty(&port_subsys_dir).await {
            let port_path = format!("{NVMET_BASE}/ports/{}", req.port_id);
            let _ = configfs_rmdir(&port_path).await;
        }

        subsys.ports.remove(port_idx);

        state_dir()
            .save(&subsys.id, &subsys)
            .await
            .map_err(NvmeofError::Io)?;

        info!(
            "Removed port {} from subsystem '{}'",
            req.port_id, subsys.nqn
        );
        Ok(subsys)
    }

    // ── Tailscale integration ────────────────────────────────

    /// Ensure every NVMe-oF subsystem has a port bound to the Tailscale IP.
    /// Called after Tailscale connects (or at boot when Tailscale is already up).
    pub async fn ensure_tailscale_ports(&self, tailscale_ip: &str) {
        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all().await;
        if subsystems.is_empty() {
            return;
        }

        info!("Syncing NVMe-oF ports for Tailscale IP {tailscale_ip}");

        for subsys in &subsystems {
            // Skip if this subsystem already has a port on the Tailscale IP
            if subsys.ports.iter().any(|p| p.addr == tailscale_ip) {
                continue;
            }

            // Pick transport/service_id from existing port, or use defaults
            let (transport, svc_id) = subsys
                .ports
                .first()
                .map(|p| (p.transport.clone(), p.service_id.clone()))
                .unwrap_or_else(|| ("tcp".to_string(), "4420".to_string()));

            let svc_port = svc_id.parse::<u16>().unwrap_or(4420);

            match self
                .add_port(AddPortRequest {
                    subsystem_id: subsys.id.clone(),
                    transport: Some(transport),
                    addr: Some(tailscale_ip.to_string()),
                    service_id: Some(svc_port),
                    addr_family: Some("ipv4".to_string()),
                })
                .await
            {
                Ok(_) => info!("Added Tailscale port for subsystem '{}'", subsys.nqn),
                Err(e) => warn!("Failed to add Tailscale port for '{}': {e}", subsys.nqn),
            }
        }
    }

    /// Remove NVMe-oF ports bound to a specific IP (e.g. when Tailscale disconnects).
    pub async fn remove_ports_for_ip(&self, ip: &str) {
        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all().await;

        for subsys in &subsystems {
            let tailscale_ports: Vec<u16> = subsys
                .ports
                .iter()
                .filter(|p| p.addr == ip)
                .map(|p| p.port_id)
                .collect();

            for port_id in tailscale_ports {
                match self
                    .remove_port(RemovePortRequest {
                        subsystem_id: subsys.id.clone(),
                        port_id,
                    })
                    .await
                {
                    Ok(_) => info!("Removed port {port_id} (IP {ip}) from '{}'", subsys.nqn),
                    Err(e) => warn!("Failed to remove port {port_id} from '{}': {e}", subsys.nqn),
                }
            }
        }
    }

    pub async fn add_host(&self, req: AddHostRequest) -> Result<NvmeofSubsystem, NvmeofError> {
        validate_host_nqn(&req.host_nqn)?;
        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        // Create host entry if it doesn't exist
        let host_path = format!("{NVMET_BASE}/hosts/{}", req.host_nqn);
        if !Path::new(&host_path).exists() {
            configfs_mkdir(&host_path).await?;
        }

        // Disable allow_any_host first: nvmet refuses (EINVAL) to link an
        // allowed host while it is set. Persist it so a failed link below
        // leaves the subsystem closed rather than open.
        let subsys_path = format!("{NVMET_BASE}/subsystems/{}", subsys.nqn);
        configfs_write(&format!("{subsys_path}/attr_allow_any_host"), "0").await?;
        if subsys.allow_any_host {
            subsys.allow_any_host = false;
            state_dir()
                .save(&subsys.id, &subsys)
                .await
                .map_err(NvmeofError::Io)?;
        }

        // Symlink into subsystem's allowed_hosts
        let link_path = format!("{subsys_path}/allowed_hosts/{}", req.host_nqn);
        configfs_symlink(&host_path, &link_path).await?;

        if !subsys.allowed_hosts.contains(&req.host_nqn) {
            subsys.allowed_hosts.push(req.host_nqn);
        }

        state_dir()
            .save(&subsys.id, &subsys)
            .await
            .map_err(NvmeofError::Io)?;

        info!("Added allowed host to subsystem '{}'", subsys.nqn);
        Ok(subsys)
    }

    pub async fn remove_host(
        &self,
        req: RemoveHostRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let link_path = format!(
            "{NVMET_BASE}/subsystems/{}/allowed_hosts/{}",
            subsys.nqn, req.host_nqn
        );
        configfs_unlink(&link_path).await?;

        subsys.allowed_hosts.retain(|h| h != &req.host_nqn);

        state_dir()
            .save(&subsys.id, &subsys)
            .await
            .map_err(NvmeofError::Io)?;

        info!("Removed allowed host from subsystem '{}'", subsys.nqn);
        Ok(subsys)
    }
}

// ── port reuse ─────────────────────────────────────────────────

/// Scan all existing configfs ports for one matching the given transport/addr/service_id.
/// Returns the port_id if found, so multiple subsystems can share a single listener.
/// True for the bind-any addresses, which the kernel accepts regardless
/// of link state.
pub(crate) fn is_wildcard_addr(addr: &str) -> bool {
    addr == "0.0.0.0" || addr == "::"
}

/// Whether `addr` is currently assigned to a local interface — tested by
/// attempting a socket bind to it, which fails with EADDRNOTAVAIL exactly
/// when nvmet's port bind would. Wildcards and unparseable addresses are
/// treated as ready (never block on them).
pub(crate) fn addr_is_local(addr: &str) -> bool {
    if is_wildcard_addr(addr) {
        return true;
    }
    match addr.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::TcpListener::bind((ip, 0)).is_ok(),
        Err(_) => true,
    }
}

/// Bind `subsys` to `port`, reusing a configfs port that already listens on
/// the same address. Returns the configfs port id. Idempotent.
async fn bind_port(subsys: &NvmeofSubsystem, port: &Port) -> Result<u16, NvmeofError> {
    let svc_id: u16 = port.service_id.parse().unwrap_or(4420);
    let port_id =
        match find_existing_port(&port.transport, &port.addr, svc_id, &port.addr_family).await {
            Some(existing) => existing,
            None => {
                let port_path = format!("{NVMET_BASE}/ports/{}", port.port_id);
                configfs_ensure_dir(&port_path).await?;
                configfs_write(&format!("{port_path}/addr_trtype"), &port.transport).await?;
                configfs_write(&format!("{port_path}/addr_traddr"), &port.addr).await?;
                configfs_write(&format!("{port_path}/addr_trsvcid"), &port.service_id).await?;
                configfs_write(&format!("{port_path}/addr_adrfam"), &port.addr_family).await?;
                port.port_id
            }
        };
    // Linking the subsystem is what triggers the kernel transport bind, so
    // this is where EADDRNOTAVAIL surfaces.
    configfs_ensure_symlink(
        &format!("{NVMET_BASE}/subsystems/{}", subsys.nqn),
        &format!("{NVMET_BASE}/ports/{port_id}/subsystems/{}", subsys.nqn),
    )
    .await?;
    Ok(port_id)
}

async fn port_is_bound(subsys: &NvmeofSubsystem, port: &Port) -> bool {
    let svc_id: u16 = port.service_id.parse().unwrap_or(4420);
    match find_existing_port(&port.transport, &port.addr, svc_id, &port.addr_family).await {
        Some(port_id) => tokio::fs::symlink_metadata(format!(
            "{NVMET_BASE}/ports/{port_id}/subsystems/{}",
            subsys.nqn
        ))
        .await
        .is_ok(),
        None => false,
    }
}

async fn find_existing_port(
    transport: &str,
    addr: &str,
    svc_id: u16,
    addr_family: &str,
) -> Option<u16> {
    let mut entries = tokio::fs::read_dir(format!("{NVMET_BASE}/ports"))
        .await
        .ok()?;
    let svc_str = svc_id.to_string();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let port_id: u16 = match name.to_str().and_then(|s| s.parse().ok()) {
            Some(id) => id,
            None => continue,
        };
        let port_path = format!("{NVMET_BASE}/ports/{port_id}");

        async fn read_attr(base: &str, attr: &str) -> Option<String> {
            tokio::fs::read_to_string(format!("{base}/{attr}"))
                .await
                .ok()
                .map(|s| s.trim().to_string())
        }

        let existing_transport = read_attr(&port_path, "addr_trtype").await;
        let existing_addr = read_attr(&port_path, "addr_traddr").await;
        let existing_svc = read_attr(&port_path, "addr_trsvcid").await;
        let existing_fam = read_attr(&port_path, "addr_adrfam").await;

        if existing_transport.as_deref() == Some(transport)
            && existing_addr.as_deref() == Some(addr)
            && existing_svc.as_deref() == Some(&svc_str)
            && existing_fam.as_deref() == Some(addr_family)
        {
            return Some(port_id);
        }
    }
    None
}

// ── configfs helpers ────────────────────────────────────────────

async fn configfs_mkdir(path: &str) -> Result<(), NvmeofError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|e| NvmeofError::ConfigFs(format!("mkdir {path}: {e}")))
}

async fn configfs_ensure_dir(path: &str) -> Result<(), NvmeofError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(NvmeofError::ConfigFs(format!(
            "{path} exists and is not a directory"
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => configfs_mkdir(path).await,
        Err(error) => Err(NvmeofError::ConfigFs(format!("stat {path}: {error}"))),
    }
}

async fn configfs_rmdir(path: &str) -> Result<(), NvmeofError> {
    tokio::fs::remove_dir(path)
        .await
        .map_err(|e| NvmeofError::ConfigFs(format!("rmdir {path}: {e}")))
}

/// Remove a symlink in configfs (e.g. port->subsystem links, allowed_hosts)
async fn configfs_unlink(path: &str) -> Result<(), NvmeofError> {
    tokio::fs::remove_file(path)
        .await
        .map_err(|e| NvmeofError::ConfigFs(format!("unlink {path}: {e}")))
}

async fn configfs_write(path: &str, value: &str) -> Result<(), NvmeofError> {
    tokio::fs::write(path, value)
        .await
        .map_err(|e| NvmeofError::ConfigFs(format!("write {path}={value}: {e}")))
}

async fn configfs_symlink(target: &str, link: &str) -> Result<(), NvmeofError> {
    tokio::fs::symlink(target, link)
        .await
        .map_err(|e| NvmeofError::ConfigFs(format!("symlink {link} -> {target}: {e}")))
}

async fn configfs_ensure_symlink(target: &str, link: &str) -> Result<(), NvmeofError> {
    match tokio::fs::read_link(link).await {
        Ok(existing) if existing == Path::new(target) => Ok(()),
        Ok(existing) => {
            let resolved_link = tokio::fs::canonicalize(link).await;
            let resolved_target = tokio::fs::canonicalize(target).await;
            if matches!((&resolved_link, &resolved_target), (Ok(link), Ok(target)) if link == target)
            {
                Ok(())
            } else {
                Err(NvmeofError::ConfigFs(format!(
                    "symlink {link} points to {} instead of {target}",
                    existing.display()
                )))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            configfs_symlink(target, link).await
        }
        Err(error) => Err(NvmeofError::ConfigFs(format!("readlink {link}: {error}"))),
    }
}

async fn dir_is_empty(path: &str) -> bool {
    match tokio::fs::read_dir(path).await {
        Ok(mut entries) => entries.next_entry().await.ok().flatten().is_none(),
        Err(_) => true,
    }
}

/// Wait for an NVMe-oF subsystem to be ready for initiator connections.
/// Checks that namespace 1 is enabled in configfs. Polls up to 5 seconds.
async fn wait_for_subsystem_ready(nqn: &str) {
    let ns_enable = format!("{NVMET_BASE}/subsystems/{nqn}/namespaces/1/enable");

    for attempt in 1..=10 {
        match tokio::fs::read_to_string(&ns_enable).await {
            Ok(val) if val.trim() == "1" => {
                info!("NVMe-oF subsystem {nqn} is ready (attempt {attempt})");
                return;
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    warn!("NVMe-oF subsystem {nqn} readiness check timed out — proceeding anyway");
}

/// Detect the primary IPv4 address via `ip route get 1.1.1.1`.
async fn detect_primary_ip() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-4", "route", "get", "1.1.1.1"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Output: "1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.5 uid 0"
    let mut iter = text.split_whitespace();
    while let Some(token) = iter.next() {
        if token == "src" {
            return iter.next().map(|s| s.to_string());
        }
    }
    None
}

fn is_managed_namespace(namespace: &Namespace) -> bool {
    namespace.backing_volume.is_some()
        || namespace.backing_volume_unresolved
        || namespace.device_path.starts_with("/dev/loop")
}

fn repair_subsystem_namespace(
    subsystem: &mut NvmeofSubsystem,
    nsid: u32,
    device_path: String,
    identity: BlockVolumeId,
) -> Result<(), NvmeofError> {
    let namespace = subsystem
        .namespaces
        .iter_mut()
        .find(|namespace| namespace.nsid == nsid)
        .ok_or(NvmeofError::NamespaceNotFound(nsid))?;
    if !namespace.backing_volume_unresolved && namespace.backing_volume.is_some() {
        return Err(NvmeofError::InvalidBackingVolume(format!(
            "namespace {nsid} is already resolved"
        )));
    }
    if !namespace.device_path.starts_with("/dev/loop") {
        return Err(NvmeofError::InvalidBackingVolume(format!(
            "namespace {nsid} is not an unresolved managed block volume"
        )));
    }
    namespace.device_path = device_path;
    namespace.backing_volume = Some(identity);
    namespace.backing_volume_unresolved = false;
    Ok(())
}

async fn restore_namespace(ns_path: &str, namespace: &Namespace) -> Result<(), NvmeofError> {
    configfs_write(&format!("{ns_path}/enable"), "0").await?;
    if !Path::new(&namespace.device_path).exists() {
        return Err(NvmeofError::DeviceNotFound(namespace.device_path.clone()));
    }
    configfs_write(&format!("{ns_path}/device_path"), &namespace.device_path).await?;
    configfs_write(&format!("{ns_path}/buffered_io"), "0").await?;
    if namespace.enabled {
        configfs_write(&format!("{ns_path}/enable"), "1").await?;
    }
    Ok(())
}

async fn validate_managed_configfs_inventory(
    expected: &[NvmeofSubsystem],
) -> Result<(), NvmeofError> {
    let prefixes = managed_prefixes(expected).await?;
    let expected: std::collections::HashMap<&str, std::collections::HashSet<u32>> = expected
        .iter()
        .map(|subsystem| {
            (
                subsystem.nqn.as_str(),
                subsystem
                    .namespaces
                    .iter()
                    .map(|namespace| namespace.nsid)
                    .collect(),
            )
        })
        .collect();
    let subsystem_dir = format!("{NVMET_BASE}/subsystems");
    let mut subsystems = match tokio::fs::read_dir(&subsystem_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(NvmeofError::Io(error)),
    };
    while let Some(subsystem) = subsystems.next_entry().await? {
        let nqn = subsystem.file_name().to_string_lossy().to_string();
        if !is_managed_nqn(&nqn, &prefixes) {
            continue;
        }
        let Some(expected_namespaces) = expected.get(nqn.as_str()) else {
            return Err(NvmeofError::ConfigFs(format!(
                "live subsystem '{nqn}' has no persisted state"
            )));
        };
        let mut namespaces = match tokio::fs::read_dir(subsystem.path().join("namespaces")).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(NvmeofError::Io(error)),
        };
        while let Some(namespace) = namespaces.next_entry().await? {
            let nsid = namespace
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .map_err(|_| {
                    NvmeofError::ConfigFs(format!(
                        "live subsystem '{nqn}' has invalid namespace entry '{}'",
                        namespace.file_name().to_string_lossy()
                    ))
                })?;
            if !expected_namespaces.contains(&nsid) {
                return Err(NvmeofError::ConfigFs(format!(
                    "live subsystem '{nqn}' namespace {nsid} has no persisted state"
                )));
            }
        }
    }
    Ok(())
}

fn remap_subsystem(
    subsystem: &mut NvmeofSubsystem,
    mappings: &BlockDeviceMappings,
) -> (bool, bool) {
    let mut changed = false;
    let mut safe = true;

    for namespace in &mut subsystem.namespaces {
        if !is_managed_namespace(namespace) {
            continue;
        }
        let identity = namespace.backing_volume.clone().or_else(|| {
            mappings
                .legacy_identity_for_exact_path(&namespace.device_path)
                .cloned()
        });
        let resolved = identity
            .as_ref()
            .and_then(|identity| mappings.current.get(identity))
            .cloned();

        match (identity, resolved) {
            (Some(identity), Some(device_path)) => {
                if namespace.backing_volume.as_ref() != Some(&identity) {
                    namespace.backing_volume = Some(identity);
                    changed = true;
                }
                if namespace.device_path != device_path {
                    info!(
                        "Remapping NVMe-oF '{}' ns{} {} → {}",
                        subsystem.nqn, namespace.nsid, namespace.device_path, device_path
                    );
                    namespace.device_path = device_path;
                    changed = true;
                }
                if namespace.backing_volume_unresolved {
                    namespace.backing_volume_unresolved = false;
                    changed = true;
                }
            }
            _ => {
                warn!(
                    "Cannot safely resolve NVMe-oF '{}' ns{} backing volume; blocking NVMe-oF restore",
                    subsystem.nqn, namespace.nsid
                );
                if !namespace.backing_volume_unresolved {
                    namespace.backing_volume_unresolved = true;
                    changed = true;
                }
                safe = false;
            }
        }
    }

    (changed, safe)
}

/// Validate the user-supplied portion of an NVMe-oF subsystem name.
/// The engine builds the full NQN as `nqn.2137-04.storage.nasty:<name>`
/// and uses that string as a configfs directory name. NVMe-oF (NVMe
/// base spec 1.4 §7.9) requires NQNs to be UTF-8 within 223 bytes —
/// but configfs is far less forgiving. Restrict to the same conservative
/// set as iSCSI target names (A-Z, a-z, 0-9, '-', '.', '_', ':') so a
/// name can't escape the subsystem directory via '/' or smuggle
/// newlines into state files.
fn validate_subsystem_name(name: &str) -> Result<(), NvmeofError> {
    if name.is_empty() {
        return Err(NvmeofError::ConfigFs(
            "NVMe-oF subsystem name is empty".to_string(),
        ));
    }
    if name.len() > 200 {
        return Err(NvmeofError::ConfigFs(
            "NVMe-oF subsystem name exceeds 200 chars".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
    {
        return Err(NvmeofError::ConfigFs(format!(
            "NVMe-oF subsystem name '{name}' contains invalid characters \
             (allowed: A-Z, a-z, 0-9, '-', '.', '_', ':')"
        )));
    }
    Ok(())
}

/// Validate an allowed-host NQN string. Unlike the subsystem name, the
/// host NQN is a full NQN supplied by the caller (initiators identify
/// themselves with strings like `nqn.2014-08.org.nvmexpress:uuid:...`).
/// We can't enforce the full NQN grammar here without breaking valid
/// initiator names, but we *can* reject the characters that would
/// escape the configfs hosts directory or break the `allowed_hosts`
/// symlink path.
fn validate_host_nqn(nqn: &str) -> Result<(), NvmeofError> {
    if nqn.is_empty() {
        return Err(NvmeofError::ConfigFs("host NQN is empty".to_string()));
    }
    if nqn.len() > 223 {
        // NVMe base spec 1.4 §7.9: NQN is at most 223 bytes.
        return Err(NvmeofError::ConfigFs(
            "host NQN exceeds 223 chars (NVMe spec limit)".to_string(),
        ));
    }
    if !nqn.starts_with("nqn.") {
        return Err(NvmeofError::ConfigFs(format!(
            "host NQN '{nqn}' must start with 'nqn.' (per NVMe spec)"
        )));
    }
    if nqn.contains('/') || nqn.contains('\\') || nqn.chars().any(|c| c.is_control() || c == ' ') {
        return Err(NvmeofError::ConfigFs(format!(
            "host NQN '{nqn}' contains invalid characters \
             (must not contain '/', '\\', whitespace, or control characters)"
        )));
    }
    Ok(())
}

/// The two transports nvmet supports on NASty. Lowercase only —
/// that's what configfs `addr_trtype` accepts; anything else used to
/// be written through verbatim and die as an opaque configfs EINVAL.
fn validate_transport(t: &str) -> Result<(), NvmeofError> {
    match t {
        "tcp" | "rdma" => Ok(()),
        other => Err(NvmeofError::InvalidTransport(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_nqn_prefix_is_validated_and_used_for_new_names() {
        assert_eq!(
            build_subsystem_nqn(" NQN.2026-09.Com.Example ", "fast-store").unwrap(),
            "nqn.2026-09.com.example:fast-store"
        );
        assert!(build_subsystem_nqn("nqn.2026-13.com.example", "fast-store").is_err());
        assert!(build_subsystem_nqn("nqn.2026-09.com.example", "bad/name").is_err());
        assert!(build_subsystem_nqn("nqn.2026-09.com.example", &"x".repeat(200)).is_err());
    }

    #[test]
    fn managed_prefix_match_keeps_old_names_and_ignores_foreign_targets() {
        let prefixes = [
            DEFAULT_NQN_PREFIX.to_string(),
            "nqn.2026-09.com.example".to_string(),
        ]
        .into_iter()
        .collect();
        assert!(is_managed_nqn("nqn.2137-04.storage.nasty:old", &prefixes));
        assert!(is_managed_nqn("nqn.2026-09.com.example:new", &prefixes));
        assert!(!is_managed_nqn(
            "nqn.2026-09.com.example-extra:foreign",
            &prefixes
        ));
        assert!(!is_managed_nqn(
            "nqn.2014-08.org.nvmexpress.discovery",
            &prefixes
        ));
    }

    #[test]
    fn client_cannot_choose_resolved_subsystem_nqn() {
        let req: CreateSubsystemRequest = serde_json::from_value(serde_json::json!({
            "name": "fast-store",
            "resolved_nqn": "nqn.2026-09.other.host:fast-store"
        }))
        .unwrap();
        assert!(req.resolved_nqn.is_none());
    }

    #[tokio::test]
    async fn prefix_history_survives_changes_and_fails_closed_when_corrupt() {
        let dir = std::env::temp_dir().join(format!("nasty-nqn-history-{}", Uuid::new_v4()));
        remember_prefix_in(&dir, DEFAULT_NQN_PREFIX).await.unwrap();
        remember_prefix_in(&dir, "nqn.2026-09.com.example")
            .await
            .unwrap();
        let history = prefix_history_in(&dir).await.unwrap();
        assert!(is_managed_nqn("nqn.2137-04.storage.nasty:old", &history));
        assert!(is_managed_nqn("nqn.2026-09.com.example:new", &history));
        tokio::fs::write(dir.join("history.json"), "not JSON")
            .await
            .unwrap();
        assert!(prefix_history_in(&dir).await.is_err());
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    fn volume(pool: &str, id: u32) -> BlockVolumeId {
        BlockVolumeId {
            filesystem_uuid: pool.to_string(),
            subvolume_id: id,
        }
    }

    fn namespace(nsid: u32, path: &str, identity: Option<BlockVolumeId>) -> Namespace {
        Namespace {
            nsid,
            device_path: path.to_string(),
            enabled: true,
            backing_volume: identity,
            backing_volume_unresolved: false,
        }
    }

    fn subsystem(name: &str, namespaces: Vec<Namespace>) -> NvmeofSubsystem {
        NvmeofSubsystem {
            id: "subsystem-id".to_string(),
            nqn: format!("nqn.test:{name}"),
            namespaces,
            ports: vec![],
            allowed_hosts: vec![],
            allow_any_host: true,
            enabled: true,
        }
    }

    #[test]
    fn remaps_each_namespace_by_immutable_identity() {
        let first = volume("pool-a", 10);
        let second = volume("pool-b", 20);
        let mut mappings = BlockDeviceMappings::default();
        mappings.current.insert(first.clone(), "/dev/loop9".into());
        mappings.current.insert(second.clone(), "/dev/loop4".into());
        let mut subsystem = subsystem(
            "custom-name",
            vec![
                namespace(1, "/dev/loop0", Some(first)),
                namespace(2, "/dev/loop1", Some(second)),
            ],
        );

        let (changed, safe) = remap_subsystem(&mut subsystem, &mappings);

        assert!(changed);
        assert!(safe);
        assert_eq!(subsystem.namespaces[0].device_path, "/dev/loop9");
        assert_eq!(subsystem.namespaces[1].device_path, "/dev/loop4");
    }

    #[test]
    fn migrates_legacy_namespace_from_exact_preexisting_attachment() {
        let identity = volume("pool-a", 10);
        let mut mappings = BlockDeviceMappings::default();
        mappings
            .current
            .insert(identity.clone(), "/dev/loop7".into());
        mappings
            .preexisting
            .insert("/dev/loop7".into(), identity.clone());
        let mut subsystem = subsystem("custom-name", vec![namespace(1, "/dev/loop7", None)]);
        subsystem.allowed_hosts = vec!["nqn.test:host".into()];
        subsystem.allow_any_host = false;

        let (changed, safe) = remap_subsystem(&mut subsystem, &mappings);

        assert!(changed);
        assert!(safe);
        assert_eq!(subsystem.namespaces[0].backing_volume, Some(identity));
        assert!(!subsystem.namespaces[0].backing_volume_unresolved);
        assert_eq!(subsystem.allowed_hosts, ["nqn.test:host"]);
        assert!(!subsystem.allow_any_host);
    }

    #[test]
    fn freshly_allocated_loop_path_is_not_legacy_namespace_evidence() {
        let identity = volume("pool-a", 10);
        let mut mappings = BlockDeviceMappings::default();
        mappings.current.insert(identity, "/dev/loop0".into());
        let mut subsystem = subsystem("volume", vec![namespace(1, "/dev/loop0", None)]);

        let (_, safe) = remap_subsystem(&mut subsystem, &mappings);

        assert!(!safe);
        assert!(subsystem.namespaces[0].backing_volume.is_none());
        assert!(subsystem.namespaces[0].backing_volume_unresolved);
    }

    #[test]
    fn repair_changes_only_selected_unresolved_namespace() {
        let identity = volume("pool-a", 10);
        let preserved = volume("pool-b", 20);
        let mut unresolved = namespace(1, "/dev/loop0", None);
        unresolved.backing_volume_unresolved = true;
        let mut subsystem = subsystem(
            "repair",
            vec![
                unresolved,
                namespace(2, "/dev/loop4", Some(preserved.clone())),
            ],
        );
        subsystem.allowed_hosts = vec!["nqn.test:host".into()];

        repair_subsystem_namespace(&mut subsystem, 1, "/dev/loop7".into(), identity.clone())
            .unwrap();

        assert_eq!(subsystem.namespaces[0].device_path, "/dev/loop7");
        assert_eq!(subsystem.namespaces[0].backing_volume, Some(identity));
        assert!(!subsystem.namespaces[0].backing_volume_unresolved);
        assert_eq!(subsystem.namespaces[1].backing_volume, Some(preserved));
        assert_eq!(subsystem.allowed_hosts, ["nqn.test:host"]);
    }

    #[test]
    fn repair_rejects_resolved_or_raw_namespaces() {
        let identity = volume("pool-a", 10);
        let mut resolved = subsystem(
            "resolved",
            vec![namespace(1, "/dev/loop0", Some(identity.clone()))],
        );
        assert!(
            repair_subsystem_namespace(&mut resolved, 1, "/dev/loop1".into(), identity.clone())
                .is_err()
        );

        let mut raw = subsystem("raw", vec![namespace(1, "/dev/sda", None)]);
        assert!(repair_subsystem_namespace(&mut raw, 1, "/dev/loop1".into(), identity).is_err());
    }

    #[test]
    fn ambiguous_legacy_multi_namespace_state_fails_closed() {
        let identity = volume("pool-a", 10);
        let mut mappings = BlockDeviceMappings::default();
        mappings
            .current
            .insert(identity.clone(), "/dev/loop7".into());
        let mut subsystem = subsystem(
            "volume",
            vec![
                namespace(1, "/dev/loop0", None),
                namespace(2, "/dev/loop1", None),
            ],
        );

        let (_, safe) = remap_subsystem(&mut subsystem, &mappings);

        assert!(!safe);
        assert!(
            subsystem
                .namespaces
                .iter()
                .all(|namespace| namespace.backing_volume_unresolved)
        );
    }

    #[test]
    fn duplicate_names_do_not_enable_legacy_fallback() {
        let first = volume("pool-a", 10);
        let second = volume("pool-b", 10);
        let mut mappings = BlockDeviceMappings::default();
        mappings.current.insert(first.clone(), "/dev/loop7".into());
        mappings.current.insert(second.clone(), "/dev/loop8".into());
        let mut subsystem = subsystem("volume", vec![namespace(1, "/dev/loop0", None)]);

        let (_, safe) = remap_subsystem(&mut subsystem, &mappings);

        assert!(!safe);
        assert!(subsystem.namespaces[0].backing_volume_unresolved);
    }

    #[test]
    fn custom_subsystem_name_never_migrates_legacy_namespace() {
        let identity = volume("pool-a", 10);
        let mut mappings = BlockDeviceMappings::default();
        mappings
            .current
            .insert(identity.clone(), "/dev/loop7".into());
        let mut subsystem = subsystem("volume", vec![namespace(1, "/dev/loop0", None)]);

        let (_, safe) = remap_subsystem(&mut subsystem, &mappings);

        assert!(!safe);
        assert!(subsystem.namespaces[0].backing_volume.is_none());
        assert!(subsystem.namespaces[0].backing_volume_unresolved);
        assert_eq!(subsystem.namespaces[0].device_path, "/dev/loop0");
    }

    #[test]
    fn raw_block_device_is_not_remapped() {
        let mut subsystem = subsystem("raw", vec![namespace(1, "/dev/sda", None)]);

        let (changed, safe) = remap_subsystem(&mut subsystem, &BlockDeviceMappings::default());

        assert!(!changed);
        assert!(safe);
        assert_eq!(subsystem.namespaces[0].device_path, "/dev/sda");
    }

    #[test]
    fn namespace_loads_legacy_state_without_identity() {
        let namespace: Namespace =
            serde_json::from_str(r#"{"nsid":1,"device_path":"/dev/loop0","enabled":true}"#)
                .unwrap();
        assert!(namespace.backing_volume.is_none());
        assert!(!namespace.backing_volume_unresolved);
    }

    #[tokio::test]
    async fn failed_namespace_rewrite_leaves_existing_namespace_disabled() {
        let dir = std::env::temp_dir().join(format!("nasty-nvmeof-test-{}", Uuid::new_v4()));
        let ns_path = dir.join("namespace");
        tokio::fs::create_dir_all(&ns_path).await.unwrap();
        tokio::fs::write(ns_path.join("enable"), "1").await.unwrap();
        tokio::fs::write(ns_path.join("device_path"), "/dev/old")
            .await
            .unwrap();
        tokio::fs::write(ns_path.join("buffered_io"), "0")
            .await
            .unwrap();
        let namespace = namespace(1, "/path/that/does/not/exist", None);

        let result = restore_namespace(ns_path.to_str().unwrap(), &namespace).await;

        assert!(result.is_err());
        assert_eq!(
            tokio::fs::read_to_string(ns_path.join("enable"))
                .await
                .unwrap(),
            "0"
        );
        assert_eq!(
            tokio::fs::read_to_string(ns_path.join("device_path"))
                .await
                .unwrap(),
            "/dev/old"
        );
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn namespace_rewrite_enables_only_after_new_path_is_written() {
        let dir = std::env::temp_dir().join(format!("nasty-nvmeof-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&dir).await.unwrap();
        let backing = dir.join("backing");
        tokio::fs::write(&backing, []).await.unwrap();
        let ns_path = dir.join("namespace");
        tokio::fs::create_dir(&ns_path).await.unwrap();
        for attribute in ["enable", "device_path", "buffered_io"] {
            tokio::fs::write(ns_path.join(attribute), "old")
                .await
                .unwrap();
        }
        let namespace = namespace(1, backing.to_str().unwrap(), None);

        restore_namespace(ns_path.to_str().unwrap(), &namespace)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(ns_path.join("device_path"))
                .await
                .unwrap(),
            backing.to_str().unwrap()
        );
        assert_eq!(
            tokio::fs::read_to_string(ns_path.join("enable"))
                .await
                .unwrap(),
            "1"
        );
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_symlink_accepts_relative_link_to_absolute_target() {
        let dir = std::env::temp_dir().join(format!("nasty-nvmeof-test-{}", Uuid::new_v4()));
        let target = dir.join("subsystems/test");
        let link = dir.join("ports/1/subsystems/test");
        tokio::fs::create_dir_all(&target).await.unwrap();
        tokio::fs::create_dir_all(link.parent().unwrap())
            .await
            .unwrap();
        let relative = Path::new("../../../subsystems/test");
        tokio::fs::symlink(relative, &link).await.unwrap();

        configfs_ensure_symlink(target.to_str().unwrap(), link.to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(tokio::fs::read_link(&link).await.unwrap(), relative);
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_symlink_rejects_relative_link_to_different_target() {
        let dir = std::env::temp_dir().join(format!("nasty-nvmeof-test-{}", Uuid::new_v4()));
        let target = dir.join("subsystems/expected");
        let wrong = dir.join("subsystems/wrong");
        let link = dir.join("ports/1/subsystems/expected");
        tokio::fs::create_dir_all(&target).await.unwrap();
        tokio::fs::create_dir_all(&wrong).await.unwrap();
        tokio::fs::create_dir_all(link.parent().unwrap())
            .await
            .unwrap();
        let relative = Path::new("../../../subsystems/wrong");
        tokio::fs::symlink(relative, &link).await.unwrap();

        let result =
            configfs_ensure_symlink(target.to_str().unwrap(), link.to_str().unwrap()).await;

        assert!(matches!(result, Err(NvmeofError::ConfigFs(_))));
        assert_eq!(tokio::fs::read_link(&link).await.unwrap(), relative);
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    // ── port-address readiness (reboot restore race, #625) ────────

    #[test]
    fn is_wildcard_addr_matches_bind_any() {
        assert!(is_wildcard_addr("0.0.0.0"));
        assert!(is_wildcard_addr("::"));
        assert!(!is_wildcard_addr("192.168.255.248"));
        assert!(!is_wildcard_addr("fd00::1"));
    }

    #[test]
    fn addr_is_local_true_for_loopback_and_wildcards() {
        // Loopback is always assigned; wildcards always "ready".
        assert!(addr_is_local("127.0.0.1"));
        assert!(addr_is_local("::1"));
        assert!(addr_is_local("0.0.0.0"));
        assert!(addr_is_local("::"));
        // Unparseable → don't block on it.
        assert!(addr_is_local("not-an-ip"));
    }

    #[test]
    fn addr_is_local_false_for_unassigned_address() {
        // TEST-NET-1 (RFC 5737) — guaranteed not assigned to this host.
        assert!(!addr_is_local("192.0.2.1"));
    }

    #[test]
    fn validate_subsystem_name_accepts_typical_names() {
        assert!(validate_subsystem_name("faststore").is_ok());
        assert!(validate_subsystem_name("DB-01").is_ok());
        assert!(validate_subsystem_name("vmware.cluster.prod").is_ok());
        assert!(validate_subsystem_name("ns_2024").is_ok());
        assert!(validate_subsystem_name("host:purpose").is_ok());
    }

    #[test]
    fn validate_subsystem_name_rejects_configfs_escape() {
        assert!(validate_subsystem_name("../escape").is_err());
        assert!(validate_subsystem_name("with/slash").is_err());
        assert!(validate_subsystem_name("with\\backslash").is_err());
    }

    #[test]
    fn validate_subsystem_name_rejects_control_and_whitespace() {
        assert!(validate_subsystem_name("with newline\n").is_err());
        assert!(validate_subsystem_name("with space").is_err());
        assert!(validate_subsystem_name("with\x00null").is_err());
    }

    #[test]
    fn validate_subsystem_name_rejects_empty_and_oversize() {
        assert!(validate_subsystem_name("").is_err());
        assert!(validate_subsystem_name(&"x".repeat(201)).is_err());
        assert!(validate_subsystem_name(&"x".repeat(200)).is_ok());
    }

    #[test]
    fn validate_host_nqn_accepts_standard_initiator_forms() {
        // The two common forms an initiator's host NQN takes in the wild.
        assert!(
            validate_host_nqn(
                "nqn.2014-08.org.nvmexpress:uuid:8e69b1d0-1234-5678-9abc-def012345678"
            )
            .is_ok()
        );
        assert!(validate_host_nqn("nqn.2024-01.com.example:client01").is_ok());
    }

    #[test]
    fn validate_host_nqn_requires_nqn_prefix() {
        // Per NVMe base spec §7.9, every NQN starts with "nqn.YYYY-MM"
        // — anything else is either an initiator typo or a hostname
        // being passed where an NQN should go.
        assert!(validate_host_nqn("example.com:client").is_err());
        assert!(validate_host_nqn("client01").is_err());
    }

    #[test]
    fn validate_host_nqn_rejects_path_escape() {
        // Slash would escape the configfs hosts directory.
        assert!(validate_host_nqn("nqn.../etc/passwd").is_err());
        assert!(validate_host_nqn("nqn.2024-01.com.example:a\\b").is_err());
    }

    #[test]
    fn validate_host_nqn_rejects_whitespace_and_control() {
        // Whitespace would break the configfs symlink path; control
        // characters would smuggle into state files.
        assert!(validate_host_nqn("nqn.2024-01.example com").is_err());
        assert!(validate_host_nqn("nqn.2024-01.example\nnewline").is_err());
        assert!(validate_host_nqn("nqn.2024-01.example\x00null").is_err());
    }

    #[test]
    fn validate_host_nqn_rejects_oversize() {
        // 224 chars exceeds the NVMe spec's 223-byte ceiling.
        let oversize = format!("nqn.2024-01.{}", "x".repeat(212));
        assert!(validate_host_nqn(&oversize).is_err());
        let at_limit = format!("nqn.2024-01.{}", "x".repeat(211));
        assert_eq!(at_limit.len(), 223);
        assert!(validate_host_nqn(&at_limit).is_ok());
    }
}
