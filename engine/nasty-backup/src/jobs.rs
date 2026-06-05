//! Background job registry for long-running backup operations.
//!
//! Some backup RPCs — `backup.repo.init`, `backup.repo.check`, and
//! `backup.run` — take long enough on real-world targets (remote
//! restic-rest, large repos, cold S3) that the synchronous RPC
//! pattern doesn't work for them: the WebUI's default 10 s WebSocket
//! timeout fires before the call returns, even when the operation
//! itself succeeds.  Observed: `backup.repo.init` against a remote
//! REST target took 32 s, the WebUI gave up at 10 s and showed
//! "Request timed out" while the engine kept going and successfully
//! initialized the repo.
//!
//! This module provides the alternative: the RPC accepts the request,
//! creates a [`BackupJob`] in [`Pending`], spawns the actual work in a
//! detached `tokio::spawn`, and returns the job handle immediately.
//! The client polls [`JobRegistry::get`] / [`JobRegistry::list`] to
//! watch the state transition `Pending → Running → Succeeded|Failed`.
//!
//! State lives in-memory only.  An engine restart during a job loses
//! the registry entry — but `BackupProfile.last_run` is persisted by
//! the backup path itself, so an operator can still tell whether the
//! actual backup completed by looking at the profile.  Persisting
//! mid-flight job state would invite "job claims Running but the
//! engine process that owned it is gone" inconsistencies that the
//! restart-clears policy sidesteps.
//!
//! [`Pending`]: BackupJobState::Pending

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// What the engine is doing on behalf of the job. The shape of the
/// success [`BackupJob::result`] payload depends on this kind: an
/// `InitRepo` job's success result is a plain message string,
/// a `RunBackup` job's is a `BackupRunResult` JSON object, and
/// `CheckRepo` is the rustic check message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupJobKind {
    InitRepo,
    RunBackup,
    CheckRepo,
}

impl BackupJobKind {
    fn label(self) -> &'static str {
        match self {
            BackupJobKind::InitRepo => "init",
            BackupJobKind::RunBackup => "run",
            BackupJobKind::CheckRepo => "check",
        }
    }
}

/// Lifecycle state of a [`BackupJob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BackupJobState {
    /// Job created, task not yet spawned (essentially transient — the
    /// caller sees this only on the very first response).
    Pending,
    /// Spawned task is running. `started_at` is populated.
    Running,
    /// Task finished without error. `finished_at` is populated and
    /// `result` carries the engine's success payload.
    Succeeded,
    /// Task finished with an error. `finished_at` is populated and
    /// `error` carries the operator-facing message.
    Failed,
}

impl BackupJobState {
    /// Has this job stopped progressing? Used by the GC sweep and by
    /// the "is a job currently active for this profile" check.
    pub fn is_terminal(self) -> bool {
        matches!(self, BackupJobState::Succeeded | BackupJobState::Failed)
    }
}

/// One unit of long-running backup work tracked by the registry.
/// Safe to surface to the WebUI as-is; carries no secrets (the
/// `result` field for `InitRepo` / `CheckRepo` is a status message,
/// for `RunBackup` it's a `BackupRunResult` which itself is already
/// safe to expose).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupJob {
    pub id: String,
    pub profile_id: String,
    pub kind: BackupJobKind,
    pub state: BackupJobState,
    /// RFC3339 timestamp string. Matches the convention used by
    /// `BackupRunResult.timestamp` — schemars doesn't derive
    /// `JsonSchema` for `chrono::DateTime` without an extra feature,
    /// and we'd rather not pull that in just for log-style timestamps.
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Free-form operator-facing message surfaced while the job runs.
    /// Reserved for a future progress-reporting hook (rustic exposes a
    /// callback we don't yet wire); empty in this Phase 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// Engine result payload on success. Shape depends on `kind`:
    /// JSON string for `InitRepo` / `CheckRepo`, `BackupRunResult`
    /// JSON object for `RunBackup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Operator-facing error message on failure. Display-formatted
    /// from the underlying `BackupError`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BackupJob {
    fn new(profile_id: String, kind: BackupJobKind) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            profile_id,
            kind,
            state: BackupJobState::Pending,
            created_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            progress: None,
            result: None,
            error: None,
        }
    }
}

/// Errors surfaced from the registry's start path. Distinct from
/// `BackupError` because the *job* lifecycle has failure modes the
/// underlying backup code doesn't (collision with a running job).
#[derive(Debug, Error)]
pub enum JobError {
    #[error("backup job already running for profile '{0}' (job id: {1})")]
    AlreadyRunning(String, String),
}

/// How long a finished job's entry sticks around in the registry
/// before being garbage-collected. Long enough that the WebUI's
/// polling loop comfortably sees the terminal state and can render
/// the success / failure UI before the entry disappears; short
/// enough that an operator clicking Run repeatedly throughout the
/// day doesn't accumulate thousands of entries.
const JOB_RETENTION: Duration = Duration::hours(1);

/// In-memory store of currently-active and recently-finished backup
/// jobs. Cheap-to-clone Arc so the spawned task can update its own
/// entry without going through the BackupService.
#[derive(Clone, Default)]
pub struct JobRegistry {
    inner: Arc<RwLock<HashMap<String, BackupJob>>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a new Pending job for `profile_id` with `kind`, fail if
    /// another non-terminal job already exists for the same profile.
    /// Returns the freshly-inserted job by value so the caller can
    /// hand it straight back to the RPC client without re-reading
    /// the registry.
    pub async fn start(
        &self,
        profile_id: &str,
        kind: BackupJobKind,
    ) -> Result<BackupJob, JobError> {
        let mut map = self.inner.write().await;
        self.gc_locked(&mut map);

        if let Some(existing) = map
            .values()
            .find(|j| j.profile_id == profile_id && !j.state.is_terminal())
        {
            return Err(JobError::AlreadyRunning(
                profile_id.to_string(),
                existing.id.clone(),
            ));
        }

        let job = BackupJob::new(profile_id.to_string(), kind);
        map.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    /// Transition a job to `Running` and stamp `started_at`. Called
    /// by the spawned task once it's actually picked up. No-op if the
    /// job no longer exists (operator GC'd it manually somehow, or
    /// the entry's been swept).
    pub async fn mark_running(&self, job_id: &str) {
        let mut map = self.inner.write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.state = BackupJobState::Running;
            job.started_at = Some(now_rfc3339());
        }
    }

    /// Transition a job to Succeeded with the given result payload.
    pub async fn mark_succeeded(&self, job_id: &str, result: serde_json::Value) {
        let mut map = self.inner.write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.state = BackupJobState::Succeeded;
            job.finished_at = Some(now_rfc3339());
            job.result = Some(result);
        }
    }

    /// Transition a job to Failed with the given operator-facing
    /// error message. Pre-formatted; the registry doesn't reach into
    /// BackupError so this module stays decoupled from the backup
    /// surface above it.
    pub async fn mark_failed(&self, job_id: &str, error: String) {
        let mut map = self.inner.write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.state = BackupJobState::Failed;
            job.finished_at = Some(now_rfc3339());
            job.error = Some(error);
        }
    }

    pub async fn get(&self, job_id: &str) -> Option<BackupJob> {
        let mut map = self.inner.write().await;
        self.gc_locked(&mut map);
        map.get(job_id).cloned()
    }

    /// List jobs, optionally filtering to a single profile. Newer
    /// jobs first (sorted by `created_at` descending) — matches how
    /// the WebUI wants to render them.
    pub async fn list(&self, profile_id: Option<&str>) -> Vec<BackupJob> {
        let mut map = self.inner.write().await;
        self.gc_locked(&mut map);
        let mut jobs: Vec<BackupJob> = map
            .values()
            .filter(|j| profile_id.is_none_or(|p| j.profile_id == p))
            .cloned()
            .collect();
        // RFC3339 strings sort lexically the same way they sort
        // chronologically — so cmp on the strings gives us
        // newest-first without parsing.
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        jobs
    }

    /// Drop terminal jobs whose `finished_at` is older than
    /// [`JOB_RETENTION`]. Cheap (called under the write lock we
    /// already hold). Called opportunistically from every public
    /// method that touches the map.
    fn gc_locked(&self, map: &mut HashMap<String, BackupJob>) {
        let cutoff = Utc::now() - JOB_RETENTION;
        map.retain(|_, job| {
            if !job.state.is_terminal() {
                return true;
            }
            match job.finished_at.as_deref().and_then(parse_rfc3339) {
                Some(ts) => ts > cutoff,
                // Unparseable finished_at — bug-bait, but the
                // conservative answer is to keep the entry rather
                // than silently dropping it.
                None => true,
            }
        });
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

impl BackupJob {
    /// Helper for the kind-tagged log line each lifecycle stage emits.
    pub fn log_target(&self) -> String {
        format!(
            "backup job {} (profile {}, kind {})",
            self.id,
            self.profile_id,
            self.kind.label()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta_ago_rfc3339(seconds: i64) -> String {
        (Utc::now() - Duration::seconds(seconds)).to_rfc3339()
    }

    #[tokio::test]
    async fn start_returns_pending_job_with_uuid_id() {
        let reg = JobRegistry::new();
        let job = reg
            .start("profile-a", BackupJobKind::InitRepo)
            .await
            .unwrap();
        assert_eq!(job.profile_id, "profile-a");
        assert_eq!(job.kind, BackupJobKind::InitRepo);
        assert_eq!(job.state, BackupJobState::Pending);
        assert!(job.started_at.is_none());
        assert!(job.finished_at.is_none());
        // UUIDs are 36 chars with hyphens — pin the rough shape so a
        // future refactor doesn't silently switch to a shorter id
        // format the WebUI's id-comparison logic can't see.
        assert_eq!(job.id.len(), 36, "id was: {}", job.id);
    }

    #[tokio::test]
    async fn start_rejects_concurrent_jobs_for_same_profile() {
        // rustic locks the repo during init/run/check, so two
        // concurrent jobs against the same profile would either
        // serialize behind that lock (slow, confusing UI) or
        // deadlock outright. Better to refuse at the API boundary
        // and tell the operator a job is already in flight.
        let reg = JobRegistry::new();
        let first = reg.start("p", BackupJobKind::RunBackup).await.unwrap();
        let err = reg.start("p", BackupJobKind::RunBackup).await.unwrap_err();
        match err {
            JobError::AlreadyRunning(profile, job_id) => {
                assert_eq!(profile, "p");
                assert_eq!(job_id, first.id);
            }
        }
    }

    #[tokio::test]
    async fn start_allows_concurrent_jobs_for_different_profiles() {
        let reg = JobRegistry::new();
        let a = reg.start("a", BackupJobKind::InitRepo).await.unwrap();
        let b = reg.start("b", BackupJobKind::RunBackup).await.unwrap();
        assert_ne!(a.id, b.id);
    }

    #[tokio::test]
    async fn start_allows_new_job_after_previous_finished() {
        // Operator clicks Run, it completes, operator clicks Run
        // again — must work. The "one per profile" rule applies to
        // *active* jobs, not terminal ones.
        let reg = JobRegistry::new();
        let first = reg.start("p", BackupJobKind::RunBackup).await.unwrap();
        reg.mark_succeeded(&first.id, serde_json::json!("ok")).await;
        let second = reg.start("p", BackupJobKind::RunBackup).await.unwrap();
        assert_ne!(second.id, first.id);
    }

    #[tokio::test]
    async fn mark_running_then_succeeded_carries_result() {
        let reg = JobRegistry::new();
        let job = reg.start("p", BackupJobKind::RunBackup).await.unwrap();
        reg.mark_running(&job.id).await;
        let mid = reg.get(&job.id).await.unwrap();
        assert_eq!(mid.state, BackupJobState::Running);
        assert!(mid.started_at.is_some());
        assert!(mid.finished_at.is_none());

        reg.mark_succeeded(&job.id, serde_json::json!({"bytes_added": 1024}))
            .await;
        let end = reg.get(&job.id).await.unwrap();
        assert_eq!(end.state, BackupJobState::Succeeded);
        assert!(end.finished_at.is_some());
        assert_eq!(end.result, Some(serde_json::json!({"bytes_added": 1024})));
    }

    #[tokio::test]
    async fn mark_failed_carries_error_message() {
        let reg = JobRegistry::new();
        let job = reg.start("p", BackupJobKind::InitRepo).await.unwrap();
        reg.mark_failed(&job.id, "no such backend".to_string())
            .await;
        let end = reg.get(&job.id).await.unwrap();
        assert_eq!(end.state, BackupJobState::Failed);
        assert_eq!(end.error.as_deref(), Some("no such backend"));
        assert!(end.result.is_none());
    }

    #[tokio::test]
    async fn list_filters_by_profile_and_sorts_newest_first() {
        let reg = JobRegistry::new();
        // Insert in interleaved profile order, time-shift created_at
        // backward on the older ones so sort order is verifiable.
        let a1 = reg.start("a", BackupJobKind::InitRepo).await.unwrap();
        let _b1 = reg.start("b", BackupJobKind::InitRepo).await.unwrap();
        reg.mark_succeeded(&a1.id, serde_json::json!("ok")).await;
        let a2 = reg.start("a", BackupJobKind::RunBackup).await.unwrap();

        let only_a = reg.list(Some("a")).await;
        assert_eq!(only_a.len(), 2);
        assert_eq!(only_a[0].id, a2.id, "newest-first by created_at");
        assert_eq!(only_a[1].id, a1.id);

        let all = reg.list(None).await;
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn gc_drops_terminal_jobs_older_than_retention_window() {
        // Inject an artificially-old finished_at on a terminal job
        // and confirm the next get / list call sweeps it.
        let reg = JobRegistry::new();
        let stale = reg.start("p", BackupJobKind::InitRepo).await.unwrap();
        reg.mark_succeeded(&stale.id, serde_json::json!("ok")).await;
        {
            let mut map = reg.inner.write().await;
            // Two hours ago — well outside JOB_RETENTION.
            if let Some(j) = map.get_mut(&stale.id) {
                j.finished_at = Some(delta_ago_rfc3339(7200));
            }
        }
        // A fresh job for the same profile would collide with the
        // stale one if GC didn't run. start() invokes gc_locked so
        // the stale entry should be gone.
        let fresh = reg.start("p", BackupJobKind::InitRepo).await.unwrap();
        assert_ne!(fresh.id, stale.id);
        assert!(
            reg.get(&stale.id).await.is_none(),
            "stale terminal job should have been GC'd"
        );
    }

    #[tokio::test]
    async fn gc_keeps_non_terminal_jobs_regardless_of_age() {
        // A Pending or Running job is never swept, however long it's
        // been hanging there. The operator may want to see "hey, this
        // has been Running for 6 hours, something's wrong" rather
        // than the entry silently vanishing.
        let reg = JobRegistry::new();
        let job = reg.start("p", BackupJobKind::RunBackup).await.unwrap();
        reg.mark_running(&job.id).await;
        {
            let mut map = reg.inner.write().await;
            // Pretend the job started 6 hours ago.
            if let Some(j) = map.get_mut(&job.id) {
                j.started_at = Some(delta_ago_rfc3339(6 * 3600));
            }
        }
        let _ = reg.list(None).await; // triggers gc
        assert!(reg.get(&job.id).await.is_some());
    }

    #[tokio::test]
    async fn mark_methods_are_noop_on_missing_id() {
        // Racing GC vs. caller — a spawned task might try to update
        // a job that's already been swept. Must not panic.
        let reg = JobRegistry::new();
        reg.mark_running("does-not-exist").await;
        reg.mark_succeeded("does-not-exist", serde_json::json!(null))
            .await;
        reg.mark_failed("does-not-exist", "irrelevant".into()).await;
        assert!(reg.list(None).await.is_empty());
    }
}
