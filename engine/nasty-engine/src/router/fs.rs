//! RPC arms in the `fs.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

fn filesystem_param(method: &str) -> Option<&'static str> {
    match method {
        "fs.device.add"
        | "fs.device.remove"
        | "fs.device.remove.status"
        | "fs.device.evacuate"
        | "fs.device.evacuate.cancel"
        | "fs.device.set_state"
        | "fs.device.set_label"
        | "fs.device.online"
        | "fs.device.offline" => Some("filesystem"),
        "fs.get"
        | "fs.destroy"
        | "fs.forget"
        | "fs.mount"
        | "fs.unmount"
        | "fs.unlock"
        | "fs.lock"
        | "fs.dependents"
        | "fs.key.export"
        | "fs.key.delete"
        | "fs.tpm.status"
        | "fs.tpm.bind"
        | "fs.tpm.unbind"
        | "fs.options.update"
        | "fs.usage"
        | "fs.scrub.start"
        | "fs.scrub.status"
        | "fs.scrub.cancel"
        | "fs.scrub.schedule.get"
        | "fs.scrub.schedule.update"
        | "fs.fsck.start"
        | "fs.fsck.status"
        | "fs.reconcile.status"
        | "fs.reconcile.enable"
        | "fs.reconcile.disable"
        | "fs.copygc.enable"
        | "fs.copygc.disable" => Some("name"),
        _ => None,
    }
}

fn requires_root_equivalent(method: &str) -> bool {
    matches!(
        method,
        "fs.create"
            | "device.wipe"
            | "device.prepare.inspect"
            | "device.set_type"
            | "device.set_io_scheduler"
    )
}

fn filesystem_scope_denied(scope: Option<&str>, requested: Option<&str>) -> bool {
    matches!((scope, requested), (Some(scope), Some(requested)) if requested != scope)
}

fn owner_scoped_fs_read(method: &str) -> bool {
    matches!(
        method,
        "fs.list"
            | "fs.unavailable.list"
            | "fs.get"
            | "fs.dependents"
            | "fs.locked_dependents"
            | "fs.usage"
            | "fs.scrub.status"
            | "fs.scrub.schedule.get"
            | "fs.fsck.status"
            | "fs.reconcile.status"
            | "fs.tpm.status"
            | "fs.device.remove.status"
    )
}

fn scoped_inventory_access_error(method: &str, session: &Session) -> Option<&'static str> {
    let scoped = session.filesystem.is_some() || session.owner.is_some();
    ((method == "device.list" && scoped)
        || (session.owner.is_some() && owner_scoped_fs_read(method)))
    .then_some("access denied: scoped credentials cannot read global storage inventory")
}

async fn require_block_share_recovery_access(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    for protocol in [
        nasty_system::protocol::Protocol::Iscsi,
        nasty_system::protocol::Protocol::Nvmeof,
    ] {
        if !state.protocols.is_enabled(protocol).await {
            continue;
        }
        match super::service::protocol_has_admin_only_sources(state, protocol).await {
            Ok(true) => {
                return require_root_equivalent(req, session, "raw_block_share_recovery");
            }
            Ok(false) => {}
            Err(error) => return Some(err(req, error)),
        }
    }
    None
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    if let Some(message) = scoped_inventory_access_error(&req.method, session) {
        return Some(err(req, message));
    }
    if requires_root_equivalent(&req.method)
        && let Some(response) = require_root_equivalent(req, session, "global_storage_mutation")
    {
        return Some(response);
    }
    if let Some(param) = filesystem_param(&req.method)
        && filesystem_scope_denied(session.filesystem.as_deref(), str_param(req, param))
    {
        return Some(err(req, "access denied"));
    }

    Some(match req.method.as_str() {
        "fs.list" => match state.filesystems.list().await {
            Ok(mut v) => {
                if let Some(ref fs_name) = session.filesystem {
                    v.retain(|p| &p.name == fs_name);
                }
                ok(req, v)
            }
            Err(e) => err(req, e),
        },
        "fs.unavailable.list" => match state.filesystems.list_unavailable().await {
            Ok(mut filesystems) => {
                if let Some(ref fs_name) = session.filesystem {
                    filesystems.retain(|filesystem| &filesystem.name == fs_name);
                }
                ok(req, filesystems)
            }
            Err(error) => err(req, error),
        },
        "fs.get" => match require_str(req, "name") {
            Ok(name) => {
                if session.filesystem.as_deref().is_some_and(|p| p != name) {
                    err(req, "access denied")
                } else {
                    match state.filesystems.get(name).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "fs.create" => match parse_params(req) {
            Ok(p) => match state.filesystems.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.destroy" => {
            match parse_params::<nasty_storage::filesystem::DestroyFilesystemRequest>(req) {
                Ok(p) => {
                    let _apps_config_guard = state.apps.storage_config_guard().await;
                    if let Some(reason) = check_filesystem_in_use(state, &p.name).await {
                        err(req, reason)
                    } else {
                        let uuid = match state.filesystems.get(&p.name).await {
                            Ok(filesystem) => filesystem.uuid,
                            Err(error) => return Some(err(req, error)),
                        };
                        match state.filesystems.destroy(p).await {
                            Ok(()) => {
                                if let Err(error) = state.scrub_schedules.remove(&uuid).await {
                                    tracing::warn!(%uuid, "Failed to remove scrub schedule after filesystem destroy: {error}");
                                }
                                ok(req, "ok")
                            }
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "fs.forget" => {
            match parse_params::<nasty_storage::filesystem::ForgetUnavailableRequest>(req) {
                Ok(request) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|filesystem| filesystem != request.name)
                    {
                        err(req, "access denied")
                    } else {
                        let dependents = crate::fs_dependents::find_dependents_with_uuid(
                            state,
                            &request.name,
                            Some(&request.expected_uuid),
                        )
                        .await;
                        if !dependents.state_errors.is_empty() {
                            err(
                                req,
                                format!(
                                    "cannot verify filesystem dependencies: {}",
                                    dependents.state_errors.join("; ")
                                ),
                            )
                        } else if dependents.has_dependents() {
                            err(
                                req,
                                format!(
                                    "filesystem '{}' still has dependent state; remove its dependents before forgetting it",
                                    request.name
                                ),
                            )
                        } else {
                            let name = request.name.clone();
                            let uuid = request.expected_uuid.clone();
                            match state.filesystems.forget_unavailable(request).await {
                                Ok(()) => {
                                    if let Err(error) = state.scrub_schedules.remove(&uuid).await {
                                        tracing::warn!(%uuid, "Failed to remove scrub schedule after forgetting filesystem: {error}");
                                    }
                                    state
                                        .mount_failures
                                        .lock()
                                        .await
                                        .retain(|failed_name| failed_name != &name);
                                    *state.alerts_cache.lock().await = None;
                                    *state.status_cache.lock().await = None;
                                    ok(req, "ok")
                                }
                                Err(error) => err(req, error),
                            }
                        }
                    }
                }
                Err(error) => invalid(req, error),
            }
        }
        "fs.mount" => {
            #[derive(Deserialize)]
            struct MountParams {
                name: String,
                /// Force a degraded mount (bring the pool up without a
                /// missing member). Surfaced by the #451 failure banner.
                #[serde(default)]
                degraded: bool,
            }
            if let Some(response) = require_block_share_recovery_access(req, state, session).await {
                return Some(response);
            }
            match parse_params::<MountParams>(req) {
                Ok(p) => match state
                    .filesystems
                    .mount_maybe_degraded(&p.name, p.degraded)
                    .await
                {
                    Ok(v) => match reconcile_block_shares(state).await {
                        Ok(()) => ok(req, v),
                        Err(error) => err(
                            req,
                            format!("filesystem mounted but block-share recovery failed: {error}"),
                        ),
                    },
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "fs.unmount" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.unmount(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.unlock" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                if let Some(response) =
                    require_block_share_recovery_access(req, state, session).await
                {
                    return Some(response);
                }
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let passphrase = p.get("passphrase").and_then(|v| v.as_str()).unwrap_or("");
                match state.filesystems.unlock(name, passphrase).await {
                    Ok(fs) => match reconcile_block_shares(state).await {
                        Ok(()) => ok(req, fs),
                        Err(error) => err(
                            req,
                            format!("filesystem unlocked but block-share recovery failed: {error}"),
                        ),
                    },
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "fs.lock" => match require_str(req, "name") {
            Ok(name) => match crate::fs_lock::lock_with_dependents(state, name).await {
                Ok(fs) => ok(req, fs),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.dependents" => match require_str(req, "name") {
            Ok(name) => ok(
                req,
                crate::fs_dependents::find_dependents(state, name).await,
            ),
            Err(r) => r,
        },
        "fs.locked_dependents" => {
            let mut dependents = crate::fs_dependents::find_locked_dependents(state).await;
            if let Some(filesystem) = session.filesystem.as_deref() {
                dependents.retain(|entry| entry.filesystem == filesystem);
            }
            ok(req, dependents)
        }
        "fs.key.export" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.export_key(name).await {
                Ok(key) => ok(req, key),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.key.delete" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.delete_key(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.tpm.status" => match require_str(req, "name") {
            Ok(name) => ok(req, state.filesystems.tpm_status(name).await),
            Err(r) => r,
        },
        "fs.tpm.bind" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.tpm_bind(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.tpm.unbind" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.tpm_unbind(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "device.list" => {
            // SMART is a cached enhancement, not a prerequisite for the
            // storage inventory. Keep device.list responsive if metrics is
            // starting or unavailable.
            let health = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                fetch_metrics_json::<Vec<nasty_system::DiskHealth>>(
                    &state.metrics_client,
                    "/api/disks",
                ),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
            match state.filesystems.list_devices_with_health(&health).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "device.prepare.inspect" => {
            match parse_params::<nasty_storage::filesystem::DiskInspectRequest>(req) {
                Ok(params) => match state.filesystems.inspect_disks(&params.paths).await {
                    Ok(disks) => ok(req, disks),
                    Err(error) => err(req, error),
                },
                Err(error) => invalid(req, error),
            }
        }
        "device.set_type" => match parse_params::<nasty_storage::disk_type::DiskTypeUpdate>(req) {
            Ok(u) => match nasty_storage::disk_type::set(u).await {
                Ok(key) => ok(req, serde_json::json!({ "stable_id": key })),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "device.set_io_scheduler" => {
            match parse_params::<nasty_storage::io_scheduler::IoSchedulerUpdate>(req) {
                Ok(update) => match nasty_storage::io_scheduler::set(update).await {
                    Ok(result) => ok(req, result),
                    Err(error) => err(req, error),
                },
                Err(error) => invalid(req, error),
            }
        }
        "device.wipe" => match parse_params::<nasty_storage::filesystem::DeviceWipeRequest>(req) {
            Ok(request) => match state.filesystems.device_wipe(request).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.options.update" => match parse_params(req) {
            Ok(p) => match state.filesystems.update_options(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.add" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_add(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.remove" => {
            match parse_params::<nasty_storage::filesystem::DeviceActionRequest>(req) {
                Ok(p) => match state.filesystems.device_remove_preflight(&p).await {
                    Err(e) => err(req, e),
                    Ok(()) => match state
                        .jobs
                        .start(DEVICE_REMOVE_JOB, &p.filesystem, &p.device)
                    {
                        Ok(job) => {
                            spawn_device_remove(state, session, job.id.clone(), p);
                            ok(req, job)
                        }
                        Err(running) => err(
                            req,
                            format!("removal of {} is already running", running.target),
                        ),
                    },
                },
                Err(e) => invalid(req, e),
            }
        }
        "fs.device.remove.status" => match require_str(req, "filesystem") {
            Ok(filesystem) => {
                let jobs: Vec<_> = state
                    .jobs
                    .list()
                    .into_iter()
                    .filter(|j| j.kind == DEVICE_REMOVE_JOB && j.filesystem == filesystem)
                    .collect();
                ok(req, jobs)
            }
            Err(r) => r,
        },
        "fs.device.evacuate" => {
            match parse_params::<nasty_storage::filesystem::DeviceActionRequest>(req) {
                Ok(p) => {
                    // Validate synchronously before returning
                    match state.filesystems.get(&p.filesystem).await {
                        Err(e) => err(req, e),
                        Ok(fs) if !fs.mounted => err(
                            req,
                            nasty_storage::FilesystemError::CommandFailed(
                                "filesystem must be mounted to evacuate a device".into(),
                            ),
                        ),
                        Ok(_) => {
                            // Run in background — bcachefs evacuate can take many minutes.
                            // Emit filesystem events every 3 s so UI shows live device state.
                            let fs_svc = state.filesystems.clone();
                            let events = state.events.clone();
                            tokio::spawn(async move {
                                let poll_events = events.clone();
                                let poll = tokio::spawn(async move {
                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                        let _ = poll_events.send("filesystem".to_string());
                                    }
                                });
                                let _ = fs_svc.device_evacuate(p).await;
                                poll.abort();
                                let _ = events.send("filesystem".to_string());
                            });
                            ok(req, serde_json::json!({"status": "started"}))
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "fs.device.set_state" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_set_state(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.online" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_online(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.offline" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_offline(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.set_label" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_set_label(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.usage" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.usage(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.scrub.start" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.scrub_start(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.scrub.status" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.scrub_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.scrub.schedule.get" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.get(name).await {
                Ok(filesystem) => match state.scrub_schedules.get(name, &filesystem.uuid).await {
                    Ok(status) => ok(req, status),
                    Err(error) => err(req, error),
                },
                Err(error) => err(req, error),
            },
            Err(response) => response,
        },
        "fs.scrub.schedule.update" => {
            match parse_params::<nasty_storage::scrub_scheduler::ScrubScheduleUpdate>(req) {
                Ok(update) => match state
                    .scrub_schedules
                    .update_for_filesystem(&state.filesystems, update)
                    .await
                {
                    Ok(status) => ok(req, status),
                    Err(error @ nasty_storage::scrub_scheduler::ScrubScheduleError::Invalid(_)) => {
                        invalid(req, error)
                    }
                    Err(error) => err(req, error),
                },
                Err(error) => invalid(req, error),
            }
        }
        "fs.fsck.start" => {
            #[derive(Deserialize)]
            struct FsckParams {
                name: String,
                /// `false` (default) = read-only dry run; `true` = auto-repair.
                #[serde(default)]
                repair: bool,
            }
            match parse_params::<FsckParams>(req) {
                Ok(p) => match state.filesystems.fsck_start(&p.name, p.repair).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "fs.fsck.status" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.fsck_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.reconcile.status" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.reconcile_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.reconcile.enable" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.set_reconcile_enabled(name, true).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.reconcile.disable" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.set_reconcile_enabled(name, false).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.copygc.enable" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.set_copygc_enabled(name, true).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.copygc.disable" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.set_copygc_enabled(name, false).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.scrub.cancel" => {
            match parse_params::<nasty_storage::filesystem::ScrubCancelRequest>(req) {
                Ok(p) => match state
                    .filesystems
                    .scrub_cancel(&p.name, p.run_id.as_deref())
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "fs.device.evacuate.cancel" => {
            match parse_params::<nasty_storage::filesystem::DeviceActionRequest>(req) {
                // Also stops a removal draining the device; mark its job first
                // so the killed process is recorded as cancelled, not failed.
                Ok(p) => {
                    state.jobs.request_cancel(DEVICE_REMOVE_JOB, &p.device);
                    match state
                        .filesystems
                        .device_evacuate_cancel(&p.filesystem, &p.device)
                        .await
                    {
                        Ok(()) => ok(req, "ok"),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        _ => return None,
    })
}

pub(crate) const DEVICE_REMOVE_JOB: &str = "device.remove";

/// Run a device removal to completion in the background (#1032): bcachefs
/// migrates the data off first, which can take hours. The outcome lands in
/// the job, the journal and the audit log.
fn spawn_device_remove(
    state: &AppState,
    session: &Session,
    job_id: String,
    req: nasty_storage::filesystem::DeviceActionRequest,
) {
    use nasty_common::jobs::JobState;
    let filesystems = state.filesystems.clone();
    let jobs = state.jobs.clone();
    let events = state.events.clone();
    let user = session.username.clone();
    let ip = session
        .client_ip
        .clone()
        .unwrap_or_else(|| "unknown".into());
    tokio::spawn(async move {
        let (outcome, error) = match filesystems.device_remove(&req).await {
            Ok(()) => (JobState::Succeeded, None),
            Err(nasty_storage::DeviceRemoveError::Refused(e)) => (JobState::Refused, Some(e)),
            Err(nasty_storage::DeviceRemoveError::Failed(e)) => (JobState::Failed, Some(e)),
        };
        let Some(job) = jobs.finish(&job_id, outcome, error) else {
            return;
        };
        let outcome = serde_json::to_value(job.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let detail = match &job.error {
            Some(e) => format!("{} {}: {outcome}: {e}", req.filesystem, req.device),
            None => format!("{} {}: {outcome}", req.filesystem, req.device),
        };
        if job.state == JobState::Succeeded {
            tracing::info!("Device removal finished: {detail}");
        } else {
            tracing::warn!("Device removal finished: {detail}");
        }
        crate::auth::audit("fs.device.remove.finished", &user, &ip, &detail);
        let _ = events.send("filesystem".to_string());
    });
}

pub(crate) async fn reconcile_block_shares(state: &AppState) -> Result<(), String> {
    let _guard = state.block_share_mutation.lock().await;
    reconcile_block_shares_under_lock(state).await
}

pub(crate) async fn reconcile_block_shares_under_lock(state: &AppState) -> Result<(), String> {
    let mut failures = Vec::new();
    let mappings = state.subvolumes.restore_block_devices().await;
    let nvmeof = state.nvmeof.remap_device_paths(&mappings).await;
    let iscsi = state.iscsi.remap_device_paths(&mappings).await;
    if !iscsi.safe_to_restore {
        if let Err(error) = state
            .protocols
            .quiesce(nasty_system::protocol::Protocol::Iscsi)
            .await
        {
            tracing::error!("Failed to quiesce unsafe iSCSI state: {error}");
            failures.push(format!("quiesce unsafe iSCSI state: {error}"));
        }
        let _ = state
            .firewall
            .close(nasty_system::protocol::Protocol::Iscsi)
            .await;
    } else if state
        .protocols
        .is_enabled(nasty_system::protocol::Protocol::Iscsi)
        .await
        && (iscsi.changed
            || !state
                .protocols
                .is_running(nasty_system::protocol::Protocol::Iscsi)
                .await)
    {
        let result = match state
            .protocols
            .quiesce(nasty_system::protocol::Protocol::Iscsi)
            .await
        {
            Ok(()) => match state
                .firewall
                .open(nasty_system::protocol::Protocol::Iscsi)
                .await
            {
                Ok(()) => state.protocols.enable("iscsi").await.map(|_| ()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!("Failed to reactivate iSCSI after mount: {error}");
            failures.push(format!("reactivate iSCSI: {error}"));
            let _ = state
                .firewall
                .close(nasty_system::protocol::Protocol::Iscsi)
                .await;
        }
    }
    if !nvmeof.safe_to_restore {
        if let Err(error) = state.nvmeof.quiesce().await {
            tracing::error!("Failed to quiesce unsafe NVMe-oF state: {error}");
            failures.push(format!("quiesce unsafe NVMe-oF state: {error}"));
        }
        let _ = state
            .firewall
            .close(nasty_system::protocol::Protocol::Nvmeof)
            .await;
    } else if state
        .protocols
        .is_enabled(nasty_system::protocol::Protocol::Nvmeof)
        .await
        && (nvmeof.changed || !state.nvmeof.is_active().await.unwrap_or(false))
    {
        if let Err(error) = state
            .firewall
            .open(nasty_system::protocol::Protocol::Nvmeof)
            .await
        {
            tracing::warn!("Failed to reopen NVMe-oF firewall after mount: {error}");
            failures.push(format!("reopen NVMe-oF firewall: {error}"));
        } else if let Err(error) = state.protocols.enable("nvmeof").await {
            tracing::warn!("Failed to reactivate NVMe-oF after mount: {error}");
            failures.push(format!("reactivate NVMe-oF: {error}"));
        } else if let Err(error) = state.nvmeof.restore().await {
            tracing::warn!("Failed to restore NVMe-oF after mount: {error}");
            failures.push(format!("restore NVMe-oF: {error}"));
            if let Err(error) = state.nvmeof.quiesce().await {
                tracing::error!("Failed to quiesce partial NVMe-oF restore: {error}");
            }
            let _ = state
                .firewall
                .close(nasty_system::protocol::Protocol::Nvmeof)
                .await;
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        filesystem_param, filesystem_scope_denied, owner_scoped_fs_read, requires_root_equivalent,
        scoped_inventory_access_error,
    };
    use crate::auth::{Role, Session};

    fn session(filesystem: bool, owner: bool) -> Session {
        Session {
            token: "token".into(),
            username: "user".into(),
            role: Role::ReadOnly,
            file_principal: None,
            filesystem: filesystem.then(|| "tank".into()),
            owner: owner.then(|| "token-a".into()),
            created_at: 0,
            must_change_password: false,
            client_ip: None,
        }
    }

    #[test]
    fn filesystem_operations_identify_and_enforce_their_scope_parameter() {
        assert_eq!(filesystem_param("fs.destroy"), Some("name"));
        assert_eq!(filesystem_param("fs.key.export"), Some("name"));
        assert_eq!(filesystem_param("fs.scrub.schedule.get"), Some("name"));
        assert_eq!(filesystem_param("fs.scrub.schedule.update"), Some("name"));
        assert_eq!(filesystem_param("fs.device.remove"), Some("filesystem"));
        assert_eq!(filesystem_param("device.wipe"), None);
        assert!(!filesystem_scope_denied(Some("tank"), Some("tank")));
        assert!(filesystem_scope_denied(Some("tank"), Some("other")));
        assert!(!filesystem_scope_denied(None, Some("other")));
    }

    #[test]
    fn global_storage_mutations_require_root_equivalent_access() {
        for method in [
            "fs.create",
            "device.wipe",
            "device.prepare.inspect",
            "device.set_type",
            "device.set_io_scheduler",
        ] {
            assert!(requires_root_equivalent(method), "{method}");
        }
        assert!(!requires_root_equivalent("fs.destroy"));
    }

    #[test]
    fn storage_inventory_reads_fail_closed_when_the_scope_cannot_be_applied() {
        let unscoped = session(false, false);
        let filesystem_scoped = session(true, false);
        let owner_scoped = session(false, true);

        assert!(scoped_inventory_access_error("fs.list", &unscoped).is_none());
        assert!(scoped_inventory_access_error("fs.list", &filesystem_scoped).is_none());
        for method in [
            "fs.list",
            "fs.unavailable.list",
            "fs.get",
            "fs.dependents",
            "fs.locked_dependents",
            "fs.usage",
            "fs.scrub.status",
            "fs.scrub.schedule.get",
            "fs.fsck.status",
            "fs.reconcile.status",
            "fs.tpm.status",
            "fs.device.remove.status",
        ] {
            assert!(owner_scoped_fs_read(method), "{method}");
            assert!(
                scoped_inventory_access_error(method, &owner_scoped).is_some(),
                "{method}"
            );
        }
        assert!(!owner_scoped_fs_read("fs.create"));
        assert!(scoped_inventory_access_error("device.list", &filesystem_scoped).is_some());
        assert!(scoped_inventory_access_error("device.list", &owner_scoped).is_some());
    }
}
