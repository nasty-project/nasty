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
use tracing_subscriber::{reload, prelude::*};

mod auth;
mod router;
mod terminal;

use auth::{AuthService, Session};
use router::handle_rpc_request;

/// Handle for dynamically reloading the tracing filter at runtime.
pub type LogReloadHandle = reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

/// Broadcast channel for notifying all WebSocket clients of state changes.
/// The payload is the collection name (e.g. "pool", "subvolume", "share.nfs").
pub type EventBus = tokio::sync::broadcast::Sender<String>;

pub struct AppState {
    pub auth: AuthService,
    pub events: EventBus,
    pub log_reload: LogReloadHandle,
    pub system: nasty_system::SystemService,
    pub settings: nasty_system::settings::SettingsService,
    pub alerts: nasty_system::alerts::AlertService,
    pub network: nasty_system::network::NetworkService,
    pub protocols: nasty_system::protocol::ProtocolService,
    pub updates: nasty_system::update::UpdateService,
    pub metrics_client: reqwest::Client,
    pub pools: nasty_storage::PoolService,
    pub subvolumes: Arc<nasty_storage::SubvolumeService>,
    pub snapshots: nasty_snapshot::SnapshotService,
    pub nfs: nasty_sharing::NfsService,
    pub smb: nasty_sharing::SmbService,
    pub iscsi: nasty_sharing::IscsiService,
    pub nvmeof: Arc<nasty_sharing::NvmeofService>,
}

/// Base URL for the nasty-metrics service.
pub const METRICS_BASE: &str = "http://127.0.0.1:2138";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("NASTY_GIT_COMMIT");
    let built = env!("NASTY_BUILD_DATE");

    let default_filter = "nasty_engine=debug,nasty_storage=debug,nasty_sharing=debug,nasty_snapshot=debug,nasty_system=info,tower_http=debug";
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    let (filter_layer, reload_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (event_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    let subvolumes = Arc::new(nasty_storage::SubvolumeService::new(nasty_storage::PoolService::new()));
    let nvmeof = Arc::new(nasty_sharing::NvmeofService::new());

    let state = Arc::new(AppState {
        auth: AuthService::new().await,
        events: event_tx,
        log_reload: reload_handle,
        system: nasty_system::SystemService::new(),
        settings: nasty_system::settings::SettingsService::new().await,
        alerts: nasty_system::alerts::AlertService::new().await,
        network: nasty_system::network::NetworkService::new(),
        protocols: nasty_system::protocol::ProtocolService::new(),
        updates: nasty_system::update::UpdateService::new(),
        metrics_client: reqwest::Client::new(),
        pools: nasty_storage::PoolService::new(),
        snapshots: nasty_snapshot::SnapshotService::new(subvolumes.clone()),
        subvolumes,
        nfs: nasty_sharing::NfsService::new(),
        smb: nasty_sharing::SmbService::new(),
        iscsi: nasty_sharing::IscsiService::new(),
        nvmeof,
    });

    // Restore state from previous session:
    // 1. Mount pools tracked in pool-state.json
    // 2. Re-attach loop devices for block subvolumes
    // 3. Start enabled protocols (services + kernel modules)
    // 4. Restore NVMe-oF configfs (volatile, needs modules from step 3)
    state.pools.restore_mounts().await;
    // Re-attach loop devices and get the current name→device mapping.
    // Loop device numbers change across reboots, so NVMe-oF and iSCSI state
    // files must be patched before their respective restore steps run.
    let dev_map = state.subvolumes.restore_block_devices().await;
    if !dev_map.is_empty() {
        state.nvmeof.remap_device_paths(&dev_map).await;
        state.iscsi.remap_device_paths(&dev_map).await;
    }
    state.protocols.restore().await;
    state.nvmeof.restore().await;

    // Pre-warm caches so first page loads are fast.
    // Runs before sd_notify_ready() — nginx won't serve until this completes.
    info!("Warming caches...");
    let t0 = std::time::Instant::now();
    tokio::join!(
        state.system.info(),
        state.updates.bcachefs_info(&state.system),
    );
    info!("Caches warm in {}ms", t0.elapsed().as_millis());

    // Signal systemd that startup is complete
    sd_notify_ready();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/api/login", post(login_handler))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 2137));
    info!("NASty Engine v{version} (commit: {commit}, built: {built})");
    info!("Listening on {addr}");

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
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    let client_ip = headers.get("x-real-ip").and_then(|v| v.to_str().ok()).unwrap_or("unknown");
    match state.auth.login(&req.username, &req.password, client_ip).await {
        Ok(token) => (StatusCode::OK, Json(serde_json::json!({ "token": token }))).into_response(),
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid credentials" })),
        )
            .into_response(),
    }
}

// ── WebSocket with auth ─────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    ws.on_upgrade(move |socket| handle_socket(socket, state, client_ip))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>, client_ip: String) {
    use futures_util::{SinkExt, StreamExt};
    use nasty_common::Notification;

    info!("WebSocket client connected from {client_ip}, awaiting authentication");

    // First message must be an auth token
    let session = match wait_for_auth(&mut socket, &state, &client_ip).await {
        Some(s) => s,
        None => return,
    };

    info!("WebSocket authenticated as '{}'", session.username);

    let mut event_rx = state.events.subscribe();
    let (mut writer, mut reader) = socket.split();

    loop {
        tokio::select! {
            msg = reader.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let response = handle_rpc_request(&text, &state, &session).await;
                        if writer.send(Message::Text(response.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            event = event_rx.recv() => {
                if let Ok(collection) = event {
                    let notification = Notification::new(
                        "event",
                        Some(serde_json::json!({ "collection": collection })),
                    );
                    let text = serde_json::to_string(&notification).unwrap();
                    if writer.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    info!("WebSocket client '{}' disconnected", session.username);
}

/// Wait for the first message which must be: {"token": "..."}
/// Returns the session if valid, or None if auth failed (socket is closed).
async fn wait_for_auth(socket: &mut WebSocket, state: &AppState, client_ip: &str) -> Option<Session> {
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

    match state.auth.validate(&auth_msg.token, client_ip).await {
        Ok(session) => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({
                        "authenticated": true,
                        "username": session.username,
                        "role": session.role,
                        "must_change_password": session.must_change_password
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            Some(session)
        }
        Err(e) => {
            tracing::warn!("Auth failed for client {client_ip}: {e}");
            let _ = socket
                .send(Message::Text(r#"{"error":"invalid token"}"#.into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            None
        }
    }
}

