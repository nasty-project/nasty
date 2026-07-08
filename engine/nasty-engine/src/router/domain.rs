//! RPC arms in the `domain.*` namespace (Active Directory member mode).

use nasty_common::{Request, Response};

use super::*;
use crate::AppState;
use crate::auth::Session;

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    _session: &Session,
) -> Option<Response> {
    Some(match req.method.as_str() {
        "domain.status" => ok(req, state.domain.status().await),
        "domain.join" => match parse_params::<nasty_system::domain::JoinDomainRequest>(req) {
            Ok(p) => match state.domain.join(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "domain.leave" => match parse_params::<nasty_system::domain::LeaveDomainRequest>(req) {
            Ok(p) => match state.domain.leave(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "domain.user.list" => match require_str(req, "prefix") {
            Ok(prefix) => match state.domain.search_users(prefix).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "domain.group.list" => match require_str(req, "prefix") {
            Ok(prefix) => match state.domain.search_groups(prefix).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}
