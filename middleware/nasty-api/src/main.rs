use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tracing::info;

mod auth;
mod router;
mod terminal;

use auth::{AuthService, Session};
use router::handle_rpc_request;

pub struct AppState {
    pub auth: AuthService,
    pub system: nasty_system::SystemService,
    pub settings: nasty_system::settings::SettingsService,
    pub alerts: nasty_system::alerts::AlertService,
    pub protocols: nasty_system::protocol::ProtocolService,
    pub updates: nasty_system::update::UpdateService,
    pub pools: nasty_storage::PoolService,
    pub subvolumes: nasty_storage::SubvolumeService,
    pub nfs: nasty_sharing::NfsService,
    pub smb: nasty_sharing::SmbService,
    pub iscsi: nasty_sharing::IscsiService,
    pub nvmeof: nasty_sharing::NvmeofService,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nasty_api=debug,tower_http=debug".into()),
        )
        .init();

    let state = Arc::new(AppState {
        auth: AuthService::new().await,
        system: nasty_system::SystemService::new(),
        settings: nasty_system::settings::SettingsService::new().await,
        alerts: nasty_system::alerts::AlertService::new().await,
        protocols: nasty_system::protocol::ProtocolService::new(),
        updates: nasty_system::update::UpdateService::new(),
        pools: nasty_storage::PoolService::new(),
        subvolumes: nasty_storage::SubvolumeService::new(nasty_storage::PoolService::new()),
        nfs: nasty_sharing::NfsService::new(),
        smb: nasty_sharing::SmbService::new(),
        iscsi: nasty_sharing::IscsiService::new(),
        nvmeof: nasty_sharing::NvmeofService::new(),
    });

    // Restore state from previous session:
    // 1. Mount pools tracked in pool-state.json
    // 2. Re-attach loop devices for block subvolumes
    // 3. Restore NVMe-oF configfs (volatile, lost on reboot)
    // 4. Start enabled protocol services (NFS, SMB, load kernel modules)
    state.pools.restore_mounts().await;
    state.subvolumes.restore_block_devices().await;
    state.nvmeof.restore().await;
    state.protocols.restore().await;

    // Signal systemd that startup is complete
    sd_notify_ready();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/api/login", post(login_handler))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 2137));
    info!("NASty middleware listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Notify systemd that the service is ready (Type=notify).
fn sd_notify_ready() {
    let Some(sock_path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let sock = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = sock.send_to(b"READY=1", &sock_path);
    info!("Notified systemd: READY");
}

async fn health() -> &'static str {
    "ok"
}

// ── Login endpoint ──────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    match state.auth.login(&req.username, &req.password).await {
        Ok(token) => (StatusCode::OK, Json(serde_json::json!({ "token": token }))).into_response(),
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid credentials" })),
        )
            .into_response(),
    }
}

// ── WebSocket with auth ─────────────────────────────────────────

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    info!("WebSocket client connected, awaiting authentication");

    // First message must be an auth token
    let session = match wait_for_auth(&mut socket, &state).await {
        Some(s) => s,
        None => return,
    };

    info!("WebSocket authenticated as '{}'", session.username);

    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(text) => {
                let response = handle_rpc_request(&text, &state, &session).await;
                if socket.send(Message::Text(response.into())).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    info!("WebSocket client '{}' disconnected", session.username);
}

/// Wait for the first message which must be: {"token": "..."}
/// Returns the session if valid, or None if auth failed (socket is closed).
async fn wait_for_auth(socket: &mut WebSocket, state: &AppState) -> Option<Session> {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv())
        .await
        .ok()??
        .ok()?;

    let text = match msg {
        Message::Text(t) => t,
        _ => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"first message must be JSON with token"}"#.into(),
                ))
                .await;
            return None;
        }
    };

    #[derive(Deserialize)]
    struct AuthMsg {
        token: String,
    }

    let auth_msg: AuthMsg = match serde_json::from_str(&text) {
        Ok(a) => a,
        Err(_) => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"expected {\"token\": \"...\"}"}"#.into(),
                ))
                .await;
            return None;
        }
    };

    match state.auth.validate(&auth_msg.token).await {
        Ok(session) => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({
                        "authenticated": true,
                        "username": session.username,
                        "role": session.role
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            Some(session)
        }
        Err(_) => {
            let _ = socket
                .send(Message::Text(r#"{"error":"invalid token"}"#.into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            None
        }
    }
}
