use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Multipart, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::Deserialize;
use tracing::info;
use tracing_subscriber::{prelude::*, reload};

mod app_deploy;
mod auth;
mod router;
mod telemetry;
mod terminal;
mod vm_console;

use auth::{AuthService, Session};
use router::handle_rpc_request;

/// Handle for dynamically reloading the tracing filter at runtime.
pub type LogReloadHandle =
    reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

/// Broadcast channel for notifying all WebSocket clients of state changes.
/// The payload is the collection name (e.g. "filesystem", "subvolume", "share.nfs").
pub type EventBus = tokio::sync::broadcast::Sender<String>;

pub struct AppState {
    pub auth: AuthService,
    pub events: EventBus,
    pub log_reload: LogReloadHandle,
    pub system: nasty_system::SystemService,
    pub settings: nasty_system::settings::SettingsService,
    pub tuning: nasty_system::tuning::TuningService,
    pub nut: nasty_system::nut::NutService,
    pub alerts: nasty_system::alerts::AlertService,
    pub network: nasty_system::network::NetworkService,
    pub protocols: nasty_system::protocol::ProtocolService,
    pub firewall: nasty_system::firewall::FirewallService,
    pub updates: nasty_system::update::UpdateService,
    pub tailscale: nasty_system::tailscale::TailscaleService,
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
    pub backups: nasty_backup::BackupService,
    pub firmware: nasty_system::firmware::FirmwareService,
    /// Cached alerts result (timestamp, json value). Avoids re-evaluating
    /// all alert checks on every WebUI poll (called every few seconds).
    pub alerts_cache: tokio::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>>,
}

/// Base URL for the nasty-metrics service.
pub const METRICS_BASE: &str = "http://127.0.0.1:2138";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let built = env!("NASTY_BUILD_DATE");
    let args = std::env::args().collect::<Vec<_>>();

    // --version flag
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("nasty-engine {version} (built: {built})");
        return Ok(());
    }

    if matches!(
        args.get(1).map(String::as_str),
        Some("bootstrap-system-flake")
    ) {
        run_bootstrap_system_flake_cli(&args[2..]).await?;
        return Ok(());
    }

    let default_filter = "nasty_engine=debug,nasty_storage=debug,nasty_sharing=debug,nasty_snapshot=debug,nasty_system=info,tower_http=debug";
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    let (filter_layer, reload_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (event_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    let subvolumes = Arc::new(nasty_storage::SubvolumeService::new(
        nasty_storage::FilesystemService::new(),
    ));
    let nvmeof = Arc::new(nasty_sharing::NvmeofService::new());

    let state = Arc::new(AppState {
        auth: AuthService::new().await,
        events: event_tx,
        log_reload: reload_handle,
        system: nasty_system::SystemService::new(None, Some(built.to_string())),
        settings: nasty_system::settings::SettingsService::new().await,
        tuning: nasty_system::tuning::TuningService::new().await,
        nut: nasty_system::nut::NutService::new().await,
        alerts: nasty_system::alerts::AlertService::new().await,
        network: nasty_system::network::NetworkService::new(),
        protocols: nasty_system::protocol::ProtocolService::new(),
        firewall: nasty_system::firewall::FirewallService::new(),
        updates: nasty_system::update::UpdateService::new(),
        tailscale: nasty_system::tailscale::TailscaleService::new().await,
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
        backups: nasty_backup::BackupService::new(),
        firmware: nasty_system::firmware::FirmwareService::new(),
        alerts_cache: tokio::sync::Mutex::new(None),
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

    // Initialize firewall based on current protocol states
    {
        use nasty_system::protocol::Protocol;
        let mut proto_states = Vec::new();
        for p in Protocol::ALL {
            let enabled = state.protocols.is_enabled(*p).await;
            proto_states.push((*p, enabled));
        }
        state.firewall.init(&proto_states).await;
    }

    // Determine desired SSH password auth state from persisted config
    let ssh_password_auth = if let Ok(content) = tokio::fs::read_to_string("/var/lib/nasty/ssh.json").await {
        serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .and_then(|v| v.get("password_auth").and_then(|v| v.as_bool()))
            .unwrap_or(true)
    } else if let Ok(nix) = tokio::fs::read_to_string("/etc/nixos/ssh.nix").await {
        !nix.contains("false")
    } else {
        true
    };

    // Ensure ssh.nix exists and reflects the correct state
    {
        let val = if ssh_password_auth { "true" } else { "false" };
        let nix = format!(
            "# Managed by NASty — edit via WebUI Settings > SSH Access\n{{ ... }}:\n{{\n  services.openssh.settings.PasswordAuthentication = {val};\n}}\n"
        );
        let _ = tokio::fs::write("/etc/nixos/ssh.nix", &nix).await;
        // Also ensure ssh.json exists
        let _ = tokio::fs::write("/var/lib/nasty/ssh.json",
            serde_json::json!({ "password_auth": ssh_password_auth }).to_string()
        ).await;
    }

    // Apply SSH setting (handles reboot before rebuild)
    if !ssh_password_auth {
        let _ = tokio::process::Command::new("sed")
            .args(["-i", "s/^.*PasswordAuthentication.*/PasswordAuthentication no/", "/etc/ssh/sshd_config"])
            .status().await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["reload", "sshd"])
            .status().await;
        info!("SSH password authentication disabled (restored from config)");
    }

    state.nvmeof.restore().await;
    state.vms.restore().await;
    state.apps.restore().await;
    state.tailscale.restore().await;

    // Sync NVMe-oF ports with Tailscale IP (if Tailscale reconnected on boot)
    {
        let ts_status = state.tailscale.get().await;
        if ts_status.connected {
            if let Some(ref ip) = ts_status.ip {
                state.nvmeof.ensure_tailscale_ports(ip).await;
            }
        }
    }

    // Pre-warm caches so first page loads are fast.
    // Runs before sd_notify_ready() — nginx won't serve until this completes.
    info!("Warming caches...");
    let t0 = std::time::Instant::now();
    state.system.info().await;
    info!("Caches warm in {}ms", t0.elapsed().as_millis());

    // Check ACME cert renewal in background (non-blocking)
    tokio::spawn(async {
        nasty_system::settings::check_acme_renewal().await;
    });

    // Start daily anonymous telemetry (if not opted out)
    telemetry::spawn_daily(state.clone());

    // Background alert evaluation + notifications
    spawn_alert_notifier(state.clone());

    // Periodic config backup to bcachefs
    nasty_system::backup::spawn_periodic();

    // Signal systemd that startup is complete
    sd_notify_ready();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/ws/apps/deploy", get(app_deploy::deploy_handler))
        .route("/ws/vm/{vm_id}/vnc", get(vm_console::vnc_handler))
        .route("/ws/vm/{vm_id}/serial", get(vm_console::serial_handler))
        .route("/api/login", post(login_handler))
        .route(
            "/api/upload/vm-image",
            post(upload_vm_image_handler).layer(DefaultBodyLimit::max(10_737_418_240)),
        )
        .route("/api/files/browse", get(files_browse_handler))
        .route("/api/files", delete(files_delete_handler))
        .route(
            "/api/files/upload",
            post(files_upload_handler).layer(DefaultBodyLimit::max(10_737_418_240)),
        )
        .route("/api/files/mkdir", post(files_mkdir_handler))
        .route("/api/files/content", get(files_content_handler))
        .route("/api/auth/check", get(auth_check_handler))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 2137));
    info!("NASty Engine v{version} (built: {built})");
    info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}

async fn run_bootstrap_system_flake_cli(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: nasty-engine bootstrap-system-flake --dest-dir <dir> --template-file <path> --system <system>"
        );
        return Ok(());
    }

    let dest_dir = required_flag_value(args, "--dest-dir")?;
    let template_file = required_flag_value(args, "--template-file")?;
    let local_system = required_flag_value(args, "--system")?;
    let nasty_version = env!("CARGO_PKG_VERSION");

    let result = nasty_system::update::bootstrap_system_flake_from_template_path(
        &template_file,
        &dest_dir,
        nasty_version,
        &local_system,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    println!("{}", result.flake_path);
    Ok(())
}

fn required_flag_value(args: &[String], flag: &str) -> anyhow::Result<String> {
    let idx = args
        .iter()
        .position(|arg| arg == flag)
        .ok_or_else(|| anyhow::anyhow!("missing required flag: {flag}"))?;
    args.get(idx + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing value for flag: {flag}"))
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
        "built": env!("NASTY_BUILD_DATE"),
    }))
}

// ── VM Image Upload ────────────────────────────────────────────────

async fn upload_vm_image_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
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
            info!(
                "VM image upload rejected: missing auth token (from {})",
                client_ip
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Missing authorization token" })),
            )
                .into_response();
        }
    };

    let session = match state.auth.validate(&token, &client_ip).await {
        Ok(s) => s,
        Err(e) => {
            info!(
                "VM image upload rejected: invalid token (from {})",
                client_ip
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": format!("Invalid token: {}", e) })),
            )
                .into_response();
        }
    };

    // Get or create the images subvolume
    let filesystems = state.filesystems.list().await.unwrap_or_default();
    let fs_name = filesystems
        .first()
        .map(|f| f.name.clone())
        .unwrap_or_default();

    if fs_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No filesystems available" })),
        )
            .into_response();
    }

    let images_path = {
        let fs = match state.filesystems.get(&fs_name).await {
            Ok(f) => f,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        let mp = match fs.mount_point {
            Some(ref p) => p.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "Filesystem not mounted" })),
                )
                    .into_response();
            }
        };
        let path = format!("{mp}/.nasty/images");
        if let Err(e) = tokio::fs::create_dir_all(&path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({ "error": format!("Failed to create .nasty/images: {e}") }),
                ),
            )
                .into_response();
        }
        path
    };

    // Process the uploaded file
    let Some(mut field) = multipart.next_field().await.ok().flatten() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No file provided" })),
        )
            .into_response();
    };

    let raw_name = field.file_name().unwrap_or("").to_string();
    // Sanitize: strip any path components to prevent path traversal
    let file_name = std::path::Path::new(&raw_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    if file_name.is_empty() {
        info!(
            "VM image upload rejected: empty filename (user '{}')",
            session.username
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No file provided" })),
        )
            .into_response();
    }

    info!(
        "User '{}' uploading VM image: '{}' to {}",
        session.username, file_name, images_path
    );

    let extensions = ["iso", "qcow2", "img", "raw"];
    let ext = std::path::Path::new(&file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !extensions.contains(&ext.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Invalid file type. Supported: {:?}", extensions) }))).into_response();
    }

    let dest_path = std::path::Path::new(&images_path).join(&file_name);

    if dest_path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("Image '{}' already exists", file_name) })),
        )
            .into_response();
    }

    // Stream file content to disk
    let mut file = match tokio::fs::File::create(&dest_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Failed to create file: {}", e) })),
            )
                .into_response();
        }
    };

    use tokio::io::AsyncWriteExt;
    let cleanup = || async {
        let _ = tokio::fs::remove_file(&dest_path).await;
    };
    let start = std::time::Instant::now();
    let mut total_bytes: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                total_bytes += chunk.len() as u64;
                if let Err(e) = file.write_all(&chunk).await {
                    drop(file);
                    cleanup().await;
                    tracing::error!(
                        "VM image upload write failed after {} bytes for '{}': {}",
                        total_bytes,
                        file_name,
                        e
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            serde_json::json!({ "error": format!("Failed to write chunk: {}", e) }),
                        ),
                    )
                        .into_response();
                }
            }
            Ok(None) => break,
            Err(e) => {
                drop(file);
                cleanup().await;
                tracing::error!(
                    "VM image upload stream failed after {} bytes for '{}': {}",
                    total_bytes,
                    file_name,
                    e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("Failed to read chunk: {}", e) })),
                )
                    .into_response();
            }
        }
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        cleanup().await;
        tracing::error!("VM image upload sync failed for '{}': {}", file_name, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to sync file: {}", e) })),
        )
            .into_response();
    }

    let elapsed = start.elapsed();
    let size_mib = total_bytes as f64 / (1024.0 * 1024.0);
    let rate_mibs = if elapsed.as_secs_f64() > 0.0 {
        size_mib / elapsed.as_secs_f64()
    } else {
        0.0
    };
    info!(
        "User '{}' uploaded VM image: '{}' ({:.1} MiB in {:.1}s, {:.1} MiB/s)",
        session.username,
        file_name,
        size_mib,
        elapsed.as_secs_f64(),
        rate_mibs
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": file_name,
            "path": dest_path.to_string_lossy(),
            "filesystem": fs_name,
        })),
    )
        .into_response()
}

// ── File Browser endpoints ──────────────────────────────────────

const FILES_ROOT: &str = "/fs";
const BLOCK_FILE_NAME: &str = "vol.img";

/// Check if any ancestor (or the path itself) is a block subvolume directory
/// (contains vol.img). Protects block device backing files from accidental
/// deletion or overwrites via the file browser.
fn is_inside_block_subvolume(path: &std::path::Path) -> bool {
    let mut p = path;
    loop {
        if p.join(BLOCK_FILE_NAME).exists() {
            return true;
        }
        match p.parent() {
            Some(parent)
                if parent.starts_with(FILES_ROOT) && parent != std::path::Path::new(FILES_ROOT) =>
            {
                p = parent;
            }
            _ => break,
        }
    }
    false
}

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
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let token = match token {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Missing token"})),
            )
                .into_response();
        }
    };
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    if state.auth.validate(&token, client_ip).await.is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid token"})),
        )
            .into_response();
    }

    let req_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match safe_path(req_path) {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    let meta = match tokio::fs::metadata(&dir).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Not found"})),
            )
                .into_response();
        }
    };

    if !meta.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a directory"})),
        )
            .into_response();
    }

    let mut entries = Vec::new();
    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().await.ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .as_ref()
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
            a["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    let display_path = dir
        .strip_prefix(FILES_ROOT)
        .unwrap_or(&dir)
        .to_string_lossy()
        .to_string();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "path": display_path,
            "entries": entries,
        })),
    )
        .into_response()
}

/// Validate bearer token from request headers. Returns client_ip on success.
async fn validate_bearer(
    headers: &axum::http::HeaderMap,
    auth: &AuthService,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let token = match token {
        Some(t) => t,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Missing token"})),
            ));
        }
    };
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    if auth.validate(&token, &client_ip).await.is_err() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid token"})),
        ));
    }
    Ok(client_ip)
}

/// Lightweight auth check.  GET /api/auth/check
/// Returns 200 if the bearer token is valid, 401 otherwise.
async fn auth_check_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match validate_bearer(&headers, &state.auth).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => e.into_response(),
    }
}

/// Delete a file or directory.  DELETE /api/files?path=first/subdir/file.txt
async fn files_delete_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = validate_bearer(&headers, &state.auth).await {
        return e.into_response();
    }

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    let target = match safe_path(req_path) {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    // Refuse to delete filesystem/subvolume roots (depth 1 under /fs, e.g. /fs/mypool)
    let rel = target.strip_prefix(FILES_ROOT).unwrap_or(&target);
    if rel.components().count() <= 1 {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Cannot delete filesystem root directories — use the Subvolumes page"}))).into_response();
    }

    // Protect block subvolume backing files (vol.img and anything in the subvolume dir)
    if is_inside_block_subvolume(&target) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Cannot modify block subvolume contents — manage via the Subvolumes page"}))).into_response();
    }

    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Not found"})),
            )
                .into_response();
        }
    };

    let result = if meta.is_dir() {
        tokio::fs::remove_dir_all(&target).await
    } else {
        tokio::fs::remove_file(&target).await
    };

    match result {
        Ok(()) => {
            info!("Deleted {}", target.display());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Upload a file to a directory.  POST /api/files/upload?path=first/subdir
async fn files_upload_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Err(e) = validate_bearer(&headers, &state.auth).await {
        return e.into_response();
    }

    let req_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match safe_path(req_path) {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    if !dir.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Target is not a directory"})),
        )
            .into_response();
    }

    // Protect block subvolume directories
    if is_inside_block_subvolume(&dir) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Cannot upload into block subvolume — manage via the Subvolumes page"}))).into_response();
    }

    // Read multipart field
    let field = match multipart.next_field().await {
        Ok(Some(f)) => f,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "No file in request"})),
            )
                .into_response();
        }
    };

    let file_name = field.file_name().unwrap_or("upload").to_string();
    // Strip path components to prevent traversal via filename
    let file_name = std::path::Path::new(&file_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload")
        .to_string();

    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid filename"})),
        )
            .into_response();
    }

    let dest = dir.join(&file_name);

    let mut file = match tokio::fs::File::create(&dest).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let mut total: u64 = 0;
    let t0 = std::time::Instant::now();
    let mut field = field;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                total += chunk.len() as u64;
                if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await {
                    let _ = tokio::fs::remove_file(&dest).await;
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(&dest).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    }

    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&dest).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }

    let elapsed = t0.elapsed();
    let speed_mb = (total as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    info!(
        "Uploaded {} ({} bytes, {:.1} MB/s)",
        file_name, total, speed_mb
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": file_name,
            "path": dest.to_string_lossy(),
            "size": total,
        })),
    )
        .into_response()
}

/// Create a directory.  POST /api/files/mkdir?path=first/subdir/newdir
async fn files_mkdir_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = validate_bearer(&headers, &state.auth).await {
        return e.into_response();
    }

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    // Validate parent is under /fs
    let parent = match req_path.rsplit_once('/') {
        Some((p, _)) => p,
        None => "",
    };
    if safe_path(parent.is_empty().then_some("").unwrap_or(parent)).is_err() && !parent.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Invalid path"})),
        )
            .into_response();
    }

    let full = std::path::Path::new(FILES_ROOT).join(req_path.trim_start_matches('/'));

    // Protect block subvolume directories
    if is_inside_block_subvolume(&full) || is_inside_block_subvolume(full.parent().unwrap_or(&full))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot create directories inside block subvolumes"})),
        )
            .into_response();
    }

    if full.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Already exists"})),
        )
            .into_response();
    }

    match tokio::fs::create_dir(&full).await {
        Ok(()) => {
            info!("Created directory {}", full.display());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── File content/download endpoint ─────────────────────────────

/// Serve file content with appropriate Content-Type for browser preview.
/// GET /api/files/content?path=first/photos/image.jpg
/// Accepts auth via Authorization header or ?token= query param (for media tags).
async fn files_content_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // Accept token from query param (for <img>/<video>/<audio> tags) or header
    let token = params
        .get("token")
        .cloned()
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|s| s.to_string())
        });
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Missing token"})),
            )
                .into_response();
        }
    };
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    if state.auth.validate(&token, client_ip).await.is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid token"})),
        )
            .into_response();
    }

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    let target = match safe_path(req_path) {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    // Don't serve directories or block subvolume contents
    if target.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Cannot serve directory"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&target) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot access block subvolume contents"})),
        )
            .into_response();
    }

    // Determine content type from extension
    let content_type = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| match ext.to_lowercase().as_str() {
            // Images
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            "bmp" => "image/bmp",
            "avif" => "image/avif",
            // Video
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "ogv" => "video/ogg",
            "mkv" => "video/x-matroska",
            "avi" => "video/x-msvideo",
            "mov" => "video/quicktime",
            // Audio
            "mp3" => "audio/mpeg",
            "ogg" | "oga" => "audio/ogg",
            "wav" => "audio/wav",
            "flac" => "audio/flac",
            "aac" | "m4a" => "audio/mp4",
            "wma" => "audio/x-ms-wma",
            "opus" => "audio/opus",
            // Documents
            "pdf" => "application/pdf",
            // Text
            "txt" | "log" | "md" | "csv" | "conf" | "cfg" | "ini" | "yml" | "yaml"
            | "toml" | "json" | "xml" | "html" | "htm" | "css" | "js" | "ts"
            | "rs" | "py" | "sh" | "bash" | "nix" | "c" | "h" | "cpp" | "go"
            | "java" | "rb" | "php" | "sql" | "dockerfile" => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        })
        .unwrap_or("application/octet-stream");

    // Stream the file
    let file = match tokio::fs::File::open(&target).await {
        Ok(f) => f,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "File not found"})),
            )
                .into_response();
        }
    };

    let metadata = file.metadata().await.ok();
    let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        content_type.parse().unwrap(),
    );
    headers.insert(
        axum::http::header::CONTENT_LENGTH,
        file_size.to_string().parse().unwrap(),
    );
    // Inline display for previewable types, attachment for downloads
    let disposition = if content_type.starts_with("image/")
        || content_type.starts_with("video/")
        || content_type.starts_with("audio/")
        || content_type == "application/pdf"
        || content_type.starts_with("text/")
    {
        format!("inline; filename=\"{file_name}\"")
    } else {
        format!("attachment; filename=\"{file_name}\"")
    };
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        disposition.parse().unwrap(),
    );

    (StatusCode::OK, headers, body).into_response()
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
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    match state
        .auth
        .login(&req.username, &req.password, client_ip)
        .await
    {
        Ok(token) => {
            info!(
                "Login successful: user '{}' from {}",
                req.username, client_ip
            );
            (StatusCode::OK, Json(serde_json::json!({ "token": token }))).into_response()
        }
        Err(_) => {
            tracing::warn!("Login failed: user '{}' from {}", req.username, client_ip);
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid credentials" })),
            )
                .into_response()
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
async fn wait_for_auth(
    socket: &mut WebSocket,
    state: &AppState,
    client_ip: &str,
) -> Option<Session> {
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

// ── Background Alert Notifier ──────────────────────────────────

fn spawn_alert_notifier(state: Arc<AppState>) {
    tokio::spawn(async move {
        use std::collections::HashSet;
        use nasty_system::notifications;
        use nasty_system::alerts::ActiveAlert;

        let mut previously_active: HashSet<(String, String)> = HashSet::new();

        // Wait for system to fully start before first evaluation
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            // Use the cached alerts from the RPC handler (populated by WebUI polling).
            // This avoids duplicating the complex evaluation logic.
            let active: Vec<ActiveAlert> = {
                let cache = state.alerts_cache.lock().await;
                match &*cache {
                    Some((ts, cached)) if ts.elapsed() < std::time::Duration::from_secs(120) => {
                        serde_json::from_value(cached.clone()).unwrap_or_default()
                    }
                    _ => continue, // No fresh alert data — skip this cycle
                }
            };

            // Find newly fired alerts (not previously active)
            let current_keys: HashSet<(String, String)> = active.iter()
                .map(|a| (a.rule_id.clone(), a.source.clone()))
                .collect();

            let new_alerts: Vec<_> = active.iter()
                .filter(|a| !previously_active.contains(&(a.rule_id.clone(), a.source.clone())))
                .collect();

            if !new_alerts.is_empty() {
                let config = notifications::NotificationConfig::load();
                if config.channels.iter().any(|ch| ch.enabled) {
                    for alert in &new_alerts {
                        let sev = match alert.severity {
                            nasty_system::alerts::AlertSeverity::Warning => "WARNING",
                            nasty_system::alerts::AlertSeverity::Critical => "CRITICAL",
                        };
                        let subject = format!("[NASty {sev}] {}", alert.rule_name);
                        let body = format!("{}\n\nSource: {}\nValue: {:.1}\nThreshold: {:.1}",
                            alert.message, alert.source, alert.current_value, alert.threshold);
                        notifications::send(&config, &subject, &body).await;
                    }
                }
            }

            previously_active = current_keys;
        }
    });
}
