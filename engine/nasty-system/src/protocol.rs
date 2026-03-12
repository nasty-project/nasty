//! Dynamic protocol management: enable/disable NFS, SMB, iSCSI, NVMe-oF at runtime.
//!
//! Persists state to `/var/lib/nasty/protocols.json` so boot-time services
//! know which protocols to start.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/protocols.json";
const SMB_NASTY_CONF: &str = "/etc/samba/smb.nasty.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Nfs,
    Smb,
    Iscsi,
    Nvmeof,
}

impl Protocol {
    pub const ALL: &[Protocol] = &[
        Protocol::Nfs,
        Protocol::Smb,
        Protocol::Iscsi,
        Protocol::Nvmeof,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::Nfs => "nfs",
            Protocol::Smb => "smb",
            Protocol::Iscsi => "iscsi",
            Protocol::Nvmeof => "nvmeof",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Protocol::Nfs => "NFS",
            Protocol::Smb => "SMB",
            Protocol::Iscsi => "iSCSI",
            Protocol::Nvmeof => "NVMe-oF",
        }
    }

    /// systemd service(s) to start/stop for this protocol
    fn services(&self) -> &[&str] {
        match self {
            Protocol::Nfs => &["nfs-server.service"],
            Protocol::Smb => &["smb.service", "nmb.service"],
            Protocol::Iscsi => &[], // LIO has no daemon; managed via targetcli
            Protocol::Nvmeof => &[], // configfs-based, no daemon
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "nfs" => Some(Protocol::Nfs),
            "smb" => Some(Protocol::Smb),
            "iscsi" => Some(Protocol::Iscsi),
            "nvmeof" => Some(Protocol::Nvmeof),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolStatus {
    pub name: String,
    pub display_name: String,
    pub enabled: bool,
    pub running: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProtocolState {
    nfs: bool,
    smb: bool,
    iscsi: bool,
    nvmeof: bool,
}

impl ProtocolState {
    fn get(&self, proto: Protocol) -> bool {
        match proto {
            Protocol::Nfs => self.nfs,
            Protocol::Smb => self.smb,
            Protocol::Iscsi => self.iscsi,
            Protocol::Nvmeof => self.nvmeof,
        }
    }

    fn set(&mut self, proto: Protocol, enabled: bool) {
        match proto {
            Protocol::Nfs => self.nfs = enabled,
            Protocol::Smb => self.smb = enabled,
            Protocol::Iscsi => self.iscsi = enabled,
            Protocol::Nvmeof => self.nvmeof = enabled,
        }
    }
}

pub struct ProtocolService;

impl ProtocolService {
    pub fn new() -> Self {
        Self
    }

    /// Restore enabled protocol services on startup.
    /// Starts daemons and loads kernel modules for protocols the user enabled.
    pub async fn restore(&self) {
        let state = load_state().await;

        for &proto in Protocol::ALL {
            if !state.get(proto) {
                continue;
            }

            info!("Restoring protocol: {}", proto.display_name());

            prepare_protocol(proto).await;

            // Start associated services
            for svc in proto.services() {
                if let Err(e) = systemctl("start", svc).await {
                    warn!("Failed to start {svc}: {e}");
                }
            }

            // Load kernel modules for iSCSI/NVMe-oF
            if proto == Protocol::Iscsi {
                let _ = modprobe("target_core_mod").await;
                let _ = modprobe("iscsi_target_mod").await;
            }
            if proto == Protocol::Nvmeof {
                let _ = modprobe("nvmet").await;
                let _ = modprobe("nvmet-tcp").await;
            }
        }
    }

    /// List all protocols with their enabled/running status
    pub async fn list(&self) -> Vec<ProtocolStatus> {
        let state = load_state().await;
        let mut result = Vec::new();

        for &proto in Protocol::ALL {
            let running = is_protocol_running(proto).await;
            result.push(ProtocolStatus {
                name: proto.name().to_string(),
                display_name: proto.display_name().to_string(),
                enabled: state.get(proto),
                running,
            });
        }

        result
    }

    /// Enable a protocol: start its services and persist state
    pub async fn enable(&self, name: &str) -> Result<ProtocolStatus, String> {
        let proto = Protocol::from_name(name)
            .ok_or_else(|| format!("unknown protocol: {name}"))?;

        let mut state = load_state().await;
        state.set(proto, true);
        save_state(&state).await?;

        prepare_protocol(proto).await;

        // Start associated services
        for svc in proto.services() {
            info!("Starting service {svc} for protocol {}", proto.display_name());
            if let Err(e) = systemctl("start", svc).await {
                warn!("Failed to start {svc}: {e}");
            }
        }

        // For iSCSI: load kernel modules
        if proto == Protocol::Iscsi {
            let _ = modprobe("target_core_mod").await;
            let _ = modprobe("iscsi_target_mod").await;
        }

        // For NVMe-oF: load kernel modules
        if proto == Protocol::Nvmeof {
            let _ = modprobe("nvmet").await;
            let _ = modprobe("nvmet-tcp").await;
        }

        let running = is_protocol_running(proto).await;
        Ok(ProtocolStatus {
            name: proto.name().to_string(),
            display_name: proto.display_name().to_string(),
            enabled: true,
            running,
        })
    }

    /// Disable a protocol: stop its services and persist state
    pub async fn disable(&self, name: &str) -> Result<ProtocolStatus, String> {
        let proto = Protocol::from_name(name)
            .ok_or_else(|| format!("unknown protocol: {name}"))?;

        let mut state = load_state().await;
        state.set(proto, false);
        save_state(&state).await?;

        // Stop associated services
        for svc in proto.services() {
            info!("Stopping service {svc} for protocol {}", proto.display_name());
            let _ = systemctl("stop", svc).await;
        }

        let running = is_protocol_running(proto).await;
        Ok(ProtocolStatus {
            name: proto.name().to_string(),
            display_name: proto.display_name().to_string(),
            enabled: false,
            running,
        })
    }
}

/// Check if a protocol is currently running
async fn is_protocol_running(proto: Protocol) -> bool {
    match proto {
        Protocol::Nfs => systemctl_is_active("nfs-server.service").await,
        Protocol::Smb => systemctl_is_active("smb.service").await,
        Protocol::Iscsi => {
            // iSCSI is "running" if the kernel modules are loaded
            std::path::Path::new("/sys/kernel/config/target/iscsi").exists()
        }
        Protocol::Nvmeof => {
            // NVMe-oF is "running" if nvmet configfs is available
            std::path::Path::new("/sys/kernel/config/nvmet").exists()
        }
    }
}

/// Ensure prerequisites exist before starting a protocol's services.
async fn prepare_protocol(proto: Protocol) {
    if proto == Protocol::Smb {
        // Samba config includes smb.nasty.conf — must exist or smbd fails to start
        if !std::path::Path::new(SMB_NASTY_CONF).exists() {
            let header = "# Managed by NASty — do not edit manually\n";
            if let Err(e) = tokio::fs::write(SMB_NASTY_CONF, header).await {
                warn!("Failed to create {SMB_NASTY_CONF}: {e}");
            }
        }
    }
}

async fn systemctl(action: &str, service: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("systemctl")
        .args([action, service])
        .output()
        .await
        .map_err(|e| format!("failed to run systemctl: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("systemctl {action} {service} failed: {stderr}"))
    }
}

async fn systemctl_is_active(service: &str) -> bool {
    tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", service])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn modprobe(module: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("modprobe")
        .arg(module)
        .output()
        .await
        .map_err(|e| format!("modprobe {module} failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!("modprobe {module} failed"))
    }
}

async fn load_state() -> ProtocolState {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => {
            // Default: all protocols disabled on fresh install.
            // User explicitly enables what they need.
            ProtocolState::default()
        }
    }
}

async fn save_state(state: &ProtocolState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize protocol state: {e}"))?;
    tokio::fs::write(STATE_PATH, json)
        .await
        .map_err(|e| format!("failed to write protocol state: {e}"))
}
