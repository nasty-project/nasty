//! Restore helpers: destination validation (jail every restore under
//! `/fs`) and the coarse-progress plumbing that feeds a `Restore`
//! job's `progress_fraction`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rustic_core::{Progress, ProgressBars, ProgressType, RusticProgress};
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
                let file = existing.file_name().ok_or(RestoreDestError::OutsideRoot)?;
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
        assert!(resolved.starts_with(root.path().join("fsname").canonicalize().unwrap()));
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
        assert_eq!(resolved, dest.canonicalize().unwrap());
    }

    #[test]
    fn allows_empty_existing_dir() {
        let root = tmp_root();
        let dest = root.path().join("fsname").join("empty");
        std::fs::create_dir(&dest).unwrap();
        assert!(validate_restore_dest(&dest, root.path(), false).is_ok());
    }

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
}
