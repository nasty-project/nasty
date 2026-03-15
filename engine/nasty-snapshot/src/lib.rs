//! Snapshot coordination layer.
//!
//! Wraps the low-level bcachefs snapshot operations in nasty-storage with
//! protocol-aware fencing so that snapshots of block subvolumes are always
//! crash-consistent, even when an NVMe-oF or iSCSI initiator has dirty data
//! in its page cache at the time of the request.
//!
//! # NVMe-oF fencing
//! Before snapshotting a block subvolume, any NVMe-oF namespaces backed by
//! its loop device are temporarily disabled via nvmet configfs. This causes
//! the initiator's nvme driver to drain in-flight writes and enter error
//! recovery. After the bcachefs snapshot is taken the namespaces are
//! re-enabled and the initiator reconnects transparently.
//!
//! # iSCSI
//! LIO does not expose per-LUN quiesce controls. For iSCSI-backed volumes a
//! server-side `sync` is issued before snapshotting, which flushes any writes
//! the target has already acknowledged into the backing filesystem. This gives
//! crash-consistent semantics for acknowledged writes.

use std::sync::Arc;
use std::time::Duration;

use nasty_sharing::NvmeofService;
use nasty_storage::SubvolumeService;
use nasty_storage::subvolume::{
    CloneSnapshotRequest, CreateSnapshotRequest, DeleteSnapshotRequest, Snapshot, Subvolume,
    SubvolumeError, SubvolumeType,
};
use tracing::{debug, info, warn};

pub struct SnapshotService {
    subvolumes: Arc<SubvolumeService>,
    nvmeof: Arc<NvmeofService>,
}

impl SnapshotService {
    pub fn new(subvolumes: Arc<SubvolumeService>, nvmeof: Arc<NvmeofService>) -> Self {
        Self { subvolumes, nvmeof }
    }

    /// Create a snapshot with protocol-aware fencing for block subvolumes.
    pub async fn create(
        &self,
        req: CreateSnapshotRequest,
        owner_filter: Option<&str>,
    ) -> Result<Snapshot, SubvolumeError> {
        let subvol = self.subvolumes.get(&req.pool, &req.subvolume, owner_filter).await?;

        let fenced = if subvol.subvolume_type == SubvolumeType::Block {
            if let Some(ref loop_dev) = subvol.block_device {
                // ── NVMe-oF: disable namespaces to drain initiator writes ──
                debug!(
                    "snapshot: block subvolume '{}' has loop device {}, searching NVMe-oF namespaces",
                    req.subvolume, loop_dev
                );
                let namespaces = self.nvmeof.find_namespaces_for_device(loop_dev).await;
                debug!(
                    "snapshot: find_namespaces_for_device({loop_dev}) returned {} result(s): {:?}",
                    namespaces.len(),
                    namespaces
                );
                if !namespaces.is_empty() {
                    info!(
                        "Fencing {} NVMe-oF namespace(s) on {} for snapshot consistency",
                        namespaces.len(),
                        loop_dev
                    );
                    for (nqn, nsid) in &namespaces {
                        if let Err(e) = self.nvmeof.set_namespace_enabled(nqn, *nsid, false).await {
                            warn!("Failed to fence NVMe-oF namespace {nqn}/ns{nsid}: {e}");
                        }
                    }
                    // Give the initiator time to drain in-flight I/O
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                // ── iSCSI: flush acknowledged writes to disk ──────────────
                // LIO has no per-LUN quiesce; sync ensures anything the target
                // has received is persisted before the snapshot.
                let _ = tokio::process::Command::new("sync").output().await;

                namespaces
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        // SubvolumeService handles blockdev flush + bcachefs snapshot
        let result = self.subvolumes.create_snapshot(req, owner_filter).await;

        // Always unfence, even if snapshot failed
        for (nqn, nsid) in &fenced {
            if let Err(e) = self.nvmeof.set_namespace_enabled(nqn, *nsid, true).await {
                warn!("Failed to unfence NVMe-oF namespace {nqn}/ns{nsid}: {e}");
            }
        }

        result
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
