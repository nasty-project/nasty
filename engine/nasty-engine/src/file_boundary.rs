//! Descriptor-relative filesystem access for untrusted paths.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

/// Open a regular file under a configured shared root without ever resolving
/// and reopening a pathname. Symlinks are not followed at either level.
pub fn open_regular_beneath(
    files_root: &Path,
    shared_root: &Path,
    relative: &Path,
) -> io::Result<File> {
    let shared_relative = shared_root
        .strip_prefix(files_root)
        .map_err(|_| invalid_path("shared root is outside the files root"))?;
    validate_relative(shared_relative, false)?;
    validate_relative(relative, true)?;

    let files_fd = open_directory(files_root)?;
    let shared_fd = open_relative(&files_fd, shared_relative, false)?;
    let shared_meta = metadata_for_fd(&shared_fd)?;

    if relative.as_os_str().is_empty() {
        return readable_regular_file(shared_fd, None);
    }
    if !shared_meta.is_dir() {
        return Err(invalid_path("relative path supplied for a file share"));
    }

    let shared_device = shared_meta.dev();
    let target_fd = open_relative(&shared_fd, relative, true)?;
    readable_regular_file(target_fd, Some(shared_device))
}

fn readable_regular_file(fd: OwnedFd, expected_device: Option<u64>) -> io::Result<File> {
    let metadata = metadata_for_fd(&fd)?;
    if !metadata.is_file() || expected_device.is_some_and(|device| metadata.dev() != device) {
        return Err(invalid_path(
            "target is not a regular file on the shared filesystem",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        // `O_PATH` lets us inspect special files without opening them for I/O.
        // Reopening this stable descriptor cannot be redirected by path races.
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)?;
        let reopened = file.metadata()?;
        if reopened.dev() != metadata.dev() || reopened.ino() != metadata.ino() {
            return Err(invalid_path("reopened descriptor identity changed"));
        }
        Ok(file)
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(File::from(fd))
    }
}

fn validate_relative(path: &Path, allow_empty: bool) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(invalid_path("empty relative path"))
        };
    }
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_path(
            "path must contain only normal relative components",
        ));
    }
    Ok(())
}

fn open_directory(path: &Path) -> io::Result<OwnedFd> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    Ok(file.into())
}

#[cfg(target_os = "linux")]
fn open_relative(base: &OwnedFd, relative: &Path, no_xdev: bool) -> io::Result<OwnedFd> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_XDEV: u64 = 0x01;
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;

    let path = c_string(relative.as_os_str())?;
    let mut resolve = RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS;
    if no_xdev {
        resolve |= RESOLVE_NO_XDEV;
    }
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            base.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// macOS development/tests use component-at-a-time `openat`. The appliance
/// uses the Linux implementation above; unsupported Linux kernels fail closed.
#[cfg(not(target_os = "linux"))]
fn open_relative(base: &OwnedFd, relative: &Path, no_xdev: bool) -> io::Result<OwnedFd> {
    let components: Vec<&OsStr> = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value),
            _ => Err(invalid_path("invalid relative component")),
        })
        .collect::<io::Result<_>>()?;
    let expected_device = if no_xdev {
        Some(metadata_for_fd(base)?.dev())
    } else {
        None
    };
    let mut current: Option<OwnedFd> = None;

    for (index, component) in components.iter().enumerate() {
        let parent = current.as_ref().unwrap_or(base).as_raw_fd();
        let name = c_string(component)?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if index + 1 < components.len() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let opened = unsafe { OwnedFd::from_raw_fd(fd) };
        if let Some(device) = expected_device
            && metadata_for_fd(&opened)?.dev() != device
        {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        current = Some(opened);
    }

    current.ok_or_else(|| invalid_path("empty relative path"))
}

fn metadata_for_fd(fd: &OwnedFd) -> io::Result<std::fs::Metadata> {
    let duplicated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = File::from(unsafe { OwnedFd::from_raw_fd(duplicated) });
    file.metadata()
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| invalid_path("path contains a NUL byte"))
}

fn invalid_path(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn opens_nested_and_single_file_shares() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        std::fs::create_dir_all(shared.join("nested")).unwrap();
        std::fs::write(shared.join("nested/report.txt"), b"report").unwrap();
        let single = files.join("pool/single.txt");
        std::fs::write(&single, b"single").unwrap();

        let mut nested =
            open_regular_beneath(&files, &shared, Path::new("nested/report.txt")).unwrap();
        let mut nested_bytes = Vec::new();
        nested.read_to_end(&mut nested_bytes).unwrap();
        assert_eq!(nested_bytes, b"report");

        let mut one = open_regular_beneath(&files, &single, Path::new("")).unwrap();
        let mut one_bytes = Vec::new();
        one.read_to_end(&mut one_bytes).unwrap();
        assert_eq!(one_bytes, b"single");
    }

    #[test]
    fn rejects_traversal_directories_and_outside_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("ok.txt"), b"ok").unwrap();

        assert!(open_regular_beneath(&files, &shared, Path::new("../ok.txt")).is_err());
        assert!(open_regular_beneath(&files, &shared, Path::new("/etc/passwd")).is_err());
        assert!(open_regular_beneath(&files, &shared, Path::new("")).is_err());
        assert!(open_regular_beneath(&files, tmp.path(), Path::new("outside")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_and_intermediate_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        symlink(outside.join("secret.txt"), shared.join("file-link")).unwrap();
        symlink(&outside, shared.join("dir-link")).unwrap();

        assert!(open_regular_beneath(&files, &shared, Path::new("file-link")).is_err());
        assert!(open_regular_beneath(&files, &shared, Path::new("dir-link/secret.txt")).is_err());
    }

    #[test]
    fn opened_descriptor_is_stable_after_path_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        std::fs::create_dir_all(&shared).unwrap();
        let target = shared.join("report.txt");
        std::fs::write(&target, b"original").unwrap();

        let mut opened = open_regular_beneath(&files, &shared, Path::new("report.txt")).unwrap();
        std::fs::rename(&target, shared.join("old-report.txt")).unwrap();
        std::fs::write(&target, b"replacement").unwrap();

        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_fifo_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        std::fs::create_dir_all(&shared).unwrap();
        let fifo = shared.join("pipe");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);

        assert!(open_regular_beneath(&files, &shared, Path::new("pipe")).is_err());
    }
}
