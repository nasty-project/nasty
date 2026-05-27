//! RPC arms in the `auth.*` domain. Extracted from the historical
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
        "auth.me" => ok(
            req,
            serde_json::json!({
                "username": session.username,
                "role": session.role,
            }),
        ),
        "auth.logout" => match state.auth.logout(&session.token).await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "auth.change_password" => {
            #[derive(Deserialize)]
            struct P {
                username: String,
                new_password: String,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .auth
                    .change_password(session, &p.username, &p.new_password)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.create_user" => {
            #[derive(Deserialize)]
            struct P {
                username: String,
                password: String,
                role: Role,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .auth
                    .create_user(session, &p.username, &p.password, p.role)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.delete_user" => match require_str(req, "username") {
            Ok(username) => match state.auth.delete_user(session, username).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "auth.list_users" => ok(req, state.auth.list_users().await),
        "auth.token.list" => match state.auth.list_api_tokens(session).await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "auth.token.create" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
                role: Role,
                filesystem: Option<String>,
                expires_in_secs: Option<u64>,
                #[serde(default)]
                allowed_ips: Vec<String>,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .auth
                    .create_api_token(
                        session,
                        &p.name,
                        p.role,
                        p.filesystem,
                        p.expires_in_secs,
                        p.allowed_ips,
                    )
                    .await
                {
                    Ok(t) => ok(req, t),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.token.delete" => match require_str(req, "id") {
            Ok(id) => match state.auth.delete_api_token(session, id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "auth.oidc.config_status" => {
            let oidc = state.settings.get().await.oidc;
            ok(req, nasty_system::settings::redact_oidc_secret(oidc))
        }
        "auth.oidc.update_config" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            match parse_params::<nasty_system::settings::OidcSettings>(req) {
                Ok(new_settings) => {
                    let enabled = new_settings.enabled;
                    match state.settings.set_oidc(new_settings).await {
                        Ok(saved) => {
                            // Rebuild the live OIDC client to reflect the new config
                            // (or tear it down when disabling).
                            let merged = state.settings.get().await.oidc;
                            let rebuild =
                                state.oidc.rebuild(&merged).await.map_err(|e| e.to_string());
                            crate::auth::audit(
                                "oidc_config_changed",
                                &session.username,
                                session.client_ip.as_deref().unwrap_or(""),
                                &format!("enabled={enabled}"),
                            );
                            match rebuild {
                                Ok(()) => ok(req, saved),
                                Err(e) => {
                                    err(req, format!("config saved but client rebuild failed: {e}"))
                                }
                            }
                        }
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "auth.oidc.test" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            let sample = req
                .params
                .as_ref()
                .and_then(|p| p.get("claims"))
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let oidc = state.settings.get().await.oidc;
            match crate::auth_oidc::dry_run_role(&sample, &oidc) {
                Ok(role) => ok(req, serde_json::json!({ "role": role })),
                Err(e) => err(req, e),
            }
        }
        // ── WebAuthn (issue #289 PR #1: registration management) ──
        // Login via webauthn lands in PR #2; for now these only let
        // an authenticated user enroll and manage their own keys.
        "auth.webauthn.config" => ok(req, state.webauthn.config()),
        "auth.webauthn.register.start" => {
            #[derive(Deserialize)]
            struct P {
                label: String,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .webauthn
                    .register_start(&state.auth, &session.username, &p.label)
                    .await
                {
                    Ok(r) => ok(req, r),
                    Err(e) => err(req, e.to_string()),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.webauthn.register.finish" => {
            #[derive(Deserialize)]
            struct P {
                registration_id: String,
                response: webauthn_rs::prelude::RegisterPublicKeyCredential,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .webauthn
                    .register_finish(
                        &state.auth,
                        &session.username,
                        &p.registration_id,
                        &p.response,
                    )
                    .await
                {
                    Ok(r) => ok(req, r),
                    Err(e) => err(req, e.to_string()),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.webauthn.list" => ok(
            req,
            state.webauthn.list(&state.auth, &session.username).await,
        ),
        "auth.webauthn.delete" => {
            #[derive(Deserialize)]
            struct P {
                credential_id: String,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .webauthn
                    .delete(&state.auth, &session.username, &p.credential_id)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e.to_string()),
                },
                Err(e) => invalid(req, e),
            }
        }
        // Admin recovery for the "user lost every authenticator"
        // case. AuthService re-checks the admin role; we also gate
        // here so a non-admin call doesn't even reach the state
        // mutation path. Audit log written by AuthService.
        "auth.webauthn.reset_for_user" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            match require_str(req, "username") {
                Ok(target) => match state.auth.reset_webauthn_credentials(session, target).await {
                    Ok(removed) => ok(req, serde_json::json!({ "removed": removed })),
                    Err(e) => err(req, e),
                },
                Err(r) => r,
            }
        }
        _ => return None,
    })
}
