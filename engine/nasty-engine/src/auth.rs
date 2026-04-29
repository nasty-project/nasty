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
    /// Argon2 password hash
    pub password_hash: String,
    pub role: Role,
    /// When true, the user must change their password before accessing anything else.
    #[serde(default)]
    pub must_change_password: bool,
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

/// Max failed login attempts before lockout.
const MAX_FAILED_ATTEMPTS: usize = 5;
/// Window for counting failed attempts and lockout duration (seconds).
const LOCKOUT_WINDOW_SECS: u64 = 15 * 60; // 15 minutes

pub struct AuthService {
    state: Arc<RwLock<AuthState>>,
    /// In-memory failed login tracking: username → list of failure timestamps.
    /// Not persisted — resets on engine restart.
    failed_attempts: Arc<RwLock<std::collections::HashMap<String, Vec<u64>>>>,
}

impl AuthService {
    pub async fn new() -> Self {
        let state = load_state().await;
        let svc = Self {
            state: Arc::new(RwLock::new(state)),
            failed_attempts: Arc::new(RwLock::new(std::collections::HashMap::new())),
        };

        // If no users exist, create default admin
        if !svc.state.read().await.initialized {
            let mut st = svc.state.write().await;
            let hash = hash_password("admin").expect("failed to hash default password");
            st.users.push(User {
                username: "admin".to_string(),
                password_hash: hash,
                role: Role::Admin,
                must_change_password: true,
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

        // Check if the account is locked out due to too many failed attempts
        {
            let attempts = self.failed_attempts.read().await;
            if let Some(failures) = attempts.get(username) {
                let recent: Vec<_> = failures.iter().filter(|&&t| now - t < LOCKOUT_WINDOW_SECS).collect();
                if recent.len() >= MAX_FAILED_ATTEMPTS {
                    tracing::warn!("Login blocked for '{}': {} failed attempts in last {} minutes",
                        username, recent.len(), LOCKOUT_WINDOW_SECS / 60);
                    audit("login_locked", username, client_ip, &format!("{} failed attempts", recent.len()));
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
                self.record_failed_attempt(username, now).await;
                return Err(e);
            }
        };

        match verify_password(password, &user.password_hash) {
            Ok(()) => {}
            Err(e) => {
                audit("login_failed", username, client_ip, "wrong password");
                self.record_failed_attempt(username, now).await;
                return Err(e);
            }
        }

        // Successful login — clear failed attempts
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

        user.password_hash = hash_password(new_password)?;
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
            password_hash: hash_password(password)?,
            role,
            must_change_password: false,
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

    async fn record_failed_attempt(&self, username: &str, now: u64) {
        let mut attempts = self.failed_attempts.write().await;
        let entry = attempts.entry(username.to_string()).or_default();
        entry.push(now);
        // Prune old entries outside the window
        entry.retain(|&t| now - t < LOCKOUT_WINDOW_SECS);
    }

    async fn clear_failed_attempts(&self, username: &str) {
        let mut attempts = self.failed_attempts.write().await;
        attempts.remove(username);
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

/// Max audit log size before rotation (1 MiB).
const AUDIT_LOG_MAX_BYTES: u64 = 1_048_576;

/// Append a structured event to the audit log (JSONL, append-only).
/// Rotates to .old when the log exceeds 1 MiB.
pub fn audit(event: &str, user: &str, ip: &str, detail: &str) {
    use std::io::Write;

    // Rotate if too large
    if let Ok(meta) = std::fs::metadata(AUDIT_LOG_PATH) {
        if meta.len() > AUDIT_LOG_MAX_BYTES {
            let _ = std::fs::rename(AUDIT_LOG_PATH, format!("{AUDIT_LOG_PATH}.old"));
        }
    }

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
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(AUDIT_LOG_PATH) {
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
