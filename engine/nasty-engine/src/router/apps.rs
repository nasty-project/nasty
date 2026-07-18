//! RPC arms in the `apps.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

pub(crate) async fn published_firewall_ports(
    state: &AppState,
) -> Result<Vec<nasty_system::firewall::PublishedAppPort>, String> {
    let apps = state
        .apps
        .list()
        .await
        .map_err(|e| format!("list apps for firewall: {e}"))?;
    let mut published: Vec<nasty_system::firewall::PublishedAppPort> = apps
        .iter()
        .flat_map(|app| {
            app.ports
                .iter()
                .map(move |port| nasty_system::firewall::PublishedAppPort {
                    app: app.name.clone(),
                    host_port: port.host_port,
                    container_port: port.container_port,
                    transport: port.protocol.to_ascii_lowercase(),
                })
        })
        .collect();
    published.sort_by(|a, b| {
        a.host_port
            .cmp(&b.host_port)
            .then_with(|| a.app.cmp(&b.app))
    });
    Ok(published)
}

pub(crate) async fn sync_published_firewall_ports(state: &AppState) -> Result<(), String> {
    let _sync = state.app_firewall_sync.lock().await;
    let published = published_firewall_ports(state).await?;
    state.firewall.set_published_app_ports(published).await
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    let response = match req.method.as_str() {
        "apps.status" => ok(req, state.apps.status().await),
        "apps.enable" => {
            let p: nasty_apps::EnableAppsRequest = parse_params(req).unwrap_or_default();
            match state.apps.enable(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            }
        }
        "apps.disable" => match state.apps.disable().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "apps.list" => match state.apps.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.stats" => match state.apps.stats().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.get" => match require_str(req, "name") {
            Ok(name) => match state.apps.get(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.inspect" => match require_str(req, "name") {
            Ok(name) => match state.apps.inspect(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.install" => match parse_params::<nasty_apps::InstallAppRequest>(req) {
            Ok(p) => {
                // Refresh Caddy TLS automation when the install opts into
                // a subdomain ingress — install() wires the route but the
                // TLS layer is reached from here (see the matching note in
                // app_deploy.rs::deploy_simple).
                let chose_subdomain = p
                    .subdomain
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty());
                match state.apps.install(p).await {
                    Ok(v) => {
                        if chose_subdomain {
                            tokio::spawn(nasty_system::settings::reapply_tls_from_disk());
                        }
                        ok(req, v)
                    }
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "apps.update" => match parse_params(req) {
            Ok(p) => match state.apps.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.inspect_image" => match require_str(req, "image") {
            Ok(image) => match state.apps.inspect_image(image).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.check_ports" => match parse_params(req) {
            Ok(p) => ok(req, state.apps.check_ports(p).await),
            Err(e) => invalid(req, e),
        },
        "apps.check_devices" => match parse_params(req) {
            Ok(p) => ok(req, state.apps.check_devices(p).await),
            Err(e) => invalid(req, e),
        },
        "apps.check_volumes" => match parse_params(req) {
            Ok(p) => ok(req, state.apps.check_volumes(p).await),
            Err(e) => invalid(req, e),
        },
        "apps.check_compose" => match parse_params(req) {
            Ok(p) => ok(req, state.apps.check_compose(p).await),
            Err(e) => invalid(req, e),
        },
        "apps.appdata.status" => ok(req, state.apps.appdata_relocate_status().await),
        "apps.appdata.relocate" => match require_str(req, "filesystem") {
            Ok(fs) => match state.apps.appdata_relocate(fs).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.fix_volume_perms" => match parse_params(req) {
            Ok(p) => match state.apps.fix_volume_perms(p).await {
                Ok(()) => ok(req, serde_json::json!({"ok": true})),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.config" => match require_str(req, "name") {
            Ok(name) => match state.apps.get_config(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.remove(name).await {
                Ok(()) => {
                    // Cover the case where the removed app had a
                    // subdomain ingress — Caddy stops trying to renew
                    // the now-orphaned cert. The internal remove path
                    // in nasty-apps clears the route via the admin API
                    // but doesn't know about the TLS-automation layer.
                    tokio::spawn(nasty_system::settings::reapply_tls_from_disk());
                    ok(req, "ok")
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.stop" => match require_str(req, "name") {
            Ok(name) => match state.apps.stop(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.start" => match require_str(req, "name") {
            Ok(name) => match state.apps.start(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.restart" => match require_str(req, "name") {
            Ok(name) => match state.apps.restart(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.pull" => match require_str(req, "name") {
            Ok(name) => match state.apps.pull(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.prune" => match state.apps.prune().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.exec_command" => match require_str(req, "name") {
            Ok(name) => match state.apps.exec_command(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.logs" => {
            let name = match require_str(req, "name") {
                Ok(n) => n,
                Err(r) => return Some(r),
            };
            let tail = req
                .params
                .as_ref()
                .and_then(|p| p.get("tail"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            match state.apps.logs(name, tail).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "apps.container.logs" => {
            let container_id = match require_str(req, "container_id") {
                Ok(n) => n,
                Err(r) => return Some(r),
            };
            let tail = req
                .params
                .as_ref()
                .and_then(|p| p.get("tail"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            match state.apps.container_logs(container_id, tail).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "apps.compose.install" => match parse_params(req) {
            Ok(p) => match state.apps.compose_install(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.compose.update" => match parse_params(req) {
            Ok(p) => match state.apps.compose_update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.compose.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.compose_remove(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.compose.get" => match require_str(req, "name") {
            Ok(name) => match state.apps.compose_get(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.compose.logs" => {
            let name = match require_str(req, "name") {
                Ok(n) => n,
                Err(r) => return Some(r),
            };
            let tail = req
                .params
                .as_ref()
                .and_then(|p| p.get("tail"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            match state.apps.compose_logs(name, tail).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "apps.compose.set_startup" => {
            match parse_params::<nasty_apps::SetComposeStartupRequest>(req) {
                Ok(p) => match state
                    .apps
                    .compose_set_startup(&p.name, p.managed, p.order, p.delay_secs)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "apps.compose.startup.list" => ok(req, state.apps.compose_list_startup().await),
        "apps.ingress.list" => match state.apps.ingress_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        // Every route Caddy is serving (engine-owned + static), powering
        // the Ingress overview page so the operator can see at a glance
        // what's exposed and where each row came from — without shelling
        // in to read the live Caddy config.
        //
        // We enrich host-match rows with their on-disk cert info here
        // (rather than inside nasty-apps' walker) because the cert
        // directory lives in nasty-system's domain — nasty-apps doesn't
        // depend on nasty-system, and reaching across crates just for
        // a single optional field would invert the dep graph.
        "apps.caddy.routes" => match state.apps.list_caddy_routes().await {
            Ok(mut rows) => {
                for row in &mut rows {
                    if row.match_kind != "host" {
                        continue;
                    }
                    if let Some(info) =
                        nasty_system::settings::cert_info_for_host(&row.match_value).await
                    {
                        row.cert = Some(nasty_apps::HostCert {
                            issuer: info.issuer,
                            issued: info.issued,
                            expires: info.expires,
                            expires_in_days: info.expires_in_days,
                            path: info.path,
                        });
                    }
                }
                ok(req, rows)
            }
            Err(e) => err(req, e),
        },
        "apps.ingress.set" => match parse_params::<nasty_apps::SetIngressRequest>(req) {
            Ok(p) => {
                // Gate the set on a subdomain-conflict check — catches the
                // "two apps claim the same hostname" / "app subdomain ==
                // WebUI hostname" cases that Caddy would silently let the
                // most recent one win. Empty subdomain (path-prefix mode)
                // short-circuits past the check inside find_subdomain_conflict.
                let conflict = match &p.subdomain {
                    Some(s) => {
                        crate::ingress_conflict::find_subdomain_conflict(state, &p.name, s).await
                    }
                    None => None,
                };
                if let Some(reason) = conflict {
                    err(req, format!("subdomain conflict: {reason}"))
                } else {
                    match state.apps.ingress_set(p).await {
                        Ok(v) => {
                            // Refresh Caddy's TLS automation so a new
                            // subdomain gets a cert immediately. Spawn
                            // so the RPC reply isn't blocked by the
                            // admin-API round-trip.
                            tokio::spawn(nasty_system::settings::reapply_tls_from_disk());
                            ok(req, v)
                        }
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },
        // Best-effort lookup used by the WebUI's subdomain dialog to
        // surface a live "in use by X" hint as the operator types,
        // before they click Save. Returns the conflict reason or an
        // empty string when the choice is clear. Read-only.
        "apps.ingress.check_conflict" => 'arm: {
            let name = match require_str(req, "name") {
                Ok(s) => s,
                Err(r) => break 'arm r,
            };
            let subdomain = match require_str(req, "subdomain") {
                Ok(s) => s,
                Err(r) => break 'arm r,
            };
            let reason = crate::ingress_conflict::find_subdomain_conflict(state, name, subdomain)
                .await
                .unwrap_or_default();
            ok(req, reason)
        }
        "apps.ingress.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.ingress_remove(name).await {
                Ok(()) => {
                    // Reapply so Caddy stops trying to renew the cert
                    // for the now-orphaned subdomain. Same fire-and-
                    // forget pattern as the set arm above.
                    tokio::spawn(nasty_system::settings::reapply_tls_from_disk());
                    ok(req, "ok")
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Managed Docker networks ──────────────────────────
        "apps.networks.list" => match state.apps.network_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.networks.create" => match parse_params::<nasty_apps::ManagedNetwork>(req) {
            Ok(spec) => {
                let ifaces = crate::system_network_ifaces(state).await;
                // Resolve the management interface from the caller's peer so we
                // can refuse a host shim on it (lockout guard, #448).
                let mgmt = match session.client_ip.as_deref() {
                    Some(peer) => nasty_system::network::mgmt_iface_for_peer(peer).await,
                    None => None,
                };
                if spec.host_shim && spec.parent.as_deref() == mgmt.as_deref() {
                    err(
                        req,
                        "refusing a host shim on the management interface (would risk lockout)"
                            .to_string(),
                    )
                } else {
                    match state.apps.network_create(spec.clone(), &ifaces).await {
                        Ok(()) => {
                            if spec.host_shim {
                                // Apply the host-side shim; on failure undo the
                                // Docker network so we don't leave a half-state.
                                match crate::add_macvlan_shim(state, &spec, mgmt.as_deref()).await {
                                    Ok(()) => ok(req, "ok"),
                                    Err(e) => {
                                        let _ = state.apps.network_remove(&spec.name).await;
                                        err(req, format!("host shim failed: {e}"))
                                    }
                                }
                            } else {
                                ok(req, "ok")
                            }
                        }
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(e) => invalid(req, e),
        },
        "apps.networks.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.network_remove(name).await {
                Ok(()) => {
                    // Tear down the host shim too (best-effort).
                    if let Err(e) = crate::remove_macvlan_shim(state, name).await {
                        tracing::warn!("apps: failed to remove macvlan shim for '{name}': {e}");
                    }
                    ok(req, "ok")
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    };
    let changes_published_ports = matches!(
        req.method.as_str(),
        "apps.enable"
            | "apps.disable"
            | "apps.install"
            | "apps.update"
            | "apps.remove"
            | "apps.pull"
            | "apps.compose.install"
            | "apps.compose.update"
            | "apps.compose.remove"
            | "apps.compose.set_startup"
    );
    if changes_published_ports && let Err(e) = sync_published_firewall_ports(state).await {
        if response.error.is_none() {
            return Some(err(
                req,
                format!("app state changed but firewall synchronization failed: {e}"),
            ));
        }
        tracing::warn!("app firewall reconciliation after failed request also failed: {e}");
    }
    Some(response)
}
