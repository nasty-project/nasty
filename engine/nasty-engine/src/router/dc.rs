//! RPC arms in the `dc.*` namespace (Active Directory Domain Controller
//! role — this box HOSTS the domain). Member mode lives in domain.rs.

use nasty_common::{Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::Session;

#[derive(Deserialize)]
struct UserCreateParams {
    name: String,
    password: String,
    #[serde(default)]
    given_name: Option<String>,
    #[serde(default)]
    surname: Option<String>,
}

#[derive(Deserialize)]
struct SetPasswordParams {
    name: String,
    password: String,
}

#[derive(Deserialize)]
struct GroupMemberParams {
    group: String,
    member: String,
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    _session: &Session,
) -> Option<Response> {
    Some(match req.method.as_str() {
        "dc.status" => ok(req, state.dc.status().await),
        "dc.provision" => match parse_params::<nasty_system::dc::ProvisionRequest>(req) {
            Ok(p) => match state.dc.provision(p).await {
                Ok((status, warnings)) => {
                    // DC ports open only after a successful provision.
                    match state.firewall.open_dc().await {
                        Ok(()) => ok(
                            req,
                            serde_json::json!({ "status": status, "warnings": warnings }),
                        ),
                        Err(e) => err(
                            req,
                            format!(
                                "domain controller provisioned but firewall update failed: {e}"
                            ),
                        ),
                    }
                }
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.demote" => match parse_params::<nasty_system::dc::DemoteRequest>(req) {
            Ok(p) => match state.dc.demote(p).await {
                Ok(()) => {
                    let firewall_result = state.firewall.close_dc().await;
                    // Teardown (inside dc.demote) stops samba-dc.service,
                    // but nothing else restarts the member-mode SMB units
                    // it had Conflicts=-swapped out — bring SMB back under
                    // its own protocol toggle, matching the spec's promise
                    // that shares resume without a reboot. dc.demote()
                    // clears dc.json before returning Ok, so this doesn't
                    // trip the enable() DC-hosting guard (#20). Best-effort:
                    // demote itself already succeeded, so a restart
                    // failure here is logged, not surfaced as an RPC error.
                    if state
                        .protocols
                        .is_enabled(nasty_system::protocol::Protocol::Smb)
                        .await
                        && let Err(e) = state.protocols.enable("smb").await
                    {
                        tracing::warn!("dc.demote: failed to restart SMB after demote: {e}");
                    }
                    match firewall_result {
                        Ok(()) => ok(req, "ok"),
                        Err(e) => err(
                            req,
                            format!("domain controller demoted but firewall update failed: {e}"),
                        ),
                    }
                }
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.backup" => match require_str(req, "dest") {
            Ok(dest) => match state.dc.backup(dest).await {
                Ok(path) => ok(req, serde_json::json!({ "path": path })),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.user.list" => match state.dc.user_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "dc.user.create" => match parse_params::<UserCreateParams>(req) {
            Ok(p) => match state
                .dc
                .user_create(
                    &p.name,
                    &p.password,
                    p.given_name.as_deref(),
                    p.surname.as_deref(),
                )
                .await
            {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.user.delete" => match require_str(req, "name") {
            Ok(name) => match state.dc.user_delete(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.user.set_password" => match parse_params::<SetPasswordParams>(req) {
            Ok(p) => match state.dc.user_set_password(&p.name, &p.password).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.user.enable" => match require_str(req, "name") {
            Ok(name) => match state.dc.user_enable(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.user.disable" => match require_str(req, "name") {
            Ok(name) => match state.dc.user_disable(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.group.list" => match state.dc.group_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "dc.group.create" => match require_str(req, "name") {
            Ok(name) => match state.dc.group_create(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.group.delete" => match require_str(req, "name") {
            Ok(name) => match state.dc.group_delete(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "dc.group.add_member" => match parse_params::<GroupMemberParams>(req) {
            Ok(p) => match state.dc.group_add_member(&p.group, &p.member).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.group.remove_member" => match parse_params::<GroupMemberParams>(req) {
            Ok(p) => match state.dc.group_remove_member(&p.group, &p.member).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "dc.computer.list" => match state.dc.computer_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        _ => return None,
    })
}
