use std::path::Path;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

const STATE_DIR: &str = "/var/lib/nasty/shares/nvmeof";
const NVMET_BASE: &str = "/sys/kernel/config/nvmet";
const DEFAULT_NQN_PREFIX: &str = "nqn.2137-04.storage.nasty";

#[derive(Debug, Error)]
pub enum NvmeofError {
    #[error("subsystem not found: {0}")]
    NotFound(String),
    #[error("subsystem already exists: {0}")]
    AlreadyExists(String),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("namespace not found: nsid {0}")]
    NamespaceNotFound(u32),
    #[error("port not found: {0}")]
    PortNotFound(u16),
    #[error("configfs error: {0}")]
    ConfigFs(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveNamespaceRequest {
    /// ID of the subsystem from which to remove the namespace.
    pub subsystem_id: String,
    /// Namespace ID to remove.
    pub nsid: u32,
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

/// Derive next port ID by scanning existing ports in configfs.
async fn next_port_id() -> u16 {
    let ports_dir = format!("{NVMET_BASE}/ports");
    let mut max_id: u16 = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(&ports_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(id) = name.parse::<u16>() {
                    max_id = max_id.max(id);
                }
            }
        }
    }
    max_id + 1
}

// ── Service ─────────────────────────────────────────────────────

pub struct NvmeofService;

impl NvmeofService {
    pub fn new() -> Self {
        Self
    }

    /// Restore NVMe-oF configfs state from persisted JSON files.
    /// Called at startup — configfs is volatile and lost on reboot.
    pub async fn restore(&self) {
        // Only restore if the nvmeof protocol is enabled
        let proto_state: serde_json::Value = tokio::fs::read_to_string("/var/lib/nasty/protocols.json")
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        if proto_state.get("nvmeof").and_then(|v| v.as_bool()) != Some(true) {
            info!("NVMe-oF protocol disabled, skipping restore");
            return;
        }

        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all().await;
        if subsystems.is_empty() {
            info!("No NVMe-oF subsystems to restore");
            return;
        }

        for subsys in &subsystems {
            info!("Restoring NVMe-oF subsystem: {}", subsys.nqn);

            // Create subsystem
            let subsys_path = format!("{NVMET_BASE}/subsystems/{}", subsys.nqn);
            if let Err(e) = configfs_mkdir(&subsys_path).await {
                warn!("Failed to create subsystem {}: {e}", subsys.nqn);
                continue;
            }
            let _ = configfs_write(
                &format!("{subsys_path}/attr_allow_any_host"),
                if subsys.allow_any_host { "1" } else { "0" },
            ).await;

            // Restore namespaces
            for ns in &subsys.namespaces {
                if !Path::new(&ns.device_path).exists() {
                    warn!("  Device {} not found, skipping namespace {}", ns.device_path, ns.nsid);
                    continue;
                }
                let ns_path = format!("{subsys_path}/namespaces/{}", ns.nsid);
                if let Err(e) = configfs_mkdir(&ns_path).await {
                    warn!("  Failed to create namespace {}: {e}", ns.nsid);
                    continue;
                }
                let _ = configfs_write(&format!("{ns_path}/device_path"), &ns.device_path).await;
                let _ = configfs_write(&format!("{ns_path}/buffered_io"), "0").await;
                if ns.enabled {
                    let _ = configfs_write(&format!("{ns_path}/enable"), "1").await;
                }
                info!("  Restored namespace {} -> {}", ns.nsid, ns.device_path);
            }

            // Restore allowed hosts
            for host_nqn in &subsys.allowed_hosts {
                let host_path = format!("{NVMET_BASE}/hosts/{host_nqn}");
                let _ = configfs_mkdir(&host_path).await;
                let link = format!("{subsys_path}/allowed_hosts/{host_nqn}");
                let _ = configfs_symlink(&host_path, &link).await;
            }

            // Restore ports — reuse existing port if one already binds to the same address
            for port in &subsys.ports {
                let svc_id: u16 = port.service_id.parse().unwrap_or(4420);
                let actual_port_id = if let Some(existing) = find_existing_port(
                    &port.transport, &port.addr, svc_id, &port.addr_family,
                ).await {
                    existing
                } else {
                    let port_path = format!("{NVMET_BASE}/ports/{}", port.port_id);
                    if let Err(e) = configfs_mkdir(&port_path).await {
                        warn!("  Failed to create port {}: {e}", port.port_id);
                        continue;
                    }
                    let _ = configfs_write(&format!("{port_path}/addr_trtype"), &port.transport).await;
                    let _ = configfs_write(&format!("{port_path}/addr_traddr"), &port.addr).await;
                    let _ = configfs_write(&format!("{port_path}/addr_trsvcid"), &port.service_id).await;
                    let _ = configfs_write(&format!("{port_path}/addr_adrfam"), &port.addr_family).await;
                    port.port_id
                };

                let port_path = format!("{NVMET_BASE}/ports/{actual_port_id}");
                let link = format!("{port_path}/subsystems/{}", subsys.nqn);
                let _ = configfs_symlink(
                    &format!("{NVMET_BASE}/subsystems/{}", subsys.nqn),
                    &link,
                ).await;
                info!("  Restored port {} ({}:{})", actual_port_id, port.addr, port.service_id);
            }
        }

        info!("NVMe-oF restore complete");
    }

    /// Update persisted device paths after a reboot where loop device numbers changed.
    /// `dev_map` maps subvolume_name → current loop device (e.g. "tank-vol" → "/dev/loop0").
    /// The subvolume name is extracted from the NQN suffix after the last ':'.
    pub async fn remap_device_paths(&self, dev_map: &std::collections::HashMap<String, String>) {
        let mut subsystems: Vec<NvmeofSubsystem> = state_dir().load_all().await;
        for subsys in &mut subsystems {
            let name = subsys.nqn.rsplit(':').next().unwrap_or("").to_string();
            let Some(new_dev) = dev_map.get(&name) else { continue };
            let mut changed = false;
            for ns in &mut subsys.namespaces {
                if &ns.device_path != new_dev {
                    info!(
                        "Remapping NVMe-oF '{}' ns{} {} → {}",
                        subsys.nqn, ns.nsid, ns.device_path, new_dev
                    );
                    ns.device_path = new_dev.clone();
                    changed = true;
                }
            }
            if changed {
                let _ = state_dir().save(&subsys.id, subsys).await;
            }
        }
    }

    pub async fn list(&self) -> Result<Vec<NvmeofSubsystem>, NvmeofError> {

        Ok(state_dir().load_all().await)
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

        let subsystems: Vec<NvmeofSubsystem> = state_dir().load_all().await;
        let nqn = format!("{DEFAULT_NQN_PREFIX}:{}", req.name);

        if let Some(existing) = subsystems.into_iter().find(|s| s.nqn == nqn) {
            info!("NVMe-oF subsystem {nqn} already exists, returning existing (idempotent)");
            return Ok(existing);
        }

        let allow_any = req.allow_any_host.unwrap_or(true);

        let subsys_path = format!("{NVMET_BASE}/subsystems/{nqn}");
        configfs_mkdir(&subsys_path).await?;
        configfs_write(&format!("{subsys_path}/attr_allow_any_host"), if allow_any { "1" } else { "0" }).await?;

        let subsystem = NvmeofSubsystem {
            id: Uuid::new_v4().to_string(),
            nqn: nqn.clone(),
            namespaces: vec![],
            ports: vec![],
            allowed_hosts: vec![],
            allow_any_host: allow_any,
            enabled: true,
        };

        state_dir().save(&subsystem.id, &subsystem).await
            .map_err(NvmeofError::Io)?;

        // Optional: add namespace if device_path was provided
        let mut subsystem = subsystem;
        if let Some(device_path) = req.device_path {
            if subsystem.namespaces.is_empty() {
                subsystem = self.add_namespace(AddNamespaceRequest {
                    subsystem_id: subsystem.id.clone(),
                    device_path,
                }).await?;
            } else {
                info!("Subsystem {} already has {} namespace(s), skipping", subsystem.nqn, subsystem.namespaces.len());
            }

            // Add a port when a namespace was created
            if subsystem.ports.is_empty() {
                subsystem = self.add_port(AddPortRequest {
                    subsystem_id: subsystem.id.clone(),
                    transport: Some("tcp".to_string()),
                    addr: Some(req.addr.unwrap_or_else(|| "0.0.0.0".to_string())),
                    service_id: Some(req.port.unwrap_or(4420)),
                    addr_family: Some("ipv4".to_string()),
                }).await?;
            } else {
                info!("Subsystem {} already has {} port(s), skipping", subsystem.nqn, subsystem.ports.len());
            }
        }

        // Optional: add allowed hosts if provided
        if let Some(hosts) = req.allowed_hosts {
            for host_nqn in hosts {
                subsystem = self.add_host(AddHostRequest {
                    subsystem_id: subsystem.id.clone(),
                    host_nqn,
                }).await?;
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

        state_dir().remove(&req.id).await
            .map_err(NvmeofError::Io)?;

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
        });

        state_dir().save(&subsys.id, &subsys).await
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

        state_dir().save(&subsys.id, &subsys).await
            .map_err(NvmeofError::Io)?;

        info!("Removed namespace {} from subsystem '{}'", req.nsid, subsys.nqn);
        Ok(subsys)
    }

    pub async fn add_port(
        &self,
        req: AddPortRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {

        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let transport = req.transport.unwrap_or_else(|| "tcp".to_string());
        let addr = match req.addr {
            Some(a) if !a.is_empty() && a != "0.0.0.0" => a,
            _ => {
                // NVMe-oF configfs requires a real IP — 0.0.0.0 causes EINVAL
                // when linking subsystems to ports. Detect the primary IP.
                detect_primary_ip().await.unwrap_or_else(|| "0.0.0.0".to_string())
            }
        };
        let svc_id = req.service_id.unwrap_or(4420);
        let addr_family = req.addr_family.unwrap_or_else(|| "ipv4".to_string());

        // Check if an existing port already binds to the same address —
        // multiple subsystems can share a single NVMe-oF port.
        let port_id = if let Some(existing) = find_existing_port(&transport, &addr, svc_id, &addr_family).await {
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

        state_dir().save(&subsys.id, &subsys).await
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

        state_dir().save(&subsys.id, &subsys).await
            .map_err(NvmeofError::Io)?;

        info!("Removed port {} from subsystem '{}'", req.port_id, subsys.nqn);
        Ok(subsys)
    }

    pub async fn add_host(
        &self,
        req: AddHostRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {

        let mut subsys: NvmeofSubsystem = state_dir()
            .load(&req.subsystem_id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        // Create host entry if it doesn't exist
        let host_path = format!("{NVMET_BASE}/hosts/{}", req.host_nqn);
        if !Path::new(&host_path).exists() {
            configfs_mkdir(&host_path).await?;
        }

        // Symlink into subsystem's allowed_hosts
        let link_path = format!(
            "{NVMET_BASE}/subsystems/{}/allowed_hosts/{}",
            subsys.nqn, req.host_nqn
        );
        configfs_symlink(&host_path, &link_path).await?;

        // Disable allow_any_host since we're using explicit ACLs
        let subsys_path = format!("{NVMET_BASE}/subsystems/{}", subsys.nqn);
        configfs_write(&format!("{subsys_path}/attr_allow_any_host"), "0").await?;

        subsys.allow_any_host = false;
        if !subsys.allowed_hosts.contains(&req.host_nqn) {
            subsys.allowed_hosts.push(req.host_nqn);
        }

        state_dir().save(&subsys.id, &subsys).await
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

        state_dir().save(&subsys.id, &subsys).await
            .map_err(NvmeofError::Io)?;

        info!("Removed allowed host from subsystem '{}'", subsys.nqn);
        Ok(subsys)
    }
}

// ── port reuse ─────────────────────────────────────────────────

/// Scan all existing configfs ports for one matching the given transport/addr/service_id.
/// Returns the port_id if found, so multiple subsystems can share a single listener.
async fn find_existing_port(transport: &str, addr: &str, svc_id: u16, addr_family: &str) -> Option<u16> {
    let mut entries = tokio::fs::read_dir(format!("{NVMET_BASE}/ports")).await.ok()?;
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

