use std::path::Path;

use nasty_common::{ErrorCode, Request, Response};
use tracing::debug;

mod alerts;
pub(crate) mod apps;
mod audit;
mod auth;
mod backup;
mod bcachefs;
mod dc;
mod domain;
mod fs;
mod guestshare;
mod notifications;
mod service;
pub(crate) mod share;
mod smb;
mod snapshot;
mod subvolume;
mod system;
mod vm;

pub(crate) use vm::{CreateVmDiskRequest, VmDiskCandidate};

use crate::AppState;
use crate::auth::{Role, Session};

/// Methods every authenticated management role can call.
/// Standard users are evaluated separately by `is_user_allowed`.
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

/// Standard users are intentionally disconnected from suffix heuristics. A
/// new `.list` or `.get` method is denied until it is explicitly reviewed and
/// added here.
fn is_user_allowed(method: &str) -> bool {
    matches!(
        method,
        "auth.me"
            | "auth.logout"
            | "auth.change_password"
            | "auth.webauthn.config"
            | "auth.webauthn.list"
            | "auth.webauthn.register.start"
            | "auth.webauthn.register.finish"
            | "auth.webauthn.delete"
            | "audit.mine"
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
                // Guest-share management exposes capability metadata and
                // root-backed actions, so every method is operator/admin-only.
                | "guestshare.list"
                | "guestshare.get"
                | "guestshare.create"
                | "guestshare.revoke"
                | "guestshare.remove"
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
                | "share.iscsi.set_portals"
                | "share.nvmeof.create"
                | "share.nvmeof.delete"
                | "share.nvmeof.add_namespace"
                | "share.nvmeof.remove_namespace"
                | "share.nvmeof.add_port"
                | "share.nvmeof.remove_port"
                | "share.nvmeof.add_host"
                | "share.nvmeof.remove_host"
                | "vm.create"
                | "vm.disk.create"
                | "vm.disk.candidates"
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
                | "apps.install"
                | "apps.update"
                | "apps.config"
                | "apps.inspect"
                | "apps.remove"
                | "apps.stop"
                | "apps.start"
                | "apps.restart"
                | "apps.pull"
                | "apps.prune"
                | "apps.ingress.set"
                | "apps.ingress.remove"
                // Docker network management (#435, #438). Registered as
                // MethodRole::Operator since the feature shipped, but never
                // added here — same drift class as `backup.restore` below,
                // surfaced by the new `operator_role_methods_are_operator_allowed`
                // guard test. Pre-existing gap, unrelated to backup/restore;
                // fixing here because the guard test now enforces it.
                | "apps.networks.create"
                | "apps.networks.remove"
                // Data backup lifecycle is operator territory in a NAS
                // appliance — the same role manages shares and apps.
                // `router::backup` adds a content-aware Admin gate when a
                // profile reads system state outside `/fs`; otherwise an
                // Operator could send arbitrary root-readable files to a
                // repository they control.
                | "backup.profile.create"
                | "backup.profile.update"
                | "backup.profile.delete"
                | "backup.run"
                | "backup.repo.check"
                | "backup.repo.init"
                | "backup.restore"
                // Service-protocol toggles (NFS/SMB/iSCSI/NVMe-oF
                // server services + SSH/mDNS/SMART). Operators were
                // creating shares for protocols they couldn't turn
                // on — the share would land on disk but no server
                // was listening. Same coupling as share CRUD.
                | "service.protocol.enable"
                | "service.protocol.disable"
                | "alert.acknowledge"
                // VM disk-image import. Operator already has
                // `vm.create` etc.; without import they can't
                // populate the disk to boot from.
                | "vm.images.ensure"
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
    //
    // Carve-outs that must NOT match the suffix heuristics: the registry
    // declares `domain.user.list` / `domain.group.list` as Admin, but their
    // `.list` suffix would otherwise slip them into the universally-allowed
    // read set. They spawn `wbinfo` to enumerate Active Directory principals
    // (users/groups) out of the joined directory, so they are privileged
    // reads gated on Admin, not routine reads. Returning false here defers
    // enforcement to the role check (Admin only), matching the registry.
    if matches!(
        method,
        "domain.user.list"
            | "domain.group.list"
            // Same trap for DC mode: these enumerate the hosted directory
            // and are Admin-gated in the registry — the `.list` suffix must
            // not slip them into the ReadOnly set.
            | "dc.user.list"
            | "dc.group.list"
            | "dc.computer.list"
            // `auth.token.list` returns metadata for ALL API tokens
            // (declared Admin, and `list_api_tokens` self-guards on
            // Role::Admin). Its `.list` suffix would otherwise slip it
            // into the universally-allowed read set — carve it out so
            // the central gate agrees with the impl and the declared
            // role, instead of relying on the inline check alone.
            | "auth.token.list"
            // Registry metadata is Admin-only and its `.list` suffix must not
            // bypass the registry role plus the unscoped-session gate.
            | "apps.registry_credentials.list"
            // Guest-share management is not a general authenticated read.
            // The router also requires an unscoped Operator/Admin session.
            | "guestshare.list"
            | "guestshare.get"
            // `system.custom_config.get` returns the raw contents of the
            // operator's `/etc/nixos/custom.nix` — system-level NixOS config that
            // can hold sensitive settings. Its `.get` suffix would otherwise slip
            // it into the universally-allowed read set; keep it Admin-only.
            | "system.custom_config.get"
            // These return container environment, raw Docker metadata,
            // compose source, or notification endpoint details that can
            // contain credentials.
            | "apps.config"
            | "apps.inspect"
            | "apps.compose.get"
            | "notifications.config.get"
            // Global audit records contain other users' identities, client
            // addresses, denied operations, and mutation details.
            | "audit.list"
    ) {
        return false;
    }
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
                | "system.rdma.status"
                | "system.stats"
                | "system.disks"
                | "system.network.get"
                | "system.network.pending"
                | "system.network.nm_preview"
                | "system.logs.units"
                | "system.ssh.status"
                | "system.alerts"
                | "system.settings.get"
                | "system.tuning.get"
                | "system.nut.config.get"
                | "system.nut.apply_status"
                | "system.nut.status"
                | "system.watchdog.config.get"
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
                | "system.update.check"
                | "system.update.check_cached"
                | "backup.secrets_status"
                | "system.update.status"
                | "system.update.build_dir.get"
                | "system.reboot_required"
                | "system.generations.list"
                | "system.version.get"
                | "system.version.tagged_release_notice"
                | "system.log.level"
                | "system.settings.timezones"
                | "audit.mine"
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
                | "system.firewall.status"
                | "vm.capabilities"
                | "vm.images.import_info"
                | "firmware.available"
                | "firmware.constraints"
                | "firmware.devices"
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
        "vm.disk.create" => Some("subvolume"),
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
        m if m.starts_with("system.watchdog.") && !is_read_only(m) => Some("watchdog"),
        m if m.starts_with("system.tailscale.") && !is_read_only(m) => Some("tailscale"),
        m if m.starts_with("alert.") && !is_read_only(m) => Some("alert"),
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
    for key in [
        "name",
        "username",
        "filesystem",
        "target",
        "id",
        "path",
        "host_path",
    ] {
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
        Role::User => !is_user_allowed(&request.method),
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
        "audit" => audit::try_route(req, session).await,
        "alert" | "telemetry" => alerts::try_route(req, state, session).await,
        "notifications" => notifications::try_route(req, session).await,
        "backup" => backup::try_route(req, state, session).await,
        "fs" | "device" => fs::try_route(req, state, session).await,
        "bcachefs" => bcachefs::try_route(req, state, session).await,
        "subvolume" => subvolume::try_route(req, state, session).await,
        "snapshot" => snapshot::try_route(req, state, session).await,
        "share" => share::try_route(req, state, session).await,
        "guestshare" => guestshare::try_route(req, state, session).await,
        "smb" => smb::try_route(req, state, session).await,
        "domain" => domain::try_route(req, state, session).await,
        "dc" => dc::try_route(req, state, session).await,
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

fn require_endpoint_access(
    req: &Request,
    session: &Session,
    access: crate::auth::EndpointAccess,
    reason: &str,
    audit_allowed: bool,
) -> Option<Response> {
    match crate::auth::authorize_session(session, access) {
        Ok(()) => {
            if audit_allowed {
                crate::auth::audit(
                    "root_equivalent_requested",
                    &session.username,
                    session.client_ip.as_deref().unwrap_or("unknown"),
                    &format!("method={} reason={reason}", req.method),
                );
            }
            None
        }
        Err(denied) => {
            crate::auth::audit(
                "permission_denied",
                &session.username,
                session.client_ip.as_deref().unwrap_or("unknown"),
                &format!(
                    "method={} role={:?} reason={reason}",
                    req.method, session.role
                ),
            );
            Some(err(req, denied.message()))
        }
    }
}

/// Gate payloads that turn an otherwise Operator-level method into a
/// root-equivalent operation. Keep this check beside dispatch so every caller
/// gets the same scope enforcement and audit trail.
pub(super) fn require_root_equivalent(
    req: &Request,
    session: &Session,
    reason: &str,
) -> Option<Response> {
    require_endpoint_access(
        req,
        session,
        crate::auth::EndpointAccess::RootEquivalent,
        reason,
        true,
    )
}

/// Gate a sensitive read as root-equivalent without emitting the mutation-style
/// success preflight event. The dispatcher audits the successful RPC once.
pub(super) fn require_root_equivalent_read(
    req: &Request,
    session: &Session,
    reason: &str,
) -> Option<Response> {
    require_endpoint_access(
        req,
        session,
        crate::auth::EndpointAccess::RootEquivalent,
        reason,
        false,
    )
}

pub(super) fn require_unscoped_mutation(
    req: &Request,
    session: &Session,
    reason: &str,
) -> Option<Response> {
    require_endpoint_access(
        req,
        session,
        crate::auth::EndpointAccess::UnscopedMutation,
        reason,
        false,
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

/// Gate an RDMA-transport request (iSER portal, NVMe-oF rdma port) on
/// the per-box RDMA opt-in, and load the transport's kernel module on
/// the way through so a stale unload can't fail the configfs write.
pub(super) async fn require_rdma(req: &Request, module: &str) -> Option<Response> {
    if !nasty_system::rdma::enabled().await {
        return Some(Response::error(
            req.id.clone(),
            ErrorCode::InternalError,
            "RDMA transport is disabled on this box — enable RDMA on the Sharing page first (requires an RDMA-capable NIC)",
        ));
    }
    if let Err(e) = nasty_system::rdma::ensure_module(module).await {
        return Some(Response::error(req.id.clone(), ErrorCode::InternalError, e));
    }
    None
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
    let identity = match state
        .subvolumes
        .block_volume_id_for_device(device_path)
        .await
    {
        Ok(identity) => identity,
        Err(error) => return Some(error.to_string()),
    };
    // Reserve VM disks even while stopped. The shared mutation lock held by
    // callers prevents a simultaneous VM attach and protocol export.
    if exclude_protocol != "vm" {
        let managed = match state.subvolumes.list_all(None, None).await {
            Ok(v) => v,
            Err(e) => return Some(format!("cannot verify VM backing inventory: {e}")),
        };
        let candidate = managed.iter().find(|sv| {
            identity
                .as_ref()
                .is_some_and(|id| sv.block_volume_id.as_ref() == Some(id))
                || sv
                    .block_device
                    .as_deref()
                    .is_some_and(|path| vm::paths_match(path, device_path))
        });
        match state.vms.list_strict().await {
            Ok(vms) => {
                for status in vms {
                    if status.config.disks.iter().any(|disk| {
                        candidate.map_or_else(
                            || {
                                vm::paths_match(
                                    disk.source.as_deref().unwrap_or(&disk.path),
                                    device_path,
                                )
                            },
                            |sv| vm::disk_matches_subvolume(disk, sv),
                        )
                    }) {
                        return Some(format!(
                            "device {device_path} is reserved by VM '{}' (including stopped VMs)",
                            status.config.name
                        ));
                    }
                }
            }
            Err(e) => return Some(format!("cannot verify VM disk usage: {e}")),
        }
    }
    if exclude_protocol != "iscsi" {
        match state.iscsi.list().await {
            Ok(targets) => {
                for target in &targets {
                    for lun in &target.luns {
                        if lun.backstore_path == device_path
                            || identity.as_ref().is_some_and(|identity| {
                                lun.backing_volume.as_ref() == Some(identity)
                            })
                        {
                            return Some(format!(
                                "device {} is already exported via iSCSI (target '{}')",
                                device_path, target.iqn
                            ));
                        }
                    }
                }
            }
            Err(error) => {
                return Some(format!(
                    "cannot verify iSCSI device conflicts because state failed to load: {error}"
                ));
            }
        }
    }

    if exclude_protocol != "nvmeof" {
        match state.nvmeof.list().await {
            Ok(subsystems) => {
                for sub in &subsystems {
                    for ns in &sub.namespaces {
                        if ns.device_path == device_path
                            || identity.as_ref().is_some_and(|identity| {
                                ns.backing_volume.as_ref() == Some(identity)
                            })
                        {
                            return Some(format!(
                                "device {} is already exported via NVMe-oF (subsystem '{}')",
                                device_path, sub.nqn
                            ));
                        }
                    }
                }
            }
            Err(error) => {
                return Some(format!(
                    "cannot verify NVMe-oF device conflicts because state failed to load: {error}"
                ));
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

fn configured_apps_storage_role(
    config: &nasty_apps::AppsConfig,
    subvolume_path: &str,
) -> Option<&'static str> {
    if config
        .storage_path
        .as_deref()
        .is_some_and(|path| Path::new(path) == Path::new(subvolume_path))
    {
        return Some("Docker storage");
    }
    if config
        .appdata_path
        .as_deref()
        .is_some_and(|path| Path::new(path) == Path::new(subvolume_path))
    {
        return Some("application data storage");
    }
    None
}

/// Check if a subvolume is in use by Apps, a VM, or a sharing service.
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
    let block_volume_id = sv.block_volume_id.as_ref();
    let subvol_path = &sv.path;

    match nasty_apps::AppsService::load_config_strict() {
        Ok(config) => {
            if let Some(role) = configured_apps_storage_role(&config, subvol_path) {
                return Some(format!(
                    "subvolume is configured as Apps {role}. Disable Apps or move its storage first."
                ));
            }
        }
        Err(error) => {
            return Some(format!(
                "cannot verify Apps storage dependencies because state failed to load: {error}"
            ));
        }
    }
    if state.apps.active_appdata_relocation_path().await.as_deref() == Some(subvol_path) {
        return Some(
            "subvolume is the target of an active appdata relocation. Wait for the move to finish."
                .to_string(),
        );
    }

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
    }

    // Sharing state persists immutable block identities, so these checks remain
    // valid even while the backing volume is detached or loop numbers changed.
    match state.iscsi.list().await {
        Ok(targets) => {
            for target in &targets {
                for lun in &target.luns {
                    if block_device.is_some_and(|device| lun.backstore_path == device)
                        || block_volume_id
                            .is_some_and(|identity| lun.backing_volume.as_ref() == Some(identity))
                    {
                        return Some(format!(
                            "subvolume is in use by iSCSI target '{}'. Delete the target first.",
                            target.iqn
                        ));
                    }
                }
            }
        }
        Err(error) => {
            return Some(format!(
                "cannot verify iSCSI dependencies because state failed to load: {error}"
            ));
        }
    }

    match state.nvmeof.list().await {
        Ok(subsystems) => {
            for subsys in &subsystems {
                for ns in &subsys.namespaces {
                    if block_device.is_some_and(|device| ns.device_path == device)
                        || block_volume_id
                            .is_some_and(|identity| ns.backing_volume.as_ref() == Some(identity))
                    {
                        return Some(format!(
                            "subvolume is in use by NVMe-oF subsystem '{}'. Delete the subsystem first.",
                            subsys.nqn
                        ));
                    }
                }
            }
        }
        Err(error) => {
            return Some(format!(
                "cannot verify NVMe-oF dependencies because state failed to load: {error}"
            ));
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
    let subvols = match state.subvolumes.list_all(None, None).await {
        Ok(subvols) => subvols,
        Err(error) => {
            return Some(format!(
                "filesystem '{name}' cannot be destroyed: cannot verify subvolume dependencies: {error}"
            ));
        }
    };
    let fs_subvols: Vec<_> = subvols.iter().filter(|sv| sv.filesystem == name).collect();

    // Check each subvolume for dependencies
    for sv in &fs_subvols {
        if let Some(reason) = check_subvolume_in_use(state, name, &sv.name).await {
            return Some(format!(
                "filesystem '{}' cannot be destroyed: subvolume '{}' is in use — {}",
                name, sv.name, reason
            ));
        }
    }

    match nasty_apps::AppsService::load_config_strict() {
        Ok(config) => {
            let filesystem_path = Path::new("/fs").join(name);
            if [
                config.storage_path.as_deref(),
                config.appdata_path.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|path| Path::new(path).starts_with(&filesystem_path))
            {
                return Some(format!(
                    "filesystem '{name}' cannot be destroyed: configured Apps storage is on this filesystem. Disable Apps or move its storage first."
                ));
            }
            if state
                .apps
                .active_appdata_relocation_path()
                .await
                .is_some_and(|path| Path::new(&path).starts_with(&filesystem_path))
            {
                return Some(format!(
                    "filesystem '{name}' cannot be destroyed: it contains an active appdata relocation target. Wait for the move to finish."
                ));
            }
        }
        Err(error) => {
            return Some(format!(
                "filesystem '{name}' cannot be destroyed: cannot verify Apps storage dependencies: {error}"
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
    filesystem_filter: Option<&str>,
    owner_filter: Option<&str>,
) -> Result<Vec<nasty_vm::VmDiskSubvolume>, String> {
    let vm_status = state.vms.get(&req.id).await.map_err(|e| e.to_string())?;
    let disks = resolve_vm_disks(state, &vm_status.config).await;

    if disks.is_empty() {
        return Err("no block subvolumes found for this VM".to_string());
    }

    for disk in &disks {
        if filesystem_filter.is_some_and(|filesystem| filesystem != disk.filesystem) {
            return Err("access denied".to_string());
        }
        state
            .subvolumes
            .get(&disk.filesystem, &disk.subvolume, owner_filter)
            .await
            .map_err(|e| e.to_string())?;
        nasty_storage::subvolume::validate_snapshot_name(&disk.subvolume, &req.name)
            .map_err(|e| e.to_string())?;
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
        state
            .snapshots
            .create(snap_req, owner_filter)
            .await
            .map_err(|e| {
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

    let clone_names = disks
        .iter()
        .map(|disk| {
            let name = format!("{}-{}", disk.subvolume, req.new_name);
            nasty_storage::subvolume::validate_subvolume_name(&name)
                .map(|()| name)
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Clone each block subvolume
    let mut new_disks = Vec::new();
    for (disk, clone_name) in disks.iter().zip(clone_names) {
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
            // Captured from the fresh loop device by VmService::create
            // (which runs backfill_disk_sources) so the clone survives a
            // reboot the same way (#592).
            source: None,
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
) -> (Vec<nasty_system::alerts::AlertOccurrence>, u64) {
    let _guard = state.alerts.lock_evaluation().await;
    let alerts = evaluate_active_alerts_inner(state).await;
    (alerts, state.alerts.revision())
}

pub(crate) async fn acknowledge_active_alert(
    state: &AppState,
    instance_id: &str,
    username: &str,
) -> Result<nasty_system::alerts::AlertAcknowledgement, String> {
    let _guard = state.alerts.lock_evaluation().await;
    let active = evaluate_active_alerts_inner(state).await;
    if !active.iter().any(|alert| alert.instance_id == instance_id) {
        return Err("alert is no longer active".into());
    }
    state.alerts.acknowledge(instance_id, username).await
}

#[derive(Default)]
struct AlertCoverage {
    unavailable_metrics: std::collections::HashSet<nasty_system::alerts::AlertMetric>,
    unavailable_sources: std::collections::HashSet<(nasty_system::alerts::AlertMetric, String)>,
    observed_smart_attributes: std::collections::HashMap<String, std::collections::HashSet<u32>>,
}

impl AlertCoverage {
    fn mark_metric(&mut self, metric: nasty_system::alerts::AlertMetric) {
        self.unavailable_metrics.insert(metric);
    }

    fn mark_source(
        &mut self,
        metric: nasty_system::alerts::AlertMetric,
        source: impl Into<String>,
    ) {
        self.unavailable_sources.insert((metric, source.into()));
    }

    fn observe_smart_attributes(
        &mut self,
        source: String,
        attribute_ids: impl Iterator<Item = u32>,
    ) {
        self.observed_smart_attributes
            .insert(source, attribute_ids.collect());
    }

    fn is_observed(&self, alert: &nasty_system::alerts::ActiveAlert) -> bool {
        if self.unavailable_metrics.contains(&alert.metric) {
            return false;
        }
        let source = if alert.metric == nasty_system::alerts::AlertMetric::SmartAttribute {
            alert
                .source
                .rsplit_once('#')
                .map_or(alert.source.as_str(), |(device, _)| device)
        } else {
            &alert.source
        };
        if self
            .unavailable_sources
            .contains(&(alert.metric.clone(), source.to_string()))
        {
            return false;
        }
        if alert.metric == nasty_system::alerts::AlertMetric::SmartAttribute
            && let Some(observed) = self.observed_smart_attributes.get(source)
        {
            return alert
                .source
                .rsplit_once('#')
                .and_then(|(_, id)| id.parse().ok())
                .is_some_and(|id| observed.contains(&id));
        }
        true
    }
}

pub(crate) async fn evaluate_active_alerts_inner(
    state: &AppState,
) -> Vec<nasty_system::alerts::AlertOccurrence> {
    use nasty_system::alerts;

    let backup_failures: Vec<alerts::BackupFailure> = state
        .backups
        .list_profiles()
        .await
        .into_iter()
        .filter_map(|profile| {
            let last_run = profile.last_run?;
            (!last_run.success).then_some(alerts::BackupFailure {
                profile_id: profile.id,
                profile_name: profile.name,
                timestamp: last_run.timestamp,
                message: last_run.message,
            })
        })
        .collect();

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
                let active = state
                    .alerts
                    .evaluate_backup_failures(&backup_failures)
                    .await;
                return state
                    .alerts
                    .reconcile_active(active, |alert| {
                        alert.metric == alerts::AlertMetric::BackupFailure
                    })
                    .await;
            }
        };

    let mut coverage = AlertCoverage::default();

    let filesystems = match state.filesystems.list().await {
        Ok(v) => v,
        Err(e) => {
            for metric in [
                alerts::AlertMetric::FsUsagePercent,
                alerts::AlertMetric::BcachefsDegraded,
                alerts::AlertMetric::BcachefsDeviceError,
                alerts::AlertMetric::BcachefsDeviceState,
                alerts::AlertMetric::BcachefsIOErrors,
                alerts::AlertMetric::BcachefsScrubErrors,
                alerts::AlertMetric::BcachefsReconcileStalled,
            ] {
                coverage.mark_metric(metric);
            }
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
        match fetch_metrics_json(&state.metrics_client, "/api/disks").await {
            Ok(disks) => disks,
            Err(e) => {
                for metric in [
                    alerts::AlertMetric::DiskTemperature,
                    alerts::AlertMetric::SmartHealth,
                    alerts::AlertMetric::SmartAttribute,
                ] {
                    coverage.mark_metric(metric);
                }
                tracing::warn!(
                    "alert evaluation: disk metrics fetch failed: {e} — \
                     SMART alerts will retain their last known state"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    for disk in &disk_health {
        let label = match &disk.transport {
            Some(transport) => format!("{} [{}]", disk.device, transport),
            None => disk.device.clone(),
        };
        if disk.temperature_c.is_none() {
            coverage.mark_source(alerts::AlertMetric::DiskTemperature, label.clone());
        }
        if disk.smart_status == "UNAVAILABLE" {
            coverage.mark_source(alerts::AlertMetric::SmartHealth, label.clone());
            coverage.mark_source(alerts::AlertMetric::SmartAttribute, label);
        } else if disk.rotational.is_none()
            || (disk.rotational == Some(true) && disk.attributes.is_empty())
        {
            coverage.mark_source(alerts::AlertMetric::SmartAttribute, label);
        } else if disk.rotational == Some(true) {
            coverage.observe_smart_attributes(
                label,
                disk.attributes.iter().map(|attribute| attribute.id),
            );
        }
    }

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
            // Gate the critical-attribute set to spinning disks — the
            // table is HDD failure data and false-fires on SSDs (#503).
            // See alerts::collect_critical_ata_attrs.
            let critical_attrs_with_value =
                alerts::collect_critical_ata_attrs(&d.smart_status, d.rotational, &d.attributes);
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
            let mut unavailable = Vec::new();
            if !io_error_count.1 {
                unavailable.push(alerts::AlertMetric::BcachefsIOErrors);
            }
            if scrub_result.is_err() {
                unavailable.push(alerts::AlertMetric::BcachefsScrubErrors);
            }

            let scrub_errors = match scrub_result {
                Ok(s) => {
                    s.last_error_kind.is_some()
                        || matches!(
                            s.last_outcome,
                            Some(nasty_storage::filesystem::ScrubOutcome::Errors)
                        )
                }
                Err(_) => false,
            };

            let reconcile_stalled = match reconcile_result {
                // An operator-disabled reconcile is expected to sit on
                // pending work indefinitely — never a stall (#487).
                Ok(s) if s.enabled => {
                    let sample = alerts::parse_reconcile_sample(&s.raw);
                    let progress = if sample.pending.is_some() && !sample.active {
                        read_reconcile_progress(&fs.uuid).await
                    } else {
                        ReconcileProgressSample::Unavailable
                    };
                    if sample.pending.is_some()
                        && !sample.active
                        && progress == ReconcileProgressSample::Unavailable
                    {
                        unavailable.push(alerts::AlertMetric::BcachefsReconcileStalled);
                    }
                    reconcile_stall_check(&fs.name, &sample, progress)
                }
                Ok(_) => {
                    clear_reconcile_tracker(&fs.name);
                    false
                }
                Err(_) => {
                    unavailable.push(alerts::AlertMetric::BcachefsReconcileStalled);
                    clear_reconcile_tracker(&fs.name);
                    false
                }
            };

            (
                alerts::BcachefsHealth {
                    fs_name: fs.name.clone(),
                    degraded,
                    devices,
                    io_error_count: io_error_count.0,
                    scrub_errors,
                    reconcile_stalled,
                },
                unavailable,
            )
        });
    }
    let mut bcachefs_health = Vec::new();
    while let Some(result) = health_tasks.join_next().await {
        if let Ok((health, unavailable)) = result {
            for metric in unavailable {
                coverage.mark_source(metric, health.fs_name.clone());
            }
            bcachefs_health.push(health);
        } else {
            for metric in [
                alerts::AlertMetric::BcachefsDegraded,
                alerts::AlertMetric::BcachefsDeviceError,
                alerts::AlertMetric::BcachefsDeviceState,
                alerts::AlertMetric::BcachefsIOErrors,
                alerts::AlertMetric::BcachefsScrubErrors,
                alerts::AlertMetric::BcachefsReconcileStalled,
            ] {
                coverage.mark_metric(metric);
            }
        }
    }

    // Kernel error counters from the metrics service.
    let kernel_summary: nasty_common::metrics_types::KernelErrorSummary =
        match fetch_metrics_json(&state.metrics_client, "/api/kernel_errors").await {
            Ok(summary) => summary,
            Err(e) => {
                coverage.mark_metric(alerts::AlertMetric::KernelErrors);
                tracing::warn!(
                    "alert evaluation: kernel metrics fetch failed: {e} — \
                     kernel alerts will retain their last known state"
                );
                Default::default()
            }
        };
    let kernel_alert = alerts::KernelErrorAlert {
        total_count: kernel_summary.total_count,
        categories: kernel_summary
            .by_category
            .iter()
            .map(|c| c.category.clone())
            .collect(),
    };

    let mut certificates = Vec::new();
    let (certificate_statuses, certificate_inventory_complete) =
        nasty_system::settings::list_host_tls_statuses_with_coverage().await;
    if !certificate_inventory_complete {
        coverage.mark_metric(alerts::AlertMetric::CertificateExpiryDays);
        coverage.mark_metric(alerts::AlertMetric::CertificateRenewalFailure);
    }
    for status in certificate_statuses {
        // Direct-IP access uses Caddy's short-lived internal CA and is
        // intentionally excluded from public ACME expiry alerts.
        if status.host.parse::<std::net::IpAddr>().is_ok() {
            continue;
        }
        if status.expires_in_days.is_none() {
            coverage.mark_source(
                alerts::AlertMetric::CertificateExpiryDays,
                status.host.clone(),
            );
        }
        if !status.renewal_observed {
            coverage.mark_source(
                alerts::AlertMetric::CertificateRenewalFailure,
                status.host.clone(),
            );
        }
        certificates.push(alerts::CertificateHealth {
            host: status.host,
            expires_in_days: status.expires_in_days,
            renewal_error: status.renewal_error,
        });
    }

    let (mut active, unavailable_disk_metrics) = state
        .alerts
        .evaluate(
            &stats,
            &fs_usage_list,
            &disk_summary,
            &bcachefs_health,
            &kernel_alert,
            &certificates,
        )
        .await;
    active.extend(
        state
            .alerts
            .evaluate_backup_failures(&backup_failures)
            .await,
    );
    for metric in unavailable_disk_metrics {
        coverage.mark_metric(metric);
    }

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
                for name in mount_failures.iter() {
                    coverage.mark_source(alerts::AlertMetric::BcachefsDegraded, name.clone());
                }
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
    drop(mount_failures);

    state
        .alerts
        .reconcile_active(active, |alert| coverage.is_observed(alert))
        .await
}

/// How long reconcile must have pending work with no pending-counter or
/// `moving_ctxts` progress before it counts as stalled (#487, #735). Generous
/// on purpose: bcachefs paces background work in throttled bursts with long
/// `waiting` gaps, and a heavily-loaded pool can legitimately go many minutes
/// without the counters moving.
const RECONCILE_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const RECONCILE_PROGRESS_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReconcileProgress {
    keys_moved: u64,
    bytes_seen: u64,
    bytes_moved: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconcileProgressSample {
    Available(Option<ReconcileProgress>),
    Unavailable,
}

#[derive(Debug)]
struct ReconcileTrackerEntry {
    pending: String,
    progress: ReconcileProgressSample,
    unchanged_since: std::time::Instant,
}

/// Per-filesystem pending and real movement counters, plus when both were
/// last observed to change. Process-lifetime state for the stall detector;
/// an engine restart just restarts the window.
static RECONCILE_STALL_TRACKER: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, ReconcileTrackerEntry>>,
> = std::sync::LazyLock::new(Default::default);
static RECONCILE_PROGRESS_READS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(Default::default);

struct ReconcileProgressReadGuard {
    uuid: String,
}

impl ReconcileProgressReadGuard {
    fn acquire(uuid: &str) -> Option<Self> {
        let mut reads = RECONCILE_PROGRESS_READS.lock().unwrap();
        reads.insert(uuid.to_string()).then(|| Self {
            uuid: uuid.to_string(),
        })
    }
}

impl Drop for ReconcileProgressReadGuard {
    fn drop(&mut self) {
        RECONCILE_PROGRESS_READS.lock().unwrap().remove(&self.uuid);
    }
}

fn reconcile_progress(
    contexts: &[nasty_storage::filesystem::MoveCtx],
) -> Option<ReconcileProgress> {
    let mut progress = None;
    for context in contexts
        .iter()
        .filter(|context| context.kind == "reconcile")
    {
        let aggregate = progress.get_or_insert(ReconcileProgress {
            keys_moved: 0,
            bytes_seen: 0,
            bytes_moved: 0,
        });
        aggregate.keys_moved = aggregate.keys_moved.saturating_add(context.keys_moved);
        aggregate.bytes_seen = aggregate.bytes_seen.saturating_add(context.bytes_seen);
        aggregate.bytes_moved = aggregate.bytes_moved.saturating_add(context.bytes_moved);
    }
    progress
}

async fn read_reconcile_progress(uuid: &str) -> ReconcileProgressSample {
    let Some(guard) = ReconcileProgressReadGuard::acquire(uuid) else {
        return ReconcileProgressSample::Unavailable;
    };
    let path = format!("/sys/fs/bcachefs/{uuid}/internal/moving_ctxts");
    let read = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        std::fs::read_to_string(path)
    });
    match tokio::time::timeout(RECONCILE_PROGRESS_READ_TIMEOUT, read).await {
        Ok(Ok(Ok(raw))) => ReconcileProgressSample::Available(reconcile_progress(
            &nasty_storage::filesystem::parse_moving_ctxts(&raw),
        )),
        // Older kernels without moving_ctxts retain the legacy pending-only
        // detector. Other failures are unknown rather than proof of no progress.
        Ok(Ok(Err(error))) if error.kind() == std::io::ErrorKind::NotFound => {
            ReconcileProgressSample::Available(None)
        }
        Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => ReconcileProgressSample::Unavailable,
    }
}

/// Stall decision for one reconcile sample (#487, #735): pending work exists,
/// the thread isn't actively progressing, and neither the pending counters nor
/// `internal/moving_ctxts` progress has changed for the full window.
fn reconcile_stall_check(
    fs_name: &str,
    sample: &nasty_system::alerts::ReconcileSample,
    progress: ReconcileProgressSample,
) -> bool {
    reconcile_stall_check_at(
        fs_name,
        sample,
        progress,
        std::time::Instant::now(),
        RECONCILE_STALL_WINDOW,
    )
}

fn reconcile_stall_check_at(
    fs_name: &str,
    sample: &nasty_system::alerts::ReconcileSample,
    progress: ReconcileProgressSample,
    now: std::time::Instant,
    window: std::time::Duration,
) -> bool {
    let mut tracker = RECONCILE_STALL_TRACKER.lock().unwrap();
    let Some(fingerprint) = sample.pending.as_ref().filter(|_| !sample.active) else {
        tracker.remove(fs_name);
        return false;
    };
    match tracker.get_mut(fs_name) {
        Some(previous) if previous.pending != *fingerprint => {
            previous.pending = fingerprint.clone();
            previous.progress = progress;
            previous.unchanged_since = now;
            false
        }
        Some(previous) => match progress {
            ReconcileProgressSample::Unavailable => false,
            _ if previous.progress == progress => {
                now.saturating_duration_since(previous.unchanged_since) >= window
            }
            _ => {
                previous.progress = progress;
                previous.unchanged_since = now;
                false
            }
        },
        None => {
            tracker.insert(
                fs_name.to_string(),
                ReconcileTrackerEntry {
                    pending: fingerprint.clone(),
                    progress,
                    unchanged_since: now,
                },
            );
            false
        }
    }
}

fn clear_reconcile_tracker(fs_name: &str) {
    RECONCILE_STALL_TRACKER.lock().unwrap().remove(fs_name);
}

/// Read bcachefs error counters from sysfs. The completeness flag prevents a
/// partial zero from resolving an alert while retaining any confirmed errors.
pub(super) async fn read_bcachefs_error_count(uuid: &str) -> (u64, bool) {
    let counters_dir = format!("/sys/fs/bcachefs/{uuid}/counters");
    let mut total = 0u64;
    let mut complete = true;
    for name in ["io_read_errors", "io_write_errors", "io_checksum_errors"] {
        let path = format!("{counters_dir}/{name}");
        match tokio::fs::read_to_string(&path).await {
            Ok(value) => match value.trim().parse::<u64>() {
                Ok(value) => total += value,
                Err(_) => complete = false,
            },
            Err(_) => complete = false,
        }
    }
    (total, complete)
}

#[cfg(test)]
mod tests {
    use super::{
        AlertCoverage, ReconcileProgress, ReconcileProgressSample, audit_detail,
        clear_reconcile_tracker, collection_for_method, configured_apps_storage_role,
        is_operator_allowed, is_read_only, is_universally_allowed, is_user_allowed,
        reconcile_progress, reconcile_stall_check_at,
    };

    #[test]
    fn configured_apps_storage_paths_are_protected() {
        let config = nasty_apps::AppsConfig {
            enabled: true,
            storage_path: Some("/fs/tank/apps".into()),
            appdata_path: Some("/fs/tank/appdata".into()),
        };

        assert_eq!(
            configured_apps_storage_role(&config, "/fs/tank/apps"),
            Some("Docker storage")
        );
        assert_eq!(
            configured_apps_storage_role(&config, "/fs/tank/appdata"),
            Some("application data storage")
        );
        assert_eq!(
            configured_apps_storage_role(&config, "/fs/tank/unrelated"),
            None
        );
    }

    #[test]
    fn vm_disk_creation_emits_a_subvolume_event() {
        assert_eq!(collection_for_method("vm.disk.create"), Some("subvolume"));
    }

    #[test]
    fn audit_detail_includes_host_path_mutations() {
        let request: nasty_common::Request = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "apps.fix_volume_perms",
            "params": {"host_path": "/appdata/example", "uid": 1000, "gid": 1000}
        }))
        .unwrap();
        assert_eq!(audit_detail(&request), "/appdata/example");
    }

    fn smart_attribute_alert(source: &str) -> nasty_system::alerts::ActiveAlert {
        nasty_system::alerts::ActiveAlert {
            rule_id: "smart-attribute".into(),
            rule_name: "SMART attribute warning".into(),
            severity: nasty_system::alerts::AlertSeverity::Warning,
            metric: nasty_system::alerts::AlertMetric::SmartAttribute,
            message: "SMART attribute is non-zero".into(),
            current_value: 1.0,
            threshold: 0.0,
            source: source.into(),
        }
    }

    #[test]
    fn smart_attribute_resolution_requires_the_specific_attribute() {
        let mut coverage = AlertCoverage::default();
        coverage.observe_smart_attributes("/dev/sda".into(), [5, 197].into_iter());

        assert!(coverage.is_observed(&smart_attribute_alert("/dev/sda#5")));
        assert!(!coverage.is_observed(&smart_attribute_alert("/dev/sda#198")));
        assert!(coverage.is_observed(&smart_attribute_alert("/dev/sdb#198")));
    }

    #[test]
    fn reconcile_progress_uses_only_reconcile_movement_counters() {
        let contexts = nasty_storage::filesystem::parse_moving_ctxts(
            "scrub: active\n  keys moved: 999\nreconcile_work: active\n  keys moved: 11\n  bytes seen: 2K\n  bytes moved: 1K\n",
        );

        assert_eq!(
            reconcile_progress(&contexts),
            Some(ReconcileProgress {
                keys_moved: 11,
                bytes_seen: 2 * 1024,
                bytes_moved: 1024,
            })
        );
    }

    #[test]
    fn reconcile_movement_resets_stall_window_when_pending_is_unchanged() {
        let fs_name = "reconcile-movement-test";
        clear_reconcile_tracker(fs_name);
        let sample = nasty_system::alerts::ReconcileSample {
            pending: Some("pending: 900 GiB".into()),
            active: false,
        };
        let start = std::time::Instant::now();
        let window = std::time::Duration::from_secs(30 * 60);
        let initial = ReconcileProgressSample::Available(Some(ReconcileProgress {
            keys_moved: 10,
            bytes_seen: 1_000,
            bytes_moved: 500,
        }));

        assert!(!reconcile_stall_check_at(
            fs_name, &sample, initial, start, window
        ));
        assert!(reconcile_stall_check_at(
            fs_name,
            &sample,
            initial,
            start + window,
            window,
        ));

        let advanced = ReconcileProgressSample::Available(Some(ReconcileProgress {
            keys_moved: 11,
            bytes_seen: 1_000,
            bytes_moved: 500,
        }));
        assert!(!reconcile_stall_check_at(
            fs_name,
            &sample,
            advanced,
            start + window,
            window,
        ));
        assert!(reconcile_stall_check_at(
            fs_name,
            &sample,
            advanced,
            start + window + window,
            window,
        ));
        clear_reconcile_tracker(fs_name);
    }

    #[test]
    fn unavailable_reconcile_progress_does_not_reset_or_fire_stall_timer() {
        let fs_name = "reconcile-progress-unavailable-test";
        clear_reconcile_tracker(fs_name);
        let sample = nasty_system::alerts::ReconcileSample {
            pending: Some("pending: 900 GiB".into()),
            active: false,
        };
        let start = std::time::Instant::now();
        let window = std::time::Duration::from_secs(30 * 60);
        let progress = ReconcileProgressSample::Available(Some(ReconcileProgress {
            keys_moved: 10,
            bytes_seen: 1_000,
            bytes_moved: 500,
        }));

        assert!(!reconcile_stall_check_at(
            fs_name, &sample, progress, start, window
        ));
        assert!(!reconcile_stall_check_at(
            fs_name,
            &sample,
            ReconcileProgressSample::Unavailable,
            start + window,
            window,
        ));
        assert!(reconcile_stall_check_at(
            fs_name,
            &sample,
            progress,
            start + window,
            window,
        ));
        clear_reconcile_tracker(fs_name);
    }

    #[test]
    fn pending_changes_reset_timer_while_reconcile_progress_is_unavailable() {
        let fs_name = "reconcile-pending-change-unavailable-test";
        clear_reconcile_tracker(fs_name);
        let first = nasty_system::alerts::ReconcileSample {
            pending: Some("pending: 900 GiB".into()),
            active: false,
        };
        let second = nasty_system::alerts::ReconcileSample {
            pending: Some("pending: 800 GiB".into()),
            active: false,
        };
        let start = std::time::Instant::now();
        let window = std::time::Duration::from_secs(30 * 60);
        let progress = ReconcileProgressSample::Available(Some(ReconcileProgress {
            keys_moved: 10,
            bytes_seen: 1_000,
            bytes_moved: 500,
        }));

        assert!(!reconcile_stall_check_at(
            fs_name, &first, progress, start, window
        ));
        assert!(!reconcile_stall_check_at(
            fs_name,
            &second,
            ReconcileProgressSample::Unavailable,
            start + window,
            window,
        ));
        assert!(!reconcile_stall_check_at(
            fs_name,
            &second,
            progress,
            start + window,
            window,
        ));
        clear_reconcile_tracker(fs_name);
    }

    #[test]
    fn missing_moving_ctxts_falls_back_to_pending_only_stall_detection() {
        let fs_name = "reconcile-progress-unsupported-test";
        clear_reconcile_tracker(fs_name);
        let sample = nasty_system::alerts::ReconcileSample {
            pending: Some("pending: 900 GiB".into()),
            active: false,
        };
        let start = std::time::Instant::now();
        let window = std::time::Duration::from_secs(30 * 60);
        let unsupported = ReconcileProgressSample::Available(None);

        assert!(!reconcile_stall_check_at(
            fs_name,
            &sample,
            unsupported,
            start,
            window,
        ));
        assert!(reconcile_stall_check_at(
            fs_name,
            &sample,
            unsupported,
            start + window,
            window,
        ));
        clear_reconcile_tracker(fs_name);
    }

    #[test]
    fn standard_user_rpc_policy_is_explicit_and_deny_by_default() {
        for method in [
            "auth.me",
            "auth.logout",
            "auth.change_password",
            "auth.webauthn.config",
            "auth.webauthn.list",
            "auth.webauthn.register.start",
            "auth.webauthn.register.finish",
            "auth.webauthn.delete",
            "audit.mine",
        ] {
            assert!(is_user_allowed(method), "expected {method} to be allowed");
        }
        for method in [
            "audit.list",
            "auth.list_users",
            "fs.list",
            "share.smb.list",
            "system.network.pending",
            "system.network.nm_preview",
            "new.feature.list",
            "auth.token.create",
            "auth.webauthn.reset_for_user",
        ] {
            assert!(!is_user_allowed(method), "expected {method} to be denied");
        }
    }

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

    #[test]
    fn scrub_schedule_role_classification_matches_api_semantics() {
        assert!(is_read_only("fs.scrub.schedule.get"));
        assert!(!is_read_only("fs.scrub.schedule.update"));
    }

    #[test]
    fn network_inspection_methods_are_management_reads() {
        for method in ["system.network.pending", "system.network.nm_preview"] {
            assert!(is_read_only(method));
            assert!(is_universally_allowed(method));
            assert!(is_operator_allowed(method));
            assert!(!is_user_allowed(method));
        }

        for method in [
            "system.network.update",
            "system.network.confirm",
            "system.network.nm_apply",
        ] {
            assert!(!is_read_only(method));
            assert!(!is_operator_allowed(method));
            assert!(!is_user_allowed(method));
        }
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
            "device.set_io_scheduler",
            "fs.create",
            "fs.destroy",
            "fs.forget",
            "fs.mount",
            "fs.unmount",
            "fs.unlock",
            "fs.lock",
            "fs.scrub.schedule.update",
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

    #[test]
    fn registry_credential_metadata_is_not_a_suffix_based_public_read() {
        assert!(!is_read_only("apps.registry_credentials.list"));
        assert!(!is_operator_allowed("apps.registry_credentials.list"));
    }

    /// Domain principal enumeration is Admin-gated in the registry, so it
    /// must NOT be classified as a routine read despite its `.list` suffix
    /// — otherwise ReadOnly/Operator users could enumerate directory
    /// principals via wbinfo. Enforcement defers to the Admin role check.
    #[test]
    fn domain_principal_search_is_not_read_only() {
        assert!(!is_read_only("domain.user.list"));
        assert!(!is_read_only("domain.group.list"));
        assert!(!is_read_only("dc.user.list"));
        assert!(!is_read_only("dc.group.list"));
        assert!(!is_read_only("dc.computer.list"));
        assert!(is_read_only("dc.status")); // status is a safe read
    }

    #[test]
    fn custom_config_contents_are_admin_only() {
        assert!(!is_read_only("system.custom_config.get"));
        assert!(!is_operator_allowed("system.custom_config.get"));
    }

    #[test]
    fn guest_share_management_is_operator_only() {
        for method in [
            "guestshare.list",
            "guestshare.get",
            "guestshare.create",
            "guestshare.revoke",
            "guestshare.remove",
        ] {
            assert!(!is_read_only(method));
            assert!(is_operator_allowed(method));
        }
    }

    #[test]
    fn secret_bearing_app_reads_require_management_roles() {
        for method in ["apps.config", "apps.inspect"] {
            assert!(!is_read_only(method));
            assert!(is_operator_allowed(method));
        }
        assert!(!is_read_only("apps.compose.get"));
        assert!(!is_operator_allowed("apps.compose.get"));
    }

    #[test]
    fn system_journal_contents_are_admin_only_but_unit_metadata_is_readable() {
        assert!(!is_read_only("system.logs"));
        assert!(!is_operator_allowed("system.logs"));
        assert!(is_read_only("system.logs.units"));
        assert!(is_universally_allowed("system.logs.units"));
    }

    #[test]
    fn firmware_mutations_are_admin_only() {
        for method in ["firmware.check", "firmware.update"] {
            assert!(!is_read_only(method));
            assert!(!is_universally_allowed(method));
            assert!(!is_operator_allowed(method));
        }
    }

    #[test]
    fn iscsi_portal_mutations_are_operator_allowed() {
        for method in [
            "share.iscsi.add_portal",
            "share.iscsi.remove_portal",
            "share.iscsi.set_portals",
        ] {
            assert!(!is_read_only(method));
            assert!(is_operator_allowed(method));
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

    /// Registry↔allowlist consistency guard.
    ///
    /// The registry's `role: MethodRole::Operator` on a method is a
    /// *declaration* of intent — it doesn't enforce anything by itself.
    /// Enforcement happens here, in `is_operator_allowed`'s hand-maintained
    /// `matches!` list. `backup.restore` shipped with `role:
    /// MethodRole::Operator` in the registry but was never added to this
    /// allowlist, so Operators got "Permission denied" and only Admins
    /// could restore — the registry and the enforcement path silently
    /// drifted apart. This test closes that gap for every method, not
    /// just backup.restore: any future method registered as Operator-role
    /// that's missing from `is_operator_allowed` now fails CI instead of
    /// shipping.
    #[test]
    fn operator_role_methods_are_operator_allowed() {
        use crate::registry::{MethodRole, build_full_registry};

        let (_g, groups) = build_full_registry();
        let mut missing = Vec::new();
        for (_, methods) in &groups {
            for m in methods {
                if m.role == MethodRole::Operator && !is_operator_allowed(m.name) {
                    missing.push(m.name);
                }
            }
        }
        assert!(
            missing.is_empty(),
            "methods registered as MethodRole::Operator but missing from \
             is_operator_allowed's allowlist: {missing:?}"
        );
    }

    /// Full bidirectional guard: for EVERY registered method, the role the
    /// central gate actually enforces must equal the role the registry
    /// declares. `MethodRole` is only documentation/OpenAPI metadata — the
    /// real gate is `is_universally_allowed` / `is_operator_allowed` (see
    /// the `denied` match in the dispatcher). They drift silently: a `.list`
    /// suffix can slip an Admin method into the read set (escalation), an
    /// allowlist entry can outlive its declared role, and a status method
    /// that doesn't end in `.status` can lock out the role that's supposed
    /// to call it. This asserts the two agree in both directions for all
    /// methods, so any future drift fails here instead of shipping.
    ///
    /// Methods whose *arm* adds its own inline role check (e.g.
    /// `auth.token.list` self-guards on Admin) must still line the central
    /// gate up with the declared role — defense in depth, not a substitute —
    /// so they are carved into `is_read_only` rather than exempted here.
    #[test]
    fn declared_roles_match_effective_gate() {
        use crate::registry::{MethodRole, build_full_registry};

        // The minimum role the central gate actually lets through.
        fn effective(method: &str) -> MethodRole {
            if is_universally_allowed(method) {
                MethodRole::Any
            } else if is_operator_allowed(method) {
                MethodRole::Operator
            } else {
                MethodRole::Admin
            }
        }

        let (_g, groups) = build_full_registry();
        let mut mismatches = Vec::new();
        for (_, methods) in &groups {
            for m in methods {
                let eff = effective(m.name);
                if eff != m.role {
                    mismatches.push(format!(
                        "{} declared {:?} but the gate enforces {:?}",
                        m.name, m.role, eff
                    ));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "registry role declarations diverge from the enforced gate \
             (fix the declaration or the allowlist/read-set so they agree):\n{}",
            mismatches.join("\n")
        );
    }
}
