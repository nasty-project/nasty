use nasty_common::{ErrorCode, Request, Response};
use tracing::debug;

mod alerts;
mod apps;
mod audit;
mod auth;
mod backup;
mod bcachefs;
mod fs;
mod notifications;
mod service;
mod share;
mod smb;
mod snapshot;
mod subvolume;
mod system;
mod vm;

use crate::AppState;
use crate::auth::{Role, Session};

/// Methods every authenticated user can call regardless of role.
/// Two categories:
///   1. Pure reads (`is_read_only`).
///   2. Mutations that only affect the caller's own session/account —
///      logging out, changing your own password. Putting these in
///      `is_read_only` would be misleading (they DO write), so the
///      role-check pipes through this wider predicate instead.
fn is_universally_allowed(method: &str) -> bool {
    is_read_only(method)
        || matches!(
            method,
            // Without these, ReadOnly and Operator users literally
            // couldn't log out or change their own password — the
            // engine would deny them with "Permission denied".
            "auth.logout" | "auth.change_password"
            // Self-managed WebAuthn credentials (#289 PR #1). Same
            // logic as change_password: every authenticated user
            // gets to manage their own security keys, regardless
            // of role.
            | "auth.webauthn.register.start"
            | "auth.webauthn.register.finish"
            | "auth.webauthn.delete"
        )
}

/// Methods an operator token is allowed to call (in addition to
/// everything in `is_universally_allowed`).
fn is_operator_allowed(method: &str) -> bool {
    is_universally_allowed(method)
        || matches!(
            method,
            "subvolume.create"
                | "subvolume.delete"
                | "subvolume.attach"
                | "subvolume.detach"
                | "subvolume.resize"
                | "subvolume.update"
                | "subvolume.clone"
                | "subvolume.set_properties"
                | "subvolume.remove_properties"
                | "snapshot.create"
                | "snapshot.delete"
                | "snapshot.clone"
                | "share.nfs.create"
                | "share.nfs.update"
                | "share.nfs.delete"
                | "share.smb.create"
                | "share.smb.update"
                | "share.smb.delete"
                | "smb.user.create"
                | "smb.user.delete"
                | "smb.user.set_password"
                // Operator can create, delete, AND manage members of
                // SMB groups — the old list only had `delete`, which
                // meant operators could tear groups down they had no
                // way to build up.
                | "smb.group.create"
                | "smb.group.delete"
                | "smb.group.add_member"
                | "smb.group.remove_member"
                | "share.iscsi.create"
                | "share.iscsi.delete"
                | "share.iscsi.add_lun"
                | "share.iscsi.remove_lun"
                | "share.iscsi.add_acl"
                | "share.iscsi.remove_acl"
                | "share.iscsi.add_portal"
                | "share.iscsi.remove_portal"
                | "share.nvmeof.create"
                | "share.nvmeof.delete"
                | "share.nvmeof.add_namespace"
                | "share.nvmeof.remove_namespace"
                | "share.nvmeof.add_port"
                | "share.nvmeof.remove_port"
                | "share.nvmeof.add_host"
                | "share.nvmeof.remove_host"
                | "vm.create"
                | "vm.update"
                // `vm.delete` was admin-only; operator could spin up
                // VMs they had no way to tear down. Closes the same
                // create-vs-delete asymmetry as smb.group above.
                | "vm.delete"
                | "vm.start"
                | "vm.stop"
                | "vm.kill"
                | "vm.snapshot"
                | "vm.clone"
                | "apps.enable"
                // `apps.disable` was admin-only — same asymmetry: an
                // operator could turn the apps runtime on but not off.
                | "apps.disable"
                | "apps.install"
                | "apps.update"
                | "apps.remove"
                | "apps.stop"
                | "apps.start"
                | "apps.restart"
                | "apps.pull"
                | "apps.prune"
                | "apps.compose.install"
                | "apps.compose.update"
                | "apps.compose.remove"
                // Appdata relocation (#436) — same altitude as the
                // rest of app lifecycle management.
                | "apps.appdata.relocate"
                | "apps.ingress.set"
                | "apps.ingress.remove"
                // Backup lifecycle is operator territory in a NAS
                // appliance — same role that manages shares + apps
                // typically manages where the data is copied. The
                // read paths (`backup.profile.get`/`list`,
                // `backup.status`, `backup.snapshots`) are already
                // in `is_read_only`, so credentials in profiles are
                // already visible to operators; admitting the write
                // paths doesn't widen secrets exposure.
                | "backup.profile.create"
                | "backup.profile.update"
                | "backup.profile.delete"
                | "backup.run"
                | "backup.repo.check"
                | "backup.repo.init"
                // Service-protocol toggles (NFS/SMB/iSCSI/NVMe-oF
                // server services + SSH/mDNS/SMART). Operators were
                // creating shares for protocols they couldn't turn
                // on — the share would land on disk but no server
                // was listening. Same coupling as share CRUD.
                | "service.protocol.enable"
                | "service.protocol.disable"
                // VM disk-image import. Operator already has
                // `vm.create` etc.; without import they can't
                // populate the disk to boot from.
                | "vm.images.ensure"
                | "firmware.update"
        )
}

/// Extract a string param from JSON-RPC params
pub(super) fn str_param<'a>(request: &'a Request, key: &str) -> Option<&'a str> {
    request
        .params
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
}

/// Parse typed params from JSON-RPC request
pub(super) fn parse_params<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, String> {
    request
        .params
        .as_ref()
        .ok_or_else(|| "missing params".to_string())
        .and_then(|p| serde_json::from_value(p.clone()).map_err(|e| e.to_string()))
}

/// Check if a method is read-only (safe for ReadOnly role)
fn is_read_only(method: &str) -> bool {
    // Suffix heuristics catch the vast majority of pure reads without
    // forcing every new endpoint to be enumerated below. `.list` /
    // `.get` were here from the start; `.status` was added after
    // `fs.tpm.status` triggered a refresh-loop on the Filesystems
    // page (its event-bus broadcast classified it as a write because
    // it didn't match any of the read suffixes or the explicit list,
    // every refresh fired another refresh, the page's `isBusy()`
    // progress bar blinked indefinitely). Every `.status` endpoint
    // in the codebase reports state without mutating it
    // (`fs.scrub.status`, `system.update.status`, `apps.status`,
    // `system.ssh.status`, `system.nut.status`, `system.acme.status`,
    // `system.firewall.status`, `system.tls.host_statuses`,
    // `fs.reconcile.status`, …), so this is safe.
    method.ends_with(".list")
        || method.ends_with(".get")
        || method.ends_with(".status")
        || matches!(
            method,
            "system.info"
                | "system.health"
                | "system.hardware.iommu"
                | "system.hardware.summary"
                | "system.secure_boot.enrollment.status"
                | "system.secure_boot.readiness"
                | "system.passthrough.get"
                | "system.stats"
                | "system.disks"
                | "system.network.get"
                | "system.logs"
                | "system.logs.units"
                | "system.ssh.status"
                | "system.alerts"
                | "system.settings.get"
                | "system.tuning.get"
                | "system.nut.config.get"
                | "system.nut.status"
                | "system.tailscale.get"
                | "system.acme.status"
                | "system.tls.local_ca_root"
                | "system.tls.host_statuses"
                | "system.metrics.history"
                | "system.metrics.prometheus"
                | "alert.rules.list"
                | "device.list"
                | "auth.me"
                | "auth.list_users"
                | "auth.token.list"
                | "fs.dependents"
                | "fs.locked_dependents"
                | "fs.usage"
                | "fs.scrub.status"
                | "fs.reconcile.status"
                | "bcachefs.usage"
                | "service.protocol.list"
                | "subvolume.list_all"
                | "subvolume.list_dependents"
                | "subvolume.find_by_property"
                | "subvolume.children"
                | "smb.user.list"
                | "smb.group.list"
                | "service.rest_server.config"
                | "service.base_names.get"
                | "system.update.version"
                | "system.update.status"
                | "system.update.build_dir.get"
                | "system.reboot_required"
                | "system.generations.list"
                | "system.version.get"
                | "system.version.tagged_release_notice"
                | "system.log.level"
                | "system.settings.timezones"
                | "audit.list"
                | "apps.check_ports"
                | "apps.check_devices"
                | "apps.check_volumes"
                | "apps.check_compose"
                | "apps.status"
                // Live CPU / mem / network stats per container.
                // Pure read; the WebUI polls it from the Apps page
                // and ReadOnly users need it for the dashboard to
                // populate.
                | "apps.stats"
                | "apps.logs"
                | "apps.compose.logs"
                | "apps.container.logs"
                | "apps.inspect"
                | "system.firewall.status"
                | "vm.capabilities"
                | "vm.images.import_info"
                | "firmware.available"
                | "firmware.constraints"
                | "firmware.check"
                | "firmware.devices"
                | "notifications.config.get"
                | "apps.config"
                | "apps.inspect_image"
                | "apps.caddy.routes"
                | "apps.ingress.check_conflict"
                | "bcachefs.timestats"
                | "bcachefs.top"
                | "backup.profile.list"
                | "backup.profile.get"
                | "backup.status"
                | "backup.snapshots"
                | "backup.jobs.list"
                | "backup.jobs.get"
                | "auth.oidc.config_status"
                | "auth.webauthn.config"
                | "auth.webauthn.list"
        )
}

/// Derive the collection name for a mutation method, or None if read-only.
fn collection_for_method(method: &str) -> Option<&'static str> {
    match method {
        m if m.starts_with("fs.device.") => Some("filesystem"),
        m if m.starts_with("fs.") && !is_read_only(m) => Some("filesystem"),
        m if m.starts_with("device.") && !is_read_only(m) => Some("filesystem"),
        m if m.starts_with("subvolume.") && !is_read_only(m) => Some("subvolume"),
        m if m.starts_with("snapshot.") && !is_read_only(m) => Some("snapshot"),
        m if m.starts_with("share.nfs.") && !is_read_only(m) => Some("share.nfs"),
        m if m.starts_with("share.smb.") && !is_read_only(m) => Some("share.smb"),
        m if m.starts_with("share.iscsi.") && !is_read_only(m) => Some("share.iscsi"),
        m if m.starts_with("share.nvmeof.") && !is_read_only(m) => Some("share.nvmeof"),
        m if m.starts_with("service.protocol.") && !is_read_only(m) => Some("protocol"),
        m if m.starts_with("system.settings.") && !is_read_only(m) => Some("settings"),
        m if m.starts_with("system.tuning.") && !is_read_only(m) => Some("tuning"),
        m if m.starts_with("system.nut.") && !is_read_only(m) => Some("nut"),
        m if m.starts_with("system.tailscale.") && !is_read_only(m) => Some("tailscale"),
        m if m.starts_with("alert.rules.") && !is_read_only(m) => Some("alert"),
        _ => None,
    }
}

/// Extract a human-readable summary from mutation params for audit logging.
fn audit_detail(request: &Request) -> String {
    let params = match request.params.as_ref() {
        Some(p) => p,
        None => return String::new(),
    };

    // Try common identifier fields in order of specificity
    for key in ["name", "username", "filesystem", "target", "id", "path"] {
        if let Some(val) = params.get(key).and_then(|v| v.as_str()) {
            return val.to_string();
        }
    }

    // For device operations, show the device
    if let Some(val) = params.get("device").and_then(|v| v.as_str()) {
        return val.to_string();
    }

    String::new()
}

/// Route a JSON-RPC request to the appropriate handler
pub async fn handle_rpc_request(raw: &str, state: &AppState, session: &Session) -> String {
    let request: Request = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(_) => {
            let resp = Response::error(
                serde_json::Value::Null,
                ErrorCode::ParseError,
                "Failed to parse JSON-RPC request",
            );
            return serde_json::to_string(&resp).unwrap();
        }
    };

    debug!("RPC call: {} (user: {})", request.method, session.username);

    // Force password change — only allow auth methods until the password is changed
    if session.must_change_password
        && !matches!(
            request.method.as_str(),
            "auth.change_password" | "auth.me" | "auth.logout"
        )
    {
        let resp = Response::error(
            request.id,
            ErrorCode::InternalError,
            "Password change required",
        );
        return serde_json::to_string(&resp).unwrap();
    }

    // Enforce role permissions.
    //
    // ReadOnly used to map to `is_read_only` directly, which meant
    // ReadOnly users couldn't log out or change their own password —
    // both are mutations and so didn't qualify as "read-only". The
    // wider `is_universally_allowed` predicate handles the read-set
    // plus the small set of self-only mutations every authenticated
    // user is allowed.
    let denied = match session.role {
        Role::Admin => false,
        Role::ReadOnly => !is_universally_allowed(&request.method),
        Role::Operator => !is_operator_allowed(&request.method),
    };
    if denied {
        // Record role-based denials so an attempted role-escalation
        // (or a misconfigured client) leaves a trail. Read methods are
        // never denied here (is_universally_allowed covers them), so
        // this can't fire on routine browser polling — every entry
        // represents an intentional mutation the session role couldn't
        // perform.
        crate::auth::audit(
            "permission_denied",
            &session.username,
            session.client_ip.as_deref().unwrap_or("unknown"),
            &format!("method={} role={:?}", request.method, session.role),
        );
        let resp = Response::error(request.id, ErrorCode::InternalError, "Permission denied");
        return serde_json::to_string(&resp).unwrap();
    }

    let t0 = std::time::Instant::now();
    let response = route(&request, state, session).await;
    let elapsed = t0.elapsed();
    if elapsed.as_millis() > 5000 {
        tracing::error!(
            "RPC very slow: {} took {}ms",
            request.method,
            elapsed.as_millis()
        );
    } else if elapsed.as_millis() > 1000 {
        tracing::warn!(
            "RPC slow: {} took {}ms",
            request.method,
            elapsed.as_millis()
        );
    } else {
        debug!("RPC done: {} in {}ms", request.method, elapsed.as_millis());
    }

    // Audit log + broadcast event on successful mutations
    if response.error.is_none() {
        // Auth mutations are already audited in auth.rs — skip them here
        if !is_read_only(&request.method) && !request.method.starts_with("auth.") {
            let detail = audit_detail(&request);
            crate::auth::audit(
                &request.method,
                &session.username,
                session.client_ip.as_deref().unwrap_or("unknown"),
                &detail,
            );
        }
        if let Some(collection) = collection_for_method(&request.method) {
            let _ = state.events.send(collection.to_string());
        }
    }

    serde_json::to_string(&response).unwrap()
}

async fn route(req: &Request, state: &AppState, session: &Session) -> Response {
    // Each domain module owns a slice of the original 231-arm match. We
    // dispatch by method prefix (one segment at most) — every method we
    // serve has a `<domain>.<rest>` shape, so a single split is enough.
    // Domains that own multiple prefixes (e.g. fs + device) declare so in
    // their `try_route` by accepting both.
    let prefix = req
        .method
        .split_once('.')
        .map(|(p, _)| p)
        .unwrap_or(req.method.as_str());
    let resp = match prefix {
        "auth" => auth::try_route(req, state, session).await,
        "audit" => audit::try_route(req, state, session).await,
        "alert" | "telemetry" => alerts::try_route(req, state, session).await,
        "notifications" => notifications::try_route(req, state, session).await,
        "backup" => backup::try_route(req, state, session).await,
        "fs" | "device" => fs::try_route(req, state, session).await,
        "bcachefs" => bcachefs::try_route(req, state, session).await,
        "subvolume" => subvolume::try_route(req, state, session).await,
        "snapshot" => snapshot::try_route(req, state, session).await,
        "share" => share::try_route(req, state, session).await,
        "smb" => smb::try_route(req, state, session).await,
        "service" => service::try_route(req, state, session).await,
        "system" => {
            // `system.alerts` lives in the alerts module; everything else
            // is system. Try alerts first and fall back.
            if req.method == "system.alerts" {
                alerts::try_route(req, state, session).await
            } else {
                system::try_route(req, state, session).await
            }
        }
        "firmware" => system::try_route(req, state, session).await,
        "vm" => vm::try_route(req, state, session).await,
        "apps" => apps::try_route(req, state, session).await,
        _ => None,
    };
    resp.unwrap_or_else(|| {
        Response::error(
            req.id.clone(),
            ErrorCode::MethodNotFound,
            format!("Unknown method: {}", req.method),
        )
    })
}

// ── Helpers ──────────────────────────────────────────────────────

pub(super) fn ok(req: &Request, val: impl serde::Serialize) -> Response {
    Response::success(req.id.clone(), serde_json::to_value(val).unwrap())
}

pub(super) fn err(req: &Request, e: impl std::fmt::Display) -> Response {
    Response::error(req.id.clone(), ErrorCode::InternalError, e.to_string())
}

pub(super) fn invalid(req: &Request, msg: impl std::fmt::Display) -> Response {
    Response::error(
        req.id.clone(),
        ErrorCode::InvalidParams,
        format!("Invalid params: {msg}"),
    )
}

/// Return an error response if the given protocol is not enabled.
pub(super) async fn require_protocol(
    state: &AppState,
    req: &Request,
    proto: nasty_system::protocol::Protocol,
) -> Option<Response> {
    if !state.protocols.is_enabled(proto).await {
        Some(Response::error(
            req.id.clone(),
            ErrorCode::InternalError,
            format!(
                "{} protocol is not enabled — enable it first via service.protocol.enable",
                proto.display_name()
            ),
        ))
    } else {
        None
    }
}

#[allow(clippy::result_large_err)]
pub(super) fn require_str<'a>(req: &'a Request, key: &str) -> Result<&'a str, Response> {
    str_param(req, key).ok_or_else(|| {
        Response::error(
            req.id.clone(),
            ErrorCode::InvalidParams,
            format!("Missing required param: {key}"),
        )
    })
}

/// Fetch JSON from the nasty-metrics service.
pub(super) async fn fetch_metrics_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
) -> Result<T, String> {
    let url = format!("{}{path}", crate::METRICS_BASE);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("metrics service unavailable: {e}"))?
        .error_for_status()
        .map_err(|e| format!("metrics service error: {e}"))?;
    resp.json::<T>()
        .await
        .map_err(|e| format!("metrics parse error: {e}"))
}

/// Check if a block device is already exported by another block protocol.
/// Returns an error message if the device is in use, None if it's free.
pub(super) async fn check_block_device_conflict(
    state: &AppState,
    device_path: &str,
    exclude_protocol: &str,
) -> Option<String> {
    if exclude_protocol != "iscsi"
        && let Ok(targets) = state.iscsi.list().await
    {
        for target in &targets {
            for lun in &target.luns {
                if lun.backstore_path == device_path {
                    return Some(format!(
                        "device {} is already exported via iSCSI (target '{}')",
                        device_path, target.iqn
                    ));
                }
            }
        }
    }

    if exclude_protocol != "nvmeof"
        && let Ok(subsystems) = state.nvmeof.list().await
    {
        for sub in &subsystems {
            for ns in &sub.namespaces {
                if ns.device_path == device_path {
                    return Some(format!(
                        "device {} is already exported via NVMe-oF (subsystem '{}')",
                        device_path, sub.nqn
                    ));
                }
            }
        }
    }

    None
}

// ── VM image management ─────────────────────────────────────────

#[derive(serde::Serialize)]
pub(super) struct VmImageListResult {
    subvolume_exists: bool,
    images: Vec<serde_json::Value>,
}

/// List all VM images from `vms/images` directories across all
/// filesystems. The classifier in `vm_disk_import` is the single
/// source of truth for what counts as a VM image — including
/// compressed shapes like `.qcow2.xz`.
pub(super) async fn list_vm_images(state: &AppState) -> VmImageListResult {
    let filesystems = match state.filesystems.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("list_vm_images: filesystems.list() failed: {e}");
            Vec::new()
        }
    };
    let mut images = Vec::new();
    let mut subvolume_exists = false;

    for fs in &filesystems {
        if !fs.mounted {
            continue;
        }
        let Some(ref mp) = fs.mount_point else {
            continue;
        };
        let dir = format!("{mp}/vms/images");
        if !std::path::Path::new(&dir).is_dir() {
            continue;
        }
        subvolume_exists = true;

        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(kind) = crate::vm_disk_import::classify_vm_image(name) else {
                    continue;
                };
                // Skip the hidden tmp files an in-flight decompression
                // leaves behind so they don't pollute the picker.
                if name.starts_with(".nasty-import.") {
                    continue;
                }
                let size = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                images.push(serde_json::json!({
                    "name": name,
                    "path": path.to_string_lossy(),
                    "filesystem": fs.name,
                    "size_bytes": size,
                    "format": kind.format,
                    "compression": kind.compression,
                }));
            }
        }
    }

    VmImageListResult {
        subvolume_exists,
        images,
    }
}

/// Ensure the `vms/images` directory exists on a filesystem. Creates it if missing.
/// Migrates from legacy `.nasty/images` path if present.
pub(super) async fn ensure_images_subvolume(
    state: &AppState,
    filesystem: &str,
) -> Result<String, String> {
    let mount_point = state
        .filesystems
        .get(filesystem)
        .await
        .map_err(|e| e.to_string())?
        .mount_point
        .ok_or_else(|| "filesystem not mounted".to_string())?;

    let images_path = format!("{mount_point}/vms/images");
    let legacy_path = format!("{mount_point}/.nasty/images");

    // Migrate legacy path
    if !std::path::Path::new(&images_path).exists() && std::path::Path::new(&legacy_path).exists() {
        tokio::fs::create_dir_all(format!("{mount_point}/vms"))
            .await
            .map_err(|e| format!("failed to create vms dir: {e}"))?;
        if let Err(e) = tokio::fs::rename(&legacy_path, &images_path).await {
            tracing::warn!(
                "Failed to migrate VM images from {legacy_path}: {e}, using legacy path"
            );
            return Ok(legacy_path);
        }
        tracing::info!("Migrated VM images from {legacy_path} to {images_path}");
    }

    tokio::fs::create_dir_all(&images_path)
        .await
        .map_err(|e| format!("failed to create vms/images: {e}"))?;

    Ok(images_path)
}

// ── Subvolume in-use check ───────────────────────────────────────

/// Check if a subvolume is in use by a VM, iSCSI target, or NVMe-oF subsystem.
/// Returns an error message if in use, None if safe to delete.
pub(super) async fn check_subvolume_in_use(
    state: &AppState,
    filesystem: &str,
    name: &str,
) -> Option<String> {
    let sv = match state.subvolumes.get(filesystem, name, None).await.ok() {
        Some(sv) => sv,
        None => return None,
    };
    let block_device = sv.block_device.as_deref();
    let subvol_path = &sv.path;

    // ── Block device checks (VMs, iSCSI, NVMe-oF) ──

    if let Some(bd) = block_device {
        // Check VMs
        if let Ok(vms) = state.vms.list().await {
            for vm in &vms {
                for disk in &vm.config.disks {
                    if disk.path == bd {
                        return Some(format!(
                            "subvolume is in use as a disk by VM '{}'. Detach the disk first.",
                            vm.config.name
                        ));
                    }
                }
            }
        }

        // Check iSCSI targets
        if let Ok(targets) = state.iscsi.list().await {
            for target in &targets {
                for lun in &target.luns {
                    if lun.backstore_path == bd {
                        return Some(format!(
                            "subvolume is in use by iSCSI target '{}'. Delete the target first.",
                            target.iqn
                        ));
                    }
                }
            }
        }

        // Check NVMe-oF subsystems
        if let Ok(subsystems) = state.nvmeof.list().await {
            for subsys in &subsystems {
                for ns in &subsys.namespaces {
                    if ns.device_path == bd {
                        return Some(format!(
                            "subvolume is in use by NVMe-oF subsystem '{}'. Delete the subsystem first.",
                            subsys.nqn
                        ));
                    }
                }
            }
        }
    }

    // ── Path-based checks (NFS, SMB shares) ──

    if let Ok(nfs_shares) = state.nfs.list().await {
        for share in &nfs_shares {
            if share.path == *subvol_path || share.path.starts_with(&format!("{subvol_path}/")) {
                return Some(format!(
                    "subvolume is shared via NFS (path: {}). Delete the NFS share first.",
                    share.path
                ));
            }
        }
    }

    if let Ok(smb_shares) = state.smb.list().await {
        for share in &smb_shares {
            if share.path == *subvol_path || share.path.starts_with(&format!("{subvol_path}/")) {
                return Some(format!(
                    "subvolume is shared via SMB as '{}'. Delete the SMB share first.",
                    share.name
                ));
            }
        }
    }

    None
}

/// Check if a filesystem has any subvolumes with dependencies that would prevent destruction.
pub(super) async fn check_filesystem_in_use(state: &AppState, name: &str) -> Option<String> {
    // Get all subvolumes on this filesystem
    let subvols = state
        .subvolumes
        .list_all(None, None)
        .await
        .unwrap_or_default();
    let fs_subvols: Vec<_> = subvols.iter().filter(|sv| sv.filesystem == name).collect();

    if fs_subvols.is_empty() {
        return None;
    }

    // Check each subvolume for dependencies
    for sv in &fs_subvols {
        if let Some(reason) = check_subvolume_in_use(state, name, &sv.name).await {
            return Some(format!(
                "filesystem '{}' cannot be destroyed: subvolume '{}' is in use — {}",
                name, sv.name, reason
            ));
        }
    }

    // Check if apps runtime uses this filesystem
    if state.apps.is_enabled() {
        let config = nasty_apps::AppsService::load_config();
        if let Some(ref path) = config.storage_path
            && path.starts_with(&format!("/fs/{name}/"))
        {
            return Some(format!(
                "filesystem '{}' cannot be destroyed: apps runtime storage is on this filesystem. Disable Apps first.",
                name
            ));
        }
    }

    None
}

// ── VM storage integration ──────────────────────────────────────

/// Resolve VM disk paths to filesystem/subvolume pairs by matching
/// against all block subvolumes' attached loop devices.
pub(super) async fn resolve_vm_disks(
    state: &AppState,
    vm: &nasty_vm::VmConfig,
) -> Vec<nasty_vm::VmDiskSubvolume> {
    let all_subvols = state
        .subvolumes
        .list_all(None, None)
        .await
        .unwrap_or_default();
    let mut resolved = Vec::new();
    for disk in &vm.disks {
        for sv in &all_subvols {
            if let Some(ref bd) = sv.block_device
                && bd == &disk.path
            {
                resolved.push(nasty_vm::VmDiskSubvolume {
                    filesystem: sv.filesystem.clone(),
                    subvolume: sv.name.clone(),
                    device: disk.path.clone(),
                });
                break;
            }
        }
    }
    resolved
}

/// Snapshot all block subvolumes belonging to a VM.
pub(super) async fn vm_snapshot(
    state: &AppState,
    req: &nasty_vm::SnapshotVmRequest,
) -> Result<Vec<nasty_vm::VmDiskSubvolume>, String> {
    let vm_status = state.vms.get(&req.id).await.map_err(|e| e.to_string())?;
    let disks = resolve_vm_disks(state, &vm_status.config).await;

    if disks.is_empty() {
        return Err("no block subvolumes found for this VM".to_string());
    }

    // VM should ideally be stopped or paused for consistent snapshots
    if vm_status.running {
        // Send sync to guest via QMP if possible (best-effort)
        let _ = nasty_vm::qmp::execute(
            &format!("/run/nasty/vm/{}.qmp", req.id),
            "guest-fsfreeze-freeze",
            None,
        )
        .await;
    }

    for disk in &disks {
        let snap_req = nasty_storage::subvolume::CreateSnapshotRequest {
            filesystem: disk.filesystem.clone(),
            subvolume: disk.subvolume.clone(),
            name: req.name.clone(),
            read_only: Some(true),
        };
        state.snapshots.create(snap_req, None).await.map_err(|e| {
            format!(
                "failed to snapshot {}/{}: {e}",
                disk.filesystem, disk.subvolume
            )
        })?;
    }

    // Thaw if we froze
    if vm_status.running {
        let _ = nasty_vm::qmp::execute(
            &format!("/run/nasty/vm/{}.qmp", req.id),
            "guest-fsfreeze-thaw",
            None,
        )
        .await;
    }

    Ok(disks)
}

/// Clone a VM: create a new VM config with COW-cloned disk subvolumes.
pub(super) async fn vm_clone(
    state: &AppState,
    req: &nasty_vm::CloneVmRequest,
) -> Result<nasty_vm::VmConfig, String> {
    let vm_status = state.vms.get(&req.id).await.map_err(|e| e.to_string())?;

    if vm_status.running {
        return Err("stop the VM before cloning".to_string());
    }

    let disks = resolve_vm_disks(state, &vm_status.config).await;

    // Clone each block subvolume
    let mut new_disks = Vec::new();
    for disk in &disks {
        let clone_name = format!("{}-{}", disk.subvolume, req.new_name);
        let clone_req = nasty_storage::subvolume::CloneSubvolumeRequest {
            filesystem: disk.filesystem.clone(),
            name: disk.subvolume.clone(),
            new_name: clone_name.clone(),
        };
        let cloned = state
            .subvolumes
            .clone_subvolume(clone_req, None)
            .await
            .map_err(|e| {
                format!(
                    "failed to clone {}/{}: {e}",
                    disk.filesystem, disk.subvolume
                )
            })?;

        new_disks.push(nasty_vm::VmDisk {
            path: cloned.block_device.unwrap_or_default(),
            interface: "virtio".to_string(),
            readonly: false,
            cache: None,
            aio: None,
            discard: None,
            iops_rd: None,
            iops_wr: None,
        });
    }

    // Create new VM config based on the source, with cloned disks
    let src = &vm_status.config;
    let create_req = nasty_vm::CreateVmRequest {
        name: req.new_name.clone(),
        cpus: Some(src.cpus),
        memory_mib: Some(src.memory_mib),
        disks: if new_disks.is_empty() {
            None
        } else {
            Some(new_disks)
        },
        networks: Some(src.networks.clone()),
        passthrough_devices: None, // Don't clone passthrough — can't share devices
        usb_devices: None,         // Same reasoning — only one VM at a time can own a USB device
        cdroms: None,
        boot_iso: None,
        boot_order: Some(src.boot_order.clone()),
        uefi: Some(src.uefi),
        description: Some(format!("Clone of {}", src.name)),
        autostart: Some(false),
    };

    state
        .vms
        .create(create_req)
        .await
        .map_err(|e| e.to_string())
}

/// Evaluate the full alert ruleset against live system state and return any
/// firing alerts. Used by both the `system.alerts` RPC handler (which adds a
/// 20s cache for cheap WebUI polling) and the background notifier in
/// `spawn_alert_notifier` (which previously depended on a browser polling to
/// populate that same cache — meaning alerts only fired when an admin had
/// the dashboard open).
///
/// Errors fetching individual signals are swallowed and the corresponding
/// alert family is treated as "no data" so a metrics-service blip doesn't
/// silence everything else.
pub(crate) async fn evaluate_active_alerts(
    state: &AppState,
) -> Vec<nasty_system::alerts::ActiveAlert> {
    use nasty_system::alerts;

    // System stats — required for CPU/memory/temp rules. If the metrics
    // service is down, evaluating those rules without data is meaningless;
    // return an empty alert set rather than fabricating false positives.
    let stats =
        match fetch_metrics_json::<nasty_system::SystemStats>(&state.metrics_client, "/api/stats")
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("alert evaluation: stats fetch failed: {e}");
                return Vec::new();
            }
        };

    let filesystems = match state.filesystems.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "alert evaluation: filesystems.list() failed: {e} — \
                 fs-level alerts will be skipped this cycle"
            );
            Vec::new()
        }
    };
    let disk_health: Vec<nasty_system::DiskHealth> = if state
        .protocols
        .is_enabled(nasty_system::protocol::Protocol::Smart)
        .await
    {
        fetch_metrics_json(&state.metrics_client, "/api/disks")
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let fs_usage_list: Vec<alerts::FsUsage> = filesystems
        .iter()
        .map(|p| alerts::FsUsage {
            name: p.name.clone(),
            used_bytes: p.used_bytes,
            total_bytes: p.total_bytes,
        })
        .collect();

    let disk_summary: Vec<alerts::DiskHealthSummary> = disk_health
        .into_iter()
        .map(|d| {
            // Pre-filter the critical-attribute set here so the alert
            // evaluator stays a pure data-in / data-out function with
            // no knowledge of attribute IDs. Skip when the drive's
            // SMART is UNAVAILABLE — `attributes` is empty by
            // construction in that case, but the filter still costs
            // a closure allocation per attribute and per drive, so
            // the early-out matters when N drives × ~30 attributes.
            let critical_attrs_with_value = if d.smart_status == "UNAVAILABLE" {
                Vec::new()
            } else {
                d.attributes
                    .iter()
                    .filter_map(|a| {
                        alerts::CRITICAL_ATA_ATTRIBUTES
                            .iter()
                            .any(|(id, _)| *id == a.id)
                            .then_some((a.id, a.raw_value))
                    })
                    .collect()
            };
            alerts::DiskHealthSummary {
                device: d.device,
                transport: d.transport,
                temperature_c: d.temperature_c,
                health_passed: d.health_passed,
                smart_status: d.smart_status,
                critical_attrs_with_value,
            }
        })
        .collect();

    // Run bcachefs health checks for every mounted filesystem in parallel.
    let mut health_tasks = tokio::task::JoinSet::new();
    for fs in filesystems.iter().filter(|fs| fs.mounted) {
        let fs_service = state.filesystems.clone();
        let fs = fs.clone();
        health_tasks.spawn(async move {
            let degraded = fs.options.degraded.unwrap_or(false);
            let devices: Vec<alerts::BcachefsDeviceHealth> = fs
                .devices
                .iter()
                .map(|d| alerts::BcachefsDeviceHealth {
                    path: d.path.clone(),
                    state: d.state.clone().unwrap_or_else(|| "rw".into()),
                    has_errors: d.has_data.as_deref().is_some_and(|s| s.contains("error")),
                })
                .collect();

            let (io_error_count, scrub_result, reconcile_result) = tokio::join!(
                read_bcachefs_error_count(&fs.uuid),
                fs_service.scrub_status(&fs.name),
                fs_service.reconcile_status(&fs.name),
            );

            let scrub_errors = match scrub_result {
                Ok(s) => s.raw.to_lowercase().contains("error"),
                Err(_) => false,
            };

            let reconcile_stalled = match reconcile_result {
                // An operator-disabled reconcile is expected to sit on
                // pending work indefinitely — never a stall (#487).
                Ok(s) if s.enabled => {
                    reconcile_stall_check(&fs.name, &alerts::parse_reconcile_sample(&s.raw))
                }
                Ok(_) | Err(_) => {
                    clear_reconcile_tracker(&fs.name);
                    false
                }
            };

            alerts::BcachefsHealth {
                fs_name: fs.name.clone(),
                degraded,
                devices,
                io_error_count,
                scrub_errors,
                reconcile_stalled,
            }
        });
    }
    let mut bcachefs_health = Vec::new();
    while let Some(result) = health_tasks.join_next().await {
        if let Ok(health) = result {
            bcachefs_health.push(health);
        }
    }

    // Kernel error counters from the metrics service.
    let kernel_summary: nasty_common::metrics_types::KernelErrorSummary =
        fetch_metrics_json(&state.metrics_client, "/api/kernel_errors")
            .await
            .unwrap_or_default();
    let kernel_alert = alerts::KernelErrorAlert {
        total_count: kernel_summary.total_count,
        categories: kernel_summary
            .by_category
            .iter()
            .map(|c| c.category.clone())
            .collect(),
    };

    let mut active = state
        .alerts
        .evaluate(
            &stats,
            &fs_usage_list,
            &disk_summary,
            &bcachefs_health,
            &kernel_alert,
        )
        .await;

    // Mount failures recorded at boot stay live until the engine is
    // restarted. Enrich the alert with current state: a locked
    // encrypted FS gets a "unlock to mount" message instead of the
    // generic "check disk connectivity" hint, since the user can
    // recover from this through the WebUI without touching cables
    // or logs (issue #87). Filesystems that have since been mounted
    // by the user drop out entirely.
    let mount_failures = state.mount_failures.lock().await;
    if !mount_failures.is_empty() {
        let current_fses = match state.filesystems.list().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "mount-failure alert enrichment: filesystems.list() failed: {e} — \
                     using empty fs set, alerts may show stale state"
                );
                Vec::new()
            }
        };
        for name in mount_failures.iter() {
            let fs = current_fses.iter().find(|f| &f.name == name);
            // Already mounted (user fixed it via UI) — drop the alert.
            if fs.is_some_and(|f| f.mounted) {
                continue;
            }
            let (rule_name, severity, message) = match fs {
                Some(f) if f.options.encrypted == Some(true) => (
                    "Encrypted filesystem locked",
                    alerts::AlertSeverity::Warning,
                    format!("Filesystem \"{name}\" is encrypted and locked — unlock it to mount."),
                ),
                _ => (
                    "Filesystem failed to mount",
                    alerts::AlertSeverity::Critical,
                    format!(
                        "Filesystem \"{name}\" failed to mount after boot. Check disk connectivity and logs."
                    ),
                ),
            };
            active.push(alerts::ActiveAlert {
                rule_id: "mount-failure".into(),
                rule_name: rule_name.into(),
                severity,
                metric: alerts::AlertMetric::BcachefsDegraded,
                message,
                current_value: 1.0,
                threshold: 0.0,
                source: name.clone(),
            });
        }
    }

    active
}

/// How long reconcile must sit on *unchanged* pending counters, in a
/// non-active state, before it counts as stalled (#487). Generous on
/// purpose: bcachefs paces background work in throttled bursts with
/// long `waiting` gaps, and a heavily-loaded pool can legitimately go
/// many minutes without the counters moving.
const RECONCILE_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Per-filesystem fingerprint of the last reconcile pending counters +
/// when that fingerprint was first seen. Process-lifetime state for
/// the stall detector; an engine restart just restarts the window.
static RECONCILE_STALL_TRACKER: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
> = std::sync::LazyLock::new(Default::default);

/// Stall decision for one reconcile sample (#487): pending work exists,
/// the thread isn't actively progressing, and the pending counters
/// haven't changed for [`RECONCILE_STALL_WINDOW`]. Anything else —
/// no pending work, an active state, or counters that moved — resets
/// the window.
fn reconcile_stall_check(fs_name: &str, sample: &nasty_system::alerts::ReconcileSample) -> bool {
    let mut tracker = RECONCILE_STALL_TRACKER.lock().unwrap();
    let Some(fingerprint) = sample.pending.as_ref().filter(|_| !sample.active) else {
        tracker.remove(fs_name);
        return false;
    };
    match tracker.get(fs_name) {
        Some((prev, since)) if prev == fingerprint => since.elapsed() >= RECONCILE_STALL_WINDOW,
        _ => {
            tracker.insert(
                fs_name.to_string(),
                (fingerprint.clone(), std::time::Instant::now()),
            );
            false
        }
    }
}

fn clear_reconcile_tracker(fs_name: &str) {
    RECONCILE_STALL_TRACKER.lock().unwrap().remove(fs_name);
}

/// Read bcachefs error counters from sysfs. Returns total read+write error count.
pub(super) async fn read_bcachefs_error_count(uuid: &str) -> u64 {
    let counters_dir = format!("/sys/fs/bcachefs/{uuid}/counters");
    let mut total = 0u64;
    for name in ["io_read_errors", "io_write_errors", "io_checksum_errors"] {
        let path = format!("{counters_dir}/{name}");
        if let Ok(val) = tokio::fs::read_to_string(&path).await
            && let Ok(n) = val.trim().parse::<u64>()
        {
            total += n;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::is_read_only;

    /// Regression for the Filesystems-page refresh loop on .71:
    /// before this fix, `fs.tpm.status` was classified as a write
    /// because it didn't match `.get` / `.list` / the explicit
    /// matches!() list. The router's event bus broadcasted a
    /// "filesystem" event for every "write," the page's event
    /// handler re-ran `refresh()`, refresh() called
    /// `fs.tpm.status` again, repeat — the in-flight RPC bar
    /// blinked indefinitely.
    #[test]
    fn fs_tpm_status_is_read_only() {
        assert!(is_read_only("fs.tpm.status"));
    }

    /// Every status endpoint in the codebase is a pure read.
    /// Pinning the suffix heuristic so a new `.status` endpoint
    /// can't accidentally trigger the same refresh loop.
    #[test]
    fn status_suffix_is_read_only() {
        for m in [
            "fs.tpm.status",
            "fs.scrub.status",
            "fs.reconcile.status",
            "system.update.status",
            "system.ssh.status",
            "system.nut.status",
            "system.acme.status",
            "system.firewall.status",
            "apps.status",
        ] {
            assert!(is_read_only(m), "expected {m} to be read-only");
        }
    }

    /// Writes must still be classified as writes — the event bus
    /// depends on this to broadcast and the WebUI depends on the
    /// broadcasts to refresh after mutations. Don't accidentally
    /// over-broaden the read-only heuristic.
    #[test]
    fn writes_stay_writes() {
        for m in [
            "fs.tpm.bind",
            "fs.tpm.unbind",
            "fs.create",
            "fs.destroy",
            "fs.mount",
            "fs.unmount",
            "fs.unlock",
            "fs.lock",
            "fs.device.add",
            "fs.device.remove",
            "subvolume.create",
            "snapshot.create",
            "share.nfs.add",
            "service.protocol.enable",
            "system.settings.set",
        ] {
            assert!(!is_read_only(m), "expected {m} to be a write");
        }
    }

    /// The .list / .get suffix matches that existed before this
    /// PR continue to work — the suffix list is additive.
    #[test]
    fn list_and_get_suffixes_remain_read_only() {
        assert!(is_read_only("fs.list"));
        assert!(is_read_only("device.list"));
        assert!(is_read_only("subvolume.get"));
        assert!(is_read_only("snapshot.get"));
        assert!(is_read_only("fs.get"));
    }
}
