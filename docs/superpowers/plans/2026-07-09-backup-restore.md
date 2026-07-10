# Backup Restore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a whole-snapshot restore operation to the NASty backup system — restore a rustic snapshot to an operator-chosen destination under `/fs`, tracked as a background job with coarse progress, surfaced from the Backups page.

**Architecture:** Mirror the existing job-tracked backup operations (`init`/`run`/`check`). A new `BackupJobKind::Restore` and a `BackupService::start_restore` wrapper spawn a background task that opens the repo, resolves the snapshot's root node, and restores it to a validated destination via rustic_core's `prepare_restore`/`restore`. A custom `ProgressBars` implementation feeds restore-byte progress into a coarse fraction that a poller writes onto the job. The WebUI adds a Restore action on the existing snapshot list, opening a dialog (filesystem + subpath + overwrite checkbox), then reuses the job-polling machinery with a progress bar.

**Tech Stack:** Rust (`nasty-backup`, `nasty-engine`), rustic_core 0.12, tokio, Svelte 5 (`webui`).

## Global Constraints

- Restore destinations MUST canonicalize under `/fs` (bcachefs-managed storage) — same jail shape as `guestshare.rs`'s `FILES_ROOT = "/fs"`. Reject `..` traversal and symlink escape.
- The `/fs/<fs>` filesystem-root of any destination MUST already exist as a directory (a mounted NASty filesystem).
- Restoring into a **non-empty** destination requires `allow_overwrite = true`; otherwise reject before starting the job.
- rustic restore MUST NOT delete extra files (`RestoreOptions.delete = false`) — restore merges, never wipes unrelated data.
- Restore is a tracked `BackupJob`, subject to the same one-non-terminal-job-per-profile guard as the other operations.
- Repo/target secrets are resolved on the async side **before** `spawn_blocking` (rustic construction is sync) — follow the existing `resolve_profile_password` + `profile.resolve_runtime()` pattern.
- Verification discipline before every commit that touches Rust: `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test` (run from `engine/`). For WebUI tasks: `npm run check` and `npm test` (run from `webui/`).
- `webui/src/lib/types.ts` is hand-maintained ("Mirrors engine Rust types") — update it by hand to match Rust changes.

---

## File Structure

- `engine/nasty-backup/src/jobs.rs` — **modify**: add `BackupJobKind::Restore`, a numeric `progress_fraction` field on `BackupJob`, and `JobRegistry::mark_progress`.
- `engine/nasty-backup/src/restore.rs` — **create**: pure/near-pure restore helpers — destination validation (`validate_restore_dest`) and the coarse-progress plumbing (`RestoreProgress`, `RestoreProgressBars`).
- `engine/nasty-backup/src/lib.rs` — **modify**: `mod restore;`, `make_repo_with_progress`, `RestoreSummary`, `BackupService::restore_inner`, `BackupService::start_restore`.
- `engine/nasty-engine/src/router/backup.rs` — **modify**: add the `backup.restore` arm.
- `engine/nasty-engine/src/registry/methods.rs` — **modify**: register `backup.restore`.
- `engine/nasty-engine/src/registry/paths.rs` — **modify (test only)**: assert `backup.restore` translates to POST.
- `webui/src/lib/types.ts` — **modify**: add `'restore'` to `BackupJobKind`, add `progress_fraction` to `BackupJob`.
- `webui/src/routes/backups/+page.svelte` — **modify**: Restore action on snapshot rows, restore dialog, `startRestore`, progress bar in the active-job badge.

---

## Task 1: Restore job kind + numeric progress

**Files:**
- Modify: `engine/nasty-backup/src/jobs.rs`
- Test: `engine/nasty-backup/src/jobs.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `BackupJob`, `BackupJobKind`, `JobRegistry`.
- Produces:
  - `BackupJobKind::Restore` (label `"restore"`).
  - `BackupJob.progress_fraction: Option<f64>` (0.0–1.0, serialized as `progress_fraction`, omitted when `None`).
  - `JobRegistry::mark_progress(&self, job_id: &str, fraction: f64)` — clamps to `[0.0, 1.0]`, sets `progress_fraction` on the live job; no-op if the job is gone.

- [ ] **Step 1: Write the failing test for the new kind's label**

Add to the `tests` module in `engine/nasty-backup/src/jobs.rs`:

```rust
#[test]
fn restore_kind_has_label() {
    assert_eq!(BackupJobKind::Restore.label(), "restore");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p nasty-backup restore_kind_has_label`
Expected: FAIL — `no variant named `Restore``.

- [ ] **Step 3: Add the `Restore` variant and its label**

In the `BackupJobKind` enum:

```rust
pub enum BackupJobKind {
    InitRepo,
    RunBackup,
    CheckRepo,
    Restore,
}
```

In `impl BackupJobKind::label`:

```rust
            BackupJobKind::CheckRepo => "check",
            BackupJobKind::Restore => "restore",
```

Also extend the enum's doc comment above it — after the `CheckRepo` sentence add: `A `Restore` job's success result is a `RestoreSummary` JSON object.`

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p nasty-backup restore_kind_has_label`
Expected: PASS

- [ ] **Step 5: Write the failing test for `mark_progress`**

```rust
#[tokio::test]
async fn mark_progress_sets_clamped_fraction() {
    let reg = JobRegistry::new();
    let job = reg
        .start("profile-p", BackupJobKind::Restore)
        .await
        .unwrap();
    reg.mark_running(&job.id).await;

    reg.mark_progress(&job.id, 0.42).await;
    assert_eq!(reg.get(&job.id).await.unwrap().progress_fraction, Some(0.42));

    // Out-of-range values clamp into [0.0, 1.0].
    reg.mark_progress(&job.id, 1.5).await;
    assert_eq!(reg.get(&job.id).await.unwrap().progress_fraction, Some(1.0));
    reg.mark_progress(&job.id, -0.3).await;
    assert_eq!(reg.get(&job.id).await.unwrap().progress_fraction, Some(0.0));
}
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test -p nasty-backup mark_progress_sets_clamped_fraction`
Expected: FAIL — no field `progress_fraction`, no method `mark_progress`.

- [ ] **Step 7: Add the `progress_fraction` field**

In `struct BackupJob`, after the `progress` field:

```rust
    /// Coarse restore progress as a fraction in `[0.0, 1.0]`. Populated
    /// only by `Restore` jobs (bytes restored / total). `None` until the
    /// first progress tick and for non-restore kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_fraction: Option<f64>,
```

In `BackupJob::new`, add `progress_fraction: None,` alongside the other `None` initializers.

- [ ] **Step 8: Add `mark_progress` to `JobRegistry`**

After `mark_running`:

```rust
    /// Update a job's coarse progress fraction. Clamped to `[0.0, 1.0]`.
    /// No-op if the job no longer exists (GC'd / swept).
    pub async fn mark_progress(&self, job_id: &str, fraction: f64) {
        let mut map = self.inner.write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.progress_fraction = Some(fraction.clamp(0.0, 1.0));
        }
    }
```

- [ ] **Step 9: Run the whole crate's tests**

Run (from `engine/`): `cargo test -p nasty-backup`
Expected: PASS (all, including the two new tests).

- [ ] **Step 10: Commit**

```bash
cd engine && cargo fmt
git add nasty-backup/src/jobs.rs
git commit -m "backup: add Restore job kind and numeric progress_fraction"
```

---

## Task 2: Destination validation (pure)

**Files:**
- Create: `engine/nasty-backup/src/restore.rs`
- Modify: `engine/nasty-backup/src/lib.rs` (add `pub mod restore;` next to the other `pub mod` lines)
- Test: `engine/nasty-backup/src/restore.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum RestoreDestError { NotAbsolute, OutsideRoot, FsRootMissing, NotEmpty, Io(std::io::Error) }` (derives `Debug`, `thiserror::Error` with operator-facing messages).
  - `pub fn validate_restore_dest(dest: &Path, root: &Path, allow_overwrite: bool) -> Result<PathBuf, RestoreDestError>` — returns the absolute destination path to restore into (not necessarily existing yet). The `root` parameter is `/fs` in production and a tempdir in tests.

**Validation rules (all enforced by `validate_restore_dest`):**
1. `dest` must be absolute → else `NotAbsolute`.
2. Resolve the longest existing ancestor of `dest`, canonicalize it, and require the canonical prefix to be `root` or under `root`. Re-append the non-existing tail; the recomposed path must still start with `root`. Canonicalization resolves symlinks, so a symlinked ancestor escaping `root` → `OutsideRoot`.
3. `dest` must be strictly below `root` (it must have at least one path component under `root`, i.e. `/fs/<fs>[/...]`). A `dest` equal to `root` → `OutsideRoot`.
4. The `/fs/<fs>` filesystem-root (first component under `root`) must exist as a directory → else `FsRootMissing`.
5. If `dest` already exists, is a directory, and has at least one entry, and `!allow_overwrite` → `NotEmpty`. (An empty dir, a non-existent path, or `allow_overwrite = true` all pass.)

- [ ] **Step 1: Write failing tests**

Create `engine/nasty-backup/src/restore.rs` with:

```rust
//! Restore helpers: destination validation (jail every restore under
//! `/fs`) and the coarse-progress plumbing that feeds a `Restore`
//! job's `progress_fraction`.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Reasons a restore destination is rejected before any work starts.
#[derive(Debug, Error)]
pub enum RestoreDestError {
    #[error("destination must be an absolute path")]
    NotAbsolute,
    #[error("destination must be inside a NASty filesystem under /fs")]
    OutsideRoot,
    #[error("target filesystem does not exist (expected a mounted filesystem under /fs)")]
    FsRootMissing,
    #[error("destination is not empty — enable 'overwrite existing files' to restore into it")]
    NotEmpty,
    #[error("io error validating destination: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate and resolve a restore destination, jailing it under `root`
/// (`/fs` in production). Returns the absolute destination to restore
/// into. See the module's plan section for the exact rules.
pub fn validate_restore_dest(
    dest: &Path,
    root: &Path,
    allow_overwrite: bool,
) -> Result<PathBuf, RestoreDestError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses a tempdir as `root` to stand in for `/fs`, with a
    // `<root>/fsname` child standing in for a mounted filesystem.
    fn tmp_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("fsname")).unwrap();
        root
    }

    #[test]
    fn accepts_path_under_existing_fs() {
        let root = tmp_root();
        let dest = root.path().join("fsname").join("restored");
        let resolved = validate_restore_dest(&dest, root.path(), false).unwrap();
        assert!(resolved.starts_with(root.path().join("fsname")));
    }

    #[test]
    fn rejects_relative_path() {
        let root = tmp_root();
        let err = validate_restore_dest(Path::new("fsname/x"), root.path(), false);
        assert!(matches!(err, Err(RestoreDestError::NotAbsolute)));
    }

    #[test]
    fn rejects_traversal_escape() {
        let root = tmp_root();
        // /<root>/fsname/../../etc escapes root once normalized.
        let dest = root.path().join("fsname").join("..").join("..").join("etc");
        let err = validate_restore_dest(&dest, root.path(), false);
        assert!(matches!(err, Err(RestoreDestError::OutsideRoot)));
    }

    #[test]
    fn rejects_symlink_escape() {
        let root = tmp_root();
        let outside = tempfile::tempdir().unwrap();
        // <root>/fsname/link -> <outside>
        let link = root.path().join("fsname").join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let dest = link.join("restored");
        let err = validate_restore_dest(&dest, root.path(), false);
        assert!(matches!(err, Err(RestoreDestError::OutsideRoot)));
    }

    #[test]
    fn rejects_missing_filesystem() {
        let root = tmp_root();
        let dest = root.path().join("nonexistent-fs").join("restored");
        let err = validate_restore_dest(&dest, root.path(), false);
        assert!(matches!(err, Err(RestoreDestError::FsRootMissing)));
    }

    #[test]
    fn rejects_root_itself() {
        let root = tmp_root();
        let err = validate_restore_dest(root.path(), root.path(), false);
        assert!(matches!(err, Err(RestoreDestError::OutsideRoot)));
    }

    #[test]
    fn rejects_non_empty_without_overwrite() {
        let root = tmp_root();
        let dest = root.path().join("fsname").join("data");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("existing.txt"), b"hi").unwrap();
        let err = validate_restore_dest(&dest, root.path(), false);
        assert!(matches!(err, Err(RestoreDestError::NotEmpty)));
    }

    #[test]
    fn allows_non_empty_with_overwrite() {
        let root = tmp_root();
        let dest = root.path().join("fsname").join("data");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("existing.txt"), b"hi").unwrap();
        let resolved = validate_restore_dest(&dest, root.path(), true).unwrap();
        assert_eq!(resolved, dest);
    }

    #[test]
    fn allows_empty_existing_dir() {
        let root = tmp_root();
        let dest = root.path().join("fsname").join("empty");
        std::fs::create_dir(&dest).unwrap();
        assert!(validate_restore_dest(&dest, root.path(), false).is_ok());
    }
}
```

Add `pub mod restore;` to `engine/nasty-backup/src/lib.rs` (with the existing `pub mod jobs;` / `pub mod scheduler;` lines).

Confirm `tempfile` is a dev-dependency of `nasty-backup`:

Run: `grep -n 'tempfile' engine/nasty-backup/Cargo.toml`
Expected: a line under `[dev-dependencies]`. If absent, add `tempfile = "3"` under `[dev-dependencies]` in `engine/nasty-backup/Cargo.toml` (it is already used elsewhere in the workspace).

- [ ] **Step 2: Run tests to verify they fail**

Run (from `engine/`): `cargo test -p nasty-backup restore::tests`
Expected: FAIL — `validate_restore_dest` panics with `not yet implemented`.

- [ ] **Step 3: Implement `validate_restore_dest`**

Replace the `todo!()` body:

```rust
pub fn validate_restore_dest(
    dest: &Path,
    root: &Path,
    allow_overwrite: bool,
) -> Result<PathBuf, RestoreDestError> {
    if !dest.is_absolute() {
        return Err(RestoreDestError::NotAbsolute);
    }

    // Canonicalize the longest existing ancestor (resolves symlinks and
    // `..`), then re-append the not-yet-existing tail. The tail can't
    // contain symlinks because it doesn't exist on disk yet.
    let mut existing = dest;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let canonical_prefix = loop {
        match existing.canonicalize() {
            Ok(p) => break p,
            Err(_) => {
                let file = existing
                    .file_name()
                    .ok_or(RestoreDestError::OutsideRoot)?;
                tail.push(file);
                existing = existing.parent().ok_or(RestoreDestError::OutsideRoot)?;
            }
        }
    };
    let mut resolved = canonical_prefix;
    for part in tail.iter().rev() {
        resolved.push(part);
    }

    // The canonical root (resolve symlinks on `root` too, so the
    // starts_with comparison is apples-to-apples).
    let canonical_root = root.canonicalize().map_err(RestoreDestError::Io)?;

    if !resolved.starts_with(&canonical_root) {
        return Err(RestoreDestError::OutsideRoot);
    }
    // Must be strictly below root — the fs name component is required.
    let rel = resolved
        .strip_prefix(&canonical_root)
        .map_err(|_| RestoreDestError::OutsideRoot)?;
    let fs_name = rel
        .components()
        .next()
        .ok_or(RestoreDestError::OutsideRoot)?;

    // The /fs/<fs> filesystem-root must already exist as a directory.
    let fs_root = canonical_root.join(fs_name);
    if !fs_root.is_dir() {
        return Err(RestoreDestError::FsRootMissing);
    }

    // Non-empty gate.
    if !allow_overwrite && resolved.is_dir() {
        let mut entries = std::fs::read_dir(&resolved)?;
        if entries.next().is_some() {
            return Err(RestoreDestError::NotEmpty);
        }
    }

    Ok(resolved)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run (from `engine/`): `cargo test -p nasty-backup restore::tests`
Expected: PASS (all nine tests).

- [ ] **Step 5: Commit**

```bash
cd engine && cargo fmt
git add nasty-backup/src/restore.rs nasty-backup/src/lib.rs nasty-backup/Cargo.toml
git commit -m "backup: restore destination validation jailed under /fs"
```

---

## Task 3: Coarse restore progress plumbing

**Files:**
- Modify: `engine/nasty-backup/src/restore.rs`
- Test: `engine/nasty-backup/src/restore.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: rustic_core's crate-root progress exports `rustic_core::{Progress, ProgressBars, ProgressType, RusticProgress}` (the `progress` module itself is `pub(crate)`; these types are re-exported at the crate root).
- Produces:
  - `pub struct RestoreProgress` — cheap-to-clone shared handle over two atomics (`total`, `done`) with `fn new() -> Self`, `fn fraction(&self) -> f64` (returns `0.0` when total is 0, else `done/total` clamped to `[0.0, 1.0]`).
  - `pub struct RestoreProgressBars(pub RestoreProgress)` — implements `ProgressBars`; returns a live tracker only for `ProgressType::Bytes` (the restore-data progress), `Progress::hidden()` otherwise.

**Why isolate `Bytes`:** rustic's restore reports moved data via `progress_bytes("restoring file contents...")` (`set_length(total)`, then `inc(bytes)` per chunk). Other phases (open, index, collect) use `Counter`/`Spinner` progress; routing only `Bytes` to the tracker keeps the fraction a clean "bytes restored / bytes to restore" measure.

- [ ] **Step 1: Write the failing test**

Add to `restore.rs`'s `tests` module:

```rust
#[test]
fn progress_bytes_tracks_fraction() {
    use rustic_core::{ProgressBars, ProgressType};

    let progress = RestoreProgress::new();
    assert_eq!(progress.fraction(), 0.0);

    let bars = RestoreProgressBars(progress.clone());
    let p = bars.progress(ProgressType::Bytes, "restoring file contents...");
    p.set_length(100);
    p.inc(25);
    assert_eq!(progress.fraction(), 0.25);
    p.inc(75);
    assert_eq!(progress.fraction(), 1.0);
}

#[test]
fn progress_non_bytes_is_hidden_and_ignored() {
    use rustic_core::{ProgressBars, ProgressType};

    let progress = RestoreProgress::new();
    let bars = RestoreProgressBars(progress.clone());
    let p = bars.progress(ProgressType::Counter, "counting...");
    assert!(p.is_hidden());
    p.set_length(100);
    p.inc(50);
    // A Counter progress must not move the restore fraction.
    assert_eq!(progress.fraction(), 0.0);
}
```

- [ ] **Step 2: Run to verify it fails**

Run (from `engine/`): `cargo test -p nasty-backup progress_bytes_tracks_fraction`
Expected: FAIL — `RestoreProgress` / `RestoreProgressBars` not found.

- [ ] **Step 3: Implement the progress types**

Add to `restore.rs` (top-level, after the imports — add `use std::sync::Arc;` and `use std::sync::atomic::{AtomicU64, Ordering};`):

```rust
use rustic_core::{Progress, ProgressBars, ProgressType, RusticProgress};

/// Shared, cheap-to-clone handle to a restore's byte counters. The
/// spawned restore updates it via the `ProgressBars` hook; a poller
/// reads `fraction()` and writes it onto the job.
#[derive(Debug, Clone, Default)]
pub struct RestoreProgress {
    inner: Arc<RestoreCounters>,
}

#[derive(Debug, Default)]
struct RestoreCounters {
    total: AtomicU64,
    done: AtomicU64,
}

impl RestoreProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fraction restored in `[0.0, 1.0]`; `0.0` until a total is known.
    pub fn fraction(&self) -> f64 {
        let total = self.inner.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let done = self.inner.done.load(Ordering::Relaxed);
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// `ProgressBars` impl that feeds only the restore-bytes progress into a
/// `RestoreProgress`. Non-byte progress (open/index/collect counters) is
/// hidden and ignored so the fraction stays a clean bytes measure.
#[derive(Debug)]
pub struct RestoreProgressBars(pub RestoreProgress);

impl ProgressBars for RestoreProgressBars {
    fn progress(&self, progress_type: ProgressType, _prefix: &str) -> Progress {
        match progress_type {
            ProgressType::Bytes => Progress::new(BytesProgress(self.0.clone())),
            _ => Progress::hidden(),
        }
    }
}

#[derive(Debug)]
struct BytesProgress(RestoreProgress);

impl RusticProgress for BytesProgress {
    fn is_hidden(&self) -> bool {
        false
    }
    fn set_length(&self, len: u64) {
        self.0.inner.total.store(len, Ordering::Relaxed);
    }
    fn set_title(&self, _title: &str) {}
    fn inc(&self, inc: u64) {
        self.0.inner.done.fetch_add(inc, Ordering::Relaxed);
    }
    fn finish(&self) {}
}
```

- [ ] **Step 4: Run to verify it passes**

Run (from `engine/`): `cargo test -p nasty-backup restore::tests`
Expected: PASS (the eleven restore tests).

- [ ] **Step 5: Commit**

```bash
cd engine && cargo fmt
git add nasty-backup/src/restore.rs
git commit -m "backup: coarse restore progress plumbing (bytes-only ProgressBars)"
```

---

## Task 4: Restore execution + job wrapper

**Files:**
- Modify: `engine/nasty-backup/src/lib.rs`
- Test: `engine/nasty-backup/src/lib.rs` (`#[cfg(test)] mod tests`) — pure surface only (see note).

**Interfaces:**
- Consumes: `restore::{validate_restore_dest, RestoreProgress, RestoreProgressBars, RestoreDestError}`; `jobs::{BackupJob, BackupJobKind}`; existing `make_repo`, `creds`, `resolve_profile_password`, `profile.resolve_runtime()`.
- Produces:
  - `pub struct RestoreSummary { pub files_restored: u64, pub bytes_restored: u64, pub dest: String }` (derives `Debug, Clone, Serialize, Deserialize, JsonSchema`).
  - `const FS_ROOT: &str = "/fs";` (module-level in `lib.rs`).
  - `fn make_repo_with_progress(profile: &BackupProfile, resolved: &ResolvedTargetSecrets, progress: RestoreProgress) -> Result<Repository<()>, BackupError>` — like `make_repo` but via `Repository::new_with_progress` (the progress backend is stored internally as `Arc<dyn ProgressBars>`; the `S` type param is the repo *status*, so the return type is `Repository<()>`, same as `make_repo`).
  - `async fn restore_inner(&self, profile_id: &str, snapshot_id: &str, dest: PathBuf, progress: RestoreProgress) -> Result<RestoreSummary, BackupError>` — opens repo, resolves the snapshot root node, restores it into `dest` (created if missing), returns the summary. Assumes `dest` is already validated.
  - `pub async fn start_restore(&self, profile_id: &str, snapshot_id: &str, dest: &str, allow_overwrite: bool) -> Result<BackupJob, BackupError>` — validates dest under `/fs` (mapping `RestoreDestError` → `BackupError::Failed`), starts a `Restore` job (mapping `JobError` → `BackupError::Failed`), spawns the worker + a 1 s progress poller, returns the `Pending` job.

**Note on tests:** the rustic restore path (`restore_inner`) hits the real FS, rustic_core, and a repo — it is integration territory, exercised by the crate's existing "on real boxes" convention and by the real-world S3 DR scenario in the spec. This task adds no unit test for `restore_inner` itself (consistent with `run_backup`/`list_snapshots`, which have none). The pure pieces it composes are already unit-tested in Tasks 2–3. The task's testable deliverable is that the crate compiles, clippy is clean, and the full suite still passes.

- [ ] **Step 1: Add the `Repository` type param note and imports**

`make_repo` currently returns `Repository<()>`. Confirm the rustic imports in `lib.rs` include what restore needs. At the top `use rustic_core::{...}` block, extend it to add `LocalDestination, LsOptions, RestoreOptions`:

```rust
use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, Credentials, ForgetGroups, Grouped, KeepOptions,
    KeyOptions, LocalDestination, LsOptions, PathList, Repository, RepositoryOptions,
    RestoreOptions, SnapshotGroupCriterion, SnapshotOptions, repofile::SnapshotFile,
};
```

Add the restore module use near the other crate-internal uses:

```rust
use restore::{RestoreProgress, RestoreProgressBars, validate_restore_dest};
```

Add the constant near `STATE_PATH`:

```rust
/// Root that every restore destination must resolve under — bcachefs
/// storage. Mirrors `guestshare.rs`'s FILES_ROOT jail.
const FS_ROOT: &str = "/fs";
```

- [ ] **Step 2: Add the `RestoreSummary` type**

After `struct BackupSnapshot { ... }`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestoreSummary {
    /// Number of files written to the destination.
    pub files_restored: u64,
    /// Total bytes written to the destination.
    pub bytes_restored: u64,
    /// Absolute destination the snapshot was restored into.
    pub dest: String,
}
```

- [ ] **Step 3: Add `make_repo_with_progress`**

After the existing `make_repo` function. `Repository::new_with_progress` returns `Repository<()>` — the `S` generic is the repo *status*, not the progress type; the progress backend is stored internally as `Arc<dyn ProgressBars>`. So this mirrors `make_repo`'s return type exactly:

```rust
/// Like [`make_repo`] but builds the repository with a live progress-bar
/// backend so restore-byte progress can be surfaced on the job. Same
/// secret-resolution contract as `make_repo`.
fn make_repo_with_progress(
    profile: &BackupProfile,
    resolved: &ResolvedTargetSecrets,
    progress: RestoreProgress,
) -> Result<Repository<()>, BackupError> {
    let backends = profile
        .target
        .to_backend_options(resolved)
        .to_backends()
        .map_err(|e| BackupError::Failed(format!("backend: {e}")))?;
    let repo_opts = RepositoryOptions::default();
    Repository::new_with_progress(&repo_opts, &backends, RestoreProgressBars(progress))
        .map_err(|e| BackupError::Failed(format!("repo: {e}")))
}
```

- [ ] **Step 4: Add `restore_inner`**

In `impl BackupService`, after `list_snapshots`:

```rust
    /// Restore a whole snapshot into an already-validated destination.
    /// Opens the repo with a progress backend, resolves the snapshot's
    /// root node, and restores it (merge semantics: create/overwrite,
    /// never delete). Returns a summary of what was written.
    async fn restore_inner(
        &self,
        profile_id: &str,
        snapshot_id: &str,
        dest: std::path::PathBuf,
        progress: RestoreProgress,
    ) -> Result<RestoreSummary, BackupError> {
        let profile = self.get_profile_internal(profile_id).await?;
        let password = resolve_profile_password(&profile).await?;
        let resolved = profile.resolve_runtime().await?;
        let snapshot_id = snapshot_id.to_string();
        let dest_str = dest.to_string_lossy().to_string();

        tokio::task::spawn_blocking(move || {
            let repo = make_repo_with_progress(&profile, &resolved, progress)?;
            let repo = repo
                .open(&creds(&password))
                .map_err(|e| BackupError::Failed(format!("open: {e}")))?;
            let repo = repo
                .to_indexed()
                .map_err(|e| BackupError::Failed(format!("index: {e}")))?;

            // Resolve the snapshot's root node ("<id>" with no subpath).
            let node = repo
                .node_from_snapshot_path(&snapshot_id, |_| true)
                .map_err(|e| BackupError::Failed(format!("snapshot not found: {e}")))?;

            // Destination directory (create=true, expect_file=false).
            let dest = LocalDestination::new(&dest_str, true, false)
                .map_err(|e| BackupError::Failed(format!("destination: {e}")))?;

            let opts = RestoreOptions::default(); // delete = false

            // prepare_restore consumes a node streamer; restore needs a
            // fresh one, so build the recursive streamer twice.
            let plan = repo
                .prepare_restore(&opts, repo.ls(&node, &LsOptions::default())
                    .map_err(|e| BackupError::Failed(format!("list: {e}")))?, &dest, false)
                .map_err(|e| BackupError::Failed(format!("prepare: {e}")))?;

            let files_restored = plan.stats.files.restore;
            let bytes_restored = plan.restore_size;

            repo.restore(
                plan,
                &opts,
                repo.ls(&node, &LsOptions::default())
                    .map_err(|e| BackupError::Failed(format!("list: {e}")))?,
                &dest,
            )
            .map_err(|e| BackupError::Failed(format!("restore: {e}")))?;

            Ok::<_, BackupError>(RestoreSummary {
                files_restored,
                bytes_restored,
                dest: dest_str,
            })
        })
        .await
        .map_err(|e| BackupError::Failed(format!("spawn: {e}")))?
    }
```

Field names confirmed against rustic_core 0.12.0 (`src/commands/restore.rs`): `RestorePlan.restore_size: u64` (pub, total bytes to restore — the same value passed to the restore `progress_bytes.set_length`) and `RestorePlan.stats.files.restore: u64` (pub, count of files to restore, via `stats: RestoreStats` → `files: FileDirStats`). Both are used above. If a future crate bump renames them, `cargo build` will point at the exact line — read the real field from the source, don't guess.

- [ ] **Step 5: Add `start_restore`**

After `start_check_repo`:

```rust
    /// Validate the destination under `/fs`, start a `Restore` job, and
    /// spawn the background worker plus a 1 s progress poller. Returns
    /// the `Pending` job immediately (poll `backup.jobs.get`). Dest and
    /// job-collision errors surface synchronously as `BackupError`.
    pub async fn start_restore(
        &self,
        profile_id: &str,
        snapshot_id: &str,
        dest: &str,
        allow_overwrite: bool,
    ) -> Result<BackupJob, BackupError> {
        // Pre-flight: jail + fs-exists + non-empty gate, before any job.
        let resolved_dest = validate_restore_dest(
            std::path::Path::new(dest),
            std::path::Path::new(FS_ROOT),
            allow_overwrite,
        )
        .map_err(|e| BackupError::Failed(e.to_string()))?;

        let job = self
            .jobs
            .start(profile_id, BackupJobKind::Restore)
            .await
            .map_err(|e| BackupError::Failed(e.to_string()))?;

        let job_id = job.id.clone();
        let profile_id = profile_id.to_string();
        let snapshot_id = snapshot_id.to_string();
        let registry = self.jobs.clone();
        let service = self.clone_for_task();
        let progress = RestoreProgress::new();
        let poll_progress = progress.clone();

        tokio::spawn(async move {
            registry.mark_running(&job_id).await;

            let work = service.restore_inner(&profile_id, &snapshot_id, resolved_dest, progress);
            tokio::pin!(work);
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.tick().await; // first tick fires immediately; skip it

            loop {
                tokio::select! {
                    res = &mut work => {
                        match res {
                            Ok(summary) => {
                                let value = serde_json::to_value(&summary)
                                    .unwrap_or(serde_json::Value::Null);
                                registry.mark_progress(&job_id, 1.0).await;
                                registry.mark_succeeded(&job_id, value).await;
                            }
                            Err(e) => registry.mark_failed(&job_id, e.to_string()).await,
                        }
                        break;
                    }
                    _ = ticker.tick() => {
                        registry.mark_progress(&job_id, poll_progress.fraction()).await;
                    }
                }
            }
        });

        Ok(job)
    }
```

- [ ] **Step 6: Verify it compiles and the suite passes**

Run (from `engine/`):
```
cargo build -p nasty-backup
cargo test -p nasty-backup
```
Expected: build succeeds; all tests pass. If `restore_inner` fails to compile on `plan.stats.files.restore` / `plan.restore_size`, fix per the Step 4 note (read the real field names from the crate source) and re-run.

- [ ] **Step 7: Full verification**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test -p nasty-backup`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
cd engine && cargo fmt
git add nasty-backup/src/lib.rs
git commit -m "backup: whole-snapshot restore execution + job wrapper with progress"
```

---

## Task 5: Engine router + method registry

**Files:**
- Modify: `engine/nasty-engine/src/router/backup.rs`
- Modify: `engine/nasty-engine/src/registry/methods.rs`
- Modify (test only): `engine/nasty-engine/src/registry/paths.rs`

**Interfaces:**
- Consumes: `BackupService::start_restore` (Task 4), the router helpers `parse_params`, `ok`, `err`.
- Produces: the `backup.restore` RPC method, POST verb, Operator role.

- [ ] **Step 1: Add the router arm**

In `engine/nasty-engine/src/router/backup.rs`, add a `RestoreParams` struct and an arm. Put the struct just below the `use` lines:

```rust
#[derive(Deserialize)]
struct RestoreParams {
    id: String,
    snapshot_id: String,
    dest: String,
    #[serde(default)]
    allow_overwrite: bool,
}
```

Add the arm alongside the other `backup.*` arms (e.g. after `"backup.snapshots"`):

```rust
        "backup.restore" => match parse_params::<RestoreParams>(req) {
            Ok(p) => match state
                .backups
                .start_restore(&p.id, &p.snapshot_id, &p.dest, p.allow_overwrite)
                .await
            {
                Ok(job) => ok(req, job),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(e) => err(req, e),
        },
```

- [ ] **Step 2: Register the method**

In `engine/nasty-engine/src/registry/methods.rs`, after the `backup.snapshots` `Method { ... }` block, add:

```rust
                Method {
                    name: "backup.restore",
                    desc: "Restore a whole snapshot into a destination under /fs. Validates the destination (jailed to a mounted filesystem, non-empty destinations require allow_overwrite) then spawns a background Restore job; poll backup.jobs.get for progress_fraction and completion. Returns a BackupJob handle immediately.",
                    role: MethodRole::Operator,
                    params: MethodParams::AdHoc(serde_json::json!({
                        "type": "object",
                        "required": ["id", "snapshot_id", "dest"],
                        "properties": {
                            "id": { "type": "string", "description": "Backup profile identifier (identifies the repo + credentials)." },
                            "snapshot_id": { "type": "string", "description": "Snapshot id to restore (from backup.snapshots)." },
                            "dest": { "type": "string", "description": "Absolute destination path; must resolve under /fs." },
                            "allow_overwrite": { "type": "boolean", "description": "Permit restoring into a non-empty destination (default false)." }
                        }
                    })),
                    result: Some(gen_schema::<nasty_backup::jobs::BackupJob>(generator)),
                },
```

- [ ] **Step 3: Add the verb-translation test**

In `engine/nasty-engine/src/registry/paths.rs`, in the test that asserts POST-defaulting methods (next to `assert_eq!(translate("backup.run").0, HttpVerb::Post);`), add:

```rust
        assert_eq!(translate("backup.restore").0, HttpVerb::Post);
```

- [ ] **Step 4: Build + test the engine crate**

Run (from `engine/`):
```
cargo build -p nasty-engine
cargo test -p nasty-engine registry
```
Expected: build succeeds; registry tests (including the new POST assertion and any "every method has an arm / every arm has a method" consistency test) pass.

If a registry consistency test reports `backup.restore` missing an arm or vice-versa, reconcile — both the arm (Step 1) and the registry entry (Step 2) must be present.

- [ ] **Step 5: Full verification**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`
Expected: clean across the workspace.

- [ ] **Step 6: Commit**

```bash
cd engine && cargo fmt
git add nasty-engine/src/router/backup.rs nasty-engine/src/registry/methods.rs nasty-engine/src/registry/paths.rs
git commit -m "backup: wire backup.restore RPC (router arm + method registry)"
```

---

## Task 6: WebUI — restore action, dialog, progress bar

**Files:**
- Modify: `webui/src/lib/types.ts`
- Modify: `webui/src/routes/backups/+page.svelte`

**Interfaces:**
- Consumes: `backup.restore` (params `{ id, snapshot_id, dest, allow_overwrite }` → `BackupJob`); existing `backup.jobs.get` polling; `filesystems` (already loaded via `fs.list`), `snapshots` (already loaded via `backup.snapshots`).
- Produces: a Restore button per snapshot row, a restore dialog, `startRestore(...)`, and a progress bar rendered from `activeJobs[profileId].progress_fraction`.

- [ ] **Step 1: Update the TypeScript types**

In `webui/src/lib/types.ts`:

Change the `BackupJobKind` union to include restore:

```ts
export type BackupJobKind = 'init_repo' | 'run_backup' | 'check_repo' | 'restore';
```

Add `progress_fraction` to the `BackupJob` interface (after `progress`):

```ts
	progress?: string | null;
	/** Coarse restore progress in [0,1]; set only by `restore` jobs. */
	progress_fraction?: number | null;
```

Add a `RestoreSummary` interface near `BackupSnapshot`:

```ts
export interface RestoreSummary {
	files_restored: number;
	bytes_restored: number;
	dest: string;
}
```

- [ ] **Step 2: Run the WebUI type check to confirm the types compile**

Run (from `webui/`): `npm run check`
Expected: no new type errors from these edits (there will be no restore UI yet — that's next).

- [ ] **Step 3: Add restore dialog state and `startRestore`**

In `webui/src/routes/backups/+page.svelte`'s `<script>`, near the snapshots-viewer state (`viewSnapshotsId`, `snapshots`), add restore-dialog state:

```ts
	// Restore dialog
	let restoreSnapshot: BackupSnapshot | null = $state(null);
	let restoreProfileId: string | null = $state(null);
	let restoreFs = $state('');
	let restoreSubpath = $state('');
	let restoreAllowOverwrite = $state(false);

	function openRestore(profileId: string, snap: BackupSnapshot) {
		restoreProfileId = profileId;
		restoreSnapshot = snap;
		restoreFs = filesystems.find(f => f.mounted)?.name ?? '';
		restoreSubpath = '';
		restoreAllowOverwrite = false;
	}

	function closeRestore() {
		restoreSnapshot = null;
		restoreProfileId = null;
	}

	/** Fire backup.restore and poll it to completion, reusing the same
	 * activeJobs badge + progress bar as the other backup jobs. */
	async function startRestore() {
		if (!restoreProfileId || !restoreSnapshot || !restoreFs) return;
		const sub = restoreSubpath.replace(/^\/+/, '').trim();
		const dest = sub ? `/fs/${restoreFs}/${sub}` : `/fs/${restoreFs}`;
		const profileId = restoreProfileId;
		const snapshotId = restoreSnapshot.id;
		const allow = restoreAllowOverwrite;
		closeRestore();
		const job = await withToast(
			() => client.call<BackupJob>('backup.restore', {
				id: profileId,
				snapshot_id: snapshotId,
				dest,
				allow_overwrite: allow,
			}),
			'Restore started',
		);
		if (!job) return;
		attachJobPoll(profileId, job, refresh);
	}
```

- [ ] **Step 4: Extract the shared job poller `attachJobPoll`**

Add a helper next to `startBackupJob` that contains the poll loop, and route `startRestore` (Step 3) through it. Add:

```ts
	/** Attach a 2 s poll for an already-started job, updating the inline
	 * activeJobs badge until it terminates. Shared by startRestore and
	 * the other job starters. */
	function attachJobPoll(
		profileId: string,
		job: BackupJob,
		onSuccess?: () => Promise<void> | void,
	) {
		activeJobs[profileId] = job;
		stopJobPoll(profileId);
		jobPollers[profileId] = setInterval(async () => {
			try {
				const updated = await client.call<BackupJob>('backup.jobs.get', { id: job.id });
				activeJobs[profileId] = updated;
				if (updated.state === 'succeeded' || updated.state === 'failed') {
					stopJobPoll(profileId);
					if (updated.state === 'failed') {
						await withToast(
							() => Promise.reject({ message: updated.error ?? 'backup job failed' }),
							'',
						);
					}
					delete activeJobs[profileId];
					if (onSuccess) await onSuccess();
				}
			} catch {
				stopJobPoll(profileId);
				delete activeJobs[profileId];
			}
		}, 2000);
	}
```

- [ ] **Step 5: Add the Restore button to each snapshot row**

In the Snapshots modal (the `{#each snapshots as ...}` list — locate it near `{:else if snapshots.length === 0}`), add a Restore button per snapshot row. Each row renders snapshot metadata; add, using the profile id the modal was opened with (`viewSnapshotsId`):

```svelte
					<Button
						size="xs"
						variant="secondary"
						onclick={() => { if (viewSnapshotsId) openRestore(viewSnapshotsId, snap); }}
					>
						Restore
					</Button>
```

(Match the existing per-row markup: `snap` is the loop variable — confirm the actual binding name in the `{#each}` and use it.)

- [ ] **Step 6: Add the restore dialog markup**

After the Snapshots modal block (`{/if}` that closes `{#if viewSnapshotsId}`), add:

```svelte
<!-- Restore dialog -->
{#if restoreSnapshot}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={closeRestore}>
		<Card class="w-[32rem] max-w-[90vw]" onclick={(e: MouseEvent) => e.stopPropagation()}>
			<CardContent class="space-y-4 p-4">
				<div class="flex items-center justify-between">
					<span class="text-sm font-semibold">Restore snapshot</span>
					<Button variant="ghost" size="xs" onclick={closeRestore}>Close</Button>
				</div>
				<p class="text-muted-foreground text-xs">
					Restoring snapshot {restoreSnapshot.id.slice(0, 8)} from {restoreSnapshot.time}.
					Files are written into the chosen filesystem; existing files are only replaced
					when overwrite is enabled, and nothing else is deleted.
				</p>

				<div class="space-y-1">
					<Label>Destination filesystem</Label>
					<select class={requiredFieldCls(!restoreFs)} bind:value={restoreFs}>
						{#each filesystems.filter(f => f.mounted) as fs}
							<option value={fs.name}>{fs.name}</option>
						{/each}
					</select>
				</div>

				<div class="space-y-1">
					<Label>Subfolder (optional)</Label>
					<Input bind:value={restoreSubpath} placeholder="restored/2026-07-09" />
					<p class="text-muted-foreground text-xs">
						Destination: /fs/{restoreFs || '<fs>'}{restoreSubpath ? '/' + restoreSubpath.replace(/^\/+/, '') : ''}
					</p>
				</div>

				<label class="flex items-center gap-2 text-sm">
					<input type="checkbox" bind:checked={restoreAllowOverwrite} />
					Overwrite existing files (allow restoring into a non-empty folder)
				</label>

				<div class="flex justify-end gap-2">
					<Button variant="secondary" size="sm" onclick={closeRestore}>Cancel</Button>
					<Button size="sm" disabled={!restoreFs} onclick={startRestore}>Restore</Button>
				</div>
			</CardContent>
		</Card>
	</div>
{/if}
```

(Match the page's existing modal/markup conventions — if other dialogs on this page use a shared modal component or different class names, mirror those rather than the generic markup above.)

- [ ] **Step 7: Render the progress bar in the active-job badge**

Locate where `activeJobs[profile.id]` renders its inline "in progress" badge (the label driven by the active job). For a `restore` job with a known fraction, show a coarse bar. Add, inside that badge's markup:

```svelte
				{#if activeJobs[profile.id]?.kind === 'restore'}
					<span>Restoring…</span>
					{#if activeJobs[profile.id]?.progress_fraction != null}
						<div class="bg-muted h-1.5 w-24 overflow-hidden rounded">
							<div
								class="bg-primary h-full"
								style="width: {Math.round((activeJobs[profile.id]!.progress_fraction ?? 0) * 100)}%"
							></div>
						</div>
					{/if}
				{/if}
```

(If the existing badge maps `kind` → label text via a helper or `{#if}` chain, extend that chain with the `restore` case instead of duplicating; keep the progress bar addition.)

- [ ] **Step 8: Type-check and test the WebUI**

Run (from `webui/`):
```
npm run check
npm test
```
Expected: no type errors; existing tests pass. (No new WebUI unit test is required — the page has no existing component test harness for these modals; the restore flow is validated end-to-end against a real engine per the spec's acceptance test.)

- [ ] **Step 9: Commit**

```bash
git add webui/src/lib/types.ts webui/src/routes/backups/+page.svelte
git commit -m "webui: restore action, dialog, and progress bar on Backups page"
```

---

## Self-Review

**Spec coverage** (checked against `docs/backup-restore.md`):
- Whole-snapshot restore to `/fs` → Tasks 2, 4.
- Cross-instance DR (add profile → restore) → works via existing profile/target/password handling; no new task needed (restore uses the same `make_repo`/`resolve` path).
- `BackupJobKind::Restore`, tracked job, coarse progress → Tasks 1, 3, 4.
- API `backup.restore { id, snapshot_id, dest, allow_overwrite }` → Task 5.
- Destination validation (under `/fs`, traversal/symlink reject, fs-exists, non-empty gate) → Task 2.
- rustic merge semantics, no `--delete` → Task 4 (`RestoreOptions::default()`, delete=false; documented).
- Progress fraction from `ProgressBars` → Tasks 3, 4; UI bar → Task 6.
- Error handling (repo open/decrypt, snapshot-not-found inside job, invalid-dest up front, mid-restore partial) → Tasks 4, 5.
- Testing: unit for validation (Task 2) and progress (Task 3); integration/real-world S3 DR is out-of-band per spec.
- Follow-ups (file-level, in-place) → explicitly not planned.

**Placeholder scan:** No TBD/TODO/"handle errors" — every code step carries concrete code. The one deliberate "read the real field names" note (Task 4, Step 4) is a guardrail against inventing rustic `RestorePlan` field names, with an exact `grep` command to resolve it, not a placeholder.

**Type consistency:** `RestoreProgress`, `RestoreProgressBars`, `validate_restore_dest`, `RestoreDestError`, `RestoreSummary`, `BackupJobKind::Restore`, `progress_fraction`, `mark_progress`, `start_restore`, `restore_inner` are named identically across the tasks that define and consume them. `backup.restore` params match between router (Task 5) and WebUI call (Task 6). `progress_fraction` is `Option<f64>` in Rust and `number | null` in TS.
