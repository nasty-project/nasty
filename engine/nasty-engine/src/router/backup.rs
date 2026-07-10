//! RPC arms in the `backup.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

#[derive(Deserialize)]
struct RestoreParams {
    id: String,
    snapshot_id: String,
    dest: String,
    #[serde(default)]
    allow_overwrite: bool,
}

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
        // The init / run / check RPCs return a BackupJob handle now.
        // Long-running ops would otherwise blow through the 10 s
        // WebSocket request timeout in the WebUI client — observed
        // with `backup.repo.init` against a remote REST target
        // taking 32 s. Clients poll backup.jobs.get / backup.jobs.list
        // to watch the Pending → Running → Succeeded|Failed transition.
        "backup.repo.init" => match require_str(req, "id") {
            Ok(id) => match state.backups.start_init_repo(id).await {
                Ok(job) => ok(req, job),
                Err(e) => err(req, e.to_string()),
            },
            Err(r) => r,
        },
        "backup.run" => match require_str(req, "id") {
            Ok(id) => match state.backups.start_run_backup(id).await {
                Ok(job) => ok(req, job),
                Err(e) => err(req, e.to_string()),
            },
            Err(r) => r,
        },
        "backup.snapshots" => match require_str(req, "id") {
            Ok(id) => match state.backups.list_snapshots(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.restore" => match parse_params::<RestoreParams>(req) {
            Ok(p) => match state
                .backups
                .start_restore(&p.id, &p.snapshot_id, &p.dest, p.allow_overwrite)
                .await
            {
                Ok(job) => ok(req, job),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(e) => err(req, e),
        },
        "backup.repo.check" => match require_str(req, "id") {
            Ok(id) => match state.backups.start_check_repo(id).await {
                Ok(job) => ok(req, job),
                Err(e) => err(req, e.to_string()),
            },
            Err(r) => r,
        },
        "backup.jobs.list" => {
            // Optional `profile_id` filter — empty / missing returns all.
            let profile_id = req
                .params
                .as_ref()
                .and_then(|p| p.get("profile_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            ok(req, state.backups.jobs().list(profile_id).await)
        }
        "backup.jobs.get" => match require_str(req, "id") {
            Ok(job_id) => match state.backups.jobs().get(job_id).await {
                Some(job) => ok(req, job),
                None => err(req, format!("backup job not found: {job_id}")),
            },
            Err(r) => r,
        },
        "backup.secrets_status" => ok(req, state.backups.secrets_status().await),
        _ => return None,
    })
}
