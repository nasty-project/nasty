//! RPC arms in the `alerts.*` domain. Extracted from the historical
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
        "telemetry.send" => {
            let sent = crate::telemetry::send_report(state).await;
            ok(req, serde_json::json!({ "sent": sent }))
        }
        "system.alerts" => {
            // Cheap path for the WebUI dashboard, which polls every few seconds:
            // serve a cached result when it's <20s old. The cache is also
            // populated by the background notifier, so even with no browser
            // open the first WebUI poll after a minute returns instantly.
            {
                let cache = state.alerts_cache.lock().await;
                if let Some((ts, ref cached)) = *cache
                    && ts.elapsed() < std::time::Duration::from_secs(20)
                {
                    return Some(ok(req, cached.clone()));
                }
            }

            let alerts = evaluate_active_alerts(state).await;
            let value = serde_json::to_value(&alerts).unwrap_or_default();
            *state.alerts_cache.lock().await = Some((std::time::Instant::now(), value));

            ok(req, alerts)
        }
        "alert.rules.list" => ok(req, state.alerts.list_rules().await),
        "alert.rules.create" => match parse_params(req) {
            Ok(rule) => match state.alerts.create_rule(rule).await {
                Ok(r) => ok(req, r),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "alert.rules.update" => match parse_params::<nasty_system::alerts::AlertRuleUpdate>(req) {
            Ok(update) => match state.alerts.update_rule(&update.id.clone(), update).await {
                Ok(r) => ok(req, r),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "alert.rules.delete" => match require_str(req, "id") {
            Ok(id) => match state.alerts.delete_rule(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        _ => return None,
    })
}
