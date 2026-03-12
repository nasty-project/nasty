use std::path::Path;

use nasty_common::{HasId, StateDir};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

const STATE_DIR: &str = "/var/lib/nasty/shares/nvmeof";
const PORT_COUNTER_PATH: &str = "/var/lib/nasty/shares/nvmeof/.next_port_id";
const NVMET_BASE: &str = "/sys/kernel/config/nvmet";
const DEFAULT_NQN_PREFIX: &str = "nqn.2024-01.com.nasty";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeofSubsystem {
    pub id: String,
    pub nqn: String,
    pub namespaces: Vec<Namespace>,
    pub ports: Vec<Port>,
    pub allowed_hosts: Vec<String>,
    pub allow_any_host: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Namespace {
    pub nsid: u32,
    pub device_path: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Port {
    pub port_id: u16,
    pub transport: String,
    pub addr: String,
    pub service_id: String,
    pub addr_family: String,
}

// ── Requests ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSubsystemRequest {
    /// Short name appended to NQN prefix
    pub name: String,
    pub allow_any_host: Option<bool>,
}

/// Simplified request: creates subsystem + namespace + port in one shot
#[derive(Debug, Deserialize)]
pub struct QuickCreateRequest {
    /// Short name for the NQN
    pub name: String,
    /// Block device path (e.g. /dev/loop0)
    pub device_path: String,
    /// Listen address (default 0.0.0.0)
    pub addr: Option<String>,
    /// Port number (default 4420)
    pub port: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteSubsystemRequest {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct AddNamespaceRequest {
    pub subsystem_id: String,
    /// Block device path (e.g. /dev/sdc)
    pub device_path: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoveNamespaceRequest {
    pub subsystem_id: String,
    pub nsid: u32,
}

#[derive(Debug, Deserialize)]
pub struct AddPortRequest {
    pub subsystem_id: String,
    /// "tcp" or "rdma"
    pub transport: Option<String>,
    pub addr: Option<String>,
    /// Port number (default 4420)
    pub service_id: Option<u16>,
    pub addr_family: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemovePortRequest {
    pub subsystem_id: String,
    pub port_id: u16,
}

#[derive(Debug, Deserialize)]
pub struct AddHostRequest {
    pub subsystem_id: String,
    pub host_nqn: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoveHostRequest {
    pub subsystem_id: String,
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

/// Atomic port ID counter (separate from per-subsystem state)
async fn next_port_id() -> u16 {
    let current = tokio::fs::read_to_string(PORT_COUNTER_PATH)
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(0);
    let next = current + 1;
    let _ = tokio::fs::write(PORT_COUNTER_PATH, next.to_string()).await;
    current
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

            // Restore ports
            for port in &subsys.ports {
                let port_path = format!("{NVMET_BASE}/ports/{}", port.port_id);
                if let Err(e) = configfs_mkdir(&port_path).await {
                    warn!("  Failed to create port {}: {e}", port.port_id);
                    continue;
                }
                let _ = configfs_write(&format!("{port_path}/addr_trtype"), &port.transport).await;
                let _ = configfs_write(&format!("{port_path}/addr_traddr"), &port.addr).await;
                let _ = configfs_write(&format!("{port_path}/addr_trsvcid"), &port.service_id).await;
                let _ = configfs_write(&format!("{port_path}/addr_adrfam"), &port.addr_family).await;

                let link = format!("{port_path}/subsystems/{}", subsys.nqn);
                let _ = configfs_symlink(
                    &format!("{NVMET_BASE}/subsystems/{}", subsys.nqn),
                    &link,
                ).await;
                info!("  Restored port {} ({}:{})", port.port_id, port.addr, port.service_id);
            }
        }

        info!("NVMe-oF restore complete");
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

        if subsystems.iter().any(|s| s.nqn == nqn) {
            return Err(NvmeofError::AlreadyExists(nqn));
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

        info!("Created NVMe-oF subsystem {nqn}");
        Ok(subsystem)
    }

    /// Create a complete NVMe-oF share in one step: subsystem + namespace + port
    pub async fn create_quick(&self, req: QuickCreateRequest) -> Result<NvmeofSubsystem, NvmeofError> {
        let subsys = self.create(CreateSubsystemRequest {
            name: req.name,
            allow_any_host: Some(true),
        }).await?;

        let subsys = self.add_namespace(AddNamespaceRequest {
            subsystem_id: subsys.id.clone(),
            device_path: req.device_path,
        }).await?;

        let subsys = self.add_port(AddPortRequest {
            subsystem_id: subsys.id.clone(),
            transport: Some("tcp".to_string()),
            addr: Some(req.addr.unwrap_or_else(|| "0.0.0.0".to_string())),
            service_id: Some(req.port.unwrap_or(4420)),
            addr_family: Some("ipv4".to_string()),
        }).await?;

        Ok(subsys)
    }

    pub async fn delete(&self, req: DeleteSubsystemRequest) -> Result<(), NvmeofError> {

        let subsys: NvmeofSubsystem = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| NvmeofError::NotFound(req.id.clone()))?;

        // Unlink from ports first
        for port in &subsys.ports {
            let link = format!(
                "{NVMET_BASE}/ports/{}/subsystems/{}",
                port.port_id, subsys.nqn
            );
            let _ = configfs_unlink(&link).await;
        }

        // Remove port directories if they were created solely for this subsystem
        for port in &subsys.ports {
            let port_dir = format!("{NVMET_BASE}/ports/{}", port.port_id);
            // Only remove if the subsystems/ dir is empty
            let subsys_dir = format!("{port_dir}/subsystems");
            if dir_is_empty(&subsys_dir).await {
                let _ = configfs_rmdir(&port_dir).await;
            }
        }

        // Disable and remove namespaces
        for ns in &subsys.namespaces {
            let ns_path = format!(
                "{NVMET_BASE}/subsystems/{}/namespaces/{}",
                subsys.nqn, ns.nsid
            );
            let _ = configfs_write(&format!("{ns_path}/enable"), "0").await;
            let _ = configfs_rmdir(&ns_path).await;
        }

        // Remove allowed hosts
        for host_nqn in &subsys.allowed_hosts {
            let link = format!(
                "{NVMET_BASE}/subsystems/{}/allowed_hosts/{host_nqn}",
                subsys.nqn
            );
            let _ = configfs_unlink(&link).await;
        }

        // Remove subsystem
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
        let _ = configfs_write(&format!("{ns_path}/enable"), "0").await;
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
        let addr = req.addr.unwrap_or_else(|| "0.0.0.0".to_string());
        let svc_id = req.service_id.unwrap_or(4420);
        let addr_family = req.addr_family.unwrap_or_else(|| "ipv4".to_string());

        let port_id = next_port_id().await;

        // Create port in configfs
        let port_path = format!("{NVMET_BASE}/ports/{port_id}");
        configfs_mkdir(&port_path).await?;
        configfs_write(&format!("{port_path}/addr_trtype"), &transport).await?;
        configfs_write(&format!("{port_path}/addr_traddr"), &addr).await?;
        configfs_write(&format!("{port_path}/addr_trsvcid"), &svc_id.to_string()).await?;
        configfs_write(&format!("{port_path}/addr_adrfam"), &addr_family).await?;

        // Link subsystem to port
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

