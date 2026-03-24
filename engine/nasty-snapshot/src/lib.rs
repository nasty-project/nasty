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
        pool_name: &str,
        owner_filter: Option<&str>,
    ) -> Result<Vec<Snapshot>, SubvolumeError> {
        self.subvolumes.list_snapshots(pool_name, owner_filter).await
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
