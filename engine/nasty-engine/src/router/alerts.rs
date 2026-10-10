//! RPC arms in the `alerts.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

fn alert_requires_unscoped(method: &str) -> bool {
    matches!(
        method,
        "system.alerts"
            | "alert.acknowledge"
            | "alert.rules.list"
            | "alert.rules.create"
            | "alert.rules.update"
            | "alert.rules.delete"
    )
}

fn alert_scope_access_error(method: &str, session: &Session) -> Option<&'static str> {
    (alert_requires_unscoped(method) && (session.filesystem.is_some() || session.owner.is_some()))
        .then_some("access denied: alert management requires unscoped credentials")
}

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    if let Some(message) = alert_scope_access_error(&req.method, session) {
        return Some(err(req, message));
    }
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

            let (evaluated, revision) = evaluate_active_alerts(state).await;
            let alerts: Vec<_> = evaluated
                .into_iter()
                .filter(|alert| !alert.acknowledged)
                .collect();
            let value = serde_json::to_value(&alerts).unwrap_or_default();
            let mut cache = state.alerts_cache.lock().await;
            if revision == state.alerts.revision() {
                *cache = Some((std::time::Instant::now(), value));
            }

            ok(req, alerts)
        }
        "alert.acknowledge" => match require_str(req, "instance_id") {
            Ok(instance_id) => {
                match acknowledge_active_alert(state, instance_id, &session.username).await {
                    Ok(acknowledgement) => {
                        *state.alerts_cache.lock().await = None;
                        *state.status_cache.lock().await = None;
                        ok(req, acknowledgement)
                    }
                    Err(e) => err(req, e),
                }
            }
            Err(r) => r,
        },
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
                Ok(r) => {
                    *state.alerts_cache.lock().await = None;
                    *state.status_cache.lock().await = None;
                    ok(req, r)
                }
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

#[cfg(test)]
mod tests {
    use super::{alert_requires_unscoped, alert_scope_access_error};
    use crate::auth::{Role, Session};

    fn session(filesystem: bool, owner: bool) -> Session {
        Session {
            token: "token".into(),
            username: "user".into(),
            role: Role::ReadOnly,
            file_principal: None,
            filesystem: filesystem.then(|| "tank".into()),
            owner: owner.then(|| "token-a".into()),
            created_at: 0,
            must_change_password: false,
            client_ip: None,
        }
    }

    #[test]
    fn global_alert_reads_and_mutations_require_unscoped_credentials() {
        for method in [
            "system.alerts",
            "alert.acknowledge",
            "alert.rules.list",
            "alert.rules.create",
            "alert.rules.update",
            "alert.rules.delete",
        ] {
            assert!(alert_requires_unscoped(method), "{method}");
            assert!(alert_scope_access_error(method, &session(true, false)).is_some());
            assert!(alert_scope_access_error(method, &session(false, true)).is_some());
            assert!(alert_scope_access_error(method, &session(false, false)).is_none());
        }
        assert!(!alert_requires_unscoped("telemetry.send"));
    }
}
