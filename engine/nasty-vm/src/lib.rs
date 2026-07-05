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
    #[error("invalid network configuration: {0}")]
    InvalidNetwork(String),
    #[error("PCI passthrough failed: {0}")]
    Passthrough(String),
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
            Self::InvalidNetwork(_) => -32011,
            Self::Passthrough(_) => -32012,
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
    ///
    /// For block-subvolume disks this is a loop device whose number is
    /// reassigned on every reboot, so it must never be trusted as a
    /// stable identifier — `source` is. On start we re-resolve `path`
    /// from `source` (#592) and heal it if the loop device moved.
    pub path: String,
    /// Stable backing file for a block-subvolume disk (the losetup
    /// `BACK-FILE`, e.g. `/fs/tank/vms/foo/vol.img`). Loop device
    /// numbers shuffle across reboots but the backing file does not, so
    /// this is what we persist and re-resolve `path` from at start time.
    /// `None` for plain image-file disks, whose `path` is already stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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
    /// Whether this is an SR-IOV virtual function (has a physfn
    /// parent). VFs are prime passthrough candidates — the host keeps
    /// the PF and siblings while one VF goes to the VM.
    #[serde(default)]
    pub virtual_function: bool,
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

// ── Loop-device resolution (#592) ───────────────────────────────
//
// Block-subvolume disks are backed by a `vol.img` attached to a loop
// device. The loop *number* is reassigned on every reboot, so a VM
// config that stores `/dev/loopN` in `path` points at the wrong (or a
// nonexistent) subvolume after a reboot. The backing file is the stable
// identifier, so we persist it in `VmDisk.source` and re-resolve the
// live loop device from it on start. This mirrors how nasty-storage
// tracks block subvolumes; we parse `losetup` directly here rather than
// take a dependency on the storage crate.

/// Read `(loop_device, backing_file)` pairs from `losetup`. Returns an
/// empty vec on failure — callers treat "not found" as "unattached".
async fn losetup_pairs() -> Vec<(String, String)> {
    let output = match Command::new("losetup")
        .args(["--list", "--output", "NAME,BACK-FILE", "--noheadings"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            warn!(
                "losetup --list failed ({}); VM disks may not resolve",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return Vec::new();
        }
        Err(e) => {
            warn!("could not run losetup: {e}; VM disks may not resolve");
            return Vec::new();
        }
    };
    parse_losetup_pairs(&String::from_utf8_lossy(&output))
}

/// Parse `losetup --list --output NAME,BACK-FILE --noheadings` output
/// into `(device, backing_file)` pairs. Split off for unit testing.
fn parse_losetup_pairs(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(dev), Some(back)) => Some((dev.to_string(), back.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// The backing file a loop device is currently attached to, if any.
fn backing_file_for_device<'a>(pairs: &'a [(String, String)], device: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(dev, _)| dev == device)
        .map(|(_, back)| back.as_str())
}

/// The loop device currently backing a given file, if any.
fn device_for_backing_file<'a>(pairs: &'a [(String, String)], backing: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(_, back)| back == backing)
        .map(|(dev, _)| dev.as_str())
}

/// Populate `source` for any block-device disk that lacks it, by asking
/// losetup what file the current `/dev/loopN` is backed by. Best-effort:
/// the loop mapping must still be valid (i.e. captured before a reboot
/// invalidates it), which is exactly the case at create/update time and
/// on the first start after upgrading past this fix. Returns whether any
/// disk changed. Pure counterpart of the losetup call for testability.
fn backfill_disk_sources(disks: &mut [VmDisk], pairs: &[(String, String)]) -> bool {
    let mut changed = false;
    for disk in disks {
        if disk.source.is_none()
            && disk.path.starts_with("/dev/loop")
            && let Some(back) = backing_file_for_device(pairs, &disk.path)
        {
            disk.source = Some(back.to_string());
            changed = true;
        }
    }
    changed
}

/// Re-resolve `path` for every disk that has a `source`, using the live
/// loop mapping. Returns `Ok(changed)` where `changed` is whether any
/// path moved, or an error naming the first backing file that is no
/// longer attached to a loop device.
fn resolve_disk_paths(disks: &mut [VmDisk], pairs: &[(String, String)]) -> Result<bool, VmError> {
    let mut changed = false;
    for disk in disks {
        let Some(source) = disk.source.as_deref() else {
            continue;
        };
        let Some(dev) = device_for_backing_file(pairs, source) else {
            return Err(VmError::InvalidDiskPath(format!(
                "backing file {source} is not attached to any loop device"
            )));
        };
        if disk.path != dev {
            info!("Remapping VM disk {source}: {} → {dev}", disk.path);
            disk.path = dev.to_string();
            changed = true;
        }
    }
    Ok(changed)
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

        let mut disks = req.disks.unwrap_or_default();
        // Capture the stable backing file for each block-device disk now,
        // while the loop mapping is guaranteed valid, so the disk still
        // resolves after a reboot renumbers the loop devices (#592).
        backfill_disk_sources(&mut disks, &losetup_pairs().await);

        let config = VmConfig {
            id: id.clone(),
            name: req.name,
            cpus: req.cpus.unwrap_or(1),
            memory_mib: req.memory_mib.unwrap_or(1024),
            disks,
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
            if let Some(mut disks) = req.disks {
                // Capture the stable backing file for new block-device
                // disks while the loop mapping is still valid (#592).
                backfill_disk_sources(&mut disks, &losetup_pairs().await);
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

        // Re-resolve block-device disks from their stable backing file
        // before validating: loop device numbers change across reboots,
        // so a persisted `/dev/loopN` may now point at the wrong (or no)
        // subvolume (#592). Backfill `source` for VMs created before this
        // fix, then heal `path` to the live loop device and persist so
        // status/list reflect reality.
        {
            let pairs = losetup_pairs().await;
            let mut dirty = backfill_disk_sources(&mut config.disks, &pairs);
            dirty |= resolve_disk_paths(&mut config.disks, &pairs)?;
            if dirty {
                state_dir().save(id, &config).await?;
            }
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

        // Validate bridge-mode NICs up front: QEMU's bridge helper fails (and the
        // VM never launches) if the target bridge doesn't exist. Catching it here
        // gives the operator a clear reason instead of a daemon that silently
        // never came up.
        validate_bridge_networks(&config.networks)?;

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

        // Bind passthrough devices to vfio-pci. A device that isn't bound
        // makes QEMU exit with an opaque "Could not open /dev/vfio/NN", so
        // fail here with the actual reason instead of launching QEMU.
        for dev in &config.passthrough_devices {
            bind_vfio(&dev.address).await.map_err(|e| {
                VmError::Passthrough(format!("VM '{}': device {}: {e}", config.name, dev.address))
            })?;
        }

        let args = build_qemu_args(&config);
        info!(
            "Starting VM '{}': qemu-system-{} {}",
            config.name,
            std::env::consts::ARCH,
            args.join(" ")
        );

        // QEMU runs as a daemon via -daemonize: the process we spawn forks the
        // long-lived guest and then exits. Crucially it exits *non-zero* if the
        // guest fails to initialize (bad bridge helper, unusable disk, invalid
        // device), so waiting on it turns a previously-silent failure into a
        // real error the operator sees — instead of "Started VM" for a guest
        // that never launched.
        let qemu_bin = format!("qemu-system-{}", std::env::consts::ARCH);
        let output = Command::new(&qemu_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| VmError::QemuFailed(format!("{qemu_bin}: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("QEMU exited with a failure status")
                .to_string();
            return Err(VmError::QemuFailed(format!(
                "VM '{}' failed to start: {reason}",
                config.name
            )));
        }

        // The daemonizing parent only exits 0 once the guest is up and the QMP
        // monitor socket is listening, so the handshake should now succeed. If
        // it doesn't, the VM is not actually usable — surface that rather than
        // reporting a healthy start.
        let qmp_path = qmp_socket_path(id);
        if let Err(e) = qmp::negotiate(&qmp_path).await {
            return Err(VmError::Qmp(format!(
                "VM '{}' launched but its monitor is unreachable: {e}",
                config.name
            )));
        }

        let pid = self.get_pid(id).await;
        info!("VM '{}' started (pid={:?})", config.name, pid);

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

/// Reject bridge-mode NICs that point at a bridge which doesn't exist on the
/// host. QEMU's bridge helper would otherwise fail and the daemonized guest
/// would never come up — historically with no error surfaced to the operator.
fn validate_bridge_networks(networks: &[VmNetwork]) -> Result<(), VmError> {
    for net in networks {
        if net.mode == "bridge" {
            let br = net.bridge.as_deref().unwrap_or("br0");
            if !Path::new(&format!("/sys/class/net/{br}")).exists() {
                return Err(VmError::InvalidNetwork(format!(
                    "bridge '{br}' does not exist — create it under Network \
                     settings (or pick another) before starting this VM"
                )));
            }
        }
    }
    Ok(())
}

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
///
/// Uses the per-device `driver_override` mechanism rather than vfio-pci's
/// global `new_id` registry: `new_id` rejects a vendor:device pair that is
/// already registered (EEXIST), which broke the second boot of a VM — the
/// device had already been unbound from its driver by then and was left
/// bound to nothing, so `/dev/vfio/N` vanished (#601). `driver_override`
/// is idempotent and scoped to the one PCI function being passed through,
/// so it also can't drag sibling devices sharing the same IDs onto vfio-pci.
async fn bind_vfio(pci_addr: &str) -> Result<(), String> {
    let dev_dir = format!("/sys/bus/pci/devices/{pci_addr}");

    let bound_driver = |dir: String| async move {
        tokio::fs::read_link(format!("{dir}/driver"))
            .await
            .ok()
            .and_then(|p| Some(p.file_name()?.to_str()?.to_owned()))
    };

    // Already on vfio-pci — claimed at boot via vfio-pci.ids, or left
    // bound by a previous run of this VM. Rebinding would only add churn.
    let current = bound_driver(dev_dir.clone()).await;
    if current.as_deref() == Some("vfio-pci") {
        return Ok(());
    }

    // Steer the next probe of this specific function to vfio-pci.
    tokio::fs::write(format!("{dev_dir}/driver_override"), "vfio-pci")
        .await
        .map_err(|e| format!("driver_override: {e}"))?;

    // Release it from its current host driver, if any.
    if current.is_some() {
        tokio::fs::write(format!("{dev_dir}/driver/unbind"), pci_addr)
            .await
            .map_err(|e| format!("unbind: {e}"))?;
    }

    // Re-probe; the override routes the device to vfio-pci.
    tokio::fs::write("/sys/bus/pci/drivers_probe", pci_addr)
        .await
        .map_err(|e| format!("drivers_probe: {e}"))?;

    // drivers_probe reports success even when no driver claims the device
    // (e.g. the vfio-pci module isn't loaded), so verify the binding took.
    match bound_driver(dev_dir).await.as_deref() {
        Some("vfio-pci") => Ok(()),
        other => Err(format!(
            "device did not bind to vfio-pci (driver after probe: {}); \
             is the vfio-pci module loaded?",
            other.unwrap_or("none")
        )),
    }
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

        let virtual_function =
            std::path::Path::new(&format!("/sys/bus/pci/devices/{address}/physfn")).exists();
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
            virtual_function,
        });
    }

    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_validation_rejects_missing_bridge() {
        // A name no real host interface will ever have.
        let nets = vec![VmNetwork {
            mode: "bridge".to_string(),
            bridge: Some("nasty-nonexistent-br-zzzzz".to_string()),
            mac: None,
        }];
        let err = validate_bridge_networks(&nets).unwrap_err();
        assert!(matches!(err, VmError::InvalidNetwork(_)));
        assert!(err.to_string().contains("nasty-nonexistent-br-zzzzz"));
    }

    #[test]
    fn bridge_validation_ignores_user_mode() {
        let nets = vec![VmNetwork {
            mode: "user".to_string(),
            bridge: None,
            mac: None,
        }];
        assert!(validate_bridge_networks(&nets).is_ok());
    }

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

    fn disk(path: &str, source: Option<&str>) -> VmDisk {
        VmDisk {
            path: path.to_string(),
            source: source.map(str::to_string),
            interface: "virtio".to_string(),
            readonly: false,
            cache: None,
            aio: None,
            discard: None,
            iops_rd: None,
            iops_wr: None,
        }
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(d, b)| (d.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn parse_losetup_pairs_reads_name_and_backfile() {
        let out = "/dev/loop0 /fs/tank/vms/foo/vol.img\n/dev/loop3 /fs/tank/vms/bar/vol.img\n";
        assert_eq!(
            parse_losetup_pairs(out),
            pairs(&[
                ("/dev/loop0", "/fs/tank/vms/foo/vol.img"),
                ("/dev/loop3", "/fs/tank/vms/bar/vol.img"),
            ])
        );
    }

    #[test]
    fn parse_losetup_pairs_skips_blank_and_partial_lines() {
        // A NAME with no BACK-FILE (unbacked loop) must be dropped, not
        // paired with the next line's device.
        let out = "\n/dev/loop7\n/dev/loop0 /fs/tank/vms/foo/vol.img\n";
        assert_eq!(
            parse_losetup_pairs(out),
            pairs(&[("/dev/loop0", "/fs/tank/vms/foo/vol.img")])
        );
    }

    #[test]
    fn backfill_captures_backing_file_for_loop_disk() {
        let mut disks = vec![disk("/dev/loop0", None)];
        let map = pairs(&[("/dev/loop0", "/fs/tank/vms/foo/vol.img")]);
        assert!(backfill_disk_sources(&mut disks, &map));
        assert_eq!(disks[0].source.as_deref(), Some("/fs/tank/vms/foo/vol.img"));
    }

    #[test]
    fn backfill_leaves_image_file_disks_untouched() {
        // A plain image-file disk (path already stable) has no loop entry
        // and must not gain a source.
        let mut disks = vec![disk("/fs/tank/vms/images/win.img", None)];
        let map = pairs(&[("/dev/loop0", "/fs/tank/vms/foo/vol.img")]);
        assert!(!backfill_disk_sources(&mut disks, &map));
        assert_eq!(disks[0].source, None);
    }

    #[test]
    fn backfill_is_idempotent_when_source_present() {
        let mut disks = vec![disk("/dev/loop9", Some("/fs/tank/vms/foo/vol.img"))];
        // Even though loop9 now backs a different file, an existing source
        // is authoritative and must not be overwritten by backfill.
        let map = pairs(&[("/dev/loop9", "/fs/tank/vms/other/vol.img")]);
        assert!(!backfill_disk_sources(&mut disks, &map));
        assert_eq!(disks[0].source.as_deref(), Some("/fs/tank/vms/foo/vol.img"));
    }

    #[test]
    fn resolve_heals_path_when_loop_number_moved() {
        // The reboot scenario from #592: source is stable, but the disk
        // now lives on a different loop device.
        let mut disks = vec![disk("/dev/loop0", Some("/fs/tank/vms/foo/vol.img"))];
        let map = pairs(&[("/dev/loop5", "/fs/tank/vms/foo/vol.img")]);
        assert!(resolve_disk_paths(&mut disks, &map).unwrap());
        assert_eq!(disks[0].path, "/dev/loop5");
    }

    #[test]
    fn resolve_swaps_two_disks_correctly() {
        // Two block disks whose loop numbers swapped on reboot must each
        // land on the loop backing *their own* file, not each other's.
        let mut disks = vec![
            disk("/dev/loop0", Some("/fs/tank/vms/foo/vol.img")),
            disk("/dev/loop1", Some("/fs/tank/vms/bar/vol.img")),
        ];
        let map = pairs(&[
            ("/dev/loop1", "/fs/tank/vms/foo/vol.img"),
            ("/dev/loop0", "/fs/tank/vms/bar/vol.img"),
        ]);
        assert!(resolve_disk_paths(&mut disks, &map).unwrap());
        assert_eq!(disks[0].path, "/dev/loop1");
        assert_eq!(disks[1].path, "/dev/loop0");
    }

    #[test]
    fn resolve_is_noop_when_path_already_correct() {
        let mut disks = vec![disk("/dev/loop5", Some("/fs/tank/vms/foo/vol.img"))];
        let map = pairs(&[("/dev/loop5", "/fs/tank/vms/foo/vol.img")]);
        assert!(!resolve_disk_paths(&mut disks, &map).unwrap());
        assert_eq!(disks[0].path, "/dev/loop5");
    }

    #[test]
    fn resolve_errors_when_backing_file_unattached() {
        let mut disks = vec![disk("/dev/loop0", Some("/fs/tank/vms/foo/vol.img"))];
        let err = resolve_disk_paths(&mut disks, &[]).unwrap_err();
        assert!(matches!(err, VmError::InvalidDiskPath(_)));
        assert!(err.to_string().contains("/fs/tank/vms/foo/vol.img"));
    }

    #[test]
    fn resolve_ignores_disks_without_source() {
        // Image-file disks carry no source and must pass through even
        // when the loop map is empty.
        let mut disks = vec![disk("/fs/tank/vms/images/win.img", None)];
        assert!(!resolve_disk_paths(&mut disks, &[]).unwrap());
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
