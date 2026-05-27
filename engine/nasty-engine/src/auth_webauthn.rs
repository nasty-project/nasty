//! WebAuthn registration + management for issue #289 PR #1.
//!
//! This PR is registration-only: an operator can register a security
//! key / passkey / Touch ID under their account, list what's
//! registered, and delete entries. PR #2 wires the same credentials
//! into a third login path (start/finish assertion → session token).
//!
//! ## RP ID derivation
//!
//! WebAuthn binds every credential to one effective domain (RP ID)
//! forever — moving NASty between hostnames silently invalidates
//! existing credentials. We pick the RP ID from the same source
//! Caddy uses for its primary TLS SAN:
//!
//!   - `settings.tls_domain` if set (operator's managed public host),
//!   - else `nasty.local` (the always-present internal-CA fallback).
//!
//! This matches "what hostname is the operator actually typing into
//! their browser" on the box that owns the trust root. Changes to
//! `tls_domain` after credentials exist will brick them — that's a
//! WebAuthn-spec constraint, not a NASty choice. The fix in the
//! field is "re-register your keys under the new domain"; PR #3
//! will surface this in the settings UI.
//!
//! ## Why server-side registration state
//!
//! webauthn-rs's `PasskeyRegistration` is the half-finished
//! challenge-and-policy state the assertion needs. The spec doesn't
//! require it server-side, but the alternative (round-trip it
//! through the browser opaquely) shifts trust to the client and
//! complicates the JSON contract. We hold it in an in-memory
//! HashMap with a 5-minute TTL, keyed by the per-flow id we hand
//! the client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;
use webauthn_rs::Webauthn;
use webauthn_rs::WebauthnBuilder;
use webauthn_rs::prelude::{
    CreationChallengeResponse, CredentialID, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse,
};

use crate::auth::{AuthService, WebauthnCredential};

/// Default RP ID when `settings.tls_domain` isn't set. Matches the
/// internal-CA SAN Caddy always serves (`nasty.local`), so a fresh
/// install with no operator-configured public host still has a
/// usable RP ID for LAN access.
pub const DEFAULT_RP_ID: &str = "nasty.local";

/// How long a half-finished registration lives before being garbage
/// collected. WebAuthn UX is "click button → tap key", typically
/// seconds; 5 min is generous and bounds the memory of the in-flight
/// cache against forgotten flows.
const REGISTRATION_TTL: Duration = Duration::from_secs(5 * 60);

/// Maximum number of in-flight registrations any single user can
/// have. Prevents a runaway client (or buggy retry loop) from
/// growing the cache unboundedly.
const MAX_IN_FLIGHT_PER_USER: usize = 4;

/// Free-form length cap on credential labels. WebAuthn doesn't care;
/// this is just to keep the operator-facing UI from being abused.
const MAX_LABEL_LEN: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum WebauthnError {
    #[error("webauthn-rs error: {0}")]
    Backend(#[from] webauthn_rs::prelude::WebauthnError),
    #[error("no in-flight registration for id {0}")]
    UnknownRegistration(String),
    #[error("no in-flight authentication for id {0}")]
    UnknownAuthentication(String),
    #[error("label is empty")]
    EmptyLabel,
    #[error("label too long (max {MAX_LABEL_LEN} chars)")]
    LabelTooLong,
    #[error("credential not found")]
    CredentialNotFound,
    #[error("user has no registered security keys")]
    NoCredentials,
    #[error("auth state I/O failed: {0}")]
    Auth(String),
}

/// Service that owns the `Webauthn` instance and the in-flight
/// registration cache. Constructed once at engine startup; the
/// per-request methods take `&self` and mutate the registration
/// cache through an interior `Mutex`. RP ID and origin are
/// captured at construction time and don't change for the life of
/// the service (changing them mid-flight would void every
/// registered credential — a WebAuthn-spec constraint, not a
/// software limitation).
pub struct WebauthnService {
    webauthn: Webauthn,
    rp_id: String,
    pending: Arc<Mutex<HashMap<String, PendingRegistration>>>,
    /// In-flight authentication challenges, keyed by the per-request
    /// `auth_id` the WebUI round-trips between login.start and
    /// login.finish. Same TTL + per-user cap as the registration cache;
    /// kept separate because the lifecycle and `PasskeyAuthentication`
    /// shape differ from registration.
    pending_auth: Arc<Mutex<HashMap<String, PendingAuthentication>>>,
}

struct PendingRegistration {
    username: String,
    label: String,
    state: PasskeyRegistration,
    started_at: Instant,
}

struct PendingAuthentication {
    username: String,
    state: PasskeyAuthentication,
    started_at: Instant,
}

/// Wire shape returned by `register.start`. The `creation_options`
/// goes directly into `navigator.credentials.create({ publicKey:
/// ... })` on the browser side; the `registration_id` is round-
/// tripped back via `register.finish` so we can pair the browser's
/// response with the server-side `PasskeyRegistration` state.
#[derive(Debug, Serialize)]
pub struct RegisterStartResponse {
    pub registration_id: String,
    pub creation_options: CreationChallengeResponse,
}

/// Wire shape for `auth.webauthn.config` — the read-only view of
/// what the engine has pinned at startup, so the WebUI can
/// pre-check the operator's current origin before attempting
/// `navigator.credentials.create`. IP origins, mismatched hostnames
/// and plain http:// all fail at the browser with cryptic errors;
/// surfacing the RP ID up front lets the UI render an actionable
/// message ("you're on 10.10.10.74 but this NASty registers
/// credentials under nasty.local — visit https://nasty.local").
#[derive(Debug, Serialize)]
pub struct WebauthnConfig {
    /// RP ID baked into this engine instance. Operators must access
    /// NASty via a hostname that equals this string or is a
    /// subdomain of it; the browser refuses to issue a credential
    /// for any other origin.
    pub rp_id: String,
}

/// Wire shape returned by `/api/auth/webauthn/login/start`. The
/// `request_options` goes straight into `@simplewebauthn/browser`'s
/// `startAuthentication`; the `auth_id` round-trips back via
/// `login/finish` so we can pair the browser's response with the
/// matching server-side `PasskeyAuthentication` state.
#[derive(Debug, Serialize)]
pub struct LoginStartResponse {
    pub auth_id: String,
    pub request_options: RequestChallengeResponse,
}

/// Wire shape for `auth.webauthn.list`. Each row is the metadata
/// the operator sees in account settings — no public key, no
/// internal webauthn-rs state.
#[derive(Debug, Serialize)]
pub struct CredentialSummary {
    pub label: String,
    pub created_at: u64,
    /// Base64url credential ID, stable across listings so the WebUI
    /// can pass it back to `auth.webauthn.delete` without needing
    /// the label (labels aren't unique by intent — operators can
    /// reuse "YubiKey" if they like).
    pub credential_id: String,
}

impl WebauthnService {
    /// Build the service from a fully-formed RP ID. The origin is
    /// constructed as `https://{rp_id}` — WebAuthn requires HTTPS
    /// (with localhost as the only exception, irrelevant here).
    pub fn new(rp_id: &str) -> Result<Self, WebauthnError> {
        let origin_str = format!("https://{rp_id}");
        let origin = Url::parse(&origin_str)
            .map_err(|e| WebauthnError::Auth(format!("invalid RP origin {origin_str}: {e}")))?;
        let webauthn = WebauthnBuilder::new(rp_id, &origin)?
            .rp_name("NASty")
            .build()?;
        Ok(Self {
            webauthn,
            rp_id: rp_id.to_string(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_auth: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Convenience: pick the RP ID from a `Settings` snapshot. See
    /// module docs for the policy (tls_domain → that domain; None →
    /// `nasty.local`).
    pub fn rp_id_from_settings(settings: &nasty_system::settings::Settings) -> String {
        settings
            .tls_domain
            .as_deref()
            .and_then(|s| {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .unwrap_or_else(|| DEFAULT_RP_ID.to_string())
    }

    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }

    /// Snapshot the engine-pinned WebAuthn config for the WebUI's
    /// origin precheck. Read-only — no state to mutate, just
    /// echoes the RP ID baked at construction.
    pub fn config(&self) -> WebauthnConfig {
        WebauthnConfig {
            rp_id: self.rp_id.clone(),
        }
    }

    /// Step 1 of registration: build a challenge for the browser.
    /// `label` is the operator-typed credential nickname; trimmed
    /// and length-checked here so the rejection happens before any
    /// crypto.
    pub async fn register_start(
        &self,
        auth: &AuthService,
        username: &str,
        label: &str,
    ) -> Result<RegisterStartResponse, WebauthnError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(WebauthnError::EmptyLabel);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(WebauthnError::LabelTooLong);
        }

        // Exclude credentials already registered to this user. Mirrors
        // the WebAuthn spec recommendation — prevents the same
        // authenticator from being registered twice, which would
        // produce two credentials with the same private key on the
        // device and confuse subsequent assertions.
        let existing = auth.webauthn_credentials_for(username).await;
        let exclude: Vec<CredentialID> = existing
            .iter()
            .map(|c| c.passkey.cred_id().clone())
            .collect();

        // Deterministic per-user UUID. webauthn-rs wants a stable
        // identifier so re-registrations across browsers/sessions
        // bind to the same user record; using v5 with the username
        // means a username rename is the only way to drift, and that's
        // already a "you need to re-register everything" event.
        let user_uuid = Uuid::new_v5(&Uuid::NAMESPACE_DNS, username.as_bytes());

        let (creation_options, state) = self.webauthn.start_passkey_registration(
            user_uuid,
            username,
            username,
            Some(exclude),
        )?;

        let registration_id = base64_token();
        let mut pending = self.pending.lock().await;
        prune_expired(&mut pending);
        // Cap per-user in-flight count.
        let same_user_count = pending.values().filter(|p| p.username == username).count();
        if same_user_count >= MAX_IN_FLIGHT_PER_USER {
            warn!(
                target: "nasty::webauthn",
                "user '{username}' has {same_user_count} pending registrations; refusing new one"
            );
            return Err(WebauthnError::Auth(format!(
                "too many in-flight registrations for {username} (limit {MAX_IN_FLIGHT_PER_USER})",
            )));
        }
        pending.insert(
            registration_id.clone(),
            PendingRegistration {
                username: username.to_string(),
                label: label.to_string(),
                state,
                started_at: Instant::now(),
            },
        );
        debug!(
            target: "nasty::webauthn",
            "started registration {registration_id} for user '{username}' label='{label}'"
        );
        Ok(RegisterStartResponse {
            registration_id,
            creation_options,
        })
    }

    /// Step 2 of registration: finalize after the browser's
    /// `navigator.credentials.create` resolves. On success, the
    /// credential is persisted under the user's record and removed
    /// from the in-flight cache.
    pub async fn register_finish(
        &self,
        auth: &AuthService,
        username: &str,
        registration_id: &str,
        response: &RegisterPublicKeyCredential,
    ) -> Result<CredentialSummary, WebauthnError> {
        let pending_entry = {
            let mut pending = self.pending.lock().await;
            prune_expired(&mut pending);
            pending.remove(registration_id)
        };
        let pending_entry = pending_entry
            .ok_or_else(|| WebauthnError::UnknownRegistration(registration_id.to_string()))?;

        // Defence-in-depth: the registration was issued for one
        // username; the session-authenticated caller's username
        // must match. Without this an attacker who intercepted a
        // start response could finish under a different account.
        if pending_entry.username != username {
            warn!(
                target: "nasty::webauthn",
                "register.finish: session user '{username}' mismatches registration user '{}'",
                pending_entry.username
            );
            return Err(WebauthnError::Auth(
                "registration_id does not belong to this session".into(),
            ));
        }

        let passkey = self
            .webauthn
            .finish_passkey_registration(response, &pending_entry.state)?;

        let created_at = unix_now();
        let credential_id = base64_url(passkey.cred_id().as_ref());
        let credential = WebauthnCredential {
            label: pending_entry.label,
            created_at,
            passkey,
        };
        auth.add_webauthn_credential(username, credential.clone())
            .await
            .map_err(|e| WebauthnError::Auth(e.to_string()))?;

        Ok(CredentialSummary {
            label: credential.label,
            created_at: credential.created_at,
            credential_id,
        })
    }

    /// List the calling user's registered credentials. Returns the
    /// metadata the WebUI shows, not the public key — that stays in
    /// the engine and is only consumed by webauthn-rs at assertion.
    pub async fn list(&self, auth: &AuthService, username: &str) -> Vec<CredentialSummary> {
        auth.webauthn_credentials_for(username)
            .await
            .iter()
            .map(|c| CredentialSummary {
                label: c.label.clone(),
                created_at: c.created_at,
                credential_id: base64_url(c.passkey.cred_id().as_ref()),
            })
            .collect()
    }

    /// Remove a credential by ID (the base64url string returned in
    /// `CredentialSummary.credential_id`). Returns `CredentialNotFound`
    /// when the ID doesn't match anything under this user — useful
    /// for the UI's "stale list" case where the entry was already
    /// deleted from another tab.
    pub async fn delete(
        &self,
        auth: &AuthService,
        username: &str,
        credential_id_b64: &str,
    ) -> Result<(), WebauthnError> {
        let target_id = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            credential_id_b64.trim_end_matches('='),
        )
        .map_err(|e| WebauthnError::Auth(format!("bad credential_id base64: {e}")))?;
        auth.remove_webauthn_credential(username, &target_id)
            .await
            .map_err(|e| match e {
                crate::auth::AuthError::UserNotFound | crate::auth::AuthError::NotFound => {
                    WebauthnError::CredentialNotFound
                }
                other => WebauthnError::Auth(other.to_string()),
            })
    }

    /// Step 1 of WebAuthn login: build an assertion challenge bound to
    /// the named user's existing credentials. The `allowCredentials`
    /// list (built from the user's registered passkeys) tells the
    /// browser which keys are acceptable for this assertion; the
    /// authenticator that the operator taps must own one of them.
    ///
    /// `NoCredentials` when the user has no registered keys — caller
    /// surfaces this as "this account has no security key set up,
    /// use a password." We could mask it to avoid user enumeration
    /// but `/api/login` already enumerates via its 401 response so
    /// the added information here is negligible.
    pub async fn login_start(
        &self,
        auth: &AuthService,
        username: &str,
    ) -> Result<LoginStartResponse, WebauthnError> {
        let stored = auth.webauthn_credentials_for(username).await;
        if stored.is_empty() {
            return Err(WebauthnError::NoCredentials);
        }
        let passkeys: Vec<_> = stored.iter().map(|c| c.passkey.clone()).collect();
        let (request_options, state) = self.webauthn.start_passkey_authentication(&passkeys)?;

        let auth_id = base64_token();
        let mut pending = self.pending_auth.lock().await;
        prune_expired_auth(&mut pending);
        let same_user_count = pending
            .values()
            .filter(|p| p.username == username)
            .count();
        if same_user_count >= MAX_IN_FLIGHT_PER_USER {
            warn!(
                target: "nasty::webauthn",
                "user '{username}' has {same_user_count} pending authentications; refusing new one"
            );
            return Err(WebauthnError::Auth(format!(
                "too many in-flight authentications for {username} (limit {MAX_IN_FLIGHT_PER_USER})",
            )));
        }
        pending.insert(
            auth_id.clone(),
            PendingAuthentication {
                username: username.to_string(),
                state,
                started_at: Instant::now(),
            },
        );
        debug!(
            target: "nasty::webauthn",
            "started authentication {auth_id} for user '{username}'"
        );
        Ok(LoginStartResponse {
            auth_id,
            request_options,
        })
    }

    /// Step 2 of WebAuthn login: verify the browser's assertion
    /// against the stored credential. On success, persists the
    /// updated `sign_count` (replay protection — webauthn-rs's
    /// `finish_passkey_authentication` rejects assertions whose
    /// sign_count is ≤ the stored value, so we must save the new
    /// one or every subsequent assertion on the same credential
    /// would fail) and returns the verified username so the REST
    /// handler can mint a session token.
    pub async fn login_finish(
        &self,
        auth: &AuthService,
        auth_id: &str,
        response: &PublicKeyCredential,
    ) -> Result<String, WebauthnError> {
        let pending_entry = {
            let mut pending = self.pending_auth.lock().await;
            prune_expired_auth(&mut pending);
            pending.remove(auth_id)
        };
        let pending_entry = pending_entry
            .ok_or_else(|| WebauthnError::UnknownAuthentication(auth_id.to_string()))?;

        let auth_result = self
            .webauthn
            .finish_passkey_authentication(response, &pending_entry.state)?;

        // Update the matching credential's sign_count in place. The
        // operator's other credentials are untouched. Failure here
        // (concurrent delete, state corruption) is logged but not
        // fatal — the assertion itself was valid, so we proceed with
        // session minting; the next assertion will simply hit the
        // sign_count check freshly.
        if let Err(e) = auth
            .update_webauthn_sign_count(&pending_entry.username, &auth_result)
            .await
        {
            warn!(
                target: "nasty::webauthn",
                "post-assertion sign_count update for '{}' failed: {e}",
                pending_entry.username,
            );
        }

        Ok(pending_entry.username)
    }
}

fn prune_expired(pending: &mut HashMap<String, PendingRegistration>) {
    let now = Instant::now();
    pending.retain(|_, p| now.duration_since(p.started_at) < REGISTRATION_TTL);
}

fn prune_expired_auth(pending: &mut HashMap<String, PendingAuthentication>) {
    let now = Instant::now();
    pending.retain(|_, p| now.duration_since(p.started_at) < REGISTRATION_TTL);
}

fn base64_token() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 18];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_domain(domain: Option<&str>) -> nasty_system::settings::Settings {
        nasty_system::settings::Settings {
            tls_domain: domain.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn rp_id_falls_back_to_nasty_local() {
        let rp_id = WebauthnService::rp_id_from_settings(&settings_with_domain(None));
        assert_eq!(rp_id, DEFAULT_RP_ID);
    }

    #[test]
    fn rp_id_uses_tls_domain_when_set() {
        let rp_id =
            WebauthnService::rp_id_from_settings(&settings_with_domain(Some("nas.example.com")));
        assert_eq!(rp_id, "nas.example.com");
    }

    #[test]
    fn rp_id_treats_empty_tls_domain_as_unset() {
        // Empty / whitespace tls_domain comes back from settings that
        // were poked via the WebUI and then cleared. Must not produce
        // an empty RP ID — that would fail WebauthnBuilder construction
        // with an unhelpful error. Fall back to nasty.local instead.
        let rp_id = WebauthnService::rp_id_from_settings(&settings_with_domain(Some("   ")));
        assert_eq!(rp_id, DEFAULT_RP_ID);
    }

    #[test]
    fn service_constructs_with_default_rp_id() {
        // Sanity check: the builder accepts our default + the
        // `https://nasty.local` derived origin. Catches typos in
        // either string.
        let svc = WebauthnService::new(DEFAULT_RP_ID).expect("builder");
        assert_eq!(svc.rp_id(), DEFAULT_RP_ID);
    }

    #[test]
    fn service_constructs_with_real_domain() {
        let svc = WebauthnService::new("nas.example.com").expect("builder");
        assert_eq!(svc.rp_id(), "nas.example.com");
    }
}
