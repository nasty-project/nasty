//! Cron-driven backup scheduler.
//!
//! Reads the `schedule` field on each [`BackupProfile`] and fires
//! `run_backup(id)` when the cron expression's next occurrence has
//! passed since the last attempted run. Designed to be polled from a
//! single tokio task in the engine's main loop — the `cron` crate
//! itself is sync and pure, so all the time + IO concerns live in
//! [`SchedulerTick::evaluate`] / [`run_scheduler_loop`] rather than
//! in the cron-handling helpers below.
//!
//! # Why poll instead of timer-per-profile
//!
//! Per-profile `tokio::time::sleep_until` would be slightly more
//! efficient but creates two problems: (1) profile-list changes
//! (add / remove / re-schedule) require cancelling and respawning
//! tasks, and the race between "operator hits Save" and "old task
//! fires once more" produces confusing duplicate-run behavior;
//! (2) the engine restart cadence is fast enough that any time-of-day
//! drift the polling approach introduces (up to one tick interval)
//! is invisible to operators. A 60 s tick is well under the cron
//! resolution and well under any backup-scheduling expectation.
//!
//! # Missed-run policy
//!
//! On engine start, each profile's `last_attempted_at` is seeded to
//! "now" — meaning a backup that was scheduled to run during engine
//! downtime is NOT caught up. Catch-up runs would risk a thundering
//! herd of overdue jobs after a long outage, plus a backup taken
//! "early" relative to its operator-set time is rarely what was
//! wanted. Operators who explicitly want a missed run can click Run
//! in the WebUI.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cron::Schedule;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{BackupProfile, BackupService};

/// Errors returned by the cron-parsing helpers. Surfaced to operators
/// via `tracing::warn!` rather than failing the engine boot — a
/// malformed schedule on one profile must not block the others.
#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("schedule '{0}' is not a valid cron expression: {1}")]
    InvalidCron(String, String),
}

/// Parse a 5-field (POSIX) or 6-field cron expression. The WebUI
/// emits 5-field expressions (`min hour dom month dow`) for the
/// presets it offers; we transparently promote those to the 6-field
/// form the `cron` crate expects by prepending a `0` seconds column.
/// 6-field expressions pass through unchanged.
pub fn parse_cron(expr: &str) -> Result<Schedule, SchedulerError> {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    let normalized = match field_count {
        5 => format!("0 {trimmed}"),
        6 | 7 => trimmed.to_string(),
        _ => {
            return Err(SchedulerError::InvalidCron(
                expr.to_string(),
                format!("expected 5, 6, or 7 fields; got {field_count}"),
            ));
        }
    };
    Schedule::from_str(&normalized)
        .map_err(|e| SchedulerError::InvalidCron(expr.to_string(), e.to_string()))
}

/// Should the profile fire on this tick? Returns `Some(next_fire)`
/// when the schedule's next occurrence after `last_attempted_at` has
/// passed (i.e. `<= now`), `None` otherwise. Pulled out as a pure
/// function so the firing decision is testable without spawning real
/// backup runs.
///
/// `last_attempted_at` is the floor for the cron lookup, so a profile
/// that has just run won't re-fire on the same tick.
pub fn should_fire(
    schedule: &Schedule,
    last_attempted_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    schedule
        .after(&last_attempted_at)
        .next()
        .filter(|next| *next <= now)
}

/// One iteration of the scheduler loop. Pulled out from
/// [`run_scheduler_loop`] so the firing decision and the in-memory
/// state-machine update are testable in isolation, without spawning
/// tokio tasks or hitting the real `run_backup`.
pub struct SchedulerTick {
    /// Per-profile timestamp of the last cron-driven attempt. Seeded
    /// to `started_at` on engine start; updated whenever
    /// [`should_fire`] returns `Some`. Profiles deleted between ticks
    /// have their entries cleaned up to keep this map bounded.
    pub last_attempted: HashMap<String, DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
}

impl SchedulerTick {
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            last_attempted: HashMap::new(),
            started_at,
        }
    }

    /// Walk `profiles` and return the IDs that should fire on this tick.
    /// Updates `self.last_attempted` for every fired profile and prunes
    /// entries for profiles that have disappeared.
    pub fn evaluate(&mut self, profiles: &[BackupProfile], now: DateTime<Utc>) -> Vec<String> {
        let live_ids: std::collections::HashSet<&str> =
            profiles.iter().map(|p| p.id.as_str()).collect();
        self.last_attempted
            .retain(|id, _| live_ids.contains(id.as_str()));

        let mut to_fire = Vec::new();
        for profile in profiles {
            if !profile.enabled {
                continue;
            }
            let Some(expr) = profile.schedule.as_deref() else {
                continue;
            };
            if expr.trim().is_empty() {
                continue;
            }
            let schedule = match parse_cron(expr) {
                Ok(s) => s,
                Err(e) => {
                    warn!("backup profile '{}' has invalid schedule: {e}", profile.id);
                    continue;
                }
            };
            let last = *self
                .last_attempted
                .entry(profile.id.clone())
                .or_insert(self.started_at);
            if let Some(fire_at) = should_fire(&schedule, last, now) {
                info!(
                    "backup profile '{}' (id {}) firing on cron: scheduled {fire_at}",
                    profile.name, profile.id
                );
                self.last_attempted.insert(profile.id.clone(), now);
                to_fire.push(profile.id.clone());
            } else {
                debug!(
                    "backup profile '{}' (id {}) not due yet",
                    profile.name, profile.id
                );
            }
        }
        to_fire
    }
}

/// Tick interval for the scheduler loop. 60 s is well under any
/// realistic cron resolution and keeps the per-tick log noise low
/// (each tick logs at `debug` per profile). The `cron` crate has
/// per-minute granularity so a faster tick would buy nothing.
pub const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Long-running scheduler loop. Polls the profile list every
/// [`TICK_INTERVAL`], evaluates each enabled profile's schedule, and
/// spawns `run_backup(id)` for every due fire. Each fire is its own
/// `tokio::spawn` so a slow / hung backup on one profile doesn't
/// block the scheduler from advancing the others.
///
/// Never returns. Call once from `main` as a long-lived
/// `tokio::spawn`.
pub async fn run_scheduler_loop(service: BackupService) {
    let mut tick_state = SchedulerTick::new(Utc::now());
    info!("backup scheduler started (tick interval: {TICK_INTERVAL:?})");

    loop {
        tokio::time::sleep(TICK_INTERVAL).await;

        let now = Utc::now();
        // Scheduler only reads id / name / enabled / schedule —
        // never secrets — so the redacted output of list_profiles is
        // fine. run_backup() resolves real secrets on its own.
        let profiles = service.list_profiles().await;
        let due = tick_state.evaluate(&profiles, now);

        for profile_id in due {
            let scheduler_service = service.clone_for_task();
            tokio::spawn(async move {
                match scheduler_service.run_backup(&profile_id).await {
                    Ok(result) if result.success => {
                        info!(
                            "scheduled backup completed for '{profile_id}' in {}s",
                            result.duration_secs
                        );
                    }
                    Ok(result) => {
                        warn!(
                            "scheduled backup for '{profile_id}' finished with error: {}",
                            result.message
                        );
                    }
                    Err(e) => {
                        warn!("scheduled backup for '{profile_id}' failed to start: {e}");
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackupTarget, RetentionPolicy};
    use chrono::TimeZone;

    fn profile_with_schedule(id: &str, cron: &str, enabled: bool) -> BackupProfile {
        BackupProfile {
            id: id.into(),
            name: format!("test-{id}"),
            enabled,
            sources: vec!["/data".into()],
            target: BackupTarget::Local {
                path: "/srv".into(),
            },
            schedule: Some(cron.into()),
            retention: RetentionPolicy::default(),
            password: Some("p".into()),
            password_encrypted: None,
            snapshot_before: true,
            repo_initialized: false,
            last_run: None,
        }
    }

    fn ts(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn parse_cron_accepts_five_field_posix_form() {
        // The WebUI's daily preset emits "0 3 * * *" — must work
        // unmodified or every existing profile silently stops firing.
        let schedule = parse_cron("0 3 * * *").expect("daily cron parses");
        let next = schedule
            .after(&ts(2026, 6, 5, 1, 0))
            .next()
            .expect("daily fires at least once");
        assert_eq!(next, ts(2026, 6, 5, 3, 0));
    }

    #[test]
    fn parse_cron_accepts_six_field_with_seconds() {
        // A hand-written 6-field expression (operator pastes an
        // expression from cronitor.io etc.) should also work — pass
        // through unchanged, don't double-prepend `0`.
        let schedule = parse_cron("0 0 3 * * *").expect("6-field parses");
        assert_eq!(
            schedule.after(&ts(2026, 6, 5, 1, 0)).next(),
            Some(ts(2026, 6, 5, 3, 0))
        );
    }

    #[test]
    fn parse_cron_rejects_garbage() {
        assert!(parse_cron("not a cron").is_err());
        assert!(parse_cron("").is_err());
        // Four fields — neither POSIX cron nor extended.
        assert!(parse_cron("0 3 * *").is_err());
    }

    #[test]
    fn should_fire_returns_some_after_next_occurrence_has_passed() {
        // 03:00 daily, last attempt was yesterday at 03:00, now is
        // 03:05 today — that's exactly the "scheduled run + small
        // tick lag" shape the loop will see in practice.
        let schedule = parse_cron("0 3 * * *").unwrap();
        let last = ts(2026, 6, 4, 3, 0);
        let now = ts(2026, 6, 5, 3, 5);
        assert!(should_fire(&schedule, last, now).is_some());
    }

    #[test]
    fn should_fire_returns_none_when_next_is_in_future() {
        // 03:00 daily, last attempt at 03:00 today, now is 04:00
        // today — the next fire is tomorrow, not today.
        let schedule = parse_cron("0 3 * * *").unwrap();
        let last = ts(2026, 6, 5, 3, 0);
        let now = ts(2026, 6, 5, 4, 0);
        assert!(should_fire(&schedule, last, now).is_none());
    }

    #[test]
    fn evaluate_fires_exactly_once_when_the_window_opens() {
        // Two ticks across the cron occurrence: the first tick fires,
        // the second tick must not (last_attempted advanced).
        // Without this property the scheduler would re-fire every
        // tick within the same minute and stomp on itself.
        let mut tick = SchedulerTick::new(ts(2026, 6, 5, 2, 0));
        let profiles = vec![profile_with_schedule("a", "0 3 * * *", true)];

        // First tick at 03:00 — should fire.
        let fired = tick.evaluate(&profiles, ts(2026, 6, 5, 3, 0));
        assert_eq!(fired, vec!["a".to_string()]);

        // Second tick at 03:01 — already fired this cycle, must not
        // re-fire.
        let fired = tick.evaluate(&profiles, ts(2026, 6, 5, 3, 1));
        assert!(fired.is_empty(), "got unexpected re-fire: {fired:?}");
    }

    #[test]
    fn evaluate_skips_disabled_profile() {
        let mut tick = SchedulerTick::new(ts(2026, 6, 5, 2, 0));
        let profiles = vec![profile_with_schedule("disabled", "0 3 * * *", false)];
        let fired = tick.evaluate(&profiles, ts(2026, 6, 5, 3, 0));
        assert!(fired.is_empty());
    }

    #[test]
    fn evaluate_skips_profile_with_no_schedule() {
        let mut tick = SchedulerTick::new(ts(2026, 6, 5, 2, 0));
        let mut profile = profile_with_schedule("manual", "", true);
        profile.schedule = None;
        let fired = tick.evaluate(&[profile], ts(2026, 6, 5, 3, 0));
        assert!(fired.is_empty());
    }

    #[test]
    fn evaluate_skips_profile_with_empty_schedule_string() {
        // Edge case: WebUI saves "" when operator picks "Manual only"
        // rather than `None`. Both should be treated identically.
        let mut tick = SchedulerTick::new(ts(2026, 6, 5, 2, 0));
        let fired = tick.evaluate(
            &[profile_with_schedule("manual", "   ", true)],
            ts(2026, 6, 5, 3, 0),
        );
        assert!(fired.is_empty());
    }

    #[test]
    fn evaluate_warns_and_skips_on_invalid_cron() {
        // Operator pastes a malformed expression. The scheduler must
        // not crash, must not fire anything for that profile, and
        // must still evaluate the other profiles in the same tick.
        let mut tick = SchedulerTick::new(ts(2026, 6, 5, 2, 0));
        let profiles = vec![
            profile_with_schedule("bad", "not a cron", true),
            profile_with_schedule("good", "0 3 * * *", true),
        ];
        let fired = tick.evaluate(&profiles, ts(2026, 6, 5, 3, 0));
        assert_eq!(fired, vec!["good".to_string()]);
    }

    #[test]
    fn evaluate_does_not_catch_up_runs_missed_during_engine_downtime() {
        // started_at is hours after the cron's intended fire time;
        // the next-fire-after-started_at is tomorrow, NOT today.
        // Without this guard, every engine restart at 09:00 would
        // immediately trigger the 03:00 backup, which is the wrong
        // behavior — operators who want catch-up runs click Run.
        let started_at = ts(2026, 6, 5, 9, 0);
        let mut tick = SchedulerTick::new(started_at);
        let profiles = vec![profile_with_schedule("a", "0 3 * * *", true)];
        let fired = tick.evaluate(&profiles, ts(2026, 6, 5, 9, 0));
        assert!(
            fired.is_empty(),
            "scheduler must not catch up the missed 03:00 run after engine start at 09:00"
        );
    }

    #[test]
    fn evaluate_prunes_state_for_deleted_profiles() {
        // Profile gets deleted between ticks — the last_attempted
        // map entry must be cleaned up so it doesn't leak memory
        // for the engine's lifetime if profiles churn.
        let mut tick = SchedulerTick::new(ts(2026, 6, 5, 2, 0));
        let _ = tick.evaluate(
            &[profile_with_schedule("a", "0 3 * * *", true)],
            ts(2026, 6, 5, 3, 0),
        );
        assert!(tick.last_attempted.contains_key("a"));
        let _ = tick.evaluate(&[], ts(2026, 6, 5, 3, 1));
        assert!(
            tick.last_attempted.is_empty(),
            "stale profile state not pruned: {:?}",
            tick.last_attempted
        );
    }
}
