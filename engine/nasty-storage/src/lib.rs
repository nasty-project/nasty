//! bcachefs filesystem and subvolume management
//!
//! This crate wraps bcachefs-tools CLI and sysfs interfaces
//! to provide storage filesystem lifecycle operations.

pub mod cmd;
pub mod disk_type;
pub mod filesystem;
pub mod io_scheduler;
mod provisioning;
pub mod scrub_scheduler;
pub mod subvolume;

pub use filesystem::{FilesystemError, FilesystemService};
pub use scrub_scheduler::ScrubScheduleService;
pub use subvolume::SubvolumeService;
