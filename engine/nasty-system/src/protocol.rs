//! Dynamic protocol management: enable/disable NFS, SMB, iSCSI, NVMe-oF at runtime.
//!
//! Persists state to `/var/lib/nasty/protocols.json` so boot-time services
//! know which protocols to start.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/protocols.json";
const SMB_NASTY_CONF: &str = "/etc/samba/smb.nasty.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Nfs,
    Smb,
    Iscsi,
    Nvmeof,
    Nut,
    Ssh,
    Avahi,
    Smart,
    RestServer,
}

impl Protocol {
    pub const ALL: &[Protocol] = &[
        Protocol::Nfs,
        Protocol::Smb,
        Protocol::Iscsi,
        Protocol::Nvmeof,
        Protocol::Nut,
        Protocol::Ssh,
        Protocol::Avahi,
        Protocol::Smart,
        Protocol::RestServer,
    ];

    pub fn is_system_service(&self) -> bool {
        matches!(self, Protocol::Nut | Protocol::Ssh | Protocol::Avahi | Protocol::Smart | Protocol::RestServer)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::Nfs => "nfs",
            Protocol::Smb => "smb",
            Protocol::Iscsi => "iscsi",
            Protocol::Nvmeof => "nvmeof",
            Protocol::Nut => "nut",
            Protocol::Ssh => "ssh",
            Protocol::Avahi => "avahi",
            Protocol::Smart => "smart",
            Protocol::RestServer => "rest-server",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Protocol::Nfs => "NFS",
            Protocol::Smb => "SMB",
            Protocol::Iscsi => "iSCSI",
            Protocol::Nvmeof => "NVMe-oF",
            Protocol::Nut => "UPS (NUT)",
            Protocol::Ssh => "SSH",
            Protocol::Avahi => "mDNS (Avahi)",
            Protocol::Smart => "SMART",
            Protocol::RestServer => "Backup Server",
        }
    }

    /// systemd service(s) to start/stop for this protocol
    fn services(&self) -> &[&str] {
        match self {
            Protocol::Nfs => &["nfs-server.service"],
            Protocol::Smb => &["samba-smbd.service", "samba-nmbd.service"],
            Protocol::Iscsi => &["target.service"],
            Protocol::Nvmeof => &[], // configfs-based, no daemon
            Protocol::Nut => &["nut-driver.service", "nut-server.service", "nut-monitor.service"],
            Protocol::Ssh => &["sshd.service"],
            Protocol::Avahi => &["avahi-daemon.service"],
            Protocol::Smart => &["smartd.service"],
            Protocol::RestServer => &["nasty-rest-server.service"],
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "nfs" => Some(Protocol::Nfs),
            "smb" => Some(Protocol::Smb),
            "iscsi" => Some(Protocol::Iscsi),
            "nvmeof" => Some(Protocol::Nvmeof),
            "nut" => Some(Protocol::Nut),
            "ssh" => Some(Protocol::Ssh),
            "avahi" => Some(Protocol::Avahi),
            "smart" => Some(Protocol::Smart),
            "rest-server" => Some(Protocol::RestServer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolStatus {
    /// Machine-readable protocol identifier (e.g. `nfs`, `smb`, `iscsi`).
    pub name: String,
    /// Human-readable display name (e.g. `NFS`, `SMB`, `iSCSI`).
    pub display_name: String,
    /// Whether the protocol is enabled in persistent state.
    pub enabled: bool,
    /// Whether the protocol's systemd service is currently active.
    pub running: bool,
    /// Whether this is a system-level service (SSH, Avahi, SMART) rather than a storage protocol.
    pub system_service: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProtocolState {
    #[serde(default)]
    nfs: bool,
    #[serde(default)]
    smb: bool,
    #[serde(default)]
    iscsi: bool,
    #[serde(default)]
    nvmeof: bool,
    #[serde(default)]
    nut: bool,
    #[serde(default = "default_true")]
    ssh: bool,
    #[serde(default = "default_true")]
    avahi: bool,
    #[serde(default = "default_true")]
    smart: bool,
    #[serde(default)]
    rest_server: bool,
}

fn default_true() -> bool { true }

impl Default for ProtocolState {
    fn default() -> Self {
        Self {
            nfs: false,
            smb: false,
            iscsi: false,
            nvmeof: false,
            nut: false,
            ssh: true,
            avahi: true,
            smart: true,
            rest_server: false,
        }
    }
}

impl ProtocolState {
    fn get(&self, proto: Protocol) -> bool {
        match proto {
            Protocol::Nfs => self.nfs,
            Protocol::Smb => self.smb,
            Protocol::Iscsi => self.iscsi,
            Protocol::Nvmeof => self.nvmeof,
            Protocol::Nut => self.nut,
            Protocol::Ssh => self.ssh,
            Protocol::Avahi => self.avahi,
            Protocol::Smart => self.smart,
            Protocol::RestServer => self.rest_server,
        }
    }

    fn set(&mut self, proto: Protocol, enabled: bool) {
        match proto {
            Protocol::Nfs => self.nfs = enabled,
            Protocol::Smb => self.smb = enabled,
            Protocol::Iscsi => self.iscsi = enabled,
            Protocol::Nvmeof => self.nvmeof = enabled,
            Protocol::Nut => self.nut = enabled,
            Protocol::Ssh => self.ssh = enabled,
            Protocol::Avahi => self.avahi = enabled,
            Protocol::Smart => self.smart = enabled,
            Protocol::RestServer => self.rest_server = enabled,
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

            // Load kernel modules before starting services (iSCSI/NVMe-oF
            // services require the LIO/nvmet modules to already be present)
            if proto == Protocol::Iscsi {
                for module in &["target_core_mod", "iscsi_target_mod"] {
                    if let Err(e) = modprobe(module).await {
                        warn!("{e}");
                    }
                }
            }
            if proto == Protocol::Nvmeof {
                for module in &["nvmet", "nvmet-tcp"] {
                    if let Err(e) = modprobe(module).await {
                        warn!("{e}");
                    }
                }
            }

            // Start associated services
            for svc in proto.services() {
                if let Err(e) = systemctl("start", svc).await {
                    warn!("Failed to start {svc}: {e}");
                }
            }
        }
    }

    /// List all protocols with their enabled/running status
    pub async fn is_enabled(&self, proto: Protocol) -> bool {
        load_state().await.get(proto)
    }

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
                system_service: proto.is_system_service(),
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

        // Load kernel modules before starting services
        if proto == Protocol::Iscsi {
            for module in &["target_core_mod", "iscsi_target_mod"] {
                if let Err(e) = modprobe(module).await {
                    warn!("{e}");
                }
            }
        }
        if proto == Protocol::Nvmeof {
            for module in &["nvmet", "nvmet-tcp"] {
                if let Err(e) = modprobe(module).await {
                    warn!("{e}");
                }
            }
        }

        // Start associated services
        for svc in proto.services() {
            info!("Starting service {svc} for protocol {}", proto.display_name());
            if let Err(e) = systemctl("start", svc).await {
                warn!("Failed to start {svc}: {e}");
            }
        }

        let running = is_protocol_running(proto).await;
        Ok(ProtocolStatus {
            name: proto.name().to_string(),
            display_name: proto.display_name().to_string(),
            enabled: true,
            running,
            system_service: proto.is_system_service(),
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
            if let Err(e) = systemctl("stop", svc).await {
                warn!("Failed to stop {svc}: {e}");
            }
        }

        let running = is_protocol_running(proto).await;
        Ok(ProtocolStatus {
            name: proto.name().to_string(),
            display_name: proto.display_name().to_string(),
            enabled: false,
            running,
            system_service: proto.is_system_service(),
        })
    }
}

/// Check if a protocol is currently running
async fn is_protocol_running(proto: Protocol) -> bool {
    match proto {
        Protocol::Nfs => systemctl_is_active("nfs-server.service").await,
        Protocol::Smb => systemctl_is_active("samba-smbd.service").await,
        Protocol::Iscsi => std::path::Path::new("/sys/module/iscsi_target_mod").exists(),
        Protocol::Nvmeof => {
            // NVMe-oF is "running" if nvmet configfs is available
            std::path::Path::new("/sys/kernel/config/nvmet").exists()
        }
        Protocol::Nut => systemctl_is_active("nut-server.service").await,
        Protocol::Ssh => systemctl_is_active("sshd.service").await,
        Protocol::Avahi => systemctl_is_active("avahi-daemon.service").await,
        Protocol::Smart => systemctl_is_active("smartd.service").await,
        Protocol::RestServer => systemctl_is_active("nasty-rest-server.service").await,
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
    if proto == Protocol::Nut {
        // Write NUT config files from persisted config before starting daemons
        let config = crate::nut::load_config().await;
        if let Err(e) = crate::nut::write_config_files(&config).await {
            warn!("Failed to write NUT config files: {e}");
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

async fn is_virtual_machine() -> bool {
    tokio::process::Command::new("systemd-detect-virt")
        .arg("--vm")
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn load_state() -> ProtocolState {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(state) => state,
            Err(e) => {
                warn!("Failed to parse protocol state, resetting to defaults: {e}");
                ProtocolState::default()
            }
        },
        Err(_) => {
            // Fresh install: disable SMART by default on VMs since smartd
            // cannot find SMART-capable drives in virtual environments.
            let mut state = ProtocolState::default();
            if is_virtual_machine().await {
                info!("Virtual machine detected — disabling SMART by default");
                state.smart = false;
            }
            state
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
