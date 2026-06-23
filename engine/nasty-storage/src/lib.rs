//! bcachefs filesystem and subvolume management
//!
//! This crate wraps bcachefs-tools CLI and sysfs interfaces
//! to provide storage filesystem lifecycle operations.

pub mod cmd;
pub mod disk_type;
pub mod filesystem;
pub mod subvolume;

pub use filesystem::{FilesystemError, FilesystemService};
pub use subvolume::SubvolumeService;
