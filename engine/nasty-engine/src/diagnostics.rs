//! Admin-only structured performance reports; never includes journal text.
use serde_json::{Value, json};
use std::sync::OnceLock;

pub fn epoch() -> &'static str {
    static EPOCH: OnceLock<String> = OnceLock::new();
    EPOCH.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
pub fn operation(method: &str) -> &'static str {
    operation_names()
        .iter()
        .copied()
        .find(|name| *name == method)
        .unwrap_or("unknown")
}
fn operation_names() -> &'static Vec<&'static str> {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        crate::registry::build_full_registry()
            .1
            .into_iter()
            .flat_map(|(_, methods)| methods.into_iter().map(|m| m.name))
            .collect()
    })
}
fn bucket(value: u64) -> u64 {
    value.checked_next_power_of_two().unwrap_or(u64::MAX)
}
fn numeric_release(text: &str) -> String {
    text.trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .take(32)
        .collect()
}
pub async fn report(state: &crate::AppState) -> Value {
    let (timings, dropped, detailed) = nasty_common::diagnostics::snapshot();
    let boot = state.boot_status.snapshot().await;
    let memory = tokio::fs::read_to_string("/proc/meminfo")
        .await
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("MemTotal:")
                    .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
            })
        })
        .map(bucket);
    let kernel = tokio::fs::read_to_string("/proc/sys/kernel/osrelease")
        .await
        .ok()
        .map(|v| numeric_release(&v));
    let mut pressure = serde_json::Map::new();
    for kind in ["cpu", "memory", "io"] {
        if let Ok(text) = tokio::fs::read_to_string(format!("/proc/pressure/{kind}")).await {
            for line in text.lines() {
                let mut fields = line.split_whitespace();
                if let Some(scope @ ("some" | "full")) = fields.next()
                    && let Some(value) = fields.find_map(|field| {
                        field
                            .strip_prefix("avg10=")
                            .and_then(|v| v.parse::<f64>().ok())
                            .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                    })
                {
                    pressure.insert(format!("{kind}_{scope}_avg10"), json!(value));
                }
            }
        }
    }
    json!({"schema_version": 1, "epoch": epoch(), "engine_version": env!("CARGO_PKG_VERSION"),
        "kernel_version": kernel, "cpu_count_bucket": std::thread::available_parallelism().ok().map(|n| bucket(n.get() as u64)),
        "memory_kib_bucket": memory, "pressure": pressure,
        "detailed_capture": detailed, "dropped_records": dropped, "timings": timings, "allowed_operations": operation_names(),
        "pool_context": nasty_common::diagnostics::pool_context().into_iter().enumerate().map(|(i, (size, devices))| json!({"pool": format!("pool-{}", i + 1), "total_bytes_bucket": size, "device_count_bucket": devices})).collect::<Vec<_>>(),
        "boot_phases": boot.phases.into_iter().map(|p| json!({"operation": p.name, "state": p.state, "duration_ms": p.duration_ms})).collect::<Vec<_>>(),
        "limitations": ["In-memory history since engine start; bounded to 2048 timings.", "Subprocess stages require detailed capture; spawned tasks may not inherit request context.", "Backend duration excludes socket queueing; client minus backend time is not a direct lock/queue measurement.", "No additional filesystem scans, raw logs, pool identities, command arguments, or command output are collected.", "Pool buckets reflect the last ordinary filesystem listing, may be stale, and are limited to 128 pools. Labels do not persist between reports.", "Shutdown/reboot persistence is not yet captured."]})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_not_version_strings_or_operations() {
        assert_eq!(numeric_release("6.18.2-private-host"), "6.18.2");
        assert_eq!(operation("fs.list/private-user"), "unknown");
        assert_eq!(operation("fs.list"), "fs.list");
    }
}
