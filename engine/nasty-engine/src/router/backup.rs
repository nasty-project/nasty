//! RPC arms in the `backup.*` domain. Extracted from the historical
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
        "backup.profile.list" => ok(req, state.backups.list_profiles().await),
        "backup.profile.get" => match require_str(req, "id") {
            Ok(id) => match state.backups.get_profile(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.profile.create" => match parse_params::<nasty_backup::BackupProfile>(req) {
            Ok(p) => match state.backups.create_profile(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(e) => err(req, e),
        },
        "backup.profile.update" => {
            let id = match require_str(req, "id") {
                Ok(s) => s.to_string(),
                Err(r) => return Some(r),
            };
            match parse_params::<nasty_backup::BackupProfile>(req) {
                Ok(p) => match state.backups.update_profile(&id, p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e.to_rpc_error()),
                },
                Err(e) => err(req, e),
            }
        }
        "backup.profile.delete" => match require_str(req, "id") {
            Ok(id) => match state.backups.delete_profile(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.status" => ok(req, state.backups.status().await),
        "backup.repo.init" => match require_str(req, "id") {
            Ok(id) => match state.backups.init_repo(id).await {
                Ok(msg) => ok(req, msg),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.run" => {
            let id = match require_str(req, "id") {
                Ok(s) => s.to_string(),
                Err(r) => return Some(r),
            };
            // Run in background, return immediately. The RPC ack is just
            // "we accepted the request" — the actual backup status lands
            // in the journal, with a per-backup-id error so the user can
            // grep for *which* backup failed.
            let backups = state.backups.clone_for_task();
            let id_for_log = id.clone();
            tokio::spawn(async move {
                if let Err(e) = backups.run_backup(&id).await {
                    tracing::warn!("backup '{id_for_log}' failed: {e}");
                }
            });
            ok(req, "Backup started")
        }
        "backup.snapshots" => match require_str(req, "id") {
            Ok(id) => match state.backups.list_snapshots(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.repo.check" => match require_str(req, "id") {
            Ok(id) => match state.backups.check_repo(id).await {
                Ok(msg) => ok(req, msg),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.secrets_status" => ok(req, state.backups.secrets_status().await),
        _ => return None,
    })
}
