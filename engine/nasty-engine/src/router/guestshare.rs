//! RPC arms in the `guestshare.*` domain — operator-managed guest file
//! shares (#474). Every method is limited to unscoped Admin/Operator sessions;
//! management responses are redacted separately from persisted records.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{EndpointAccess, Role, Session, authorize_session};
use crate::guestshare::GuestShareInfo;

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    if let Err(denied) = authorize_session(session, EndpointAccess::UnscopedMutation) {
        crate::auth::audit(
            "permission_denied",
            &session.username,
            session.client_ip.as_deref().unwrap_or("unknown"),
            &format!("method={} reason={}", req.method, denied.message()),
        );
        return Some(Response::error(
            req.id.clone(),
            ErrorCode::InternalError,
            denied.message(),
        ));
    }

    Some(match req.method.as_str() {
        "guestshare.list" => match state.guest_shares.list().await {
            Ok(v) => ok(req, v.iter().map(GuestShareInfo::from).collect::<Vec<_>>()),
            Err(e) => err(req, e),
        },
        "guestshare.get" => match require_str(req, "id") {
            Ok(id) => match state.guest_shares.get(id).await {
                Ok(v) => ok(req, GuestShareInfo::from(&v)),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "guestshare.create" => match parse_params(req) {
            Ok(p) => match state.guest_shares.create(p, &session.username).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "guestshare.revoke" => match require_str(req, "id") {
            Ok(id) => match state.guest_shares.revoke(id).await {
                Ok(v) => {
                    audit_share(session, "guest_share_revoked", id);
                    ok(req, GuestShareInfo::from(&v))
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "guestshare.remove" => match require_str(req, "id") {
            Ok(id) => match state.guest_shares.remove(id).await {
                Ok(()) => {
                    audit_share(session, "guest_share_removed", id);
                    ok(req, "ok")
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}

/// Append an audit entry for an operator action on a guest share, attributed
/// to the session user + their client IP.
fn audit_share(session: &Session, event: &str, share_id: &str) {
    crate::auth::audit(
        event,
        &session.username,
        session.client_ip.as_deref().unwrap_or("unknown"),
        &format!("share_id={share_id}"),
    );
}
