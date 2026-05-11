//! RPC arms in the `snapshot.*` domain. Extracted from the historical
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
        "snapshot.list" => match require_str(req, "filesystem") {
            Ok(fs_name) => {
                if session.filesystem.as_deref().is_some_and(|p| p != fs_name) {
                    err(req, "access denied")
                } else {
                    match state
                        .snapshots
                        .list(fs_name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "snapshot.create" => match parse_params(req) {
            Ok(p) => match state.snapshots.create(p, session.owner.as_deref()).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "snapshot.delete" => match parse_params(req) {
            Ok(p) => match state.snapshots.delete(p, session.owner.as_deref()).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "snapshot.clone" => match parse_params(req) {
            Ok(p) => match state
                .snapshots
                .clone_snapshot(p, session.owner.as_deref())
                .await
            {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        _ => return None,
    })
}
