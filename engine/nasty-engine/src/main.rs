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
use tracing::{error, info};
use tracing_subscriber::{prelude::*, reload};

mod app_deploy;
mod auth;
mod auth_oidc;
mod fs_dependents;
mod fs_lock;
mod log_stream;
mod router;
mod telemetry;
mod terminal;
mod vm_console;
mod vm_disk_import;

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
    pub oidc: auth_oidc::OidcHolder,
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
    /// Filesystems that failed to mount on startup (persistent alert source).
    pub mount_failures: tokio::sync::Mutex<Vec<String>>,
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
        oidc: auth_oidc::OidcHolder::default(),
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
        mount_failures: tokio::sync::Mutex::new(Vec::new()),
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
    let mount_failures = state.filesystems.restore_mounts().await;
    if !mount_failures.is_empty() {
        error!(
            "CRITICAL: {} filesystem(s) failed to mount: {}",
            mount_failures.len(),
            mount_failures.join(", ")
        );
        *state.mount_failures.lock().await = mount_failures;
    }
    // Re-attach loop devices and get the current name→device mapping.
    // Loop device numbers change across reboots, so NVMe-oF and iSCSI state
    // files must be patched before their respective restore steps run.
    let dev_map = state.subvolumes.restore_block_devices().await;
    if !dev_map.is_empty() {
        state.nvmeof.remap_device_paths(&dev_map).await;
        state.iscsi.remap_device_paths(&dev_map).await;
    }
    state.protocols.restore().await;

    // SSH password auth is managed via /var/lib/nasty/sshd_override.conf
    // (created by tmpfiles with default "yes", toggled by the WebUI).

    state.nvmeof.restore().await;
    state.vms.restore().await;
    state.apps.restore().await;
    state.tailscale.restore().await;

    // If the engine was killed mid-apply (or restarted before the user
    // confirmed a risky network change), restore the prior config from
    // /var/lib/nasty/networking.json.pending-revert. No-op if the file
    // doesn't exist. Runs before the HTTP server starts accepting calls
    // so a confirm can't race the rollback.
    state.network.restore_pending_revert().await;

    // One-shot migration from the pre-cutover legacy networking stack
    // to NetworkManager (phase 3b-beta). Idempotent — gated on a
    // marker file. Runs after restore_pending_revert so any in-flight
    // rollback from before the upgrade is settled first. Best-effort:
    // skipped if NM isn't reachable yet, retried next boot.
    //
    // Runs BEFORE firewall.init: migration may prune orphaned
    // interfaces[] entries (issue #96) and strip dangling references
    // to them from firewall-restrictions.json. Initializing the
    // firewall after migration means the in-memory restrictions
    // mirror the cleaned-on-disk state, so the next user edit
    // doesn't accidentally re-persist the orphans.
    state.network.run_migration_if_needed().await;

    // Backfill project quota IDs on filesystem subvolumes that
    // predate the always-assign change (#176). Without this, those
    // subvolumes have no repquota row, so their `used_bytes` stays
    // `None` and the WebUI shows `—` forever. Idempotent: scans
    // repquota output and only writes for subvolumes that lack a
    // row. Best-effort; failures are logged and don't block startup.
    state.subvolumes.reconcile_project_ids().await;

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

    // Sync NVMe-oF ports with Tailscale IP (if Tailscale reconnected on boot)
    {
        let ts_status = state.tailscale.get().await;
        if ts_status.connected
            && let Some(ref ip) = ts_status.ip
        {
            state.nvmeof.ensure_tailscale_ports(ip).await;
        }
    }

    // Pre-warm caches so first page loads are fast.
    // Runs before sd_notify_ready() — nginx won't serve until this completes.
    info!("Warming caches...");
    let t0 = std::time::Instant::now();
    state.system.info().await;
    info!("Caches warm in {}ms", t0.elapsed().as_millis());

    // Check ACME cert renewal on startup and daily thereafter
    tokio::spawn(async {
        nasty_system::settings::check_acme_renewal().await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        interval.tick().await; // skip first immediate tick
        loop {
            interval.tick().await;
            nasty_system::settings::check_acme_renewal().await;
        }
    });

    // Build the OIDC client if SSO is configured. Failures are logged, not
    // fatal — a misconfigured IdP shouldn't block the engine from starting,
    // and admins can fix the config via the WebUI.
    {
        let oidc_settings = state.settings.get().await.oidc;
        if oidc_settings.enabled {
            if let Err(e) = state.oidc.rebuild(&oidc_settings).await {
                tracing::warn!("OIDC client init failed at startup: {e}");
            } else {
                info!(
                    "OIDC client initialized (issuer={:?})",
                    oidc_settings.issuer_url
                );
            }
        }
    }

    // Start daily anonymous telemetry (if not opted out)
    telemetry::spawn_daily(state.clone());

    // Background alert evaluation + notifications
    spawn_alert_notifier(state.clone());

    // Signal systemd that startup is complete
    sd_notify_ready();

    let ws_routes = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/ws/apps/deploy", get(app_deploy::deploy_handler))
        .route(
            "/ws/vm/disk-import",
            get(vm_disk_import::disk_import_handler),
        )
        .route("/ws/system/logs", get(log_stream::logs_handler))
        .route("/ws/vm/{vm_id}/vnc", get(vm_console::vnc_handler))
        .route("/ws/vm/{vm_id}/serial", get(vm_console::serial_handler))
        .layer(axum::middleware::from_fn(ws_origin_check));

    let app = Router::new()
        .merge(ws_routes)
        .route("/api/login", post(login_handler))
        .route("/api/logout", post(logout_handler))
        .route("/api/auth/oidc/available", get(oidc_available_handler))
        .route("/api/auth/oidc/start", get(oidc_start_handler))
        .route("/api/auth/oidc/callback", get(oidc_callback_handler))
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
        .route("/api/files/rename", post(files_rename_handler))
        .route(
            "/api/files/content",
            get(files_content_handler)
                // 10 MiB cap on edit-in-place writes. The Files page surfaces an
                // edit affordance only for textual files (conf, yml, md, …) where
                // hand-editing past a megabyte is already a smell; using upload
                // for bigger blobs keeps the small fast-path small.
                .put(files_content_put_handler)
                .layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route("/api/auth/check", get(auth_check_handler))
        .route("/health", get(health))
        .with_state(state);

    // 127.0.0.1 only — nginx proxies https://nas:443/ → http://127.0.0.1:2137/.
    // Direct LAN access to the engine port would bypass TLS, the security
    // headers, and the nginx-only X-Real-IP plumbing the session-IP-binding
    // depends on.
    let addr = SocketAddr::from(([127, 0, 0, 1], 2137));
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

    // Authenticate — accepts the session cookie or a Bearer token.
    let token = match token_from_headers(&headers) {
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
        let path = format!("{mp}/vms/images");
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

    // Validate the filename through the central classifier so plain
    // and compressed shapes (e.g. .qcow2.xz, .img.bz2) are all
    // accepted via the same allowlist the importer and lister use.
    if vm_disk_import::classify_vm_image(&file_name).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "Invalid file type. Supported: {}",
                    vm_disk_import::supported_image_extensions_hint()
                )
            })),
        )
            .into_response();
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
    // Auth check — accepts session cookie or Bearer token.
    let token = match token_from_headers(&headers) {
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
/// Name of the httpOnly session cookie set by /api/login and /api/auth/oidc/callback.
const SESSION_COOKIE: &str = "nasty_session";

/// Pull the session token from (in priority order):
///   1. The httpOnly `nasty_session` cookie set by /api/login (browser flow).
///   2. The `Authorization: Bearer ...` header (CLI / kubectl / CSI clients).
///   3. A `?token=...` query parameter (only consulted by routes that accept it,
///      e.g. noVNC and the file-content fallback — handled at the call site).
pub(crate) fn token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(t) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_session_cookie)
    {
        return Some(t);
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Parse a Cookie header value (e.g. "a=1; nasty_session=xyz; b=2") and pull
/// out the session cookie if present. Quietly returns None on any malformed
/// segment rather than throwing — that matches how cookie parsers in stdlib
/// implementations behave.
fn parse_session_cookie(header: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        let (name, value) = match part.split_once('=') {
            Some(t) => t,
            None => continue,
        };
        if name == SESSION_COOKIE && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

async fn validate_bearer(
    headers: &axum::http::HeaderMap,
    auth: &AuthService,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let token = match token_from_headers(headers) {
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
    if safe_path(if parent.is_empty() { "" } else { parent }).is_err() && !parent.is_empty() {
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

#[derive(serde::Deserialize)]
struct RenameRequest {
    /// Existing path under /fs. Resolved to a canonical path; the
    /// rename is rejected if it falls outside FILES_ROOT or targets a
    /// filesystem root (the first path component under /fs).
    from: String,
    /// New path. Doesn't have to exist yet — the parent must, and the
    /// destination itself must not already be there (we never silently
    /// overwrite). Cross-directory renames are allowed as long as both
    /// ends live under the same filesystem (kernel `rename(2)` will
    /// return EXDEV otherwise; we surface that as a 409 with a hint).
    to: String,
}

/// Rename or move a file/directory.  POST /api/files/rename
/// Body: { from: "first/foo.txt", to: "first/bar.txt" }
async fn files_rename_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<RenameRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate_bearer(&headers, &state.auth).await {
        return e.into_response();
    }

    // Source must already exist and resolve under /fs.
    let from = match safe_path(&req.from) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(serde_json::json!({"error": "Invalid source path"})),
            )
                .into_response();
        }
    };
    // Refuse to move filesystem/subvolume roots (depth 1 under /fs).
    let from_rel = from.strip_prefix(FILES_ROOT).unwrap_or(&from);
    if from_rel.components().count() <= 1 {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot rename filesystem root directories — use the Subvolumes page"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&from) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot rename block subvolume contents — manage via the Subvolumes page"})),
        )
            .into_response();
    }

    // Destination: parent must resolve under /fs and not already exist.
    // We don't canonicalize the destination itself (it doesn't exist
    // yet); we canonicalize the parent and join the leaf name back on
    // so traversal in the leaf is impossible.
    let clean_to = req.to.replace("\\", "/");
    let trimmed_to = clean_to.trim_start_matches('/');
    let (parent_req, leaf) = match trimmed_to.rsplit_once('/') {
        Some((p, l)) => (p, l),
        None => {
            // Renaming to a bare leaf under /fs would land at /fs/<leaf>
            // which is a filesystem root — refuse.
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Destination must include a filesystem path component"})),
            )
                .into_response();
        }
    };
    if leaf.is_empty() || leaf == "." || leaf == ".." || leaf.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid destination name"})),
        )
            .into_response();
    }
    let parent = match safe_path(parent_req) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(serde_json::json!({"error": "Invalid destination parent"})),
            )
                .into_response();
        }
    };
    if is_inside_block_subvolume(&parent) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot move into block subvolume contents"})),
        )
            .into_response();
    }
    let to = parent.join(leaf);
    if !to.starts_with(FILES_ROOT) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Invalid destination path"})),
        )
            .into_response();
    }
    if to == from {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "noop": true})),
        )
            .into_response();
    }
    if to.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Destination already exists"})),
        )
            .into_response();
    }

    match tokio::fs::rename(&from, &to).await {
        Ok(()) => {
            info!("Renamed {} -> {}", from.display(), to.display());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        // EXDEV (18) — cross-device rename. Reachable when source and
        // destination live on different bcachefs filesystems mounted
        // under /fs. `rename(2)` is atomic but inherently single-fs,
        // so we surface a clear message rather than swallowing it as
        // a generic 500.
        Err(e) if e.raw_os_error() == Some(18) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Cross-filesystem rename not supported — use copy + delete"})),
        )
            .into_response(),
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
///
/// Auth is via the session cookie (same-origin browsers — `<img>` / `<iframe>`
/// send it automatically) or `Authorization: Bearer` (CLI tools).
async fn files_content_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let token = match token_from_headers(&headers) {
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
            "txt" | "log" | "md" | "csv" | "conf" | "cfg" | "ini" | "yml" | "yaml" | "toml"
            | "json" | "xml" | "html" | "htm" | "css" | "js" | "ts" | "rs" | "py" | "sh"
            | "bash" | "nix" | "c" | "h" | "cpp" | "go" | "java" | "rb" | "php" | "sql"
            | "dockerfile" => "text/plain; charset=utf-8",
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

/// Overwrite a file with new content. PUT /api/files/content?path=…
///
/// Used by the in-page text editor (config files, YAML, scripts).
/// Body is the raw new contents — Content-Type is ignored. The target
/// must already exist as a regular file; writing to a missing path
/// would be the upload endpoint's job, and writing into a directory
/// or block-subvolume backing file is rejected. Body size is capped
/// by the route's `DefaultBodyLimit` (10 MiB) — the in-browser editor
/// isn't where someone should be pasting a gigabyte of logs.
async fn files_content_put_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
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

    // Refuse to edit filesystem roots (no regular-file targets there
    // anyway, but the error message is clearer than "is a directory").
    let rel = target.strip_prefix(FILES_ROOT).unwrap_or(&target);
    if rel.components().count() <= 1 {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot edit filesystem root directories"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&target) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot edit block subvolume contents — manage via the Subvolumes page"})),
        )
            .into_response();
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
    if !meta.is_file() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Target is not a regular file"})),
        )
            .into_response();
    }

    // Write to a sibling temp file and rename. Keeps the original
    // intact if the write fails partway, and means concurrent readers
    // never see a truncated file. The temp name uses the PID to avoid
    // colliding with anything else the engine might create.
    let tmp = target.with_extension(format!("nasty-edit.{}.tmp", std::process::id()));
    if let Err(e) = tokio::fs::write(&tmp, &body).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("write failed: {e}")})),
        )
            .into_response();
    }
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("rename failed: {e}")})),
        )
            .into_response();
    }
    info!("Edited {} ({} bytes)", target.display(), body.len());
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "bytes": body.len()})),
    )
        .into_response()
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
            // Two delivery channels for the same token:
            //   - Set-Cookie for browsers — httpOnly, so XSS can't read it.
            //   - JSON body for CLI clients (kubectl, CSI driver) that don't
            //     have a cookie jar.
            // The token in the body is the same value the cookie carries; both
            // are valid until the session TTL expires.
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert(
                axum::http::header::SET_COOKIE,
                build_session_cookie(&token).parse().unwrap(),
            );
            (
                StatusCode::OK,
                resp_headers,
                Json(serde_json::json!({ "token": token })),
            )
                .into_response()
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

/// Revoke the current session and clear the cookie. Browsers can't remove an
/// httpOnly cookie themselves, so logout has to round-trip to the server.
async fn logout_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut resp_headers = axum::http::HeaderMap::new();
    resp_headers.insert(
        axum::http::header::SET_COOKIE,
        build_session_clear_cookie().parse().unwrap(),
    );
    if let Some(token) = token_from_headers(&headers) {
        // Best-effort revoke; if the token is already invalid we still want
        // the browser to drop the cookie below.
        let _ = state.auth.logout(&token).await;
    }
    (
        StatusCode::OK,
        resp_headers,
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

/// 8h, matches SESSION_TTL_SECS in auth.rs (kept in sync by hand).
const SESSION_COOKIE_MAX_AGE_SECS: u64 = 8 * 3600;

fn build_session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_COOKIE_MAX_AGE_SECS}"
    )
}

fn build_session_clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
}

// ── OIDC SSO ─────────────────────────────────────────────────────

/// Percent-encode a value for placement in a URL fragment.
fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Tells the WebUI whether to render the "Sign in with SSO" button.
/// No auth required — the response only exposes booleans / public config.
async fn oidc_available_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let oidc = state.settings.get().await.oidc;
    let configured = state.oidc.current().await.is_some();
    Json(serde_json::json!({
        "enabled": oidc.enabled && configured,
    }))
}

/// Start an OIDC authorization-code flow. 302s the browser to the IdP.
/// Returns 404 when SSO is disabled so the endpoint doesn't leak its existence.
async fn oidc_start_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(client) = state.oidc.current().await else {
        return (StatusCode::NOT_FOUND, "OIDC not enabled").into_response();
    };
    let url = client.authorize_url().await;
    axum::response::Redirect::to(url.as_str()).into_response()
}

#[derive(Deserialize)]
struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// IdP callback. Validates state + code, exchanges for tokens, mints a NASty
/// session, and 302s the browser to `/#nasty_token=…&oidc=1`. Errors land at
/// `/#oidc_error=<reason>` so the SPA can show a meaningful message.
async fn oidc_callback_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<OidcCallbackQuery>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let bounce = |fragment: String| -> axum::response::Response {
        axum::response::Redirect::to(&format!("/#{fragment}")).into_response()
    };

    if let Some(err) = q.error {
        let detail = q.error_description.unwrap_or_default();
        crate::auth::audit(
            "oidc_login_failed",
            "anonymous",
            &client_ip,
            &format!("{err}: {detail}"),
        );
        return bounce(format!(
            "oidc_error={}",
            url_encode(&format!("{err}: {detail}"))
        ));
    }

    let (Some(code), Some(state_param)) = (q.code, q.state) else {
        crate::auth::audit(
            "oidc_login_failed",
            "anonymous",
            &client_ip,
            "missing code or state",
        );
        return bounce("oidc_error=missing+code+or+state".into());
    };

    let Some(client) = state.oidc.current().await else {
        return (StatusCode::NOT_FOUND, "OIDC not enabled").into_response();
    };

    let identity = match client.exchange_code(&state_param, &code).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("OIDC token exchange failed: {e}");
            crate::auth::audit("oidc_login_failed", "anonymous", &client_ip, &e.to_string());
            return bounce(format!("oidc_error={}", url_encode(&e.to_string())));
        }
    };

    let oidc_settings = state.settings.get().await.oidc;
    let derived_role_str = auth_oidc::role_for_groups(&identity.groups, &oidc_settings);
    let derived_role = derived_role_str
        .as_deref()
        .and_then(crate::auth::parse_role_str);

    match state
        .auth
        .login_or_provision_oidc(
            &identity,
            derived_role,
            oidc_settings.auto_provision,
            &client_ip,
        )
        .await
    {
        // Token is delivered via httpOnly cookie now — never lands in the URL,
        // browser history, or referer header. The fragment is just a flag the
        // SPA reads to know "we just came back from OIDC, refresh state".
        Ok(token) => {
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert(
                axum::http::header::SET_COOKIE,
                build_session_cookie(&token).parse().unwrap(),
            );
            (resp_headers, axum::response::Redirect::to("/#oidc=1")).into_response()
        }
        Err(e) => bounce(format!("oidc_error={}", url_encode(&e.to_string()))),
    }
}

// ── WebSocket with auth ─────────────────────────────────────────

/// Reject WebSocket upgrades whose `Origin` header does not match `Host`.
/// Defends against cross-site WebSocket hijacking: a malicious page in the
/// user's browser cannot open a WS to the appliance and ride existing auth.
///
/// No Origin header → non-browser client (curl, kubectl, CSI driver) → allow.
async fn ws_origin_check(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    let headers = req.headers();
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok());

    if let Some(origin) = origin {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok());
        let origin_authority = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .map(|s| s.split('/').next().unwrap_or(s));
        let allowed = matches!(
            (origin_authority, host),
            (Some(o), Some(h)) if o.eq_ignore_ascii_case(h)
        );
        if !allowed {
            tracing::warn!(
                "WS rejected: Origin '{}' does not match Host '{}'",
                origin,
                host.unwrap_or("")
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(next.run(req).await)
}

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
    // Browsers send the session cookie on the upgrade request automatically;
    // resolve it here so the WS task doesn't have to wait for an auth message.
    // Non-browser clients (kubectl, CSI driver) typically don't have a cookie
    // and still send {"token": "..."} as the first message — handled in
    // handle_socket().
    let pre_auth_token = token_from_headers(&headers);
    ws.on_upgrade(move |socket| handle_socket(socket, state, client_ip, pre_auth_token))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    use futures_util::{SinkExt, StreamExt};
    use nasty_common::Notification;

    info!("WebSocket client connected from {client_ip}, awaiting authentication");

    let session = match resolve_session(&mut socket, &state, &client_ip, pre_auth_token).await {
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

/// Pick the right auth path for a WebSocket connection. If the upgrade
/// request carried a session cookie or Bearer token, validate it directly
/// and acknowledge — that's the browser path now. Otherwise, fall back to
/// waiting for a `{"token": "..."}` message, which is how non-browser
/// clients (kubectl, CSI driver) authenticate.
async fn resolve_session(
    socket: &mut WebSocket,
    state: &AppState,
    client_ip: &str,
    pre_auth_token: Option<String>,
) -> Option<Session> {
    if let Some(token) = pre_auth_token {
        return match state.auth.validate(&token, client_ip).await {
            Ok(session) => {
                let _ = socket
                    .send(Message::Text(
                        serde_json::json!({
                            "authenticated": true,
                            "username": session.username,
                            "role": session.role,
                            "must_change_password": session.must_change_password,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                Some(session)
            }
            Err(_) => {
                let _ = socket
                    .send(Message::Text(r#"{"error":"invalid session"}"#.into()))
                    .await;
                let _ = socket.send(Message::Close(None)).await;
                None
            }
        };
    }
    wait_for_auth(socket, state, client_ip).await
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
        use nasty_system::notifications;
        use std::collections::HashSet;

        let mut previously_active: HashSet<(String, String)> = HashSet::new();

        // Wait for the metrics service and the rest of the system to come up
        // before the first evaluation; first-boot stats are noisy.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            // Evaluate directly. The previous version read state.alerts_cache,
            // which was only populated by the WebUI dashboard polling — meaning
            // the notifier silently skipped every cycle when no admin had a
            // browser open. A drive failing at 3am went unalerted until someone
            // opened the dashboard the next morning.
            let active = crate::router::evaluate_active_alerts(&state).await;

            // Refresh the RPC cache as a side effect so the next WebUI poll
            // returns instantly with up-to-date data.
            if let Ok(value) = serde_json::to_value(&active) {
                *state.alerts_cache.lock().await = Some((std::time::Instant::now(), value));
            }

            // Find newly fired alerts (not previously active)
            let current_keys: HashSet<(String, String)> = active
                .iter()
                .map(|a| (a.rule_id.clone(), a.source.clone()))
                .collect();

            let new_alerts: Vec<_> = active
                .iter()
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
                        let body = format!(
                            "{}\n\nSource: {}\nValue: {:.1}\nThreshold: {:.1}",
                            alert.message, alert.source, alert.current_value, alert.threshold
                        );
                        notifications::send(&config, &subject, &body).await;
                    }
                }
            }

            previously_active = current_keys;
        }
    });
}
