use std::path::{Path, PathBuf};
use std::sync::Arc;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;

const STATE_PATH: &str = "/var/lib/nasty/auth.json";
const STATE_DIR: &str = "/var/lib/nasty";
const AUDIT_LOG_PATH: &str = "/var/lib/nasty/audit.log";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    /// Argon2 password hash. None for users provisioned via OIDC who have never set
    /// a local password; such users can only authenticate through the configured IdP.
    #[serde(default)]
    pub password_hash: Option<String>,
    pub role: Role,
    /// When true, the user must change their password before accessing anything else.
    #[serde(default)]
    pub must_change_password: bool,
    /// OIDC subject identifier (`sub` claim). Set on first OIDC login.
    #[serde(default)]
    pub oidc_subject: Option<String>,
    /// OIDC issuer URL. Pinned alongside the subject so a sub from a different IdP
    /// never collides with an existing user.
    #[serde(default)]
    pub oidc_issuer: Option<String>,
    /// WebAuthn credentials registered to this user. PR #1 of issue #289 —
    /// PR #2 wires these into a third login path; PR #1 only manages
    /// registration/listing/deletion. Empty for users who never enrolled
    /// a security key. Stored inline in `auth.json` (one user, one
    /// blob) because the credential count is bounded (operators
    /// typically register 1–3) and centralising under `User` keeps
    /// the "delete user wipes everything they own" invariant trivial.
    #[serde(default)]
    pub webauthn_credentials: Vec<WebauthnCredential>,
}

/// One registered WebAuthn credential. Wraps webauthn-rs's `Passkey`
/// (which carries the credential ID, public key, sign counter,
/// attestation transports, and the policy bits webauthn-rs needs at
/// assertion time) with NASty-facing metadata: a free-form label
/// the operator types ("Personal YubiKey", "Touch ID on laptop")
/// and the creation timestamp for the management UI.
///
/// `Passkey` serializes to a deterministic JSON shape — round-trips
/// through serde without losing any of webauthn-rs's internal state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebauthnCredential {
    /// Operator-friendly label shown in account settings. Required;
    /// the registration UI prompts for it before invoking the
    /// browser's create() call. Trimmed but not otherwise validated —
    /// any printable string is fine.
    pub label: String,
    /// Unix seconds at registration time, for the "added on …" line
    /// in the list view.
    pub created_at: u64,
    /// The webauthn-rs Passkey blob. Treat as opaque — round-trip
    /// only, never construct or inspect outside `auth_webauthn.rs`.
    pub passkey: webauthn_rs::prelude::Passkey,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    ReadOnly,
    /// Can create/delete/attach subvolumes and snapshots, read filesystems.
    /// Cannot destroy filesystems, manage users, or touch system settings.
    Operator,
}

/// Login sessions expire after this many seconds.
const SESSION_TTL_SECS: u64 = 8 * 3600; // 8 hours

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub token: String,
    pub username: String,
    pub role: Role,
    /// For API tokens: restricts filesystem visibility to a single filesystem.
    #[serde(default)]
    pub filesystem: Option<String>,
    /// For API tokens: only subvolumes with this owner are visible/manageable.
    #[serde(default)]
    pub owner: Option<String>,
    /// Unix timestamp when this session was created.
    pub created_at: u64,
    /// When true, the user must change their password before doing anything else.
    #[serde(default)]
    pub must_change_password: bool,
    /// Client IP that created this session. Requests from other IPs are rejected.
    #[serde(default)]
    pub client_ip: Option<String>,
}

/// Authorization policy for engine endpoints that do not pass through the
/// central JSON-RPC dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointAccess {
    /// Any authenticated role. Filesystem/owner scope is enforced separately
    /// by handlers that operate on scoped resources.
    Read,
    /// Operator or Admin. Resource scope is enforced by the handler.
    Mutation,
    /// Operator or Admin, but only with an unscoped session.
    UnscopedMutation,
    /// Admin only, with no filesystem or owner scope.
    RootEquivalent,
    /// Session-management operations that remain available while a password
    /// change is required, such as checking the current session or logout.
    SelfService,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDenied {
    PasswordChangeRequired,
    InsufficientRole,
    ScopedCredential,
}

impl AccessDenied {
    pub fn message(self) -> &'static str {
        match self {
            Self::PasswordChangeRequired => "Password change required",
            Self::InsufficientRole => "Permission denied",
            Self::ScopedCredential => "Scoped credentials cannot access this endpoint",
        }
    }
}

/// Apply the authorization rules shared by direct HTTP and specialized
/// WebSocket endpoints. Resource-specific filesystem/owner checks remain in
/// the relevant handler after this coarse endpoint gate.
pub fn authorize_session(session: &Session, access: EndpointAccess) -> Result<(), AccessDenied> {
    if session.must_change_password && access != EndpointAccess::SelfService {
        return Err(AccessDenied::PasswordChangeRequired);
    }

    let role_allowed = match access {
        EndpointAccess::Read | EndpointAccess::SelfService => true,
        EndpointAccess::Mutation | EndpointAccess::UnscopedMutation => {
            matches!(session.role, Role::Admin | Role::Operator)
        }
        EndpointAccess::RootEquivalent => session.role == Role::Admin,
    };
    if !role_allowed {
        return Err(AccessDenied::InsufficientRole);
    }

    if matches!(
        access,
        EndpointAccess::UnscopedMutation | EndpointAccess::RootEquivalent
    ) && (session.filesystem.is_some() || session.owner.is_some())
    {
        return Err(AccessDenied::ScopedCredential);
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    /// Argon2 hash of the token value. The raw token is returned only once on creation.
    pub token: String,
    pub role: Role,
    pub created_at: u64,
    /// If set, token can only see/manage subvolumes in this filesystem.
    #[serde(default)]
    pub filesystem: Option<String>,
    /// Unix timestamp after which the token is rejected. None = never expires.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// If set, token is only accepted from these IP addresses. Empty = any IP.
    #[serde(default)]
    pub allowed_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiTokenInfo {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub created_at: u64,
    pub filesystem: Option<String>,
    pub expires_at: Option<u64>,
    pub allowed_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AuthState {
    users: Vec<User>,
    sessions: Vec<Session>,
    api_tokens: Vec<ApiToken>,
    initialized: bool,
}

/// Per-username failed login limit. Tighter than the per-IP limit because
/// a username is the natural attacker target.
const MAX_FAILED_ATTEMPTS: usize = 5;
/// Window for counting per-username failures and the lockout duration.
const LOCKOUT_WINDOW_SECS: u64 = 15 * 60; // 15 minutes

/// Per-IP failed login limit, applied across *all* usernames. Stops a single
/// IP from spraying many usernames at low per-username rates.
const MAX_IP_FAILED_ATTEMPTS: usize = 20;
/// Window for counting per-IP failures.
const IP_LOCKOUT_WINDOW_SECS: u64 = 60 * 60; // 1 hour

const RATE_LIMIT_PATH: &str = "/var/lib/nasty/auth-rate-limit.json";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct RateLimitState {
    /// Failure timestamps per username.
    #[serde(default)]
    by_user: std::collections::HashMap<String, Vec<u64>>,
    /// Failure timestamps per source IP.
    #[serde(default)]
    by_ip: std::collections::HashMap<String, Vec<u64>>,
}

impl RateLimitState {
    /// Drop entries older than their relevant window so the file doesn't
    /// grow without bound.
    fn prune(&mut self, now: u64) {
        for v in self.by_user.values_mut() {
            v.retain(|&t| now.saturating_sub(t) < LOCKOUT_WINDOW_SECS);
        }
        self.by_user.retain(|_, v| !v.is_empty());
        for v in self.by_ip.values_mut() {
            v.retain(|&t| now.saturating_sub(t) < IP_LOCKOUT_WINDOW_SECS);
        }
        self.by_ip.retain(|_, v| !v.is_empty());
    }

    fn user_failures(&self, username: &str, now: u64) -> usize {
        self.by_user
            .get(username)
            .map(|v| {
                v.iter()
                    .filter(|&&t| now.saturating_sub(t) < LOCKOUT_WINDOW_SECS)
                    .count()
            })
            .unwrap_or(0)
    }

    fn ip_failures(&self, ip: &str, now: u64) -> usize {
        self.by_ip
            .get(ip)
            .map(|v| {
                v.iter()
                    .filter(|&&t| now.saturating_sub(t) < IP_LOCKOUT_WINDOW_SECS)
                    .count()
            })
            .unwrap_or(0)
    }
}

pub struct AuthService {
    state: Arc<RwLock<AuthState>>,
    /// Failed-login tracking, keyed both per-username (tight) and per-IP
    /// (broad spray). Persisted to disk so an engine restart does not reset
    /// the lockout — that would let an attacker dodge limits by killing the
    /// service via, say, a memory-pressure DoS.
    rate_limit: Arc<RwLock<RateLimitState>>,
}

impl AuthService {
    pub async fn new() -> Result<Self, AuthError> {
        let state = initialize_state(Path::new(STATE_PATH), Some(0)).await?;
        let mut rl = load_rate_limit().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        rl.prune(now);
        Ok(Self {
            state: Arc::new(RwLock::new(state)),
            rate_limit: Arc::new(RwLock::new(rl)),
        })
    }

    /// Authenticate with username/password, returns a session token
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        client_ip: &str,
    ) -> Result<String, AuthError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Per-IP rate limit applies first, regardless of whether the username
        // exists. This blocks a single attacker spraying many usernames from
        // one address — the per-username limit alone wouldn't catch that.
        // Skip the bucket "unknown" because it pools every legitimate user
        // behind a misconfigured proxy with every potential attacker.
        if client_ip != "unknown" {
            let rl = self.rate_limit.read().await;
            let ip_fails = rl.ip_failures(client_ip, now);
            if ip_fails >= MAX_IP_FAILED_ATTEMPTS {
                tracing::warn!(
                    "Login blocked from {client_ip}: {ip_fails} failed attempts in the last hour"
                );
                audit(
                    "login_ip_locked",
                    username,
                    client_ip,
                    &format!("{ip_fails} failed attempts"),
                );
                return Err(AuthError::AccountLocked);
            }
        }

        // Per-username lockout is suppressed when the locked user is the only
        // Admin in the system — locking them would lock the whole appliance.
        // The per-IP limit above still protects against spray; a distributed
        // attacker is still slowed by per-IP limits at every source they use.
        let only_admin = self.is_only_local_admin(username).await;
        {
            let rl = self.rate_limit.read().await;
            let user_fails = rl.user_failures(username, now);
            if user_fails >= MAX_FAILED_ATTEMPTS {
                if only_admin {
                    tracing::warn!(
                        "Per-username lockout for '{username}' suppressed (only Admin); per-IP limit still applies"
                    );
                    audit(
                        "login_lockout_suppressed_only_admin",
                        username,
                        client_ip,
                        &format!("{user_fails} failed attempts"),
                    );
                } else {
                    tracing::warn!(
                        "Login blocked for '{}': {} failed attempts in last {} minutes",
                        username,
                        user_fails,
                        LOCKOUT_WINDOW_SECS / 60
                    );
                    audit(
                        "login_locked",
                        username,
                        client_ip,
                        &format!("{user_fails} failed attempts"),
                    );
                    return Err(AuthError::AccountLocked);
                }
            }
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();

        let user = state
            .users
            .iter()
            .find(|u| u.username == username)
            .ok_or(AuthError::InvalidCredentials);

        let user = match user {
            Ok(u) => u,
            Err(e) => {
                audit("login_failed", username, client_ip, "user not found");
                self.record_failed_attempt(username, client_ip, now).await;
                return Err(e);
            }
        };

        let hash = match user.password_hash.as_deref() {
            Some(h) => h,
            None => {
                audit(
                    "login_failed",
                    username,
                    client_ip,
                    "no local password (OIDC-only user)",
                );
                self.record_failed_attempt(username, client_ip, now).await;
                return Err(AuthError::InvalidCredentials);
            }
        };
        match verify_password(password, hash) {
            Ok(()) => {}
            Err(e) => {
                audit("login_failed", username, client_ip, "wrong password");
                self.record_failed_attempt(username, client_ip, now).await;
                return Err(e);
            }
        }

        // Successful login — clear per-username failures. We deliberately
        // leave per-IP failures alone: a successful login from one user does
        // not "redeem" the IP if it was just spraying other usernames.
        self.clear_failed_attempts(username).await;

        let token = generate_token();
        let session = Session {
            token: token.clone(),
            username: user.username.clone(),
            role: user.role.clone(),
            filesystem: None,
            owner: None,
            created_at: now,
            must_change_password: user.must_change_password,
            client_ip: Some(client_ip.to_string()),
        };

        // Prune expired sessions while we hold the write lock
        state
            .sessions
            .retain(|s| now - s.created_at <= SESSION_TTL_SECS);

        state.sessions.push(session);
        commit_state(&mut current, state).await?;

        audit("login_success", username, client_ip, "");
        info!("User '{}' logged in", username);
        Ok(token)
    }

    /// Mint a session for a verified OIDC identity. Looks up the user by
    /// (oidc_subject, oidc_issuer); auto-provisions a new user if enabled and
    /// no match exists. Re-derives role from the supplied groups every login,
    /// so IdP group changes propagate without admin action.
    ///
    /// `derived_role` is computed by the caller (router/HTTP handler) using
    /// `auth_oidc::role_for_groups` against the current `OidcSettings`.
    /// Passing it in keeps this function free of `nasty_system` types.
    pub async fn login_or_provision_oidc(
        &self,
        identity: &crate::auth_oidc::OidcIdentity,
        derived_role: Option<Role>,
        auto_provision: bool,
        client_ip: &str,
    ) -> Result<String, AuthError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let role = match derived_role {
            Some(r) => r,
            None => {
                audit(
                    "oidc_login_denied_no_role",
                    identity
                        .preferred_username
                        .as_deref()
                        .unwrap_or(&identity.subject),
                    client_ip,
                    &format!("issuer={} sub={}", identity.issuer, identity.subject),
                );
                return Err(AuthError::Forbidden);
            }
        };

        let mut current = self.state.write().await;
        let mut state = current.clone();
        let mut provisioned_username = None;
        let mut prior_role = None;

        let mut existing_idx = state.users.iter().position(|u| {
            u.oidc_subject.as_deref() == Some(&identity.subject)
                && u.oidc_issuer.as_deref() == Some(&identity.issuer)
        });

        if existing_idx.is_none() {
            if !auto_provision {
                audit(
                    "oidc_login_failed",
                    identity
                        .preferred_username
                        .as_deref()
                        .unwrap_or(&identity.subject),
                    client_ip,
                    "user not provisioned and auto_provision disabled",
                );
                return Err(AuthError::UserNotFound);
            }
            let username = pick_username(&state.users, identity);
            state.users.push(User {
                username: username.clone(),
                password_hash: None,
                role: role.clone(),
                must_change_password: false,
                oidc_subject: Some(identity.subject.clone()),
                oidc_issuer: Some(identity.issuer.clone()),
                webauthn_credentials: Vec::new(),
            });
            existing_idx = Some(state.users.len() - 1);
            provisioned_username = Some(username);
        }

        let idx = existing_idx.expect("user index resolved above");
        let user = &mut state.users[idx];
        if user.role != role {
            prior_role = Some(user.role.clone());
            user.role = role.clone();
        }

        let username = user.username.clone();
        let token = generate_token();
        let session = Session {
            token: token.clone(),
            username: username.clone(),
            role: role.clone(),
            filesystem: None,
            owner: None,
            created_at: now,
            must_change_password: false,
            client_ip: Some(client_ip.to_string()),
        };

        state
            .sessions
            .retain(|s| now - s.created_at <= SESSION_TTL_SECS);
        state.sessions.push(session);
        commit_state(&mut current, state).await?;

        if let Some(provisioned_username) = provisioned_username {
            audit(
                "oidc_user_provisioned",
                &provisioned_username,
                client_ip,
                &format!(
                    "issuer={} sub={} role={:?}",
                    identity.issuer, identity.subject, role
                ),
            );
        }
        if let Some(prior_role) = prior_role {
            audit(
                "oidc_role_updated_on_login",
                &username,
                client_ip,
                &format!("from={prior_role:?} to={role:?}"),
            );
        }
        audit("oidc_login_success", &username, client_ip, "");
        info!("OIDC login succeeded for user '{}'", username);
        Ok(token)
    }

    /// Validate a token and return the session (checks both login sessions and API tokens)
    pub async fn validate(&self, token: &str, client_ip: &str) -> Result<Session, AuthError> {
        let state = self.state.read().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Check login sessions first. Constant-time compare on the token —
        // the practical timing attack against a 256-bit URL-safe-base64 token
        // is infeasible, but defense-in-depth is cheap.
        if let Some(session) = state.sessions.iter().find(|s| ct_eq_str(&s.token, token)) {
            // Check TTL
            if now - session.created_at > SESSION_TTL_SECS {
                drop(state);
                let mut current = self.state.write().await;
                let mut state = current.clone();
                state.sessions.retain(|s| !ct_eq_str(&s.token, token));
                commit_state(&mut current, state).await.ok();
                return Err(AuthError::TokenExpired);
            }
            // Verify client IP matches the one that created this session
            if let Some(ref bound_ip) = session.client_ip
                && bound_ip != client_ip
            {
                tracing::warn!(
                    "Session for '{}' rejected: IP mismatch (bound={}, request={})",
                    session.username,
                    bound_ip,
                    client_ip
                );
                audit(
                    "session_ip_mismatch",
                    &session.username,
                    client_ip,
                    &format!("bound={bound_ip}"),
                );
                return Err(AuthError::InvalidToken);
            }
            return Ok(session.clone());
        }
        // Check long-lived API tokens — SHA-256 comparison (tokens are high-entropy,
        // don't need Argon2's brute-force resistance, and Argon2 is too slow for O(n) scan)
        let incoming_hash = hash_token(token);
        let t = state
            .api_tokens
            .iter()
            .find(|t| ct_eq_str(&t.token, &incoming_hash))
            .ok_or(AuthError::InvalidToken)?;

        if let Some(exp) = t.expires_at
            && now >= exp
        {
            return Err(AuthError::TokenExpired);
        }

        // Check IP allowlist if configured
        if !t.allowed_ips.is_empty() && !t.allowed_ips.iter().any(|ip| ip == client_ip) {
            tracing::warn!(
                "API token '{}' rejected: IP {} not in allowed list {:?}",
                t.name,
                client_ip,
                t.allowed_ips
            );
            audit(
                "token_ip_rejected",
                &t.name,
                client_ip,
                &format!("allowed={:?}", t.allowed_ips),
            );
            return Err(AuthError::InvalidToken);
        }

        Ok(Session {
            token: token.to_string(),
            username: t.name.clone(),
            role: t.role.clone(),
            filesystem: t.filesystem.clone(),
            owner: if t.role == Role::Operator {
                Some(t.name.clone())
            } else {
                None
            },
            created_at: t.created_at,
            must_change_password: false,
            client_ip: None,
        })
    }

    /// Create a long-lived API token (admin only). Returns the token value — shown only once.
    pub async fn create_api_token(
        &self,
        session: &Session,
        name: &str,
        role: Role,
        filesystem: Option<String>,
        expires_in_secs: Option<u64>,
        allowed_ips: Vec<String>,
    ) -> Result<ApiToken, AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();
        if state.api_tokens.iter().any(|t| t.name == name) {
            return Err(AuthError::UserExists); // reuse: token name already taken
        }

        let id = generate_id();
        let raw_token = generate_token();
        let token_hash = hash_token(&raw_token);
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let expires_at = expires_in_secs.map(|s| created_at + s);

        let stored = ApiToken {
            id: id.clone(),
            name: name.to_string(),
            token: token_hash,
            role: role.clone(),
            created_at,
            filesystem: filesystem.clone(),
            expires_at,
            allowed_ips: allowed_ips.clone(),
        };

        state.api_tokens.push(stored);
        commit_state(&mut current, state).await?;

        audit(
            "token_created",
            &session.username,
            session.client_ip.as_deref().unwrap_or(""),
            &format!("name={name}"),
        );
        info!("Created API token '{name}'");

        // Return the raw token to the caller — shown only once, never stored
        Ok(ApiToken {
            id,
            name: name.to_string(),
            token: raw_token,
            role,
            created_at,
            filesystem,
            expires_at,
            allowed_ips,
        })
    }

    /// List API tokens without exposing the token value
    pub async fn list_api_tokens(&self, session: &Session) -> Result<Vec<ApiTokenInfo>, AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }
        let state = self.state.read().await;
        Ok(state
            .api_tokens
            .iter()
            .map(|t| ApiTokenInfo {
                id: t.id.clone(),
                name: t.name.clone(),
                role: t.role.clone(),
                created_at: t.created_at,
                filesystem: t.filesystem.clone(),
                expires_at: t.expires_at,
                allowed_ips: t.allowed_ips.clone(),
            })
            .collect())
    }

    /// Delete an API token by ID (admin only)
    pub async fn delete_api_token(&self, session: &Session, id: &str) -> Result<(), AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();
        let len_before = state.api_tokens.len();
        state.api_tokens.retain(|t| t.id != id);
        if state.api_tokens.len() == len_before {
            return Err(AuthError::UserNotFound);
        }
        commit_state(&mut current, state).await?;

        audit(
            "token_deleted",
            &session.username,
            session.client_ip.as_deref().unwrap_or(""),
            &format!("id={id}"),
        );
        info!("Deleted API token '{id}'");
        Ok(())
    }

    /// Revoke a token (logout)
    pub async fn logout(&self, token: &str) -> Result<(), AuthError> {
        let mut current = self.state.write().await;
        let mut state = current.clone();
        let len_before = state.sessions.len();
        state.sessions.retain(|s| s.token != token);
        if state.sessions.len() == len_before {
            return Err(AuthError::InvalidToken);
        }
        commit_state(&mut current, state).await?;
        Ok(())
    }

    /// Change a user's password (requires current session to be admin or the user themselves)
    pub async fn change_password(
        &self,
        session: &Session,
        username: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        if session.role != Role::Admin && session.username != username {
            return Err(AuthError::Forbidden);
        }

        if new_password.len() < 8 {
            return Err(AuthError::WeakPassword);
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();
        let user = state
            .users
            .iter_mut()
            .find(|u| u.username == username)
            .ok_or(AuthError::UserNotFound)?;

        user.password_hash = Some(hash_password(new_password)?);
        user.must_change_password = false;

        // Also clear the flag on any active sessions for this user
        for s in state.sessions.iter_mut().filter(|s| s.username == username) {
            s.must_change_password = false;
        }

        commit_state(&mut current, state).await?;

        audit(
            "password_changed",
            &session.username,
            session.client_ip.as_deref().unwrap_or(""),
            &format!("target={username}"),
        );
        info!("Password changed for user '{username}'");
        Ok(())
    }

    /// Create a new user (admin only)
    pub async fn create_user(
        &self,
        session: &Session,
        username: &str,
        password: &str,
        role: Role,
    ) -> Result<(), AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }

        if password.len() < 8 {
            return Err(AuthError::WeakPassword);
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();
        if state.users.iter().any(|u| u.username == username) {
            return Err(AuthError::UserExists);
        }

        state.users.push(User {
            username: username.to_string(),
            password_hash: Some(hash_password(password)?),
            role: role.clone(),
            must_change_password: false,
            oidc_subject: None,
            oidc_issuer: None,
            webauthn_credentials: Vec::new(),
        });
        commit_state(&mut current, state).await?;

        audit(
            "user_created",
            &session.username,
            session.client_ip.as_deref().unwrap_or(""),
            &format!("target={username}, role={role:?}"),
        );
        info!("Created user '{username}'");
        Ok(())
    }

    /// Delete a user (admin only, cannot delete self)
    pub async fn delete_user(&self, session: &Session, username: &str) -> Result<(), AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }
        if session.username == username {
            return Err(AuthError::Forbidden);
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();
        let len_before = state.users.len();
        state.users.retain(|u| u.username != username);
        if state.users.len() == len_before {
            return Err(AuthError::UserNotFound);
        }

        // Also revoke all their sessions
        state.sessions.retain(|s| s.username != username);
        commit_state(&mut current, state).await?;

        audit(
            "user_deleted",
            &session.username,
            session.client_ip.as_deref().unwrap_or(""),
            &format!("target={username}"),
        );
        info!("Deleted user '{username}'");
        Ok(())
    }

    /// List users (no passwords)
    pub async fn list_users(&self) -> Vec<UserInfo> {
        let state = self.state.read().await;
        state
            .users
            .iter()
            .map(|u| UserInfo {
                username: u.username.clone(),
                role: u.role.clone(),
                webauthn_credential_count: u.webauthn_credentials.len(),
            })
            .collect()
    }

    async fn record_failed_attempt(&self, username: &str, client_ip: &str, now: u64) {
        let mut rl = self.rate_limit.write().await;
        let user_entry = rl.by_user.entry(username.to_string()).or_default();
        user_entry.push(now);
        user_entry.retain(|&t| now.saturating_sub(t) < LOCKOUT_WINDOW_SECS);
        if client_ip != "unknown" {
            let ip_entry = rl.by_ip.entry(client_ip.to_string()).or_default();
            ip_entry.push(now);
            ip_entry.retain(|&t| now.saturating_sub(t) < IP_LOCKOUT_WINDOW_SECS);
        }
        let snapshot = rl.clone();
        drop(rl);
        if let Err(e) = save_rate_limit(&snapshot).await {
            tracing::warn!("Failed to persist rate-limit state: {e}");
        }
    }

    async fn clear_failed_attempts(&self, username: &str) {
        let mut rl = self.rate_limit.write().await;
        if rl.by_user.remove(username).is_none() {
            return;
        }
        let snapshot = rl.clone();
        drop(rl);
        if let Err(e) = save_rate_limit(&snapshot).await {
            tracing::warn!("Failed to persist rate-limit state: {e}");
        }
    }

    /// True when `username` is an Admin and the only Admin in the system.
    /// Used to suppress per-username lockout for the last admin standing.
    async fn is_only_local_admin(&self, username: &str) -> bool {
        let state = self.state.read().await;
        is_only_admin(&state.users, username)
    }

    /// Check if the token has admin role
    pub async fn require_admin(&self, token: &str, client_ip: &str) -> Result<Session, AuthError> {
        let session = self.validate(token, client_ip).await?;
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }
        Ok(session)
    }

    // ── WebAuthn credential management (#289) ─────────────────────
    //
    // These methods are the persistence half of `auth_webauthn.rs` —
    // they live here (not in `auth_webauthn.rs`) because they
    // mutate `auth.json` under the same lock that protects every
    // other user-record write. The webauthn module owns the
    // crypto + challenge state; this module owns the on-disk shape.

    /// Whether ANY user has at least one registered WebAuthn
    /// credential. Used by the unauthenticated
    /// `/api/auth/webauthn/available` endpoint to gate the login
    /// page's "Sign in with security key" button — on a fresh
    /// install with no keys registered yet the button is just
    /// visual noise that fails at click time.
    ///
    /// One-bit leak (an unauthenticated caller learns whether any
    /// keys exist) is intentional and matches the parallel
    /// `/api/auth/oidc/available` — both endpoints exist to let
    /// the login page render the right buttons without first
    /// requiring auth.
    pub async fn any_webauthn_credentials_registered(&self) -> bool {
        let state = self.state.read().await;
        state
            .users
            .iter()
            .any(|u| !u.webauthn_credentials.is_empty())
    }

    /// Snapshot of a user's registered WebAuthn credentials. Returns
    /// an empty vec when the user doesn't exist (caller is the
    /// session-bound user, so a missing entry is degenerate and
    /// silent is fine).
    pub async fn webauthn_credentials_for(&self, username: &str) -> Vec<WebauthnCredential> {
        let state = self.state.read().await;
        state
            .users
            .iter()
            .find(|u| u.username == username)
            .map(|u| u.webauthn_credentials.clone())
            .unwrap_or_default()
    }

    /// Append a new credential to a user's record and persist. The
    /// credential ID uniqueness is enforced by webauthn-rs's exclude-
    /// credentials list at registration time; if a duplicate somehow
    /// makes it through, the second entry wins (later assertions
    /// would match either by credential ID anyway).
    pub async fn add_webauthn_credential(
        &self,
        username: &str,
        credential: WebauthnCredential,
    ) -> Result<(), AuthError> {
        let mut current = self.state.write().await;
        let mut state = current.clone();
        let user = state
            .users
            .iter_mut()
            .find(|u| u.username == username)
            .ok_or(AuthError::UserNotFound)?;
        user.webauthn_credentials.push(credential);
        commit_state(&mut current, state).await?;
        Ok(())
    }

    /// Remove a credential by its raw credential-ID bytes (the
    /// webauthn-rs `CredentialID`, not the base64url wrapping the
    /// WebUI uses). Returns `NotFound` when no credential matches —
    /// the typical "list view was stale" case the WebUI ignores.
    pub async fn remove_webauthn_credential(
        &self,
        username: &str,
        credential_id: &[u8],
    ) -> Result<(), AuthError> {
        let mut current = self.state.write().await;
        let mut state = current.clone();
        let user = state
            .users
            .iter_mut()
            .find(|u| u.username == username)
            .ok_or(AuthError::UserNotFound)?;
        let before = user.webauthn_credentials.len();
        user.webauthn_credentials
            .retain(|c| c.passkey.cred_id().as_ref() != credential_id);
        if user.webauthn_credentials.len() == before {
            return Err(AuthError::NotFound);
        }
        commit_state(&mut current, state).await?;
        Ok(())
    }

    /// True iff the user has at least one non-WebAuthn factor
    /// (a local password OR an OIDC link). Used as the registration
    /// precheck: a user with only WebAuthn credentials who loses
    /// every authenticator is locked out, so we require a
    /// recoverable fallback before letting them register the first
    /// security key. Today's user-creation paths always set one of
    /// the two, but this check guards against future API additions
    /// that might break that invariant (e.g. an "unlink OIDC" RPC
    /// without a matching "but the user has WebAuthn-only" check).
    pub async fn has_non_webauthn_factor(&self, username: &str) -> bool {
        let state = self.state.read().await;
        state
            .users
            .iter()
            .find(|u| u.username == username)
            .map(|u| {
                u.password_hash.is_some() || (u.oidc_subject.is_some() && u.oidc_issuer.is_some())
            })
            .unwrap_or(false)
    }

    /// Admin recovery for the "lost every WebAuthn authenticator"
    /// case — wipes every credential under the named user. Audit
    /// log records the actor + target so a malicious admin can't
    /// quietly clear someone else's keys without a trail. Returns
    /// the number of credentials removed (0 when the user existed
    /// but had no credentials; this is not an error — admin UI
    /// surfaces it as a no-op).
    pub async fn reset_webauthn_credentials(
        &self,
        actor: &Session,
        target_username: &str,
    ) -> Result<usize, AuthError> {
        if actor.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }
        let mut current = self.state.write().await;
        let mut state = current.clone();
        let user = state
            .users
            .iter_mut()
            .find(|u| u.username == target_username)
            .ok_or(AuthError::UserNotFound)?;
        let removed = user.webauthn_credentials.len();
        user.webauthn_credentials.clear();
        commit_state(&mut current, state).await?;
        drop(current);
        audit(
            "webauthn_reset_for_user",
            &actor.username,
            actor.client_ip.as_deref().unwrap_or(""),
            &format!("target={target_username} removed={removed}"),
        );
        info!(
            "admin '{}' reset {} WebAuthn credential(s) for user '{}'",
            actor.username, removed, target_username
        );
        Ok(removed)
    }

    /// Persist the new `sign_count` (and any other counter / backup
    /// state) into the matching credential after a successful
    /// assertion. Without this, every subsequent assertion would
    /// be rejected by webauthn-rs's `finish_passkey_authentication`
    /// — that function checks the incoming sign_count is strictly
    /// greater than the stored value as replay protection.
    pub async fn update_webauthn_sign_count(
        &self,
        username: &str,
        auth_result: &webauthn_rs::prelude::AuthenticationResult,
    ) -> Result<(), AuthError> {
        let mut current = self.state.write().await;
        let mut state = current.clone();
        let user = state
            .users
            .iter_mut()
            .find(|u| u.username == username)
            .ok_or(AuthError::UserNotFound)?;
        let cred = user
            .webauthn_credentials
            .iter_mut()
            .find(|c| c.passkey.cred_id() == auth_result.cred_id())
            .ok_or(AuthError::NotFound)?;
        // `update_credential` consumes the auth result's counter +
        // backup-state and returns whether anything actually changed.
        // We persist regardless of the return value — saving an
        // unchanged record is cheap and avoids a branch for
        // "credential is identical, skip the disk write" that's
        // hard to verify.
        cred.passkey.update_credential(auth_result);
        commit_state(&mut current, state).await?;
        Ok(())
    }

    /// Mint a fresh session for a user whose identity has already
    /// been verified out-of-band (today: WebAuthn assertion; PR #2
    /// of issue #289). Mirrors the lockout + session-pruning logic
    /// from `login` so a WebAuthn-only attacker spamming bad
    /// assertions hits the same per-IP / per-username limits the
    /// password path uses. The username **must** be one this engine
    /// has already authenticated (the caller's job, not this
    /// function's) — there's no credential check here.
    pub async fn mint_session_for_webauthn(
        &self,
        username: &str,
        client_ip: &str,
    ) -> Result<String, AuthError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Same per-IP lockout as `login` — a WebAuthn-only attacker
        // can't get around it by switching from password to webauthn
        // attempts.
        if client_ip != "unknown" {
            let rl = self.rate_limit.read().await;
            let ip_fails = rl.ip_failures(client_ip, now);
            if ip_fails >= MAX_IP_FAILED_ATTEMPTS {
                tracing::warn!(
                    "WebAuthn login blocked from {client_ip}: {ip_fails} failed attempts in the last hour"
                );
                audit(
                    "login_webauthn_ip_locked",
                    username,
                    client_ip,
                    &format!("{ip_fails} failed attempts"),
                );
                return Err(AuthError::AccountLocked);
            }
        }

        // Per-username lockout — same suppression rule for the
        // last-admin case as `login`, since locking the last admin
        // out of every login path would brick the appliance.
        let only_admin = self.is_only_local_admin(username).await;
        {
            let rl = self.rate_limit.read().await;
            let user_fails = rl.user_failures(username, now);
            if user_fails >= MAX_FAILED_ATTEMPTS && !only_admin {
                tracing::warn!(
                    "WebAuthn login blocked for '{}': {} failed attempts in last {} minutes",
                    username,
                    user_fails,
                    LOCKOUT_WINDOW_SECS / 60
                );
                audit(
                    "login_webauthn_locked",
                    username,
                    client_ip,
                    &format!("{user_fails} failed attempts"),
                );
                return Err(AuthError::AccountLocked);
            }
        }

        let mut current = self.state.write().await;
        let mut state = current.clone();
        let user = state
            .users
            .iter()
            .find(|u| u.username == username)
            .ok_or(AuthError::UserNotFound)?;

        let token = generate_token();
        let session = Session {
            token: token.clone(),
            username: user.username.clone(),
            role: user.role.clone(),
            filesystem: None,
            owner: None,
            created_at: now,
            // WebAuthn-verified sessions don't trigger the
            // password-change wall — the operator already proved
            // possession of a registered credential. If their local
            // password is still flagged for change they can do it
            // from /users when they want to.
            must_change_password: false,
            client_ip: Some(client_ip.to_string()),
        };

        state
            .sessions
            .retain(|s| now - s.created_at <= SESSION_TTL_SECS);
        state.sessions.push(session);
        commit_state(&mut current, state).await?;

        // Webauthn success clears the per-username failure bucket
        // (mirrors the password path); per-IP failures stay so a
        // single redeemed username doesn't whitewash an attacker's
        // spray across other accounts.
        self.clear_failed_attempts(username).await;
        audit("login_webauthn_success", username, client_ip, "");
        info!("User '{}' logged in via WebAuthn", username);
        Ok(token)
    }

    /// Counterpart to `mint_session_for_webauthn` for the failure
    /// case — bumps the per-user + per-IP lockout counters so a
    /// stream of bad WebAuthn assertions feeds the same rate limit
    /// as a stream of bad passwords. Exposed publicly so the REST
    /// handler can call it after `login_finish` fails.
    pub async fn record_webauthn_failure(&self, username: &str, client_ip: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.record_failed_attempt(username, client_ip, now).await;
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UserInfo {
    pub username: String,
    pub role: Role,
    /// How many WebAuthn credentials are registered to this user.
    /// Drives the admin "Reset security keys" affordance on the
    /// /users page — admins only see the button on rows where
    /// there are credentials to reset, instead of a no-op button
    /// on every row.
    #[serde(default)]
    pub webauthn_credential_count: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("account temporarily locked due to too many failed attempts")]
    AccountLocked,
    #[error("invalid token")]
    InvalidToken,
    #[error("token has expired")]
    TokenExpired,
    #[error("forbidden")]
    Forbidden,
    #[error("user not found")]
    UserNotFound,
    #[error("user already exists")]
    UserExists,
    #[error("not found")]
    NotFound,
    #[error("password must be at least 8 characters")]
    WeakPassword,
    #[error("password hash error: {0}")]
    HashError(String),
    #[error("auth state at {path} is corrupt: {source}")]
    StateCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("auth state at {0} exists but is not initialized")]
    StateUninitialized(PathBuf),
    #[error("unsafe auth state at {path}: {reason}")]
    StateUnsafe { path: PathBuf, reason: String },
    #[error("failed to {operation} auth state at {path}: {source}")]
    StateIo {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize auth state: {0}")]
    StateSerialize(serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Append a structured event to the audit log (JSONL, append-only).
///
/// The on-disk file is rotated externally by logrotate (configured in
/// nixos/modules/nasty.nix with `copytruncate` so the engine's open handle
/// keeps appending). We do not rotate from inside the engine; doing so used
/// to rename the file out from under logrotate, leaving rotated history
/// orphaned and overwritten on the next cycle.
///
/// Every event is also emitted to `tracing` (target = `audit`), which the
/// engine ships to journald. An attacker who tampers with the on-disk file
/// still leaves a journald trail that lives on a different storage path
/// and rotates separately.
pub fn audit(event: &str, user: &str, ip: &str, detail: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = serde_json::json!({
        "ts": now,
        "event": event,
        "user": user,
        "ip": ip,
        "detail": detail,
    });

    tracing::info!(target: "audit", event, user, ip, detail);

    // mode(0o600) only takes effect on file creation, but that's the window
    // that matters — once created with the right mode, subsequent appends
    // preserve it. logrotate's `create` directive can be added later if the
    // post-rotation truncated file ends up with a wider mode.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(AUDIT_LOG_PATH)
    {
        let _ = writeln!(f, "{}", line);
    }
}

/// Read audit log entries, most recent first.
pub async fn read_audit_log(limit: usize) -> Vec<serde_json::Value> {
    let content = tokio::fs::read_to_string(AUDIT_LOG_PATH)
        .await
        .unwrap_or_default();
    let mut entries: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}

pub(crate) fn hash_password(password: &str) -> Result<String, AuthError> {
    // Generate 16 random bytes for salt, encode as base64ct for SaltString
    let mut salt_bytes = [0u8; 16];
    rand::fill(&mut salt_bytes);
    let salt =
        SaltString::encode_b64(&salt_bytes).map_err(|e| AuthError::HashError(e.to_string()))?;
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::HashError(e.to_string()))?;
    Ok(hash.to_string())
}

pub(crate) fn verify_password(password: &str, hash: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(hash).map_err(|e| AuthError::HashError(e.to_string()))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::InvalidCredentials)
}

/// Constant-time string equality. Avoids the timing side-channel of `==`
/// even though our tokens are 256-bit random — keeps the auth path uniform
/// when tokens of different shapes (session vs hashed API tokens) are
/// compared.
fn ct_eq_str(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// SHA-256 hash for API tokens. Tokens are 32 random bytes — high entropy,
/// no need for Argon2's brute-force resistance. Instant O(1) comparison.
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for byte in hasher.finalize() {
        write!(&mut out, "{byte:02x}").unwrap();
    }
    out
}

/// Pick a unique username for a freshly-provisioned OIDC user. Prefers
/// `preferred_username`, falls back to email local-part, then to subject.
/// Appends a numeric suffix if the chosen name already exists.
fn pick_username(existing: &[User], identity: &crate::auth_oidc::OidcIdentity) -> String {
    let base = identity
        .preferred_username
        .clone()
        .or_else(|| {
            identity
                .email
                .as_deref()
                .and_then(|e| e.split('@').next().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| identity.subject.clone());
    let base = base.trim().to_string();
    let base = if base.is_empty() {
        identity.subject.clone()
    } else {
        base
    };
    if !existing.iter().any(|u| u.username == base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|u| u.username == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Parse a role string from configuration. Accepts `admin`, `operator`, `readonly`.
pub fn parse_role_str(s: &str) -> Option<Role> {
    match s.trim().to_ascii_lowercase().as_str() {
        "admin" => Some(Role::Admin),
        "operator" => Some(Role::Operator),
        "readonly" | "read_only" | "read-only" => Some(Role::ReadOnly),
        _ => None,
    }
}

pub(crate) fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

fn generate_id() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

async fn initialize_state(
    path: &Path,
    expected_owner: Option<u32>,
) -> Result<AuthState, AuthError> {
    if let Some(state) = load_state_from(path, expected_owner).await? {
        if !state.initialized {
            return Err(AuthError::StateUninitialized(path.to_path_buf()));
        }
        return Ok(state);
    }

    let state = AuthState {
        users: vec![User {
            username: "admin".to_string(),
            password_hash: Some(hash_password("admin")?),
            role: Role::Admin,
            must_change_password: true,
            oidc_subject: None,
            oidc_issuer: None,
            webauthn_credentials: Vec::new(),
        }],
        initialized: true,
        ..AuthState::default()
    };
    create_state_to(path, &state).await?;
    info!("Created default admin user (password: admin) — change this immediately!");
    Ok(state)
}

async fn load_state_from(
    path: &Path,
    expected_owner: Option<u32>,
) -> Result<Option<AuthState>, AuthError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tokio::io::AsyncReadExt;

    let mut options = tokio::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = match options.open(path).await {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) if source.raw_os_error() == Some(libc::ELOOP) => {
            return Err(AuthError::StateUnsafe {
                path: path.to_path_buf(),
                reason: "refusing to follow an auth state symlink".to_string(),
            });
        }
        Err(source) => {
            return Err(AuthError::StateIo {
                operation: "open without following symlinks",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let metadata = file.metadata().await.map_err(|source| AuthError::StateIo {
        operation: "inspect",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(AuthError::StateUnsafe {
            path: path.to_path_buf(),
            reason: "expected a regular file, not a symlink or special file".to_string(),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(AuthError::StateUnsafe {
            path: path.to_path_buf(),
            reason: format!("permissions {mode:o} expose authentication secrets"),
        });
    }
    if let Some(expected_owner) = expected_owner
        && metadata.uid() != expected_owner
    {
        return Err(AuthError::StateUnsafe {
            path: path.to_path_buf(),
            reason: format!(
                "owner UID {} does not match expected UID {expected_owner}",
                metadata.uid()
            ),
        });
    }

    let mut content = String::new();
    file.read_to_string(&mut content)
        .await
        .map_err(|source| AuthError::StateIo {
            operation: "read",
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|source| AuthError::StateCorrupt {
            path: path.to_path_buf(),
            source,
        })
}

async fn commit_state(current: &mut AuthState, next: AuthState) -> Result<(), AuthError> {
    commit_state_to(Path::new(STATE_PATH), current, next).await
}

async fn commit_state_to(
    path: &Path,
    current: &mut AuthState,
    next: AuthState,
) -> Result<(), AuthError> {
    let result = persist_state_to(path, &next, true).await;
    finish_state_commit(current, next, result)
}

#[cfg(test)]
async fn save_state_to(path: &Path, state: &AuthState) -> Result<(), AuthError> {
    persist_state_to(path, state, true)
        .await
        .map_err(|failure| failure.source)
}

async fn create_state_to(path: &Path, state: &AuthState) -> Result<(), AuthError> {
    persist_state_to(path, state, false)
        .await
        .map_err(|failure| failure.source)
}

fn finish_state_commit(
    current: &mut AuthState,
    next: AuthState,
    result: Result<(), StatePersistFailure>,
) -> Result<(), AuthError> {
    match result {
        Ok(()) => {
            *current = next;
            Ok(())
        }
        Err(failure) => {
            if failure.committed {
                *current = next;
                audit(
                    "auth_state_committed_with_durability_error",
                    "system",
                    "local",
                    &failure.source.to_string(),
                );
                tracing::error!(
                    "Auth state changed but its directory sync failed: {}",
                    failure.source
                );
            }
            Err(failure.source)
        }
    }
}

#[derive(Debug)]
struct StatePersistFailure {
    source: AuthError,
    committed: bool,
}

async fn persist_state_to(
    path: &Path,
    state: &AuthState,
    replace: bool,
) -> Result<(), StatePersistFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::StateIo {
            operation: "resolve parent for",
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "state path has no parent",
            ),
        })
        .map_err(|source| StatePersistFailure {
            source,
            committed: false,
        })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    let temp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    save_state_with_temp(path, &temp, state, replace).await
}

async fn save_state_with_temp(
    path: &Path,
    temp: &Path,
    state: &AuthState,
    replace: bool,
) -> Result<(), StatePersistFailure> {
    use tokio::io::AsyncWriteExt;

    let parent = path
        .parent()
        .ok_or_else(|| AuthError::StateIo {
            operation: "resolve parent for",
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "state path has no parent",
            ),
        })
        .map_err(|source| StatePersistFailure {
            source,
            committed: false,
        })?;
    let json = serde_json::to_vec_pretty(state)
        .map_err(AuthError::StateSerialize)
        .map_err(|source| StatePersistFailure {
            source,
            committed: false,
        })?;

    let mut committed = false;
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options
            .open(temp)
            .await
            .map_err(|source| AuthError::StateIo {
                operation: "create temporary file for",
                path: path.to_path_buf(),
                source,
            })?;
        file.write_all(&json)
            .await
            .map_err(|source| AuthError::StateIo {
                operation: "write temporary file for",
                path: path.to_path_buf(),
                source,
            })?;
        file.sync_all().await.map_err(|source| AuthError::StateIo {
            operation: "sync temporary file for",
            path: path.to_path_buf(),
            source,
        })?;
        drop(file);

        if replace {
            std::fs::rename(temp, path).map_err(|source| AuthError::StateIo {
                operation: "replace",
                path: path.to_path_buf(),
                source,
            })?;
            committed = true;
        } else {
            std::fs::hard_link(temp, path).map_err(|source| AuthError::StateIo {
                operation: "create",
                path: path.to_path_buf(),
                source,
            })?;
            committed = true;
            std::fs::remove_file(temp).map_err(|source| AuthError::StateIo {
                operation: "remove temporary file for",
                path: path.to_path_buf(),
                source,
            })?;
        }
        let directory = std::fs::File::open(parent).map_err(|source| AuthError::StateIo {
            operation: "open parent directory for",
            path: path.to_path_buf(),
            source,
        })?;
        directory.sync_all().map_err(|source| AuthError::StateIo {
            operation: "sync parent directory for",
            path: path.to_path_buf(),
            source,
        })
    }
    .await;

    if result.is_err() && !committed {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(|source| StatePersistFailure { source, committed })
}

async fn load_rate_limit() -> RateLimitState {
    nasty_common::load_singleton_or_recover(RATE_LIMIT_PATH).await
}

async fn save_rate_limit(state: &RateLimitState) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string(state).unwrap();
    tokio::fs::write(RATE_LIMIT_PATH, json).await?;
    // 0600: holds source IPs of attackers, who don't always count as PII but
    // are non-trivial to leak. Match auth.json's permissions.
    tokio::fs::set_permissions(RATE_LIMIT_PATH, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

/// Pure helper: returns true when `username` is the only `Admin` in `users`.
fn is_only_admin(users: &[User], username: &str) -> bool {
    let admins: Vec<&User> = users.iter().filter(|u| u.role == Role::Admin).collect();
    admins.len() == 1 && admins[0].username == username
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_owner(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(path).expect("path metadata").uid()
    }

    async fn write_secure(path: &Path, content: &[u8]) {
        use std::os::unix::fs::PermissionsExt;

        tokio::fs::write(path, content).await.expect("write state");
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .expect("secure state permissions");
    }

    fn initialized_state() -> AuthState {
        AuthState {
            users: vec![user("alice", Role::Admin)],
            initialized: true,
            ..AuthState::default()
        }
    }

    fn user(name: &str, role: Role) -> User {
        User {
            username: name.to_string(),
            password_hash: Some("hash".to_string()),
            role,
            must_change_password: false,
            oidc_subject: None,
            oidc_issuer: None,
            webauthn_credentials: Vec::new(),
        }
    }

    fn session(role: Role) -> Session {
        Session {
            token: "token".to_string(),
            username: "test".to_string(),
            role,
            filesystem: None,
            owner: None,
            created_at: 0,
            must_change_password: false,
            client_ip: None,
        }
    }

    #[tokio::test]
    async fn missing_auth_state_bootstraps_and_persists() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        let owner = test_owner(dir.path());
        let state = initialize_state(&path, Some(owner))
            .await
            .expect("initialize state");

        assert!(state.initialized);
        assert_eq!(state.users.len(), 1);
        assert_eq!(state.users[0].username, "admin");
        assert!(state.users[0].must_change_password);

        let persisted = load_state_from(&path, Some(owner))
            .await
            .expect("load persisted state")
            .expect("state should exist");
        assert!(persisted.initialized);
        assert_eq!(persisted.users[0].username, "admin");
        let mode = tokio::fs::metadata(&path)
            .await
            .expect("state metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn valid_existing_auth_state_loads_without_bootstrap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        let state = initialized_state();
        write_secure(
            &path,
            &serde_json::to_vec_pretty(&state).expect("serialize state"),
        )
        .await;

        let loaded = initialize_state(&path, Some(test_owner(dir.path())))
            .await
            .expect("initialize state");
        assert_eq!(loaded.users.len(), 1);
        assert_eq!(loaded.users[0].username, "alice");
    }

    #[tokio::test]
    async fn corrupt_existing_auth_state_fails_without_replacement() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        let corrupt = b"{not valid json";
        write_secure(&path, corrupt).await;

        let err = initialize_state(&path, Some(test_owner(dir.path())))
            .await
            .expect_err("corrupt state must fail");
        assert!(matches!(err, AuthError::StateCorrupt { .. }));
        assert_eq!(
            tokio::fs::read(&path).await.expect("read corrupt state"),
            corrupt
        );
    }

    #[tokio::test]
    async fn unreadable_existing_auth_state_fails_without_replacement() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        tokio::fs::create_dir(&path)
            .await
            .expect("create unreadable state path");

        let err = initialize_state(&path, Some(test_owner(dir.path())))
            .await
            .expect_err("unreadable state must fail");
        assert!(matches!(err, AuthError::StateUnsafe { .. }));
        assert!(
            tokio::fs::metadata(&path)
                .await
                .expect("state metadata")
                .is_dir()
        );
    }

    #[tokio::test]
    async fn fifo_auth_state_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "create FIFO: {}", std::io::Error::last_os_error());

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            initialize_state(&path, Some(test_owner(dir.path()))),
        )
        .await
        .expect("FIFO validation must not block")
        .expect_err("FIFO state must fail");
        assert!(matches!(err, AuthError::StateUnsafe { .. }));
    }

    #[tokio::test]
    async fn existing_uninitialized_auth_state_does_not_bootstrap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        let state = AuthState::default();
        save_state_to(&path, &state).await.expect("write state");
        let before = tokio::fs::read(&path).await.expect("read state");

        let err = initialize_state(&path, Some(test_owner(dir.path())))
            .await
            .expect_err("existing uninitialized state must fail");
        assert!(matches!(err, AuthError::StateUninitialized(_)));
        assert_eq!(tokio::fs::read(&path).await.expect("read state"), before);
    }

    #[tokio::test]
    async fn failed_atomic_write_preserves_existing_auth_state() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        save_state_to(&path, &initialized_state())
            .await
            .expect("write initial state");
        let before = tokio::fs::read(&path).await.expect("read initial state");

        let temp_collision = dir.path().join("occupied.tmp");
        tokio::fs::create_dir(&temp_collision)
            .await
            .expect("create temp collision");
        let mut replacement = initialized_state();
        replacement.users.push(user("bob", Role::Operator));
        save_state_with_temp(&path, &temp_collision, &replacement, true)
            .await
            .expect_err("staging failure must be returned");

        assert_eq!(
            tokio::fs::read(&path).await.expect("read preserved state"),
            before
        );
    }

    #[tokio::test]
    async fn failed_persistence_does_not_commit_in_memory_auth_state() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let blocked_parent = dir.path().join("not-a-directory");
        tokio::fs::write(&blocked_parent, b"occupied")
            .await
            .expect("create blocked parent");
        let path = blocked_parent.join("auth.json");
        let mut current = initialized_state();
        let mut next = current.clone();
        next.users.push(user("bob", Role::Operator));

        commit_state_to(&path, &mut current, next)
            .await
            .expect_err("persistence failure must be returned");
        assert_eq!(current.users.len(), 1);
        assert_eq!(current.users[0].username, "alice");
    }

    #[test]
    fn post_commit_sync_failure_keeps_memory_aligned_with_disk_state() {
        let mut current = initialized_state();
        let mut next = current.clone();
        next.users.push(user("bob", Role::Operator));
        let failure = StatePersistFailure {
            source: AuthError::StateIo {
                operation: "sync parent directory for",
                path: PathBuf::from("auth.json"),
                source: std::io::Error::other("injected sync failure"),
            },
            committed: true,
        };

        finish_state_commit(&mut current, next, Err(failure))
            .expect_err("durability failure must be returned");
        assert_eq!(current.users.len(), 2);
        assert_eq!(current.users[1].username, "bob");
    }

    #[tokio::test]
    async fn dangling_auth_state_symlink_does_not_trigger_bootstrap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        std::os::unix::fs::symlink(dir.path().join("missing"), &path)
            .expect("create dangling symlink");

        let err = initialize_state(&path, Some(test_owner(dir.path())))
            .await
            .expect_err("symlinked state must fail");
        assert!(matches!(err, AuthError::StateUnsafe { .. }));
        assert!(
            tokio::fs::symlink_metadata(&path)
                .await
                .expect("symlink metadata")
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn permissive_auth_state_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        tokio::fs::write(
            &path,
            serde_json::to_vec_pretty(&initialized_state()).expect("serialize state"),
        )
        .await
        .expect("write state");
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .await
            .expect("set permissive permissions");

        let err = initialize_state(&path, Some(test_owner(dir.path())))
            .await
            .expect_err("permissive state must fail");
        assert!(matches!(err, AuthError::StateUnsafe { .. }));
    }

    #[tokio::test]
    async fn unexpected_auth_state_owner_is_rejected() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        save_state_to(&path, &initialized_state())
            .await
            .expect("write state");

        let err = initialize_state(&path, Some(test_owner(dir.path()).wrapping_add(1)))
            .await
            .expect_err("unexpected owner must fail");
        assert!(matches!(err, AuthError::StateUnsafe { .. }));
    }

    #[tokio::test]
    async fn bootstrap_does_not_replace_concurrently_created_state() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("auth.json");
        save_state_to(&path, &initialized_state())
            .await
            .expect("write existing state");
        let before = tokio::fs::read(&path).await.expect("read existing state");
        let temp = dir.path().join("bootstrap.tmp");

        save_state_with_temp(&path, &temp, &AuthState::default(), false)
            .await
            .expect_err("bootstrap must not replace existing state");
        assert_eq!(
            tokio::fs::read(&path).await.expect("read preserved state"),
            before
        );
    }

    #[test]
    fn endpoint_access_role_matrix() {
        let admin = session(Role::Admin);
        let operator = session(Role::Operator);
        let read_only = session(Role::ReadOnly);

        for access in [EndpointAccess::Read, EndpointAccess::SelfService] {
            assert_eq!(authorize_session(&admin, access), Ok(()));
            assert_eq!(authorize_session(&operator, access), Ok(()));
            assert_eq!(authorize_session(&read_only, access), Ok(()));
        }

        assert_eq!(authorize_session(&admin, EndpointAccess::Mutation), Ok(()));
        assert_eq!(
            authorize_session(&operator, EndpointAccess::Mutation),
            Ok(())
        );
        assert_eq!(
            authorize_session(&read_only, EndpointAccess::Mutation),
            Err(AccessDenied::InsufficientRole)
        );

        assert_eq!(
            authorize_session(&admin, EndpointAccess::RootEquivalent),
            Ok(())
        );
        assert_eq!(
            authorize_session(&operator, EndpointAccess::RootEquivalent),
            Err(AccessDenied::InsufficientRole)
        );
    }

    #[test]
    fn forced_password_change_only_allows_self_service() {
        let mut admin = session(Role::Admin);
        admin.must_change_password = true;

        assert_eq!(
            authorize_session(&admin, EndpointAccess::SelfService),
            Ok(())
        );
        for access in [
            EndpointAccess::Read,
            EndpointAccess::Mutation,
            EndpointAccess::UnscopedMutation,
            EndpointAccess::RootEquivalent,
        ] {
            assert_eq!(
                authorize_session(&admin, access),
                Err(AccessDenied::PasswordChangeRequired)
            );
        }
    }

    #[test]
    fn root_and_interactive_access_reject_scoped_credentials() {
        for scoped in [
            Session {
                filesystem: Some("pool".to_string()),
                ..session(Role::Admin)
            },
            Session {
                owner: Some("automation".to_string()),
                ..session(Role::Operator)
            },
        ] {
            assert_eq!(authorize_session(&scoped, EndpointAccess::Read), Ok(()));
            assert_eq!(
                authorize_session(&scoped, EndpointAccess::UnscopedMutation),
                Err(AccessDenied::ScopedCredential)
            );
            if scoped.role == Role::Admin {
                assert_eq!(
                    authorize_session(&scoped, EndpointAccess::RootEquivalent),
                    Err(AccessDenied::ScopedCredential)
                );
            }
        }
    }

    #[test]
    fn only_admin_when_alone() {
        let users = vec![user("alice", Role::Admin), user("bob", Role::ReadOnly)];
        assert!(is_only_admin(&users, "alice"));
        assert!(!is_only_admin(&users, "bob"));
    }

    #[test]
    fn not_only_admin_when_two() {
        let users = vec![user("alice", Role::Admin), user("bob", Role::Admin)];
        assert!(!is_only_admin(&users, "alice"));
        assert!(!is_only_admin(&users, "bob"));
    }

    #[test]
    fn not_only_admin_when_user_isnt_admin() {
        let users = vec![user("alice", Role::Admin), user("bob", Role::Operator)];
        assert!(!is_only_admin(&users, "bob"));
    }

    #[test]
    fn rate_limit_user_failures_window() {
        let now = 1_000_000;
        let mut rl = RateLimitState::default();
        rl.by_user.insert(
            "alice".to_string(),
            vec![now - LOCKOUT_WINDOW_SECS - 1, now - 30, now],
        );
        // The very-old entry is outside the window and shouldn't count.
        assert_eq!(rl.user_failures("alice", now), 2);
        assert_eq!(rl.user_failures("bob", now), 0);
    }

    #[test]
    fn rate_limit_ip_failures_window() {
        let now = 1_000_000;
        let mut rl = RateLimitState::default();
        rl.by_ip.insert(
            "10.0.0.1".to_string(),
            vec![now - IP_LOCKOUT_WINDOW_SECS - 1, now - 60, now - 30, now],
        );
        assert_eq!(rl.ip_failures("10.0.0.1", now), 3);
    }

    #[test]
    fn rate_limit_prune_drops_expired_and_empty() {
        let now = 1_000_000;
        let mut rl = RateLimitState::default();
        rl.by_user.insert(
            "alice".to_string(),
            vec![now - LOCKOUT_WINDOW_SECS - 100, now - 5],
        );
        rl.by_user
            .insert("bob".to_string(), vec![now - LOCKOUT_WINDOW_SECS - 100]);
        rl.by_ip.insert(
            "1.2.3.4".to_string(),
            vec![now - IP_LOCKOUT_WINDOW_SECS - 1],
        );
        rl.prune(now);
        // alice keeps the recent entry, bob drops out, ip drops out.
        assert_eq!(rl.by_user.get("alice").map(|v| v.len()), Some(1));
        assert!(!rl.by_user.contains_key("bob"));
        assert!(rl.by_ip.is_empty());
    }
}
