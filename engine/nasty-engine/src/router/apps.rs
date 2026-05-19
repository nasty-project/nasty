//! RPC arms in the `apps.*` domain. Extracted from the historical
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
        "apps.install" => match parse_params(req) {
            Ok(p) => match state.apps.install(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
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
                Ok(()) => ok(req, "ok"),
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
        "apps.ingress.set" => match parse_params(req) {
            Ok(p) => match state.apps.ingress_set(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.ingress.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.ingress_remove(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}
