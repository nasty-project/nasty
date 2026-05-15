//! RPC arms in the `share.*` domain. Extracted from the historical
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
        "share.nfs.create" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nfs).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nfs.create(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nfs.update" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nfs).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nfs.update(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nfs.delete" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nfs).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nfs.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
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
        "share.smb.create" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.smb.create(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.smb.update" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.smb.update(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.smb.delete" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.smb.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
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
        "share.iscsi.create" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params::<nasty_sharing::iscsi::CreateTargetRequest>(req) {
                Ok(p) => {
                    if let Some(ref dp) = p.device_path
                        && let Some(conflict) =
                            check_block_device_conflict(state, dp, "iscsi").await
                    {
                        return Some(err(req, conflict));
                    }
                    match state.iscsi.create(p).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.delete" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.add_lun" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params::<nasty_sharing::iscsi::AddLunRequest>(req) {
                Ok(p) => {
                    if let Some(conflict) =
                        check_block_device_conflict(state, &p.backstore_path, "iscsi").await
                    {
                        err(req, conflict)
                    } else {
                        match state.iscsi.add_lun(p).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.remove_lun" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.remove_lun(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.add_acl" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.add_acl(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.remove_acl" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.remove_acl(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
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
        "share.nvmeof.create" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params::<nasty_sharing::nvmeof::CreateSubsystemRequest>(req) {
                Ok(p) => {
                    if let Some(ref device_path) = p.device_path
                        && let Some(conflict) =
                            check_block_device_conflict(state, device_path, "nvmeof").await
                    {
                        return Some(err(req, conflict));
                    }
                    match state.nvmeof.create(p).await {
                        Ok(v) => {
                            // If Tailscale is connected, add a port for its IP too
                            if !v.ports.is_empty() {
                                let ts = state.tailscale.get().await;
                                if ts.connected
                                    && let Some(ref ip) = ts.ip
                                {
                                    let nvmeof = state.nvmeof.clone();
                                    let id = v.id.clone();
                                    let ip = ip.clone();
                                    let nqn_for_log = v.nqn.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = nvmeof
                                            .add_port(nasty_sharing::nvmeof::AddPortRequest {
                                                subsystem_id: id,
                                                transport: Some("tcp".to_string()),
                                                addr: Some(ip.clone()),
                                                service_id: Some(4420),
                                                addr_family: Some("ipv4".to_string()),
                                            })
                                            .await
                                        {
                                            tracing::warn!(
                                                "auto-add Tailscale port for '{nqn_for_log}' on {ip} failed: {e}"
                                            );
                                        }
                                    });
                                }
                            }
                            ok(req, v)
                        }
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.delete" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.add_namespace" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params::<nasty_sharing::nvmeof::AddNamespaceRequest>(req) {
                Ok(p) => {
                    if let Some(conflict) =
                        check_block_device_conflict(state, &p.device_path, "nvmeof").await
                    {
                        err(req, conflict)
                    } else {
                        match state.nvmeof.add_namespace(p).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.remove_namespace" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.remove_namespace(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.add_port" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.add_port(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.remove_port" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.remove_port(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.add_host" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.add_host(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.remove_host" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.remove_host(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        _ => return None,
    })
}
