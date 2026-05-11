//! RPC arms in the `vm.*` domain. Extracted from the historical
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
        "vm.capabilities" => ok(req, state.vms.capabilities().await),
        "vm.list" => match state.vms.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "vm.get" => match require_str(req, "id") {
            Ok(id) => match state.vms.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.create" => match parse_params(req) {
            Ok(p) => match state.vms.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.update" => match parse_params(req) {
            Ok(p) => match state.vms.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.delete" => match require_str(req, "id") {
            Ok(id) => match state.vms.delete(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.start" => match require_str(req, "id") {
            Ok(id) => match state.vms.start(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.stop" => match require_str(req, "id") {
            Ok(id) => match state.vms.stop(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.kill" => match require_str(req, "id") {
            Ok(id) => match state.vms.kill(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.snapshot" => match parse_params::<nasty_vm::SnapshotVmRequest>(req) {
            Ok(p) => match vm_snapshot(state, &p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.clone" => match parse_params::<nasty_vm::CloneVmRequest>(req) {
            Ok(p) => match vm_clone(state, &p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.images.list" => ok(req, list_vm_images(state).await),
        "vm.images.ensure" => match require_str(req, "filesystem") {
            Ok(fs) => match ensure_images_subvolume(state, fs).await {
                Ok(path) => ok(req, path),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}
