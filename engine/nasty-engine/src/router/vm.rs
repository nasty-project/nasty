//! RPC arms in the `vm.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use std::path::{Path, PathBuf};

use nasty_common::{ErrorCode, Request, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

#[derive(Debug)]
struct ManagedVmDisk {
    device: Option<String>,
    source: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CreateVmDiskRequest {
    pub filesystem: String,
    pub name: String,
    pub volsize_bytes: u64,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct VmDiskCandidate {
    pub subvolume: nasty_storage::subvolume::Subvolume,
    pub consumers: Vec<String>,
}

fn csi_consumer(properties: &std::collections::HashMap<String, String>) -> Option<String> {
    if !properties.keys().any(|key| key.starts_with("nasty-csi:")) {
        return None;
    }
    let namespace = properties
        .get("nasty-csi:pvc_namespace")
        .map(String::as_str)
        .unwrap_or("?");
    let name = properties
        .get("nasty-csi:pvc_name")
        .map(String::as_str)
        .unwrap_or("?");
    Some(format!(
        "Kubernetes CSI volume (PVC {namespace}/{name}); reserved even when not mounted"
    ))
}

pub(super) fn disk_matches_subvolume(
    disk: &nasty_vm::VmDisk,
    sv: &nasty_storage::subvolume::Subvolume,
) -> bool {
    let source = Path::new(&sv.path).join("vol.img");
    if let Some(saved_source) = disk.source.as_deref() {
        return paths_match(saved_source, &source.to_string_lossy());
    }
    paths_match(&disk.path, &source.to_string_lossy())
        || sv
            .block_device
            .as_deref()
            .is_some_and(|p| paths_match(p, &disk.path))
}

fn disks_share_backing(left: &nasty_vm::VmDisk, right: &nasty_vm::VmDisk) -> bool {
    paths_match(
        left.source.as_deref().unwrap_or(&left.path),
        right.source.as_deref().unwrap_or(&right.path),
    )
}

fn mounted_disk_consumers(device: &str, mounts: &str) -> Vec<String> {
    let decode = |field: &str| {
        field
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
    };
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = decode(fields.next()?);
            let mountpoint = decode(fields.next()?);
            paths_match(device, &source).then(|| format!("Local mount '{mountpoint}'"))
        })
        .collect()
}

async fn disk_consumers(
    state: &AppState,
    sv: &nasty_storage::subvolume::Subvolume,
    except_vm: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut consumers = Vec::new();
    if let Some(csi) = csi_consumer(&sv.properties) {
        consumers.push(csi);
    }
    for vm in state
        .vms
        .list_strict()
        .await
        .map_err(|e| format!("cannot verify VM disk usage: {e}"))?
    {
        if except_vm == Some(vm.config.id.as_str()) {
            continue;
        }
        if vm
            .config
            .disks
            .iter()
            .any(|d| disk_matches_subvolume(d, sv))
        {
            consumers.push(format!(
                "VM '{}' ({})",
                vm.config.name,
                if vm.running { "running" } else { "stopped" }
            ));
        }
    }
    for target in state
        .iscsi
        .list()
        .await
        .map_err(|e| format!("cannot verify iSCSI usage: {e}"))?
    {
        if target.luns.iter().any(|lun| {
            sv.block_volume_id
                .as_ref()
                .is_some_and(|id| lun.backing_volume.as_ref() == Some(id))
                || sv
                    .block_device
                    .as_deref()
                    .is_some_and(|p| paths_match(p, &lun.backstore_path))
        }) {
            consumers.push(format!("iSCSI target '{}'", target.iqn));
        }
    }
    for sub in state
        .nvmeof
        .list()
        .await
        .map_err(|e| format!("cannot verify NVMe-oF usage: {e}"))?
    {
        if sub.namespaces.iter().any(|ns| {
            sv.block_volume_id
                .as_ref()
                .is_some_and(|id| ns.backing_volume.as_ref() == Some(id))
                || sv
                    .block_device
                    .as_deref()
                    .is_some_and(|p| paths_match(p, &ns.device_path))
        }) {
            consumers.push(format!("NVMe-oF subsystem '{}'", sub.nqn));
        }
    }
    // Local mounts are consumers too, even when not configured as exports.
    let mounts = tokio::fs::read_to_string("/proc/mounts")
        .await
        .map_err(|e| format!("cannot verify local mounts: {e}"))?;
    if let Some(device) = sv.block_device.as_deref() {
        consumers.extend(mounted_disk_consumers(device, &mounts));
    }
    Ok(consumers)
}

async fn check_new_vm_disks(
    state: &AppState,
    disks: &[nasty_vm::VmDisk],
    existing: &[nasty_vm::VmDisk],
    vm_id: Option<&str>,
) -> Result<(), String> {
    if disks.is_empty() {
        return Ok(());
    }
    for (index, disk) in disks.iter().enumerate() {
        if disks[..index]
            .iter()
            .any(|other| disks_share_backing(disk, other))
        {
            return Err("The same backing disk cannot be attached twice to one VM".into());
        }
    }
    let managed = state
        .subvolumes
        .list_all(None, None)
        .await
        .map_err(|e| format!("cannot verify disk inventory: {e}"))?;
    let vms = state
        .vms
        .list_strict()
        .await
        .map_err(|e| format!("cannot verify VM disk usage: {e}"))?;
    for disk in disks {
        if existing
            .iter()
            .any(|d| d.source == disk.source && paths_match(&d.path, &disk.path))
        {
            continue;
        }
        for vm in &vms {
            if vm_id == Some(vm.config.id.as_str()) {
                continue;
            }
            if vm.config.disks.iter().any(|d| disks_share_backing(d, disk)) {
                return Err(format!(
                    "Disk is already attached to VM '{}'. Detach it there first.",
                    vm.config.name
                ));
            }
        }
        if let Some(sv) = managed.iter().find(|sv| disk_matches_subvolume(disk, sv)) {
            if disks
                .iter()
                .filter(|d| disk_matches_subvolume(d, sv))
                .count()
                > 1
            {
                return Err("The same backing disk cannot be attached twice to one VM".into());
            }
            let consumers = disk_consumers(state, sv, vm_id).await?;
            if !consumers.is_empty() {
                return Err(format!(
                    "Cannot attach {}/{}: {}",
                    sv.filesystem,
                    sv.name,
                    consumers.join("; ")
                ));
            }
        } else if let Some(reason) = check_block_device_conflict(state, &disk.path, "vm").await {
            return Err(reason);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct VmSecurityFields<'a> {
    disks: &'a [nasty_vm::VmDisk],
    cdroms: &'a [String],
    legacy_boot_iso: Option<&'a str>,
    passthrough_devices: &'a [nasty_vm::PassthroughDevice],
    usb_devices: &'a [nasty_vm::UsbPassthrough],
    extra_args: Option<&'a [String]>,
    cpu_model: Option<&'a str>,
    machine_type: Option<&'a str>,
    vga: Option<&'a str>,
}

pub(super) fn paths_match(left: &str, right: &str) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| PathBuf::from(left));
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| PathBuf::from(right));
    left == right
}

fn path_resolves_under_dev(path: &str) -> bool {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(path))
        .starts_with("/dev")
}

fn operator_disk_is_safe(disk: &nasty_vm::VmDisk, managed: &[ManagedVmDisk]) -> bool {
    if let Some(source) = disk.source.as_deref() {
        return managed
            .iter()
            .any(|candidate| paths_match(source, &candidate.source));
    }

    if !path_resolves_under_dev(&disk.path) {
        return true;
    }

    managed.iter().any(|candidate| {
        candidate
            .device
            .as_deref()
            .is_some_and(|device| paths_match(&disk.path, device))
    })
}

fn vm_security_reason(
    fields: VmSecurityFields<'_>,
    managed: &[ManagedVmDisk],
) -> Option<&'static str> {
    if !fields.passthrough_devices.is_empty() {
        return Some("pci_passthrough");
    }
    if !fields.usb_devices.is_empty() {
        return Some("usb_passthrough");
    }
    if fields.extra_args.is_some_and(|args| !args.is_empty()) {
        return Some("raw_qemu_arguments");
    }
    if fields
        .cpu_model
        .is_some_and(|value| !matches!(value, "" | "host" | "max" | "qemu64"))
        || fields
            .machine_type
            .is_some_and(|value| !matches!(value, "" | "q35" | "i440fx"))
        || fields
            .vga
            .is_some_and(|value| !matches!(value, "" | "virtio" | "qxl" | "std" | "none"))
    {
        return Some("custom_qemu_hardware");
    }
    if fields
        .cdroms
        .iter()
        .any(|path| path_resolves_under_dev(path))
        || fields.legacy_boot_iso.is_some_and(path_resolves_under_dev)
    {
        return Some("raw_vm_cdrom");
    }
    if fields
        .disks
        .iter()
        .any(|disk| !operator_disk_is_safe(disk, managed))
    {
        return Some("unmanaged_vm_disk");
    }
    None
}

async fn managed_vm_disks(
    state: &AppState,
    session: &Session,
) -> Result<Vec<ManagedVmDisk>, String> {
    let subvolumes = state
        .subvolumes
        .list_all(session.filesystem.as_deref(), session.owner.as_deref())
        .await
        .map_err(|error| error.to_string())?;
    Ok(subvolumes
        .into_iter()
        .filter(|subvolume| {
            subvolume.subvolume_type == nasty_storage::subvolume::SubvolumeType::Block
                && subvolume.block_volume_id.is_some()
        })
        .map(|subvolume| ManagedVmDisk {
            device: subvolume.block_device,
            source: Path::new(&subvolume.path)
                .join("vol.img")
                .to_string_lossy()
                .into_owned(),
        })
        .collect())
}

async fn require_vm_payload_access(
    req: &Request,
    state: &AppState,
    session: &Session,
    fields: VmSecurityFields<'_>,
) -> Option<Response> {
    if session.filesystem.is_some() || session.owner.is_some() {
        if fields.disks.is_empty() {
            return require_unscoped_mutation(req, session, "vm_without_scoped_backing_disk");
        }
        for path in fields
            .disks
            .iter()
            .filter(|disk| disk.source.is_none() && !path_resolves_under_dev(&disk.path))
            .map(|disk| disk.path.as_str())
            .chain(fields.cdroms.iter().map(String::as_str))
            .chain(fields.legacy_boot_iso)
        {
            if let Err(error) = super::share::authorize_path_source(state, session, path).await {
                return Some(err(req, error));
            }
        }
    }

    let preliminary_reason = vm_security_reason(fields, &[])?;

    // Only raw-looking block disks need inventory resolution. The other
    // privileged fields are unconditionally Admin-only.
    if preliminary_reason != "unmanaged_vm_disk"
        || (session.role == Role::Admin && session.filesystem.is_none() && session.owner.is_none())
    {
        return require_root_equivalent(req, session, preliminary_reason);
    }

    let managed = match managed_vm_disks(state, session).await {
        Ok(managed) => managed,
        Err(error) => return Some(err(req, error)),
    };
    vm_security_reason(fields, &managed)
        .and_then(|reason| require_root_equivalent(req, session, reason))
}

async fn existing_vm_access_error(
    req: &Request,
    state: &AppState,
    session: &Session,
    id: &str,
) -> Option<Response> {
    if session.filesystem.is_none() && session.owner.is_none() {
        return None;
    }
    let config = match state.vms.get(id).await {
        Ok(status) => status.config,
        Err(error) => return Some(err(req, error)),
    };
    require_vm_payload_access(
        req,
        state,
        session,
        VmSecurityFields {
            disks: &config.disks,
            cdroms: &config.cdroms,
            legacy_boot_iso: config.boot_iso.as_deref(),
            passthrough_devices: &config.passthrough_devices,
            usb_devices: &config.usb_devices,
            extra_args: config.extra_args.as_deref(),
            cpu_model: config.cpu_model.as_deref(),
            machine_type: config.machine_type.as_deref(),
            vga: config.vga.as_deref(),
        },
    )
    .await
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    let _block_share_guard = if req.method == "vm.clone" {
        Some(state.block_share_mutation.lock().await)
    } else {
        None
    };

    Some(match req.method.as_str() {
        "vm.capabilities" => ok(req, state.vms.capabilities().await),
        "vm.disk.candidates" => {
            let result = async {
                let subvolumes = state
                    .subvolumes
                    .list_all(session.filesystem.as_deref(), session.owner.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                let mut candidates = Vec::new();
                for subvolume in subvolumes.into_iter().filter(|sv| {
                    sv.subvolume_type == nasty_storage::subvolume::SubvolumeType::Block
                }) {
                    let consumers = disk_consumers(state, &subvolume, None).await?;
                    candidates.push(VmDiskCandidate {
                        subvolume,
                        consumers,
                    });
                }
                Ok::<_, String>(candidates)
            }
            .await;
            match result {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "vm.list" => match state.vms.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "vm.get" => match require_str(req, "id") {
            Ok(id) => match state.vms.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.create" => match parse_params::<nasty_vm::CreateVmRequest>(req) {
            Ok(p) => {
                let _guard = state.block_share_mutation.lock().await;
                if let Some(response) = require_vm_payload_access(
                    req,
                    state,
                    session,
                    VmSecurityFields {
                        disks: p.disks.as_deref().unwrap_or_default(),
                        cdroms: p.cdroms.as_deref().unwrap_or_default(),
                        legacy_boot_iso: p.boot_iso.as_deref(),
                        passthrough_devices: p.passthrough_devices.as_deref().unwrap_or_default(),
                        usb_devices: p.usb_devices.as_deref().unwrap_or_default(),
                        extra_args: None,
                        cpu_model: None,
                        machine_type: None,
                        vga: None,
                    },
                )
                .await
                {
                    return Some(response);
                }
                if let Err(error) =
                    check_new_vm_disks(state, p.disks.as_deref().unwrap_or_default(), &[], None)
                        .await
                {
                    return Some(err(req, error));
                }
                match state.vms.create(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "vm.disk.create" => match parse_params::<CreateVmDiskRequest>(req) {
            Ok(p) => {
                if session
                    .filesystem
                    .as_deref()
                    .is_some_and(|filesystem| filesystem != p.filesystem)
                {
                    err(req, "access denied")
                } else {
                    let _guard = state.block_share_mutation.lock().await;
                    match state
                        .subvolumes
                        .create_vm_disk(
                            p.filesystem,
                            p.name,
                            p.volsize_bytes,
                            session.owner.clone(),
                        )
                        .await
                    {
                        Ok(disk) => ok(req, disk),
                        Err(error) => err(req, error),
                    }
                }
            }
            Err(error) => invalid(req, error),
        },
        "vm.update" => match parse_params::<nasty_vm::UpdateVmRequest>(req) {
            Ok(p) => {
                let _guard = state.block_share_mutation.lock().await;
                if let Some(response) = existing_vm_access_error(req, state, session, &p.id).await {
                    return Some(response);
                }
                let changes_security = p.disks.is_some()
                    || p.passthrough_devices.is_some()
                    || p.usb_devices.is_some()
                    || p.cdroms.is_some()
                    || p.boot_iso.is_some()
                    || p.extra_args.is_some()
                    || p.cpu_model.is_some()
                    || p.machine_type.is_some()
                    || p.vga.is_some()
                    || p.autostart == Some(true);
                if changes_security {
                    let existing = match state.vms.get(&p.id).await {
                        Ok(status) => status.config,
                        Err(error) => return Some(err(req, error)),
                    };
                    let replacing_cdroms = p.cdroms.is_some() || p.boot_iso.is_some();
                    let effective_cdroms: &[String] = if let Some(cdroms) = p.cdroms.as_deref() {
                        cdroms
                    } else if p.boot_iso.is_some() {
                        &[]
                    } else {
                        &existing.cdroms
                    };
                    if let Some(response) = require_vm_payload_access(
                        req,
                        state,
                        session,
                        VmSecurityFields {
                            disks: p.disks.as_deref().unwrap_or(&existing.disks),
                            cdroms: effective_cdroms,
                            legacy_boot_iso: if replacing_cdroms {
                                p.boot_iso.as_deref()
                            } else {
                                existing.boot_iso.as_deref()
                            },
                            passthrough_devices: p
                                .passthrough_devices
                                .as_deref()
                                .unwrap_or(&existing.passthrough_devices),
                            usb_devices: p.usb_devices.as_deref().unwrap_or(&existing.usb_devices),
                            extra_args: p.extra_args.as_deref().or(existing.extra_args.as_deref()),
                            cpu_model: p.cpu_model.as_deref().or(existing.cpu_model.as_deref()),
                            machine_type: p
                                .machine_type
                                .as_deref()
                                .or(existing.machine_type.as_deref()),
                            vga: p.vga.as_deref().or(existing.vga.as_deref()),
                        },
                    )
                    .await
                    {
                        return Some(response);
                    }
                }
                if let Some(disks) = p.disks.as_deref() {
                    let existing = match state.vms.get(&p.id).await {
                        Ok(vm) => vm.config,
                        Err(error) => return Some(err(req, error)),
                    };
                    if let Err(error) =
                        check_new_vm_disks(state, disks, &existing.disks, Some(&p.id)).await
                    {
                        return Some(err(req, error));
                    }
                }
                match state.vms.update(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "vm.delete" => match require_str(req, "id") {
            Ok(id) => {
                if let Some(response) = existing_vm_access_error(req, state, session, id).await {
                    return Some(response);
                }
                match state.vms.delete(id).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                }
            }
            Err(r) => r,
        },
        "vm.start" => match require_str(req, "id") {
            Ok(id) => {
                let _guard = state.block_share_mutation.lock().await;
                match state.vms.get(id).await {
                    Ok(status) => {
                        if let Some(response) = require_vm_payload_access(
                            req,
                            state,
                            session,
                            VmSecurityFields {
                                disks: &status.config.disks,
                                cdroms: &status.config.cdroms,
                                legacy_boot_iso: status.config.boot_iso.as_deref(),
                                passthrough_devices: &status.config.passthrough_devices,
                                usb_devices: &status.config.usb_devices,
                                extra_args: status.config.extra_args.as_deref(),
                                cpu_model: status.config.cpu_model.as_deref(),
                                machine_type: status.config.machine_type.as_deref(),
                                vga: status.config.vga.as_deref(),
                            },
                        )
                        .await
                        {
                            return Some(response);
                        }
                        if let Err(error) =
                            check_new_vm_disks(state, &status.config.disks, &[], Some(id)).await
                        {
                            return Some(err(req, error));
                        }
                        match state.vms.start(id).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                    Err(e) => err(req, e),
                }
            }
            Err(r) => r,
        },
        "vm.stop" => match require_str(req, "id") {
            Ok(id) => {
                if let Some(response) = existing_vm_access_error(req, state, session, id).await {
                    return Some(response);
                }
                match state.vms.stop(id).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                }
            }
            Err(r) => r,
        },
        "vm.kill" => match require_str(req, "id") {
            Ok(id) => {
                if let Some(response) = existing_vm_access_error(req, state, session, id).await {
                    return Some(response);
                }
                match state.vms.kill(id).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                }
            }
            Err(r) => r,
        },
        "vm.snapshot" => match parse_params::<nasty_vm::SnapshotVmRequest>(req) {
            Ok(p) => match vm_snapshot(
                state,
                &p,
                session.filesystem.as_deref(),
                session.owner.as_deref(),
            )
            .await
            {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.clone" => match parse_params::<nasty_vm::CloneVmRequest>(req) {
            Ok(p) => {
                if let Some(response) =
                    require_unscoped_mutation(req, session, "global_vm_clone_inventory")
                {
                    return Some(response);
                }
                match vm_clone(state, &p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "vm.images.list" => ok(req, list_vm_images(state).await),
        "vm.images.ensure" => match require_str(req, "filesystem") {
            Ok(fs) => match ensure_images_subvolume(state, fs).await {
                Ok(path) => ok(req, path),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        // Pre-flight for the streaming disk-import WS — surfaces the
        // image's virtual size so the UI can recommend (or block) a
        // target subvolume before opening the WebSocket.
        "vm.images.import_info" => match (require_str(req, "filesystem"), require_str(req, "name"))
        {
            (Ok(fs), Ok(name)) => {
                match crate::vm_disk_import::resolve_image_path(state, fs, name).await {
                    Ok((path, kind)) => {
                        match crate::vm_disk_import::read_image_info(&path, &kind).await {
                            Ok(info) => ok(req, info),
                            Err(e) => err(req, e),
                        }
                    }
                    Err(e) => err(req, e),
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_subvolume() -> nasty_storage::subvolume::Subvolume {
        serde_json::from_value(serde_json::json!({
            "name":"data", "filesystem":"tank", "subvolume_type":"block",
            "path":"/fs/tank/data", "block_device":"/dev/loop7", "snapshots":[]
        }))
        .unwrap()
    }

    #[test]
    fn csi_reservation_reports_pvc_even_without_an_active_export() {
        let mut properties = std::collections::HashMap::new();
        assert!(csi_consumer(&properties).is_none());
        properties.insert("nasty-csi:managed_by".into(), "nasty-csi".into());
        properties.insert("nasty-csi:pvc_namespace".into(), "lab".into());
        properties.insert("nasty-csi:pvc_name".into(), "database".into());
        assert!(csi_consumer(&properties).unwrap().contains("lab/database"));
        properties.remove("nasty-csi:pvc_name");
        assert!(csi_consumer(&properties).is_some());
    }

    #[test]
    fn saved_disk_source_wins_over_recycled_loop_device() {
        let sv = test_subvolume();
        assert!(disk_matches_subvolume(
            &disk("/dev/loop99", Some("/fs/tank/data/vol.img")),
            &sv
        ));
        assert!(!disk_matches_subvolume(
            &disk("/dev/loop7", Some("/fs/tank/other/vol.img")),
            &sv
        ));
        assert!(disk_matches_subvolume(&disk("/dev/loop7", None), &sv));
        assert!(disk_matches_subvolume(
            &disk("/fs/tank/data/vol.img", None),
            &sv
        ));
        assert!(!disks_share_backing(
            &disk("/dev/loop7", Some("/fs/tank/data/vol.img")),
            &disk("/dev/loop7", Some("/fs/tank/other/vol.img")),
        ));
    }

    #[test]
    fn managed_disk_aliases_match_by_source_and_device() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("vol.img");
        std::fs::write(&file, []).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        let mut sv = test_subvolume();
        sv.path = dir.path().to_string_lossy().into_owned();
        assert!(disk_matches_subvolume(
            &disk(alias.to_str().unwrap(), None),
            &sv
        ));
        assert!(disk_matches_subvolume(
            &disk("/dev/loop99", Some(alias.to_str().unwrap())),
            &sv
        ));
    }

    #[test]
    fn local_mount_consumers_decode_escaped_paths_and_ignore_unrelated_devices() {
        let mounts = "/dev/loop7 /mnt/with\\040space ext4 rw 0 0\n/dev/loop8 /other ext4 rw 0 0\n";
        assert_eq!(
            mounted_disk_consumers("/dev/loop7", mounts),
            ["Local mount '/mnt/with space'"]
        );
        assert!(mounted_disk_consumers("/dev/loop9", mounts).is_empty());
    }

    fn disk(path: &str, source: Option<&str>) -> nasty_vm::VmDisk {
        nasty_vm::VmDisk {
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

    fn managed() -> Vec<ManagedVmDisk> {
        vec![ManagedVmDisk {
            device: Some("/dev/loop7".to_string()),
            source: "/fs/tank/vms/managed/vol.img".to_string(),
        }]
    }

    fn fields<'a>(disks: &'a [nasty_vm::VmDisk]) -> VmSecurityFields<'a> {
        VmSecurityFields {
            disks,
            cdroms: &[],
            legacy_boot_iso: None,
            passthrough_devices: &[],
            usb_devices: &[],
            extra_args: None,
            cpu_model: None,
            machine_type: None,
            vga: None,
        }
    }

    #[test]
    fn operator_allows_image_and_managed_block_disks() {
        let managed = managed();
        assert!(operator_disk_is_safe(
            &disk("/fs/tank/vms/image.qcow2", None),
            &managed
        ));
        assert!(operator_disk_is_safe(&disk("/dev/loop7", None), &managed));
        assert!(operator_disk_is_safe(
            &disk("/dev/loop99", Some("/fs/tank/vms/managed/vol.img")),
            &managed
        ));
    }

    #[test]
    fn operator_rejects_raw_devices_and_forged_sources() {
        let managed = managed();
        assert!(!operator_disk_is_safe(&disk("/dev/sda", None), &managed));
        assert!(!operator_disk_is_safe(&disk("/dev/loop8", None), &managed));
        assert!(!operator_disk_is_safe(
            &disk(
                "/fs/tank/vms/image.qcow2",
                Some("/fs/tank/vms/unknown/vol.img")
            ),
            &managed
        ));
    }

    #[test]
    fn operator_vm_policy_rejects_passthrough_and_raw_qemu_args() {
        let pci = nasty_vm::PassthroughDevice {
            address: "0000:03:00.0".to_string(),
            label: None,
        };
        let usb = nasty_vm::UsbPassthrough {
            vendor_id: "1234".to_string(),
            product_id: "5678".to_string(),
            label: None,
        };
        assert_eq!(
            vm_security_reason(
                VmSecurityFields {
                    passthrough_devices: &[pci],
                    ..fields(&[])
                },
                &managed()
            ),
            Some("pci_passthrough")
        );
        assert_eq!(
            vm_security_reason(
                VmSecurityFields {
                    usb_devices: &[usb],
                    ..fields(&[])
                },
                &managed()
            ),
            Some("usb_passthrough")
        );
        assert_eq!(
            vm_security_reason(
                VmSecurityFields {
                    extra_args: Some(&["-nodefaults".to_string()]),
                    ..fields(&[])
                },
                &managed()
            ),
            Some("raw_qemu_arguments")
        );
    }

    #[test]
    fn empty_privileged_fields_are_operator_safe() {
        assert_eq!(
            vm_security_reason(
                VmSecurityFields {
                    extra_args: Some(&[]),
                    ..fields(&[])
                },
                &managed()
            ),
            None
        );
    }

    #[test]
    fn operator_rejects_raw_device_cdroms() {
        assert_eq!(
            vm_security_reason(
                VmSecurityFields {
                    cdroms: &["/dev/sr0".to_string()],
                    ..fields(&[])
                },
                &managed()
            ),
            Some("raw_vm_cdrom")
        );
        assert_eq!(
            vm_security_reason(
                VmSecurityFields {
                    legacy_boot_iso: Some("/dev/sda"),
                    ..fields(&[])
                },
                &managed()
            ),
            Some("raw_vm_cdrom")
        );
    }
}
