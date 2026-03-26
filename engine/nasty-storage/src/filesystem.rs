use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::cmd;

const NASTY_MOUNT_BASE: &str = "/fs";
const FS_STATE_PATH: &str = "/var/lib/nasty/fs-state.json";
const KEYS_DIR: &str = "/var/lib/nasty/keys";

#[derive(Debug, Error)]
pub enum FilesystemError {
    #[error("bcachefs command failed: {0}")]
    CommandFailed(String),
    #[error("filesystem not found: {0}")]
    NotFound(String),
    #[error("filesystem already exists: {0}")]
    AlreadyExists(String),
    #[error("device {0} is already in use")]
    DeviceInUse(String),
    #[error("no devices specified")]
    NoDevices,
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Filesystem {
    /// Human-readable filesystem name, derived from the mount point directory.
    pub name: String,
    /// bcachefs filesystem UUID.
    pub uuid: String,
    /// Member devices of the filesystem.
    pub devices: Vec<FilesystemDevice>,
    /// Absolute path where the filesystem is mounted (e.g. `/fs/tank`).
    pub mount_point: Option<String>,
    /// Whether the filesystem is currently mounted.
    pub mounted: bool,
    /// Total usable capacity in bytes.
    pub total_bytes: u64,
    /// Bytes currently in use.
    pub used_bytes: u64,
    /// Bytes available for writing.
    pub available_bytes: u64,
    /// Filesystem-level options read from sysfs or show-super.
    pub options: FilesystemOptions,
}

/// Filesystem-level bcachefs options.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FilesystemOptions {
    /// Foreground (inline) compression algorithm (e.g. `lz4`, `zstd`, `none`).
    pub compression: Option<String>,
    /// Background recompression algorithm applied by the background worker.
    pub background_compression: Option<String>,
    /// Number of replicas for data extents.
    pub data_replicas: Option<u32>,
    /// Number of replicas for metadata (btree) extents.
    pub metadata_replicas: Option<u32>,
    /// Checksum algorithm for data (e.g. `crc32c`, `xxhash`).
    pub data_checksum: Option<String>,
    /// Checksum algorithm for metadata.
    pub metadata_checksum: Option<String>,
    /// Target label for foreground (new) writes.
    pub foreground_target: Option<String>,
    /// Target label for background migration writes.
    pub background_target: Option<String>,
    /// Target label for data promotion (cache tier).
    pub promote_target: Option<String>,
    /// Target label for metadata placement.
    pub metadata_target: Option<String>,
    /// Whether erasure coding (EC) is enabled on the filesystem.
    pub erasure_code: Option<bool>,
    /// Whether the filesystem is encrypted at rest.
    pub encrypted: Option<bool>,
    /// Whether the encrypted filesystem is currently locked (needs unlock before mount).
    pub locked: Option<bool>,
    /// Whether a stored key exists for auto-unlock on boot.
    pub key_stored: Option<bool>,
    /// Action on unrecoverable read errors (`continue`, `ro`, `panic`).
    pub error_action: Option<String>,
    /// Version upgrade behavior at mount: `compatible`, `incompatible`, or `none`.
    pub version_upgrade: Option<String>,
    /// Whether mounted in degraded mode (missing devices).
    pub degraded: Option<bool>,
    /// Whether verbose mount logging is enabled.
    pub verbose: Option<bool>,
    /// Whether fsck runs at mount time.
    pub fsck: Option<bool>,
    /// Whether journal flushing is disabled.
    pub journal_flush_disabled: Option<bool>,
}

/// A device within a filesystem, with its per-device bcachefs configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesystemDevice {
    pub path: String,
    /// Hierarchical label (e.g. "ssd.fast", "hdd.archive").
    /// Used for target-based tiering.
    pub label: Option<String>,
    /// How many replicas a copy on this device counts for.
    /// 0 = cache only, 1 = normal (default), 2 = hardware RAID.
    pub durability: Option<u32>,
    /// Persistent device state: rw, ro, evacuating, spare.
    pub state: Option<String>,
    /// Which data types are allowed on this device (e.g. "journal,btree,user").
    pub data_allowed: Option<String>,
    /// Which data types are currently stored on this device (e.g. "btree,user").
    pub has_data: Option<String>,
    /// Whether TRIM/discard is enabled on this device.
    pub discard: Option<bool>,
}

/// Specifies a device and its per-device options for filesystem creation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeviceSpec {
    /// Absolute block device path (e.g. `/dev/sda`).
    pub path: String,
    /// Hierarchical label (e.g. "ssd.fast", "hdd.archive").
    pub label: Option<String>,
    /// Durability: 0 = cache, 1 = normal, 2 = hardware RAID.
    pub durability: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateFilesystemRequest {
    /// Name for the new filesystem; becomes the mount point directory under `/fs/`.
    pub name: String,
    /// Devices to include in the filesystem.
    pub devices: Vec<DeviceSpec>,
    /// Number of data replicas (default 1).
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    /// Inline compression algorithm (e.g. `lz4`, `zstd`, `none`).
    pub compression: Option<String>,
    /// Whether to enable encryption at format time.
    pub encryption: Option<bool>,
    /// Passphrase for encryption (required when encryption is true).
    pub passphrase: Option<String>,
    /// Whether to store the key for auto-unlock on boot (default true).
    /// When false, user must enter passphrase via WebUI after every reboot.
    #[serde(default = "default_store_key")]
    pub store_key: Option<bool>,
    /// Filesystem-wide label (used as default when no per-device labels set).
    pub label: Option<String>,
    /// Tiering targets set at format time.
    pub foreground_target: Option<String>,
    /// Target label for metadata placement.
    pub metadata_target: Option<String>,
    /// Target label for background migration.
    pub background_target: Option<String>,
    /// Target label for data promotion (cache tier).
    pub promote_target: Option<String>,
    /// Whether to enable erasure coding.
    pub erasure_code: Option<bool>,
    /// Data checksum algorithm (e.g. `crc32c`, `crc64`, `xxhash`, `none`).
    pub data_checksum: Option<String>,
    /// Metadata checksum algorithm.
    pub metadata_checksum: Option<String>,
    /// Bucket size in bytes (e.g. `"512k"`, `"1M"`). Affects allocation granularity.
    pub bucket_size: Option<String>,
    /// Maximum encoded extent size (e.g. `"64k"`, `"128k"`).
    pub encoded_extent_max: Option<String>,
    /// Version upgrade behavior at mount time: `compatible`, `incompatible`, or `none`.
    pub version_upgrade: Option<String>,
}

fn default_replicas() -> u32 { 1 }
fn default_store_key() -> Option<bool> { Some(true) }

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DestroyFilesystemRequest {
    /// Name of the filesystem to destroy.
    pub name: String,
    /// If true, wipe bcachefs superblocks from all member devices after unmounting.
    pub force: Option<bool>,
}

/// Update runtime-mutable filesystem options on a mounted filesystem.
/// Options are written directly to sysfs (/sys/fs/bcachefs/<uuid>/options/).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateFilesystemOptionsRequest {
    /// Name of the filesystem to update.
    pub name: String,
    /// Inline compression algorithm (e.g. `lz4`, `zstd`, `none`).
    pub compression: Option<String>,
    /// Background recompression algorithm.
    pub background_compression: Option<String>,
    /// Target label for foreground (new) writes.
    pub foreground_target: Option<String>,
    /// Target label for background migration.
    pub background_target: Option<String>,
    /// Target label for data promotion (cache tier).
    pub promote_target: Option<String>,
    /// Target label for metadata placement.
    pub metadata_target: Option<String>,
    /// Action on unrecoverable read errors (`continue`, `ro`, `panic`).
    pub error_action: Option<String>,
    /// Whether to enable erasure coding.
    pub erasure_code: Option<bool>,
    /// Version upgrade behavior at mount time: `compatible`, `incompatible`, or `none`.
    /// Changing mount options requires a remount.
    pub version_upgrade: Option<String>,
    /// Mount in degraded mode (allow mounting with missing devices).
    pub degraded: Option<bool>,
    /// Enable verbose mount logging.
    pub verbose: Option<bool>,
    /// Run fsck at mount time.
    pub fsck: Option<bool>,
    /// Disable journal flushing (unsafe, for benchmarking).
    pub journal_flush_disabled: Option<bool>,
}

/// Add a device to an existing filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceAddRequest {
    /// Name of the filesystem to add the device to.
    pub filesystem: String,
    /// Device to add, with optional label and durability settings.
    pub device: DeviceSpec,
}

/// Remove/evacuate/online/offline a device in a filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceActionRequest {
    /// Name of the filesystem containing the device.
    pub filesystem: String,
    /// Absolute path of the block device (e.g. `/dev/sdb`).
    pub device: String,
}

/// Set a label on a device in a filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceSetLabelRequest {
    /// Name of the filesystem containing the device.
    pub filesystem: String,
    /// Absolute path of the block device (e.g. `/dev/sdb`).
    pub device: String,
    /// New hierarchical label (e.g. `ssd.fast`, `hdd.archive`).
    pub label: String,
}

/// Change the persistent state of a device within a filesystem.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeviceSetStateRequest {
    /// Name of the filesystem containing the device.
    pub filesystem: String,
    /// Absolute path of the block device (e.g. `/dev/sdb`).
    pub device: String,
    /// One of: rw, ro, failed, spare
    pub state: String,
}

/// Detailed filesystem usage from `bcachefs fs usage`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FsUsage {
    /// Raw output from `bcachefs fs usage`, structured where possible.
    pub raw: String,
    /// Per-device usage breakdown.
    pub devices: Vec<DeviceUsage>,
    /// Total data stored (before replication).
    pub data_bytes: u64,
    /// Total metadata stored.
    pub metadata_bytes: u64,
    /// Reserved bytes.
    pub reserved_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeviceUsage {
    /// Block device path.
    pub path: String,
    /// Bytes currently used on this device.
    pub used_bytes: u64,
    /// Bytes available on this device.
    pub free_bytes: u64,
    /// Total capacity of this device in bytes.
    pub total_bytes: u64,
}

/// Scrub operation status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScrubStatus {
    /// Whether a scrub is currently in progress.
    pub running: bool,
    /// Raw text output from the bcachefs scrub status command.
    pub raw: String,
}

/// Reconcile (background work) status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReconcileStatus {
    /// Raw text output from the bcachefs reconcile status command.
    pub raw: String,
}

#[derive(Clone)]
pub struct FilesystemService;

impl FilesystemService {
    pub fn new() -> Self {
        Self
    }

    /// Mount filesystems that were previously tracked as mounted.
    /// Called at startup to restore filesystem state across reboots.
    pub async fn restore_mounts(&self) {
        let state = load_fs_state().await;
        if state.is_empty() {
            info!("No filesystems to restore");
            return;
        }

        for (name, opts) in &state {
            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");

            // Skip if already mounted
            if is_mountpoint(&mount_point).await {
                info!("Filesystem '{name}' already mounted at {mount_point}");
                continue;
            }

            info!("Restoring filesystem '{name}'...");
            match self.mount_with_opts(name, opts).await {
                Ok(_) => info!("Filesystem '{name}' mounted at {mount_point}"),
                Err(e) => tracing::warn!("Failed to mount filesystem '{name}': {e}"),
            }
        }
    }

    /// List all bcachefs filesystems (mounted and known via blkid)
    pub async fn list(&self) -> Result<Vec<Filesystem>, FilesystemError> {
        let mounts = read_bcachefs_mounts().await?;
        let mut filesystems = Vec::new();
        let mut seen_uuids = std::collections::HashSet::new();

        for (mount_point, devices) in &mounts {
            let uuid = get_fs_uuid(devices.first().map(|s| s.as_str()).unwrap_or(""))
                .await
                .unwrap_or_default();

            let (total, used, available) = get_mount_usage(mount_point).await.unwrap_or((0, 0, 0));

            let name = Path::new(mount_point)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if !uuid.is_empty() {
                seen_uuids.insert(uuid.clone());
            }

            // Read per-device labels and fs options for mounted filesystems
            let fs_devices = read_fs_devices(&uuid, devices).await;
            let options = read_fs_options_sysfs(&uuid).await;

            filesystems.push(Filesystem {
                name,
                uuid,
                devices: fs_devices,
                mount_point: Some(mount_point.clone()),
                mounted: true,
                total_bytes: total,
                used_bytes: used,
                available_bytes: available,
                options,
            });
        }

        // Discover unmounted bcachefs filesystems via blkid
        let unmounted = discover_unmounted_bcachefs(&seen_uuids).await;
        for (uuid, label, devices) in unmounted {
            // Infer filesystem name: use label if available, else check for existing mount dir
            let name = if !label.is_empty() {
                label
            } else {
                // Look for an existing directory under mount base
                find_fs_name_by_devices(&devices).unwrap_or_else(|| uuid[..8].to_string())
            };

            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
            let has_mount_dir = Path::new(&mount_point).is_dir();

            let fs_devices = devices
                .iter()
                .map(|d| FilesystemDevice {
                    path: d.clone(),
                    label: None,
                    durability: None,
                    state: None,
                    data_allowed: None,
                    has_data: None,
                    discard: None,
                })
                .collect();

            // For unmounted filesystems, try reading options from show-super
            let options = read_fs_options_show_super(devices.first().map(|s| s.as_str())).await;

            filesystems.push(Filesystem {
                name,
                uuid,
                devices: fs_devices,
                mount_point: if has_mount_dir { Some(mount_point) } else { None },
                mounted: false,
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
                options,
            });
        }

        // Overlay persisted mount options onto sysfs options
        let state = load_fs_state().await;
        for fs in &mut filesystems {
            if let Some(opts) = state.get(&fs.name) {
                if fs.options.version_upgrade.is_none() { fs.options.version_upgrade = opts.version_upgrade.clone(); }
                if fs.options.degraded.is_none() { fs.options.degraded = opts.degraded; }
                if fs.options.verbose.is_none() { fs.options.verbose = opts.verbose; }
                if fs.options.fsck.is_none() { fs.options.fsck = opts.fsck; }
                if fs.options.journal_flush_disabled.is_none() { fs.options.journal_flush_disabled = opts.journal_flush_disabled; }

                // Encryption state
                if opts.encrypted == Some(true) {
                    if fs.options.encrypted.is_none() { fs.options.encrypted = Some(true); }
                    let key_path = format!("{KEYS_DIR}/{}.key", fs.name);
                    fs.options.key_stored = Some(Path::new(&key_path).exists());
                    // Locked = encrypted + not mounted
                    fs.options.locked = Some(!fs.mounted);
                }
            }
        }

        Ok(filesystems)
    }

    /// Get a single filesystem by name
    pub async fn get(&self, name: &str) -> Result<Filesystem, FilesystemError> {
        let filesystems = self.list().await?;
        filesystems
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| FilesystemError::NotFound(name.to_string()))
    }

    /// Create a new bcachefs filesystem: format devices, create mount point, mount
    pub async fn create(&self, req: CreateFilesystemRequest) -> Result<Filesystem, FilesystemError> {
        if req.devices.is_empty() {
            return Err(FilesystemError::NoDevices);
        }

        // Validate devices exist
        for dev in &req.devices {
            if !Path::new(&dev.path).exists() {
                return Err(FilesystemError::DeviceNotFound(dev.path.clone()));
            }
        }

        // Check devices aren't already in use by a bcachefs filesystem
        for dev in &req.devices {
            if is_device_bcachefs(&dev.path).await {
                return Err(FilesystemError::DeviceInUse(dev.path.clone()));
            }
        }

        // Check mount point doesn't already exist with content
        let mount_point = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        if Path::new(&mount_point).exists() {
            return Err(FilesystemError::AlreadyExists(req.name.clone()));
        }

        // Build bcachefs format command
        // Global options first, then per-device options + device path pairs
        let mut args: Vec<String> = vec!["format".to_string()];

        if req.replicas > 1 {
            args.push(format!("--replicas={}", req.replicas));
        }

        if let Some(ref comp) = req.compression {
            args.push(format!("--compression={comp}"));
        }

        if req.encryption == Some(true) {
            args.push("--encrypted".to_string());
        }

        if let Some(ref t) = req.foreground_target {
            args.push(format!("--foreground_target={t}"));
        }
        if let Some(ref t) = req.metadata_target {
            args.push(format!("--metadata_target={t}"));
        }
        if let Some(ref t) = req.background_target {
            args.push(format!("--background_target={t}"));
        }
        if let Some(ref t) = req.promote_target {
            args.push(format!("--promote_target={t}"));
        }

        if req.erasure_code == Some(true) {
            args.push("--erasure_code".to_string());
        }

        if let Some(ref v) = req.data_checksum {
            args.push(format!("--data_checksum={v}"));
        }
        if let Some(ref v) = req.metadata_checksum {
            args.push(format!("--metadata_checksum={v}"));
        }
        if let Some(ref v) = req.bucket_size {
            args.push(format!("--bucket={v}"));
        }
        if let Some(ref v) = req.encoded_extent_max {
            args.push(format!("--encoded_extent_max={v}"));
        }

        // Per-device options go immediately before each device path
        let has_targets = req.foreground_target.is_some()
            || req.metadata_target.is_some()
            || req.background_target.is_some()
            || req.promote_target.is_some();

        for dev in &req.devices {
            // Only add labels when tiering targets are configured or device has an explicit label
            if let Some(ref label) = dev.label {
                args.push(format!("--label={label}"));
            } else if has_targets {
                // Fall back to filesystem-level label or name when targets need labels to route to
                let default_label = req.label.as_deref().unwrap_or(&req.name);
                args.push(format!("--label={default_label}"));
            }

            if let Some(durability) = dev.durability {
                args.push(format!("--durability={durability}"));
            }

            args.push(dev.path.clone());
        }

        // Format
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let dev_paths: Vec<&str> = req.devices.iter().map(|d| d.path.as_str()).collect();
        let is_encrypted = req.encryption == Some(true);
        info!("Formatting bcachefs filesystem '{}' on {:?}{}", req.name, dev_paths, if is_encrypted { " (encrypted)" } else { "" });

        if is_encrypted {
            let passphrase = req.passphrase.as_deref().ok_or_else(|| {
                FilesystemError::CommandFailed("passphrase required for encrypted filesystem".to_string())
            })?;
            // bcachefs format --encrypted reads passphrase twice from stdin (passphrase + confirm)
            let stdin = format!("{passphrase}\n{passphrase}\n");
            cmd::run_ok_stdin("bcachefs", &arg_refs, stdin.as_bytes())
                .await
                .map_err(FilesystemError::CommandFailed)?;

            // Store key for auto-unlock (default: yes)
            if req.store_key != Some(false) {
                tokio::fs::create_dir_all(KEYS_DIR).await?;
                let key_path = format!("{KEYS_DIR}/{}.key", req.name);
                tokio::fs::write(&key_path, passphrase.as_bytes()).await?;
                info!("Encryption key stored at {key_path}");
            }
        } else {
            cmd::run_ok("bcachefs", &arg_refs)
                .await
                .map_err(FilesystemError::CommandFailed)?;
        }

        // Create mount point
        tokio::fs::create_dir_all(&mount_point).await?;

        let device_arg = req
            .devices
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join(":");

        // Unlock encrypted filesystem before mounting
        if is_encrypted {
            let key_path = format!("{KEYS_DIR}/{}.key", req.name);
            if Path::new(&key_path).exists() {
                cmd::run_ok("bcachefs", &["unlock", "-f", &key_path, &req.devices[0].path])
                    .await
                    .map_err(FilesystemError::CommandFailed)?;
            } else if let Some(ref passphrase) = req.passphrase {
                let stdin = format!("{passphrase}\n");
                cmd::run_ok_stdin("bcachefs", &["unlock", &req.devices[0].path], stdin.as_bytes())
                    .await
                    .map_err(FilesystemError::CommandFailed)?;
            }
        }

        // Mount
        let mount_opts = FsMountOptions {
            encrypted: if is_encrypted { Some(true) } else { None },
            version_upgrade: req.version_upgrade.clone(),
            ..FsMountOptions::default()
        };
        let mount_opt_str = build_mount_opts(&mount_opts);
        info!("Mounting filesystem '{}' at {} with options: {}", req.name, mount_point, mount_opt_str);
        cmd::run_ok("bcachefs", &["mount", "-o", &mount_opt_str, &device_arg, &mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        // Track mount state for boot reconciliation
        save_fs_mounted_with_opts(&req.name, mount_opts).await;

        // Read back the filesystem info
        let uuid = get_fs_uuid(&req.devices[0].path).await.unwrap_or_default();
        let (total, used, available) = get_mount_usage(&mount_point).await.unwrap_or((0, 0, 0));

        let fs_devices = req
            .devices
            .iter()
            .map(|d| FilesystemDevice {
                path: d.path.clone(),
                label: d.label.clone(),
                durability: d.durability,
                state: Some("rw".to_string()),
                data_allowed: None,
                has_data: None,
                discard: None,
            })
            .collect();

        Ok(Filesystem {
            name: req.name.clone(),
            uuid: uuid.clone(),
            devices: fs_devices,
            mount_point: Some(mount_point),
            mounted: true,
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            options: read_fs_options_sysfs(&uuid).await,
        })
    }

    /// Unmount and optionally wipe a filesystem
    pub async fn destroy(&self, req: DestroyFilesystemRequest) -> Result<(), FilesystemError> {
        let fs = self.get(&req.name).await?;

        // Unmount if mounted
        if fs.mounted {
            if let Some(ref mp) = fs.mount_point {
                info!("Unmounting filesystem '{}' from {}", req.name, mp);
                cmd::run_ok("umount", &[mp.as_str()])
                    .await
                    .map_err(FilesystemError::CommandFailed)?;
            }
        }

        // Track mount state
        save_fs_unmounted(&req.name).await;

        // Remove mount point directory if it exists
        let mount_dir = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        let _ = tokio::fs::remove_dir(&mount_dir).await;

        // If force, wipe the superblocks
        if req.force == Some(true) {
            for dev in &fs.devices {
                info!("Wiping bcachefs superblock on {}", dev.path);
                let _ = cmd::run_ok("wipefs", &["-a", &dev.path]).await;
            }
        }

        Ok(())
    }

    /// Mount an existing unmounted filesystem
    pub async fn mount(&self, name: &str) -> Result<Filesystem, FilesystemError> {
        let state = load_fs_state().await;
        let opts = get_fs_mount_options(&state, name);
        self.mount_with_opts(name, &opts).await
    }

    /// Mount with explicit mount options
    async fn mount_with_opts(&self, name: &str, opts: &FsMountOptions) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(name).await?;
        if fs.mounted {
            return Ok(fs);
        }

        let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
        tokio::fs::create_dir_all(&mount_point).await?;

        let first_device = fs.devices.first().map(|d| d.path.as_str()).unwrap_or("");

        // Unlock encrypted filesystem if key is stored
        if opts.encrypted == Some(true) {
            let key_path = format!("{KEYS_DIR}/{name}.key");
            if Path::new(&key_path).exists() {
                cmd::run_ok("bcachefs", &["unlock", "-f", &key_path, first_device])
                    .await
                    .map_err(FilesystemError::CommandFailed)?;
            } else {
                return Err(FilesystemError::CommandFailed(
                    format!("encrypted filesystem '{name}' is locked — unlock it first, then mount.")
                ));
            }
        }

        let device_arg = fs
            .devices
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join(":");
        let mount_opt_str = build_mount_opts(opts);
        cmd::run_ok("bcachefs", &["mount", "-o", &mount_opt_str, &device_arg, &mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        // Track mount state for boot reconciliation
        save_fs_mounted_with_opts(name, opts.clone()).await;

        self.get(name).await
    }

    /// Unlock an encrypted filesystem with a passphrase (does not mount).
    pub async fn unlock(&self, name: &str, passphrase: &str) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(name).await?;

        let first_device = fs.devices.first()
            .map(|d| d.path.clone())
            .ok_or_else(|| FilesystemError::CommandFailed("no devices".to_string()))?;

        let stdin = format!("{passphrase}\n");
        cmd::run_ok_stdin("bcachefs", &["unlock", &first_device], stdin.as_bytes())
            .await
            .map_err(FilesystemError::CommandFailed)?;

        info!("Filesystem '{name}' unlocked");
        self.get(name).await
    }

    /// Export the stored encryption key for a filesystem.
    pub async fn export_key(&self, name: &str) -> Result<String, FilesystemError> {
        let key_path = format!("{KEYS_DIR}/{name}.key");
        tokio::fs::read_to_string(&key_path)
            .await
            .map_err(|_| FilesystemError::CommandFailed(format!("no stored key for filesystem '{name}'")))
    }

    /// Delete the stored encryption key (switch to passphrase-only mode).
    pub async fn delete_key(&self, name: &str) -> Result<(), FilesystemError> {
        let key_path = format!("{KEYS_DIR}/{name}.key");
        tokio::fs::remove_file(&key_path)
            .await
            .map_err(|_| FilesystemError::CommandFailed(format!("no stored key for filesystem '{name}'")))
    }

    /// Update runtime-mutable options on a mounted filesystem via sysfs.
    pub async fn update_options(&self, req: UpdateFilesystemOptionsRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to update options".to_string(),
            ));
        }
        let uuid = &fs.uuid;
        let base = format!("/sys/fs/bcachefs/{uuid}/options");

        async fn write_opt(base: &str, name: &str, value: &str) -> Result<(), FilesystemError> {
            let path = format!("{base}/{name}");
            let v = if value.is_empty() { "none" } else { value };
            tokio::fs::write(&path, v).await.map_err(|e| {
                FilesystemError::CommandFailed(format!("failed to set {name}: {e}"))
            })
        }

        if let Some(ref v) = req.compression {
            write_opt(&base, "compression", v).await?;
        }
        if let Some(ref v) = req.background_compression {
            write_opt(&base, "background_compression", v).await?;
        }
        if let Some(ref v) = req.foreground_target {
            write_opt(&base, "foreground_target", v).await?;
        }
        if let Some(ref v) = req.background_target {
            write_opt(&base, "background_target", v).await?;
        }
        if let Some(ref v) = req.promote_target {
            write_opt(&base, "promote_target", v).await?;
        }
        if let Some(ref v) = req.metadata_target {
            write_opt(&base, "metadata_target", v).await?;
        }
        if let Some(ref v) = req.error_action {
            write_opt(&base, "errors", v).await?;
        }
        if let Some(ec) = req.erasure_code {
            write_opt(&base, "erasure_code", if ec { "1" } else { "0" }).await?;
        }

        // Mount options require a remount to take effect
        let has_mount_changes = req.version_upgrade.is_some()
            || req.degraded.is_some()
            || req.verbose.is_some()
            || req.fsck.is_some()
            || req.journal_flush_disabled.is_some();

        if has_mount_changes {
            let mut state = load_fs_state().await;
            let opts = state.entry(req.name.clone()).or_default();
            if let Some(ref v) = req.version_upgrade { opts.version_upgrade = Some(v.clone()); }
            if let Some(v) = req.degraded { opts.degraded = Some(v); }
            if let Some(v) = req.verbose { opts.verbose = Some(v); }
            if let Some(v) = req.fsck { opts.fsck = Some(v); }
            if let Some(v) = req.journal_flush_disabled { opts.journal_flush_disabled = Some(v); }
            let _ = save_fs_state(&state).await;

            // Remount with new options
            self.unmount(&req.name).await?;
            self.mount(&req.name).await?;
            return self.get(&req.name).await;
        }

        self.get(&req.name).await
    }

    /// Unmount a filesystem
    pub async fn unmount(&self, name: &str) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if let Some(ref mp) = fs.mount_point {
            cmd::run_ok("umount", &[mp.as_str()])
                .await
                .map_err(FilesystemError::CommandFailed)?;
        }

        // Track mount state
        save_fs_unmounted(name).await;

        Ok(())
    }

    /// List block devices available for filesystem creation
    pub async fn list_devices(&self) -> Result<Vec<BlockDevice>, FilesystemError> {
        // Collect all device paths already used by filesystems
        let filesystems = self.list().await.unwrap_or_default();
        let used_devices: std::collections::HashSet<String> = filesystems
            .iter()
            .flat_map(|f| f.devices.iter().map(|d| d.path.clone()))
            .collect();

        let output = cmd::run_ok("lsblk", &["-Jbno", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,ROTA"])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or(serde_json::Value::Null);

        let mut devices = Vec::new();
        if let Some(blockdevices) = parsed.get("blockdevices").and_then(|v| v.as_array()) {
            fn classify(name: &str, rota: bool) -> (bool, String) {
                if name.starts_with("nvme") {
                    return (false, "nvme".to_string());
                }
                if rota {
                    (true, "hdd".to_string())
                } else {
                    (false, "ssd".to_string())
                }
            }

            fn collect_devices(
                devs: &[serde_json::Value],
                fs_devices: &std::collections::HashSet<String>,
                out: &mut Vec<BlockDevice>,
            ) {
                for dev in devs {
                    let name = dev.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let dev_type = dev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let size = dev
                        .get("size")
                        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                        .unwrap_or(0);
                    let mountpoint = dev.get("mountpoint").and_then(|v| v.as_str()).map(String::from);
                    let fstype = dev.get("fstype").and_then(|v| v.as_str()).map(String::from);
                    let rota = dev.get("rota")
                        .and_then(|v| {
                            v.as_bool()
                                .or_else(|| v.as_str().map(|s| s == "1"))
                                .or_else(|| v.as_u64().map(|n| n == 1))
                        })
                        .unwrap_or(false);
                    let (rotational, device_class) = classify(name, rota);

                    if dev_type == "disk" || dev_type == "part" {
                        let path = format!("/dev/{name}");
                        let has_mount = mountpoint.is_some();
                        let in_fs = fs_devices.contains(&path);
                        out.push(BlockDevice {
                            path,
                            size_bytes: size,
                            dev_type: dev_type.to_string(),
                            mount_point: mountpoint,
                            fs_type: fstype,
                            in_use: has_mount || in_fs,
                            rotational,
                            device_class,
                        });
                    }

                    if let Some(children) = dev.get("children").and_then(|v| v.as_array()) {
                        collect_devices(children, fs_devices, out);
                    }
                }
            }
            collect_devices(blockdevices, &used_devices, &mut devices);
        }

        // Mark parent disks as in_use if any of their partitions are in_use.
        // e.g. /dev/sdc shows "Free" even though /dev/sdc1 and /dev/sdc2 are mounted.
        let in_use_paths: std::collections::HashSet<String> = devices.iter()
            .filter(|d| d.in_use && d.dev_type == "part")
            .map(|d| d.path.clone())
            .collect();
        for dev in &mut devices {
            if dev.dev_type == "disk" && !dev.in_use {
                if in_use_paths.iter().any(|p| p.starts_with(&dev.path)) {
                    dev.in_use = true;
                }
            }
        }

        Ok(devices)
    }

    /// Wipe all filesystem signatures from a device.
    /// Only allowed if the device is not currently in use by any filesystem.
    pub async fn device_wipe(&self, path: &str) -> Result<(), FilesystemError> {
        let devices = self.list_devices().await?;
        let dev = devices.iter().find(|d| d.path == path).ok_or_else(|| {
            FilesystemError::CommandFailed(format!("device not found: {path}"))
        })?;
        if dev.in_use {
            return Err(FilesystemError::CommandFailed(format!(
                "device {path} is currently in use"
            )));
        }
        info!("Wiping device {path}");
        cmd::run_ok("wipefs", &["-a", path])
            .await
            .map_err(FilesystemError::CommandFailed)?;
        Ok(())
    }

    /// Add a device to an existing mounted filesystem.
    /// bcachefs device add [--label=X] [--durability=X] <mountpoint> <device>
    pub async fn device_add(&self, req: DeviceAddRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to add a device".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap().clone();

        if !Path::new(&req.device.path).exists() {
            return Err(FilesystemError::DeviceNotFound(req.device.path.clone()));
        }

        // Reject if the device is actively in use (mounted or member of a live filesystem).
        let known_devices = self.list_devices().await?;
        if known_devices.iter().any(|d| d.path == req.device.path && d.in_use) {
            return Err(FilesystemError::DeviceInUse(req.device.path.clone()));
        }
        // Reject if the device has a filesystem signature (including stale bcachefs superblocks
        // left over after removal). The user must explicitly wipe it via Disks → Wipe first.
        if is_device_bcachefs(&req.device.path).await {
            return Err(FilesystemError::CommandFailed(format!(
                "{} has an existing bcachefs superblock. Go to Disks → Wipe to erase it before adding it to a filesystem.",
                req.device.path
            )));
        }

        let mut args: Vec<String> = vec!["device".into(), "add".into()];
        if let Some(ref label) = req.device.label {
            args.push(format!("--label={label}"));
        }
        if let Some(durability) = req.device.durability {
            args.push(format!("--durability={durability}"));
        }
        args.push(mount_point.clone());
        args.push(req.device.path.clone());

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        info!("Adding device {} to filesystem '{}'", req.device.path, req.filesystem);
        cmd::run_ok("bcachefs", &arg_refs)
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.get(&req.filesystem).await
    }

    /// Remove a device from a mounted filesystem.
    /// This evacuates data first, then removes the device.
    /// bcachefs device remove <device> <mountpoint>
    pub async fn device_remove(&self, req: DeviceActionRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to remove a device".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();

        info!("Removing device {} from filesystem '{}'", req.device, req.filesystem);
        cmd::run_ok("bcachefs", &["device", "remove", &req.device, mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.get(&req.filesystem).await
    }

    /// Evacuate all data off a device (move to other devices in the filesystem).
    /// This is a prerequisite for safe device removal.
    /// bcachefs device evacuate <device>
    pub async fn device_evacuate(&self, req: DeviceActionRequest) -> Result<(), FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to evacuate a device".to_string(),
            ));
        }

        info!("Evacuating device {} in filesystem '{}'", req.device, req.filesystem);
        cmd::run_ok("bcachefs", &["device", "evacuate", &req.device])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        // Mark as spare so bcachefs won't write new data to it and the UI
        // shows a clear visual change (amber "spare" instead of green "rw").
        let _ = cmd::run_ok(
            "bcachefs",
            &["device", "set-state", "spare", &req.device],
        )
        .await;

        info!("Device {} marked as spare after evacuation", req.device);
        Ok(())
    }

    /// Change the persistent state of a device (rw, ro, failed, spare).
    /// bcachefs device set-state <new_state> <device> [path]
    pub async fn device_set_state(&self, req: DeviceSetStateRequest) -> Result<Filesystem, FilesystemError> {
        let valid_states = ["rw", "ro", "failed", "spare"];
        if !valid_states.contains(&req.state.as_str()) {
            return Err(FilesystemError::CommandFailed(format!(
                "invalid device state '{}', must be one of: {}",
                req.state,
                valid_states.join(", ")
            )));
        }

        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to change device state".to_string(),
            ));
        }
        info!(
            "Setting device {} state to '{}' in filesystem '{}'",
            req.device, req.state, req.filesystem
        );
        cmd::run_ok(
            "bcachefs",
            &["device", "set-state", &req.state, &req.device],
        )
        .await
        .map_err(FilesystemError::CommandFailed)?;

        self.get(&req.filesystem).await
    }

    /// Bring a device online (temporary, no membership change).
    /// bcachefs device online <device>
    pub async fn device_online(&self, req: DeviceActionRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to online a device".to_string(),
            ));
        }

        info!("Onlining device {} in filesystem '{}'", req.device, req.filesystem);
        cmd::run_ok("bcachefs", &["device", "online", &req.device])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.get(&req.filesystem).await
    }

    /// Take a device offline (temporary, no membership change).
    /// bcachefs device offline <device>
    pub async fn device_offline(&self, req: DeviceActionRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to offline a device".to_string(),
            ));
        }
        info!("Offlining device {} in filesystem '{}'", req.device, req.filesystem);
        cmd::run_ok("bcachefs", &["device", "offline", &req.device])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        self.get(&req.filesystem).await
    }

    /// Set the label on a device of a mounted filesystem via the bcachefs sysfs interface.
    ///
    /// Labels drive tiering target selection (e.g. "ssd.fast", "hdd.archive").
    /// The sysfs entry `/sys/fs/bcachefs/<uuid>/dev-<N>/label` is writable on a
    /// live filesystem; we find the right dev-N by matching the `block` symlink.
    pub async fn device_set_label(&self, req: DeviceSetLabelRequest) -> Result<Filesystem, FilesystemError> {
        let fs = self.get(&req.filesystem).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to set a device label".to_string(),
            ));
        }

        // Validate: device must be a member of the filesystem
        if !fs.devices.iter().any(|d| d.path == req.device) {
            return Err(FilesystemError::CommandFailed(format!(
                "{} is not a member of filesystem '{}'", req.device, req.filesystem
            )));
        }

        // Find the sysfs dev-N directory whose `block` symlink resolves to our device.
        // The symlink target ends with the kernel device name (e.g. "sdc").
        let dev_name = req.device.trim_start_matches("/dev/");
        let sysfs_base = format!("/sys/fs/bcachefs/{}", fs.uuid);
        let mut label_path: Option<std::path::PathBuf> = None;

        let mut rd = tokio::fs::read_dir(&sysfs_base).await.map_err(|e| {
            FilesystemError::CommandFailed(format!("failed to read sysfs {sysfs_base}: {e}"))
        })?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("dev-") {
                continue;
            }
            let block_link = entry.path().join("block");
            if let Ok(target) = tokio::fs::read_link(&block_link).await {
                if target.file_name().map(|n| n == dev_name).unwrap_or(false) {
                    label_path = Some(entry.path().join("label"));
                    break;
                }
            }
        }

        let label_path = label_path.ok_or_else(|| {
            FilesystemError::CommandFailed(format!(
                "could not find sysfs entry for {} in filesystem '{}'", req.device, req.filesystem
            ))
        })?;

        info!("Setting label '{}' on {} in filesystem '{}'", req.label, req.device, req.filesystem);
        tokio::fs::write(&label_path, &req.label).await.map_err(|e| {
            FilesystemError::CommandFailed(format!("failed to write sysfs label: {e}"))
        })?;

        self.get(&req.filesystem).await
    }

    // ── Filesystem health & monitoring ────────────────────────────────

    /// Get detailed filesystem usage from `bcachefs fs usage`.
    pub async fn usage(&self, name: &str) -> Result<FsUsage, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to read usage".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();

        let raw = cmd::run_ok("bcachefs", &["fs", "usage", mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;

        let mut dev_usages = Vec::new();
        let mut data_bytes: u64 = 0;
        let mut metadata_bytes: u64 = 0;
        let mut reserved_bytes: u64 = 0;

        // Parse per-device lines: "  /dev/sdX:  123456 used  789012 free  912468 total"
        // and summary lines for data/metadata/reserved
        for line in raw.lines() {
            let trimmed = line.trim();

            // Per-device usage
            if trimmed.starts_with("/dev/") {
                if let Some(dev_usage) = parse_device_usage_line(trimmed) {
                    dev_usages.push(dev_usage);
                }
            }

            // Summary data/metadata/reserved
            let lower = trimmed.to_lowercase();
            if lower.starts_with("data:") || lower.starts_with("user data:") {
                if let Some(bytes) = extract_first_bytes(trimmed) {
                    data_bytes = bytes;
                }
            } else if lower.starts_with("metadata:") || lower.starts_with("btree:") {
                if let Some(bytes) = extract_first_bytes(trimmed) {
                    metadata_bytes = bytes;
                }
            } else if lower.starts_with("reserved:") {
                if let Some(bytes) = extract_first_bytes(trimmed) {
                    reserved_bytes = bytes;
                }
            }
        }

        Ok(FsUsage {
            raw,
            devices: dev_usages,
            data_bytes,
            metadata_bytes,
            reserved_bytes,
        })
    }

    /// Start a data scrub on a filesystem.
    /// `bcachefs scrub <mountpoint>`
    /// Scrub runs synchronously, so we spawn it in the background.
    pub async fn scrub_start(&self, name: &str) -> Result<(), FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to start scrub".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap().clone();

        info!("Starting scrub on filesystem '{}'", name);
        tokio::spawn(async move {
            match cmd::run_ok("bcachefs", &["scrub", &mount_point]).await {
                Ok(output) => info!("Scrub completed: {}", output),
                Err(e) => warn!("Scrub failed: {}", e),
            }
        });

        Ok(())
    }

    /// Get scrub status for a filesystem.
    /// bcachefs scrub is synchronous — we check if a scrub process is running.
    pub async fn scrub_status(&self, name: &str) -> Result<ScrubStatus, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to check scrub status".to_string(),
            ));
        }

        // Check if a bcachefs scrub process is running for this filesystem
        let running = cmd::run_ok("pgrep", &["-f", &format!("bcachefs scrub")])
            .await
            .is_ok();

        let raw = if running {
            "Scrub in progress...".to_string()
        } else {
            "No scrub running".to_string()
        };

        Ok(ScrubStatus { running, raw })
    }

    /// Get reconcile (background work) status for a filesystem.
    /// `bcachefs reconcile status <mountpoint>`
    pub async fn reconcile_status(&self, name: &str) -> Result<ReconcileStatus, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted to check reconcile status".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();

        let raw = cmd::run_ok("bcachefs", &["reconcile", "status", mount_point])
            .await
            .unwrap_or_else(|_| "No reconcile data available".to_string());

        Ok(ReconcileStatus { raw })
    }

    /// Raw output of `bcachefs fs usage <mount>` — space breakdown by data type and device.
    pub async fn bcachefs_usage(&self, name: &str) -> Result<String, FilesystemError> {
        let fs = self.get(name).await?;
        if !fs.mounted {
            return Err(FilesystemError::CommandFailed(
                "filesystem must be mounted".to_string(),
            ));
        }
        let mount_point = fs.mount_point.as_ref().unwrap();
        let raw = cmd::run_ok("bcachefs", &["fs", "usage", "-h", mount_point])
            .await
            .map_err(FilesystemError::CommandFailed)?;
        Ok(raw)
    }

}

/// Strip ANSI escape sequences (used for bcachefs raw text output).
#[allow(dead_code)]
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() { break; }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a device usage line like "/dev/sda: 123 used  456 free  789 total"
fn parse_device_usage_line(line: &str) -> Option<DeviceUsage> {
    let (path, rest) = line.split_once(':')?;
    let path = path.trim().to_string();

    let mut used = 0u64;
    let mut free = 0u64;
    let mut total = 0u64;

    let parts: Vec<&str> = rest.split_whitespace().collect();
    for chunk in parts.chunks(2) {
        if chunk.len() == 2 {
            if let Ok(n) = chunk[0].parse::<u64>() {
                match chunk[1].to_lowercase().as_str() {
                    "used" => used = n,
                    "free" => free = n,
                    "total" => total = n,
                    _ => {}
                }
            }
        }
    }

    Some(DeviceUsage {
        path,
        used_bytes: used,
        free_bytes: free,
        total_bytes: total,
    })
}

/// Extract the first number (byte count) from a summary line.
fn extract_first_bytes(line: &str) -> Option<u64> {
    let after_colon = line.split_once(':')?.1.trim();
    after_colon.split_whitespace().next()?.parse().ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BlockDevice {
    /// Absolute path of the block device (e.g. `/dev/sda`).
    pub path: String,
    /// Total capacity in bytes.
    pub size_bytes: u64,
    /// lsblk device type: `disk` or `part`.
    pub dev_type: String,
    /// Current mount point, if mounted.
    pub mount_point: Option<String>,
    /// Filesystem type detected on the device (e.g. `bcachefs`, `ext4`).
    pub fs_type: Option<String>,
    /// Whether the device is currently in use (mounted, in a filesystem, or has partitions in use).
    pub in_use: bool,
    /// Whether the underlying disk spins (false for NVMe/SSD, true for HDD).
    pub rotational: bool,
    /// Device speed class: "nvme", "ssd", or "hdd".
    pub device_class: String,
}

/// Read per-device info (labels, durability) for a mounted bcachefs filesystem.
/// Uses `bcachefs show-super` on the first device to extract member info.
async fn read_fs_devices(_uuid: &str, device_paths: &[String]) -> Vec<FilesystemDevice> {
    let first_dev = match device_paths.first() {
        Some(d) => d.as_str(),
        None => return Vec::new(),
    };

    let member_info = cmd::run_ok("bcachefs", &["show-super", "-f", "members_v2", first_dev])
        .await
        .unwrap_or_default();

    // show-super -f members_v2 output comes in two formats:
    //
    // Single-line (older):
    //   Device 0 (label ssd.fast):  /dev/sda  ...  durability: 1  state: rw
    //
    // Multi-line (newer):
    //   Device 0:       /dev/sda
    //           Label:          ssd.fast
    //           State:          rw
    //           Durability:     1
    //
    // Split output into per-device blocks by "Device N:" markers, then scan
    // each block for the info we need regardless of which format is used.

    // Build blocks: each block is all lines from one "Device N:" until the next.
    let lines: Vec<&str> = member_info.lines().collect();
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in &lines {
        let trimmed = line.trim();
        // A new device block starts when a line begins with "Device " followed by a digit.
        if trimmed.starts_with("Device ")
            && trimmed.chars().nth(7).map_or(false, |c| c.is_ascii_digit())
        {
            if !current.is_empty() {
                blocks.push(current.clone());
                current.clear();
            }
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let extract_value = |block: &[&str], key: &str| -> Option<String> {
        for line in block {
            let lower = line.to_lowercase();
            if let Some(pos) = lower.find(key) {
                let rest = &line[pos + key.len()..];
                let rest = rest.trim_start_matches(|c: char| c == ':' || c == ' ' || c == '\t');
                // Take first token, strip surrounding punctuation
                if let Some(tok) = rest.split_whitespace().next() {
                    let tok = tok.trim_matches(|c: char| c == '(' || c == ')' || c == ',' || c == ';');
                    if !tok.is_empty() && tok != "none" {
                        return Some(tok.to_string());
                    }
                }
            }
        }
        None
    };

    let mut devices: Vec<FilesystemDevice> = Vec::new();

    for dev_path in device_paths {
        let dev_short = dev_path.trim_start_matches("/dev/");

        // Find the block that mentions this device path
        let block = blocks.iter().find(|b| {
            b.iter().any(|l| l.contains(dev_path.as_str()) || l.contains(dev_short))
        });

        let (label, durability, state, data_allowed, has_data, discard) =
            if let Some(block) = block {
                let label = extract_value(block, "label");
                let durability =
                    extract_value(block, "durability").and_then(|s| s.parse().ok());
                let state = extract_value(block, "state");
                let data_allowed = extract_value(block, "data allowed");
                let has_data = extract_value(block, "has data");
                let discard = extract_value(block, "discard").map(|s| s == "1" || s == "true");
                (label, durability, state, data_allowed, has_data, discard)
            } else {
                (None, None, None, None, None, None)
            };

        devices.push(FilesystemDevice {
            path: dev_path.clone(),
            label,
            durability,
            state,
            data_allowed,
            has_data,
            discard,
        });
    }

    devices
}

/// Read filesystem options from sysfs for a mounted bcachefs filesystem.
/// Options live at /sys/fs/bcachefs/<uuid>/options/<option_name>
async fn read_fs_options_sysfs(uuid: &str) -> FilesystemOptions {
    if uuid.is_empty() {
        return FilesystemOptions::default();
    }

    let base = format!("/sys/fs/bcachefs/{uuid}/options");

    async fn read_opt(base: &str, name: &str) -> Option<String> {
        let path = format!("{base}/{name}");
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => {
                let v = s.trim().to_string();
                if v.is_empty() || v == "none" || v == "(none)" {
                    None
                } else {
                    Some(v)
                }
            }
            Err(_) => None,
        }
    }

    async fn read_opt_u32(base: &str, name: &str) -> Option<u32> {
        read_opt(base, name).await.and_then(|s| s.parse().ok())
    }

    async fn read_opt_bool(base: &str, name: &str) -> Option<bool> {
        read_opt(base, name).await.map(|s| s == "1" || s == "true")
    }

    FilesystemOptions {
        compression: read_opt(&base, "compression").await,
        background_compression: read_opt(&base, "background_compression").await,
        data_replicas: read_opt_u32(&base, "data_replicas").await,
        metadata_replicas: read_opt_u32(&base, "metadata_replicas").await,
        data_checksum: read_opt(&base, "data_checksum").await,
        metadata_checksum: read_opt(&base, "metadata_checksum").await,
        foreground_target: read_opt(&base, "foreground_target").await,
        background_target: read_opt(&base, "background_target").await,
        promote_target: read_opt(&base, "promote_target").await,
        metadata_target: read_opt(&base, "metadata_target").await,
        erasure_code: read_opt_bool(&base, "erasure_code").await,
        encrypted: read_opt_bool(&base, "encrypted").await,
        error_action: read_opt(&base, "errors").await,
        version_upgrade: read_opt(&base, "version_upgrade").await,
        locked: None,
        key_stored: None,
        degraded: None,
        verbose: None,
        fsck: None,
        journal_flush_disabled: None,
    }
}

/// Read filesystem options from `bcachefs show-super` for an unmounted filesystem.
async fn read_fs_options_show_super(device: Option<&str>) -> FilesystemOptions {
    let dev = match device {
        Some(d) => d,
        None => return FilesystemOptions::default(),
    };

    let output = match cmd::run_ok("bcachefs", &["show-super", dev]).await {
        Ok(o) => o,
        Err(_) => return FilesystemOptions::default(),
    };

    let mut opts = FilesystemOptions::default();

    for line in output.lines() {
        let line = line.trim();
        // show-super outputs lines like "Option:  value" or "Option          value"
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let val = val.trim();
            if val.is_empty() || val == "none" || val == "(none)" {
                continue;
            }
            match key.as_str() {
                "compression" => opts.compression = Some(val.to_string()),
                "background_compression" => opts.background_compression = Some(val.to_string()),
                "data_replicas" => opts.data_replicas = val.parse().ok(),
                "metadata_replicas" => opts.metadata_replicas = val.parse().ok(),
                "data_checksum" => opts.data_checksum = Some(val.to_string()),
                "metadata_checksum" => opts.metadata_checksum = Some(val.to_string()),
                "foreground_target" => opts.foreground_target = Some(val.to_string()),
                "background_target" => opts.background_target = Some(val.to_string()),
                "promote_target" => opts.promote_target = Some(val.to_string()),
                "metadata_target" => opts.metadata_target = Some(val.to_string()),
                "erasure_code" => opts.erasure_code = Some(val == "1" || val == "true"),
                "encrypted" => opts.encrypted = Some(val == "1" || val == "true" || val == "yes"),
                "errors" => opts.error_action = Some(val.to_string()),
                _ => {}
            }
        }
    }

    opts
}

/// Parse /proc/mounts for bcachefs entries.
/// Returns map of mount_point -> list of devices.
async fn read_bcachefs_mounts() -> Result<HashMap<String, Vec<String>>, FilesystemError> {
    let content = tokio::fs::read_to_string("/proc/mounts")
        .await
        .unwrap_or_default();

    let mut mounts: HashMap<String, Vec<String>> = HashMap::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[2] == "bcachefs" {
            let device_str = parts[0]; // could be "dev1:dev2" for multi-device
            let mount_point = parts[1].to_string();
            let devices: Vec<String> = device_str.split(':').map(String::from).collect();
            mounts.insert(mount_point, devices);
        }
    }

    Ok(mounts)
}

/// Get the bcachefs UUID for a device using blkid
async fn get_fs_uuid(device: &str) -> Option<String> {
    let output = cmd::run_ok("blkid", &["-s", "UUID", "-o", "value", device])
        .await
        .ok()?;
    let uuid = output.trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// Get filesystem usage via statvfs-style info from `df`
async fn get_mount_usage(mount_point: &str) -> Option<(u64, u64, u64)> {
    let output = cmd::run_ok("df", &["-B1", "--output=size,used,avail", mount_point])
        .await
        .ok()?;

    // Skip header line, parse second line
    let line = output.lines().nth(1)?;
    let nums: Vec<u64> = line.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    if nums.len() == 3 {
        Some((nums[0], nums[1], nums[2]))
    } else {
        None
    }
}

/// Check if a device already has a bcachefs filesystem
async fn is_device_bcachefs(device: &str) -> bool {
    cmd::run_ok("blkid", &["-s", "TYPE", "-o", "value", device])
        .await
        .map(|s| s.trim() == "bcachefs")
        .unwrap_or(false)
}

/// Discover unmounted bcachefs filesystems via blkid.
/// Returns Vec of (uuid, label, devices) for filesystems not in seen_uuids.
async fn discover_unmounted_bcachefs(
    seen_uuids: &std::collections::HashSet<String>,
) -> Vec<(String, String, Vec<String>)> {
    let output = match cmd::run_ok("blkid", &["-t", "TYPE=bcachefs", "-o", "export"]).await {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    // Parse blkid export format: blocks separated by blank lines
    // Each block has KEY=VALUE lines
    let mut results: HashMap<String, (String, Vec<String>)> = HashMap::new(); // uuid -> (label, devices)

    for block in output.split("\n\n") {
        let mut devname = String::new();
        let mut uuid = String::new();
        let mut label = String::new();

        for line in block.lines() {
            if let Some(val) = line.strip_prefix("DEVNAME=") {
                devname = val.to_string();
            } else if let Some(val) = line.strip_prefix("UUID=") {
                uuid = val.to_string();
            } else if let Some(val) = line.strip_prefix("LABEL_SUB=") {
                label = val.to_string();
            }
        }

        if uuid.is_empty() || devname.is_empty() || seen_uuids.contains(&uuid) {
            continue;
        }

        let entry = results.entry(uuid.clone()).or_insert_with(|| (label.clone(), Vec::new()));
        if !label.is_empty() && entry.0.is_empty() {
            entry.0 = label;
        }
        entry.1.push(devname);
    }

    results
        .into_iter()
        .map(|(uuid, (label, devices))| (uuid, label, devices))
        .collect()
}

// ── Filesystem mount state persistence ────────────────────────────────

/// Track which filesystems should be mounted across reboots
/// Per-filesystem mount state, persisted across reboots.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FsMountOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version_upgrade: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    degraded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verbose: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fsck: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_flush_disabled: Option<bool>,
}

/// Filesystem state: maps fs name → mount options.
type FsState = HashMap<String, FsMountOptions>;

async fn save_fs_mounted(fs_name: &str) {
    save_fs_mounted_with_opts(fs_name, FsMountOptions::default()).await;
}

async fn save_fs_mounted_with_opts(fs_name: &str, opts: FsMountOptions) {
    let mut state = load_fs_state().await;
    state.insert(fs_name.to_string(), opts);
    let _ = save_fs_state(&state).await;
}

async fn save_fs_unmounted(fs_name: &str) {
    let mut state = load_fs_state().await;
    state.remove(fs_name);
    let _ = save_fs_state(&state).await;
}

async fn load_fs_state() -> FsState {
    let content = match tokio::fs::read_to_string(FS_STATE_PATH).await {
        Ok(c) => c,
        Err(_) => return FsState::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

async fn save_fs_state(state: &FsState) -> Result<(), FilesystemError> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| FilesystemError::CommandFailed(e.to_string()))?;
    tokio::fs::write(FS_STATE_PATH, json).await?;
    Ok(())
}

fn get_fs_mount_options(state: &FsState, name: &str) -> FsMountOptions {
    state.get(name).cloned().unwrap_or_default()
}

fn build_mount_opts(opts: &FsMountOptions) -> String {
    let mut parts = vec!["prjquota".to_string()];
    if let Some(ref vu) = opts.version_upgrade {
        if !vu.is_empty() && vu != "none" {
            parts.push(format!("version_upgrade={vu}"));
        }
    }
    if opts.degraded == Some(true) { parts.push("degraded".to_string()); }
    if opts.verbose == Some(true) { parts.push("verbose".to_string()); }
    if opts.fsck == Some(true) { parts.push("fsck".to_string()); }
    if opts.journal_flush_disabled == Some(true) { parts.push("journal_flush_disabled".to_string()); }
    parts.join(",")
}

async fn is_mountpoint(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    // A path is a mount point when its device ID differs from its parent's,
    // or when it is the filesystem root (path == parent, same inode).
    let Ok(meta) = tokio::fs::metadata(path).await else { return false; };
    let parent = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("/"));
    let Ok(parent_meta) = tokio::fs::metadata(parent).await else { return false; };
    meta.dev() != parent_meta.dev() || meta.ino() == parent_meta.ino()
}

/// Try to find filesystem name from existing mount point directories
fn find_fs_name_by_devices(_devices: &[String]) -> Option<String> {
    // Check if any directory exists under the mount base
    let base = Path::new(NASTY_MOUNT_BASE);
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                return Some(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    None
}
