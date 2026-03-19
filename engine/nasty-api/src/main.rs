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

/// Broadcast channel for notifying all WebSocket clients of state changes.
/// The payload is the collection name (e.g. "pool", "subvolume", "share.nfs").
pub type EventBus = tokio::sync::broadcast::Sender<String>;

pub struct AppState {
    pub auth: AuthService,
    pub events: EventBus,
    pub system: nasty_system::SystemService,
    pub settings: nasty_system::settings::SettingsService,
    pub alerts: nasty_system::alerts::AlertService,
    pub network: nasty_system::network::NetworkService,
    pub protocols: nasty_system::protocol::ProtocolService,
    pub updates: nasty_system::update::UpdateService,
    pub metrics: Arc<nasty_system::metrics::MetricsDb>,
    pub pools: nasty_storage::PoolService,
    pub subvolumes: Arc<nasty_storage::SubvolumeService>,
    pub snapshots: nasty_snapshot::SnapshotService,
    pub nfs: nasty_sharing::NfsService,
    pub smb: nasty_sharing::SmbService,
    pub iscsi: nasty_sharing::IscsiService,
    pub nvmeof: Arc<nasty_sharing::NvmeofService>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nasty_api=debug,nasty_storage=debug,nasty_sharing=debug,nasty_snapshot=debug,nasty_system=info,tower_http=debug".into()),
        )
        .init();

    let (event_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    let metrics = Arc::new(
        nasty_system::metrics::MetricsDb::open()
            .expect("failed to open metrics database"),
    );

    let subvolumes = Arc::new(nasty_storage::SubvolumeService::new(nasty_storage::PoolService::new()));
    let nvmeof = Arc::new(nasty_sharing::NvmeofService::new());

    let state = Arc::new(AppState {
        auth: AuthService::new().await,
        events: event_tx,
        system: nasty_system::SystemService::new(),
        settings: nasty_system::settings::SettingsService::new().await,
        alerts: nasty_system::alerts::AlertService::new().await,
        network: nasty_system::network::NetworkService::new(),
        protocols: nasty_system::protocol::ProtocolService::new(),
        updates: nasty_system::update::UpdateService::new(),
        metrics: metrics.clone(),
        pools: nasty_storage::PoolService::new(),
        snapshots: nasty_snapshot::SnapshotService::new(subvolumes.clone(), nvmeof.clone()),
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

    // Background metrics collector: samples I/O rates every 5s, writes to SQLite
    tokio::spawn(metrics_collector(metrics));

    // Signal systemd that startup is complete
    sd_notify_ready();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/api/login", post(login_handler))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 2137));
    info!("NASty engine listening on {addr}");

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
    use futures_util::{SinkExt, StreamExt};
    use nasty_common::Notification;

    info!("WebSocket client connected, awaiting authentication");

    // First message must be an auth token
    let session = match wait_for_auth(&mut socket, &state).await {
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

// ── Metrics collector ───────────────────────────────────────────

async fn metrics_collector(db: Arc<nasty_system::metrics::MetricsDb>) {
    use nasty_system::SystemService;

    let sys = SystemService::new();
    let mut prev_stats = sys.stats().await;
    let mut prev_time = std::time::Instant::now();
    let mut prune_counter = 0u32;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(prev_time).as_secs_f64();
        if elapsed <= 0.0 {
            continue;
        }

        let stats = sys.stats().await;

        // Compute network rates
        let net_samples: Vec<(&str, f64, f64)> = stats
            .network
            .iter()
            .filter_map(|curr| {
                let prev = prev_stats.network.iter().find(|p| p.name == curr.name)?;
                let rx = (curr.rx_bytes.saturating_sub(prev.rx_bytes)) as f64 / elapsed;
                let tx = (curr.tx_bytes.saturating_sub(prev.tx_bytes)) as f64 / elapsed;
                Some((curr.name.as_str(), rx, tx))
            })
            .collect();

        // Compute disk rates
        let disk_samples: Vec<(&str, f64, f64)> = stats
            .disk_io
            .iter()
            .filter_map(|curr| {
                let prev = prev_stats.disk_io.iter().find(|p| p.name == curr.name)?;
                let read = (curr.read_bytes.saturating_sub(prev.read_bytes)) as f64 / elapsed;
                let write = (curr.write_bytes.saturating_sub(prev.write_bytes)) as f64 / elapsed;
                Some((curr.name.as_str(), read, write))
            })
            .collect();

        if !net_samples.is_empty() {
            db.insert("net", &net_samples);
        }
        if !disk_samples.is_empty() {
            db.insert("disk", &disk_samples);
        }

        // CPU usage percentage (load / cores × 100)
        let cpu_pct = (stats.cpu.load_1 / stats.cpu.count as f64) * 100.0;
        db.insert("cpu", &[("cpu", cpu_pct.min(100.0), 0.0)]);

        // Memory usage percentage
        let mem_pct = if stats.memory.total_bytes > 0 {
            (stats.memory.used_bytes as f64 / stats.memory.total_bytes as f64) * 100.0
        } else {
            0.0
        };
        db.insert("mem", &[("mem", mem_pct, 0.0)]);

        prev_stats = stats;
        prev_time = now;

        // Prune old data every ~5 minutes (60 iterations × 5s)
        prune_counter += 1;
        if prune_counter >= 60 {
            prune_counter = 0;
            db.prune();
        }
    }
}
