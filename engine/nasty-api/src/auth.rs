use std::sync::Arc;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    ReadOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub username: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AuthState {
    users: Vec<User>,
    sessions: Vec<Session>,
    initialized: bool,
}

pub struct AuthService {
    state: Arc<RwLock<AuthState>>,
}

impl AuthService {
    pub async fn new() -> Self {
        let state = load_state().await;
        let svc = Self {
            state: Arc::new(RwLock::new(state)),
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
        let mut state = self.state.write().await;

        let user = state
            .users
            .iter()
            .find(|u| u.username == username)
            .ok_or(AuthError::InvalidCredentials)?;

        verify_password(password, &user.password_hash)?;

        let token = generate_token();
        let session = Session {
            token: token.clone(),
            username: user.username.clone(),
            role: user.role.clone(),
        };

        state.sessions.push(session);
        save_state(&state).await?;

        info!("User '{}' logged in", username);
        Ok(token)
    }

    /// Validate a token and return the session
    pub async fn validate(&self, token: &str) -> Result<Session, AuthError> {
        let state = self.state.read().await;
        state
            .sessions
            .iter()
            .find(|s| s.token == token)
            .cloned()
            .ok_or(AuthError::InvalidToken)
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

    /// Check if the token has admin role
    pub async fn require_admin(&self, token: &str) -> Result<Session, AuthError> {
        let session = self.validate(token).await?;
        if session.role != Role::Admin {
            return Err(AuthError::Forbidden);
        }
        Ok(session)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserInfo {
    pub username: String,
    pub role: Role,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("invalid or expired token")]
    InvalidToken,
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

async fn load_state() -> AuthState {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => AuthState::default(),
    }
}

async fn save_state(state: &AuthState) -> Result<(), AuthError> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(state).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}
