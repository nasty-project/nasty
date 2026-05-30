//! Method registry data — metadata for every RPC method declared in this
//! crate's router. See `super` for type definitions and the public surface.

use schemars::{JsonSchema, SchemaGenerator};
use serde::Deserialize;

use super::{Method, MethodParams, MethodRole, ad_hoc_one, ad_hoc_two, gen_schema};
use crate::auth::{ApiToken, ApiTokenInfo, Role, Session, UserInfo};
use nasty_sharing::iscsi::{
    AddAclRequest, AddLunRequest, CreateTargetRequest, DeleteTargetRequest, IscsiTarget,
    RemoveAclRequest, RemoveLunRequest,
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
    CreateSmbShareRequest, DeleteSmbShareRequest, SmbShare, UpdateSmbShareRequest,
};
use nasty_storage::filesystem::{
    BlockDevice, CreateFilesystemRequest, DestroyFilesystemRequest, DeviceActionRequest,
    DeviceAddRequest, DeviceSetLabelRequest, DeviceSetStateRequest, Filesystem, FsUsage,
    ReconcileStatus, ScrubStatus, TpmBindStatus, UpdateFilesystemOptionsRequest,
};
use nasty_storage::subvolume::{
    CloneSnapshotRequest, CreateSnapshotRequest, CreateSubvolumeRequest, DeleteSnapshotRequest,
    DeleteSubvolumeRequest, FindByPropertyRequest, RemovePropertiesRequest, ResizeSubvolumeRequest,
    SetPropertiesRequest, Snapshot, Subvolume,
};
use nasty_system::alerts::{ActiveAlert, AlertRule, AlertRuleUpdate};
use nasty_system::network::NetworkConfig;
use nasty_system::protocol::ProtocolStatus;
use nasty_system::settings::{Settings, SettingsUpdate};
use nasty_system::update::{
    UpdateBuildDirConfig, UpdateInfo, UpdateStatus, VersionInfo, VersionSwitchRequest,
    VersionTaggedReleaseStatus,
};
use nasty_system::{DiskHealth, SystemHealth, SystemInfo, SystemStats};

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
                    params: MethodParams::AdHoc(ad_hoc_one("path", "Block device path (e.g. /dev/sdb).")),
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
                        "filesystem", "Filesystem name.",
                        "name", "Subvolume name.",
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
                        "filesystem", "Filesystem name.",
                        "name", "Subvolume name.",
                    )),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.detach",
                    desc: "Detach the loop device for a block subvolume.",
                    role: MethodRole::Operator,
                    // BUGFIX: docs said "pool" but the runtime parses "filesystem".
                    params: MethodParams::AdHoc(ad_hoc_two(
                        "filesystem", "Filesystem name.",
                        "name", "Subvolume name.",
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
    ]
}
