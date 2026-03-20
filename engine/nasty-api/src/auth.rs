use std::sync::Arc;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;

const STATE_PATH: &str = "/var/lib/nasty/auth.json";
const STATE_DIR: &str = "/var/lib/nasty";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    /// Argon2 password hash
    pub password_hash: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    ReadOnly,
    /// Can create/delete/attach subvolumes and snapshots, read pools.
    /// Cannot destroy pools, manage users, or touch system settings.
    Operator,
}

/// Login sessions expire after this many seconds.
const SESSION_TTL_SECS: u64 = 8 * 3600; // 8 hours

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub token: String,
    pub username: String,
    pub role: Role,
    /// For API tokens: restricts pool visibility to a single pool.
    #[serde(default)]
    pub pool: Option<String>,
    /// For API tokens: only subvolumes with this owner are visible/manageable.
    #[serde(default)]
    pub owner: Option<String>,
    /// Unix timestamp when this session was created. None for legacy sessions.
    #[serde(default)]
    pub created_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    /// Argon2 hash of the token value. The raw token is returned only once on creation.
    /// Legacy tokens stored in plaintext are auto-migrated on next validation.
    pub token: String,
    pub role: Role,
    pub created_at: u64,
    /// If set, token can only see/manage subvolumes in this pool.
    #[serde(default)]
    pub pool: Option<String>,
    /// Unix timestamp after which the token is rejected. None = never expires.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// True when the token field contains an Argon2 hash (not plaintext).
    #[serde(default)]
    pub hashed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiTokenInfo {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub created_at: u64,
    pub pool: Option<String>,
    pub expires_at: Option<u64>,
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
            });
            st.initialized = true;
            save_state(&st).await.ok();
            info!("Created default admin user (password: admin) — change this immediately!");
        }

        svc
    }

    /// Authenticate with username/password, returns a session token
    pub async fn login(&self, username: &str, password: &str) -> Result<String, AuthError> {
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
                self.record_failed_attempt(username, now).await;
                return Err(e);
            }
        };

        match verify_password(password, &user.password_hash) {
            Ok(()) => {}
            Err(e) => {
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
            pool: None,
            owner: None,
            created_at: Some(now),
        };

        // Prune expired sessions while we hold the write lock
        state.sessions.retain(|s| {
            s.created_at.map_or(true, |c| now - c <= SESSION_TTL_SECS)
        });

        state.sessions.push(session);
        save_state(&state).await?;

        info!("User '{}' logged in", username);
        Ok(token)
    }

    /// Validate a token and return the session (checks both login sessions and API tokens)
    pub async fn validate(&self, token: &str) -> Result<Session, AuthError> {
        let state = self.state.read().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Check login sessions first
        if let Some(session) = state.sessions.iter().find(|s| s.token == token) {
            // Check TTL — sessions without created_at (legacy) are treated as valid
            if let Some(created) = session.created_at {
                if now - created > SESSION_TTL_SECS {
                    drop(state);
                    // Clean up the expired session
                    let mut state = self.state.write().await;
                    state.sessions.retain(|s| s.token != token);
                    save_state(&state).await.ok();
                    return Err(AuthError::TokenExpired);
                }
            }
            return Ok(session.clone());
        }
        // Check long-lived API tokens (these have their own expiry via expires_at, not SESSION_TTL)
        // Try each stored token: hashed ones use Argon2 verify, legacy plaintext uses ==
        let matched = state.api_tokens.iter().find(|t| {
            if t.hashed {
                verify_password(token, &t.token).is_ok()
            } else {
                t.token == token
            }
        });

        let t = matched.ok_or(AuthError::InvalidToken)?;

        if let Some(exp) = t.expires_at {
            if now >= exp {
                return Err(AuthError::TokenExpired);
            }
        }

        let session = Session {
            token: token.to_string(),
            username: t.name.clone(),
            role: t.role.clone(),
            pool: t.pool.clone(),
            owner: if t.role == Role::Operator { Some(t.name.clone()) } else { None },
            created_at: Some(t.created_at),
        };

        // Auto-migrate legacy plaintext token to hashed
        if !t.hashed {
            let name = t.name.clone();
            drop(state);
            let mut state = self.state.write().await;
            if let Some(t) = state.api_tokens.iter_mut().find(|t| t.name == name && !t.hashed) {
                if let Ok(h) = hash_password(token) {
                    t.token = h;
                    t.hashed = true;
                    save_state(&state).await.ok();
                    info!("Auto-migrated API token '{}' to hashed storage", name);
                }
            }
        }

        Ok(session)
    }

    /// Create a long-lived API token (admin only). Returns the token value — shown only once.
    pub async fn create_api_token(
        &self,
        session: &Session,
        name: &str,
        role: Role,
        pool: Option<String>,
        expires_in_secs: Option<u64>,
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
        let token_hash = hash_password(&raw_token)?;
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
            pool: pool.clone(),
            expires_at,
            hashed: true,
        };

        state.api_tokens.push(stored);
        save_state(&state).await?;

        info!("Created API token '{name}'");

        // Return the raw token to the caller — shown only once, never stored
        Ok(ApiToken {
            id,
            name: name.to_string(),
            token: raw_token,
            role,
            created_at,
            pool,
            expires_at,
            hashed: false, // the returned copy has the raw value
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
                pool: t.pool.clone(),
                expires_at: t.expires_at,
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
        save_state(&state).await?;

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

        state.users.push(User {
            username: username.to_string(),
            password_hash: hash_password(password)?,
            role,
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
    pub async fn require_admin(&self, token: &str) -> Result<Session, AuthError> {
        let session = self.validate(token).await?;
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
