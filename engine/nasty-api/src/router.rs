use serde::Deserialize;
use nasty_common::{ErrorCode, Request, Response};
use tracing::debug;

use crate::AppState;
use crate::auth::{Role, Session};

/// Methods an operator token is allowed to call (in addition to all read-only methods)
fn is_operator_allowed(method: &str) -> bool {
    is_read_only(method)
        || matches!(
            method,
            "subvolume.create" | "subvolume.delete" | "subvolume.attach" | "subvolume.detach"
            | "subvolume.resize" | "subvolume.clone"
            | "subvolume.set_properties" | "subvolume.remove_properties"
            | "snapshot.create" | "snapshot.delete" | "snapshot.clone"
        )
}

/// Extract a string param from JSON-RPC params
fn str_param<'a>(request: &'a Request, key: &str) -> Option<&'a str> {
    request
        .params
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
}

/// Parse typed params from JSON-RPC request
fn parse_params<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, String> {
    request
        .params
        .as_ref()
        .ok_or_else(|| "missing params".to_string())
        .and_then(|p| serde_json::from_value(p.clone()).map_err(|e| e.to_string()))
}

/// Check if a method is read-only (safe for ReadOnly role)
fn is_read_only(method: &str) -> bool {
    method.ends_with(".list")
        || method.ends_with(".get")
        || matches!(
            method,
            "system.info" | "system.health" | "system.stats" | "system.disks" | "system.network.get"
            | "system.alerts" | "system.settings.get" | "system.metrics.history" | "alert.rules.list"
            | "device.list" | "auth.me" | "auth.list_users" | "auth.token.list"
            | "pool.usage" | "pool.scrub.status" | "pool.reconcile.status"
            | "bcachefs.usage"
            | "service.protocol.list" | "subvolume.list_all" | "subvolume.find_by_property"
            | "system.update.version" | "system.update.status" | "system.reboot_required"
            | "system.settings.timezones"
            | "bcachefs.tools.info" | "bcachefs.tools.status"
        )
}

/// Derive the collection name for a mutation method, or None if read-only.
fn collection_for_method(method: &str) -> Option<&'static str> {
    match method {
        m if m.starts_with("pool.device.") => Some("pool"),
        m if m.starts_with("pool.") && !is_read_only(m) => Some("pool"),
        m if m.starts_with("device.") && !is_read_only(m) => Some("pool"),
        m if m.starts_with("subvolume.") && !is_read_only(m) => Some("subvolume"),
        m if m.starts_with("snapshot.") && !is_read_only(m) => Some("snapshot"),
        m if m.starts_with("share.nfs.") && !is_read_only(m) => Some("share.nfs"),
        m if m.starts_with("share.smb.") && !is_read_only(m) => Some("share.smb"),
        m if m.starts_with("share.iscsi.") && !is_read_only(m) => Some("share.iscsi"),
        m if m.starts_with("share.nvmeof.") && !is_read_only(m) => Some("share.nvmeof"),
        m if m.starts_with("service.protocol.") && !is_read_only(m) => Some("protocol"),
        m if m.starts_with("system.settings.") && !is_read_only(m) => Some("settings"),
        m if m.starts_with("alert.rules.") && !is_read_only(m) => Some("alert"),
        _ => None,
    }
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
        && !matches!(request.method.as_str(), "auth.change_password" | "auth.me" | "auth.logout")
    {
        let resp = Response::error(
            request.id,
            ErrorCode::InternalError,
            "Password change required",
        );
        return serde_json::to_string(&resp).unwrap();
    }

    // Enforce role permissions
    let denied = match session.role {
        Role::Admin => false,
        Role::ReadOnly => !is_read_only(&request.method),
        Role::Operator => !is_operator_allowed(&request.method),
    };
    if denied {
        let resp = Response::error(
            request.id,
            ErrorCode::InternalError,
            "Permission denied",
        );
        return serde_json::to_string(&resp).unwrap();
    }

    let t0 = std::time::Instant::now();
    let response = route(&request, state, session).await;
    let elapsed = t0.elapsed();
    if elapsed.as_millis() > 50 {
        tracing::warn!("RPC slow: {} took {}ms", request.method, elapsed.as_millis());
    } else {
        debug!("RPC done: {} in {}ms", request.method, elapsed.as_millis());
    }

    // Broadcast event to all clients on successful mutations
    if response.error.is_none() {
        if let Some(collection) = collection_for_method(&request.method) {
            let _ = state.events.send(collection.to_string());
        }
    }

    serde_json::to_string(&response).unwrap()
}

async fn route(req: &Request, state: &AppState, session: &Session) -> Response {
    match req.method.as_str() {
        // ── Auth ──────────────────────────────────────────────────
        "auth.me" => ok(req, serde_json::json!({
            "username": session.username,
            "role": session.role,
        })),
        "auth.logout" => match state.auth.logout(&session.token).await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "auth.change_password" => {
            #[derive(Deserialize)]
            struct P { username: String, new_password: String }
            match parse_params::<P>(req) {
                Ok(p) => match state.auth.change_password(session, &p.username, &p.new_password).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.create_user" => {
            #[derive(Deserialize)]
            struct P { username: String, password: String, role: Role }
            match parse_params::<P>(req) {
                Ok(p) => match state.auth.create_user(session, &p.username, &p.password, p.role).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.delete_user" => {
            match require_str(req, "username") {
                Ok(username) => match state.auth.delete_user(session, username).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(r) => r,
            }
        }
        "auth.list_users" => ok(req, state.auth.list_users().await),
        "auth.token.list" => match state.auth.list_api_tokens(session).await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "auth.token.create" => {
            #[derive(Deserialize)]
            struct P { name: String, role: Role, pool: Option<String>, expires_in_secs: Option<u64> }
            match parse_params::<P>(req) {
                Ok(p) => match state.auth.create_api_token(session, &p.name, p.role, p.pool, p.expires_in_secs).await {
                    Ok(t) => ok(req, t),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.token.delete" => match require_str(req, "id") {
            Ok(id) => match state.auth.delete_api_token(session, id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── System ──────────────────────────────────────────────
        "system.info" => ok(req, state.system.info().await),
        "system.health" => ok(req, state.system.health().await),
        "system.stats" => ok(req, state.system.stats().await),
        "system.network.get" => ok(req, state.network.get().await),
        "system.network.update" => match parse_params::<nasty_system::network::NetworkConfig>(req) {
            Ok(p) => match state.network.update(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "system.metrics.history" => {
            let kind = str_param(req, "kind").unwrap_or("net");
            let name = str_param(req, "name");
            let range = str_param(req, "range").unwrap_or("5m");
            ok(req, state.metrics.query(kind, name, range))
        }
        "system.disks" => {
            if state.protocols.is_enabled(nasty_system::protocol::Protocol::Smart).await {
                ok(req, state.system.disks().await)
            } else {
                ok(req, Vec::<nasty_system::DiskHealth>::new())
            }
        }

        // ── Settings ──────────────────────────────────────────────
        "system.settings.timezones" => match nasty_system::settings::list_timezones().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.settings.get" => ok(req, state.settings.get().await),
        "system.settings.update" => match parse_params(req) {
            Ok(p) => match state.settings.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── System Update ─────────────────────────────────────────
        "system.update.version" => ok(req, state.updates.version().await),
        "system.update.check" => match state.updates.check().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.update.apply" => match state.updates.apply().await {
            Ok(()) => { state.system.invalidate_bcachefs_cache().await; state.updates.invalidate_bcachefs_cache().await; ok(req, "ok") }
            Err(e) => err(req, e),
        },
        "system.update.rollback" => match state.updates.rollback().await {
            Ok(()) => { state.system.invalidate_bcachefs_cache().await; state.updates.invalidate_bcachefs_cache().await; ok(req, "ok") }
            Err(e) => err(req, e),
        },
        "system.update.status" => ok(req, state.updates.status().await),
        "system.reboot_required" => ok(req, state.updates.reboot_required().await),

        // ── bcachefs-tools version switching ──────────────────────
        "bcachefs.tools.info" => ok(req, state.updates.bcachefs_info(&state.system).await),
        "bcachefs.tools.switch" => match parse_params::<nasty_system::update::BcachefsToolsSwitchRequest>(req) {
            Ok(p) => match state.updates.bcachefs_switch(p).await {
                Ok(()) => {
                    state.system.invalidate_bcachefs_cache().await;
                    state.updates.invalidate_bcachefs_cache().await;
                    ok(req, serde_json::json!({"status": "started"}))
                }
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "bcachefs.tools.status" => ok(req, state.updates.bcachefs_status().await),

        "system.reboot" => match state.updates.reboot().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "system.shutdown" => match state.updates.shutdown().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },

        // ── Protocols ────────────────────────────────────────────
        "service.protocol.list" => ok(req, state.protocols.list().await),
        "service.protocol.enable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.enable(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "service.protocol.disable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.disable(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Alerts ───────────────────────────────────────────────
        "system.alerts" => {
            // Evaluate current alert rules against live system state
            let stats = state.system.stats().await;
            let pools_list = state.pools.list().await;
            let disk_health = if state.protocols.is_enabled(nasty_system::protocol::Protocol::Smart).await {
                state.system.disks().await
            } else {
                Vec::new()
            };

            let pool_usage: Vec<nasty_system::alerts::PoolUsage> = pools_list
                .unwrap_or_default()
                .into_iter()
                .map(|p| nasty_system::alerts::PoolUsage {
                    name: p.name,
                    used_bytes: p.used_bytes,
                    total_bytes: p.total_bytes,
                })
                .collect();

            let disk_summary: Vec<nasty_system::alerts::DiskHealthSummary> = disk_health
                .into_iter()
                .map(|d| nasty_system::alerts::DiskHealthSummary {
                    device: d.device,
                    temperature_c: d.temperature_c,
                    health_passed: d.health_passed,
                })
                .collect();

            let alerts = state.alerts.evaluate(&stats, &pool_usage, &disk_summary).await;
            ok(req, alerts)
        }
        "alert.rules.list" => ok(req, state.alerts.list_rules().await),
        "alert.rules.create" => match parse_params(req) {
            Ok(rule) => match state.alerts.create_rule(rule).await {
                Ok(r) => ok(req, r),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "alert.rules.update" => match parse_params::<nasty_system::alerts::AlertRuleUpdate>(req) {
            Ok(update) => match state.alerts.update_rule(&update.id.clone(), update).await {
                Ok(r) => ok(req, r),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "alert.rules.delete" => match require_str(req, "id") {
            Ok(id) => match state.alerts.delete_rule(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Pools ───────────────────────────────────────────────
        "pool.list" => match state.pools.list().await {
            Ok(mut v) => {
                if let Some(ref pool_name) = session.pool {
                    v.retain(|p| &p.name == pool_name);
                }
                ok(req, v)
            }
            Err(e) => err(req, e),
        },
        "pool.get" => match require_str(req, "name") {
            Ok(name) => {
                if session.pool.as_deref().map_or(false, |p| p != name) {
                    err(req, "access denied")
                } else {
                    match state.pools.get(name).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "pool.create" => match parse_params(req) {
            Ok(p) => match state.pools.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.destroy" => match parse_params(req) {
            Ok(p) => match state.pools.destroy(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.mount" => match require_str(req, "name") {
            Ok(name) => match state.pools.mount(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "pool.unmount" => match require_str(req, "name") {
            Ok(name) => match state.pools.unmount(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "device.list" => match state.pools.list_devices().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "device.wipe" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                let path = p.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
                match state.pools.device_wipe(&path).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "pool.options.update" => match parse_params(req) {
            Ok(p) => match state.pools.update_options(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.add" => match parse_params(req) {
            Ok(p) => match state.pools.device_add(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.remove" => match parse_params(req) {
            Ok(p) => match state.pools.device_remove(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.evacuate" => match parse_params::<nasty_storage::pool::DeviceActionRequest>(req) {
            Ok(p) => {
                // Validate synchronously before returning
                match state.pools.get(&p.pool).await {
                    Err(e) => err(req, e),
                    Ok(pool) if !pool.mounted => err(req, nasty_storage::PoolError::CommandFailed(
                        "pool must be mounted to evacuate a device".into(),
                    )),
                    Ok(_) => {
                        // Run in background — bcachefs evacuate can take many minutes.
                        // Emit pool events every 3 s so UI shows live device state.
                        let pools = state.pools.clone();
                        let events = state.events.clone();
                        tokio::spawn(async move {
                            let poll_events = events.clone();
                            let poll = tokio::spawn(async move {
                                loop {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    let _ = poll_events.send("pool".to_string());
                                }
                            });
                            let _ = pools.device_evacuate(p).await;
                            poll.abort();
                            let _ = events.send("pool".to_string());
                        });
                        ok(req, serde_json::json!({"status": "started"}))
                    }
                }
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.set_state" => match parse_params(req) {
            Ok(p) => match state.pools.device_set_state(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.online" => match parse_params(req) {
            Ok(p) => match state.pools.device_online(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.offline" => match parse_params(req) {
            Ok(p) => match state.pools.device_offline(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "pool.device.set_label" => match parse_params(req) {
            Ok(p) => match state.pools.device_set_label(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── Pool health & monitoring ─────────────────────────────
        "pool.usage" => match require_str(req, "name") {
            Ok(name) => match state.pools.usage(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "pool.scrub.start" => match require_str(req, "name") {
            Ok(name) => match state.pools.scrub_start(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "pool.scrub.status" => match require_str(req, "name") {
            Ok(name) => match state.pools.scrub_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "pool.reconcile.status" => match require_str(req, "name") {
            Ok(name) => match state.pools.reconcile_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── bcachefs diagnostics ────────────────────────────────
        "bcachefs.usage" => match require_str(req, "name") {
            Ok(name) => match state.pools.bcachefs_usage(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Subvolumes ──────────────────────────────────────────
        "subvolume.list_all" => {
            let pool_filter = session.pool.as_deref();
            let owner_filter = session.owner.as_deref();
            match state.subvolumes.list_all(pool_filter, owner_filter).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "subvolume.list" => match require_str(req, "pool") {
            Ok(pool) => {
                if session.pool.as_deref().map_or(false, |p| p != pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.list(pool, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "subvolume.get" => match (require_str(req, "pool"), require_str(req, "name")) {
            (Ok(pool), Ok(name)) => {
                if session.pool.as_deref().map_or(false, |p| p != pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.get(pool, name, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.create" => match parse_params::<nasty_storage::subvolume::CreateSubvolumeRequest>(req) {
            Ok(p) => {
                if session.pool.as_deref().map_or(false, |pool| pool != p.pool) {
                    err(req, "access denied")
                } else {
                    let owner = session.owner.clone();
                    match state.subvolumes.create(p, owner).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },
        "subvolume.delete" => match parse_params::<nasty_storage::subvolume::DeleteSubvolumeRequest>(req) {
            Ok(p) => {
                if session.pool.as_deref().map_or(false, |pool| pool != p.pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.delete(p, session.owner.as_deref()).await {
                        Ok(()) => ok(req, "ok"),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },
        "subvolume.attach" => match (require_str(req, "pool"), require_str(req, "name")) {
            (Ok(pool), Ok(name)) => {
                if session.pool.as_deref().map_or(false, |p| p != pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.attach(pool, name, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.detach" => match (require_str(req, "pool"), require_str(req, "name")) {
            (Ok(pool), Ok(name)) => {
                if session.pool.as_deref().map_or(false, |p| p != pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.detach(pool, name, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },

        "subvolume.resize" => match parse_params::<nasty_storage::subvolume::ResizeSubvolumeRequest>(req) {
            Ok(p) => {
                if session.pool.as_deref().map_or(false, |pool| pool != p.pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.resize(p, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },

        "subvolume.clone" => match parse_params::<nasty_storage::subvolume::CloneSubvolumeRequest>(req) {
            Ok(p) => {
                if session.pool.as_deref().map_or(false, |pool| pool != p.pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.clone_subvolume(p, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },

        "subvolume.set_properties" => match parse_params::<nasty_storage::subvolume::SetPropertiesRequest>(req) {
            Ok(p) => {
                if session.pool.as_deref().map_or(false, |sp| sp != p.pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.set_properties(p, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },
        "subvolume.remove_properties" => match parse_params::<nasty_storage::subvolume::RemovePropertiesRequest>(req) {
            Ok(p) => {
                if session.pool.as_deref().map_or(false, |sp| sp != p.pool) {
                    err(req, "access denied")
                } else {
                    match state.subvolumes.remove_properties(p, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },
        "subvolume.find_by_property" => match parse_params::<nasty_storage::subvolume::FindByPropertyRequest>(req) {
            Ok(p) => {
                // Enforce pool-scoped token restriction
                let effective_pool = match (&session.pool, &p.pool) {
                    (Some(sp), Some(rp)) if sp != rp => {
                        return err(req, "access denied");
                    }
                    (Some(sp), None) => Some(nasty_storage::subvolume::FindByPropertyRequest {
                        pool: Some(sp.clone()),
                        key: p.key.clone(),
                        value: p.value.clone(),
                    }),
                    _ => None,
                };
                let req_effective = effective_pool.unwrap_or(p);
                match state.subvolumes.find_by_property(req_effective, session.owner.as_deref()).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },

        // ── Snapshots ───────────────────────────────────────────
        "snapshot.list" => match require_str(req, "pool") {
            Ok(pool) => {
                if session.pool.as_deref().map_or(false, |p| p != pool) {
                    err(req, "access denied")
                } else {
                    match state.snapshots.list(pool, session.owner.as_deref()).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "snapshot.create" => match parse_params(req) {
            Ok(p) => match state.snapshots.create(p, session.owner.as_deref()).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "snapshot.delete" => match parse_params(req) {
            Ok(p) => match state.snapshots.delete(p, session.owner.as_deref()).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "snapshot.clone" => match parse_params(req) {
            Ok(p) => match state.snapshots.clone_snapshot(p, session.owner.as_deref()).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        // ── NFS Shares ──────────────────────────────────────────
        "share.nfs.list" => match state.nfs.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.nfs.get" => match require_str(req, "id") {
            Ok(id) => match state.nfs.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.nfs.create" => match parse_params(req) {
            Ok(p) => match state.nfs.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nfs.update" => match parse_params(req) {
            Ok(p) => match state.nfs.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nfs.delete" => match parse_params(req) {
            Ok(p) => match state.nfs.delete(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── SMB Shares ──────────────────────────────────────────
        "share.smb.list" => match state.smb.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.smb.get" => match require_str(req, "id") {
            Ok(id) => match state.smb.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.smb.create" => match parse_params(req) {
            Ok(p) => match state.smb.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.smb.update" => match parse_params(req) {
            Ok(p) => match state.smb.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.smb.delete" => match parse_params(req) {
            Ok(p) => match state.smb.delete(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── iSCSI Targets ───────────────────────────────────────
        "share.iscsi.list" => match state.iscsi.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.iscsi.get" => match require_str(req, "id") {
            Ok(id) => match state.iscsi.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.iscsi.create_quick" => match parse_params(req) {
            Ok(p) => match state.iscsi.create_quick(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.iscsi.create" => match parse_params(req) {
            Ok(p) => match state.iscsi.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.iscsi.delete" => match parse_params(req) {
            Ok(p) => match state.iscsi.delete(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.iscsi.add_lun" => match parse_params(req) {
            Ok(p) => match state.iscsi.add_lun(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.iscsi.remove_lun" => match parse_params(req) {
            Ok(p) => match state.iscsi.remove_lun(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.iscsi.add_acl" => match parse_params(req) {
            Ok(p) => match state.iscsi.add_acl(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.iscsi.remove_acl" => match parse_params(req) {
            Ok(p) => match state.iscsi.remove_acl(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── NVMe-oF Subsystems ─────────────────────────────────
        "share.nvmeof.list" => match state.nvmeof.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.nvmeof.get" => match require_str(req, "id") {
            Ok(id) => match state.nvmeof.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.nvmeof.create_quick" => match parse_params(req) {
            Ok(p) => match state.nvmeof.create_quick(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.create" => match parse_params(req) {
            Ok(p) => match state.nvmeof.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.delete" => match parse_params(req) {
            Ok(p) => match state.nvmeof.delete(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.add_namespace" => match parse_params(req) {
            Ok(p) => match state.nvmeof.add_namespace(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.remove_namespace" => match parse_params(req) {
            Ok(p) => match state.nvmeof.remove_namespace(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.add_port" => match parse_params(req) {
            Ok(p) => match state.nvmeof.add_port(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.remove_port" => match parse_params(req) {
            Ok(p) => match state.nvmeof.remove_port(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.add_host" => match parse_params(req) {
            Ok(p) => match state.nvmeof.add_host(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "share.nvmeof.remove_host" => match parse_params(req) {
            Ok(p) => match state.nvmeof.remove_host(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── Unknown ─────────────────────────────────────────────
        _ => Response::error(
            req.id.clone(),
            ErrorCode::MethodNotFound,
            format!("Unknown method: {}", req.method),
        ),
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn ok(req: &Request, val: impl serde::Serialize) -> Response {
    Response::success(req.id.clone(), serde_json::to_value(val).unwrap())
}

fn err(req: &Request, e: impl std::fmt::Display) -> Response {
    Response::error(req.id.clone(), ErrorCode::InternalError, e.to_string())
}

fn invalid(req: &Request, msg: impl std::fmt::Display) -> Response {
    Response::error(
        req.id.clone(),
        ErrorCode::InvalidParams,
        format!("Invalid params: {msg}"),
    )
}

fn require_str<'a>(req: &'a Request, key: &str) -> Result<&'a str, Response> {
    str_param(req, key).ok_or_else(|| {
        Response::error(
            req.id.clone(),
            ErrorCode::InvalidParams,
            format!("Missing required param: {key}"),
        )
    })
}
