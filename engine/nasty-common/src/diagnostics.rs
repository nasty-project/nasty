//! Bounded, local performance observations. Never accepts arguments or output.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const LIMIT: usize = 2048;
#[derive(Clone, Serialize)]
pub struct Timing {
    pub sequence: u64,
    pub operation: &'static str,
    pub stage: &'static str,
    pub duration_ms: u64,
    pub outcome: &'static str,
}
struct Recorder {
    next: u64,
    entries: VecDeque<Timing>,
    dropped: u64,
    detailed_until: Option<Instant>,
    pools: Vec<(u64, u64)>,
}
fn recorder() -> &'static Mutex<Recorder> {
    static VALUE: OnceLock<Mutex<Recorder>> = OnceLock::new();
    VALUE.get_or_init(|| {
        Mutex::new(Recorder {
            next: 0,
            entries: VecDeque::new(),
            dropped: 0,
            detailed_until: None,
            pools: Vec::new(),
        })
    })
}
tokio::task_local! { pub static REQUEST: (u64, &'static str); }
pub fn next_sequence() -> u64 {
    let mut r = recorder().lock().unwrap();
    r.next += 1;
    r.next
}
pub fn record(
    sequence: u64,
    operation: &'static str,
    stage: &'static str,
    elapsed: Duration,
    outcome: &'static str,
) {
    let mut r = recorder().lock().unwrap();
    if r.entries.len() == LIMIT {
        r.entries.pop_front();
        r.dropped += 1;
    }
    r.entries.push_back(Timing {
        sequence,
        operation,
        stage,
        duration_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        outcome,
    });
}
pub fn snapshot() -> (Vec<Timing>, u64, bool) {
    let r = recorder().lock().unwrap();
    (
        r.entries.iter().cloned().collect(),
        r.dropped,
        r.detailed_until.is_some_and(|until| Instant::now() < until),
    )
}
pub fn detailed(enabled: bool) {
    recorder().lock().unwrap().detailed_until =
        enabled.then(|| Instant::now() + Duration::from_secs(15 * 60));
}
pub fn clear() {
    let mut r = recorder().lock().unwrap();
    r.entries.clear();
    r.dropped = 0;
    r.pools.clear();
}
pub fn observe_pools(pools: impl Iterator<Item = (u64, u64)>) {
    let mut values: Vec<_> = pools
        .take(128)
        .map(|(size, devices)| {
            (
                if size == 0 {
                    0
                } else {
                    size.checked_next_power_of_two().unwrap_or(u64::MAX)
                },
                if devices == 0 {
                    0
                } else {
                    devices.checked_next_power_of_two().unwrap_or(u64::MAX)
                },
            )
        })
        .collect();
    values.sort_unstable();
    recorder().lock().unwrap().pools = values;
}
pub fn pool_context() -> Vec<(u64, u64)> {
    recorder().lock().unwrap().pools.clone()
}
/// Drop records cancellation too, without changing subprocess cancellation semantics.
pub struct Stage {
    context: Option<(u64, &'static str)>,
    name: &'static str,
    started: Instant,
    outcome: &'static str,
}
impl Stage {
    pub fn new(name: &'static str) -> Self {
        let enabled = recorder()
            .lock()
            .unwrap()
            .detailed_until
            .is_some_and(|until| Instant::now() < until);
        Self {
            context: enabled.then(|| REQUEST.try_with(|v| *v).ok()).flatten(),
            name,
            started: Instant::now(),
            outcome: "cancelled",
        }
    }
    pub fn finish(&mut self, success: bool) {
        self.outcome = if success { "ok" } else { "error" };
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if let Some((sequence, operation)) = self.context {
            record(
                sequence,
                operation,
                self.name,
                self.started.elapsed(),
                self.outcome,
            );
        }
    }
}
/// Do not export arbitrary executable paths/names.
pub fn command_stage(program: &str) -> &'static str {
    match program {
        "bcachefs" => "command.bcachefs",
        "findmnt" => "command.findmnt",
        "lsblk" => "command.lsblk",
        "blkid" => "command.blkid",
        "df" => "command.df",
        "du" => "command.du",
        "systemctl" => "command.systemctl",
        "docker" => "command.docker",
        "mount" => "command.mount",
        "umount" => "command.umount",
        "nasty-maintenance" => "command.maintenance",
        _ => "command.other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_names_are_allowlisted() {
        assert_eq!(command_stage("/fs/private/customer-tool"), "command.other");
        assert_eq!(command_stage("bcachefs"), "command.bcachefs");
    }
    #[tokio::test]
    async fn bounds_and_expiry() {
        clear();
        for _ in 0..LIMIT + 10 {
            record(0, "fs.list", "backend", Duration::ZERO, "ok");
        }
        let (entries, dropped, _) = snapshot();
        assert_eq!(entries.len(), LIMIT);
        assert!(dropped >= 10);
        detailed(true);
        assert!(snapshot().2);
        detailed(false);
        assert!(!snapshot().2);
        recorder().lock().unwrap().detailed_until =
            Instant::now().checked_sub(Duration::from_secs(1));
        assert!(!snapshot().2);
        clear();
        assert!(snapshot().0.is_empty());
        detailed(true);
        let sequence = next_sequence();
        REQUEST
            .scope((sequence, "fs.list"), async {
                let mut stage = Stage::new("filesystem.discovery");
                stage.finish(true);
                drop(stage);
                let _cancelled = Stage::new("command.other");
            })
            .await;
        let entries = snapshot().0;
        assert!(
            entries
                .iter()
                .any(|e| e.sequence == sequence && e.outcome == "ok")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.sequence == sequence && e.outcome == "cancelled")
        );
        detailed(false);
        REQUEST
            .scope((sequence, "fs.list"), async {
                drop(Stage::new("not_collected"));
            })
            .await;
        assert!(!snapshot().0.iter().any(|e| e.stage == "not_collected"));
        clear();
    }
}
