use std::sync::Arc;

use rand::Rng;
use serde::Serialize;
use tokio::time::{Duration, interval};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::AppState;

const TELEMETRY_URL: &str = "https://nasty-telemetry.bartosz-625.workers.dev/api/report";
const TELEMETRY_ID_PATH: &str = "/var/lib/nasty/telemetry-id";
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

#[derive(Serialize)]
struct Report {
    instance_id: String,
    drives: usize,
    total_bytes: u64,
    used_bytes: u64,
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

    Some(Report {
        instance_id: id,
        drives,
        total_bytes,
        used_bytes,
    })
}

/// Send a telemetry report. Returns true on success.
pub async fn send_report(state: &AppState) -> bool {
    let report = match collect_report(state).await {
        Some(r) => r,
        None => return false,
    };

    debug!(
        "Sending telemetry: drives={}, total={}B, used={}B",
        report.drives, report.total_bytes, report.used_bytes
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
    tokio::spawn(async move {
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
}
