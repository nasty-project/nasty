pub mod qmp;

use std::path::Path;
use std::process::Stdio;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{error, info, warn};
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
    #[error("invalid USB device: {0}")]
    InvalidUsbDevice(String),
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
            Self::InvalidUsbDevice(_) => -32010,
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
    /// USB devices to pass through. Identified by vendor/product ID
    /// rather than bus/addr because USB enumeration order shuffles
    /// across reboots; pinning to IDs is the stable choice. Caveat:
    /// all devices matching a (vendor, product) pair attach, so
    /// plugging in two identical keyboards passes both through.
    #[serde(default)]
    pub usb_devices: Vec<UsbPassthrough>,
    /// ISO files to attach as CD-ROM devices. The first entry is the
    /// one QEMU treats as the boot CD when `boot_order = "cdrom"`;
    /// additional entries show up as extra read-only CDs inside the
    /// guest (typical use: Windows 11 install needs the Win11 ISO
    /// alongside the virtio-win driver ISO so the installer can see
    /// the virtio storage controller — issue #285).
    #[serde(default)]
    pub cdroms: Vec<String>,
    /// Legacy single-ISO field, kept for cross-version state-file
    /// compatibility. On load we migrate this into `cdroms` if
    /// `cdroms` is empty; on save we mirror `cdroms.first()` back
    /// into here so a hypothetical rollback to a pre-`cdroms` engine
    /// still sees the boot ISO. New code reads `cdroms` exclusively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// CPU model: "host" (default), "max", "qemu64", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// Machine type: "q35" (default for x86), "i440fx".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_type: Option<String>,
    /// VGA device type: "virtio" (default), "qxl", "std", "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vga: Option<String>,
    /// Extra raw QEMU arguments for advanced users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
}

fn default_boot_order() -> String {
    "disk".to_string()
}
fn default_true() -> bool {
    true
}

impl HasId for VmConfig {
    fn id(&self) -> &str {
        &self.id
    }
}

impl VmConfig {
    /// In-memory migration after loading from disk. Old state files
    /// have `boot_iso = "/path"` and an empty `cdroms`; promote the
    /// single ISO into the list so the rest of the engine can stay
    /// single-source-of-truth on `cdroms`. Idempotent — re-running
    /// on an already-migrated config is a no-op.
    pub fn migrate_cdroms(&mut self) {
        if self.cdroms.is_empty()
            && let Some(iso) = self.boot_iso.as_deref()
            && !iso.is_empty()
        {
            self.cdroms.push(iso.to_string());
        }
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

fn default_disk_interface() -> String {
    "virtio".to_string()
}

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

fn default_net_mode() -> String {
    "user".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PassthroughDevice {
    /// PCI address (e.g. "0000:03:00.0").
    pub address: String,
    /// Human-readable label (e.g. "NVIDIA RTX 3080").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UsbPassthrough {
    /// 4-hex-digit USB vendor ID (e.g. "0bda"). Stored lowercase
    /// without the `0x` prefix to match `lsusb` formatting.
    pub vendor_id: String,
    /// 4-hex-digit USB product ID.
    pub product_id: String,
    /// Human-readable label preserved for the UI (e.g. "Realtek
    /// Bluetooth dongle"). The kernel can't tell us this — it comes
    /// from the original `lsusb` listing the user picked from.
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
    /// USB devices to pass through (vendor:product pairs).
    pub usb_devices: Option<Vec<UsbPassthrough>>,
    /// ISO files to attach as CD-ROM devices. First entry boots when
    /// `boot_order = "cdrom"`. See #285 for the Windows-install
    /// motivating case (Win11 ISO + virtio-win driver ISO).
    pub cdroms: Option<Vec<String>>,
    /// Legacy single-ISO field. When set and `cdroms` is unset, the
    /// engine treats it as `cdroms = vec![boot_iso]`. Kept for
    /// clients that haven't been updated to send the new field.
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
    /// Replace USB passthrough devices.
    pub usb_devices: Option<Vec<UsbPassthrough>>,
    /// Replace the CD-ROM list. Empty vec clears all CD-ROMs; absent
    /// (`None`) leaves the existing list untouched.
    pub cdroms: Option<Vec<String>>,
    /// Legacy single-ISO setter. When set and `cdroms` is absent,
    /// the engine treats an empty string as "clear all CD-ROMs" and
    /// a non-empty string as "set CD-ROM list to a single entry."
    /// Use `cdroms` for new code.
    pub boot_iso: Option<String>,
    /// Boot order.
    pub boot_order: Option<String>,
    /// UEFI setting.
    pub uefi: Option<bool>,
    /// Description.
    pub description: Option<String>,
    /// Auto-start.
    pub autostart: Option<bool>,
    /// CPU model.
    pub cpu_model: Option<String>,
    /// Machine type.
    pub machine_type: Option<String>,
    /// VGA device type.
    pub vga: Option<String>,
    /// Extra raw QEMU arguments.
    pub extra_args: Option<Vec<String>>,
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
    let canonical = std::fs::canonicalize(path).map_err(|_| {
        VmError::InvalidDiskPath(format!("{} does not exist or cannot be resolved", path))
    })?;
    let canonical_str = canonical.to_string_lossy();
    if !canonical_str.starts_with("/fs/") && !canonical_str.starts_with("/dev/") {
        return Err(VmError::InvalidDiskPath(format!(
            "{} resolves to {} which is not under /fs/ or /dev/",
            path, canonical_str
        )));
    }
    Ok(())
}

/// Path to the QMP unix socket for a given VM.
fn qmp_socket_path(vm_id: &str) -> String {
    format!("{QMP_DIR}/{vm_id}.qmp")
}

/// Validate a USB vendor/product ID. We format these directly into the
/// QEMU command line, so anything but a 4-hex-digit string is a
/// rejection (defends against a malformed manifest sneaking arbitrary
/// args onto the qemu invocation).
fn validate_usb_id(id: &str) -> Result<(), VmError> {
    if id.len() != 4 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(VmError::InvalidUsbDevice(format!(
            "USB id '{id}' must be 4 hex digits"
        )));
    }
    Ok(())
}

/// Validate every USB passthrough entry in a config.
fn validate_usb_passthroughs(devices: &[UsbPassthrough]) -> Result<(), VmError> {
    for d in devices {
        validate_usb_id(&d.vendor_id)?;
        validate_usb_id(&d.product_id)?;
    }
    Ok(())
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

impl Default for VmService {
    fn default() -> Self {
        Self::new()
    }
}

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
        for mut config in configs {
            config.migrate_cdroms();
            let running = self.is_running(&config.id).await;
            let pid = if running {
                self.get_pid(&config.id).await
            } else {
                None
            };
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
        let mut config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;
        config.migrate_cdroms();

        let running = self.is_running(id).await;
        let pid = if running {
            self.get_pid(id).await
        } else {
            None
        };

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
                        "disk path {} does not exist",
                        disk.path
                    )));
                }
                validate_vm_path(&disk.path)?;
            }
        }
        // Merge the new `cdroms` list with the legacy `boot_iso`
        // single-string field. Clients that haven't been updated
        // still send `boot_iso`; new clients send `cdroms`. When
        // both are present `cdroms` wins (it's the canonical field).
        let cdroms: Vec<String> = match (req.cdroms.clone(), req.boot_iso.clone()) {
            (Some(list), _) => list,
            (None, Some(iso)) if !iso.is_empty() => vec![iso],
            _ => Vec::new(),
        }
        // Drop blank entries — a placeholder "add a CD-ROM" row in the
        // UI can arrive as an empty path and would otherwise fail the
        // existence check below ("CD-ROM ISO  does not exist", #514).
        .into_iter()
        .filter(|iso| !iso.trim().is_empty())
        .collect();
        for iso in &cdroms {
            if !Path::new(iso).exists() {
                return Err(VmError::InvalidDiskPath(format!(
                    "CD-ROM ISO {iso} does not exist"
                )));
            }
            validate_vm_path(iso)?;
        }
        if let Some(ref usb) = req.usb_devices {
            validate_usb_passthroughs(usb)?;
        }

        let id = Uuid::new_v4().to_string();

        let config = VmConfig {
            id: id.clone(),
            name: req.name,
            cpus: req.cpus.unwrap_or(1),
            memory_mib: req.memory_mib.unwrap_or(1024),
            disks: req.disks.unwrap_or_default(),
            networks: req.networks.unwrap_or_else(|| {
                vec![VmNetwork {
                    mode: "user".to_string(),
                    bridge: None,
                    mac: None,
                }]
            }),
            passthrough_devices: req.passthrough_devices.unwrap_or_default(),
            usb_devices: req.usb_devices.unwrap_or_default(),
            // Legacy compat mirror: first cdrom into boot_iso so a
            // rollback engine still sees the boot ISO.
            boot_iso: cdroms.first().cloned(),
            cdroms,
            boot_order: req.boot_order.unwrap_or_else(|| "disk".to_string()),
            uefi: req.uefi.unwrap_or(true),
            description: req.description,
            autostart: req.autostart.unwrap_or(false),
            cpu_model: None,
            machine_type: None,
            vga: None,
            extra_args: None,
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
        config.migrate_cdroms();

        // Don't allow updates while running (except autostart/description)
        let running = self.is_running(&req.id).await;

        if let Some(name) = req.name {
            config.name = name;
        }
        if let Some(desc) = req.description {
            config.description = Some(desc);
        }
        if let Some(auto) = req.autostart {
            config.autostart = auto;
        }

        // Hardware changes require VM to be stopped
        if running {
            if req.cpus.is_some()
                || req.memory_mib.is_some()
                || req.disks.is_some()
                || req.networks.is_some()
                || req.passthrough_devices.is_some()
                || req.usb_devices.is_some()
                || req.cdroms.is_some()
                || req.boot_iso.is_some()
                || req.boot_order.is_some()
                || req.uefi.is_some()
                || req.cpu_model.is_some()
                || req.machine_type.is_some()
                || req.vga.is_some()
                || req.extra_args.is_some()
            {
                return Err(VmError::AlreadyRunning(
                    "stop the VM before changing hardware settings".to_string(),
                ));
            }
        } else {
            if let Some(cpus) = req.cpus {
                config.cpus = cpus;
            }
            if let Some(mem) = req.memory_mib {
                config.memory_mib = mem;
            }
            if let Some(disks) = req.disks {
                config.disks = disks;
            }
            if let Some(nets) = req.networks {
                config.networks = nets;
            }
            if let Some(pt) = req.passthrough_devices {
                config.passthrough_devices = pt;
            }
            if let Some(usb) = req.usb_devices {
                validate_usb_passthroughs(&usb)?;
                config.usb_devices = usb;
            }
            // CD-ROM update: `cdroms` is the canonical setter, but
            // older clients still send `boot_iso`. `cdroms` wins
            // when both are present.
            let new_cdroms: Option<Vec<String>> = match (req.cdroms, req.boot_iso.as_deref()) {
                (Some(list), _) => Some(list),
                (None, Some("")) => Some(Vec::new()),
                (None, Some(iso)) => Some(vec![iso.to_string()]),
                _ => None,
            };
            if let Some(list) = new_cdroms {
                // Drop blank entries (e.g. an unfilled "add ISO" row), #514.
                let list: Vec<String> = list
                    .into_iter()
                    .filter(|iso| !iso.trim().is_empty())
                    .collect();
                for iso in &list {
                    if !Path::new(iso).exists() {
                        return Err(VmError::InvalidDiskPath(format!(
                            "CD-ROM ISO {iso} does not exist"
                        )));
                    }
                    validate_vm_path(iso)?;
                }
                config.cdroms = list;
                // Keep the legacy field mirrored for rollback safety.
                config.boot_iso = config.cdroms.first().cloned();
            }
            if let Some(bo) = req.boot_order {
                config.boot_order = bo;
            }
            if let Some(uefi) = req.uefi {
                config.uefi = uefi;
            }
            if req.cpu_model.is_some() {
                config.cpu_model = req.cpu_model;
            }
            if req.machine_type.is_some() {
                config.machine_type = req.machine_type;
            }
            if req.vga.is_some() {
                config.vga = req.vga;
            }
            if req.extra_args.is_some() {
                config.extra_args = req.extra_args;
            }
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
        let mut config: VmConfig = state_dir()
            .load(id)
            .await
            .ok_or_else(|| VmError::NotFound(id.to_string()))?;
        config.migrate_cdroms();

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
                    "disk path {} does not exist",
                    disk.path
                )));
            }
            validate_vm_path(&disk.path)?;
        }
        for iso in &config.cdroms {
            if !Path::new(iso).exists() {
                return Err(VmError::InvalidDiskPath(format!(
                    "CD-ROM ISO {iso} does not exist"
                )));
            }
            validate_vm_path(iso)?;
        }

        // Ensure runtime directory exists with restrictive permissions (owner-only)
        tokio::fs::create_dir_all(QMP_DIR).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(QMP_DIR, std::fs::Permissions::from_mode(0o700)).await;
        }

        // Copy OVMF_VARS template for this VM if it doesn't exist yet
        if config.uefi {
            let vars = ovmf_vars_path(id);
            if !Path::new(&vars).exists() {
                tokio::fs::copy(OVMF_VARS_TEMPLATE, &vars)
                    .await
                    .map_err(|e| VmError::QemuFailed(format!("failed to copy OVMF_VARS: {e}")))?;
            }
        }

        // Bind passthrough devices to vfio-pci
        for dev in &config.passthrough_devices {
            if let Err(e) = bind_vfio(&dev.address).await {
                warn!("Failed to bind {} to vfio-pci: {e}", dev.address);
            }
        }

        let args = build_qemu_args(&config);
        info!(
            "Starting VM '{}': qemu-system-{} {}",
            config.name,
            std::env::consts::ARCH,
            args.join(" ")
        );

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
            warn!(
                "QMP handshake failed for '{}': {e} (VM may still be starting)",
                config.name
            );
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
        qmp::execute(&qmp_path, "system_powerdown", None)
            .await
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

        // If QMP quit didn't work, try killing by PID. `try_run` logs
        // a kill failure (e.g. PID already gone, EPERM) — useful when
        // a VM "won't stop" reproduces and we want to know why kill -9
        // didn't take effect.
        if let Some(pid) = self.get_pid(id).await {
            nasty_common::cmd::try_run("kill", &["-9", &pid.to_string()]).await;
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
    let cpu_model = config.cpu_model.as_deref().unwrap_or("host");
    if std::env::consts::ARCH == "aarch64" {
        args.extend_from_slice(&["-machine".to_string(), "virt,gic-version=3".to_string()]);
    } else {
        let machine = config.machine_type.as_deref().unwrap_or("q35");
        args.extend_from_slice(&["-machine".to_string(), format!("{machine},accel=kvm")]);
    }
    args.extend_from_slice(&["-cpu".to_string(), cpu_model.to_string()]);

    // Enable KVM
    args.push("-enable-kvm".to_string());

    // CPU and memory
    args.extend_from_slice(&[
        "-smp".to_string(),
        config.cpus.to_string(),
        "-m".to_string(),
        format!("{}M", config.memory_mib),
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
        let cache = disk
            .cache
            .as_deref()
            .map(|c| format!(",cache={c}"))
            .unwrap_or_default();
        let aio = disk
            .aio
            .as_deref()
            .map(|a| format!(",aio={a}"))
            .unwrap_or_default();
        let discard = disk
            .discard
            .as_deref()
            .map(|d| format!(",discard={d}"))
            .unwrap_or_default();
        let mut throttle = String::new();
        if let Some(v) = disk.iops_rd
            && v > 0
        {
            throttle.push_str(&format!(",throttling.iops-read={v}"));
        }
        if let Some(v) = disk.iops_wr
            && v > 0
        {
            throttle.push_str(&format!(",throttling.iops-write={v}"));
        }
        args.extend_from_slice(&[
            "-drive".to_string(),
            format!(
                "file={},format=raw,if={iface},id=drive{i}{ro}{cache}{aio}{discard}{throttle}",
                disk.path
            ),
        ]);
        // Add virtio-blk-pci device for virtio disks
        if disk.interface == "virtio" || disk.interface.is_empty() {
            args.extend_from_slice(&[
                "-device".to_string(),
                format!("virtio-blk-pci,drive=drive{i}"),
            ]);
        }
    }

    // CD-ROM devices. The legacy `-cdrom <path>` shortcut only
    // attaches one ISO at IDE 1:0; multiple CD-ROMs need explicit
    // `-drive` entries with unique indices. We always use the
    // explicit form, including when there's only one ISO, so the
    // emission stays uniform.
    //
    // `if=ide` puts each ISO on the IDE controller present on
    // Q35 (and i440fx) machine types — that's where Windows
    // installers and most live images expect to find their CD.
    // `index=N` is the slot on the controller (0..3 for IDE);
    // first entry is index=0 which is what `boot order=d` boots
    // from when boot_order = "cdrom".
    for (i, iso) in config.cdroms.iter().enumerate() {
        args.extend_from_slice(&[
            "-drive".to_string(),
            format!("file={iso},media=cdrom,if=ide,index={i},readonly=on,id=cd{i}"),
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
        let mac_opt = net
            .mac
            .as_deref()
            .map(|m| format!(",mac={m}"))
            .unwrap_or_default();

        match net.mode.as_str() {
            "bridge" => {
                let br = net.bridge.as_deref().unwrap_or("br0");
                // QEMU's default helper path is the package's libexec, which
                // doesn't have CAP_NET_ADMIN. Point at the NixOS wrapper that
                // does (configured in nasty.nix: security.wrappers.qemu-bridge-helper).
                args.extend_from_slice(&[
                    "-netdev".to_string(),
                    format!("bridge,id=net{i},br={br},helper=/run/wrappers/bin/qemu-bridge-helper"),
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

    // USB passthrough — only emit the XHCI controller when there's at
    // least one device, so VMs without USB passthrough stay minimal.
    if !config.usb_devices.is_empty() {
        args.extend_from_slice(&["-device".to_string(), "qemu-xhci,id=xhci".to_string()]);
        for dev in &config.usb_devices {
            // QEMU expects the `0x` prefix; lsusb-style 4-hex is what
            // we store, so prepend here. Validation lives in
            // `validate_usb_id` so a malformed manifest can't sneak
            // garbage onto the command line.
            args.extend_from_slice(&[
                "-device".to_string(),
                format!(
                    "usb-host,bus=xhci.0,vendorid=0x{},productid=0x{}",
                    dev.vendor_id, dev.product_id
                ),
            ]);
        }
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

    // VGA device (for VNC console)
    let vga = config.vga.as_deref().unwrap_or("virtio");
    if vga != "none" {
        args.extend_from_slice(&["-vga".to_string(), vga.to_string()]);
    }

    // No local display (VNC over unix socket handles console)
    args.push("-display".to_string());
    args.push("none".to_string());

    // Extra raw QEMU args for advanced users
    if let Some(ref extra) = config.extra_args {
        args.extend(extra.iter().cloned());
    }

    args
}

// ── VFIO passthrough helpers ────────────────────────────────────

/// Bind a PCI device to the vfio-pci driver.
async fn bind_vfio(pci_addr: &str) -> Result<(), String> {
    // Unbind from current driver
    let driver_path = format!("/sys/bus/pci/devices/{pci_addr}/driver/unbind");
    if Path::new(&driver_path).exists() {
        tokio::fs::write(&driver_path, pci_addr)
            .await
            .map_err(|e| format!("unbind {pci_addr}: {e}"))?;
    }

    // Get vendor:device ID
    let vendor = tokio::fs::read_to_string(format!("/sys/bus/pci/devices/{pci_addr}/vendor"))
        .await
        .map_err(|e| format!("read vendor: {e}"))?;
    let device = tokio::fs::read_to_string(format!("/sys/bus/pci/devices/{pci_addr}/device"))
        .await
        .map_err(|e| format!("read device: {e}"))?;

    let vendor_device = format!("{} {}", vendor.trim(), device.trim());

    // Tell vfio-pci about this device
    tokio::fs::write("/sys/bus/pci/drivers/vfio-pci/new_id", &vendor_device)
        .await
        .map_err(|e| format!("vfio new_id: {e}"))?;

    Ok(())
}

/// List PCI devices available for passthrough.
async fn list_pci_devices() -> Vec<PciDevice> {
    let output = Command::new("lspci").args(["-Dnn"]).output().await;

    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return vec![],
    };

    let mut devices = Vec::new();
    for line in output.lines() {
        // Format: "0000:03:00.0 VGA compatible controller [0300]: NVIDIA ... [10de:2206] (rev a1)"
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 {
            continue;
        }
        let address = parts[0].to_string();
        let description = parts[1].to_string();

        // Extract vendor:device from brackets
        let vendor_device = line
            .rfind('[')
            .and_then(|start| line.rfind(']').map(|end| &line[start + 1..end]))
            .unwrap_or("")
            .to_string();

        // Find IOMMU group
        let iommu_group =
            tokio::fs::read_link(format!("/sys/bus/pci/devices/{address}/iommu_group"))
                .await
                .ok()
                .and_then(|p| p.file_name()?.to_str()?.parse::<u32>().ok())
                .unwrap_or(0);

        let bound_to_vfio = tokio::fs::read_link(format!("/sys/bus/pci/devices/{address}/driver"))
            .await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> VmConfig {
        VmConfig {
            id: "test-id".to_string(),
            name: "test".to_string(),
            cpus: 1,
            memory_mib: 512,
            disks: vec![],
            networks: vec![],
            passthrough_devices: vec![],
            usb_devices: vec![],
            cdroms: vec![],
            boot_iso: None,
            boot_order: "disk".to_string(),
            uefi: false,
            description: None,
            autostart: false,
            cpu_model: None,
            machine_type: None,
            vga: None,
            extra_args: None,
        }
    }

    #[test]
    fn usb_id_validator_accepts_lowercase_hex() {
        assert!(validate_usb_id("0bda").is_ok());
        assert!(validate_usb_id("ffff").is_ok());
    }

    #[test]
    fn usb_id_validator_accepts_uppercase_hex() {
        assert!(validate_usb_id("0BDA").is_ok());
    }

    #[test]
    fn usb_id_validator_rejects_wrong_length() {
        assert!(validate_usb_id("0bd").is_err());
        assert!(validate_usb_id("0bdaa").is_err());
        assert!(validate_usb_id("").is_err());
    }

    #[test]
    fn usb_id_validator_rejects_non_hex() {
        // Catches injection attempts like " -device foo".
        assert!(validate_usb_id("0xff").is_err()); // the leading "0x" is 4 chars but not hex
        assert!(validate_usb_id("zzzz").is_err());
        assert!(validate_usb_id("00 0").is_err());
    }

    #[test]
    fn qemu_args_omit_xhci_when_no_usb_devices() {
        let cfg = base_config();
        let args = build_qemu_args(&cfg);
        assert!(
            !args.iter().any(|a| a.contains("qemu-xhci")),
            "got: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("usb-host")),
            "got: {args:?}"
        );
    }

    #[test]
    fn qemu_args_emit_xhci_controller_and_each_usb_device() {
        let mut cfg = base_config();
        cfg.usb_devices = vec![
            UsbPassthrough {
                vendor_id: "0bda".to_string(),
                product_id: "8153".to_string(),
                label: Some("Realtek USB Ethernet".to_string()),
            },
            UsbPassthrough {
                vendor_id: "046d".to_string(),
                product_id: "c52b".to_string(),
                label: None,
            },
        ];
        let args = build_qemu_args(&cfg);
        // Exactly one XHCI controller, regardless of device count.
        let xhci_count = args.iter().filter(|a| a.contains("qemu-xhci")).count();
        assert_eq!(xhci_count, 1, "args: {args:?}");
        assert!(
            args.iter()
                .any(|a| a == "usb-host,bus=xhci.0,vendorid=0x0bda,productid=0x8153"),
            "missing first device, args: {args:?}"
        );
        assert!(
            args.iter().any(
                |a| a == "usb-host,bus=xhci.0,vendorid=0x046d,productid=0x52b"
                    || a == "usb-host,bus=xhci.0,vendorid=0x046d,productid=0xc52b"
            ),
            "missing second device, args: {args:?}"
        );
    }

    #[test]
    fn migrate_cdroms_promotes_legacy_boot_iso() {
        // State file from a pre-#285 engine: boot_iso populated,
        // cdroms empty (serde default).
        let mut cfg = base_config();
        cfg.boot_iso = Some("/fs/tank/iso/win11.iso".to_string());
        cfg.migrate_cdroms();
        assert_eq!(cfg.cdroms, vec!["/fs/tank/iso/win11.iso"]);
    }

    #[test]
    fn migrate_cdroms_idempotent() {
        // A freshly-migrated VmConfig has both fields populated and
        // mirrored. Running migrate again must not duplicate.
        let mut cfg = base_config();
        cfg.cdroms = vec!["/fs/tank/iso/a.iso".to_string()];
        cfg.boot_iso = Some("/fs/tank/iso/a.iso".to_string());
        cfg.migrate_cdroms();
        assert_eq!(cfg.cdroms, vec!["/fs/tank/iso/a.iso"]);
    }

    #[test]
    fn migrate_cdroms_skips_when_already_multi() {
        // Multi-cdrom state from a new-engine save. The legacy
        // boot_iso mirror is the first entry; migrate must NOT
        // append it to the existing list.
        let mut cfg = base_config();
        cfg.cdroms = vec!["/iso/a.iso".to_string(), "/iso/b.iso".to_string()];
        cfg.boot_iso = Some("/iso/a.iso".to_string());
        cfg.migrate_cdroms();
        assert_eq!(cfg.cdroms.len(), 2);
        assert_eq!(cfg.cdroms[0], "/iso/a.iso");
        assert_eq!(cfg.cdroms[1], "/iso/b.iso");
    }

    #[test]
    fn migrate_cdroms_empty_boot_iso_treated_as_none() {
        // Legacy state files occasionally have `boot_iso: ""` rather
        // than missing the key — make sure that doesn't end up as a
        // CD-ROM entry with an empty path that QEMU would reject.
        let mut cfg = base_config();
        cfg.boot_iso = Some(String::new());
        cfg.migrate_cdroms();
        assert!(cfg.cdroms.is_empty());
    }

    #[test]
    fn qemu_emits_one_drive_per_cdrom_with_unique_indices() {
        // The motivating scenario: Win11 + virtio-win driver ISO.
        // Both need to be visible to the guest at the same time,
        // first one boots when boot_order = "cdrom".
        let mut cfg = base_config();
        cfg.cdroms = vec![
            "/fs/tank/iso/Win11.iso".to_string(),
            "/fs/tank/iso/virtio-win.iso".to_string(),
        ];
        cfg.boot_order = "cdrom".to_string();
        let args = build_qemu_args(&cfg);

        // Each ISO gets its own -drive entry with a distinct index.
        let drives: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, a)| {
                a.as_str() == "-drive" && args.get(i + 1).is_some_and(|v| v.contains("media=cdrom"))
            })
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert_eq!(drives.len(), 2, "expected 2 CD-ROM drives, args: {args:?}");
        assert!(drives[0].contains("file=/fs/tank/iso/Win11.iso"));
        assert!(drives[0].contains("index=0"));
        assert!(drives[0].contains("readonly=on"));
        assert!(drives[1].contains("file=/fs/tank/iso/virtio-win.iso"));
        assert!(drives[1].contains("index=1"));

        // Boot order still points at the CD-ROM channel.
        let boot_pos = args.iter().position(|a| a == "-boot").expect("boot arg");
        assert_eq!(args[boot_pos + 1], "order=d");
    }

    #[test]
    fn qemu_emits_no_cdrom_when_list_empty() {
        // VM with no ISOs at all (post-install state, or a VM that
        // never had one) emits zero CD-ROM -drive entries. Boot
        // order can still be "disk" / "network".
        let mut cfg = base_config();
        cfg.cdroms = vec![];
        let args = build_qemu_args(&cfg);
        let cdrom_drives = args.iter().filter(|a| a.contains("media=cdrom")).count();
        assert_eq!(cdrom_drives, 0);
    }
}
