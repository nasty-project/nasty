//! RPC arms in the `fs.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
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
                    if let Some(reason) = check_filesystem_in_use(state, &p.name).await {
                        err(req, reason)
                    } else {
                        match state.filesystems.destroy(p).await {
                            Ok(()) => ok(req, "ok"),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "fs.mount" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.mount(name).await {
                Ok(v) => {
                    // Cascade: restore block devices on this filesystem
                    let _ = state.subvolumes.restore_block_devices().await;
                    ok(req, v)
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.unmount" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.unmount(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.unlock" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let passphrase = p.get("passphrase").and_then(|v| v.as_str()).unwrap_or("");
                match state.filesystems.unlock(name, passphrase).await {
                    Ok(fs) => ok(req, fs),
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
        "fs.locked_dependents" => ok(
            req,
            crate::fs_dependents::find_locked_dependents(state).await,
        ),
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
        "device.list" => match state.filesystems.list_devices().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "device.wipe" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                let path = p
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match state.filesystems.device_wipe(&path).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                }
            }
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
        "fs.device.remove" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_remove(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
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
        _ => return None,
    })
}
