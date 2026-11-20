//! RPC arms in the `subvolume.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

fn filter_owner_visible_children(
    mut children: Vec<String>,
    visible_names: Option<&std::collections::HashSet<String>>,
) -> Vec<String> {
    let Some(visible_names) = visible_names else {
        return children;
    };
    children.retain(|child| visible_names.contains(child));
    children
}

fn owner_can_read_children(
    parent: &str,
    visible_names: Option<&std::collections::HashSet<String>>,
) -> bool {
    visible_names.is_none_or(|visible_names| visible_names.contains(parent))
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    let mutates_block_attachment = matches!(
        req.method.as_str(),
        "subvolume.create"
            | "subvolume.create_atomic"
            | "subvolume.delete"
            | "subvolume.attach"
            | "subvolume.detach"
            | "subvolume.clone"
            | "subvolume.clone_atomic"
    );
    let _block_share_guard = if mutates_block_attachment {
        Some(state.block_share_mutation.lock().await)
    } else {
        None
    };

    Some(match req.method.as_str() {
        "subvolume.list_all" => {
            let fs_filter = session.filesystem.as_deref();
            let owner_filter = session.owner.as_deref();
            match state.subvolumes.list_all(fs_filter, owner_filter).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        // Aggregate "what depends on each subvolume" — same shape as
        // fs.dependents but one level deeper. Read-only batched call:
        // the Subvolumes page's Usage column wants the value for every
        // row at once, so we walk each downstream service once and
        // bucket references by owning subvolume rather than paying
        // service-fanout N times.
        "subvolume.list_dependents" => {
            let dependents = crate::subvolume_dependents::find_all_subvolume_dependents(
                state,
                session.filesystem.as_deref(),
                session.owner.as_deref(),
            )
            .await;
            ok(req, dependents)
        }
        "subvolume.list" => match require_str(req, "filesystem") {
            Ok(fs_name) => {
                if session.filesystem.as_deref().is_some_and(|p| p != fs_name) {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
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
        "subvolume.get" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session.filesystem.as_deref().is_some_and(|p| p != fs_name) {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .get(fs_name, name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.children" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session.filesystem.as_deref().is_some_and(|p| p != fs_name) {
                    err(req, "access denied")
                } else {
                    let visible_names = if session.owner.is_some() {
                        match state
                            .subvolumes
                            .list(fs_name, session.owner.as_deref())
                            .await
                        {
                            Ok(visible) => Some(
                                visible
                                    .into_iter()
                                    .map(|subvolume| subvolume.name)
                                    .collect::<std::collections::HashSet<_>>(),
                            ),
                            Err(error) => return Some(err(req, error)),
                        }
                    } else {
                        None
                    };
                    if !owner_can_read_children(name, visible_names.as_ref()) {
                        return Some(err(req, "access denied"));
                    }
                    match state.subvolumes.list_children(fs_name, name).await {
                        Ok(children) => ok(
                            req,
                            filter_owner_visible_children(children, visible_names.as_ref()),
                        ),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.create" | "subvolume.create_atomic" => {
            match parse_params::<nasty_storage::subvolume::CreateSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        let owner = session.owner.clone();
                        let result = if req.method == "subvolume.create_atomic" {
                            state.subvolumes.create_atomic(p, owner).await
                        } else {
                            state.subvolumes.create(p, owner).await
                        };
                        match result {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.delete" => {
            match parse_params::<nasty_storage::subvolume::DeleteSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        let _apps_config_guard = state.apps.storage_config_guard().await;
                        if let Some(conflict) =
                            check_subvolume_in_use(state, &p.filesystem, &p.name).await
                        {
                            err(req, conflict)
                        } else {
                            match state.subvolumes.delete(p, session.owner.as_deref()).await {
                                Ok(()) => ok(req, "ok"),
                                Err(e) => err(req, e),
                            }
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.attach" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session.filesystem.as_deref().is_some_and(|p| p != fs_name) {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .attach(fs_name, name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.detach" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session.filesystem.as_deref().is_some_and(|p| p != fs_name) {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .detach(fs_name, name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.resize" => {
            match parse_params::<nasty_storage::subvolume::ResizeSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state.subvolumes.resize(p, session.owner.as_deref()).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.update" => {
            match parse_params::<nasty_storage::subvolume::UpdateSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state.subvolumes.update(p, session.owner.as_deref()).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.clone" | "subvolume.clone_atomic" => {
            match parse_params::<nasty_storage::subvolume::CloneSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        let result = if req.method == "subvolume.clone_atomic" {
                            state
                                .subvolumes
                                .clone_subvolume_atomic(p, session.owner.as_deref())
                                .await
                        } else {
                            state
                                .subvolumes
                                .clone_subvolume(p, session.owner.as_deref())
                                .await
                        };
                        match result {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.set_properties" => {
            match parse_params::<nasty_storage::subvolume::SetPropertiesRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|sp| sp != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state
                            .subvolumes
                            .set_properties(p, session.owner.as_deref())
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
        "subvolume.remove_properties" => {
            match parse_params::<nasty_storage::subvolume::RemovePropertiesRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .is_some_and(|sp| sp != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state
                            .subvolumes
                            .remove_properties(p, session.owner.as_deref())
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
        "subvolume.find_by_property" => {
            match parse_params::<nasty_storage::subvolume::FindByPropertyRequest>(req) {
                Ok(p) => {
                    // Enforce filesystem-scoped token restriction
                    let effective_fs = match (&session.filesystem, &p.filesystem) {
                        (Some(sp), Some(rp)) if sp != rp => {
                            return Some(err(req, "access denied"));
                        }
                        (Some(sp), None) => Some(nasty_storage::subvolume::FindByPropertyRequest {
                            filesystem: Some(sp.clone()),
                            key: p.key.clone(),
                            value: p.value.clone(),
                        }),
                        _ => None,
                    };
                    let req_effective = effective_fs.unwrap_or(p);
                    match state
                        .subvolumes
                        .find_by_property(req_effective, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
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
    use std::collections::HashSet;

    use super::{filter_owner_visible_children, owner_can_read_children};

    #[test]
    fn child_inventory_requires_a_visible_parent_and_filters_hidden_children() {
        let children = vec!["parent/own".to_string(), "parent/foreign".to_string()];
        let visible = HashSet::from(["parent".to_string(), "parent/own".to_string()]);
        assert_eq!(
            filter_owner_visible_children(children, Some(&visible)),
            vec!["parent/own"]
        );
        assert!(owner_can_read_children("parent", Some(&visible)));
        assert!(!owner_can_read_children("foreign-parent", Some(&visible)));
        assert!(owner_can_read_children("foreign-parent", None));
    }
}
