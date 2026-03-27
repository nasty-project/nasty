pub mod qmp;

use std::path::Path;
use std::process::Stdio;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{info, warn, error};
use uuid::Uuid;

const STATE_DIR: &str = "/var/lib/nasty/vms";
const QMP_DIR: &str = "/run/nasty/vm";
const OVMF_CODE: &str = "/etc/nasty/ovmf/OVMF_CODE.fd";
const OVMF_VARS_TEMPLATE: &str = "/etc/nasty/ovmf/OVMF_VARS.fd";

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum VmError {
    #[error("VM not found: {0}")]
    NotFound(String),
    #[error("VM already exists: {0}")]
    AlreadyExists(String),
    #[error("VM is already running: {0}")]
    AlreadyRunning(String),
    #[error("VM is not running: {0}")]
    NotRunning(String),
    #[error("KVM not available: /dev/kvm not found")]
    KvmNotAvailable,
    #[error("invalid disk path: {0}")]
    InvalidDiskPath(String),
    #[error("QEMU command failed: {0}")]
    QemuFailed(String),
    #[error("QMP error: {0}")]
    Qmp(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl VmError {
    pub fn code(&self) -> i64 {
        match self {
            Self::NotFound(_) => -32001,
            Self::AlreadyExists(_) => -32002,
            Self::AlreadyRunning(_) => -32003,
            Self::NotRunning(_) => -32004,
            Self::KvmNotAvailable => -32005,
            Self::InvalidDiskPath(_) => -32009,
            Self::QemuFailed(_) => -32006,
            Self::Qmp(_) => -32007,
            Self::Io(_) => -32008,
        }
    }
}

// ── Types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VmConfig {
    /// Unique VM identifier (UUID).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Number of virtual CPU cores.
    pub cpus: u32,
    /// RAM in MiB.
    pub memory_mib: u64,
    /// Boot disk configuration.
    pub disks: Vec<VmDisk>,
    /// Network interfaces.
    pub networks: Vec<VmNetwork>,
    /// PCI devices to pass through via VFIO.
    pub passthrough_devices: Vec<PassthroughDevice>,
    /// Path to boot ISO (for installation). Removed after first boot if desired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_iso: Option<String>,
    /// Boot order: "disk", "cdrom", or "network".
    #[serde(default = "default_boot_order")]
    pub boot_order: String,
    /// Whether to use UEFI boot (default: true).
    #[serde(default = "default_true")]
    pub uefi: bool,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the VM should auto-start on NASty boot.
    #[serde(default)]
    pub autostart: bool,
}

fn default_boot_order() -> String { "disk".to_string() }
fn default_true() -> bool { true }

impl HasId for VmConfig {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VmDisk {
    /// Disk path — block device (/dev/loopX) or image file.
    pub path: String,
    /// Disk interface: "virtio" (default), "scsi", "ide".
    #[serde(default = "default_disk_interface")]
    pub interface: String,
    /// Whether this is a read-only disk.
    #[serde(default)]
    pub readonly: bool,
    /// Cache mode: "writeback" (default), "writethrough", "none", "unsafe".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// I/O mode: "threads" (default), "native" (requires cache=none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aio: Option<String>,
    /// Discard/TRIM support: "unmap" or "ignore" (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discard: Option<String>,
    /// I/O throttling: max read IOPS (0 = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iops_rd: Option<u64>,
    /// I/O throttling: max write IOPS (0 = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iops_wr: Option<u64>,
}

fn default_disk_interface() -> String { "virtio".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VmNetwork {
    /// Network mode: "bridge" or "user" (NAT).
    #[serde(default = "default_net_mode")]
    pub mode: String,
    /// Bridge name (for bridge mode, e.g. "br0").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    /// MAC address (auto-generated if omitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

fn default_net_mode() -> String { "user".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PassthroughDevice {
    /// PCI address (e.g. "0000:03:00.0").
    pub address: String,
    /// Human-readable label (e.g. "NVIDIA RTX 3080").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct VmStatus {
    /// VM configuration.
    #[serde(flatten)]
    pub config: VmConfig,
    /// Whether the VM is currently running.
    pub running: bool,
    /// QEMU process PID (if running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// VNC display port (if running, for console access).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vnc_port: Option<u16>,
}

// ── Requests ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateVmRequest {
    /// Human-readable name.
    pub name: String,
    /// Number of virtual CPU cores (default: 1).
    pub cpus: Option<u32>,
    /// RAM in MiB (default: 1024).
    pub memory_mib: Option<u64>,
    /// Block device paths for VM disks.
    pub disks: Option<Vec<VmDisk>>,
    /// Network configuration.
    pub networks: Option<Vec<VmNetwork>>,
    /// PCI devices to pass through.
    pub passthrough_devices: Option<Vec<PassthroughDevice>>,
    /// Path to boot ISO for installation.
    pub boot_iso: Option<String>,
    /// Boot order: "disk", "cdrom", or "network".
    pub boot_order: Option<String>,
    /// Use UEFI boot (default: true).
    pub uefi: Option<bool>,
    /// Description.
    pub description: Option<String>,
    /// Auto-start on NASty boot (default: false).
    pub autostart: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateVmRequest {
    /// VM ID.
    pub id: String,
    /// New name.
    pub name: Option<String>,
    /// New CPU count.
    pub cpus: Option<u32>,
    /// New RAM in MiB.
    pub memory_mib: Option<u64>,
    /// Replace disk list.
    pub disks: Option<Vec<VmDisk>>,
    /// Replace network list.
    pub networks: Option<Vec<VmNetwork>>,
    /// Replace passthrough devices.
    pub passthrough_devices: Option<Vec<PassthroughDevice>>,
    /// Set or clear boot ISO.
    pub boot_iso: Option<String>,
    /// Boot order.
    pub boot_order: Option<String>,
    /// UEFI setting.
    pub uefi: Option<bool>,
    /// Description.
    pub description: Option<String>,
    /// Auto-start.
    pub autostart: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotVmRequest {
    /// VM ID.
    pub id: String,
    /// Snapshot name (applied to all disk subvolumes).
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CloneVmRequest {
    /// Source VM ID.
    pub id: String,
    /// Name for the cloned VM.
    pub new_name: String,
}

/// Disk info resolved from a VM's disk path back to filesystem/subvolume.
/// Block subvolumes use loop devices, so the path is `/dev/loopX`.
/// We track the filesystem and subvolume name in the VM disk path comments
/// or resolve them from the subvolume service at runtime.
#[derive(Debug, Serialize, JsonSchema)]
pub struct VmDiskSubvolume {
    /// Filesystem name.
    pub filesystem: String,
    /// Subvolume name.
    pub subvolume: String,
    /// Block device path.
    pub device: String,
}

// ── Capabilities ────────────────────────────────────────────────

/// Runtime capabilities — what the host supports.
#[derive(Debug, Serialize, JsonSchema)]
pub struct VmCapabilities {
    /// Whether KVM hardware acceleration is available.
    pub kvm_available: bool,
    /// Whether OVMF UEFI firmware is available.
    pub uefi_available: bool,
    /// CPU architecture (e.g. "x86_64", "aarch64").
    pub arch: String,
    /// Available PCI devices for passthrough.
    pub passthrough_devices: Vec<PciDevice>,
}

/// A PCI device available for VFIO passthrough.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PciDevice {
    /// PCI address (e.g. "0000:03:00.0").
    pub address: String,
    /// Vendor:device ID (e.g. "10de:2206").
    pub vendor_device: String,
    /// Human-readable description from lspci.
    pub description: String,
    /// IOMMU group number.
    pub iommu_group: u32,
    /// Whether the device is currently bound to vfio-pci.
    pub bound_to_vfio: bool,
}

// ── Service ─────────────────────────────────────────────────────

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

/// Validate that a VM disk/ISO path resolves to an allowed location.
/// After symlink resolution, the path must be under `/fs/` or be a `/dev/` block device.
fn validate_vm_path(path: &str) -> Result<(), VmError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| VmError::InvalidDiskPath(format!("{} does not exist or cannot be resolved", path)))?;
    let canonical_str = canonical.to_string_lossy();
    if !canonical_str.starts_with("/fs/") && !canonical_str.starts_with("/dev/") {
        return Err(VmError::InvalidDiskPath(format!(
            "{} resolves to {} which is not under /fs/ or /dev/", path, canonical_str
        )));
    }
    Ok(())
}

/// Path to the QMP unix socket for a given VM.
fn qmp_socket_path(vm_id: &str) -> String {
    format!("{QMP_DIR}/{vm_id}.qmp")
}

/// Path to the VNC unix socket for a given VM.
fn vnc_socket_path(vm_id: &str) -> String {
    format!("{QMP_DIR}/{vm_id}.vnc")
}

/// Path to the serial console unix socket for a given VM.
fn serial_socket_path(vm_id: &str) -> String {
    format!("{QMP_DIR}/{vm_id}.serial")
}

/// Path to per-VM OVMF_VARS copy (so each VM has its own UEFI variable store).
fn ovmf_vars_path(vm_id: &str) -> String {
    format!("{STATE_DIR}/{vm_id}.ovmf_vars.fd")
}

pub struct VmService;

impl VmService {
    pub fn new() -> Self {
        Self
    }

    // ── Capabilities ────────────────────────────────────────

    /// Check whether the host supports VM features.
    pub async fn capabilities(&self) -> VmCapabilities {
        let kvm = Path::new("/dev/kvm").exists();
        let uefi = Path::new(OVMF_CODE).exists();
        let arch = std::env::consts::ARCH.to_string();
        let passthrough = list_pci_devices().await;

        VmCapabilities {
            kvm_available: kvm,
            uefi_available: uefi,
            arch,
            passthrough_devices: passthrough,
        }
    }

    /// Quick check — is KVM usable?
    pub fn kvm_available(&self) -> bool {
        Path::new("/dev/kvm").exists()
    }

    // ── CRUD ────────────────────────────────────────────────

    pub async fn list(&self) -> Result<Vec<VmStatus>, VmError> {
        let configs: Vec<VmConfig> = state_dir().load_all().await;
        let mut result = Vec::with_capacity(configs.len());
        for config in configs {
            let running = self.is_running(&config.id).await;
            let pid = if running { self.get_pid(&config.id).await } else { None };
            result.push(VmStatus {
                config,
                running,
                pid,
                vnc_port: None, // VNC via unix socket, not TCP port
            });
        }
        Ok(result)
    }

    pub async fn get(&self, id: &str) -> Result<VmStatus, VmError> {
        let config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;

        let running = self.is_running(id).await;
        let pid = if running { self.get_pid(id).await } else { None };

        Ok(VmStatus {
            config,
            running,
            pid,
            vnc_port: None,
        })
    }

    pub async fn create(&self, req: CreateVmRequest) -> Result<VmConfig, VmError> {
        if !self.kvm_available() {
            return Err(VmError::KvmNotAvailable);
        }

        // Check for duplicate name
        let existing: Vec<VmConfig> = state_dir().load_all().await;
        if existing.iter().any(|v| v.name == req.name) {
            return Err(VmError::AlreadyExists(req.name));
        }

        // Validate disk paths exist and are within allowed locations
        if let Some(ref disks) = req.disks {
            for disk in disks {
                if !Path::new(&disk.path).exists() {
                    return Err(VmError::InvalidDiskPath(format!(
                        "disk path {} does not exist", disk.path
                    )));
                }
                validate_vm_path(&disk.path)?;
            }
        }
        if let Some(ref iso) = req.boot_iso {
            if !Path::new(iso).exists() {
                return Err(VmError::InvalidDiskPath(format!(
                    "boot ISO {} does not exist", iso
                )));
            }
            validate_vm_path(iso)?;
        }

        let id = Uuid::new_v4().to_string();

        let config = VmConfig {
            id: id.clone(),
            name: req.name,
            cpus: req.cpus.unwrap_or(1),
            memory_mib: req.memory_mib.unwrap_or(1024),
            disks: req.disks.unwrap_or_default(),
            networks: req.networks.unwrap_or_else(|| vec![VmNetwork {
                mode: "user".to_string(),
                bridge: None,
                mac: None,
            }]),
            passthrough_devices: req.passthrough_devices.unwrap_or_default(),
            boot_iso: req.boot_iso,
            boot_order: req.boot_order.unwrap_or_else(|| "disk".to_string()),
            uefi: req.uefi.unwrap_or(true),
            description: req.description,
            autostart: req.autostart.unwrap_or(false),
        };

        state_dir().save(&id, &config).await?;

        info!("Created VM '{}' (id={})", config.name, id);
        Ok(config)
    }

    pub async fn update(&self, req: UpdateVmRequest) -> Result<VmConfig, VmError> {
        let mut config: VmConfig = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| VmError::NotFound(req.id.clone()))?;

        // Don't allow updates while running (except autostart/description)
        let running = self.is_running(&req.id).await;

        if let Some(name) = req.name { config.name = name; }
        if let Some(desc) = req.description { config.description = Some(desc); }
        if let Some(auto) = req.autostart { config.autostart = auto; }

        // Hardware changes require VM to be stopped
        if running {
            if req.cpus.is_some() || req.memory_mib.is_some() || req.disks.is_some()
                || req.networks.is_some() || req.passthrough_devices.is_some()
                || req.boot_iso.is_some() || req.boot_order.is_some() || req.uefi.is_some()
            {
                return Err(VmError::AlreadyRunning(
                    "stop the VM before changing hardware settings".to_string(),
                ));
            }
        } else {
            if let Some(cpus) = req.cpus { config.cpus = cpus; }
            if let Some(mem) = req.memory_mib { config.memory_mib = mem; }
            if let Some(disks) = req.disks { config.disks = disks; }
            if let Some(nets) = req.networks { config.networks = nets; }
            if let Some(pt) = req.passthrough_devices { config.passthrough_devices = pt; }
            if let Some(iso) = req.boot_iso { config.boot_iso = Some(iso); }
            if let Some(bo) = req.boot_order { config.boot_order = bo; }
            if let Some(uefi) = req.uefi { config.uefi = uefi; }
        }

        state_dir().save(&config.id, &config).await?;
        info!("Updated VM '{}' (id={})", config.name, config.id);
        Ok(config)
    }

    pub async fn delete(&self, id: &str) -> Result<(), VmError> {
        let config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;

        if self.is_running(id).await {
            return Err(VmError::AlreadyRunning(
                "stop the VM before deleting".to_string(),
            ));
        }

        // Clean up OVMF vars file
        let vars_path = ovmf_vars_path(id);
        let _ = tokio::fs::remove_file(&vars_path).await;

        state_dir().remove(id).await?;
        info!("Deleted VM '{}' (id={})", config.name, id);
        Ok(())
    }

    // ── Lifecycle ───────────────────────────────────────────

    pub async fn start(&self, id: &str) -> Result<VmStatus, VmError> {
        let config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;

        if self.is_running(id).await {
            return Err(VmError::AlreadyRunning(config.name));
        }

        if !self.kvm_available() {
            return Err(VmError::KvmNotAvailable);
        }

        // Validate all disk paths exist and are within allowed locations before starting
        for disk in &config.disks {
            if !Path::new(&disk.path).exists() {
                return Err(VmError::InvalidDiskPath(format!(
                    "disk path {} does not exist", disk.path
                )));
            }
            validate_vm_path(&disk.path)?;
        }
        if let Some(ref iso) = config.boot_iso {
            if !Path::new(iso).exists() {
                return Err(VmError::InvalidDiskPath(format!(
                    "boot ISO {} does not exist", iso
                )));
            }
            validate_vm_path(iso)?;
        }

        // Ensure runtime directory exists with restrictive permissions (owner-only)
        tokio::fs::create_dir_all(QMP_DIR).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(QMP_DIR, std::fs::Permissions::from_mode(0o700)).await;
        }

        // Copy OVMF_VARS template for this VM if it doesn't exist yet
        if config.uefi {
            let vars = ovmf_vars_path(id);
            if !Path::new(&vars).exists() {
                tokio::fs::copy(OVMF_VARS_TEMPLATE, &vars).await.map_err(|e| {
                    VmError::QemuFailed(format!("failed to copy OVMF_VARS: {e}"))
                })?;
            }
        }

        // Bind passthrough devices to vfio-pci
        for dev in &config.passthrough_devices {
            if let Err(e) = bind_vfio(&dev.address).await {
                warn!("Failed to bind {} to vfio-pci: {e}", dev.address);
            }
        }

        let args = build_qemu_args(&config);
        info!("Starting VM '{}': qemu-system-{} {}", config.name, std::env::consts::ARCH,
              args.join(" "));

        let qemu_bin = format!("qemu-system-{}", std::env::consts::ARCH);
        let child = Command::new(&qemu_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| VmError::QemuFailed(format!("{qemu_bin}: {e}")))?;

        let pid = child.id();
        info!("VM '{}' started (pid={:?})", config.name, pid);

        // Detach — QEMU runs as a daemon via -daemonize
        // Give it a moment to initialize
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Negotiate QMP handshake
        let qmp_path = qmp_socket_path(id);
        if let Err(e) = qmp::negotiate(&qmp_path).await {
            warn!("QMP handshake failed for '{}': {e} (VM may still be starting)", config.name);
        }

        Ok(VmStatus {
            config,
            running: true,
            pid,
            vnc_port: None,
        })
    }

    /// Graceful shutdown via QMP (sends ACPI power button).
    pub async fn stop(&self, id: &str) -> Result<(), VmError> {
        let config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;

        if !self.is_running(id).await {
            return Err(VmError::NotRunning(config.name));
        }

        let qmp_path = qmp_socket_path(id);
        qmp::execute(&qmp_path, "system_powerdown", None).await
            .map_err(|e| VmError::Qmp(format!("system_powerdown: {e}")))?;

        info!("Sent shutdown signal to VM '{}'", config.name);
        Ok(())
    }

    /// Force-kill the QEMU process.
    pub async fn kill(&self, id: &str) -> Result<(), VmError> {
        let config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;

        if !self.is_running(id).await {
            return Err(VmError::NotRunning(config.name));
        }

        let qmp_path = qmp_socket_path(id);
        let _ = qmp::execute(&qmp_path, "quit", None).await;

        // If QMP quit didn't work, try killing by PID
        if let Some(pid) = self.get_pid(id).await {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output()
                .await;
        }

        // Clean up socket files
        let _ = tokio::fs::remove_file(&qmp_path).await;
        let _ = tokio::fs::remove_file(vnc_socket_path(id)).await;
        let _ = tokio::fs::remove_file(serial_socket_path(id)).await;

        info!("Force-killed VM '{}'", config.name);
        Ok(())
    }

    /// Restore VMs that have autostart=true.
    pub async fn restore(&self) {
        let configs: Vec<VmConfig> = state_dir().load_all().await;
        for config in configs {
            if config.autostart {
                info!("Auto-starting VM '{}'...", config.name);
                if let Err(e) = self.start(&config.id).await {
                    error!("Failed to auto-start VM '{}': {e}", config.name);
                }
            }
        }
    }

    // ── Helpers ─────────────────────────────────────────────

    async fn is_running(&self, id: &str) -> bool {
        let qmp_path = qmp_socket_path(id);
        Path::new(&qmp_path).exists() && qmp::ping(&qmp_path).await.is_ok()
    }

    async fn get_pid(&self, id: &str) -> Option<u32> {
        let qmp_path = qmp_socket_path(id);
        qmp::get_pid(&qmp_path).await.ok()
    }
}

// ── QEMU command builder ────────────────────────────────────────

fn build_qemu_args(config: &VmConfig) -> Vec<String> {
    let mut args = Vec::new();

    // Daemonize — QEMU runs in the background
    args.push("-daemonize".to_string());

    // Machine type
    if std::env::consts::ARCH == "aarch64" {
        args.extend_from_slice(&["-machine".to_string(), "virt,gic-version=3".to_string()]);
        args.extend_from_slice(&["-cpu".to_string(), "host".to_string()]);
    } else {
        args.extend_from_slice(&["-machine".to_string(), "q35,accel=kvm".to_string()]);
        args.extend_from_slice(&["-cpu".to_string(), "host".to_string()]);
    }

    // Enable KVM
    args.push("-enable-kvm".to_string());

    // CPU and memory
    args.extend_from_slice(&[
        "-smp".to_string(), config.cpus.to_string(),
        "-m".to_string(), format!("{}M", config.memory_mib),
    ]);

    // UEFI firmware
    if config.uefi {
        args.extend_from_slice(&[
            "-drive".to_string(),
            format!("if=pflash,format=raw,readonly=on,file={OVMF_CODE}"),
            "-drive".to_string(),
            format!("if=pflash,format=raw,file={}", ovmf_vars_path(&config.id)),
        ]);
    }

    // Disks
    for (i, disk) in config.disks.iter().enumerate() {
        let iface = match disk.interface.as_str() {
            "scsi" => "none", // SCSI uses -device scsi-hd
            "ide" => "ide",
            _ => "none", // virtio uses -device virtio-blk-pci
        };
        let ro = if disk.readonly { ",readonly=on" } else { "" };
        let cache = disk.cache.as_deref().map(|c| format!(",cache={c}")).unwrap_or_default();
        let aio = disk.aio.as_deref().map(|a| format!(",aio={a}")).unwrap_or_default();
        let discard = disk.discard.as_deref().map(|d| format!(",discard={d}")).unwrap_or_default();
        let mut throttle = String::new();
        if let Some(v) = disk.iops_rd { if v > 0 { throttle.push_str(&format!(",throttling.iops-read={v}")); } }
        if let Some(v) = disk.iops_wr { if v > 0 { throttle.push_str(&format!(",throttling.iops-write={v}")); } }
        args.extend_from_slice(&[
            "-drive".to_string(),
            format!("file={},format=raw,if={iface},id=drive{i}{ro}{cache}{aio}{discard}{throttle}", disk.path),
        ]);
        // Add virtio-blk-pci device for virtio disks
        if disk.interface == "virtio" || disk.interface.is_empty() {
            args.extend_from_slice(&[
                "-device".to_string(),
                format!("virtio-blk-pci,drive=drive{i}"),
            ]);
        }
    }

    // Boot ISO (as a CDROM)
    if let Some(ref iso) = config.boot_iso {
        args.extend_from_slice(&[
            "-cdrom".to_string(),
            iso.clone(),
        ]);
    }

    // Boot order
    let boot_char = match config.boot_order.as_str() {
        "cdrom" => "d",
        "network" => "n",
        _ => "c", // disk
    };
    args.extend_from_slice(&["-boot".to_string(), format!("order={boot_char}")]);

    // Network interfaces
    for (i, net) in config.networks.iter().enumerate() {
        let mac_opt = net.mac.as_deref()
            .map(|m| format!(",mac={m}"))
            .unwrap_or_default();

        match net.mode.as_str() {
            "bridge" => {
                let br = net.bridge.as_deref().unwrap_or("br0");
                args.extend_from_slice(&[
                    "-netdev".to_string(),
                    format!("bridge,id=net{i},br={br}"),
                    "-device".to_string(),
                    format!("virtio-net-pci,netdev=net{i}{mac_opt}"),
                ]);
            }
            _ => {
                // User-mode NAT networking
                args.extend_from_slice(&[
                    "-netdev".to_string(),
                    format!("user,id=net{i}"),
                    "-device".to_string(),
                    format!("virtio-net-pci,netdev=net{i}{mac_opt}"),
                ]);
            }
        }
    }

    // VFIO passthrough devices
    for dev in &config.passthrough_devices {
        args.extend_from_slice(&[
            "-device".to_string(),
            format!("vfio-pci,host={}", dev.address),
        ]);
    }

    // QMP control socket
    args.extend_from_slice(&[
        "-qmp".to_string(),
        format!("unix:{},server,nowait", qmp_socket_path(&config.id)),
    ]);

    // VNC over unix socket (for WebUI console)
    args.extend_from_slice(&[
        "-vnc".to_string(),
        format!("unix:{}", vnc_socket_path(&config.id)),
    ]);

    // Serial console over unix socket
    args.extend_from_slice(&[
        "-serial".to_string(),
        format!("unix:{},server,nowait", serial_socket_path(&config.id)),
    ]);

    // No default display
    args.push("-display".to_string());
    args.push("none".to_string());

    args
}

// ── VFIO passthrough helpers ────────────────────────────────────

/// Bind a PCI device to the vfio-pci driver.
async fn bind_vfio(pci_addr: &str) -> Result<(), String> {
    // Unbind from current driver
    let driver_path = format!("/sys/bus/pci/devices/{pci_addr}/driver/unbind");
    if Path::new(&driver_path).exists() {
        tokio::fs::write(&driver_path, pci_addr).await
            .map_err(|e| format!("unbind {pci_addr}: {e}"))?;
    }

    // Get vendor:device ID
    let vendor = tokio::fs::read_to_string(format!("/sys/bus/pci/devices/{pci_addr}/vendor"))
        .await.map_err(|e| format!("read vendor: {e}"))?;
    let device = tokio::fs::read_to_string(format!("/sys/bus/pci/devices/{pci_addr}/device"))
        .await.map_err(|e| format!("read device: {e}"))?;

    let vendor_device = format!("{} {}", vendor.trim(), device.trim());

    // Tell vfio-pci about this device
    tokio::fs::write("/sys/bus/pci/drivers/vfio-pci/new_id", &vendor_device)
        .await
        .map_err(|e| format!("vfio new_id: {e}"))?;

    Ok(())
}

/// List PCI devices available for passthrough.
async fn list_pci_devices() -> Vec<PciDevice> {
    let output = Command::new("lspci")
        .args(["-Dnn"])
        .output()
        .await;

    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return vec![],
    };

    let mut devices = Vec::new();
    for line in output.lines() {
        // Format: "0000:03:00.0 VGA compatible controller [0300]: NVIDIA ... [10de:2206] (rev a1)"
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 { continue; }
        let address = parts[0].to_string();
        let description = parts[1].to_string();

        // Extract vendor:device from brackets
        let vendor_device = line.rfind('[')
            .and_then(|start| line.rfind(']').map(|end| &line[start+1..end]))
            .unwrap_or("")
            .to_string();

        // Find IOMMU group
        let iommu_group = tokio::fs::read_link(
            format!("/sys/bus/pci/devices/{address}/iommu_group")
        ).await
            .ok()
            .and_then(|p| p.file_name()?.to_str()?.parse::<u32>().ok())
            .unwrap_or(0);

        let bound_to_vfio = tokio::fs::read_link(
            format!("/sys/bus/pci/devices/{address}/driver")
        ).await
            .ok()
            .and_then(|p| Some(p.file_name()?.to_str()? == "vfio-pci"))
            .unwrap_or(false);

        devices.push(PciDevice {
            address,
            vendor_device,
            description,
            iommu_group,
            bound_to_vfio,
        });
    }

    devices
}
