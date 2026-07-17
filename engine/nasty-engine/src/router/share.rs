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
    let resp = route_inner(req, state, session).await?;
    // iSCSI/NVMe-oF firewall rules follow the configured portal ports
    // (#602). Resync after any mutation that can change them; gate
    // refusals and errors return early above or carry `error`, so only
    // real changes pay the recompute.
    let changes_portals = matches!(
        req.method.as_str(),
        "share.iscsi.create"
            | "share.iscsi.delete"
            | "share.iscsi.add_portal"
            | "share.iscsi.remove_portal"
            | "share.iscsi.set_portals"
            | "share.nvmeof.create"
            | "share.nvmeof.delete"
            | "share.nvmeof.add_port"
            | "share.nvmeof.remove_port"
    );
    if changes_portals && let Err(e) = sync_portal_firewall_ports(state).await {
        if resp.error.is_none() {
            return Some(err(
                req,
                format!("share changed but firewall port synchronization failed: {e}"),
            ));
        }
        tracing::warn!("portal firewall reconciliation after failed request also failed: {e}");
    }
    Some(resp)
}

/// Recompute the iSCSI and NVMe-oF firewall port sets from the
/// configured portals. Defaults to the protocol's standard port while
/// no targets exist, so enabling the protocol opens what the next
/// `create` will bind. NVMe-oF RDMA listeners are excluded — RoCE
/// rides udp/4791 under the separate `rdma` rule, and native IB never
/// traverses netfilter.
pub(crate) async fn portal_firewall_ports(
    state: &AppState,
) -> Result<
    (
        Vec<nasty_system::firewall::PortSpec>,
        Vec<nasty_system::firewall::PortSpec>,
    ),
    String,
> {
    use nasty_system::firewall::{PortSpec, Transport};
    use std::collections::BTreeSet;

    let tcp = |port: u16| PortSpec {
        port,
        to: None,
        transport: Transport::Tcp,
        source: None,
        iface: None,
    };

    let mut iscsi_ports: BTreeSet<u16> = BTreeSet::new();
    let targets = state
        .iscsi
        .list()
        .await
        .map_err(|e| format!("list iSCSI targets: {e}"))?;
    iscsi_ports.extend(
        targets
            .iter()
            .flat_map(|target| target.portals.iter().map(|portal| portal.port)),
    );
    if iscsi_ports.is_empty() {
        iscsi_ports.insert(3260);
    }

    let mut nvmeof_ports: BTreeSet<u16> = BTreeSet::new();
    let subsystems = state
        .nvmeof
        .list()
        .await
        .map_err(|e| format!("list NVMe-oF subsystems: {e}"))?;
    nvmeof_ports.extend(subsystems.iter().flat_map(|subsystem| {
        subsystem
            .ports
            .iter()
            .filter(|port| port.transport == "tcp")
            .filter_map(|port| port.service_id.parse::<u16>().ok())
    }));
    if nvmeof_ports.is_empty() {
        nvmeof_ports.insert(4420);
    }
    Ok((
        iscsi_ports.into_iter().map(tcp).collect(),
        nvmeof_ports.into_iter().map(tcp).collect(),
    ))
}

pub(crate) async fn sync_portal_firewall_ports(state: &AppState) -> Result<(), String> {
    let _sync = state.portal_firewall_sync.lock().await;
    let (iscsi_ports, nvmeof_ports) = portal_firewall_ports(state).await?;
    state
        .firewall
        .set_portal_ports(iscsi_ports, nvmeof_ports)
        .await
}

async fn route_inner(req: &Request, state: &AppState, session: &Session) -> Option<Response> {
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
                    if p.portals
                        .as_deref()
                        .is_some_and(|ps| ps.iter().any(|portal| portal.iser))
                        && let Some(r) = require_rdma(req, "ib_isert").await
                    {
                        return Some(r);
                    }
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
        "share.iscsi.add_portal" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params::<nasty_sharing::iscsi::AddPortalRequest>(req) {
                Ok(p) => {
                    if p.iser
                        && let Some(r) = require_rdma(req, "ib_isert").await
                    {
                        return Some(r);
                    }
                    match state.iscsi.add_portal(p).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.set_portals" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params::<nasty_sharing::iscsi::SetPortalsRequest>(req) {
                Ok(p) => {
                    if p.portals.iter().any(|portal| portal.iser)
                        && let Some(r) = require_rdma(req, "ib_isert").await
                    {
                        return Some(r);
                    }
                    match state.iscsi.set_portals(p).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.remove_portal" => {
            if let Some(r) =
                require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await
            {
                return Some(r);
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.remove_portal(p).await {
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
                                    && let Err(e) = state
                                        .nvmeof
                                        .add_port(nasty_sharing::nvmeof::AddPortRequest {
                                            subsystem_id: v.id.clone(),
                                            transport: Some("tcp".to_string()),
                                            addr: Some(ip.clone()),
                                            service_id: Some(4420),
                                            addr_family: Some("ipv4".to_string()),
                                        })
                                        .await
                                {
                                    tracing::warn!(
                                        "auto-add Tailscale port for '{}' on {ip} failed: {e}",
                                        v.nqn
                                    );
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
            match parse_params::<nasty_sharing::nvmeof::AddPortRequest>(req) {
                Ok(p) => {
                    if p.transport.as_deref() == Some("rdma")
                        && let Some(r) = require_rdma(req, "nvmet-rdma").await
                    {
                        return Some(r);
                    }
                    match state.nvmeof.add_port(p).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
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
