use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::info;

mod collect_bcachefs;
mod collect_kernel;
mod collect_system;
mod db;
mod prometheus;

use collect_bcachefs::BcachefsMetrics;
use nasty_common::metrics_types::{DiskHealth, KernelErrorSummary, ResourceHistory, SystemStats};

struct AppState {
    db: db::MetricsDb,
    /// Cached latest system stats snapshot (updated every 5s by collector).
    stats: RwLock<SystemStats>,
    /// Cached latest disk health snapshot (updated every 60s by collector).
    disks: RwLock<Vec<DiskHealth>>,
    /// Cached latest bcachefs metrics snapshot.
    bcachefs: RwLock<Vec<BcachefsMetrics>>,
    /// Cached kernel error summary.
    kernel_errors: RwLock<KernelErrorSummary>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nasty_metrics=info".into()),
        )
        .init();

    let metrics_db = db::MetricsDb::open()
        .expect("failed to open metrics database");

    let state = Arc::new(AppState {
        db: metrics_db,
        stats: RwLock::new(collect_system::system_stats()),
        disks: RwLock::new(Vec::new()),
        bcachefs: RwLock::new(Vec::new()),
        kernel_errors: RwLock::new(KernelErrorSummary::default()),
    });

    // Background collectors
    tokio::spawn(system_collector(state.clone()));
    tokio::spawn(disk_collector(state.clone()));
    tokio::spawn(bcachefs_collector(state.clone()));
    tokio::spawn(kernel_error_collector(state.clone()));

    // Signal systemd readiness
    sd_notify_ready();

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/api/stats", get(stats_handler))
        .route("/api/disks", get(disks_handler))
        .route("/api/kernel_errors", get(kernel_errors_handler))
        .route("/api/history", get(history_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 2138));
    info!("nasty-metrics listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Handlers ────────────────────────────────────────────────────

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let stats = state.stats.read().await;
    let disks = state.disks.read().await;
    let bcachefs = state.bcachefs.read().await;

    let body = prometheus::render(&stats, &disks, &bcachefs);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

async fn stats_handler(State(state): State<Arc<AppState>>) -> Json<SystemStats> {
    let stats = state.stats.read().await;
    Json(stats.clone())
}

async fn disks_handler(State(state): State<Arc<AppState>>) -> Json<Vec<DiskHealth>> {
    let disks = state.disks.read().await;
    Json(disks.clone())
}

async fn kernel_errors_handler(State(state): State<Arc<AppState>>) -> Json<KernelErrorSummary> {
    let errors = state.kernel_errors.read().await;
    Json(errors.clone())
}

#[derive(Deserialize)]
struct HistoryQuery {
    kind: Option<String>,
    name: Option<String>,
    range: Option<String>,
    /// Offset in milliseconds into the past (0 = anchored to now).
    offset: Option<i64>,
}

async fn history_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<Vec<ResourceHistory>> {
    let kind = q.kind.as_deref().unwrap_or("net");
    let name = q.name.as_deref();
    let range = q.range.as_deref().unwrap_or("5m");
    let offset = q.offset.unwrap_or(0).max(0);
    Json(state.db.query(kind, name, range, offset))
}

// ── Collectors ──────────────────────────────────────────────────

/// System stats collector: samples every 5s, computes I/O deltas, writes to SQLite.
async fn system_collector(state: Arc<AppState>) {
    let mut prev_stats = collect_system::system_stats();
    let mut prev_time = std::time::Instant::now();
    let mut prune_counter = 0u32;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(prev_time).as_secs_f64();
        if elapsed <= 0.0 {
            continue;
        }

        let stats = collect_system::system_stats();

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
            state.db.insert("net", &net_samples);
        }
        if !disk_samples.is_empty() {
            state.db.insert("disk", &disk_samples);
        }

        // CPU usage percentage
        let cpu_pct = (stats.cpu.load_1 / stats.cpu.count as f64) * 100.0;
        state.db.insert("cpu", &[("cpu", cpu_pct.min(100.0), 0.0)]);

        // Memory usage percentage
        let mem_pct = if stats.memory.total_bytes > 0 {
            (stats.memory.used_bytes as f64 / stats.memory.total_bytes as f64) * 100.0
        } else {
            0.0
        };
        state.db.insert("mem", &[("mem", mem_pct, 0.0)]);

        // Update cached snapshot
        *state.stats.write().await = stats;

        prev_stats = state.stats.read().await.clone();
        prev_time = now;

        // Prune old data every ~5 minutes
        prune_counter += 1;
        if prune_counter >= 60 {
            prune_counter = 0;
            state.db.prune();
        }
    }
}

/// Disk health collector: samples every 60s (smartctl is slow).
async fn disk_collector(state: Arc<AppState>) {
    loop {
        let disks = collect_system::disk_health().await;
        *state.disks.write().await = disks;
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// bcachefs metrics collector: samples every 5s.
async fn bcachefs_collector(state: Arc<AppState>) {
    loop {
        let metrics = tokio::task::spawn_blocking(collect_bcachefs::collect_all)
            .await
            .unwrap_or_default();
        *state.bcachefs.write().await = metrics;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Kernel error collector: scans dmesg every 30s for suspicious messages.
async fn kernel_error_collector(state: Arc<AppState>) {
    let collector = std::sync::Arc::new(std::sync::Mutex::new(
        collect_kernel::KernelErrorCollector::new(),
    ));
    loop {
        let c = collector.clone();
        let summary = tokio::task::spawn_blocking(move || c.lock().unwrap().collect())
            .await
            .unwrap_or_default();
        *state.kernel_errors.write().await = summary;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

// ── systemd notify ──────────────────────────────────────────────

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
