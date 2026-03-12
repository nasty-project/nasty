//! bcachefs pool and subvolume management
//!
//! This crate wraps bcachefs-tools CLI and sysfs interfaces
//! to provide storage pool lifecycle operations.

pub mod cmd;
pub mod pool;
pub mod subvolume;

pub use pool::PoolService;
pub use subvolume::SubvolumeService;
