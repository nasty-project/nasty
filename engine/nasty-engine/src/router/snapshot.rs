//! RPC arms in the `snapshot.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

fn filesystem_scope_denied(scope: Option<&str>, requested: &str) -> bool {
    scope.is_some_and(|filesystem| filesystem != requested)
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    Some(match req.method.as_str() {
        "snapshot.list" => match require_str(req, "filesystem") {
            Ok(fs_name) => {
                if filesystem_scope_denied(session.filesystem.as_deref(), fs_name) {
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
        "snapshot.create" => {
            match parse_params::<nasty_storage::subvolume::CreateSnapshotRequest>(req) {
                Ok(p) if filesystem_scope_denied(session.filesystem.as_deref(), &p.filesystem) => {
                    err(req, "access denied")
                }
                Ok(p) => match state.snapshots.create(p, session.owner.as_deref()).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "snapshot.delete" => {
            match parse_params::<nasty_storage::subvolume::DeleteSnapshotRequest>(req) {
                Ok(p) if filesystem_scope_denied(session.filesystem.as_deref(), &p.filesystem) => {
                    err(req, "access denied")
                }
                Ok(p) => match state
                    .snapshots
                    .delete(p, session.owner.as_deref(), session.role == Role::Admin)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "snapshot.clone" => {
            match parse_params::<nasty_storage::subvolume::CloneSnapshotRequest>(req) {
                Ok(p) if filesystem_scope_denied(session.filesystem.as_deref(), &p.filesystem) => {
                    err(req, "access denied")
                }
                Ok(p) => match state
                    .snapshots
                    .clone_snapshot(p, session.owner.as_deref(), session.role == Role::Admin)
                    .await
                {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        // Whole-subvolume rollback: quiesce dependents (apps/VMs/shares),
        // swap the subvolume to the snapshot, resume. Destructive — takes a
        // safety snapshot first. Orchestrated in the engine layer.
        "snapshot.rollback" => {
            match parse_params::<nasty_storage::subvolume::RollbackSnapshotRequest>(req) {
                Ok(p) => {
                    if filesystem_scope_denied(session.filesystem.as_deref(), &p.filesystem) {
                        err(req, "access denied")
                    } else {
                        match crate::subvol_rollback::rollback_with_dependents(
                            state,
                            p,
                            session.owner.as_deref(),
                            session.role == Role::Admin,
                        )
                        .await
                        {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::filesystem_scope_denied;

    #[test]
    fn filesystem_scope_only_allows_its_configured_filesystem() {
        assert!(!filesystem_scope_denied(None, "tank"));
        assert!(!filesystem_scope_denied(Some("tank"), "tank"));
        assert!(filesystem_scope_denied(Some("tank"), "other"));
    }
}
