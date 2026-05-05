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
    #[serde(default, deserialize_with = "deserialize_password_hash")]
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
}

/// Accept legacy auth.json files where `password_hash` is a plain string (not Option).
fn deserialize_password_hash<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::IntoDeserializer;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) if s.is_empty() => Ok(None),
        serde_json::Value::String(s) => Ok(Some(s)),
        other => Option::<String>::deserialize(other.into_deserializer())
            .map_err(|e: serde_json::Error| serde::de::Error::custom(e.to_string())),
    }
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
        self.by_user.get(username)
            .map(|v| v.iter().filter(|&&t| now.saturating_sub(t) < LOCKOUT_WINDOW_SECS).count())
            .unwrap_or(0)
    }

    fn ip_failures(&self, ip: &str, now: u64) -> usize {
        self.by_ip.get(ip)
            .map(|v| v.iter().filter(|&&t| now.saturating_sub(t) < IP_LOCKOUT_WINDOW_SECS).count())
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
    pub async fn new() -> Self {
        let state = load_state().await;
        let mut rl = load_rate_limit().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        rl.prune(now);
        let svc = Self {
            state: Arc::new(RwLock::new(state)),
            rate_limit: Arc::new(RwLock::new(rl)),
        };

        // If no users exist, create default admin
        if !svc.state.read().await.initialized {
            let mut st = svc.state.write().await;
            let hash = hash_password("admin").expect("failed to hash default password");
            st.users.push(User {
                username: "admin".to_string(),
                password_hash: Some(hash),
                role: Role::Admin,
                must_change_password: true,
                oidc_subject: None,
                oidc_issuer: None,
            });
            st.initialized = true;
            save_state(&st).await.ok();
            info!("Created default admin user (password: admin) — change this immediately!");
        }

        svc
    }

    /// Authenticate with username/password, returns a session token
    pub async fn login(&self, username: &str, password: &str, client_ip: &str) -> Result<String, AuthError> {
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
                audit("login_ip_locked", username, client_ip, &format!("{ip_fails} failed attempts"));
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
                        username, user_fails, LOCKOUT_WINDOW_SECS / 60
                    );
                    audit("login_locked", username, client_ip, &format!("{user_fails} failed attempts"));
                    return Err(AuthError::AccountLocked);
                }
            }
        }

        let mut state = self.state.write().await;

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
                audit("login_failed", username, client_ip, "no local password (OIDC-only user)");
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
        state.sessions.retain(|s| now - s.created_at <= SESSION_TTL_SECS);

        state.sessions.push(session);
        save_state(&state).await?;

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
                    identity.preferred_username.as_deref().unwrap_or(&identity.subject),
                    client_ip,
                    &format!("issuer={} sub={}", identity.issuer, identity.subject),
                );
                return Err(AuthError::Forbidden);
            }
        };

        let mut state = self.state.write().await;

        let mut existing_idx = state.users.iter().position(|u| {
            u.oidc_subject.as_deref() == Some(&identity.subject)
                && u.oidc_issuer.as_deref() == Some(&identity.issuer)
        });

        if existing_idx.is_none() {
            if !auto_provision {
                audit(
                    "oidc_login_failed",
                    identity.preferred_username.as_deref().unwrap_or(&identity.subject),
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
            });
            existing_idx = Some(state.users.len() - 1);
            audit(
                "oidc_user_provisioned",
                &username,
                client_ip,
                &format!("issuer={} sub={} role={:?}", identity.issuer, identity.subject, role),
            );
        }

        let idx = existing_idx.expect("user index resolved above");
        let user = &mut state.users[idx];
        if user.role != role {
            audit(
                "oidc_role_updated_on_login",
                &user.username,
                client_ip,
                &format!("from={:?} to={:?}", user.role, role),
            );
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

        state.sessions.retain(|s| now - s.created_at <= SESSION_TTL_SECS);
        state.sessions.push(session);
        save_state(&state).await?;

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
        // Check login sessions first
        if let Some(session) = state.sessions.iter().find(|s| s.token == token) {
            // Check TTL
            if now - session.created_at > SESSION_TTL_SECS {
                drop(state);
                let mut state = self.state.write().await;
                state.sessions.retain(|s| s.token != token);
                save_state(&state).await.ok();
                return Err(AuthError::TokenExpired);
            }
            // Verify client IP matches the one that created this session
            if let Some(ref bound_ip) = session.client_ip {
                if bound_ip != client_ip {
                    tracing::warn!(
                        "Session for '{}' rejected: IP mismatch (bound={}, request={})",
                        session.username, bound_ip, client_ip
                    );
                    audit("session_ip_mismatch", &session.username, client_ip, &format!("bound={bound_ip}"));
                    return Err(AuthError::InvalidToken);
                }
            }
            return Ok(session.clone());
        }
        // Check long-lived API tokens — SHA-256 comparison (tokens are high-entropy,
        // don't need Argon2's brute-force resistance, and Argon2 is too slow for O(n) scan)
        let incoming_hash = hash_token(token);
        let t = state.api_tokens.iter()
            .find(|t| t.token == incoming_hash)
            .ok_or(AuthError::InvalidToken)?;

        if let Some(exp) = t.expires_at {
            if now >= exp {
                return Err(AuthError::TokenExpired);
            }
        }

        // Check IP allowlist if configured
        if !t.allowed_ips.is_empty() && !t.allowed_ips.iter().any(|ip| ip == client_ip) {
            tracing::warn!(
                "API token '{}' rejected: IP {} not in allowed list {:?}",
                t.name, client_ip, t.allowed_ips
            );
            audit("token_ip_rejected", &t.name, client_ip, &format!("allowed={:?}", t.allowed_ips));
            return Err(AuthError::InvalidToken);
        }

        Ok(Session {
            token: token.to_string(),
            username: t.name.clone(),
            role: t.role.clone(),
            filesystem: t.filesystem.clone(),
            owner: if t.role == Role::Operator { Some(t.name.clone()) } else { None },
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

        let mut state = self.state.write().await;
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
        save_state(&state).await?;

        audit("token_created", &session.username, session.client_ip.as_deref().unwrap_or(""), &format!("name={name}"));
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
    pub async fn delete_api_token(
        &self,
        session: &Session,
        id: &str,
    ) -> Result<(), AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }

        let mut state = self.state.write().await;
        let len_before = state.api_tokens.len();
        state.api_tokens.retain(|t| t.id != id);
        if state.api_tokens.len() == len_before {
            return Err(AuthError::UserNotFound);
        }
        save_state(&state).await?;

        audit("token_deleted", &session.username, session.client_ip.as_deref().unwrap_or(""), &format!("id={id}"));
        info!("Deleted API token '{id}'");
        Ok(())
    }

    /// Revoke a token (logout)
    pub async fn logout(&self, token: &str) -> Result<(), AuthError> {
        let mut state = self.state.write().await;
        let len_before = state.sessions.len();
        state.sessions.retain(|s| s.token != token);
        if state.sessions.len() == len_before {
            return Err(AuthError::InvalidToken);
        }
        save_state(&state).await?;
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

        let mut state = self.state.write().await;
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

        save_state(&state).await?;

        audit("password_changed", &session.username, session.client_ip.as_deref().unwrap_or(""), &format!("target={username}"));
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

        let mut state = self.state.write().await;
        if state.users.iter().any(|u| u.username == username) {
            return Err(AuthError::UserExists);
        }

        audit("user_created", &session.username, session.client_ip.as_deref().unwrap_or(""), &format!("target={username}, role={role:?}"));
        state.users.push(User {
            username: username.to_string(),
            password_hash: Some(hash_password(password)?),
            role,
            must_change_password: false,
            oidc_subject: None,
            oidc_issuer: None,
        });
        save_state(&state).await?;

        info!("Created user '{username}'");
        Ok(())
    }

    /// Delete a user (admin only, cannot delete self)
    pub async fn delete_user(
        &self,
        session: &Session,
        username: &str,
    ) -> Result<(), AuthError> {
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }
        if session.username == username {
            return Err(AuthError::Forbidden);
        }

        let mut state = self.state.write().await;
        let len_before = state.users.len();
        state.users.retain(|u| u.username != username);
        if state.users.len() == len_before {
            return Err(AuthError::UserNotFound);
        }

        // Also revoke all their sessions
        state.sessions.retain(|s| s.username != username);
        save_state(&state).await?;

        audit("user_deleted", &session.username, session.client_ip.as_deref().unwrap_or(""), &format!("target={username}"));
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
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UserInfo {
    pub username: String,
    pub role: Role,
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
    #[error("password must be at least 8 characters")]
    WeakPassword,
    #[error("password hash error: {0}")]
    HashError(String),
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
    let content = tokio::fs::read_to_string(AUDIT_LOG_PATH).await.unwrap_or_default();
    let mut entries: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    // Generate 16 random bytes for salt, encode as base64ct for SaltString
    let mut salt_bytes = [0u8; 16];
    rand::fill(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| AuthError::HashError(e.to_string()))?;
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::HashError(e.to_string()))?;
    Ok(hash.to_string())
}

fn verify_password(password: &str, hash: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(hash).map_err(|e| AuthError::HashError(e.to_string()))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::InvalidCredentials)
}

/// SHA-256 hash for API tokens. Tokens are 32 random bytes — high entropy,
/// no need for Argon2's brute-force resistance. Instant O(1) comparison.
fn hash_token(token: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
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
    let base = if base.is_empty() { identity.subject.clone() } else { base };
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

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

fn generate_id() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

async fn load_state() -> AuthState {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => AuthState::default(),
    }
}

async fn save_state(state: &AuthState) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(state).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    tokio::fs::set_permissions(STATE_PATH, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

async fn load_rate_limit() -> RateLimitState {
    match tokio::fs::read_to_string(RATE_LIMIT_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => RateLimitState::default(),
    }
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

    fn user(name: &str, role: Role) -> User {
        User {
            username: name.to_string(),
            password_hash: Some("hash".to_string()),
            role,
            must_change_password: false,
            oidc_subject: None,
            oidc_issuer: None,
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
        rl.by_user.insert(
            "bob".to_string(),
            vec![now - LOCKOUT_WINDOW_SECS - 100],
        );
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
