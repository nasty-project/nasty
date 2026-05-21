//! Snapshot coordination layer.
//!
//! Wraps the low-level bcachefs snapshot operations in nasty-storage.
//! bcachefs snapshots are atomic COW operations — the backing block device
//! remains fully operational throughout. No protocol-level fencing is needed.

use std::sync::Arc;

use nasty_storage::SubvolumeService;
use nasty_storage::subvolume::{
    CloneSnapshotRequest, CreateSnapshotRequest, DeleteSnapshotRequest, Snapshot, Subvolume,
    SubvolumeError,
};

pub struct SnapshotService {
    subvolumes: Arc<SubvolumeService>,
}

impl SnapshotService {
    pub fn new(subvolumes: Arc<SubvolumeService>) -> Self {
        Self { subvolumes }
    }

    pub async fn create(
        &self,
        req: CreateSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<Snapshot, SubvolumeError> {
        self.subvolumes.create_snapshot(req, owner_filter).await
    }

    pub async fn list(
        &self,
        fs_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        self.subvolumes.list_snapshots(fs_name, owner_filter).await
    }

    pub async fn delete(
        &self,
        req: DeleteSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<(), SubvolumeError> {
        self.subvolumes.delete_snapshot(req, owner_filter).await
    }

    pub async fn clone_snapshot(
        &self,
        req: CloneSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<Subvolume, SubvolumeError> {
        self.subvolumes.clone_snapshot(req, owner_filter).await
    }
}

// ── Tests ─────────────────────────────────────────────────────
//
// This crate is a thin delegate over nasty-storage's SubvolumeService
// — every method forwards verbatim. The behavioural tests live next
// to the real implementation in nasty-storage; what we can pin here
// is that the wrapping stays a zero-cost shared-Arc indirection.
//
// Both checks have caught regressions in similar wrapper crates
// elsewhere: a constructor that suddenly takes a config Path, or a
// service that picks up an internal RwLock and stops being trivially
// shareable across tasks.
#[cfg(test)]
mod tests {
    use super::*;
    use nasty_storage::FilesystemService;

    #[test]
    fn construction_is_pure() {
        // SnapshotService::new must not touch the filesystem — it's
        // called at engine startup before mount restore has run, and
        // any sync I/O here would push state-read failures earlier
        // than the load_singleton_or_recover recovery path can catch.
        let subs = Arc::new(SubvolumeService::new(FilesystemService::new()));
        let _svc = SnapshotService::new(subs);
        // No assertion needed — reaching this line under cargo test
        // (no root, no bcachefs, no /var/lib/nasty) is the assertion.
    }

    #[test]
    fn service_is_send_and_sync() {
        // The engine stores SnapshotService inside AppState which is
        // wrapped in Arc and handed to axum handlers across tasks.
        // Losing Send/Sync — e.g. by introducing a non-Send field —
        // would manifest only at the AppState assembly site with a
        // hard-to-parse trait-bound error; this catches it at the
        // wrapper level.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SnapshotService>();
    }
}
