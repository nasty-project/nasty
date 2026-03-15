use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::cmd;

const NASTY_MOUNT_BASE: &str = "/storage";
const POOL_STATE_PATH: &str = "/var/lib/nasty/pool-state.json";

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("bcachefs command failed: {0}")]
    CommandFailed(String),
    #[error("pool not found: {0}")]
    NotFound(String),
    #[error("pool already exists: {0}")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    pub uuid: String,
    pub devices: Vec<PoolDevice>,
    pub mount_point: Option<String>,
    pub mounted: bool,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    /// Filesystem-level options read from sysfs or show-super.
    pub options: PoolOptions,
}

/// Filesystem-level bcachefs options for a pool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolOptions {
    pub compression: Option<String>,
    pub background_compression: Option<String>,
    pub data_replicas: Option<u32>,
    pub metadata_replicas: Option<u32>,
    pub data_checksum: Option<String>,
    pub metadata_checksum: Option<String>,
    pub foreground_target: Option<String>,
    pub background_target: Option<String>,
    pub promote_target: Option<String>,
    pub metadata_target: Option<String>,
    pub erasure_code: Option<bool>,
    pub encrypted: Option<bool>,
    pub error_action: Option<String>,
}

/// A device within a pool, with its per-device bcachefs configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolDevice {
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

/// Specifies a device and its per-device options for pool creation.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceSpec {
    pub path: String,
    /// Hierarchical label (e.g. "ssd.fast", "hdd.archive").
    pub label: Option<String>,
    /// Durability: 0 = cache, 1 = normal, 2 = hardware RAID.
    pub durability: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePoolRequest {
    pub name: String,
    pub devices: Vec<DeviceSpec>,
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    pub compression: Option<String>,
    pub encryption: Option<bool>,
    /// Filesystem-wide label (used as default when no per-device labels set).
    pub label: Option<String>,
    /// Tiering targets set at format time.
    pub foreground_target: Option<String>,
    pub metadata_target: Option<String>,
    pub background_target: Option<String>,
    pub promote_target: Option<String>,
}

fn default_replicas() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
pub struct DestroyPoolRequest {
    pub name: String,
    pub force: Option<bool>,
}

/// Update runtime-mutable filesystem options on a mounted pool.
/// Options are written directly to sysfs (/sys/fs/bcachefs/<uuid>/options/).
#[derive(Debug, Deserialize)]
pub struct UpdatePoolOptionsRequest {
    pub name: String,
    pub compression: Option<String>,
    pub background_compression: Option<String>,
    pub foreground_target: Option<String>,
    pub background_target: Option<String>,
    pub promote_target: Option<String>,
    pub metadata_target: Option<String>,
    pub error_action: Option<String>,
}

/// Add a device to an existing pool.
#[derive(Debug, Deserialize)]
pub struct DeviceAddRequest {
    pub pool: String,
    pub device: DeviceSpec,
}

/// Remove/evacuate/online/offline a device in a pool.
#[derive(Debug, Deserialize)]
pub struct DeviceActionRequest {
    pub pool: String,
    pub device: String,
}

/// Change the persistent state of a device within a pool.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceSetStateRequest {
    pub pool: String,
    pub device: String,
    /// One of: rw, ro, failed, spare
    pub state: String,
}

/// Detailed filesystem usage from `bcachefs fs usage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceUsage {
    pub path: String,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// Scrub operation status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrubStatus {
    pub running: bool,
    pub raw: String,
}

/// Reconcile (background work) status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileStatus {
    pub raw: String,
}

#[derive(Clone)]
pub struct PoolService;

impl PoolService {
    pub fn new() -> Self {
        Self
    }

    /// Mount pools that were previously tracked as mounted.
    /// Called at startup to restore pool state across reboots.
    pub async fn restore_mounts(&self) {
        let pool_names = load_pool_state().await;
        if pool_names.is_empty() {
            info!("No pools to restore");
            return;
        }

        for name in &pool_names {
            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");

            // Skip if already mounted
            if is_mountpoint(&mount_point).await {
                info!("Pool '{name}' already mounted at {mount_point}");
                continue;
            }

            info!("Restoring pool '{name}'...");
            match self.mount(name).await {
                Ok(_) => info!("Pool '{name}' mounted at {mount_point}"),
                Err(e) => tracing::warn!("Failed to mount pool '{name}': {e}"),
            }
        }
    }

    /// List all bcachefs filesystems (mounted and known via blkid)
    pub async fn list(&self) -> Result<Vec<Pool>, PoolError> {
        let mounts = read_bcachefs_mounts().await?;
        let mut pools = Vec::new();
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

            // Read per-device labels and fs options for mounted pools
            let pool_devices = read_pool_devices(&uuid, devices).await;
            let options = read_fs_options_sysfs(&uuid).await;

            pools.push(Pool {
                name,
                uuid,
                devices: pool_devices,
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
            // Infer pool name: use label if available, else check for existing mount dir
            let name = if !label.is_empty() {
                label
            } else {
                // Look for an existing directory under mount base
                find_pool_name_by_devices(&devices).unwrap_or_else(|| uuid[..8].to_string())
            };

            let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
            let has_mount_dir = Path::new(&mount_point).is_dir();

            let pool_devices = devices
                .iter()
                .map(|d| PoolDevice {
                    path: d.clone(),
                    label: None,
                    durability: None,
                    state: None,
                    data_allowed: None,
                    has_data: None,
                    discard: None,
                })
                .collect();

            // For unmounted pools, try reading options from show-super
            let options = read_fs_options_show_super(devices.first().map(|s| s.as_str())).await;

            pools.push(Pool {
                name,
                uuid,
                devices: pool_devices,
                mount_point: if has_mount_dir { Some(mount_point) } else { None },
                mounted: false,
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
                options,
            });
        }

        Ok(pools)
    }

    /// Get a single pool by name
    pub async fn get(&self, name: &str) -> Result<Pool, PoolError> {
        let pools = self.list().await?;
        pools
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| PoolError::NotFound(name.to_string()))
    }

    /// Create a new bcachefs pool: format devices, create mount point, mount
    pub async fn create(&self, req: CreatePoolRequest) -> Result<Pool, PoolError> {
        if req.devices.is_empty() {
            return Err(PoolError::NoDevices);
        }

        // Validate devices exist
        for dev in &req.devices {
            if !Path::new(&dev.path).exists() {
                return Err(PoolError::DeviceNotFound(dev.path.clone()));
            }
        }

        // Check devices aren't already in use by a bcachefs filesystem
        for dev in &req.devices {
            if is_device_bcachefs(&dev.path).await {
                return Err(PoolError::DeviceInUse(dev.path.clone()));
            }
        }

        // Check mount point doesn't already exist with content
        let mount_point = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        if Path::new(&mount_point).exists() {
            return Err(PoolError::AlreadyExists(req.name.clone()));
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

        // Per-device options go immediately before each device path
        let default_label = req.label.as_deref().unwrap_or(&req.name);
        for dev in &req.devices {
            let label = dev.label.as_deref().unwrap_or(default_label);
            args.push(format!("--label={label}"));

            if let Some(durability) = dev.durability {
                args.push(format!("--durability={durability}"));
            }

            args.push(dev.path.clone());
        }

        // Format
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let dev_paths: Vec<&str> = req.devices.iter().map(|d| d.path.as_str()).collect();
        info!("Formatting bcachefs pool '{}' on {:?}", req.name, dev_paths);
        cmd::run_ok("bcachefs", &arg_refs)
            .await
            .map_err(PoolError::CommandFailed)?;

        // Create mount point
        tokio::fs::create_dir_all(&mount_point).await?;

        // Mount — bcachefs mount takes device(s) colon-separated
        let device_arg = req
            .devices
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join(":");
        info!("Mounting pool '{}' at {}", req.name, mount_point);
        cmd::run_ok("bcachefs", &["mount", &device_arg, &mount_point])
            .await
            .map_err(PoolError::CommandFailed)?;

        // Track mount state for boot reconciliation
        save_pool_mounted(&req.name).await;

        // Read back the pool info
        let uuid = get_fs_uuid(&req.devices[0].path).await.unwrap_or_default();
        let (total, used, available) = get_mount_usage(&mount_point).await.unwrap_or((0, 0, 0));

        let pool_devices = req
            .devices
            .iter()
            .map(|d| PoolDevice {
                path: d.path.clone(),
                label: d.label.clone().or_else(|| Some(default_label.to_string())),
                durability: d.durability,
                state: Some("rw".to_string()),
                data_allowed: None,
                has_data: None,
                discard: None,
            })
            .collect();

        Ok(Pool {
            name: req.name.clone(),
            uuid: uuid.clone(),
            devices: pool_devices,
            mount_point: Some(mount_point),
            mounted: true,
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            options: read_fs_options_sysfs(&uuid).await,
        })
    }

    /// Unmount and optionally wipe a pool
    pub async fn destroy(&self, req: DestroyPoolRequest) -> Result<(), PoolError> {
        let pool = self.get(&req.name).await?;

        // Unmount if mounted
        if pool.mounted {
            if let Some(ref mp) = pool.mount_point {
                info!("Unmounting pool '{}' from {}", req.name, mp);
                cmd::run_ok("umount", &[mp.as_str()])
                    .await
                    .map_err(PoolError::CommandFailed)?;
            }
        }

        // Track mount state
        save_pool_unmounted(&req.name).await;

        // Remove mount point directory if it exists
        let mount_dir = format!("{NASTY_MOUNT_BASE}/{}", req.name);
        let _ = tokio::fs::remove_dir(&mount_dir).await;

        // If force, wipe the superblocks
        if req.force == Some(true) {
            for dev in &pool.devices {
                info!("Wiping bcachefs superblock on {}", dev.path);
                let _ = cmd::run_ok("wipefs", &["-a", &dev.path]).await;
            }
        }

        Ok(())
    }

    /// Mount an existing unmounted pool
    pub async fn mount(&self, name: &str) -> Result<Pool, PoolError> {
        let pool = self.get(name).await?;
        if pool.mounted {
            return Ok(pool);
        }

        let mount_point = format!("{NASTY_MOUNT_BASE}/{name}");
        tokio::fs::create_dir_all(&mount_point).await?;

        let device_arg = pool
            .devices
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join(":");
        cmd::run_ok("bcachefs", &["mount", &device_arg, &mount_point])
            .await
            .map_err(PoolError::CommandFailed)?;

        // Track mount state for boot reconciliation
        save_pool_mounted(name).await;

        self.get(name).await
    }

    /// Update runtime-mutable options on a mounted pool via sysfs.
    pub async fn update_options(&self, req: UpdatePoolOptionsRequest) -> Result<Pool, PoolError> {
        let pool = self.get(&req.name).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to update options".to_string(),
            ));
        }
        let uuid = &pool.uuid;
        let base = format!("/sys/fs/bcachefs/{uuid}/options");

        async fn write_opt(base: &str, name: &str, value: &str) -> Result<(), PoolError> {
            let path = format!("{base}/{name}");
            let v = if value.is_empty() { "none" } else { value };
            tokio::fs::write(&path, v).await.map_err(|e| {
                PoolError::CommandFailed(format!("failed to set {name}: {e}"))
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

        self.get(&req.name).await
    }

    /// Unmount a pool
    pub async fn unmount(&self, name: &str) -> Result<(), PoolError> {
        let pool = self.get(name).await?;
        if let Some(ref mp) = pool.mount_point {
            cmd::run_ok("umount", &[mp.as_str()])
                .await
                .map_err(PoolError::CommandFailed)?;
        }

        // Track mount state
        save_pool_unmounted(name).await;

        Ok(())
    }

    /// List block devices available for pool creation
    pub async fn list_devices(&self) -> Result<Vec<BlockDevice>, PoolError> {
        // Collect all device paths already used by pools
        let pools = self.list().await.unwrap_or_default();
        let pool_devices: std::collections::HashSet<String> = pools
            .iter()
            .flat_map(|p| p.devices.iter().map(|d| d.path.clone()))
            .collect();

        let output = cmd::run_ok("lsblk", &["-Jbno", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,ROTA"])
            .await
            .map_err(PoolError::CommandFailed)?;

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
                pool_devices: &std::collections::HashSet<String>,
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
                        let in_pool = pool_devices.contains(&path);
                        out.push(BlockDevice {
                            path,
                            size_bytes: size,
                            dev_type: dev_type.to_string(),
                            mount_point: mountpoint,
                            fs_type: fstype,
                            in_use: has_mount || in_pool,
                            rotational,
                            device_class,
                        });
                    }

                    if let Some(children) = dev.get("children").and_then(|v| v.as_array()) {
                        collect_devices(children, pool_devices, out);
                    }
                }
            }
            collect_devices(blockdevices, &pool_devices, &mut devices);
        }

        Ok(devices)
    }

    /// Add a device to an existing mounted pool.
    /// bcachefs device add [--label=X] [--durability=X] <mountpoint> <device>
    pub async fn device_add(&self, req: DeviceAddRequest) -> Result<Pool, PoolError> {
        let pool = self.get(&req.pool).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to add a device".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        if !Path::new(&req.device.path).exists() {
            return Err(PoolError::DeviceNotFound(req.device.path.clone()));
        }
        if is_device_bcachefs(&req.device.path).await {
            return Err(PoolError::DeviceInUse(req.device.path.clone()));
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
        info!("Adding device {} to pool '{}'", req.device.path, req.pool);
        cmd::run_ok("bcachefs", &arg_refs)
            .await
            .map_err(PoolError::CommandFailed)?;

        self.get(&req.pool).await
    }

    /// Remove a device from a mounted pool.
    /// This evacuates data first, then removes the device.
    /// bcachefs device remove <mountpoint> <device>
    pub async fn device_remove(&self, req: DeviceActionRequest) -> Result<Pool, PoolError> {
        let pool = self.get(&req.pool).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to remove a device".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        info!("Removing device {} from pool '{}'", req.device, req.pool);
        cmd::run_ok("bcachefs", &["device", "remove", mount_point, &req.device])
            .await
            .map_err(PoolError::CommandFailed)?;

        self.get(&req.pool).await
    }

    /// Evacuate all data off a device (move to other devices in the pool).
    /// This is a prerequisite for safe device removal.
    /// bcachefs device evacuate <device>
    pub async fn device_evacuate(&self, req: DeviceActionRequest) -> Result<(), PoolError> {
        let pool = self.get(&req.pool).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to evacuate a device".to_string(),
            ));
        }

        let mount_point = pool.mount_point.as_ref().unwrap().clone();

        info!("Evacuating device {} in pool '{}'", req.device, req.pool);
        cmd::run_ok("bcachefs", &["device", "evacuate", &req.device])
            .await
            .map_err(PoolError::CommandFailed)?;

        // Mark as spare so bcachefs won't write new data to it and the UI
        // shows a clear visual change (amber "spare" instead of green "rw").
        let _ = cmd::run_ok(
            "bcachefs",
            &["device", "set-state", &mount_point, &req.device, "spare"],
        )
        .await;

        info!("Device {} marked as spare after evacuation", req.device);
        Ok(())
    }

    /// Change the persistent state of a device (rw, ro, failed, spare).
    /// bcachefs device set-state <mountpoint> <device> <state>
    pub async fn device_set_state(&self, req: DeviceSetStateRequest) -> Result<Pool, PoolError> {
        let valid_states = ["rw", "ro", "failed", "spare"];
        if !valid_states.contains(&req.state.as_str()) {
            return Err(PoolError::CommandFailed(format!(
                "invalid device state '{}', must be one of: {}",
                req.state,
                valid_states.join(", ")
            )));
        }

        let pool = self.get(&req.pool).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to change device state".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        info!(
            "Setting device {} state to '{}' in pool '{}'",
            req.device, req.state, req.pool
        );
        cmd::run_ok(
            "bcachefs",
            &["device", "set-state", mount_point, &req.device, &req.state],
        )
        .await
        .map_err(PoolError::CommandFailed)?;

        self.get(&req.pool).await
    }

    /// Bring a device online (temporary, no membership change).
    /// bcachefs device online <mountpoint> <device>
    pub async fn device_online(&self, req: DeviceActionRequest) -> Result<Pool, PoolError> {
        let pool = self.get(&req.pool).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to online a device".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        info!("Onlining device {} in pool '{}'", req.device, req.pool);
        cmd::run_ok("bcachefs", &["device", "online", mount_point, &req.device])
            .await
            .map_err(PoolError::CommandFailed)?;

        self.get(&req.pool).await
    }

    /// Take a device offline (temporary, no membership change).
    /// bcachefs device offline <mountpoint> <device>
    pub async fn device_offline(&self, req: DeviceActionRequest) -> Result<Pool, PoolError> {
        let pool = self.get(&req.pool).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to offline a device".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        info!("Offlining device {} in pool '{}'", req.device, req.pool);
        cmd::run_ok("bcachefs", &["device", "offline", mount_point, &req.device])
            .await
            .map_err(PoolError::CommandFailed)?;

        self.get(&req.pool).await
    }

    // ── Pool health & monitoring ────────────────────────────────

    /// Get detailed filesystem usage from `bcachefs fs usage`.
    pub async fn usage(&self, name: &str) -> Result<FsUsage, PoolError> {
        let pool = self.get(name).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to read usage".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        let raw = cmd::run_ok("bcachefs", &["fs", "usage", mount_point])
            .await
            .map_err(PoolError::CommandFailed)?;

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

    /// Start a data scrub on a pool.
    /// `bcachefs scrub <mountpoint>`
    /// Scrub runs synchronously, so we spawn it in the background.
    pub async fn scrub_start(&self, name: &str) -> Result<(), PoolError> {
        let pool = self.get(name).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to start scrub".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap().clone();

        info!("Starting scrub on pool '{}'", name);
        tokio::spawn(async move {
            match cmd::run_ok("bcachefs", &["scrub", &mount_point]).await {
                Ok(output) => info!("Scrub completed: {}", output),
                Err(e) => warn!("Scrub failed: {}", e),
            }
        });

        Ok(())
    }

    /// Get scrub status for a pool.
    /// bcachefs scrub is synchronous — we check if a scrub process is running.
    pub async fn scrub_status(&self, name: &str) -> Result<ScrubStatus, PoolError> {
        let pool = self.get(name).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to check scrub status".to_string(),
            ));
        }

        // Check if a bcachefs scrub process is running for this pool
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

    /// Get reconcile (background work) status for a pool.
    /// `bcachefs reconcile status <mountpoint>`
    pub async fn reconcile_status(&self, name: &str) -> Result<ReconcileStatus, PoolError> {
        let pool = self.get(name).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted to check reconcile status".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();

        let raw = cmd::run_ok("bcachefs", &["reconcile", "status", mount_point])
            .await
            .unwrap_or_else(|_| "No reconcile data available".to_string());

        Ok(ReconcileStatus { raw })
    }

    /// Raw output of `bcachefs fs usage <mount>` — space breakdown by data type and device.
    pub async fn bcachefs_usage(&self, name: &str) -> Result<String, PoolError> {
        let pool = self.get(name).await?;
        if !pool.mounted {
            return Err(PoolError::CommandFailed(
                "pool must be mounted".to_string(),
            ));
        }
        let mount_point = pool.mount_point.as_ref().unwrap();
        let raw = cmd::run_ok("bcachefs", &["fs", "usage", "-h", mount_point])
            .await
            .map_err(PoolError::CommandFailed)?;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDevice {
    pub path: String,
    pub size_bytes: u64,
    pub dev_type: String,
    pub mount_point: Option<String>,
    pub fs_type: Option<String>,
    pub in_use: bool,
    /// Whether the underlying disk spins (false for NVMe/SSD, true for HDD).
    pub rotational: bool,
    /// Device speed class: "nvme", "ssd", or "hdd".
    pub device_class: String,
}

/// Read per-device info (labels, durability) for a mounted bcachefs filesystem.
/// Uses `bcachefs show-super` on the first device to extract member info.
async fn read_pool_devices(_uuid: &str, device_paths: &[String]) -> Vec<PoolDevice> {
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
                // Take first token, strip trailing punctuation
                if let Some(tok) = rest.split_whitespace().next() {
                    let tok = tok.trim_matches(|c: char| c == ')' || c == ',' || c == ';');
                    if !tok.is_empty() && tok != "(none)" && tok != "none" {
                        return Some(tok.to_string());
                    }
                }
            }
        }
        None
    };

    let mut devices: Vec<PoolDevice> = Vec::new();

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

        devices.push(PoolDevice {
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
async fn read_fs_options_sysfs(uuid: &str) -> PoolOptions {
    if uuid.is_empty() {
        return PoolOptions::default();
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

    PoolOptions {
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
    }
}

/// Read filesystem options from `bcachefs show-super` for an unmounted filesystem.
async fn read_fs_options_show_super(device: Option<&str>) -> PoolOptions {
    let dev = match device {
        Some(d) => d,
        None => return PoolOptions::default(),
    };

    let output = match cmd::run_ok("bcachefs", &["show-super", dev]).await {
        Ok(o) => o,
        Err(_) => return PoolOptions::default(),
    };

    let mut opts = PoolOptions::default();

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
async fn read_bcachefs_mounts() -> Result<HashMap<String, Vec<String>>, PoolError> {
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

// ── Pool mount state persistence ────────────────────────────────

/// Track which pools should be mounted across reboots
async fn save_pool_mounted(pool_name: &str) {
    let mut state = load_pool_state().await;
    let name = pool_name.to_string();
    if !state.contains(&name) {
        state.push(name);
    }
    let _ = save_pool_state(&state).await;
}

async fn save_pool_unmounted(pool_name: &str) {
    let mut state = load_pool_state().await;
    state.retain(|n| n != pool_name);
    let _ = save_pool_state(&state).await;
}

async fn load_pool_state() -> Vec<String> {
    match tokio::fs::read_to_string(POOL_STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn save_pool_state(state: &[String]) -> Result<(), PoolError> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| PoolError::CommandFailed(e.to_string()))?;
    tokio::fs::write(POOL_STATE_PATH, json).await?;
    Ok(())
}

async fn is_mountpoint(path: &str) -> bool {
    tokio::process::Command::new("mountpoint")
        .args(["-q", path])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Try to find pool name from existing mount point directories
fn find_pool_name_by_devices(_devices: &[String]) -> Option<String> {
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
