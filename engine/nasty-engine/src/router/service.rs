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
            Ok(name) => {
                if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                    match state.firewall.open(proto).await {
                        Err(e) => err(req, format!("firewall update failed: {e}")),
                        Ok(()) => match state.protocols.enable(name).await {
                            Ok(v) => ok(req, v),
                            Err(service_error) => {
                                let rollback = state.firewall.close(proto).await;
                                match rollback {
                                    Ok(()) => err(req, service_error),
                                    Err(firewall_error) => err(
                                        req,
                                        format!(
                                            "{service_error}; firewall rollback also failed: {firewall_error}"
                                        ),
                                    ),
                                }
                            }
                        },
                    }
                } else {
                    match state.protocols.enable(name).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "service.protocol.disable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.disable(name).await {
                Ok(v) => {
                    if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                        match state.firewall.close(proto).await {
                            Ok(()) => ok(req, v),
                            Err(e) => err(
                                req,
                                format!("protocol disabled but firewall update failed: {e}"),
                            ),
                        }
                    } else {
                        ok(req, v)
                    }
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
                && let Err(e) = tokio::fs::write("/var/lib/nasty/iscsi-base-iqn", iqn.trim()).await
            {
                // Non-fatal — the engine still has the value in memory
                // — but at restart it'll fall back to the default IQN,
                // which is confusing if the user just configured a
                // custom one.
                tracing::warn!("persist iscsi base IQN failed: {e}");
            }
            if let Some(nqn) = req
                .params
                .as_ref()
                .and_then(|p| p.get("nqn_prefix"))
                .and_then(|v| v.as_str())
                && let Err(e) = tokio::fs::write("/var/lib/nasty/nvmeof-base-nqn", nqn.trim()).await
            {
                tracing::warn!("persist nvmeof base NQN failed: {e}");
            }
            ok(req, "ok")
        }
        "service.rest_server.config" => {
            let path = tokio::fs::read_to_string("/var/lib/nasty/rest-server-path")
                .await
                .unwrap_or_else(|_| "/var/lib/nasty/rest-server".into());
            ok(req, serde_json::json!({ "path": path.trim() }))
        }
        "service.rest_server.credentials" => {
            // Returns the plaintext user + password for the rest-server's
            // basic auth. The operator pastes these into the source-side
            // backup profile URL (`https://<user>:<password>@<host>:8000/`).
            // Lazily generates credentials on first call if the protocol
            // was enabled before this code shipped — same idempotent
            // ensure path the protocol-enable hook uses.
            if let Err(e) = nasty_system::rest_server::ensure_credentials().await {
                return Some(err(req, format!("ensure credentials: {e}")));
            }
            match nasty_system::rest_server::get_credentials().await {
                Ok(c) => ok(req, c),
                Err(e) => err(req, e.to_string()),
            }
        }
        "service.rest_server.rotate_credentials" => {
            // Generate a fresh random password (optionally a new username),
            // rewrite the htpasswd file, restart the service so it picks
            // up the new file. Operator follow-up: update every source
            // profile that points at this rest-server with the new URL.
            let new_username = req
                .params
                .as_ref()
                .and_then(|p| p.get("username"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            match nasty_system::rest_server::rotate_password(new_username).await {
                Ok(creds) => {
                    nasty_common::cmd::try_run("systemctl", &["restart", "nasty-rest-server"])
                        .await;
                    ok(req, creds)
                }
                Err(e) => err(req, e.to_string()),
            }
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
                if let Err(e) = state.subvolumes.create(create_req, None).await {
                    // Without this log, the path write below succeeds
                    // but the subvolume actually doesn't exist —
                    // rest-server then refuses to start with a confusing
                    // "no such file" error and the user has nothing to
                    // tie the two together.
                    tracing::warn!("rest-server storage subvolume create failed: {e}");
                }
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
