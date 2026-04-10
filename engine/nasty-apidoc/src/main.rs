//! NASty API documentation generator.
//!
//! Run: `cargo run -p nasty-apidoc`
//! Output: `docs/api.md` at the repository root.

use schemars::{JsonSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

// ── Auth types (nasty-engine is a binary crate, so we mirror the types here) ──

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    ReadOnly,
    Operator,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Session {
    /// Session or API token value.
    pub token: String,
    /// Username of the authenticated user.
    pub username: String,
    /// Role assigned to this session.
    pub role: Role,
    /// If set, token can only see subvolumes in this filesystem.
    pub filesystem: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ApiToken {
    /// Unique token identifier.
    pub id: String,
    /// Human-readable token name.
    pub name: String,
    /// The actual token value — shown only once on creation.
    pub token: String,
    /// Role assigned to this token.
    pub role: Role,
    /// Unix timestamp (seconds) when the token was created.
    pub created_at: u64,
    /// Filesystem this token is scoped to, if any.
    pub filesystem: Option<String>,
    /// Unix timestamp after which the token is rejected. None = never expires.
    pub expires_at: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ApiTokenInfo {
    /// Unique token identifier.
    pub id: String,
    /// Human-readable token name.
    pub name: String,
    /// Role assigned to this token.
    pub role: Role,
    /// Unix timestamp (seconds) when the token was created.
    pub created_at: u64,
    /// Filesystem this token is scoped to, if any.
    pub filesystem: Option<String>,
    /// Unix timestamp after which the token is rejected. None = never expires.
    pub expires_at: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UserInfo {
    /// Login username.
    pub username: String,
    /// Role assigned to this user.
    pub role: Role,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CreateUserRequest {
    /// Login username for the new user.
    pub username: String,
    /// Initial password for the new user.
    pub password: String,
    /// Role to assign to the new user.
    pub role: Role,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ChangePasswordRequest {
    /// Username of the account to update.
    pub username: String,
    /// New password to set.
    pub new_password: String,
}

// ── Imports from other crates ─────────────────────────────────────

use nasty_sharing::iscsi::{AddAclRequest, AddLunRequest, CreateTargetRequest, IscsiTarget};
use nasty_sharing::nfs::{CreateNfsShareRequest, NfsShare, UpdateNfsShareRequest};
use nasty_sharing::nvmeof::{
    AddHostRequest, AddNamespaceRequest, AddPortRequest, CreateSubsystemRequest, NvmeofSubsystem,
};
use nasty_sharing::smb::{CreateSmbShareRequest, SmbShare, UpdateSmbShareRequest};
use nasty_storage::filesystem::{
    BlockDevice, CreateFilesystemRequest, DestroyFilesystemRequest, DeviceActionRequest,
    DeviceAddRequest, DeviceSetLabelRequest, DeviceSetStateRequest, Filesystem, FsUsage,
    ReconcileStatus, ScrubStatus, UpdateFilesystemOptionsRequest,
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
    UpdateInfo, UpdateStatus, VersionInfo, VersionSwitchRequest, VersionTaggedReleaseStatus,
};
use nasty_system::{DiskHealth, SystemHealth, SystemInfo, SystemStats};

// ── Method registry ───────────────────────────────────────────────

struct Method {
    name: &'static str,
    desc: &'static str,
    role: &'static str,
    /// Serialized JSON Schema for the params object, or a literal description.
    params: MethodParams,
    /// Serialized JSON Schema for the result type, or None.
    result: Option<Value>,
}

enum MethodParams {
    None,
    Literal(&'static str),
    Schema(Value),
}

fn gen_schema<T: JsonSchema>(generator: &mut SchemaGenerator) -> Value {
    let schema = generator.root_schema_for::<T>();
    serde_json::to_value(&schema).unwrap()
}

fn methods(generator: &mut SchemaGenerator) -> Vec<(&'static str, Vec<Method>)> {
    vec![
        (
            "Authentication",
            vec![
                Method {
                    name: "auth.me",
                    desc: "Return the current session's username and role.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Session>(generator)),
                },
                Method {
                    name: "auth.logout",
                    desc: "Invalidate the current session token.",
                    role: "any",
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "auth.change_password",
                    desc: "Change a user's password. Admins can change any user; users can change their own.",
                    role: "any",
                    params: MethodParams::Schema(gen_schema::<ChangePasswordRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "auth.create_user",
                    desc: "Create a new local user account.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<CreateUserRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "auth.delete_user",
                    desc: "Delete a user. Cannot delete your own account.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"username\": string}`"),
                    result: None,
                },
                Method {
                    name: "auth.list_users",
                    desc: "List all users (no password hashes).",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<UserInfo>>(generator)),
                },
                Method {
                    name: "auth.token.list",
                    desc: "List all API tokens (without token values).",
                    role: "admin",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<ApiTokenInfo>>(generator)),
                },
                Method {
                    name: "auth.token.create",
                    desc: "Create a long-lived API token. Returns the token value — shown only once.",
                    role: "admin",
                    params: MethodParams::Literal(
                        "`{\"name\": string, \"role\": Role, \"pool\": string?, \"expires_in_secs\": integer?}`",
                    ),
                    result: Some(gen_schema::<ApiToken>(generator)),
                },
                Method {
                    name: "auth.token.delete",
                    desc: "Delete an API token by ID.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"id\": string}`"),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<SystemInfo>(generator)),
                },
                Method {
                    name: "system.health",
                    desc: "Return health status of all systemd services.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<SystemHealth>(generator)),
                },
                Method {
                    name: "system.stats",
                    desc: "Return current CPU, memory, network interface, and disk I/O statistics.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<SystemStats>(generator)),
                },
                Method {
                    name: "system.disks",
                    desc: "Return S.M.A.R.T. health data for all drives. Requires SMART protocol to be enabled.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<DiskHealth>>(generator)),
                },
                Method {
                    name: "system.alerts",
                    desc: "Evaluate alert rules against current system state and return any active alerts.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<ActiveAlert>>(generator)),
                },
                Method {
                    name: "system.reboot",
                    desc: "Reboot the system.",
                    role: "admin",
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.shutdown",
                    desc: "Shut down the system.",
                    role: "admin",
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateInfo>(generator)),
                },
                Method {
                    name: "system.update.check",
                    desc: "Check for available updates against the upstream repository.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateInfo>(generator)),
                },
                Method {
                    name: "system.update.apply",
                    desc: "Fetch and apply the latest NixOS generation. Runs `nixos-rebuild switch` in the background.",
                    role: "admin",
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.update.rollback",
                    desc: "Roll back to the previous NixOS generation.",
                    role: "admin",
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.update.status",
                    desc: "Return the current update operation status and log.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<UpdateStatus>(generator)),
                },
                Method {
                    name: "system.version.get",
                    desc: "Return exact input URLs from `/etc/nixos/flake.nix` and locked revs from `/etc/nixos/flake.lock` for the Version page.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<VersionInfo>(generator)),
                },
                Method {
                    name: "system.version.tagged_release_notice",
                    desc: "Return the latest official tagged release and whether the current `nasty.url` already matches its standard `github:nasty-project/nasty/vX.Y.Z` form.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<VersionTaggedReleaseStatus>(generator)),
                },
                Method {
                    name: "system.version.upgrade_tagged_release",
                    desc: "Bootstrap a new wrapper `flake.nix` from the latest official tagged release template and start a switch rebuild.",
                    role: "admin",
                    params: MethodParams::None,
                    result: None,
                },
                Method {
                    name: "system.version.switch",
                    desc: "Update selected flake inputs on the installed system and rebuild only if `flake.lock` changed.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<VersionSwitchRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "system.version.cleanup",
                    desc: "Purge any stale legacy backup directory left by older Version-page builds.",
                    role: "admin",
                    params: MethodParams::None,
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Settings>(generator)),
                },
                Method {
                    name: "system.settings.update",
                    desc: "Update system settings. Only provided fields are changed.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<SettingsUpdate>(generator)),
                    result: Some(gen_schema::<Settings>(generator)),
                },
                Method {
                    name: "system.settings.timezones",
                    desc: "Return list of valid IANA timezone strings.",
                    role: "any",
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<NetworkConfig>(generator)),
                },
                Method {
                    name: "system.network.update",
                    desc: "Update network configuration (DHCP or static). Applied immediately without rebooting.",
                    role: "admin",
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<ProtocolStatus>>(generator)),
                },
                Method {
                    name: "service.protocol.enable",
                    desc: "Enable a protocol service. Available names: `nfs`, `smb`, `iscsi`, `nvmeof`, `ssh`, `avahi`, `smart`.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<ProtocolStatus>(generator)),
                },
                Method {
                    name: "service.protocol.disable",
                    desc: "Disable a protocol service.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"name\": string}`"),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<AlertRule>>(generator)),
                },
                Method {
                    name: "alert.rules.create",
                    desc: "Create a new alert rule.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AlertRule>(generator)),
                    result: Some(gen_schema::<AlertRule>(generator)),
                },
                Method {
                    name: "alert.rules.update",
                    desc: "Update an existing alert rule. Only provided fields are changed.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AlertRuleUpdate>(generator)),
                    result: Some(gen_schema::<AlertRule>(generator)),
                },
                Method {
                    name: "alert.rules.delete",
                    desc: "Delete an alert rule by ID.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"id\": string}`"),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<BlockDevice>>(generator)),
                },
                Method {
                    name: "device.wipe",
                    desc: "Erase all filesystem signatures from a device (wipefs). The device must not be in use.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"path\": string}`"),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<Filesystem>>(generator)),
                },
                Method {
                    name: "fs.get",
                    desc: "Get a single filesystem by name.",
                    role: "any",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.create",
                    desc: "Format and mount a new bcachefs filesystem.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<CreateFilesystemRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.destroy",
                    desc: "Unmount and unregister a filesystem. Does not wipe the devices.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DestroyFilesystemRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "fs.mount",
                    desc: "Mount a known filesystem.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.unmount",
                    desc: "Unmount a filesystem.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: None,
                },
                Method {
                    name: "fs.options.update",
                    desc: "Update runtime-mutable bcachefs filesystem options (written to sysfs).",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<UpdateFilesystemOptionsRequest>(
                        generator,
                    )),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.usage",
                    desc: "Return detailed bcachefs `fs usage` breakdown.",
                    role: "any",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<FsUsage>(generator)),
                },
                Method {
                    name: "fs.scrub.start",
                    desc: "Start a scrub on a mounted filesystem.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: None,
                },
                Method {
                    name: "fs.scrub.status",
                    desc: "Return current scrub status.",
                    role: "any",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<ScrubStatus>(generator)),
                },
                Method {
                    name: "fs.reconcile.status",
                    desc: "Return bcachefs background work (reconcile) status.",
                    role: "any",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<ReconcileStatus>(generator)),
                },
                Method {
                    name: "bcachefs.usage",
                    desc: "Return raw `bcachefs fs usage` output for a filesystem.",
                    role: "any",
                    params: MethodParams::Literal("`{\"name\": string}`"),
                    result: Some(gen_schema::<FsUsage>(generator)),
                },
            ],
        ),
        (
            "Filesystem Devices",
            vec![
                Method {
                    name: "fs.device.add",
                    desc: "Add a device to an existing mounted filesystem.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DeviceAddRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.remove",
                    desc: "Remove a device from a filesystem. The device should be fully evacuated first.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.evacuate",
                    desc: "Evacuate all data from a device to the remaining filesystem members. Long-running — returns `{\"status\": \"started\"}` immediately. Filesystem events are broadcast every 3s during evacuation.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "fs.device.set_state",
                    desc: "Set persistent device state (`rw`, `ro`, `failed`, `spare`).",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DeviceSetStateRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.set_label",
                    desc: "Set or update the hierarchical label on a device in a mounted filesystem. Written live via sysfs.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DeviceSetLabelRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.online",
                    desc: "Bring a device back online.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<DeviceActionRequest>(generator)),
                    result: Some(gen_schema::<Filesystem>(generator)),
                },
                Method {
                    name: "fs.device.offline",
                    desc: "Take a device offline.",
                    role: "admin",
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
                    role: "any",
                    params: MethodParams::Literal("`{\"pool\": string}`"),
                    result: Some(gen_schema::<Vec<Subvolume>>(generator)),
                },
                Method {
                    name: "subvolume.list_all",
                    desc: "List all subvolumes across all pools.",
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<Subvolume>>(generator)),
                },
                Method {
                    name: "subvolume.get",
                    desc: "Get a single subvolume.",
                    role: "any",
                    params: MethodParams::Literal("`{\"pool\": string, \"name\": string}`"),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.create",
                    desc: "Create a new bcachefs subvolume (filesystem or block-backed).",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<CreateSubvolumeRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.delete",
                    desc: "Delete a subvolume and all its snapshots.",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<DeleteSubvolumeRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "subvolume.attach",
                    desc: "Attach the loop device for a block subvolume (mounts `vol.img` via losetup).",
                    role: "operator",
                    params: MethodParams::Literal("`{\"pool\": string, \"name\": string}`"),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.detach",
                    desc: "Detach the loop device for a block subvolume.",
                    role: "operator",
                    params: MethodParams::Literal("`{\"pool\": string, \"name\": string}`"),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.resize",
                    desc: "Resize a block subvolume's backing image.",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<ResizeSubvolumeRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.set_properties",
                    desc: "Set arbitrary key-value metadata on a subvolume (stored as POSIX xattrs in the `user.*` namespace). Used by the CSI driver.",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<SetPropertiesRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.remove_properties",
                    desc: "Remove specific metadata keys from a subvolume.",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<RemovePropertiesRequest>(generator)),
                    result: Some(gen_schema::<Subvolume>(generator)),
                },
                Method {
                    name: "subvolume.find_by_property",
                    desc: "Find subvolumes matching a specific metadata key-value pair.",
                    role: "any",
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
                    role: "any",
                    params: MethodParams::Literal("`{\"pool\": string}`"),
                    result: Some(gen_schema::<Vec<Snapshot>>(generator)),
                },
                Method {
                    name: "snapshot.create",
                    desc: "Create a snapshot of a subvolume.",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<CreateSnapshotRequest>(generator)),
                    result: Some(gen_schema::<Snapshot>(generator)),
                },
                Method {
                    name: "snapshot.delete",
                    desc: "Delete a snapshot.",
                    role: "operator",
                    params: MethodParams::Schema(gen_schema::<DeleteSnapshotRequest>(generator)),
                    result: None,
                },
                Method {
                    name: "snapshot.clone",
                    desc: "Clone a snapshot into a new independent subvolume.",
                    role: "operator",
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<NfsShare>>(generator)),
                },
                Method {
                    name: "share.nfs.get",
                    desc: "Get an NFS share by ID.",
                    role: "any",
                    params: MethodParams::Literal("`{\"id\": string}`"),
                    result: Some(gen_schema::<NfsShare>(generator)),
                },
                Method {
                    name: "share.nfs.create",
                    desc: "Create an NFS share.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<CreateNfsShareRequest>(generator)),
                    result: Some(gen_schema::<NfsShare>(generator)),
                },
                Method {
                    name: "share.nfs.update",
                    desc: "Update an NFS share.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<UpdateNfsShareRequest>(generator)),
                    result: Some(gen_schema::<NfsShare>(generator)),
                },
                Method {
                    name: "share.nfs.delete",
                    desc: "Delete an NFS share.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"id\": string}`"),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<SmbShare>>(generator)),
                },
                Method {
                    name: "share.smb.get",
                    desc: "Get an SMB share by ID.",
                    role: "any",
                    params: MethodParams::Literal("`{\"id\": string}`"),
                    result: Some(gen_schema::<SmbShare>(generator)),
                },
                Method {
                    name: "share.smb.create",
                    desc: "Create an SMB share.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<CreateSmbShareRequest>(generator)),
                    result: Some(gen_schema::<SmbShare>(generator)),
                },
                Method {
                    name: "share.smb.update",
                    desc: "Update an SMB share.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<UpdateSmbShareRequest>(generator)),
                    result: Some(gen_schema::<SmbShare>(generator)),
                },
                Method {
                    name: "share.smb.delete",
                    desc: "Delete an SMB share.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"id\": string}`"),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<IscsiTarget>>(generator)),
                },
                Method {
                    name: "share.iscsi.get",
                    desc: "Get an iSCSI target by ID.",
                    role: "any",
                    params: MethodParams::Literal("`{\"id\": string}`"),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.create",
                    desc: "Create an iSCSI target. Optionally attach a LUN and ACLs in one call.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<CreateTargetRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.delete",
                    desc: "Delete an iSCSI target.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"id\": string}`"),
                    result: None,
                },
                Method {
                    name: "share.iscsi.add_lun",
                    desc: "Add a LUN to an iSCSI target.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AddLunRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.remove_lun",
                    desc: "Remove a LUN from an iSCSI target.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"target_id\": string, \"lun_id\": integer}`"),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.add_acl",
                    desc: "Allow an iSCSI initiator IQN to connect.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AddAclRequest>(generator)),
                    result: Some(gen_schema::<IscsiTarget>(generator)),
                },
                Method {
                    name: "share.iscsi.remove_acl",
                    desc: "Remove an iSCSI initiator ACL.",
                    role: "admin",
                    params: MethodParams::Literal(
                        "`{\"target_id\": string, \"initiator_iqn\": string}`",
                    ),
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
                    role: "any",
                    params: MethodParams::None,
                    result: Some(gen_schema::<Vec<NvmeofSubsystem>>(generator)),
                },
                Method {
                    name: "share.nvmeof.get",
                    desc: "Get an NVMe-oF subsystem by ID.",
                    role: "any",
                    params: MethodParams::Literal("`{\"id\": string}`"),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.create",
                    desc: "Create an NVMe-oF subsystem. Optionally attach a namespace, port, and host ACLs in one call.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<CreateSubsystemRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.delete",
                    desc: "Delete an NVMe-oF subsystem.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"id\": string}`"),
                    result: None,
                },
                Method {
                    name: "share.nvmeof.add_namespace",
                    desc: "Add a namespace (block device) to a subsystem.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AddNamespaceRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.remove_namespace",
                    desc: "Remove a namespace from a subsystem.",
                    role: "admin",
                    params: MethodParams::Literal(
                        "`{\"subsystem_id\": string, \"nsid\": integer}`",
                    ),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.add_port",
                    desc: "Add a transport port to a subsystem.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AddPortRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.remove_port",
                    desc: "Remove a transport port from a subsystem.",
                    role: "admin",
                    params: MethodParams::Literal(
                        "`{\"subsystem_id\": string, \"port_id\": integer}`",
                    ),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.add_host",
                    desc: "Allow a host NQN to connect to a subsystem.",
                    role: "admin",
                    params: MethodParams::Schema(gen_schema::<AddHostRequest>(generator)),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
                Method {
                    name: "share.nvmeof.remove_host",
                    desc: "Disallow a host NQN from a subsystem.",
                    role: "admin",
                    params: MethodParams::Literal("`{\"subsystem_id\": string, \"nqn\": string}`"),
                    result: Some(gen_schema::<NvmeofSubsystem>(generator)),
                },
            ],
        ),
    ]
}

// ── Markdown rendering ────────────────────────────────────────────

fn type_str_from_schema(schema: &Value) -> String {
    if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        return format!("`{}`", r.split('/').last().unwrap_or(r));
    }
    if let Some(items) = schema.get("items") {
        let inner = type_str_from_schema(items);
        return format!("{inner}[]");
    }
    if let Some(t) = schema.get("type").and_then(|v| v.as_str()) {
        return t.to_string();
    }
    if let Some(arr) = schema.get("type").and_then(|v| v.as_array()) {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|&s| s != "null")
            .collect();
        return parts.join(" | ");
    }
    if let Some(variants) = schema.get("oneOf").or_else(|| schema.get("anyOf")) {
        if let Some(arr) = variants.as_array() {
            let parts: Vec<String> = arr.iter().map(type_str_from_schema).collect();
            return parts.join(" \\| ");
        }
    }
    if let Some(vals) = schema.get("enum").and_then(|v| v.as_array()) {
        let parts: Vec<String> = vals
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| format!("`{s}`"))
            .collect();
        return parts.join(" \\| ");
    }
    "object".to_string()
}

fn render_properties(schema: &Value, defs: &Value) -> Option<String> {
    // Resolve $ref if needed
    let resolved;
    let schema = if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        let def_name = r.strip_prefix("#/$defs/").unwrap_or(r);
        resolved = defs.get(def_name).cloned().unwrap_or(Value::Null);
        &resolved
    } else {
        schema
    };

    let props = schema.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut rows = String::new();
    for (name, prop) in props {
        let is_req = required.contains(&name.as_str());
        let type_s = type_str_from_schema(prop);
        let desc = prop
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        rows.push_str(&format!(
            "| `{name}` | {type_s} | {} | {desc} |\n",
            if is_req { "yes" } else { "no" }
        ));
    }

    Some(format!(
        "| Field | Type | Required | Description |\n\
         |-------|------|:--------:|-------------|\n\
         {rows}"
    ))
}

fn render_result_summary(schema: &Value) -> String {
    // Top-level array
    if schema.get("type").and_then(|v| v.as_str()) == Some("array") {
        if let Some(items) = schema.get("items") {
            let t = type_str_from_schema(items);
            return format!("`{t}[]`");
        }
        return "`array`".into();
    }
    if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        return format!("`{}`", r.split('/').last().unwrap_or(r));
    }
    "`object`".into()
}

fn collect_defs(groups: &[(&str, Vec<Method>)]) -> BTreeMap<String, Value> {
    let mut all = BTreeMap::new();
    for (_, methods) in groups {
        for m in methods {
            for schema in [
                &m.params,
                &MethodParams::Schema(m.result.clone().unwrap_or(Value::Null)),
            ] {
                let v = match schema {
                    MethodParams::Schema(v) => v,
                    _ => continue,
                };
                if let Some(defs) = v.get("$defs").and_then(|d| d.as_object()) {
                    for (k, s) in defs {
                        all.entry(k.clone()).or_insert_with(|| s.clone());
                    }
                }
            }
        }
    }
    all
}

fn generate_markdown(groups: &[(&str, Vec<Method>)]) -> String {
    let defs_json = {
        let mut merged = serde_json::Map::new();
        let all = collect_defs(groups);
        for (k, v) in all {
            merged.insert(k, v);
        }
        Value::Object(merged)
    };

    let mut out = String::new();

    out.push_str("# NASty JSON-RPC API\n\n");
    out.push_str("NASty exposes a **JSON-RPC 2.0** API over **WebSocket** at `/ws`.\n\n");
    out.push_str("## Transport\n\n");
    out.push_str("Connect to `ws://<host>/ws` with a valid session cookie or `Authorization: Bearer <token>` header.\n\n");
    out.push_str("**Request:**\n```json\n{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"pool.list\", \"params\": {}}\n```\n\n");
    out.push_str(
        "**Response:**\n```json\n{\"jsonrpc\": \"2.0\", \"id\": 1, \"result\": [...]}\n```\n\n",
    );
    out.push_str("**Error:**\n```json\n{\"jsonrpc\": \"2.0\", \"id\": 1, \"error\": {\"code\": -32603, \"message\": \"filesystem not found: mypool\"}}\n```\n\n");
    out.push_str("## Authentication\n\n");
    out.push_str("Send `POST /api/login` with `{\"username\": \"...\", \"password\": \"...\"}` to receive a session token. ");
    out.push_str("Pass it as a cookie (`session=<token>`) or `Authorization: Bearer <token>` header on the WebSocket upgrade.\n\n");
    out.push_str("## Roles\n\n");
    out.push_str("| Role | Description |\n|------|-------------|\n");
    out.push_str("| `admin` | Full access to all methods |\n");
    out.push_str("| `operator` | Create/delete subvolumes and snapshots; read pools. Cannot manage users, destroy pools, or change system settings. |\n");
    out.push_str("| `readonly` | Read-only access to all list/get methods |\n\n");
    out.push_str(
        "API tokens can additionally be scoped to a single **filesystem** (restricts visibility) ",
    );
    out.push_str("and for operator tokens to a single **owner** (restricts to subvolumes owned by that token).\n\n");
    out.push_str("## Real-time Events\n\n");
    out.push_str(
        "After any successful mutation the server broadcasts an event on the same WebSocket:\n",
    );
    out.push_str("```json\n{\"event\": \"pool\"}\n```\n");
    out.push_str("Clients should re-fetch the relevant resource when they receive an event. ");
    out.push_str("Event types: `filesystem`, `subvolume`, `snapshot`, `share.nfs`, `share.smb`, `share.iscsi`, `share.nvmeof`, `protocol`, `settings`, `alert`.\n\n");
    out.push_str("---\n\n");

    // TOC
    out.push_str("## Contents\n\n");
    for (group, _) in groups {
        let anchor = group
            .to_lowercase()
            .replace([' ', '/'], "-")
            .replace(['&', '(', ')', '.'], "");
        out.push_str(&format!("- [{group}](#{anchor})\n"));
    }
    out.push('\n');

    // Methods
    for (group, methods) in groups {
        out.push_str(&format!("## {group}\n\n"));
        for m in methods {
            out.push_str(&format!("### `{}`\n\n", m.name));
            out.push_str(&format!("{}\n\n", m.desc));
            out.push_str(&format!("**Role:** `{}`\n\n", m.role));

            match &m.params {
                MethodParams::None => {}
                MethodParams::Literal(s) => {
                    out.push_str(&format!("**Params:** {s}\n\n"));
                }
                MethodParams::Schema(v) => {
                    out.push_str("**Params:**\n\n");
                    if let Some(table) = render_properties(v, &defs_json) {
                        out.push_str(&table);
                        out.push('\n');
                    } else {
                        out.push_str(&format!("{}\n\n", render_result_summary(v)));
                    }
                }
            }

            if let Some(result) = &m.result {
                out.push_str("**Returns:**\n\n");
                if let Some(table) = render_properties(result, &defs_json) {
                    out.push_str(&table);
                    out.push('\n');
                } else {
                    out.push_str(&format!("{}\n\n", render_result_summary(result)));
                }
            }

            out.push('\n');
        }
    }

    // Object definitions
    out.push_str("---\n\n## Object Definitions\n\n");
    let mut def_names: Vec<&String> = defs_json
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    def_names.sort();

    for name in def_names {
        let schema = &defs_json[name];
        out.push_str(&format!("### `{name}`\n\n"));

        // Enum
        if let Some(vals) = schema.get("enum").and_then(|v| v.as_array()) {
            let parts: Vec<String> = vals
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| format!("`{s}`"))
                .collect();
            out.push_str(&format!("Enum: {}\n\n", parts.join(", ")));
            continue;
        }
        // oneOf enum (schemars renders Rust enums as oneOf sometimes)
        if let Some(variants) = schema.get("oneOf").and_then(|v| v.as_array()) {
            let parts: Vec<String> = variants
                .iter()
                .filter_map(|v| {
                    v.get("enum")?
                        .as_array()?
                        .first()?
                        .as_str()
                        .map(|s| format!("`{s}`"))
                })
                .collect();
            if !parts.is_empty() {
                out.push_str(&format!("Enum: {}\n\n", parts.join(", ")));
                continue;
            }
        }

        if let Some(table) = render_properties(schema, &defs_json) {
            out.push_str(&table);
            out.push('\n');
        } else {
            out.push_str("*(see schema)*\n\n");
        }
    }

    out
}

fn main() {
    let mut generator = SchemaGenerator::default();
    let groups = methods(&mut generator);
    let md = generate_markdown(&groups);

    let out_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // engine/
        .and_then(|p| p.parent()) // nasty/
        .map(|p| p.join("docs/api.md"))
        .expect("could not resolve output path");

    std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
    std::fs::write(&out_path, &md).unwrap();

    println!("Written {} ({} bytes)", out_path.display(), md.len());
}
