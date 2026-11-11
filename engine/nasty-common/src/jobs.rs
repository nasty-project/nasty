//! In-memory registry of long-running engine operations (#1032).
//!
//! An RPC that would otherwise block for minutes or hours starts a [`Job`]
//! and returns it at once; the work runs in a detached task that records the
//! outcome here. An engine restart clears the registry, and with it the
//! history: the system's own state (e.g. a bcachefs member still evacuating)
//! stays the source of truth for anything that was interrupted.

use std::sync::Mutex;

use schemars::JsonSchema;
use serde::Serialize;

/// Finished jobs are kept this long so the WebUI can show how they ended.
const FINISHED_RETENTION_SECS: i64 = 24 * 60 * 60;
/// And at most this many of them.
const FINISHED_RETENTION_COUNT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    /// The command ran and declined, with its own explanation in `error`.
    Refused,
    /// The command could not run, was killed, or did not do what it reported.
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Job {
    pub id: String,
    /// What the job does, e.g. "device.remove".
    pub kind: String,
    /// Filesystem the job acts on; used for scoping.
    pub filesystem: String,
    /// What within the filesystem it acts on, e.g. a device path.
    pub target: String,
    pub state: JobState,
    /// Unix seconds.
    pub started_at: i64,
    /// Unix seconds; `None` while running.
    pub finished_at: Option<i64>,
    /// Why the job was refused or failed.
    pub error: Option<String>,
    /// Cancellation was requested; the job is still winding down.
    pub cancel_requested: bool,
}

#[derive(Default)]
pub struct JobRegistry {
    jobs: Mutex<Vec<Job>>,
}

impl JobRegistry {
    /// Start a job, or return the one of the same kind already running on
    /// the same target.
    pub fn start(&self, kind: &str, filesystem: &str, target: &str) -> Result<Job, Box<Job>> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(running) = find_running(&jobs, kind, target) {
            return Err(Box::new(running.clone()));
        }
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.to_string(),
            filesystem: filesystem.to_string(),
            target: target.to_string(),
            state: JobState::Running,
            started_at: now(),
            finished_at: None,
            error: None,
            cancel_requested: false,
        };
        jobs.push(job.clone());
        Ok(job)
    }

    /// Record how a job ended. A failure after a cancel request is recorded
    /// as cancelled, since the cancel is what stopped it.
    pub fn finish(&self, id: &str, state: JobState, error: Option<String>) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|j| j.id == id && j.state == JobState::Running)?;
        job.state = match state {
            JobState::Failed if job.cancel_requested => JobState::Cancelled,
            state => state,
        };
        job.finished_at = Some(now());
        job.error = error;
        let finished = job.clone();
        prune(&mut jobs, now());
        Some(finished)
    }

    /// Mark the running job of `kind` on `target` as cancelling.
    pub fn request_cancel(&self, kind: &str, target: &str) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|j| j.kind == kind && j.target == target && j.state == JobState::Running)?;
        job.cancel_requested = true;
        Some(job.clone())
    }

    pub fn running(&self, kind: &str, target: &str) -> Option<Job> {
        find_running(&self.jobs.lock().unwrap(), kind, target).cloned()
    }

    /// Running and recently finished jobs, oldest first.
    pub fn list(&self) -> Vec<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        prune(&mut jobs, now());
        jobs.clone()
    }
}

fn find_running<'a>(jobs: &'a [Job], kind: &str, target: &str) -> Option<&'a Job> {
    jobs.iter()
        .find(|j| j.kind == kind && j.target == target && j.state == JobState::Running)
}

fn prune(jobs: &mut Vec<Job>, now: i64) {
    jobs.retain(|j| {
        j.finished_at
            .is_none_or(|t| now - t < FINISHED_RETENTION_SECS)
    });
    let finished = jobs.iter().filter(|j| j.finished_at.is_some()).count();
    let mut excess = finished.saturating_sub(FINISHED_RETENTION_COUNT);
    // Jobs are pushed in start order, so the oldest finished ones come first.
    jobs.retain(|j| {
        if excess > 0 && j.finished_at.is_some() {
            excess -= 1;
            return false;
        }
        true
    });
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_running_job_per_kind_and_target() {
        let jobs = JobRegistry::default();
        let first = jobs.start("device.remove", "pool", "/dev/sdd").unwrap();
        let again = jobs.start("device.remove", "pool", "/dev/sdd").unwrap_err();
        assert_eq!(again.id, first.id);
        assert!(jobs.start("device.remove", "pool", "/dev/sde").is_ok());

        jobs.finish(&first.id, JobState::Refused, Some("would lose data".into()));
        assert!(jobs.start("device.remove", "pool", "/dev/sdd").is_ok());
    }

    #[test]
    fn failure_after_cancel_is_cancelled() {
        let jobs = JobRegistry::default();
        let job = jobs.start("device.remove", "pool", "/dev/sdd").unwrap();
        assert!(jobs.request_cancel("device.remove", "/dev/sdd").is_some());
        let done = jobs
            .finish(&job.id, JobState::Failed, Some("signal 15".into()))
            .unwrap();
        assert_eq!(done.state, JobState::Cancelled);
        assert!(jobs.finish(&job.id, JobState::Succeeded, None).is_none());
    }

    #[test]
    fn prune_keeps_running_and_recent_finished_jobs() {
        let job = |finished_at| Job {
            id: String::new(),
            kind: "k".into(),
            filesystem: "pool".into(),
            target: "t".into(),
            state: JobState::Succeeded,
            started_at: 0,
            finished_at,
            error: None,
            cancel_requested: false,
        };
        let now = 100_000;
        let mut jobs = vec![job(None), job(Some(now - FINISHED_RETENTION_SECS))];
        jobs.extend((0..25).map(|i| job(Some(now - 100 + i))));
        prune(&mut jobs, now);

        assert_eq!(jobs.len(), 1 + FINISHED_RETENTION_COUNT);
        assert!(jobs[0].finished_at.is_none());
        assert_eq!(jobs[1].finished_at, Some(now - 95));
    }
}
