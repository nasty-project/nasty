//! Method registry data — metadata for every RPC method declared in this
//! crate's router. See `super` for type definitions and the public surface.

use schemars::{JsonSchema, SchemaGenerator};
use serde::Deserialize;
use serde_json::Value;

use super::{Method, MethodParams, MethodRole, ad_hoc_one, ad_hoc_two, gen_schema};
use crate::auth::{ApiToken, ApiTokenInfo, Role, Session, UserInfo};
use crate::fs_dependents::FsDependents;
use crate::subvolume_dependents::SubvolumeDependents;
use nasty_apps::{
    App, AppConfig, AppIngress, AppStats, AppsStatus, CaddyRouteSummary, CheckDevicesRequest,
    CheckPortsRequest, CheckVolumesRequest, DeviceMissing, EnableAppsRequest,
    FixVolumePermsRequest, ImageInspectResult, InstallAppRequest, InstallComposeRequest,
    ManagedNetwork, NetworkSummary, PortConflict, PruneResult, SetIngressRequest, VolumeMismatch,
};
use nasty_backup::{BackupProfile, BackupSnapshot, BackupStatus};
use nasty_sharing::iscsi::{
    AddAclRequest, AddLunRequest, AddPortalRequest, CreateTargetRequest, DeleteTargetRequest,
    IscsiTarget, RemoveAclRequest, RemoveLunRequest, RemovePortalRequest,
};
use nasty_sharing::nfs::{
    CreateNfsShareRequest, DeleteNfsShareRequest, NfsShare, UpdateNfsShareRequest,
};
use nasty_sharing::nvmeof::{
    AddHostRequest, AddNamespaceRequest, AddPortRequest, CreateSubsystemRequest,
    DeleteSubsystemRequest, NvmeofSubsystem, RemoveHostRequest, RemoveNamespaceRequest,
    RemovePortRequest,
};
use nasty_sharing::smb::{
    CreateSmbShareRequest, CreateSmbUserRequest, DeleteSmbShareRequest, SmbGroup, SmbShare,
    SmbUser, UpdateSmbShareRequest,
};
use nasty_storage::filesystem::{
    BlockDevice, CreateFilesystemRequest, DestroyFilesystemRequest, DeviceActionRequest,
    DeviceAddRequest, DeviceSetLabelRequest, DeviceSetStateRequest, Filesystem, FsUsage,
    FsckStatus, ReconcileStatus, ScrubStatus, TpmBindStatus, UpdateFilesystemOptionsRequest,
};
use nasty_storage::subvolume::{
    CloneSnapshotRequest, CloneSubvolumeRequest, CreateSnapshotRequest, CreateSubvolumeRequest,
    DeleteSnapshotRequest, DeleteSubvolumeRequest, FindByPropertyRequest, RemovePropertiesRequest,
    ResizeSubvolumeRequest, SetPropertiesRequest, Snapshot, Subvolume, UpdateSubvolumeRequest,
};
use nasty_system::alerts::{ActiveAlert, AlertRule, AlertRuleUpdate};
use nasty_system::firewall::FirewallStatus;
use nasty_system::firmware::{FirmwareConstraints, FirmwareDevice, FirmwareUpdateResult};
use nasty_system::hardware::{HardwareSummary, IommuGroup};
use nasty_system::network::nm::dbus::{NmApplyOutcome, NmDiff};
use nasty_system::network::{ConfirmRequest, NetworkConfig, NetworkPendingTxn};
use nasty_system::notifications::{ChannelType, NotificationConfig};
use nasty_system::nut::{NutConfig, NutConfigUpdate, UpsStatus};
use nasty_system::passthrough::{PassthroughConfig, PassthroughUpdate};
use nasty_system::protocol::ProtocolStatus;
use nasty_system::secure_boot::ReadinessReport;
use nasty_system::secure_boot_enrollment::{EnrollmentState, EnrollmentStatusResponse};
use nasty_system::settings::{AcmeStatus, HostTlsStatus, OidcSettings, Settings, SettingsUpdate};
use nasty_system::tailscale::{TailscaleConnectRequest, TailscaleStatus};
use nasty_system::tuning::{TuningConfig, TuningUpdate};
use nasty_system::update::{
    Generation, ReleaseChannel, UpdateBuildDirConfig, UpdateInfo, UpdateStatus, VersionInfo,
    VersionSwitchRequest, VersionTaggedReleaseStatus,
};
use nasty_system::{DiskHealth, SystemHealth, SystemInfo, SystemStats};
use nasty_vm::{
    CloneVmRequest, CreateVmRequest, SnapshotVmRequest, UpdateVmRequest, VmCapabilities, VmConfig,
    VmDiskSubvolume, VmStatus,
};

/// Mirror of the anonymous inline `struct P` in `router/auth.rs` that
/// `auth.create_user` parses. Defined here for `JsonSchema` derivation —
/// drift risk is low because the inline struct is one place and this is one
/// place, both tiny.
#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)] // Used via JsonSchema derivation, never deserialized in this crate.
pub struct CreateUserRequest {
    /// Login username for the new user.
    pub username: String,
    /// Initial password for the new user.
    pub password: String,
    /// Role to assign to the new user.
    pub role: Role,
}

/// Mirror of the anonymous inline `struct P` in `router/auth.rs` that
/// `auth.change_password` parses.
#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)] // Used via JsonSchema derivation, never deserialized in this crate.
pub struct ChangePasswordRequest {
    /// Username of the account to update.
    pub username: String,
    /// New password to set.
    pub new_password: String,
}

/// Mirror of the inline params struct for `auth.oidc.test`.
#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct OidcTestClaims {
    /// Sample IdP claims to feed through the role-mapping policy.
    pub claims: Value,
}

/// Mirror of the inline params struct for `auth.webauthn.register.start`.
#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct WebauthnRegisterStartRequest {
    /// Operator-facing label for the new credential ("YubiKey", "Phone", …).
    pub label: String,
}

pub(super) fn registry(generator: &mut SchemaGenerator) -> Vec<(&'static str, Vec<Method>)> {
    vec![
        (
            "Authentication",
            vec![
                Method {
                    name: "auth.me",
                    desc: "Return the current session's username and role.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Session>(generator)),
                },
                Method {
                    name: "auth.logout",
                    desc: "Invalidate the current session token.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "auth.change_password",
                    desc: "Change a user's password. Admins can change any user; users can change their own.",
                    role: MethodRole::Any,
                    params: MethodParams::Schema(gen_schema::<ChangePasswordRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "auth.create_user",
                    desc: "Create a new local user account.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<CreateUserRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "auth.delete_user",
                    desc: "Delete a user. Cannot delete your own account.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "username",
                        "Login username of the account to delete.",
                    )),
                    result: None,
                },
                Method {
                    name: "auth.list_users",
                    desc: "List all users (no password hashes).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<UserInfo>>(generator)),
                },
                Method {
                    name: "auth.token.list",
                    desc: "List all API tokens (without token values).",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<ApiTokenInfo>>(generator)),
                },
                Method {
                    name: "auth.token.create",
                    desc: "Create a long-lived API token. Returns the token value — shown only once.",
                    role: MethodRole::Admin,
                    // BUGFIX: docs said "pool" but the runtime parses
                    // "filesystem" (router/auth.rs auth.token.create struct).
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Human-readable token name."},
                            "role": {"$ref": "#/$defs/Role"},
                            "filesystem": {"type": "string", "description": "If set, the token can only see subvolumes in this filesystem."},
                            "expires_in_secs": {"type": "integer", "minimum": 0, "description": "Seconds until the token expires. Omit for a non-expiring token."}
                        },
                        "required": ["name", "role"]
                    })),
                    result: Some(gen_schema::<ApiToken>(generator)),
                },
                Method {
                    name: "auth.token.delete",
                    desc: "Delete an API token by ID.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Unique token identifier.")),
                    result: None,
                },
            ],
        ),
        (
            "System",
            vec![
                Method {
                    name: "system.info",
                    desc: "Return hostname, OS version, uptime, bcachefs-tools version info.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<SystemInfo>(generator)),
                },
                Method {
                    name: "system.health",
                    desc: "Return health status of all systemd services.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<SystemHealth>(generator)),
                },
                Method {
                    name: "system.stats",
                    desc: "Return current CPU, memory, network interface, and disk I/O statistics.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<SystemStats>(generator)),
                },
                Method {
                    name: "system.disks",
                    desc: "Return S.M.A.R.T. health data for all drives. Requires SMART protocol to be enabled.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<DiskHealth>>(generator)),
                },
                Method {
                    name: "system.alerts",
                    desc: "Evaluate alert rules against current system state and return any active alerts.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<ActiveAlert>>(generator)),
                },
                Method {
                    name: "system.reboot",
                    desc: "Reboot the system.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.shutdown",
                    desc: "Shut down the system.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
            ],
        ),
        (
            "System Update",
            vec![
                Method {
                    name: "system.update.version",
                    desc: "Return current version and latest available version.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateInfo>(generator)),
                },
                Method {
                    name: "system.update.check",
                    desc: "Check for available updates against the upstream repository.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateInfo>(generator)),
                },
                Method {
                    name: "system.update.apply",
                    desc: "Fetch and apply the latest NixOS generation. Runs `nixos-rebuild switch` in the background.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.update.rollback",
                    desc: "Roll back to the previous NixOS generation.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.update.status",
                    desc: "Return the current update operation status and log.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateStatus>(generator)),
                },
                Method {
                    name: "system.update.build_dir.get",
                    desc: "Return the configured Nix build-dir spillover path (if any) plus the live list of mounted bcachefs pools eligible to host the sandbox. Useful on small-rootfs installs where the default `/tmp` (tmpfs) doesn't have room for kernel-module / Rust compile sandboxes.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateBuildDirConfig>(generator)),
                },
                Method {
                    name: "system.update.build_dir.set",
                    desc: "Set or clear the Nix build-dir spillover path. Pass `{\"path\": \"/fs/<pool>\"}` to enable (must match one of the mounted bcachefs pools reported by `build_dir.get`) or `{\"path\": null}` to disable. When set, the engine runs upgrade scripts with `NIX_REMOTE=local` and `--option build-dir <pool>/.nasty-nix-build` so the sandbox spills onto bcachefs instead of tmpfs/root.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": ["string", "null"], "description": "Filesystem mount path to use as the Nix sandbox spillover, or null to disable."}
                        },
                        "required": ["path"]
                    })),
                    result: Some(gen_schema::<UpdateBuildDirConfig>(generator)),
                },
                Method {
                    name: "system.version.get",
                    desc: "Return exact input URLs from `/etc/nixos/flake.nix` and locked revs from `/etc/nixos/flake.lock` for the Version page.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<VersionInfo>(generator)),
                },
                Method {
                    name: "system.version.tagged_release_notice",
                    desc: "Return the latest official tagged release and whether the current `nasty.url` already matches its standard `github:nasty-project/nasty/vX.Y.Z` form.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<VersionTaggedReleaseStatus>(generator)),
                },
                Method {
                    name: "system.version.upgrade_tagged_release",
                    desc: "Bootstrap a new wrapper `flake.nix` from the latest official tagged release template and start a switch rebuild.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.version.switch",
                    desc: "Update selected flake inputs on the installed system and rebuild only if `flake.lock` changed.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<VersionSwitchRequest>(generator)),
                    result: None,
                },
            ],
        ),
        (
            "Settings",
            vec![
                Method {
                    name: "system.settings.get",
                    desc: "Return current system settings.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Settings>(generator)),
                },
                Method {
                    name: "system.settings.update",
                    desc: "Update system settings. Only provided fields are changed.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<SettingsUpdate>(generator)),
                    result: Some(gen_schema::<Settings>(generator)),
                },
                Method {
                    name: "system.settings.timezones",
                    desc: "Return list of valid IANA timezone strings.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<String>>(generator)),
                },
            ],
        ),
        (
            "Network",
            vec![
                Method {
                    name: "system.network.get",
                    desc: "Return current network configuration including live interface state.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<NetworkConfig>(generator)),
                },
                Method {
                    name: "system.network.update",
                    desc: "Update network configuration (DHCP or static). Applied immediately without rebooting.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<NetworkConfig>(generator)),
                    result: None,
                },
            ],
        ),
        (
            "Protocols & Services",
            vec![
                Method {
                    name: "service.protocol.list",
                    desc: "List all protocols and their current status.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<ProtocolStatus>>(generator)),
                },
                Method {
                    name: "service.protocol.enable",
                    desc: "Enable a protocol service. Available names: `nfs`, `smb`, `iscsi`, `nvmeof`, `ssh`, `avahi`, `smart`.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "name",
                        "Protocol name (nfs, smb, iscsi, nvmeof, ssh, avahi, smart).",
                    )),
                    result: Some(gen_schema::<ProtocolStatus>(generator)),
                },
                Method {
                    name: "service.protocol.disable",
                    desc: "Disable a protocol service.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "name",
                        "Protocol name (nfs, smb, iscsi, nvmeof, ssh, avahi, smart).",
                    )),
                    result: Some(gen_schema::<ProtocolStatus>(generator)),
                },
            ],
        ),
        (
            "Alert Rules",
            vec![
                Method {
                    name: "alert.rules.list",
                    desc: "List all alert rules.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<AlertRule>>(generator)),
                },
                Method {
                    name: "alert.rules.create",
                    desc: "Create a new alert rule.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AlertRule>(generator)),
                    result: Some(gen_schema::<AlertRule>(generator)),
                },
                Method {
                    name: "alert.rules.update",
                    desc: "Update an existing alert rule. Only provided fields are changed.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AlertRuleUpdate>(generator)),
                    result: Some(gen_schema::<AlertRule>(generator)),
                },
                Method {
                    name: "alert.rules.delete",
                    desc: "Delete an alert rule by ID.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Unique rule identifier.")),
                    result: None,
                },
            ],
        ),
        (
            "Block Devices",
            vec![
                Method {
                    name: "device.list",
                    desc: "List all block devices and partitions visible to the system.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<BlockDevice>>(generator)),
                },
                Method {
                    name: "device.wipe",
                    desc: "Erase all filesystem signatures from a device (wipefs). The device must not be in use.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "path",
                        "Block device path (e.g. /dev/sdb).",
                    )),
                    result: None,
                },
            ],
        ),
        (
            "Filesystems",
            vec![
                Method {
                    name: "fs.list",
                    desc: "List all filesystems. Filesystem-scoped tokens see only their assigned filesystem.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<Filesystem>>(generator)),
                },
                Method {
                    name: "fs.get",
                    desc: "Get a single filesystem by name.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.create",
                    desc: "Format and mount a new bcachefs filesystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<CreateFilesystemRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.destroy",
                    desc: "Unmount and unregister a filesystem. Does not wipe the devices.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DestroyFilesystemRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "fs.mount",
                    desc: "Mount a known filesystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.unmount",
                    desc: "Unmount a filesystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: None,
                },
                Method {
                    name: "fs.options.update",
                    desc: "Update runtime-mutable bcachefs filesystem options (written to sysfs).",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<UpdateFilesystemOptionsRequest>(
                        generator,
                    )),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.usage",
                    desc: "Return detailed bcachefs `fs usage` breakdown.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<FsUsage>(generator)),
                },
                Method {
                    name: "fs.scrub.start",
                    desc: "Start a scrub on a mounted filesystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: None,
                },
                Method {
                    name: "fs.scrub.status",
                    desc: "Return current scrub status.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<ScrubStatus>(generator)),
                },
                Method {
                    name: "fs.fsck.start",
                    desc: "Start an offline bcachefs fsck on an unmounted filesystem \
                           (dry run by default; set repair=true to auto-repair).",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: None,
                },
                Method {
                    name: "fs.fsck.status",
                    desc: "Return current fsck status.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<FsckStatus>(generator)),
                },
                Method {
                    name: "fs.reconcile.status",
                    desc: "Return bcachefs background work (reconcile) status.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<ReconcileStatus>(generator)),
                },
                Method {
                    name: "bcachefs.usage",
                    desc: "Return raw `bcachefs fs usage` output for a filesystem.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<FsUsage>(generator)),
                },
                Method {
                    name: "fs.tpm.status",
                    desc: "Report TPM2 host capability and per-filesystem bind state. `tpm_available` reflects whether `/dev/tpmrm0` is present; `bound` reflects whether a sealed-key blob exists for this filesystem.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<TpmBindStatus>(generator)),
                },
                Method {
                    name: "fs.tpm.bind",
                    desc: "Seal the filesystem's stored encryption key with the host TPM2 (PCR-7 bound). Writes the sealed blob next to the plaintext `.key`; the plaintext is retained as a recovery path until `fs.key.delete` is invoked. Errors when the host has no usable TPM2 or no stored `.key` exists.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<TpmBindStatus>(generator)),
                },
                Method {
                    name: "fs.tpm.unbind",
                    desc: "Remove the TPM2-sealed copy of the encryption key. The plaintext `.key` is unaffected. No-op success when no sealed blob exists.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<TpmBindStatus>(generator)),
                },
            ],
        ),
        (
            "Filesystem Devices",
            vec![
                Method {
                    name: "fs.device.add",
                    desc: "Add a device to an existing mounted filesystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceAddRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.remove",
                    desc: "Remove a device from a filesystem. The device should be fully evacuated first.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.evacuate",
                    desc: "Evacuate all data from a device to the remaining filesystem members. Long-running — returns `{\"status\": \"started\"}` immediately. Filesystem events are broadcast every 3s during evacuation.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "fs.device.set_state",
                    desc: "Set persistent device state (`rw`, `ro`, `failed`, `spare`).",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceSetStateRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.set_label",
                    desc: "Set or update the hierarchical label on a device in a mounted filesystem. Written live via sysfs.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceSetLabelRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.online",
                    desc: "Bring a device back online.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.offline",
                    desc: "Take a device offline.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
            ],
        ),
        (
            "Subvolumes",
            vec![
                Method {
                    name: "subvolume.list",
                    desc: "List subvolumes in a filesystem.",
                    role: MethodRole::Any,
                    // BUGFIX: docs said "pool" but the runtime parses
                    // "filesystem" (router/subvolume.rs require_str).
                    params: MethodParams::AdHoc(ad_hoc_one("filesystem", "Filesystem name.")),
                    result: Some(gen_schema::<Vec<Subvolume>>(generator)),
                },
                Method {
                    name: "subvolume.list_all",
                    desc: "List all subvolumes across all pools.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<Subvolume>>(generator)),
                },
                Method {
                    name: "subvolume.get",
                    desc: "Get a single subvolume.",
                    role: MethodRole::Any,
                    // BUGFIX: docs said "pool" but the runtime parses "filesystem".
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "filesystem",
                        "Filesystem name.",
                        "name",
                        "Subvolume name.",
                    )),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.create",
                    desc: "Create a new bcachefs subvolume (filesystem or block-backed).",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CreateSubvolumeRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.delete",
                    desc: "Delete a subvolume and all its snapshots.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<DeleteSubvolumeRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "subvolume.attach",
                    desc: "Attach the loop device for a block subvolume (mounts `vol.img` via losetup).",
                    role: MethodRole::Operator,
                    // BUGFIX: docs said "pool" but the runtime parses "filesystem".
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "filesystem",
                        "Filesystem name.",
                        "name",
                        "Subvolume name.",
                    )),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.detach",
                    desc: "Detach the loop device for a block subvolume.",
                    role: MethodRole::Operator,
                    // BUGFIX: docs said "pool" but the runtime parses "filesystem".
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "filesystem",
                        "Filesystem name.",
                        "name",
                        "Subvolume name.",
                    )),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.resize",
                    desc: "Resize a block subvolume's backing image.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<ResizeSubvolumeRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.set_properties",
                    desc: "Set arbitrary key-value metadata on a subvolume (stored as POSIX xattrs in the `user.*` namespace). Used by the CSI driver.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<SetPropertiesRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.remove_properties",
                    desc: "Remove specific metadata keys from a subvolume.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<RemovePropertiesRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.find_by_property",
                    desc: "Find subvolumes matching a specific metadata key-value pair.",
                    role: MethodRole::Any,
                    params: MethodParams::Schema(gen_schema::<FindByPropertyRequest>(generator)),
                    result: Some(gen_schema::<Vec<Subvolume>>(generator)),
                },
            ],
        ),
        (
            "Snapshots",
            vec![
                Method {
                    name: "snapshot.list",
                    desc: "List snapshots for all subvolumes in a filesystem.",
                    role: MethodRole::Any,
                    // BUGFIX: docs said "pool" but the runtime parses "filesystem".
                    params: MethodParams::AdHoc(ad_hoc_one("filesystem", "Filesystem name.")),
                    result: Some(gen_schema::<Vec<Snapshot>>(generator)),
                },
                Method {
                    name: "snapshot.create",
                    desc: "Create a snapshot of a subvolume.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CreateSnapshotRequest>(generator)),
                    result: Some(gen_schema::<Snapshot>(generator)),
                },
                Method {
                    name: "snapshot.delete",
                    desc: "Delete a snapshot.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<DeleteSnapshotRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "snapshot.clone",
                    desc: "Clone a snapshot into a new independent subvolume.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CloneSnapshotRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
            ],
        ),
        (
            "NFS Shares",
            vec![
                Method {
                    name: "share.nfs.list",
                    desc: "List all NFS shares.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<NfsShare>>(generator)),
                },
                Method {
                    name: "share.nfs.get",
                    desc: "Get an NFS share by ID.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Unique share identifier.")),
                    result: Some(gen_schema::<NfsShare>(generator)),
                },
                Method {
                    name: "share.nfs.create",
                    desc: "Create an NFS share.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<CreateNfsShareRequest>(generator)),
                    result: Some(gen_schema::<NfsShare>(generator)),
                },
                Method {
                    name: "share.nfs.update",
                    desc: "Update an NFS share.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<UpdateNfsShareRequest>(generator)),
                    result: Some(gen_schema::<NfsShare>(generator)),
                },
                Method {
                    name: "share.nfs.delete",
                    desc: "Delete an NFS share.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeleteNfsShareRequest>(generator)),
                    result: None,
                },
            ],
        ),
        (
            "SMB Shares",
            vec![
                Method {
                    name: "share.smb.list",
                    desc: "List all SMB shares.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<SmbShare>>(generator)),
                },
                Method {
                    name: "share.smb.get",
                    desc: "Get an SMB share by ID.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Unique share identifier.")),
                    result: Some(gen_schema::<SmbShare>(generator)),
                },
                Method {
                    name: "share.smb.create",
                    desc: "Create an SMB share.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<CreateSmbShareRequest>(generator)),
                    result: Some(gen_schema::<SmbShare>(generator)),
                },
                Method {
                    name: "share.smb.update",
                    desc: "Update an SMB share.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<UpdateSmbShareRequest>(generator)),
                    result: Some(gen_schema::<SmbShare>(generator)),
                },
                Method {
                    name: "share.smb.delete",
                    desc: "Delete an SMB share.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeleteSmbShareRequest>(generator)),
                    result: None,
                },
            ],
        ),
        (
            "iSCSI Targets",
            vec![
                Method {
                    name: "share.iscsi.list",
                    desc: "List all iSCSI targets.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<IscsiTarget>>(generator)),
                },
                Method {
                    name: "share.iscsi.get",
                    desc: "Get an iSCSI target by ID.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Unique share identifier.")),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.create",
                    desc: "Create an iSCSI target. Optionally attach a LUN and ACLs in one call.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<CreateTargetRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.delete",
                    desc: "Delete an iSCSI target.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeleteTargetRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "share.iscsi.add_lun",
                    desc: "Add a LUN to an iSCSI target.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AddLunRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.remove_lun",
                    desc: "Remove a LUN from an iSCSI target.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<RemoveLunRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.add_acl",
                    desc: "Allow an iSCSI initiator IQN to connect.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AddAclRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.remove_acl",
                    desc: "Remove an iSCSI initiator ACL.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<RemoveAclRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.add_portal",
                    desc: "Add a listening portal (IP:port) to an iSCSI target. Use 0.0.0.0 for all IPv4 interfaces, :: for all IPv6 interfaces, or a specific host address.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AddPortalRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.remove_portal",
                    desc: "Remove a listening portal from an iSCSI target. The last portal cannot be removed; add a replacement first.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<RemovePortalRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
            ],
        ),
        (
            "NVMe-oF Subsystems",
            vec![
                Method {
                    name: "share.nvmeof.list",
                    desc: "List all NVMe-oF subsystems.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<NvmeofSubsystem>>(generator)),
                },
                Method {
                    name: "share.nvmeof.get",
                    desc: "Get an NVMe-oF subsystem by ID.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Unique share identifier.")),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.create",
                    desc: "Create an NVMe-oF subsystem. Optionally attach a namespace, port, and host ACLs in one call.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<CreateSubsystemRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.delete",
                    desc: "Delete an NVMe-oF subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<DeleteSubsystemRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "share.nvmeof.add_namespace",
                    desc: "Add a namespace (block device) to a subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AddNamespaceRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.remove_namespace",
                    desc: "Remove a namespace from a subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<RemoveNamespaceRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.add_port",
                    desc: "Add a transport port to a subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AddPortRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.remove_port",
                    desc: "Remove a transport port from a subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<RemovePortRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.add_host",
                    desc: "Allow a host NQN to connect to a subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<AddHostRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.remove_host",
                    desc: "Disallow a host NQN from a subsystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<RemoveHostRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
            ],
        ),
        // ── Authentication adjuncts ────────────────────────────────────
        (
            "OIDC",
            vec![
                Method {
                    name: "auth.oidc.config_status",
                    desc: "Return the current OIDC settings with `client_secret` redacted to `<set>`/`<unset>` so the secret value never leaves the engine.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<OidcSettings>(generator)),
                },
                Method {
                    name: "auth.oidc.test",
                    desc: "Dry run that maps a sample claims object through the current OIDC role-mapping policy without contacting the IdP.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<OidcTestClaims>(generator)),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "role": {"type": ["string", "null"], "description": "Matched role name, or null when no mapping fires."}
                        },
                        "required": ["role"]
                    })),
                },
                Method {
                    name: "auth.oidc.update_config",
                    desc: "Update the OIDC settings (preserves the stored client_secret if the caller sends the `<unchanged>` placeholder), rebuild the live OIDC client, and audit-log the change.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<OidcSettings>(generator)),
                    result: Some(gen_schema::<OidcSettings>(generator)),
                },
            ],
        ),
        (
            "WebAuthn",
            vec![
                Method {
                    name: "auth.webauthn.config",
                    desc: "Return the engine-pinned WebAuthn RP ID so the WebUI can pre-check the operator's current origin before triggering credential creation.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "rp_id": {"type": "string", "description": "RP ID baked into this engine instance."}
                        },
                        "required": ["rp_id"]
                    })),
                },
                Method {
                    name: "auth.webauthn.list",
                    desc: "List the calling user's registered WebAuthn credentials (label, created_at, base64url credential_id) — no public-key material.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "created_at": {"type": "integer", "minimum": 0},
                                "credential_id": {"type": "string", "description": "Base64url credential ID, stable across listings."}
                            },
                            "required": ["label", "created_at", "credential_id"]
                        }
                    })),
                },
                Method {
                    name: "auth.webauthn.register.start",
                    desc: "Begin WebAuthn registration: issue a server-side challenge and return creation_options for `navigator.credentials.create` plus a registration_id to round-trip to `register.finish`; rejects when the user has no non-WebAuthn fallback factor.",
                    role: MethodRole::Any,
                    params: MethodParams::Schema(gen_schema::<WebauthnRegisterStartRequest>(
                        generator,
                    )),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "registration_id": {"type": "string", "description": "Opaque token to pass back to register.finish."},
                            "creation_options": {"type": "object", "description": "Browser-facing `PublicKeyCredentialCreationOptions`; pass straight to `navigator.credentials.create`."}
                        },
                        "required": ["registration_id", "creation_options"]
                    })),
                },
                Method {
                    name: "auth.webauthn.register.finish",
                    desc: "Complete WebAuthn registration: validate the browser's `navigator.credentials.create` response against the pending challenge and persist the new passkey under the caller's user record.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "registration_id": {"type": "string", "description": "Opaque token from register.start."},
                            "response": {"type": "object", "description": "Browser's `RegisterPublicKeyCredential` response object."}
                        },
                        "required": ["registration_id", "response"]
                    })),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "label": {"type": "string"},
                            "created_at": {"type": "integer", "minimum": 0},
                            "credential_id": {"type": "string"}
                        },
                        "required": ["label", "created_at", "credential_id"]
                    })),
                },
                Method {
                    name: "auth.webauthn.delete",
                    desc: "Delete one of the calling user's own registered WebAuthn credentials, identified by its base64url credential_id.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "credential_id",
                        "Base64url credential ID to remove.",
                    )),
                    result: None,
                },
                Method {
                    name: "auth.webauthn.reset_for_user",
                    desc: "Admin recovery: clear every WebAuthn credential registered to the target user (used when they've lost all authenticators); audit-logged.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "username",
                        "Login username whose credentials should be cleared.",
                    )),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "removed": {"type": "integer", "minimum": 0, "description": "Number of credentials cleared (0 is a successful no-op)."}
                        },
                        "required": ["removed"]
                    })),
                },
            ],
        ),
        // ── Audit log ────────────────────────────────────────────────────
        (
            "Audit",
            vec![Method {
                name: "audit.list",
                desc: "Return the most recent audit-log entries (default 200, capped by `limit`), parsed line-by-line in reverse chronological order. Entry shape depends on the action being audited.",
                role: MethodRole::Any,
                params: MethodParams::AdHoc(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 0, "default": 200, "description": "Maximum number of entries to return."}
                    }
                })),
                result: Some(serde_json::json!({
                    "type": "array",
                    "items": {"type": "object", "description": "Parsed JSON line from the audit log — schema varies by event."}
                })),
            }],
        ),
        // ── System (additions to the existing System group) ─────────────
        (
            "System (continued)",
            vec![Method {
                name: "system.reboot_required",
                desc: "Return true if the booted kernel/modules differ from the current system (a reboot is needed).",
                role: MethodRole::Any,
                params: MethodParams::None,
                result: Some(serde_json::json!({"type": "boolean"})),
            }],
        ),
        // ── System: NixOS generations ────────────────────────────────────
        (
            "System Generations",
            vec![
                Method {
                    name: "system.generations.list",
                    desc: "List all NixOS generations with metadata (date, kernel, NASty version, current/booted flags, user-assigned label).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<Generation>>(generator)),
                },
                Method {
                    name: "system.generations.label",
                    desc: "Set or clear a user-assigned label (e.g. \"known good\") on a NixOS generation.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "generation": {"type": "integer", "minimum": 0, "description": "Generation number to label."},
                            "label": {"type": ["string", "null"], "description": "New label, or null to clear."}
                        },
                        "required": ["generation"]
                    })),
                    result: None,
                },
                Method {
                    name: "system.generations.delete",
                    desc: "Delete a specific NixOS generation from the boot menu.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "generation": {"type": "integer", "minimum": 0, "description": "Generation number to delete."}
                        },
                        "required": ["generation"]
                    })),
                    result: None,
                },
                Method {
                    name: "system.generations.switch",
                    desc: "Switch the active system to a specific NixOS generation (rebuild-switch into it). Returns immediately while the switch runs in the background.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "generation": {"type": "integer", "minimum": 0, "description": "Generation number to switch to."}
                        },
                        "required": ["generation"]
                    })),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "status": {"type": "string", "enum": ["started"]}
                        },
                        "required": ["status"]
                    })),
                },
            ],
        ),
        // ── System: Hardware introspection ───────────────────────────────
        (
            "System Hardware",
            vec![
                Method {
                    name: "system.hardware.iommu",
                    desc: "Return IOMMU groups with their PCI device members (for passthrough planning).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<IommuGroup>>(generator)),
                },
                Method {
                    name: "system.hardware.summary",
                    desc: "Return a host hardware summary (DMI system/BIOS, CPU, memory DIMMs, USB devices, TPM, Secure Boot state).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<HardwareSummary>(generator)),
                },
            ],
        ),
        // ── System: Logs (journal + tracing filter) ──────────────────────
        (
            "System Logs",
            vec![
                Method {
                    name: "system.logs",
                    desc: "Return the tail of a systemd unit's journal.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "unit": {"type": "string", "default": "nasty-engine", "description": "Systemd unit to read."},
                            "lines": {"type": "integer", "minimum": 0, "default": 100, "description": "Number of lines from the tail."},
                            "grep": {"type": "string", "description": "Optional substring filter."}
                        }
                    })),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Raw `journalctl` stdout."}),
                    ),
                },
                Method {
                    name: "system.logs.units",
                    desc: "Return the list of well-known systemd units that exist on this host (for the log-viewer unit picker).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "array",
                        "items": {"type": "string"}
                    })),
                },
                Method {
                    name: "system.log.level",
                    desc: "Return the engine's currently-active tracing/log filter string.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Rendered `EnvFilter` directive."}),
                    ),
                },
                Method {
                    name: "system.log.set_level",
                    desc: "Hot-reload the engine's tracing log filter without restart.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "filter",
                        "New `EnvFilter` directive (e.g. `nasty_engine=trace,nasty_storage=debug`).",
                    )),
                    result: None,
                },
            ],
        ),
        // ── System: Metrics proxy ────────────────────────────────────────
        (
            "System Metrics",
            vec![
                Method {
                    name: "system.metrics.history",
                    desc: "Proxy historical metrics samples (CPU/net/disk/etc.) from the nasty-metrics sidecar.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "kind": {"type": "string", "description": "Resource kind (cpu, net, disk, ...)."},
                            "name": {"type": "string", "description": "Resource name (interface, device, ...)."},
                            "range": {"type": "string", "description": "Lookback window (e.g. `1h`, `24h`)."},
                            "offset": {"type": "integer", "description": "Seconds offset from now (negative = past)."}
                        }
                    })),
                    result: Some(serde_json::json!({
                        "type": "array",
                        "items": {"type": "object", "description": "Time-series window — see ResourceHistory for shape."}
                    })),
                },
                Method {
                    name: "system.metrics.prometheus",
                    desc: "Proxy the raw Prometheus-formatted scrape from the nasty-metrics sidecar.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Prometheus exposition text."}),
                    ),
                },
            ],
        ),
        // ── System: ACME (TLS automation) ────────────────────────────────
        (
            "System ACME",
            vec![
                Method {
                    name: "system.acme.status",
                    desc: "Return the current ACME certificate issuance status (state, message, domain, issuer, expiry).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<AcmeStatus>(generator)),
                },
                Method {
                    name: "system.acme.reset",
                    desc: "Reset the in-memory ACME certificate issuance status back to \"idle\".",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.acme.retry",
                    desc: "Re-apply Caddy's TLS automation policy from disk to retry stalled ACME issuance.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: None,
                },
            ],
        ),
        // ── System: TLS introspection ────────────────────────────────────
        (
            "System TLS",
            vec![
                Method {
                    name: "system.tls.host_statuses",
                    desc: "Return per-host TLS automation status for every hostname Caddy is managing (active/issuing/failed/pending with last log message).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<HostTlsStatus>>(generator)),
                },
                Method {
                    name: "system.tls.local_ca_root",
                    desc: "Return Caddy's internal-CA root certificate (PEM) so operators can import it into their trust store. Errors with a \"try again\" message when Caddy hasn't bootstrapped yet.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(
                        serde_json::json!({"type": "string", "description": "PEM-encoded root certificate."}),
                    ),
                },
            ],
        ),
        // ── System: NUT (UPS) ────────────────────────────────────────────
        (
            "System NUT (UPS)",
            vec![
                Method {
                    name: "system.nut.config.get",
                    desc: "Return the persisted NUT (UPS) configuration.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<NutConfig>(generator)),
                },
                Method {
                    name: "system.nut.config.update",
                    desc: "Apply a partial update to the NUT configuration and persist it.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<NutConfigUpdate>(generator)),
                    result: Some(gen_schema::<NutConfig>(generator)),
                },
                Method {
                    name: "system.nut.status",
                    desc: "Return the live UPS status (charge, runtime, voltage, load, model) as reported by `upsc`.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpsStatus>(generator)),
                },
            ],
        ),
        // ── System: PCI passthrough ──────────────────────────────────────
        (
            "System Passthrough",
            vec![
                Method {
                    name: "system.passthrough.get",
                    desc: "Return the persisted PCI vfio-pci passthrough configuration (vendor:device pairs claimed at boot).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<PassthroughConfig>(generator)),
                },
                Method {
                    name: "system.passthrough.update",
                    desc: "Validate, persist, and regenerate the passthrough Nix snippet from a new vendor:device list (reboot required to apply).",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<PassthroughUpdate>(generator)),
                    result: Some(gen_schema::<PassthroughConfig>(generator)),
                },
            ],
        ),
        // ── System: Secure Boot ──────────────────────────────────────────
        (
            "System Secure Boot",
            vec![
                Method {
                    name: "system.secure_boot.readiness",
                    desc: "Compute the Secure Boot readiness checklist (UEFI/TPM/ESP space/lanzaboote-in-flake/sbctl keys) for the Hardware page.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<ReadinessReport>(generator)),
                },
                Method {
                    name: "system.secure_boot.enrollment.status",
                    desc: "Return the combined persistent enrollment state plus the live `nasty-rebuild` unit snapshot.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<EnrollmentStatusResponse>(generator)),
                },
                Method {
                    name: "system.secure_boot.enrollment.begin",
                    desc: "Start the Secure Boot enrollment ceremony by writing the lanzaboote Nix overlay and locking inputs.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<EnrollmentState>(generator)),
                },
                Method {
                    name: "system.secure_boot.enrollment.rebuild",
                    desc: "Trigger `nasty-rebuild` via systemd-run to apply the enrollment overlay the wizard wrote.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "triggered": {"type": "boolean", "enum": [true]}
                        },
                        "required": ["triggered"]
                    })),
                },
                Method {
                    name: "system.secure_boot.enrollment.complete",
                    desc: "Mark the Secure Boot enrollment ceremony done (only valid from PostEnrollment phase).",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<EnrollmentState>(generator)),
                },
                Method {
                    name: "system.secure_boot.enrollment.abort",
                    desc: "Abort an in-progress Secure Boot enrollment by removing the lanzaboote overlay and lock entries.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "reason": {"type": "string", "description": "Optional operator-supplied reason recorded in audit log."}
                        }
                    })),
                    result: Some(gen_schema::<EnrollmentState>(generator)),
                },
            ],
        ),
        // ── System: SSH ──────────────────────────────────────────────────
        (
            "System SSH",
            vec![
                Method {
                    name: "system.ssh.status",
                    desc: "Return whether sshd password auth is enabled and the list of authorized SSH keys.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "password_auth": {"type": "boolean"},
                            "keys": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["password_auth", "keys"]
                    })),
                },
                Method {
                    name: "system.ssh.add_key",
                    desc: "Append an SSH public key to `/root/.ssh/authorized_keys`.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "key",
                        "Full public key line (must start with `ssh-` or `ecdsa-`).",
                    )),
                    result: None,
                },
                Method {
                    name: "system.ssh.remove_key",
                    desc: "Remove a matching SSH public key line from `/root/.ssh/authorized_keys`.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "key",
                        "Full public key line to remove.",
                    )),
                    result: None,
                },
                Method {
                    name: "system.ssh.set_password_auth",
                    desc: "Toggle sshd `PasswordAuthentication` via the engine-managed override file and reload sshd. Refuses to disable when no SSH keys are present.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "enabled": {"type": "boolean", "default": true, "description": "True to enable password auth, false to disable."}
                        }
                    })),
                    result: None,
                },
            ],
        ),
        // ── System: Tailscale ────────────────────────────────────────────
        (
            "System Tailscale",
            vec![
                Method {
                    name: "system.tailscale.get",
                    desc: "Return the persisted Tailscale config plus the live daemon/connection state (IP, hostname, version).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<TailscaleStatus>(generator)),
                },
                Method {
                    name: "system.tailscale.connect",
                    desc: "Start the Tailscale daemon and authenticate with the supplied auth key (falling back to the stored key when empty); also re-sync NVMe-oF ports for the new Tailscale IP.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<TailscaleConnectRequest>(generator)),
                    result: Some(gen_schema::<TailscaleStatus>(generator)),
                },
                Method {
                    name: "system.tailscale.disconnect",
                    desc: "Stop the Tailscale daemon, persist `enabled=false`, and clean up NVMe-oF ports on the 100.x Tailscale IP.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<TailscaleStatus>(generator)),
                },
            ],
        ),
        // ── System: Tuning ───────────────────────────────────────────────
        (
            "System Tuning",
            vec![
                Method {
                    name: "system.tuning.get",
                    desc: "Return the persisted system-wide NAS performance tuning configuration (NFS/SMB/iSCSI/VM-writeback knobs).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<TuningConfig>(generator)),
                },
                Method {
                    name: "system.tuning.update",
                    desc: "Apply a partial update to the tuning configuration, persist it, and reapply the affected sysctl/Samba/etc. settings.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<TuningUpdate>(generator)),
                    result: Some(gen_schema::<TuningConfig>(generator)),
                },
            ],
        ),
        // ── System: Firewall ─────────────────────────────────────────────
        (
            "System Firewall",
            vec![
                Method {
                    name: "system.firewall.status",
                    desc: "Return the current firewall rules and per-service source/interface restrictions.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<FirewallStatus>(generator)),
                },
                Method {
                    name: "system.firewall.restrict",
                    desc: "Set per-service source-IP/interface restrictions and rebuild the nftables rules.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "service": {"type": "string", "description": "Service name (nfs, smb, iscsi, …)."},
                            "sources": {"type": "array", "items": {"type": "string"}, "description": "Allowed source CIDRs. Omit to clear."},
                            "interfaces": {"type": "array", "items": {"type": "string"}, "description": "Allowed interfaces. Omit to clear."}
                        },
                        "required": ["service"]
                    })),
                    result: None,
                },
            ],
        ),
        // ── System Update (release channel) ──────────────────────────────
        (
            "System Update Channel",
            vec![
                Method {
                    name: "system.update.channel.get",
                    desc: "Return the currently-selected release channel (Mild/Spicy/Nasty).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<ReleaseChannel>(generator)),
                },
                Method {
                    name: "system.update.channel.set",
                    desc: "Persist the selected release channel.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "channel",
                        "Channel name (`mild`, `spicy`, or `nasty`).",
                    )),
                    result: Some(gen_schema::<ReleaseChannel>(generator)),
                },
            ],
        ),
        // ── Network (NetworkManager + pending-txn flow) ──────────────────
        (
            "Network (continued)",
            vec![
                Method {
                    name: "system.network.pending",
                    desc: "Return the list of network-update transactions still awaiting confirm-or-rollback. (Admin-only by current role-gate even though it's a read.)",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<NetworkPendingTxn>>(generator)),
                },
                Method {
                    name: "system.network.confirm",
                    desc: "Confirm a pending network-change rollback transaction so the new config sticks.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<ConfirmRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "system.network.nm_preview",
                    desc: "Compute the diff between desired NetworkManager profiles and NM's current state without applying.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<NmDiff>(generator)),
                },
                Method {
                    name: "system.network.nm_apply",
                    desc: "Apply the desired NetworkManager profile set (add/update/delete + activate) to the live system.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(gen_schema::<NmApplyOutcome>(generator)),
                },
            ],
        ),
        // ── Protocols & Services (continued) ─────────────────────────────
        (
            "Protocols & Services (continued)",
            vec![
                Method {
                    name: "service.base_names.get",
                    desc: "Return the configured iSCSI IQN prefix and NVMe-oF NQN prefix used as the base for newly created targets/subsystems (built-in defaults if no override file exists).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "iqn_prefix": {"type": "string"},
                            "nqn_prefix": {"type": "string"}
                        },
                        "required": ["iqn_prefix", "nqn_prefix"]
                    })),
                },
                Method {
                    name: "service.base_names.update",
                    desc: "Persist user-supplied iSCSI IQN and/or NVMe-oF NQN base name prefixes so future target/subsystem creations use them.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "iqn_prefix": {"type": "string"},
                            "nqn_prefix": {"type": "string"}
                        }
                    })),
                    result: None,
                },
                Method {
                    name: "service.rest_server.config",
                    desc: "Return the configured filesystem path used by the embedded `nasty-rest-server` (restic REST API) backup endpoint.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    })),
                },
                Method {
                    name: "service.rest_server.configure",
                    desc: "Set the rest-server storage path, auto-creating a subvolume under `/fs/<name>/...` if needed, persisting the path, and restarting `nasty-rest-server` to pick up the change.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "path",
                        "Absolute filesystem path for restic REST API storage.",
                    )),
                    result: None,
                },
                Method {
                    name: "service.rest_server.credentials",
                    desc: "Return the basic-auth username + password the rest-server requires. Source-side backup profiles need these in their target URL as `https://<user>:<password>@<host>:8000/`. Credentials are generated lazily on first call and persisted (password sealed via systemd-creds). Operators who lose track can re-read this RPC at any time.",
                    role: MethodRole::Admin,
                    params: MethodParams::None,
                    result: Some(
                        gen_schema::<nasty_system::rest_server::RestServerCredentials>(generator),
                    ),
                },
                Method {
                    name: "service.rest_server.rotate_credentials",
                    desc: "Generate a fresh random password (and optionally a new username), rewrite the htpasswd file, restart `nasty-rest-server` so it picks up the new file. Source-side backup profiles pointing at this rest-server need their URLs updated with the new credentials before the next run, or they'll fail with HTTP 401.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "username": {
                                "type": "string",
                                "description": "Optional new username. Omit or pass empty to keep the existing one."
                            }
                        }
                    })),
                    result: Some(
                        gen_schema::<nasty_system::rest_server::RestServerCredentials>(generator),
                    ),
                },
            ],
        ),
        // ── Telemetry ────────────────────────────────────────────────────
        (
            "Telemetry",
            vec![Method {
                name: "telemetry.send",
                desc: "Trigger an immediate anonymous telemetry report (drive/VM/app counts, version, arch) to the NASty telemetry endpoint. No-op when telemetry is disabled in settings.",
                role: MethodRole::Admin,
                params: MethodParams::None,
                result: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "sent": {"type": "boolean", "description": "True if the report was transmitted; false when telemetry is disabled."}
                    },
                    "required": ["sent"]
                })),
            }],
        ),
        // ── Notifications ────────────────────────────────────────────────
        (
            "Notifications",
            vec![
                Method {
                    name: "notifications.config.get",
                    desc: "Return the persisted notification-channels configuration (SMTP / Telegram / Webhook / ntfy / Signal).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<NotificationConfig>(generator)),
                },
                Method {
                    name: "notifications.config.update",
                    desc: "Replace the on-disk notifications config with the supplied one. File is chmod 0600 because it carries SMTP passwords and bot tokens.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<NotificationConfig>(generator)),
                    result: None,
                },
                Method {
                    name: "notifications.test",
                    desc: "Send a one-shot test message (\"NASty Test\") through the supplied channel configuration without persisting it.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<ChannelType>(generator)),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Human-readable confirmation."}),
                    ),
                },
                Method {
                    name: "notifications.test_saved",
                    desc: "Send a test message through an already-saved channel, identified by id. Sealed secrets are resolved server-side, so the secret never has to round-trip through the client.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(serde_json::json!({
                        "type": "object",
                        "required": ["id"],
                        "properties": {
                            "id": {"type": "string", "description": "Channel id from notifications.config.get."}
                        }
                    })),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Human-readable confirmation."}),
                    ),
                },
            ],
        ),
        // ── Firmware (fwupd) ─────────────────────────────────────────────
        (
            "Firmware",
            vec![
                Method {
                    name: "firmware.available",
                    desc: "Report whether firmware management is usable on this host (false inside VMs as detected by `systemd-detect-virt`, true on bare metal).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({"type": "boolean"})),
                },
                Method {
                    name: "firmware.devices",
                    desc: "List every device known to `fwupdmgr` with its name, vendor, device ID, and currently installed firmware version (no update check).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<FirmwareDevice>>(generator)),
                },
                Method {
                    name: "firmware.check",
                    desc: "Refresh LVFS metadata via `fwupdmgr refresh` then return the device list with `update_available`/`update_version`/`update_description` populated for devices with pending updates.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<FirmwareDevice>>(generator)),
                },
                Method {
                    name: "firmware.constraints",
                    desc: "Return a snapshot of system-level blockers on applying firmware updates (today: whether Secure Boot enforcement is preventing fwupd's capsule shim per lanzaboote#591).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<FirmwareConstraints>(generator)),
                },
                Method {
                    name: "firmware.update",
                    desc: "Apply the available firmware update for the named device via fwupd. Refuses the call if Secure Boot constraints block the capsule-apply path.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "device_id",
                        "fwupd device identifier (from firmware.devices).",
                    )),
                    result: Some(gen_schema::<FirmwareUpdateResult>(generator)),
                },
            ],
        ),
        // ── Filesystem additions (key/lock/reconcile/dependents) ─────────
        (
            "Filesystem Encryption",
            vec![
                Method {
                    name: "fs.lock",
                    desc: "Lock an encrypted filesystem by unmounting it (with cascading dependent teardown) and unlinking its key from the session keyring.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.unlock",
                    desc: "Unlock an encrypted filesystem by passing the supplied passphrase to `bcachefs unlock` against its first device, loading the key into the session keyring.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "name",
                        "Filesystem name.",
                        "passphrase",
                        "User-supplied unlock passphrase.",
                    )),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.key.export",
                    desc: "Read and return the stored encryption key file contents for the named encrypted filesystem.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Raw key file contents."}),
                    ),
                },
                Method {
                    name: "fs.key.delete",
                    desc: "Delete the on-disk stored encryption key for an encrypted filesystem, switching it to passphrase-only mode.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: None,
                },
            ],
        ),
        (
            "Filesystem Reconcile",
            vec![
                Method {
                    name: "fs.reconcile.enable",
                    desc: "Turn on bcachefs background reconcile work on a mounted filesystem by writing `1` to its sysfs `reconcile_enabled` knob.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: None,
                },
                Method {
                    name: "fs.reconcile.disable",
                    desc: "Turn off bcachefs background reconcile work on a mounted filesystem by writing `0` to its sysfs `reconcile_enabled` knob.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: None,
                },
            ],
        ),
        (
            "Filesystem Dependents",
            vec![
                Method {
                    name: "fs.dependents",
                    desc: "Return all downstream entities (subvolumes, apps, VMs, backup jobs, NFS/SMB/iSCSI/NVMe-oF shares) that reference a given filesystem, used to preview impact before destructive operations like lock.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(gen_schema::<FsDependents>(generator)),
                },
                Method {
                    name: "fs.locked_dependents",
                    desc: "Return the reverse-index of currently locked encrypted filesystems mapped to their app/VM dependents (for the WebUI's \"locked on FS\" badges).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<FsDependents>>(generator)),
                },
            ],
        ),
        // ── bcachefs tools (additions) ───────────────────────────────────
        (
            "bcachefs Tools",
            vec![
                Method {
                    name: "bcachefs.top",
                    desc: "Capture ~2 seconds of `bcachefs fs top` output for the named filesystem via a PTY, strip ANSI/header noise, and return the last complete frame as plain text.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Cleaned plain-text frame."}),
                    ),
                },
                Method {
                    name: "bcachefs.timestats",
                    desc: "Run `bcachefs fs timestats --json --once` against the named filesystem's mount point and return the parsed JSON (latency/duration histograms for bcachefs operations).",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Filesystem name.")),
                    result: Some(
                        serde_json::json!({"type": "object", "description": "Raw `bcachefs fs timestats --json` output — schema follows upstream bcachefs."}),
                    ),
                },
            ],
        ),
        // ── Subvolume additions ──────────────────────────────────────────
        (
            "Subvolumes (continued)",
            vec![
                Method {
                    name: "subvolume.children",
                    desc: "List nested child subvolume names found beneath the named parent subvolume on the given filesystem.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "filesystem",
                        "Filesystem name.",
                        "name",
                        "Parent subvolume name.",
                    )),
                    result: Some(serde_json::json!({
                        "type": "array",
                        "items": {"type": "string", "description": "Child subvolume name."}
                    })),
                },
                Method {
                    name: "subvolume.clone",
                    desc: "Create a writable COW clone of a subvolume by taking a non-read-only bcachefs snapshot under a new name (O(1), shares data blocks with the source).",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CloneSubvolumeRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.update",
                    desc: "Update mutable subvolume attributes (compression, comments, foreground/background/promote/metadata targets, data replicas) via bcachefs `set-file-option` and xattrs.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<UpdateSubvolumeRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.list_dependents",
                    desc: "Batched read returning the set of downstream entities (apps, VMs, backup jobs, shares of every protocol) attributed to each subvolume on the system, optionally filtered to the session's scoped filesystem.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<SubvolumeDependents>>(generator)),
                },
            ],
        ),
        // ── SMB users ────────────────────────────────────────────────────
        (
            "SMB Users",
            vec![
                Method {
                    name: "smb.user.list",
                    desc: "List SMB users by parsing `pdbedit -L` output and filtering to UIDs ≥ 3000.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<SmbUser>>(generator)),
                },
                Method {
                    name: "smb.user.create",
                    desc: "Create a Linux system user (no shell, no home, UID auto-assigned from 3000+) and set their Samba password. Requires the SMB protocol to be enabled.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CreateSmbUserRequest>(generator)),
                    result: Some(gen_schema::<SmbUser>(generator)),
                },
                Method {
                    name: "smb.user.delete",
                    desc: "Remove the user's Samba password entry and delete the Linux system account. Requires the SMB protocol to be enabled.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("username", "SMB username to delete.")),
                    result: None,
                },
                Method {
                    name: "smb.user.set_password",
                    desc: "Change an existing SMB user's Samba password. Requires the SMB protocol to be enabled.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "username",
                        "Existing SMB username.",
                        "password",
                        "New password.",
                    )),
                    result: None,
                },
            ],
        ),
        // ── SMB groups ───────────────────────────────────────────────────
        (
            "SMB Groups",
            vec![
                Method {
                    name: "smb.group.list",
                    desc: "List SMB-managed groups (GIDs in the 3000-3999 range) read from `/etc/group`, including members.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<SmbGroup>>(generator)),
                },
                Method {
                    name: "smb.group.create",
                    desc: "Create a Linux system group (GID auto-assigned from the SMB range, 3000+) used for SMB access control.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Group name to create.")),
                    result: Some(gen_schema::<SmbGroup>(generator)),
                },
                Method {
                    name: "smb.group.delete",
                    desc: "Delete the SMB-managed Linux group via `groupdel`.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Group name to delete.")),
                    result: None,
                },
                Method {
                    name: "smb.group.add_member",
                    desc: "Add an existing user to an existing SMB group via `usermod -aG`.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "group",
                        "Group name.",
                        "user",
                        "Username to add.",
                    )),
                    result: None,
                },
                Method {
                    name: "smb.group.remove_member",
                    desc: "Remove a user from an SMB group via `gpasswd -d`.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "group",
                        "Group name.",
                        "user",
                        "Username to remove.",
                    )),
                    result: None,
                },
            ],
        ),
        // ── Backup ───────────────────────────────────────────────────────
        (
            "Backup",
            vec![
                Method {
                    name: "backup.status",
                    desc: "Report whether any backup is currently running and which profile id it belongs to.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<BackupStatus>(generator)),
                },
                Method {
                    name: "backup.profile.list",
                    desc: "Return all configured backup profiles.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<BackupProfile>>(generator)),
                },
                Method {
                    name: "backup.profile.get",
                    desc: "Return a single backup profile by id.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Backup profile identifier.")),
                    result: Some(gen_schema::<BackupProfile>(generator)),
                },
                Method {
                    name: "backup.profile.create",
                    desc: "Create a new backup profile (name, sources, target, password, retention) and persist it to `/var/lib/nasty/backups.json`. Returns the stored profile with an auto-assigned 8-char UUID id when the caller leaves `id` empty.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<BackupProfile>(generator)),
                    result: Some(gen_schema::<BackupProfile>(generator)),
                },
                Method {
                    name: "backup.profile.update",
                    desc: "Replace the backup profile identified by `id` with the supplied profile body. The handler reads `id` from params *and* deserializes the same params object into BackupProfile — send the full BackupProfile shape with `id` populated.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<BackupProfile>(generator)),
                    result: Some(gen_schema::<BackupProfile>(generator)),
                },
                Method {
                    name: "backup.profile.delete",
                    desc: "Remove the backup profile with the given id from persisted state.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Backup profile identifier.")),
                    result: None,
                },
                Method {
                    name: "backup.run",
                    desc: "Spawn a background task that runs the profile's backup (auto-initializing the repo if needed, then pruning per the retention policy). Returns a BackupJob handle immediately; poll backup.jobs.get / backup.jobs.list to watch the Pending → Running → Succeeded|Failed transition. Returns an `AlreadyRunning` error if another job for the same profile is in flight.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Backup profile identifier.")),
                    result: Some(gen_schema::<nasty_backup::jobs::BackupJob>(generator)),
                },
                Method {
                    name: "backup.snapshots",
                    desc: "List all snapshots stored in the profile's repository (id, time, hostname, paths, tags).",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Backup profile identifier.")),
                    result: Some(gen_schema::<Vec<BackupSnapshot>>(generator)),
                },
                Method {
                    name: "backup.repo.init",
                    desc: "Initialize a fresh rustic repository at the profile's target using its password, then mark the profile as `repo_initialized`. Returns a BackupJob handle immediately; init can take 30+ seconds on remote REST / S3 targets so the actual work runs in the background and the caller polls backup.jobs.get for completion.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Backup profile identifier.")),
                    result: Some(gen_schema::<nasty_backup::jobs::BackupJob>(generator)),
                },
                Method {
                    name: "backup.repo.check",
                    desc: "Run a rustic repository integrity check (`repo.check`) on the profile's target repo. Returns a BackupJob handle immediately; check can take minutes on large repos and the caller polls backup.jobs.get for the result.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "Backup profile identifier.")),
                    result: Some(gen_schema::<nasty_backup::jobs::BackupJob>(generator)),
                },
                Method {
                    name: "backup.jobs.list",
                    desc: "List active and recently-finished backup jobs (init / run / check), newest first. Optional `profile_id` filter narrows the list to one profile. Terminal jobs are GC'd one hour after they finish, so this returns a bounded window rather than full history.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "profile_id": {
                                "type": "string",
                                "description": "Optional profile id filter; omit to list jobs across all profiles."
                            }
                        }
                    })),
                    result: Some(gen_schema::<Vec<nasty_backup::jobs::BackupJob>>(generator)),
                },
                Method {
                    name: "backup.jobs.get",
                    desc: "Return one backup job by id. 404-equivalent error when the id is unknown (job never existed or was GC'd after its retention window).",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "id",
                        "Backup job identifier (UUID returned by backup.repo.init / backup.run / backup.repo.check).",
                    )),
                    result: Some(gen_schema::<nasty_backup::jobs::BackupJob>(generator)),
                },
                Method {
                    name: "backup.secrets_status",
                    desc: "Report whether systemd-creds is available on this host and which backend (`tpm-and-host` / `host-only`) it would use to encrypt new backup secrets. Surfaced as the status pill on the Backups page so operators can tell at a glance whether their stored passwords / cloud keys are encrypted at rest.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<nasty_common::secrets::SecretsStatus>(
                        generator,
                    )),
                },
            ],
        ),
        // ── VMs ──────────────────────────────────────────────────────────
        (
            "VMs",
            vec![
                Method {
                    name: "vm.capabilities",
                    desc: "Report host VM capabilities (KVM availability, UEFI firmware availability, CPU arch, and PCI devices available for passthrough).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<VmCapabilities>(generator)),
                },
                Method {
                    name: "vm.list",
                    desc: "List all VMs with their current status (config plus running/pid/vnc_port).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<VmStatus>>(generator)),
                },
                Method {
                    name: "vm.get",
                    desc: "Return full status (config plus running/pid/vnc_port) for a single VM by id.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "VM identifier.")),
                    result: Some(gen_schema::<VmStatus>(generator)),
                },
                Method {
                    name: "vm.create",
                    desc: "Create a new VM config from the supplied spec (name, CPUs, memory, disks, networks, passthrough devices, boot options, etc.).",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CreateVmRequest>(generator)),
                    result: Some(gen_schema::<VmConfig>(generator)),
                },
                Method {
                    name: "vm.update",
                    desc: "Apply partial edits to an existing VM's config (name, CPUs, memory, disks, networks, passthrough, CD-ROMs, boot order, UEFI, autostart, etc.). Hardware changes require the VM to be stopped.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<UpdateVmRequest>(generator)),
                    result: Some(gen_schema::<VmConfig>(generator)),
                },
                Method {
                    name: "vm.delete",
                    desc: "Delete the VM config identified by `id` (does not remove backing disk subvolumes).",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "VM identifier.")),
                    result: None,
                },
                Method {
                    name: "vm.start",
                    desc: "Launch QEMU for the VM identified by `id` and return its updated status.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "VM identifier.")),
                    result: Some(gen_schema::<VmStatus>(generator)),
                },
                Method {
                    name: "vm.stop",
                    desc: "Gracefully stop the running VM identified by `id` (ACPI shutdown via QMP).",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "VM identifier.")),
                    result: None,
                },
                Method {
                    name: "vm.kill",
                    desc: "Force-terminate the QEMU process for a running VM by `id` (ungraceful — used when `vm.stop` won't return).",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("id", "VM identifier.")),
                    result: None,
                },
                Method {
                    name: "vm.snapshot",
                    desc: "Snapshot every block subvolume backing a VM under a shared name, freezing the guest filesystem via QMP `guest-fsfreeze-freeze` first if the VM is running.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<SnapshotVmRequest>(generator)),
                    result: Some(gen_schema::<Vec<VmDiskSubvolume>>(generator)),
                },
                Method {
                    name: "vm.clone",
                    desc: "Clone a stopped VM by COW-cloning each of its disk subvolumes and creating a new VM config that points at those clones.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<CloneVmRequest>(generator)),
                    result: Some(gen_schema::<VmConfig>(generator)),
                },
            ],
        ),
        // ── VM disk images ───────────────────────────────────────────────
        (
            "VM Disk Images",
            vec![
                Method {
                    name: "vm.images.list",
                    desc: "List VM image files found under `vms/images` across all mounted filesystems, with per-image name/path/filesystem/size/format/compression, plus a flag indicating whether any such directory exists.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "subvolume_exists": {"type": "boolean", "description": "True if at least one `vms/images` directory was found."},
                            "images": {
                                "type": "array",
                                "items": {"type": "object", "description": "Image entry — name, path, filesystem, size, format, compression."}
                            }
                        },
                        "required": ["subvolume_exists", "images"]
                    })),
                },
                Method {
                    name: "vm.images.ensure",
                    desc: "Ensure the `vms/images` directory exists on the named filesystem (creating it, and migrating from legacy `.nasty/images` if present); return the absolute images directory path.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("filesystem", "Filesystem name.")),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Absolute path to the images directory."}),
                    ),
                },
                Method {
                    name: "vm.images.import_info",
                    desc: "Pre-flight inspection for the disk-import WebSocket — return format, virtual size, actual size, and (if applicable) compression for a VM image file under `vms/images` on the named filesystem.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "filesystem",
                        "Filesystem name.",
                        "name",
                        "Image filename.",
                    )),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "format": {"type": "string", "description": "Image format (qcow2, raw, …)."},
                            "virtual_size": {"type": "integer", "minimum": 0},
                            "actual_size": {"type": "integer", "minimum": 0},
                            "compression": {"type": ["string", "null"], "description": "Compression algorithm if any."}
                        },
                        "required": ["format", "virtual_size", "actual_size"]
                    })),
                },
            ],
        ),
        // ── Apps (Docker-managed application platform) ───────────────────
        (
            "Apps",
            vec![
                Method {
                    name: "apps.status",
                    desc: "Return runtime status of the apps subsystem (enabled flag, Docker running, app count, memory usage, storage path/health, Docker version, total disk usage).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<AppsStatus>(generator)),
                },
                Method {
                    name: "apps.list",
                    desc: "List every NASty-managed app (both simple and compose), returning each one's high-level App record.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<App>>(generator)),
                },
                Method {
                    name: "apps.get",
                    desc: "Return the high-level App record (name, image, status, kind, containers, ports, unsafe_mode, proxy_disabled_reason) for a single named app.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: Some(gen_schema::<App>(generator)),
                },
                Method {
                    name: "apps.config",
                    desc: "Return the deployed configuration of a named simple app (image, ports, env, volumes, resource limits, allow_unsafe), with env entries tagged where they match the image's own defaults so the WebUI Edit form can grey them out.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: Some(gen_schema::<AppConfig>(generator)),
                },
                Method {
                    name: "apps.stats",
                    desc: "Return live CPU / memory / network / block-IO stats for every NASty-managed app, summed across containers for compose apps.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<AppStats>>(generator)),
                },
                Method {
                    name: "apps.logs",
                    desc: "Return Docker logs for a named simple app's container (the `nasty-<name>` container), defaulting to the last 100 lines unless `tail` overrides it.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "App name."},
                            "tail": {"type": "integer", "minimum": 0, "default": 100, "description": "Number of log lines from the tail."}
                        },
                        "required": ["name"]
                    })),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Concatenated log lines."}),
                    ),
                },
                Method {
                    name: "apps.container.logs",
                    desc: "Return Docker logs for an arbitrary container by ID or name (no `nasty-` prefix assumed), defaulting to the last 100 lines unless `tail` overrides it.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "container_id": {"type": "string", "description": "Container ID or name."},
                            "tail": {"type": "integer", "minimum": 0, "default": 100}
                        },
                        "required": ["container_id"]
                    })),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Concatenated log lines."}),
                    ),
                },
                Method {
                    name: "apps.inspect",
                    desc: "Return the raw Docker `inspect` JSON for a named simple app's container as an untyped object.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: Some(
                        serde_json::json!({"type": "object", "description": "Raw Docker `inspect` payload — shape follows the Docker API."}),
                    ),
                },
                Method {
                    name: "apps.inspect_image",
                    desc: "Inspect a container image (registry/local) and return its declared ports, VOLUME bind paths, runtime user, and any known sub-path recipe — used by the install wizard to prefill the form.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one(
                        "image",
                        "Image reference (`repo:tag`).",
                    )),
                    result: Some(gen_schema::<ImageInspectResult>(generator)),
                },
                Method {
                    name: "apps.caddy.routes",
                    desc: "Return every route Caddy is currently serving (engine-owned and static), enriched with on-disk TLS cert info for host-match rows — powers the Ingress overview page.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<CaddyRouteSummary>>(generator)),
                },
                Method {
                    name: "apps.check_ports",
                    desc: "Check a list of host ports for conflicts against other managed apps and system listeners, returning each conflicting port and what is using it.",
                    role: MethodRole::Any,
                    params: MethodParams::Schema(gen_schema::<CheckPortsRequest>(generator)),
                    result: Some(gen_schema::<Vec<PortConflict>>(generator)),
                },
                Method {
                    name: "apps.check_devices",
                    desc: "Stat a list of host device paths and report which are missing, including whether the parent directory exists to distinguish \"device absent\" from \"kernel module not loaded\".",
                    role: MethodRole::Any,
                    params: MethodParams::Schema(gen_schema::<CheckDevicesRequest>(generator)),
                    result: Some(gen_schema::<Vec<DeviceMissing>>(generator)),
                },
                Method {
                    name: "apps.check_volumes",
                    desc: "Parse a docker-compose YAML and report bind-mount source paths whose host owner does not match the container's runtime user (or whose source path is missing).",
                    role: MethodRole::Any,
                    params: MethodParams::Schema(gen_schema::<CheckVolumesRequest>(generator)),
                    result: Some(gen_schema::<Vec<VolumeMismatch>>(generator)),
                },
                Method {
                    name: "apps.enable",
                    desc: "Enable the apps runtime on this box (optionally pinning the storage filesystem) and start Docker.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<EnableAppsRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "apps.disable",
                    desc: "Disable the apps runtime on this box (stop Docker and clear the persisted enabled flag in AppsConfig).",
                    role: MethodRole::Operator,
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "apps.install",
                    desc: "Deploy a new simple (single-container) app: validate bind mounts, pull the image, create volume dirs with the image's runtime uid/gid, start the container, and optionally wire up subdomain or path-prefix ingress.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<InstallAppRequest>(generator)),
                    result: Some(gen_schema::<App>(generator)),
                },
                Method {
                    name: "apps.update",
                    desc: "Update a previously installed simple app in place by re-running the install pipeline against the new InstallAppRequest (same shape as install).",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<InstallAppRequest>(generator)),
                    result: Some(gen_schema::<App>(generator)),
                },
                Method {
                    name: "apps.remove",
                    desc: "Remove a named simple app (stop + delete the container, clean up volume dirs, remove the Caddy ingress) and asynchronously reapply TLS so Caddy stops renewing any orphaned subdomain cert.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: None,
                },
                Method {
                    name: "apps.start",
                    desc: "Start a previously stopped named app (simple container or compose project).",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: None,
                },
                Method {
                    name: "apps.stop",
                    desc: "Stop a named app (simple container or compose project) without removing it.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: None,
                },
                Method {
                    name: "apps.restart",
                    desc: "Restart a named app (simple container or compose project).",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: None,
                },
                Method {
                    name: "apps.pull",
                    desc: "Pull the latest image(s) for a named app and recreate the container(s) — for simple apps it stops/removes/reinstalls preserving config and subdomain mode; for compose apps it runs `docker compose pull` then `up -d`.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: Some(gen_schema::<App>(generator)),
                },
                Method {
                    name: "apps.prune",
                    desc: "Prune unused Docker images and volumes, returning the number of images removed and the bytes reclaimed.",
                    role: MethodRole::Operator,
                    params: MethodParams::None,
                    result: Some(gen_schema::<PruneResult>(generator)),
                },
                Method {
                    name: "apps.exec_command",
                    desc: "Return the `docker exec -it <container> <shell>` command string for opening an interactive shell into a named app, probing /bin/bash, /bin/sh, /bin/ash to find an available shell.",
                    role: MethodRole::Admin,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Full `docker exec` command line."}),
                    ),
                },
                Method {
                    name: "apps.fix_volume_perms",
                    desc: "Chown a host bind-mount source path to the given uid/gid (optionally recursively), enforcing the same forbidden-bind validation as compose deploys.",
                    role: MethodRole::Admin,
                    params: MethodParams::Schema(gen_schema::<FixVolumePermsRequest>(generator)),
                    result: Some(serde_json::json!({
                        "type": "object",
                        "properties": {"ok": {"type": "boolean", "enum": [true]}},
                        "required": ["ok"]
                    })),
                },
                Method {
                    name: "apps.compose.get",
                    desc: "Return the raw docker-compose.yml file contents for a named compose-based app.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Compose app name.")),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Raw docker-compose.yml body."}),
                    ),
                },
                Method {
                    name: "apps.compose.logs",
                    desc: "Return aggregated `docker compose logs` output for a named compose app, defaulting to the last 100 lines unless `tail` overrides it.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "tail": {"type": "integer", "minimum": 0, "default": 100}
                        },
                        "required": ["name"]
                    })),
                    result: Some(serde_json::json!({"type": "string"})),
                },
                Method {
                    name: "apps.compose.install",
                    desc: "Deploy a new compose-based app by writing its docker-compose.yml to disk, pre-creating bind-mount dirs with the right ownership, running `docker compose up -d`, and auto-creating an ingress for the first exposed TCP port.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<InstallComposeRequest>(generator)),
                    result: Some(gen_schema::<App>(generator)),
                },
                Method {
                    name: "apps.compose.update",
                    desc: "Overwrite a compose app's docker-compose.yml, pre-create any newly added bind-mount sources, and run `docker compose up -d --no-build --pull missing --remove-orphans` to apply the new config.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<InstallComposeRequest>(generator)),
                    result: Some(gen_schema::<App>(generator)),
                },
                Method {
                    name: "apps.compose.remove",
                    desc: "Tear down a compose app via `docker compose down -v --remove-orphans`, delete its project directory, and remove its Caddy ingress.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Compose app name.")),
                    result: None,
                },
                Method {
                    name: "apps.ingress.list",
                    desc: "List all per-app reverse-proxy ingresses currently registered in Caddy (name, host_port, path, optional subdomain).",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<AppIngress>>(generator)),
                },
                Method {
                    name: "apps.ingress.set",
                    desc: "Set (or replace) an app's Caddy reverse-proxy ingress, gated on a subdomain-conflict check, persisting subdomain mode and asynchronously reapplying TLS automation so a new subdomain gets a cert immediately.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<SetIngressRequest>(generator)),
                    result: Some(gen_schema::<AppIngress>(generator)),
                },
                Method {
                    name: "apps.ingress.remove",
                    desc: "Remove an app's Caddy ingress route, clear the persisted subdomain choice, and asynchronously reapply TLS automation so Caddy stops trying to renew the orphaned cert.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "App name.")),
                    result: None,
                },
                Method {
                    name: "apps.ingress.check_conflict",
                    desc: "Best-effort read-only lookup that returns a human-readable \"in use by X\" reason if the proposed subdomain conflicts with another app or the WebUI hostname, or an empty string when the choice is clear.",
                    role: MethodRole::Any,
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "name",
                        "App name (for self-exclusion).",
                        "subdomain",
                        "Proposed subdomain.",
                    )),
                    result: Some(
                        serde_json::json!({"type": "string", "description": "Conflict reason, or empty when no conflict."}),
                    ),
                },
                Method {
                    name: "apps.networks.list",
                    desc: "List Docker networks NASty can manage — merges live Docker state with persisted managed-network specs and annotates each with exists/managed/attached_apps.",
                    role: MethodRole::Any,
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<NetworkSummary>>(generator)),
                },
                Method {
                    name: "apps.networks.create",
                    desc: "Create a NASty-managed Docker network (bridge/macvlan/ipvlan) on a validated host parent interface and persist the spec for boot reconcile.",
                    role: MethodRole::Operator,
                    params: MethodParams::Schema(gen_schema::<ManagedNetwork>(generator)),
                    result: None,
                },
                Method {
                    name: "apps.networks.remove",
                    desc: "Remove a NASty-managed Docker network. Refuses while any container is still attached.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(ad_hoc_one("name", "Network name.")),
                    result: None,
                },
            ],
        ),
    ]
}
