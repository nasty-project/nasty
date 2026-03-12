use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

const STATE_PATH: &str = "/var/lib/nasty/nvmeof-targets.json";
const STATE_DIR: &str = "/var/lib/nasty";
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

// ── State ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NvmeofState {
    subsystems: Vec<NvmeofSubsystem>,
    next_port_id: u16,
}

// ── Service ─────────────────────────────────────────────────────

pub struct NvmeofService;

impl NvmeofService {
    pub fn new() -> Self {
        Self
    }

    pub async fn list(&self) -> Result<Vec<NvmeofSubsystem>, NvmeofError> {
        Ok(load_state().await.subsystems)
    }

    pub async fn get(&self, id: &str) -> Result<NvmeofSubsystem, NvmeofError> {
        load_state()
            .await
            .subsystems
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| NvmeofError::NotFound(id.to_string()))
    }

    pub async fn create(
        &self,
        req: CreateSubsystemRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut state = load_state().await;
        let nqn = format!("{DEFAULT_NQN_PREFIX}:{}", req.name);

        if state.subsystems.iter().any(|s| s.nqn == nqn) {
            return Err(NvmeofError::AlreadyExists(nqn));
        }

        let allow_any = req.allow_any_host.unwrap_or(true);

        // Create subsystem in configfs
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

        state.subsystems.push(subsystem.clone());
        save_state(&state).await?;

        info!("Created NVMe-oF subsystem {nqn}");
        Ok(subsystem)
    }

    /// Create a complete NVMe-oF share in one step: subsystem + namespace + port
    pub async fn create_quick(&self, req: QuickCreateRequest) -> Result<NvmeofSubsystem, NvmeofError> {
        // Create subsystem
        let subsys = self.create(CreateSubsystemRequest {
            name: req.name,
            allow_any_host: Some(true),
        }).await?;

        // Add namespace with the block device
        let subsys = self.add_namespace(AddNamespaceRequest {
            subsystem_id: subsys.id.clone(),
            device_path: req.device_path,
        }).await?;

        // Add a TCP port
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
        let mut state = load_state().await;

        let idx = state
            .subsystems
            .iter()
            .position(|s| s.id == req.id)
            .ok_or_else(|| NvmeofError::NotFound(req.id.clone()))?;

        let subsys = &state.subsystems[idx];

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

        state.subsystems.remove(idx);
        save_state(&state).await?;

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

        let mut state = load_state().await;
        let subsys = state
            .subsystems
            .iter_mut()
            .find(|s| s.id == req.subsystem_id)
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

        let result = subsys.clone();
        save_state(&state).await?;

        info!("Added namespace {nsid} to subsystem '{}'", result.nqn);
        Ok(result)
    }

    pub async fn remove_namespace(
        &self,
        req: RemoveNamespaceRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut state = load_state().await;
        let subsys = state
            .subsystems
            .iter_mut()
            .find(|s| s.id == req.subsystem_id)
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
        let result = subsys.clone();
        save_state(&state).await?;

        info!("Removed namespace {} from subsystem '{}'", req.nsid, result.nqn);
        Ok(result)
    }

    pub async fn add_port(
        &self,
        req: AddPortRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut state = load_state().await;
        let subsys = state
            .subsystems
            .iter_mut()
            .find(|s| s.id == req.subsystem_id)
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let transport = req.transport.unwrap_or_else(|| "tcp".to_string());
        let addr = req.addr.unwrap_or_else(|| "0.0.0.0".to_string());
        let svc_id = req.service_id.unwrap_or(4420);
        let addr_family = req.addr_family.unwrap_or_else(|| "ipv4".to_string());

        let port_id = state.next_port_id;
        state.next_port_id += 1;

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
        let result = subsys.clone();
        save_state(&state).await?;

        info!("Added port {port_id} to subsystem '{}'", result.nqn);
        Ok(result)
    }

    pub async fn remove_port(
        &self,
        req: RemovePortRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut state = load_state().await;
        let subsys = state
            .subsystems
            .iter_mut()
            .find(|s| s.id == req.subsystem_id)
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
        let result = subsys.clone();
        save_state(&state).await?;

        info!("Removed port {} from subsystem '{}'", req.port_id, result.nqn);
        Ok(result)
    }

    pub async fn add_host(
        &self,
        req: AddHostRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut state = load_state().await;
        let subsys = state
            .subsystems
            .iter_mut()
            .find(|s| s.id == req.subsystem_id)
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

        let result = subsys.clone();
        save_state(&state).await?;

        info!("Added allowed host to subsystem '{}'", result.nqn);
        Ok(result)
    }

    pub async fn remove_host(
        &self,
        req: RemoveHostRequest,
    ) -> Result<NvmeofSubsystem, NvmeofError> {
        let mut state = load_state().await;
        let subsys = state
            .subsystems
            .iter_mut()
            .find(|s| s.id == req.subsystem_id)
            .ok_or_else(|| NvmeofError::NotFound(req.subsystem_id.clone()))?;

        let link_path = format!(
            "{NVMET_BASE}/subsystems/{}/allowed_hosts/{}",
            subsys.nqn, req.host_nqn
        );
        configfs_unlink(&link_path).await?;

        subsys.allowed_hosts.retain(|h| h != &req.host_nqn);
        let result = subsys.clone();
        save_state(&state).await?;

        info!("Removed allowed host from subsystem '{}'", result.nqn);
        Ok(result)
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

// ── State persistence ───────────────────────────────────────────

async fn load_state() -> NvmeofState {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => NvmeofState::default(),
    }
}

async fn save_state(state: &NvmeofState) -> Result<(), NvmeofError> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(state).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}
