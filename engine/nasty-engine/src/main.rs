use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{
        DefaultBodyLimit,
        Multipart,
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
mod telemetry;
mod terminal;
mod vm_console;

use auth::{AuthService, Session};
use router::handle_rpc_request;

/// Handle for dynamically reloading the tracing filter at runtime.
pub type LogReloadHandle = reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

/// Broadcast channel for notifying all WebSocket clients of state changes.
/// The payload is the collection name (e.g. "filesystem", "subvolume", "share.nfs").
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
    pub filesystems: nasty_storage::FilesystemService,
    pub subvolumes: Arc<nasty_storage::SubvolumeService>,
    pub snapshots: nasty_snapshot::SnapshotService,
    pub nfs: nasty_sharing::NfsService,
    pub smb: nasty_sharing::SmbService,
    pub iscsi: nasty_sharing::IscsiService,
    pub nvmeof: Arc<nasty_sharing::NvmeofService>,
    pub vms: nasty_vm::VmService,
    pub apps: nasty_apps::AppsService,
    pub firmware: nasty_system::firmware::FirmwareService,
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

    let subvolumes = Arc::new(nasty_storage::SubvolumeService::new(nasty_storage::FilesystemService::new()));
    let nvmeof = Arc::new(nasty_sharing::NvmeofService::new());

    let state = Arc::new(AppState {
        auth: AuthService::new().await,
        events: event_tx,
        log_reload: reload_handle,
        system: nasty_system::SystemService::new(
            Some(commit.to_string()),
            Some(built.to_string()),
        ),
        settings: nasty_system::settings::SettingsService::new().await,
        alerts: nasty_system::alerts::AlertService::new().await,
        network: nasty_system::network::NetworkService::new(),
        protocols: nasty_system::protocol::ProtocolService::new(),
        updates: nasty_system::update::UpdateService::new(),
        metrics_client: reqwest::Client::new(),
        filesystems: nasty_storage::FilesystemService::new(),
        snapshots: nasty_snapshot::SnapshotService::new(subvolumes.clone()),
        subvolumes,
        nfs: nasty_sharing::NfsService::new(),
        smb: nasty_sharing::SmbService::new(),
        iscsi: nasty_sharing::IscsiService::new(),
        nvmeof,
        vms: nasty_vm::VmService::new(),
        apps: nasty_apps::AppsService::new(),
        firmware: nasty_system::firmware::FirmwareService::new(),
    });

    // Restore state from previous session:
    // 1. Mount filesystems tracked in fs-state.json
    // 2. Re-attach loop devices for block subvolumes
    // 3. Start enabled protocols (services + kernel modules)
    // 4. Restore NVMe-oF configfs (volatile, needs modules from step 3)
    state.filesystems.restore_mounts().await;
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
    state.vms.restore().await;
    state.apps.restore().await;

    // Pre-warm caches so first page loads are fast.
    // Runs before sd_notify_ready() — nginx won't serve until this completes.
    info!("Warming caches...");
    let t0 = std::time::Instant::now();
    tokio::join!(
        state.system.info(),
        state.updates.bcachefs_info(&state.system),
    );
    info!("Caches warm in {}ms", t0.elapsed().as_millis());

    // Check ACME cert renewal in background (non-blocking)
    tokio::spawn(async {
        nasty_system::settings::check_acme_renewal().await;
    });

    // Start daily anonymous telemetry (if not opted out)
    telemetry::spawn_daily(state.clone());

    // Signal systemd that startup is complete
    sd_notify_ready();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/ws/vm/{vm_id}/vnc", get(vm_console::vnc_handler))
        .route("/ws/vm/{vm_id}/serial", get(vm_console::serial_handler))
        .route("/api/login", post(login_handler))
        .route("/api/upload/vm-image", post(upload_vm_image_handler).layer(DefaultBodyLimit::max(10_737_418_240)))
        .route("/api/files/browse", get(files_browse_handler))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 2137));
    info!("NASty Engine v{version} (commit: {commit}, built: {built})");
    info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;

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

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "commit": env!("NASTY_GIT_COMMIT"),
        "built": env!("NASTY_BUILD_DATE"),
    }))
}

// ── VM Image Upload ────────────────────────────────────────────────

async fn upload_vm_image_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let client_ip = headers.get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    info!("VM image upload request from {}", client_ip);

    // Authenticate
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    let token = match token {
        Some(t) => t,
        None => {
            info!("VM image upload rejected: missing auth token (from {})", client_ip);
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Missing authorization token" }))).into_response();
        }
    };

    let session = match state.auth.validate(&token, &client_ip).await {
        Ok(s) => s,
        Err(e) => {
            info!("VM image upload rejected: invalid token (from {})", client_ip);
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": format!("Invalid token: {}", e) }))).into_response();
        }
    };

    // Get or create the images subvolume
    let filesystems = state.filesystems.list().await.unwrap_or_default();
    let fs_name = filesystems.first().map(|f| f.name.clone()).unwrap_or_default();
    
    if fs_name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "No filesystems available" }))).into_response();
    }

    let subvolume = match state.subvolumes.get(&fs_name, "images", None).await {
        Ok(sv) => sv,
        Err(_) => {
            // Create the images subvolume if it doesn't exist
            let req = nasty_storage::subvolume::CreateSubvolumeRequest {
                filesystem: fs_name.clone(),
                name: "images".to_string(),
                subvolume_type: nasty_storage::subvolume::SubvolumeType::Filesystem,
                volsize_bytes: None,
                compression: Some("zstd".to_string()),
                comments: Some("VM images (ISO, qcow2, img, raw)".to_string()),
                direct_io: None,
            };
            match state.subvolumes.create(req, None::<String>).await {
                Ok(sv) => sv,
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Failed to create images subvolume: {}", e) }))).into_response();
                }
            }
        }
    };

    // Process the uploaded file
    let Some(mut field) = multipart.next_field().await.ok().flatten() else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "No file provided" }))).into_response();
    };

    let raw_name = field.file_name().unwrap_or("").to_string();
    // Sanitize: strip any path components to prevent path traversal
    let file_name = std::path::Path::new(&raw_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    if file_name.is_empty() {
        info!("VM image upload rejected: empty filename (user '{}')", session.username);
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "No file provided" }))).into_response();
    }

    info!("User '{}' uploading VM image: '{}' to {}", session.username, file_name, subvolume.path);

    let extensions = ["iso", "qcow2", "img", "raw"];
    let ext = std::path::Path::new(&file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !extensions.contains(&ext.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Invalid file type. Supported: {:?}", extensions) }))).into_response();
    }

    let dest_path = std::path::Path::new(&subvolume.path).join(&file_name);

    if dest_path.exists() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "error": format!("Image '{}' already exists", file_name) }))).into_response();
    }

    // Stream file content to disk
    let mut file = match tokio::fs::File::create(&dest_path).await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Failed to create file: {}", e) }))).into_response();
        }
    };

    use tokio::io::AsyncWriteExt;
    let cleanup = || async { let _ = tokio::fs::remove_file(&dest_path).await; };
    let start = std::time::Instant::now();
    let mut total_bytes: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                total_bytes += chunk.len() as u64;
                if let Err(e) = file.write_all(&chunk).await {
                    drop(file);
                    cleanup().await;
                    tracing::error!("VM image upload write failed after {} bytes for '{}': {}", total_bytes, file_name, e);
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Failed to write chunk: {}", e) }))).into_response();
                }
            }
            Ok(None) => break,
            Err(e) => {
                drop(file);
                cleanup().await;
                tracing::error!("VM image upload stream failed after {} bytes for '{}': {}", total_bytes, file_name, e);
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Failed to read chunk: {}", e) }))).into_response();
            }
        }
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        cleanup().await;
        tracing::error!("VM image upload sync failed for '{}': {}", file_name, e);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Failed to sync file: {}", e) }))).into_response();
    }

    let elapsed = start.elapsed();
    let size_mib = total_bytes as f64 / (1024.0 * 1024.0);
    let rate_mibs = if elapsed.as_secs_f64() > 0.0 { size_mib / elapsed.as_secs_f64() } else { 0.0 };
    info!("User '{}' uploaded VM image: '{}' ({:.1} MiB in {:.1}s, {:.1} MiB/s)", session.username, file_name, size_mib, elapsed.as_secs_f64(), rate_mibs);
    (StatusCode::OK, Json(serde_json::json!({
        "name": file_name,
        "path": dest_path.to_string_lossy(),
        "filesystem": fs_name,
    }))).into_response()
}

// ── File Browser endpoints ──────────────────────────────────────

const FILES_ROOT: &str = "/fs";

/// Validate that a path is under /fs and doesn't escape via traversal.
fn safe_path(requested: &str) -> Result<std::path::PathBuf, StatusCode> {
    let clean = requested.replace("\\", "/");
    let joined = std::path::Path::new(FILES_ROOT).join(clean.trim_start_matches('/'));
    let canonical = joined.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !canonical.starts_with(FILES_ROOT) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(canonical)
}

/// List directory contents. GET /api/files/browse?path=/first
async fn files_browse_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // Auth check
    let token = headers.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let token = match token {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Missing token"}))).into_response(),
    };
    let client_ip = headers.get("x-real-ip").and_then(|v| v.to_str().ok()).unwrap_or("unknown");
    if state.auth.validate(&token, client_ip).await.is_err() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid token"}))).into_response();
    }

    let req_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match safe_path(req_path) {
        Ok(p) => p,
        Err(status) => return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response(),
    };

    let meta = match tokio::fs::metadata(&dir).await {
        Ok(m) => m,
        Err(_) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Not found"}))).into_response(),
    };

    if !meta.is_dir() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Not a directory"}))).into_response();
    }

    let mut entries = Vec::new();
    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().await.ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(serde_json::json!({
            "name": name,
            "is_dir": is_dir,
            "size": if is_dir { 0 } else { size },
            "modified": modified,
        }));
    }

    // Sort: directories first, then by name
    entries.sort_by(|a, b| {
        let a_dir = a["is_dir"].as_bool().unwrap_or(false);
        let b_dir = b["is_dir"].as_bool().unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    let display_path = dir.strip_prefix(FILES_ROOT).unwrap_or(&dir).to_string_lossy().to_string();
    (StatusCode::OK, Json(serde_json::json!({
        "path": display_path,
        "entries": entries,
    }))).into_response()
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
        Ok(token) => {
            info!("Login successful: user '{}' from {}", req.username, client_ip);
            (StatusCode::OK, Json(serde_json::json!({ "token": token }))).into_response()
        }
        Err(_) => {
            tracing::warn!("Login failed: user '{}' from {}", req.username, client_ip);
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "invalid credentials" }))).into_response()
        }
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

