//! bcachefs filesystem and subvolume management
//!
//! This crate wraps bcachefs-tools CLI and sysfs interfaces
//! to provide storage filesystem lifecycle operations.

pub mod cmd;
pub mod filesystem;
pub mod subvolume;

pub use filesystem::{FilesystemService, FilesystemError};
pub use subvolume::SubvolumeService;
