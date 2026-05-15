//! RPC arms in the `service.*` domain. Extracted from the historical
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
        "service.protocol.list" => ok(req, state.protocols.list().await),
        "service.protocol.enable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.enable(name).await {
                Ok(v) => {
                    if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                        state.firewall.open(proto).await;
                    }
                    ok(req, v)
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "service.protocol.disable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.disable(name).await {
                Ok(v) => {
                    if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                        state.firewall.close(proto).await;
                    }
                    ok(req, v)
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "service.base_names.get" => {
            let iqn = tokio::fs::read_to_string("/var/lib/nasty/iscsi-base-iqn")
                .await
                .unwrap_or_else(|_| "iqn.2137-04.storage.nasty".into());
            let nqn = tokio::fs::read_to_string("/var/lib/nasty/nvmeof-base-nqn")
                .await
                .unwrap_or_else(|_| "nqn.2137-04.storage.nasty".into());
            ok(
                req,
                serde_json::json!({ "iqn_prefix": iqn.trim(), "nqn_prefix": nqn.trim() }),
            )
        }
        "service.base_names.update" => {
            if let Some(iqn) = req
                .params
                .as_ref()
                .and_then(|p| p.get("iqn_prefix"))
                .and_then(|v| v.as_str())
            {
                let _ = tokio::fs::write("/var/lib/nasty/iscsi-base-iqn", iqn.trim()).await;
            }
            if let Some(nqn) = req
                .params
                .as_ref()
                .and_then(|p| p.get("nqn_prefix"))
                .and_then(|v| v.as_str())
            {
                let _ = tokio::fs::write("/var/lib/nasty/nvmeof-base-nqn", nqn.trim()).await;
            }
            ok(req, "ok")
        }
        "service.rest_server.config" => {
            let path = tokio::fs::read_to_string("/var/lib/nasty/rest-server-path")
                .await
                .unwrap_or_else(|_| "/var/lib/nasty/rest-server".into());
            ok(req, serde_json::json!({ "path": path.trim() }))
        }
        "service.rest_server.configure" => {
            let path = match require_str(req, "path") {
                Ok(s) => s.to_string(),
                Err(r) => return Some(r),
            };

            // Create subvolume if path is under /fs/ and doesn't exist
            if path.starts_with("/fs/")
                && !std::path::Path::new(&path).exists()
                && let Some(rest) = path.strip_prefix("/fs/")
                && let Some((fs_name, subvol_name)) = rest.split_once('/')
            {
                let create_req = nasty_storage::subvolume::CreateSubvolumeRequest {
                    filesystem: fs_name.to_string(),
                    name: subvol_name.to_string(),
                    subvolume_type: nasty_storage::subvolume::SubvolumeType::Filesystem,
                    volsize_bytes: None,
                    compression: None,
                    comments: Some("Backup Server storage".to_string()),
                    direct_io: None,
                    foreground_target: None,
                    background_target: None,
                    promote_target: None,
                    metadata_target: None,
                    data_replicas: None,
                };
                let _ = state.subvolumes.create(create_req, None).await;
            }

            if let Err(e) = tokio::fs::write("/var/lib/nasty/rest-server-path", &path).await {
                return Some(err(req, format!("write config: {e}")));
            }

            // Restart rest-server to pick up new path. `try_run` logs
            // failures so a botched restart (config typo, port collision,
            // etc.) shows up in the journal even though we don't surface
            // it on the RPC reply (we already ack'd the path write).
            nasty_common::cmd::try_run("systemctl", &["restart", "nasty-rest-server"]).await;

            ok(req, "ok")
        }
        _ => return None,
    })
}
