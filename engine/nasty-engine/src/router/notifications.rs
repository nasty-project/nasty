//! RPC arms in the `notifications.*` domain. Extracted from the historical
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
        "notifications.config.get" => {
            ok(req, nasty_system::notifications::NotificationConfig::load())
        }
        "notifications.config.update" => {
            match parse_params::<nasty_system::notifications::NotificationConfig>(req) {
                Ok(config) => match config.save().await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => err(req, e),
            }
        }
        "notifications.test" => match parse_params::<nasty_system::notifications::ChannelType>(req)
        {
            Ok(channel) => match nasty_system::notifications::test_channel(&channel).await {
                Ok(msg) => ok(req, msg),
                Err(e) => err(req, e),
            },
            Err(e) => err(req, e),
        },
        _ => return None,
    })
}
