//! RPC arms in the `guestshare.*` domain — operator-managed guest file
//! shares (#474). The public, unauthenticated access surface lives
//! elsewhere (a later PR); these methods are admin/operator-only CRUD.
//!
//! `guestshare.list` / `guestshare.get` are reads (auto-allowed by
//! `is_read_only`); `guestshare.create` / `guestshare.revoke` are gated on
//! `is_operator_allowed` in `super`.

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
        "guestshare.list" => match state.guest_shares.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "guestshare.get" => match require_str(req, "id") {
            Ok(id) => match state.guest_shares.get(id).await {
                Ok(v) => ok(req, v),
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
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}
