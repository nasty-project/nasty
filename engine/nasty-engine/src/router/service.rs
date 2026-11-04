//! RPC arms in the `service.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{EndpointAccess, Session};

fn protocol_mutation_access(protocol: nasty_system::protocol::Protocol) -> EndpointAccess {
    if protocol.is_system_service() {
        EndpointAccess::RootEquivalent
    } else {
        EndpointAccess::UnscopedMutation
    }
}

fn rest_server_requires_root_equivalent(method: &str) -> bool {
    matches!(
        method,
        "service.rest_server.configure"
            | "service.rest_server.credentials"
            | "service.rest_server.rotate_credentials"
    )
}

fn validate_rest_server_path(path: &str) -> Result<(), String> {
    if !std::path::Path::new(path).is_absolute()
        || std::path::Path::new(path)
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        || path.chars().any(char::is_control)
    {
        return Err(
            "Backup Server path must be absolute, without '..' or control characters".into(),
        );
    }
    Ok(())
}

fn require_protocol_mutation_access(
    req: &Request,
    session: &Session,
    protocol: nasty_system::protocol::Protocol,
) -> Option<Response> {
    match protocol_mutation_access(protocol) {
        EndpointAccess::RootEquivalent => {
            require_root_equivalent(req, session, "system_service_protocol_mutation")
        }
        EndpointAccess::UnscopedMutation => {
            require_unscoped_mutation(req, session, "global_share_protocol_mutation")
        }
        _ => unreachable!("protocol mutations use an explicit access tier"),
    }
}

pub(super) async fn protocol_has_admin_only_sources(
    state: &AppState,
    protocol: nasty_system::protocol::Protocol,
) -> Result<bool, String> {
    match protocol {
        nasty_system::protocol::Protocol::Iscsi => {
            let targets = state
                .iscsi
                .list()
                .await
                .map_err(|error| error.to_string())?;
            for lun in targets.iter().flat_map(|target| &target.luns) {
                match lun.backstore_type.as_str() {
                    "block" if lun.backing_volume.is_none() || lun.backing_volume_unresolved => {
                        return Ok(true);
                    }
                    "fileio"
                        if !super::share::fileio_source_is_managed(&lun.backstore_path).await =>
                    {
                        return Ok(true);
                    }
                    "block" | "fileio" => {}
                    _ => return Ok(true),
                }
            }
            Ok(false)
        }
        nasty_system::protocol::Protocol::Nvmeof => state
            .nvmeof
            .list()
            .await
            .map(|subsystems| {
                subsystems
                    .iter()
                    .flat_map(|subsystem| &subsystem.namespaces)
                    .any(|namespace| {
                        namespace.backing_volume.is_none() || namespace.backing_volume_unresolved
                    })
            })
            .map_err(|error| error.to_string()),
        _ => Ok(false),
    }
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    if rest_server_requires_root_equivalent(&req.method)
        && let Some(response) =
            require_root_equivalent(req, session, "global_rest_server_management")
    {
        return Some(response);
    }
    let block_protocol_mutation = matches!(
        req.method.as_str(),
        "service.protocol.enable" | "service.protocol.disable"
    ) && req.params.as_ref().is_some_and(|params| {
        matches!(
            params.get("name").and_then(serde_json::Value::as_str),
            Some("iscsi" | "nvmeof")
        )
    });
    let _block_share_guard = if block_protocol_mutation {
        Some(state.block_share_mutation.lock().await)
    } else {
        None
    };

    Some(match req.method.as_str() {
        "service.protocol.list" => ok(req, state.protocols.list().await),
        "service.protocol.enable" => match require_str(req, "name") {
            Ok(name) => {
                if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                    if let Some(response) = require_protocol_mutation_access(req, session, proto) {
                        return Some(response);
                    }
                    if matches!(
                        proto,
                        nasty_system::protocol::Protocol::Iscsi
                            | nasty_system::protocol::Protocol::Nvmeof
                    ) {
                        match protocol_has_admin_only_sources(state, proto).await {
                            Ok(true) => {
                                if let Some(response) = require_root_equivalent(
                                    req,
                                    session,
                                    "raw_block_protocol_restore",
                                ) {
                                    return Some(response);
                                }
                            }
                            Ok(false) => {}
                            Err(error) => return Some(err(req, error)),
                        }
                        let mappings = state.subvolumes.restore_block_devices().await;
                        let safe = match proto {
                            nasty_system::protocol::Protocol::Iscsi => {
                                let outcome = state.iscsi.remap_device_paths(&mappings).await;
                                outcome.safe_to_restore
                            }
                            nasty_system::protocol::Protocol::Nvmeof => {
                                let outcome = state.nvmeof.remap_device_paths(&mappings).await;
                                outcome.safe_to_restore
                            }
                            _ => true,
                        };
                        if !safe {
                            let quiesce_error = match proto {
                                nasty_system::protocol::Protocol::Iscsi => {
                                    state.protocols.quiesce(proto).await.err()
                                }
                                nasty_system::protocol::Protocol::Nvmeof => state
                                    .nvmeof
                                    .quiesce()
                                    .await
                                    .err()
                                    .map(|error| error.to_string()),
                                _ => None,
                            };
                            let _ = state.firewall.close(proto).await;
                            return Some(err(
                                req,
                                if let Some(quiesce_error) = quiesce_error {
                                    format!(
                                        "{} backing volumes could not be resolved safely; live export quiescing also failed: {quiesce_error}",
                                        proto.display_name()
                                    )
                                } else {
                                    format!(
                                        "{} backing volumes could not be resolved safely",
                                        proto.display_name()
                                    )
                                },
                            ));
                        }
                        if proto == nasty_system::protocol::Protocol::Iscsi
                            && let Err(error) = state.protocols.quiesce(proto).await
                        {
                            return Some(err(
                                req,
                                format!("failed to reload remapped iSCSI state: {error}"),
                            ));
                        }
                    }
                    match state.firewall.open(proto).await {
                        Err(e) => err(req, format!("firewall update failed: {e}")),
                        Ok(()) => match state.protocols.enable(name).await {
                            Ok(v) => {
                                if proto == nasty_system::protocol::Protocol::Nvmeof
                                    && let Err(error) = state.nvmeof.restore().await
                                {
                                    let quiesce = state.nvmeof.quiesce().await.err();
                                    let disable = state.protocols.disable(name).await.err();
                                    let close = state.firewall.close(proto).await.err();
                                    let mut message =
                                        format!("failed to restore NVMe-oF safely: {error}");
                                    if let Some(error) = quiesce {
                                        message.push_str(&format!(
                                            "; live namespace quiescing also failed: {error}"
                                        ));
                                    }
                                    if let Some(error) = disable {
                                        message.push_str(&format!(
                                            "; protocol rollback also failed: {error}"
                                        ));
                                    }
                                    if let Some(error) = close {
                                        message.push_str(&format!(
                                            "; firewall rollback also failed: {error}"
                                        ));
                                    }
                                    return Some(err(req, message));
                                }
                                ok(req, v)
                            }
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
            Ok(name) => {
                if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                    if let Some(response) = require_protocol_mutation_access(req, session, proto) {
                        return Some(response);
                    }
                    match state.protocols.disable(name).await {
                        Ok(v) => match state.firewall.close(proto).await {
                            Ok(()) => ok(req, v),
                            Err(e) => err(
                                req,
                                format!("protocol disabled but firewall update failed: {e}"),
                            ),
                        },
                        Err(e) => err(req, e),
                    }
                } else {
                    match state.protocols.disable(name).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
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
                serde_json::json!({ "iqn_prefix": iqn.trim().to_ascii_lowercase(), "nqn_prefix": nqn.trim() }),
            )
        }
        "service.base_names.update" => {
            if let Some(iqn) = req
                .params
                .as_ref()
                .and_then(|p| p.get("iqn_prefix"))
                .and_then(|v| v.as_str())
            {
                let prefix = match nasty_sharing::iscsi::normalize_iqn_prefix(iqn) {
                    Ok(prefix) => prefix,
                    Err(error) => return Some(err(req, error)),
                };
                if let Err(error) = tokio::fs::write("/var/lib/nasty/iscsi-base-iqn", prefix).await
                {
                    return Some(err(req, format!("persist iscsi base IQN failed: {error}")));
                }
            }
            if let Some(nqn) = req
                .params
                .as_ref()
                .and_then(|p| p.get("nqn_prefix"))
                .and_then(|v| v.as_str())
                && let Err(error) =
                    nasty_sharing::nvmeof::NvmeofService::update_base_nqn_prefix(nqn).await
            {
                return Some(err(req, error));
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
            if let Err(error) = validate_rest_server_path(&path) {
                return Some(err(req, error));
            }

            // Create subvolume if path is under /fs/ and doesn't exist
            if path.starts_with("/fs/") && !std::path::Path::new(&path).exists() {
                let Some((fs_name, subvol_name)) = path[4..].split_once('/') else {
                    return Some(err(
                        req,
                        "Backup Server path must specify /fs/<filesystem>/<subvolume>",
                    ));
                };
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
                    block_filesystem: None,
                    initial_properties: std::collections::HashMap::new(),
                };
                if let Err(e) = state.subvolumes.create(create_req, None).await {
                    return Some(err(
                        req,
                        format!("Backup Server storage could not be created: {e}"),
                    ));
                }
            }

            if !path.starts_with("/fs/")
                && let Err(error) = tokio::fs::create_dir_all(&path).await
            {
                return Some(err(req, format!("create Backup Server directory: {error}")));
            }
            if !std::path::Path::new(&path).is_dir() {
                return Some(err(
                    req,
                    format!("Backup Server path is not a directory: {path}"),
                ));
            }

            if let Err(e) = tokio::fs::write("/var/lib/nasty/rest-server-path", &path).await {
                return Some(err(req, format!("write config: {e}")));
            }

            // A disabled service reads the path on its next enable. Restart
            // only when enabled and report a failed apply distinctly from
            // the already-persisted path setting.
            if state
                .protocols
                .is_enabled(nasty_system::protocol::Protocol::RestServer)
                .await
            {
                if let Err(error) =
                    nasty_common::cmd::run_ok("systemctl", &["restart", "nasty-rest-server"]).await
                {
                    return Some(err(
                        req,
                        format!("Backup Server path saved, but restart failed: {error}"),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if !state
                    .protocols
                    .is_running(nasty_system::protocol::Protocol::RestServer)
                    .await
                {
                    return Some(err(
                        req,
                        "Backup Server path saved, but service failed to stay running",
                    ));
                }
            }

            ok(req, "ok")
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        protocol_mutation_access, rest_server_requires_root_equivalent, validate_rest_server_path,
    };
    use crate::auth::{EndpointAccess, Role, Session, authorize_session};
    use nasty_system::protocol::Protocol;

    fn session(role: Role, scoped: bool) -> Session {
        Session {
            token: "token".into(),
            username: "user".into(),
            role,
            file_principal: None,
            filesystem: scoped.then(|| "tank".into()),
            owner: None,
            created_at: 0,
            must_change_password: false,
            client_ip: None,
        }
    }

    #[test]
    fn rest_server_storage_path_must_be_absolute_without_parent_traversal() {
        assert!(validate_rest_server_path("/fs/tank/backups").is_ok());
        assert!(validate_rest_server_path("/var/lib/nasty/backups").is_ok());
        assert!(validate_rest_server_path("backups").is_err());
        assert!(validate_rest_server_path("/fs/tank/../elsewhere").is_err());
        assert!(validate_rest_server_path("/fs/tank/backup\nelsewhere").is_err());
    }

    #[test]
    fn share_protocol_mutations_allow_only_unscoped_operators_and_admins() {
        for protocol in [
            Protocol::Nfs,
            Protocol::Smb,
            Protocol::Iscsi,
            Protocol::Nvmeof,
        ] {
            let access = protocol_mutation_access(protocol);
            assert_eq!(access, EndpointAccess::UnscopedMutation);
            assert!(authorize_session(&session(Role::Operator, false), access).is_ok());
            assert!(authorize_session(&session(Role::Admin, false), access).is_ok());
            assert!(authorize_session(&session(Role::Operator, true), access).is_err());
            assert!(authorize_session(&session(Role::Admin, true), access).is_err());
            assert!(authorize_session(&session(Role::ReadOnly, false), access).is_err());
        }
    }

    #[test]
    fn system_service_mutations_require_an_unscoped_admin() {
        for protocol in [
            Protocol::Nut,
            Protocol::Ssh,
            Protocol::Avahi,
            Protocol::Smart,
            Protocol::Watchdog,
            Protocol::RestServer,
        ] {
            let access = protocol_mutation_access(protocol);
            assert_eq!(access, EndpointAccess::RootEquivalent);
            assert!(authorize_session(&session(Role::Admin, false), access).is_ok());
            assert!(authorize_session(&session(Role::Admin, true), access).is_err());
            assert!(authorize_session(&session(Role::Operator, false), access).is_err());
            assert!(authorize_session(&session(Role::ReadOnly, false), access).is_err());
        }
    }

    #[test]
    fn rest_server_secrets_and_mutations_require_root_equivalent_access() {
        for method in [
            "service.rest_server.configure",
            "service.rest_server.credentials",
            "service.rest_server.rotate_credentials",
        ] {
            assert!(rest_server_requires_root_equivalent(method), "{method}");
        }
        assert!(!rest_server_requires_root_equivalent(
            "service.rest_server.config"
        ));

        let access = EndpointAccess::RootEquivalent;
        assert!(authorize_session(&session(Role::Admin, false), access).is_ok());
        assert!(authorize_session(&session(Role::Admin, true), access).is_err());
        assert!(authorize_session(&session(Role::Operator, false), access).is_err());
        assert!(authorize_session(&session(Role::ReadOnly, false), access).is_err());
    }
}
