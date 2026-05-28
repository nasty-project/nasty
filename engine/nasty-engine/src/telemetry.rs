use std::sync::Arc;

use rand::RngExt;
use serde::Serialize;
use tokio::time::{Duration, interval};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::AppState;

const TELEMETRY_URL: &str = "https://nasty-telemetry.nasty-project.workers.dev/api/report";
const TELEMETRY_ID_PATH: &str = "/var/lib/nasty/telemetry-id";
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

#[derive(Serialize)]
struct Report {
    instance_id: String,
    drives: usize,
    total_bytes: u64,
    used_bytes: u64,
    version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    vms: usize,
    apps: usize,
    arch: &'static str,
}

/// Short git SHA this engine was built from. `None` for dev cargo
/// builds outside Nix where `NASTY_GIT_SHA` wasn't injected. Matches
/// the 7-char form used elsewhere (see `nasty-system::update`).
fn build_commit() -> Option<String> {
    let raw = option_env!("NASTY_GIT_SHA")?.trim();
    if raw.is_empty() || raw == "unknown" {
        return None;
    }
    Some(raw[..7.min(raw.len())].to_string())
}

/// Get or create the persistent instance ID.
async fn instance_id() -> Option<String> {
    if let Ok(id) = tokio::fs::read_to_string(TELEMETRY_ID_PATH).await {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }

    let id = Uuid::new_v4().to_string();
    if let Err(e) = tokio::fs::write(TELEMETRY_ID_PATH, &id).await {
        warn!("Failed to write telemetry ID: {e}");
        return None;
    }
    info!("Generated telemetry instance ID");
    Some(id)
}

/// Collect current stats from mounted bcachefs filesystems.
async fn collect_report(state: &AppState) -> Option<Report> {
    let id = instance_id().await?;

    let filesystems = match state.filesystems.list().await {
        Ok(fs) => fs,
        Err(e) => {
            debug!("Failed to list filesystems for telemetry: {e}");
            return None;
        }
    };
    let mounted: Vec<_> = filesystems.iter().filter(|fs| fs.mounted).collect();

    if mounted.is_empty() {
        debug!("No mounted bcachefs filesystems, skipping telemetry report");
        return None;
    }

    let mut drives: usize = 0;
    let mut total_bytes: u64 = 0;
    let mut used_bytes: u64 = 0;

    for fs in &mounted {
        drives += fs.devices.len();
        total_bytes += fs.total_bytes;
        used_bytes += fs.used_bytes;
    }

    let vms = state.vms.list().await.map(|v| v.len()).unwrap_or(0);
    let apps = state.apps.list().await.map(|a| a.len()).unwrap_or(0);

    Some(Report {
        instance_id: id,
        drives,
        total_bytes,
        used_bytes,
        version: env!("CARGO_PKG_VERSION"),
        commit: build_commit(),
        vms,
        apps,
        arch: std::env::consts::ARCH,
    })
}

/// Send a telemetry report. Returns true on success.
pub async fn send_report(state: &AppState) -> bool {
    if !state.settings.get().await.telemetry_enabled {
        debug!("Telemetry disabled, skipping report");
        return false;
    }

    let report = match collect_report(state).await {
        Some(r) => r,
        None => return false,
    };

    debug!(
        "Sending telemetry: drives={}, total={}B, used={}B, vms={}, apps={}, arch={}, version={}, commit={:?}",
        report.drives,
        report.total_bytes,
        report.used_bytes,
        report.vms,
        report.apps,
        report.arch,
        report.version,
        report.commit
    );

    match state
        .metrics_client
        .post(TELEMETRY_URL)
        .json(&report)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            debug!("Telemetry report sent successfully");
            true
        }
        Ok(resp) => {
            debug!("Telemetry report rejected: {}", resp.status());
            false
        }
        Err(e) => {
            debug!("Telemetry report failed: {e}");
            false
        }
    }
}

/// Spawn the daily telemetry background task.
pub fn spawn_daily(state: Arc<AppState>) {
    let h = tokio::spawn(async move {
        // Random initial delay (0-24h) to spread load across instances
        let jitter = rand::rng().random_range(0..TELEMETRY_INTERVAL.as_secs());
        debug!("Telemetry: first report in {}s", jitter);
        tokio::time::sleep(Duration::from_secs(jitter)).await;

        let mut ticker = interval(TELEMETRY_INTERVAL);
        loop {
            ticker.tick().await;
            send_report(&state).await;
        }
    });
    // Observer spawn — telemetry loop is supposed to run forever; if
    // it exits (cleanly or by panic) we want a single log line so the
    // user can see why telemetry stopped reporting.
    tokio::spawn(async move {
        match h.await {
            Ok(()) => tracing::warn!("telemetry loop exited unexpectedly"),
            Err(e) => tracing::warn!("telemetry loop panicked / cancelled: {e}"),
        }
    });
}
