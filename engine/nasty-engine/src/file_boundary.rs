//! Descriptor-relative filesystem access for untrusted paths.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};
use std::sync::Arc;

pub enum BoundaryNode {
    File(File),
    Directory(BoundaryDirectory),
}

#[derive(Clone)]
pub struct BoundaryDirectory {
    fd: Arc<OwnedFd>,
    device: u64,
}

impl BoundaryDirectory {
    /// List names from the already-authorized directory descriptor. Returned
    /// names are untrusted and must be reopened relative to this descriptor.
    pub fn entry_names(&self, limit: usize) -> io::Result<Vec<OsString>> {
        let dot = c_string(OsStr::new("."))?;
        let directory = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if directory < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(directory) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(directory) };
            return Err(error);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            set_errno(0);
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = errno();
                return if error == 0 {
                    Ok(names)
                } else {
                    Err(io::Error::from_raw_os_error(error))
                };
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes != b"." && bytes != b".." {
                if names.len() >= limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory exceeds archive entry limit",
                    ));
                }
                names.push(OsStr::from_bytes(bytes).to_os_string());
            }
        }
    }

    /// Open one listed name without following symlinks or crossing a mount.
    pub fn open_child(&self, name: &OsStr) -> io::Result<BoundaryNode> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(invalid_path("child name must be one normal component"));
        }
        let fd = open_relative(&self.fd, Path::new(name), true)?;
        classify_node(fd, Some(self.device))
    }
}

impl BoundaryNode {
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        match self {
            Self::File(file) => file.metadata(),
            Self::Directory(directory) => metadata_for_fd(&directory.fd),
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Directory(_))
    }
}

/// A directory opened without following a symlink at any level. `join` names
/// a child through `/proc/self/fd`, so only that last name is looked up by
/// path: swapping an ancestor for a symlink afterwards cannot redirect it.
pub struct PinnedDirectory {
    #[cfg(target_os = "linux")]
    _fd: Arc<OwnedFd>,
    path: std::path::PathBuf,
}

impl PinnedDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &OsStr) -> std::path::PathBuf {
        self.path.join(name)
    }
}

/// Pin an absolute, already-canonical directory. Fails if any component is
/// now a symlink, i.e. the tree changed since the path was canonicalized.
#[cfg(target_os = "linux")]
pub fn pin_directory(dir: &Path) -> io::Result<PinnedDirectory> {
    match open_root_beneath(Path::new("/"), dir)? {
        BoundaryNode::Directory(directory) => Ok(PinnedDirectory {
            path: format!("/proc/self/fd/{}", directory.fd.as_raw_fd()).into(),
            _fd: directory.fd,
        }),
        BoundaryNode::File(_) => Err(invalid_path("not a directory")),
    }
}

// shortcut: no /proc magic links off Linux, so macOS dev builds act on the
// plain path without race protection; the engine only ships on Linux.
#[cfg(not(target_os = "linux"))]
pub fn pin_directory(dir: &Path) -> io::Result<PinnedDirectory> {
    Ok(PinnedDirectory {
        path: dir.to_path_buf(),
    })
}

/// Pin the parent of `path` and return the child path to act on. Keep the
/// returned directory alive while the child path is in use.
pub fn pin_parent(path: &Path) -> io::Result<(PinnedDirectory, std::path::PathBuf)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(invalid_path("path has no parent"));
    };
    let parent = pin_directory(parent)?;
    let child = parent.join(name);
    Ok((parent, child))
}

/// Copy a regular file without following a symlink at either end. With
/// `replace`, an existing regular file is overwritten in place (keeping its
/// owner); otherwise the destination must not exist (O_EXCL).
pub fn copy_regular_file(src: &Path, dst: &Path, replace: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut from = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)?;
    let meta = from.metadata()?;
    if !meta.is_file() {
        return Err(invalid_path("source is not a regular file"));
    }
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(!replace)
        .create(replace)
        .truncate(replace)
        .mode(meta.permissions().mode())
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dst)?;
    if !to.metadata()?.is_file() {
        return Err(invalid_path("destination is not a regular file"));
    }
    io::copy(&mut from, &mut to)?;
    // `mode` above is filtered by the umask; match std::fs::copy exactly.
    to.set_permissions(meta.permissions())
}

/// Open a shared file or directory as the root of a descriptor-relative walk.
pub fn open_root_beneath(files_root: &Path, shared_root: &Path) -> io::Result<BoundaryNode> {
    let shared_relative = shared_root
        .strip_prefix(files_root)
        .map_err(|_| invalid_path("shared root is outside the files root"))?;
    validate_relative(shared_relative, false)?;

    let files_fd = open_directory(files_root)?;
    let shared_fd = open_relative(&files_fd, shared_relative, false)?;
    classify_node(shared_fd, None)
}

pub fn open_directory_from_root(
    root: BoundaryNode,
    relative: &Path,
) -> io::Result<BoundaryDirectory> {
    validate_relative(relative, true)?;
    let mut directory = match root {
        BoundaryNode::Directory(directory) => directory,
        BoundaryNode::File(_) => return Err(invalid_path("shared root is not a directory")),
    };
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(invalid_path("invalid directory component"));
        };
        directory = match directory.open_child(name)? {
            BoundaryNode::Directory(child) => child,
            BoundaryNode::File(_) => return Err(invalid_path("target is not a directory")),
        };
    }
    Ok(directory)
}

/// Open a regular file under a configured shared root without ever resolving
/// and reopening a pathname. Symlinks are not followed at either level.
#[cfg(test)]
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

    open_regular_from_root_fd(shared_fd, shared_meta, relative)
}

pub fn open_regular_from_root(root: BoundaryNode, relative: &Path) -> io::Result<File> {
    validate_relative(relative, true)?;
    match root {
        BoundaryNode::File(file) if relative.as_os_str().is_empty() => Ok(file),
        BoundaryNode::File(_) => Err(invalid_path("relative path supplied for a file share")),
        BoundaryNode::Directory(_) if relative.as_os_str().is_empty() => {
            Err(invalid_path("target is not a regular file"))
        }
        BoundaryNode::Directory(directory) => {
            let target_fd = open_relative(&directory.fd, relative, true)?;
            readable_regular_file(target_fd, Some(directory.device))
        }
    }
}

#[cfg(test)]
fn open_regular_from_root_fd(
    shared_fd: OwnedFd,
    shared_meta: std::fs::Metadata,
    relative: &Path,
) -> io::Result<File> {
    if relative.as_os_str().is_empty() {
        return readable_regular_file(shared_fd, None);
    }
    if !shared_meta.is_dir() {
        return Err(invalid_path("relative path supplied for a file share"));
    }
    let target_fd = open_relative(&shared_fd, relative, true)?;
    readable_regular_file(target_fd, Some(shared_meta.dev()))
}

fn classify_node(fd: OwnedFd, expected_device: Option<u64>) -> io::Result<BoundaryNode> {
    let metadata = metadata_for_fd(&fd)?;
    if expected_device.is_some_and(|device| metadata.dev() != device) {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    if metadata.is_file() {
        return readable_regular_file(fd, expected_device).map(BoundaryNode::File);
    }
    if metadata.is_dir() {
        // Keep the O_PATH descriptor returned by openat2. It is already a
        // stable directory handle and is valid as a dirfd for the guarded
        // openat/openat2 calls below; reopening it through /proc/self/fd adds
        // a filesystem-specific failure point without strengthening the jail.
        return Ok(BoundaryNode::Directory(BoundaryDirectory {
            fd: Arc::new(fd),
            device: metadata.dev(),
        }));
    }
    Err(invalid_path("target is not a regular file or directory"))
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

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "linux")]
fn errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(target_os = "linux")]
fn set_errno(value: libc::c_int) {
    unsafe { *libc::__errno_location() = value };
}

#[cfg(not(target_os = "linux"))]
fn errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(not(target_os = "linux"))]
fn set_errno(value: libc::c_int) {
    unsafe { *libc::__error() = value };
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
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOSYS) {
            return open_relative_components(base, relative, no_xdev);
        }
        return Err(error);
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// macOS uses component-at-a-time `openat`. Linux uses the same guarded walk
/// only when `openat2` is unavailable (including seccomp returning ENOSYS).
#[cfg(not(target_os = "linux"))]
fn open_relative(base: &OwnedFd, relative: &Path, no_xdev: bool) -> io::Result<OwnedFd> {
    open_relative_components(base, relative, no_xdev)
}

fn open_relative_components(base: &OwnedFd, relative: &Path, no_xdev: bool) -> io::Result<OwnedFd> {
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
        #[cfg(target_os = "linux")]
        let mut flags = libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        #[cfg(not(target_os = "linux"))]
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

    #[test]
    fn directory_walk_stays_on_opened_root_after_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("report.txt"), b"original").unwrap();

        let BoundaryNode::Directory(opened) = open_root_beneath(&files, &shared).unwrap() else {
            panic!("shared root should be a directory");
        };
        std::fs::rename(&shared, files.join("pool/old-docs")).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("report.txt"), b"replacement").unwrap();
        std::fs::write(shared.join("new-secret.txt"), b"secret").unwrap();

        let names = opened.entry_names(10).unwrap();
        assert!(names.iter().any(|name| name == "report.txt"));
        assert!(!names.iter().any(|name| name == "new-secret.txt"));
        assert!(opened.entry_names(0).is_err());
        let BoundaryNode::File(mut report) = opened.open_child(OsStr::new("report.txt")).unwrap()
        else {
            panic!("report should be a regular file");
        };
        let mut bytes = Vec::new();
        report.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert!(opened.open_child(OsStr::new("../docs/report.txt")).is_err());
    }

    #[test]
    fn component_fallback_walks_from_stable_descriptors() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("fs");
        let shared = files.join("pool/docs");
        std::fs::create_dir_all(shared.join("nested")).unwrap();
        std::fs::write(shared.join("nested/report.txt"), b"report").unwrap();

        let files_fd = open_directory(&files).unwrap();
        let shared_fd = open_relative_components(&files_fd, Path::new("pool/docs"), false).unwrap();
        let root = classify_node(shared_fd, None).unwrap();
        let BoundaryNode::Directory(directory) = root else {
            panic!("shared root should be a directory");
        };
        assert!(
            directory
                .entry_names(10)
                .unwrap()
                .iter()
                .any(|name| name == "nested")
        );

        let mut report = open_regular_from_root(
            BoundaryNode::Directory(directory),
            Path::new("nested/report.txt"),
        )
        .unwrap();
        let mut bytes = Vec::new();
        report.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"report");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_parent_ignores_later_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let share = root.join("share");
        let outside = root.join("outside");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(share.join("old"), b"share").unwrap();
        std::fs::write(outside.join("old"), b"secret").unwrap();

        let (_parent, pinned) = pin_parent(&share.join("old")).unwrap();
        // Swap the checked folder for a symlink to somewhere else.
        std::fs::rename(&share, root.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &share).unwrap();

        std::fs::remove_file(&pinned).unwrap();
        assert!(!root.join("moved/old").exists());
        assert_eq!(std::fs::read(outside.join("old")).unwrap(), b"secret");
        // A fresh pin sees the symlink and refuses.
        assert!(pin_parent(&share.join("old")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn copy_regular_file_never_follows_or_clobbers_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let victim = tmp.path().join("victim");
        let link = tmp.path().join("link");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        assert!(copy_regular_file(&src, &link, false).is_err());
        assert!(copy_regular_file(&src, &link, true).is_err());
        assert!(copy_regular_file(&link, &tmp.path().join("out"), false).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");

        let dst = tmp.path().join("dst");
        copy_regular_file(&src, &dst, false).unwrap();
        assert!(copy_regular_file(&src, &dst, false).is_err());
        std::fs::write(&src, b"newer").unwrap();
        copy_regular_file(&src, &dst, true).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"newer");
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
