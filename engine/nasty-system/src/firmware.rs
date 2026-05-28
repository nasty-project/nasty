//! Firmware update management via fwupd.
//!
//! Wraps `fwupdmgr` CLI to list devices, check for updates, and apply them.

use schemars::JsonSchema;
use serde::Serialize;
use tokio::process::Command;
use tracing::{info, warn};

/// A device known to fwupd.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FirmwareDevice {
    /// Device name (e.g. "UEFI dbx", "WD Black SN850X").
    pub name: String,
    /// Device ID (fwupd GUID).
    pub device_id: String,
    /// Currently installed firmware version.
    pub version: String,
    /// Vendor name.
    pub vendor: String,
    /// Whether an update is available.
    pub update_available: bool,
    /// Available update version (if any).
    pub update_version: Option<String>,
    /// Update description/summary.
    pub update_description: Option<String>,
}

/// Result of a firmware update operation.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FirmwareUpdateResult {
    pub device_name: String,
    pub success: bool,
    pub message: String,
    /// Whether a reboot is required to apply the update.
    pub reboot_required: bool,
}

/// Constraints on firmware updates from the surrounding system —
/// today, just whether Secure Boot is blocking the apply path.
/// Returned by `firmware.constraints` so the WebUI can disable the
/// per-row Apply button (with an explanatory tooltip) instead of
/// letting the operator click and surface the error in a toast.
///
/// SB-blocking exists because of upstream lanzaboote#591: the
/// EFI-capsule shim fwupd uses to apply updates can't be launched
/// from a lanzaboote-managed boot chain under enforcing Secure Boot.
/// Listing devices / refreshing metadata still works — only the
/// final `fwupdmgr update` step breaks.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FirmwareConstraints {
    /// True when the engine has detected Secure Boot is enforcing.
    /// The Apply UI surfaces a banner + per-row disable when set;
    /// `firmware.update` itself rejects the call as well.
    pub sb_blocks_apply: bool,
    /// Operator-facing reason string, suitable for surfacing
    /// verbatim in a tooltip or banner. Empty when
    /// `sb_blocks_apply` is false (the WebUI hides the banner).
    pub sb_blocks_apply_reason: String,
}

pub struct FirmwareService;

impl Default for FirmwareService {
    fn default() -> Self {
        Self::new()
    }
}

impl FirmwareService {
    pub fn new() -> Self {
        Self
    }

    /// Check if firmware management is available.
    /// Disabled on VMs (no real firmware) — detected via systemd-detect-virt.
    pub async fn is_available(&self) -> bool {
        let output = Command::new("systemd-detect-virt").output().await;
        match output {
            Ok(o) => {
                // Exit code 0 = virtualized, non-zero = bare metal (or container)
                // If running in a VM, firmware updates are not useful.
                !o.status.success()
            }
            Err(_) => true, // If detect-virt is missing, assume bare metal
        }
    }

    /// List all devices known to fwupd with their firmware versions.
    pub async fn list_devices(&self) -> Vec<FirmwareDevice> {
        let output = Command::new("fwupdmgr")
            .args(["get-devices", "--json"])
            .output()
            .await;

        let output = match output {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                warn!("fwupdmgr get-devices failed: {stderr}");
                return vec![];
            }
            Err(e) => {
                warn!("fwupdmgr not available: {e}");
                return vec![];
            }
        };

        let json: serde_json::Value = match serde_json::from_slice(&output) {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to parse fwupdmgr output: {e}");
                return vec![];
            }
        };

        let devices = json["Devices"].as_array().cloned().unwrap_or_default();

        devices
            .iter()
            .map(|d| FirmwareDevice {
                name: d["Name"].as_str().unwrap_or("Unknown").to_string(),
                device_id: d["DeviceId"].as_str().unwrap_or("").to_string(),
                version: d["Version"].as_str().unwrap_or("unknown").to_string(),
                vendor: d["Vendor"].as_str().unwrap_or("").to_string(),
                update_available: false,
                update_version: None,
                update_description: None,
            })
            .collect()
    }

    /// Check for available firmware updates.
    /// Returns the device list with update info populated.
    pub async fn check_updates(&self) -> Vec<FirmwareDevice> {
        // First refresh metadata from LVFS — best-effort. A failure here
        // means the device list won't have the latest firmware versions
        // available, which is logged at warn! by `try_run`.
        nasty_common::cmd::try_run("fwupdmgr", &["refresh", "--force"]).await;

        let mut devices = self.list_devices().await;

        // Get available updates
        let output = Command::new("fwupdmgr")
            .args(["get-updates", "--json"])
            .output()
            .await;

        let updates_json = match output {
            Ok(o) if o.status.success() => {
                serde_json::from_slice::<serde_json::Value>(&o.stdout).ok()
            }
            _ => None,
        };

        if let Some(json) = updates_json {
            let updates = json["Devices"].as_array().cloned().unwrap_or_default();

            for update in &updates {
                let device_id = update["DeviceId"].as_str().unwrap_or("");
                if let Some(dev) = devices.iter_mut().find(|d| d.device_id == device_id) {
                    dev.update_available = true;
                    // Releases array — take the first (latest)
                    if let Some(releases) = update["Releases"].as_array()
                        && let Some(release) = releases.first()
                    {
                        dev.update_version = release["Version"].as_str().map(|s| s.to_string());
                        dev.update_description = release["Summary"].as_str().map(|s| s.to_string());
                    }
                }
            }
        }

        devices
    }

    /// Apply a firmware update to a specific device.
    /// Snapshot of update-blocking constraints. Today only Secure
    /// Boot is in play (upstream lanzaboote#591 breaks fwupd's
    /// capsule-apply path); future constraints — pending reboot
    /// already queued, disk-space gates, etc. — would slot in
    /// here so the WebUI gets them in one call.
    pub async fn constraints(&self) -> FirmwareConstraints {
        let sb = nasty_common::secure_boot::status().await;
        if sb.enabled == Some(true) {
            FirmwareConstraints {
                sb_blocks_apply: true,
                sb_blocks_apply_reason:
                    "Firmware updates can't be applied while Secure Boot is enforcing — \
                     fwupd's EFI-capsule shim isn't compatible with lanzaboote's \
                     boot chain (upstream issue lanzaboote#591). Disable Secure \
                     Boot in firmware to apply updates, then re-enroll afterward."
                        .to_string(),
            }
        } else {
            FirmwareConstraints {
                sb_blocks_apply: false,
                sb_blocks_apply_reason: String::new(),
            }
        }
    }

    pub async fn update_device(&self, device_id: &str) -> FirmwareUpdateResult {
        // Belt-and-suspenders: the WebUI disables the Apply button when
        // SB blocks updates, but a direct RPC client (curl, scripted
        // automation) wouldn't see that gate. Refuse here too with the
        // same message so the failure mode is consistent and the
        // operator doesn't waste time chasing a fwupd error that's
        // actually upstream-broken.
        let constraints = self.constraints().await;
        if constraints.sb_blocks_apply {
            warn!(
                target: "nasty::firmware",
                "firmware.update refused for {device_id}: Secure Boot enforcing (lanzaboote#591)"
            );
            return FirmwareUpdateResult {
                device_name: device_id.to_string(),
                success: false,
                message: constraints.sb_blocks_apply_reason,
                reboot_required: false,
            };
        }

        info!("Applying firmware update to device {device_id}");

        let output = Command::new("fwupdmgr")
            .args(["update", device_id, "--no-reboot-check", "-y"])
            .output()
            .await;

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                let combined = format!("{stdout}\n{stderr}");
                let reboot = combined.contains("reboot") || combined.contains("restart");

                if o.status.success() {
                    info!("Firmware update applied to {device_id}");
                    FirmwareUpdateResult {
                        device_name: device_id.to_string(),
                        success: true,
                        message: stdout.trim().to_string(),
                        reboot_required: reboot,
                    }
                } else {
                    warn!("Firmware update failed for {device_id}: {stderr}");
                    FirmwareUpdateResult {
                        device_name: device_id.to_string(),
                        success: false,
                        message: combined.trim().to_string(),
                        reboot_required: false,
                    }
                }
            }
            Err(e) => FirmwareUpdateResult {
                device_name: device_id.to_string(),
                success: false,
                message: format!("fwupdmgr not available: {e}"),
                reboot_required: false,
            },
        }
    }
}
