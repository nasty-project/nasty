//! Guest file sharing records, authenticated management, and public access
//! state.
//!
//! This module is the *spine* of #474: it persists a [`GuestShare`] per
//! share and exposes [`GuestShareService`] for the operator-only
//! `guestshare.*` RPCs and unauthenticated share handlers.
//!
//! Security shape, mirroring the issue's design notes:
//!   * The URL token IS the credential. Only its SHA-256 is stored, so a
//!     leak of `/var/lib/nasty/guest-shares` cannot reconstruct a working
//!     link. The plaintext token is returned from [`create`] exactly once.
//!   * Share paths are canonicalized and must resolve under `/fs` — the
//!     same guard NFS export creation uses (`nasty-sharing` `nfs.rs`).
//!   * Passwords reuse the login Argon2 hasher (`crate::auth`), so there is
//!     one crypto path, not two.
//!
//! [`create`]: GuestShareService::create

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

/// Where share records live, one JSON file per share keyed by UUID.
const STATE_DIR: &str = "/var/lib/nasty/guest-shares";
/// Root every shared path must canonicalize under.
const FILES_ROOT: &str = "/fs";

/// How long a password-unlock grant stays valid. Short by design — a guest
/// re-enters the password if it lapses (the share itself is long-lived; the
/// proof-of-password is ephemeral, mirroring a session).
const GRANT_TTL_SECS: i64 = 3600;
/// Failed unlock attempts (per IP+token) before the unlock endpoint locks
/// out, and the window those attempts are counted over.
const UNLOCK_MAX_FAILURES: usize = 10;
const UNLOCK_WINDOW_SECS: i64 = 15 * 60;
const UNLOCK_MAX_TRACKED_KEYS: usize = 4096;
const UNLOCK_REQUEST_MAX: usize = 30;
const UNLOCK_REQUEST_WINDOW_SECS: i64 = 60;
const UNLOCK_MAX_TRACKED_IPS: usize = 4096;
const MAX_CONCURRENT_PASSWORD_CHECKS: usize = 4;
const MAX_CONCURRENT_BROWSES: usize = 16;
const MAX_PUBLIC_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_PUBLIC_PATH_BYTES: usize = 4096;
const MAX_SHARE_ROOTS: usize = 32;
/// Bound descriptors held by guest downloads, including slow clients.
const MAX_CONCURRENT_DOWNLOADS: usize = 32;
/// ZIP compression is substantially heavier than streaming one file.
const MAX_CONCURRENT_ARCHIVES: usize = 4;
const MAX_ARCHIVE_ENTRIES: usize = 50_000;
const MAX_ARCHIVE_DIRECTORY_ENTRIES: usize = 25_000;
const MAX_ARCHIVE_DEPTH: usize = 64;
const MAX_ARCHIVE_PATH_BYTES: usize = u16::MAX as usize;
const MAX_ARCHIVE_TOTAL_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_DURATION_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum GuestShareError {
    #[error("share not found: {0}")]
    NotFound(String),
    #[error("share must be revoked before it can be removed: {0}")]
    NotRevoked(String),
    #[error("no paths supplied")]
    NoPaths,
    #[error("too many paths supplied")]
    TooManyPaths,
    #[error("path does not exist: {0}")]
    PathNotFound(String),
    #[error("path is not within a NASty filesystem: {0}")]
    PathNotInFilesystem(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error("password verification is busy")]
    PasswordCheckBusy,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A guest share record. Persisted verbatim; the plaintext token never is.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GuestShare {
    /// Unique share identifier (UUID) — also the on-disk filename.
    pub id: String,
    /// SHA-256 (lowercase hex) of the URL token. The link is the
    /// credential; storing only its hash means a state-file leak leaks no
    /// working links.
    pub token_hash: String,
    /// Absolute, canonicalized paths being shared. Each is under `/fs`.
    pub paths: Vec<String>,
    /// Username of the operator/admin who created the share.
    pub created_by: String,
    /// Unix seconds at creation.
    pub created_at: i64,
    /// Unix seconds after which the share stops working (enforced by the
    /// public surface in a later PR). `None` = never expires.
    pub expires_at: Option<i64>,
    /// Argon2 hash of the share password (same hasher as login). `None` =
    /// no password.
    pub password_hash: Option<String>,
    /// Maximum number of downloads before the share stops working. `None` = unlimited.
    pub max_downloads: Option<u32>,
    /// Downloads served so far.
    pub downloads: u32,
    /// Metadata views so far.
    pub views: u32,
    /// Whether the share has been revoked. Revoked records are kept (not
    /// deleted) so history/audit survive.
    pub revoked: bool,
    /// Soft "removed" — hidden from the default management list while the
    /// record is kept on disk for audit/history. Only a *revoked* share can
    /// be hidden (the UI revokes first). `#[serde(default)]` so shares
    /// written before this field load as not-hidden.
    #[serde(default)]
    pub hidden: bool,
    /// Optional free-text note for the management UI.
    pub note: Option<String>,
}

impl HasId for GuestShare {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateGuestShareRequest {
    /// One or more absolute paths to share. Each must exist and resolve
    /// under `/fs`.
    pub paths: Vec<String>,
    /// Optional expiry, Unix seconds.
    pub expires_at: Option<i64>,
    /// Optional password. When present, hashed with the login Argon2.
    pub password: Option<String>,
    /// Optional download cap.
    pub max_downloads: Option<u32>,
    /// Optional free-text note.
    pub note: Option<String>,
}

/// Result of [`GuestShareService::create`]. Carries the plaintext token
/// **once** — it is never stored and never returned by `list`/`get`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CreateGuestShareResult {
    pub share: GuestShareInfo,
    /// Plaintext URL token. Show it to the operator now; it cannot be
    /// recovered later (only its hash is persisted).
    pub token: String,
}

/// Redacted management view. Persisted path and credential material never
/// crosses the RPC boundary.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GuestShareInfo {
    pub id: String,
    pub names: Vec<String>,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub password_protected: bool,
    pub max_downloads: Option<u32>,
    pub downloads: u32,
    pub views: u32,
    pub revoked: bool,
    pub hidden: bool,
    pub note: Option<String>,
}

impl From<&GuestShare> for GuestShareInfo {
    fn from(share: &GuestShare) -> Self {
        Self {
            id: share.id.clone(),
            names: share
                .paths
                .iter()
                .map(|path| {
                    Path::new(path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("file")
                        .to_string()
                })
                .collect(),
            created_by: share.created_by.clone(),
            created_at: share.created_at,
            expires_at: share.expires_at,
            password_protected: share.password_hash.is_some(),
            max_downloads: share.max_downloads,
            downloads: share.downloads,
            views: share.views,
            revoked: share.revoked,
            hidden: share.hidden,
            note: share.note.clone(),
        }
    }
}

/// One entry shown to a guest on the public share page.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PublicEntry {
    pub root: usize,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PublicDirectoryEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PublicDirectoryListing {
    pub root: usize,
    pub path: String,
    pub entries: Vec<PublicDirectoryEntry>,
}

/// Public metadata for a share — deliberately minimal and leaks no absolute
/// server paths. Returned only for shares that exist and are still active.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PublicShareMeta {
    pub entries: Vec<PublicEntry>,
    pub password_required: bool,
    pub unlocked: bool,
    pub expires_at: Option<i64>,
}

/// Whether a share may still be served: not revoked, not past expiry, not
/// over its download cap. Every "no" collapses to the same caller response,
/// so a guesser can't tell *why* a token is unavailable.
fn is_accessible(s: &GuestShare, now: i64) -> bool {
    !s.revoked
        && s.expires_at.is_none_or(|e| now < e)
        && s.max_downloads.is_none_or(|m| s.downloads < m)
}

pub struct OpenedGuestFile {
    file: File,
    name: String,
    size: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl OpenedGuestFile {
    pub fn into_parts(self) -> (GuestDownloadReader, String, u64) {
        let reader = GuestDownloadReader {
            file: tokio::fs::File::from_std(self.file),
            _permit: self.permit,
        };
        (reader, self.name, self.size)
    }
}

pub struct GuestDownloadReader {
    file: tokio::fs::File,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl tokio::io::AsyncRead for GuestDownloadReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.file).poll_read(cx, buf)
    }
}

pub struct OpenedGuestArchive {
    roots: Vec<PreparedArchiveRoot>,
    permit: tokio::sync::OwnedSemaphorePermit,
}

struct PreparedArchiveRoot {
    node: crate::file_boundary::BoundaryNode,
    name: String,
}

impl OpenedGuestArchive {
    pub fn into_stream(self) -> GuestArchiveReader {
        let (reader, writer) = tokio::io::duplex(64 * 1024);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer_cancelled = Arc::clone(&cancelled);
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let result = runtime.block_on(tokio::time::timeout(
                std::time::Duration::from_secs(MAX_ARCHIVE_DURATION_SECS),
                write_share_zip(writer, self.roots, self.permit, producer_cancelled),
            ));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    // The client gets a truncated archive; nothing else we can
                    // do once the response body has started streaming.
                    tracing::warn!("guest share zip stream aborted: {error}");
                }
                Err(_) => tracing::warn!("guest share zip stream reached its time limit"),
            }
        });
        GuestArchiveReader { reader, cancelled }
    }
}

pub struct GuestArchiveReader {
    reader: tokio::io::DuplexStream,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl tokio::io::AsyncRead for GuestArchiveReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl Drop for GuestArchiveReader {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

enum ArchiveWork {
    Node {
        node: crate::file_boundary::BoundaryNode,
        name: String,
        depth: usize,
    },
    Directory {
        directory: crate::file_boundary::BoundaryDirectory,
        prefix: String,
        names: std::vec::IntoIter<std::ffi::OsString>,
        depth: usize,
    },
}

/// Write a ZIP of every `root` into `writer`, entries named relative to each
/// root's parent (so a share of `/fs/tank/photos` yields `photos/img.jpg`).
/// Every listed name is reopened relative to its authorized parent descriptor.
async fn write_share_zip(
    writer: tokio::io::DuplexStream,
    roots: Vec<PreparedArchiveRoot>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use async_zip::{Compression, ZipEntryBuilder};
    use futures_util::io::AsyncReadExt;
    use tokio_util::compat::TokioAsyncReadCompatExt;

    let mut zip = async_zip::tokio::write::ZipFileWriter::with_tokio(writer);
    let mut entry_count = 0usize;
    let mut total_path_bytes = 0usize;
    let mut total_size = 0u64;

    for root in roots {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        entry_count = entry_count
            .checked_add(1)
            .filter(|count| *count <= MAX_ARCHIVE_ENTRIES)
            .ok_or_else(|| archive_limit("archive entry limit exceeded"))?;
        validate_zip_path(&root.name)?;
        let mut stack = vec![ArchiveWork::Node {
            node: root.node,
            name: root.name,
            depth: 0,
        }];

        while let Some(work) = stack.pop() {
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            match work {
                ArchiveWork::Node {
                    node: crate::file_boundary::BoundaryNode::File(file),
                    name,
                    ..
                } => {
                    let size = file.metadata()?.len();
                    total_path_bytes = total_path_bytes
                        .checked_add(name.len())
                        .filter(|total| *total <= MAX_ARCHIVE_TOTAL_PATH_BYTES)
                        .ok_or_else(|| archive_limit("archive path metadata limit exceeded"))?;
                    total_size = total_size
                        .checked_add(size)
                        .filter(|total| *total <= MAX_ARCHIVE_UNCOMPRESSED_BYTES)
                        .ok_or_else(|| archive_limit("archive byte limit exceeded"))?;
                    let builder = ZipEntryBuilder::new(name.into(), Compression::Deflate);
                    let mut entry = zip.write_entry_stream(builder).await?;
                    let mut file = tokio::fs::File::from_std(file).compat().take(size);
                    futures_util::io::copy(&mut file, &mut entry).await?;
                    entry.close().await?;
                }
                ArchiveWork::Node {
                    node: crate::file_boundary::BoundaryNode::Directory(directory),
                    name,
                    depth,
                } => {
                    let remaining = MAX_ARCHIVE_ENTRIES - entry_count;
                    let limit = remaining.min(MAX_ARCHIVE_DIRECTORY_ENTRIES);
                    let names = directory.entry_names(limit)?;
                    entry_count += names.len();
                    stack.push(ArchiveWork::Directory {
                        directory,
                        prefix: name,
                        names: names.into_iter(),
                        depth,
                    });
                }
                ArchiveWork::Directory {
                    directory,
                    prefix,
                    mut names,
                    depth,
                } => {
                    let Some(child_name) = names.next() else {
                        continue;
                    };
                    let child_depth = depth + 1;
                    if child_depth > MAX_ARCHIVE_DEPTH {
                        return Err(archive_limit("archive depth limit exceeded").into());
                    }
                    let archive_name = format!("{prefix}/{}", safe_zip_component(&child_name));
                    validate_zip_path(&archive_name)?;
                    stack.push(ArchiveWork::Directory {
                        directory: directory.clone(),
                        prefix,
                        names,
                        depth,
                    });
                    if let Ok(node) = directory.open_child(&child_name) {
                        stack.push(ArchiveWork::Node {
                            node,
                            name: archive_name,
                            depth: child_depth,
                        });
                    }
                }
            }
        }
    }

    zip.close().await?;
    Ok(())
}

fn safe_zip_component(name: &std::ffi::OsStr) -> String {
    const RAW_PREFIX: &str = "__nasty_raw_";

    let mut value = if let Some(name) = name.to_str() {
        let mut value = String::new();
        for character in name.chars() {
            if character == '/'
                || character == '\\'
                || character == ':'
                || character == '%'
                || character.is_control()
            {
                let mut bytes = [0; 4];
                for byte in character.encode_utf8(&mut bytes).as_bytes() {
                    value.push_str(&format!("%{byte:02X}"));
                }
            } else {
                value.push(character);
            }
        }
        if value.starts_with(RAW_PREFIX) {
            value.replace_range(..1, "%5F");
        }
        value
    } else {
        let mut value = RAW_PREFIX.to_string();
        for byte in name.as_bytes() {
            value.push_str(&format!("{byte:02X}"));
        }
        value
    };
    if value.is_empty() {
        value = "file".to_string();
    } else if matches!(value.as_str(), "." | "..") {
        value.replace_range(..1, "%2E");
    }
    value
}

fn unique_zip_root_name(name: String, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(name.clone()) {
        return name;
    }
    for suffix in 2.. {
        let candidate = format!("{name} ({suffix})");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded suffix search must find an unused root name")
}

fn validate_zip_path(path: &str) -> std::io::Result<()> {
    if path.len() > MAX_ARCHIVE_PATH_BYTES {
        Err(archive_limit("archive path length limit exceeded"))
    } else {
        Ok(())
    }
}

fn archive_limit(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reject control characters, then canonicalize and confirm the path stays
/// under `root` (resolving `.`/`..`/symlinks). Returns the canonical path.
///
/// `Path::starts_with` is component-wise, so a sibling like `/fsxyz` does
/// not match the `/fs` root.
fn canonicalize_under(root: &Path, requested: &str) -> Result<String, GuestShareError> {
    if requested
        .chars()
        .any(|c| c.is_control() || matches!(c, '\t' | '\n' | '\r' | '"' | '\'' | '\\'))
    {
        return Err(GuestShareError::InvalidPath(requested.to_string()));
    }
    let canonical = std::fs::canonicalize(requested)
        .map_err(|_| GuestShareError::PathNotFound(requested.to_string()))?;
    if !canonical.starts_with(root) {
        return Err(GuestShareError::PathNotInFilesystem(requested.to_string()));
    }
    if canonical == root {
        return Err(GuestShareError::InvalidPath(requested.to_string()));
    }
    let metadata = std::fs::metadata(&canonical)
        .map_err(|_| GuestShareError::PathNotFound(requested.to_string()))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(GuestShareError::InvalidPath(requested.to_string()));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn public_relative_path(requested: &str) -> Option<(PathBuf, String)> {
    if requested.len() > MAX_PUBLIC_PATH_BYTES
        || requested
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '\'' | '\\'))
    {
        return None;
    }
    let path = Path::new(requested);
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return None;
        };
        let name = name.to_str()?;
        if !public_name_is_supported(name) {
            return None;
        }
        normalized.push(name);
        names.push(name);
    }
    Some((normalized, names.join("/")))
}

fn public_name_is_supported(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '\'' | '\\'))
}

/// A live password-unlock grant: proof a guest entered the right password
/// for `share_id`, valid until `expires_at` (Unix seconds).
struct GrantEntry {
    share_id: String,
    expires_at: i64,
}

struct RateLimitEntry {
    timestamps: Vec<i64>,
}

#[derive(Default)]
struct BoundedRateLimiter {
    entries: HashMap<String, RateLimitEntry>,
    last_sweep_at: Option<i64>,
}

impl BoundedRateLimiter {
    fn reserve(
        &mut self,
        key: String,
        now: i64,
        window_secs: i64,
        max_attempts: usize,
        max_keys: usize,
    ) -> bool {
        if self.last_sweep_at.is_some_and(|last| now < last) {
            self.entries.clear();
            self.last_sweep_at = Some(now);
        }
        if self
            .last_sweep_at
            .is_none_or(|last| now - last >= window_secs)
        {
            self.sweep_expired(now, window_secs);
        }

        if let Some(entry) = self.entries.get_mut(&key) {
            entry
                .timestamps
                .retain(|&timestamp| now >= timestamp && now - timestamp < window_secs);
            if entry.timestamps.len() >= max_attempts {
                return false;
            }
            entry.timestamps.push(now);
            return true;
        }

        if self.entries.len() >= max_keys {
            // At capacity, sweep at most once per second. Repeated rejected
            // traffic stays O(1), while expired entries are reclaimed quickly.
            if self.last_sweep_at.is_none_or(|last| now - last >= 1) {
                self.sweep_expired(now, window_secs);
            }
            if self.entries.len() >= max_keys {
                return false;
            }
        }
        self.entries.insert(
            key,
            RateLimitEntry {
                timestamps: vec![now],
            },
        );
        true
    }

    fn clear(&mut self, key: &str) {
        self.entries.remove(key);
    }

    fn sweep_expired(&mut self, now: i64, window_secs: i64) {
        self.entries.retain(|_, entry| {
            entry
                .timestamps
                .last()
                .is_some_and(|&last| now >= last && now - last < window_secs)
        });
        self.last_sweep_at = Some(now);
    }
}

/// Operator-facing guest-share store + the ephemeral state the public
/// access surface needs: password-unlock grants and an unlock rate-limiter.
///
/// Grants and the rate-limiter live in memory only — they're intentionally
/// ephemeral (a restart just makes guests re-enter the password) and never
/// touch disk, so a state-file leak exposes neither.
pub struct GuestShareService {
    dir: PathBuf,
    fs_root: PathBuf,
    /// grant token -> what it unlocks. Opaque random tokens, exactly like
    /// the engine's session model.
    grants: StdMutex<HashMap<String, GrantEntry>>,
    /// Bounded IP+token password-attempt windows.
    unlock_failures: StdMutex<BoundedRateLimiter>,
    /// Bounded IP request windows. This gate runs before token lookup so
    /// random-token traffic cannot force unbounded state scans.
    unlock_requests: StdMutex<BoundedRateLimiter>,
    /// Serializes every record load-modify-save operation. Using separate
    /// locks for counters and revocation would let stale writers resurrect a share.
    state_lock: Arc<tokio::sync::Mutex<()>>,
    /// Argon2 is intentionally expensive. Keep verification off async worker
    /// threads and cap global concurrency to prevent unlock CPU exhaustion.
    password_checks: Arc<tokio::sync::Semaphore>,
    /// Bounds guest descriptors for their full streaming lifetime.
    active_downloads: Arc<tokio::sync::Semaphore>,
    /// Bounds CPU-heavy archive producers for their full streaming lifetime.
    active_archives: Arc<tokio::sync::Semaphore>,
    /// Bounds descriptor walks triggered by guest navigation and metadata.
    browse_operations: Arc<tokio::sync::Semaphore>,
}

impl Default for GuestShareService {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestShareService {
    pub fn new() -> Self {
        Self::with_dirs(PathBuf::from(STATE_DIR), PathBuf::from(FILES_ROOT))
    }

    /// Test seam: store records under `dir` and require paths under `fs_root`.
    fn with_dirs(dir: PathBuf, fs_root: PathBuf) -> Self {
        Self {
            dir,
            fs_root,
            grants: StdMutex::new(HashMap::new()),
            unlock_failures: StdMutex::new(BoundedRateLimiter::default()),
            unlock_requests: StdMutex::new(BoundedRateLimiter::default()),
            state_lock: Arc::new(tokio::sync::Mutex::new(())),
            password_checks: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS)),
            active_downloads: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
            active_archives: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_ARCHIVES)),
            browse_operations: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_BROWSES)),
        }
    }

    fn state_dir(&self) -> StateDir {
        StateDir::new(self.dir.clone())
    }

    /// Keep the lock inside an independently owned task so cancellation of an
    /// HTTP request cannot release it while Tokio filesystem work continues.
    async fn mutate_state<T, F, Fut>(&self, operation: F) -> Result<T, GuestShareError>
    where
        T: Send + 'static,
        F: FnOnce(StateDir) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, GuestShareError>> + Send + 'static,
    {
        let state_lock = Arc::clone(&self.state_lock);
        let state_dir = self.state_dir();
        tokio::spawn(async move {
            let _guard = state_lock.lock().await;
            operation(state_dir).await
        })
        .await
        .map_err(|error| GuestShareError::Io(std::io::Error::other(error)))?
    }

    /// List every share (including revoked ones). Never includes a
    /// plaintext token — only the hash is persisted.
    pub async fn list(&self) -> Result<Vec<GuestShare>, GuestShareError> {
        Ok(self.state_dir().load_all().await)
    }

    /// Fetch a single share by id.
    pub async fn get(&self, id: &str) -> Result<GuestShare, GuestShareError> {
        self.state_dir()
            .load::<GuestShare>(id)
            .await
            .ok_or_else(|| GuestShareError::NotFound(id.to_string()))
    }

    /// Create a share. Validates every path under `/fs`, mints a token,
    /// stores only its hash, and returns the plaintext token once.
    pub async fn create(
        &self,
        req: CreateGuestShareRequest,
        created_by: &str,
    ) -> Result<CreateGuestShareResult, GuestShareError> {
        if req.paths.is_empty() {
            return Err(GuestShareError::NoPaths);
        }
        if req.paths.len() > MAX_SHARE_ROOTS {
            return Err(GuestShareError::TooManyPaths);
        }
        let mut canonical_paths = Vec::with_capacity(req.paths.len());
        for p in &req.paths {
            canonical_paths.push(canonicalize_under(&self.fs_root, p)?);
        }
        let password_hash = match req.password.as_deref() {
            Some(pw) if !pw.is_empty() => Some(
                crate::auth::hash_password(pw).map_err(|e| GuestShareError::Hash(e.to_string()))?,
            ),
            _ => None,
        };

        let token = crate::auth::generate_token();
        let share = GuestShare {
            id: Uuid::new_v4().to_string(),
            token_hash: sha256_hex(&token),
            paths: canonical_paths,
            created_by: created_by.to_string(),
            created_at: now_secs(),
            expires_at: req.expires_at,
            password_hash,
            max_downloads: req.max_downloads,
            downloads: 0,
            views: 0,
            revoked: false,
            hidden: false,
            note: req.note,
        };

        let saved_share = share.clone();
        self.mutate_state(move |state_dir| async move {
            state_dir.save(&saved_share.id, &saved_share).await?;
            Ok(())
        })
        .await?;
        let info = GuestShareInfo::from(&share);
        Ok(CreateGuestShareResult { share: info, token })
    }

    /// Revoke a share. The record is kept (marked `revoked`) so history and
    /// audit survive — only the public surface stops honoring it.
    pub async fn revoke(&self, id: &str) -> Result<GuestShare, GuestShareError> {
        let id = id.to_string();
        self.mutate_state(move |state_dir| async move {
            let mut share = state_dir
                .load::<GuestShare>(&id)
                .await
                .ok_or_else(|| GuestShareError::NotFound(id.clone()))?;
            if !share.revoked {
                share.revoked = true;
                state_dir.save(&share.id, &share).await?;
            }
            Ok(share)
        })
        .await
    }

    /// Soft-remove a share: hide it from the default management list while
    /// keeping the record on disk for audit/history. Only a *revoked* share
    /// can be removed — the UI revokes first, so a live link is never
    /// silently dropped. Idempotent.
    pub async fn remove(&self, id: &str) -> Result<(), GuestShareError> {
        let id = id.to_string();
        self.mutate_state(move |state_dir| async move {
            let mut share = state_dir
                .load::<GuestShare>(&id)
                .await
                .ok_or_else(|| GuestShareError::NotFound(id.clone()))?;
            if !share.revoked {
                return Err(GuestShareError::NotRevoked(id));
            }
            if !share.hidden {
                share.hidden = true;
                state_dir.save(&share.id, &share).await?;
            }
            Ok(())
        })
        .await
    }

    // ── Public access surface ───────────────────────────────────────────
    // These back the unauthenticated `/api/public/share/*` HTTP handlers.

    /// Resolve a URL token to its share, but only if the share is still
    /// active. Returns `None` for unknown / expired / revoked / exhausted
    /// tokens alike — the caller turns every case into the same generic
    /// "not available", giving a token-guesser no oracle.
    pub async fn lookup_active(&self, token: &str, now: i64) -> Option<GuestShare> {
        let hash = sha256_hex(token);
        self.state_dir()
            .load_all::<GuestShare>()
            .await
            .into_iter()
            .find(|share| share.token_hash == hash && is_accessible(share, now))
    }

    /// Public metadata for the guest landing page.
    pub async fn meta(&self, share: &GuestShare, unlocked: bool) -> Option<PublicShareMeta> {
        let password_required = share.password_hash.is_some();
        if password_required && !unlocked {
            return Some(PublicShareMeta {
                entries: Vec::new(),
                password_required,
                unlocked: false,
                expires_at: share.expires_at,
            });
        }
        if share.paths.len() > MAX_SHARE_ROOTS {
            return None;
        }
        let permit = self.browse_operations.clone().try_acquire_owned().ok()?;
        let files_root = self.fs_root.clone();
        let paths = share.paths.clone();
        let expires_at = share.expires_at;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let entries = paths
                .into_iter()
                .enumerate()
                .filter_map(|(root, path)| {
                    let path = PathBuf::from(path);
                    let node = crate::file_boundary::open_root_beneath(&files_root, &path).ok()?;
                    let metadata = node.metadata().ok()?;
                    Some(PublicEntry {
                        root,
                        name: path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("file")
                            .to_string(),
                        is_dir: node.is_directory(),
                        size: if metadata.is_file() {
                            metadata.len()
                        } else {
                            0
                        },
                    })
                })
                .collect();
            PublicShareMeta {
                entries,
                password_required,
                unlocked: true,
                expires_at,
            }
        })
        .await
        .ok()
    }

    /// List one guest-visible directory through retained descriptors.
    pub async fn browse_directory(
        &self,
        share: &GuestShare,
        root: usize,
        requested: &str,
    ) -> Option<PublicDirectoryListing> {
        let (relative, path) = public_relative_path(requested)?;
        let shared_root = PathBuf::from(share.paths.get(root)?);
        let permit = self.browse_operations.clone().try_acquire_owned().ok()?;
        let files_root = self.fs_root.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let root_node =
                crate::file_boundary::open_root_beneath(&files_root, &shared_root).ok()?;
            let directory =
                crate::file_boundary::open_directory_from_root(root_node, &relative).ok()?;
            let names = directory.entry_names(MAX_PUBLIC_DIRECTORY_ENTRIES).ok()?;
            let mut entries = Vec::with_capacity(names.len());
            for name in names {
                let Some(name) = name.to_str().filter(|name| public_name_is_supported(name)) else {
                    continue;
                };
                let Ok(node) = directory.open_child(std::ffi::OsStr::new(name)) else {
                    continue;
                };
                let Ok(metadata) = node.metadata() else {
                    continue;
                };
                entries.push(PublicDirectoryEntry {
                    name: name.to_string(),
                    is_dir: node.is_directory(),
                    size: if metadata.is_file() {
                        metadata.len()
                    } else {
                        0
                    },
                });
            }
            entries.sort_by(|left, right| {
                right.is_dir.cmp(&left.is_dir).then_with(|| {
                    left.name
                        .to_lowercase()
                        .cmp(&right.name.to_lowercase())
                        .then_with(|| left.name.cmp(&right.name))
                })
            });
            Some(PublicDirectoryListing {
                root,
                path,
                entries,
            })
        })
        .await
        .ok()
        .flatten()
    }

    /// Open a guest file beneath one of the share roots. The returned
    /// descriptor is the object that was authorized; callers never reopen a
    /// validated pathname.
    pub async fn open_download(
        &self,
        share: &GuestShare,
        root: Option<usize>,
        rel: &str,
    ) -> Option<OpenedGuestFile> {
        let (relative, _) = public_relative_path(rel)?;
        let permit = self.active_downloads.clone().try_acquire_owned().ok()?;
        let files_root = self.fs_root.clone();
        let roots = match root {
            Some(root) => vec![share.paths.get(root)?.clone()],
            None => share.paths.clone(),
        };
        tokio::task::spawn_blocking(move || {
            for root in roots {
                let root = PathBuf::from(root);
                let Ok(node) = crate::file_boundary::open_root_beneath(&files_root, &root) else {
                    continue;
                };
                let Ok(file) = crate::file_boundary::open_regular_from_root(node, &relative) else {
                    continue;
                };
                let size = file.metadata().ok()?.len();
                let name = if relative.as_os_str().is_empty() {
                    root.file_name()
                } else {
                    relative.file_name()
                }
                .and_then(|name| name.to_str())
                .unwrap_or("file")
                .to_string();
                return Some(OpenedGuestFile {
                    file,
                    name,
                    size,
                    permit,
                });
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// Whether `share` is password-protected.
    pub fn needs_password(share: &GuestShare) -> bool {
        share.password_hash.is_some()
    }

    /// Verify a guest-supplied password against the share. `true` when the
    /// share has no password (nothing to prove).
    pub async fn verify_share_password(
        &self,
        share: &GuestShare,
        password: &str,
    ) -> Result<bool, GuestShareError> {
        let Some(hash) = share.password_hash.clone() else {
            return Ok(true);
        };
        let permit = self
            .password_checks
            .clone()
            .try_acquire_owned()
            .map_err(|_| GuestShareError::PasswordCheckBusy)?;
        let password = password.to_string();
        Ok(tokio::task::spawn_blocking(move || {
            // The worker owns the permit so cancellation of the HTTP future
            // cannot release capacity while Argon2 continues in the background.
            let _permit = permit;
            crate::auth::verify_password(&password, &hash).is_ok()
        })
        .await
        .unwrap_or(false))
    }

    /// Count a metadata view. Best-effort; a lost increment is harmless.
    pub async fn record_view(&self, id: &str) {
        // Never queue public view writes ahead of revoke. At most one disk
        // mutation is delayed by a view; contended increments are dropped.
        let Ok(guard) = Arc::clone(&self.state_lock).try_lock_owned() else {
            return;
        };
        let state_dir = self.state_dir();
        let id = id.to_string();
        let _ = tokio::spawn(async move {
            let _guard = guard;
            if let Some(mut share) = state_dir.load::<GuestShare>(&id).await {
                share.views = share.views.saturating_add(1);
                let _ = state_dir.save(&id, &share).await;
            }
        })
        .await;
    }

    async fn register_download_holding<T>(
        &self,
        id: &str,
        now: i64,
        held: T,
    ) -> Result<T, GuestShareError>
    where
        T: Send + 'static,
    {
        let id = id.to_string();
        self.mutate_state(move |state_dir| async move {
            let mut share = state_dir
                .load::<GuestShare>(&id)
                .await
                .ok_or_else(|| GuestShareError::NotFound(id.clone()))?;
            if !is_accessible(&share, now) {
                return Err(GuestShareError::NotFound(id));
            }
            share.downloads = share.downloads.saturating_add(1);
            state_dir.save(&share.id, &share).await?;
            Ok(held)
        })
        .await
    }

    /// Count a download, enforcing `max_downloads`. Serialized with every
    /// other record mutation so counters cannot overwrite revocation.
    /// Returns `Err(NotFound)` if the share went inactive between lookup and
    /// here — the handler maps that to the same generic "not available".
    pub async fn register_download(&self, id: &str, now: i64) -> Result<(), GuestShareError> {
        self.register_download_holding(id, now, ()).await
    }

    /// Register a single-file download while retaining its descriptor slot.
    /// If the request is cancelled, the detached state mutation remains
    /// bounded by the same semaphore as active streams.
    pub async fn register_opened_download(
        &self,
        id: &str,
        now: i64,
        opened: OpenedGuestFile,
    ) -> Result<OpenedGuestFile, GuestShareError> {
        self.register_download_holding(id, now, opened).await
    }

    /// Open and retain every archive root before registering the download.
    pub async fn prepare_archive(&self, share: &GuestShare) -> Option<OpenedGuestArchive> {
        let permit = self.active_archives.clone().try_acquire_owned().ok()?;
        if share.paths.is_empty() || share.paths.len() > MAX_SHARE_ROOTS {
            return None;
        }
        let files_root = self.fs_root.clone();
        let paths = share.paths.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<OpenedGuestArchive> {
            let mut root_names = std::collections::HashSet::new();
            let roots = paths
                .into_iter()
                .map(|path| {
                    let path = PathBuf::from(path);
                    let name = unique_zip_root_name(
                        safe_zip_component(
                            path.file_name()
                                .unwrap_or_else(|| std::ffi::OsStr::new("share")),
                        ),
                        &mut root_names,
                    );
                    let node = crate::file_boundary::open_root_beneath(&files_root, &path)?;
                    Ok(PreparedArchiveRoot { node, name })
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            Ok(OpenedGuestArchive { roots, permit })
        })
        .await
        .ok()?
        .ok()
    }

    /// Register an archive while retaining its stream slot through persistence.
    pub async fn register_opened_archive(
        &self,
        id: &str,
        now: i64,
        archive: OpenedGuestArchive,
    ) -> Result<OpenedGuestArchive, GuestShareError> {
        self.register_download_holding(id, now, archive).await
    }

    /// A filename for the downloaded archive, derived from the first shared
    /// root's basename (e.g. "photos.zip"). Quotes are stripped so it's safe
    /// in a `Content-Disposition` header.
    pub fn zip_filename(share: &GuestShare) -> String {
        let base = share
            .paths
            .first()
            .map(Path::new)
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("share")
            .replace(['"', '\\'], "");
        format!("{base}.zip")
    }

    // ── Password-unlock grants (ephemeral, in-memory) ───────────────────

    /// Mint a grant proving the guest unlocked `share_id`. Opportunistically
    /// prunes expired grants. The returned opaque token goes in a cookie.
    pub fn mint_grant(&self, share_id: &str, now: i64) -> String {
        let token = crate::auth::generate_token();
        let mut grants = self.grants.lock().unwrap();
        grants.retain(|_, e| e.expires_at > now);
        grants.insert(
            token.clone(),
            GrantEntry {
                share_id: share_id.to_string(),
                expires_at: now + GRANT_TTL_SECS,
            },
        );
        token
    }

    /// Whether `grant` is a live unlock for `share_id`.
    pub fn check_grant(&self, grant: &str, share_id: &str, now: i64) -> bool {
        let mut grants = self.grants.lock().unwrap();
        grants.retain(|_, e| e.expires_at > now);
        grants
            .get(grant)
            .is_some_and(|e| e.share_id == share_id && e.expires_at > now)
    }

    // ── Unlock rate-limiting (per IP+token sliding window) ──────────────

    /// Reserve one request before token lookup. Returns false when this IP is
    /// over limit or the bounded tracker is saturated by other active IPs.
    pub fn allow_unlock_request(&self, ip: &str, now: i64) -> bool {
        let mut requests = self.unlock_requests.lock().unwrap();
        requests.reserve(
            ip.to_string(),
            now,
            UNLOCK_REQUEST_WINDOW_SECS,
            UNLOCK_REQUEST_MAX,
            UNLOCK_MAX_TRACKED_IPS,
        )
    }

    /// Atomically reserve one password verification for this IP and share
    /// token. Successful verification clears the history; failures remain.
    pub fn reserve_unlock_attempt(&self, ip: &str, token: &str, now: i64) -> bool {
        let key = unlock_failure_key(ip, token);
        let mut m = self.unlock_failures.lock().unwrap();
        m.reserve(
            key,
            now,
            UNLOCK_WINDOW_SECS,
            UNLOCK_MAX_FAILURES,
            UNLOCK_MAX_TRACKED_KEYS,
        )
    }

    /// Clear the failure counter for this (ip, token) after a success.
    pub fn clear_unlock_failures(&self, ip: &str, token: &str) {
        let key = unlock_failure_key(ip, token);
        self.unlock_failures.lock().unwrap().clear(&key);
    }
}

fn unlock_failure_key(ip: &str, token: &str) -> String {
    format!("{ip}|{}", sha256_hex(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(tmp: &std::path::Path) -> (GuestShareService, PathBuf) {
        let state = tmp.join("state");
        std::fs::create_dir_all(tmp.join("fs")).unwrap();
        // Canonicalized so the symlinked-`/var` tempdir on macOS matches
        // what `canonicalize_under` resolves to (a test-only concern; `/fs`
        // is canonical on the appliance).
        let fs_root = std::fs::canonicalize(tmp.join("fs")).unwrap();
        (
            GuestShareService::with_dirs(state, fs_root.clone()),
            fs_root,
        )
    }

    #[test]
    fn sha256_is_stable_and_distinct() {
        assert_eq!(sha256_hex("hello"), sha256_hex("hello"));
        assert_ne!(sha256_hex("hello"), sha256_hex("hellp"));
        // 32 bytes -> 64 hex chars.
        assert_eq!(sha256_hex("anything").len(), 64);
    }

    #[test]
    fn canonicalize_accepts_inside_rejects_escape_and_outside() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("fs")).unwrap();
        // Canonicalize the root up front: on macOS the tempdir lives under
        // a symlinked `/var` → `/private/var`, so `canonicalize` inside the
        // function resolves it while a raw `tmp.path()` would not. On the
        // appliance `/fs` is already canonical, so this is a test concern.
        let root = std::fs::canonicalize(tmp.path().join("fs")).unwrap();
        let inside = root.join("share");
        std::fs::create_dir_all(&inside).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        // Inside the root: accepted, returns a canonical path under root.
        let got = canonicalize_under(&root, inside.to_str().unwrap()).unwrap();
        assert!(Path::new(&got).starts_with(&root));

        // Sharing all of /fs would bypass the intended filesystem boundary.
        assert!(matches!(
            canonicalize_under(&root, root.to_str().unwrap()),
            Err(GuestShareError::InvalidPath(_))
        ));

        // `..`-escape that resolves outside the root (inside is root/share,
        // so ../.. lands at the tempdir root where `outside` lives): rejected.
        let escape = format!("{}/../../outside", inside.display());
        assert!(matches!(
            canonicalize_under(&root, &escape),
            Err(GuestShareError::PathNotInFilesystem(_))
        ));

        // A path entirely outside the root: rejected.
        assert!(matches!(
            canonicalize_under(&root, outside.to_str().unwrap()),
            Err(GuestShareError::PathNotInFilesystem(_))
        ));

        // A non-existent path: rejected as not-found, never canonicalized.
        assert!(matches!(
            canonicalize_under(&root, &root.join("nope").to_string_lossy()),
            Err(GuestShareError::PathNotFound(_))
        ));

        // Embedded newline: rejected before touching the filesystem.
        assert!(matches!(
            canonicalize_under(&root, "/fs/a\nb"),
            Err(GuestShareError::InvalidPath(_))
        ));
    }

    #[tokio::test]
    async fn create_list_revoke_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, fs_root) = service(tmp.path());
        let shared = fs_root.join("docs");
        std::fs::create_dir_all(&shared).unwrap();

        let res = svc
            .create(
                CreateGuestShareRequest {
                    paths: vec![shared.to_string_lossy().into_owned()],
                    expires_at: None,
                    password: None,
                    max_downloads: Some(5),
                    note: Some("quarterly report".into()),
                },
                "alice",
            )
            .await
            .unwrap();

        // The plaintext token is returned but never equals what's stored.
        assert!(!res.token.is_empty());
        let stored = svc.get(&res.share.id).await.unwrap();
        assert_eq!(stored.token_hash, sha256_hex(&res.token));
        assert_ne!(stored.token_hash, res.token);
        assert_eq!(res.share.created_by, "alice");
        assert!(!res.share.password_protected);
        assert_eq!(res.share.downloads, 0);
        assert!(!res.share.revoked);

        // Management responses contain display names, not capability hashes,
        // password hashes, or absolute server paths.
        assert_eq!(res.share.names, vec!["docs"]);
        let management_json = serde_json::to_string(&res.share).unwrap();
        assert!(!management_json.contains("token_hash"));
        assert!(!management_json.contains("password_hash"));
        assert!(!management_json.contains(fs_root.to_str().unwrap()));

        // list shows it; the stored record carries the hash, not the token.
        let listed = svc.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, res.share.id);
        let serialized = serde_json::to_string(&listed[0]).unwrap();
        assert!(
            !serialized.contains(&res.token),
            "plaintext token must not be persisted/listed"
        );

        // revoke flips the flag but keeps the record.
        let revoked = svc.revoke(&res.share.id).await.unwrap();
        assert!(revoked.revoked);
        assert_eq!(svc.list().await.unwrap().len(), 1);
        assert!(svc.list().await.unwrap()[0].revoked);
    }

    #[tokio::test]
    async fn remove_requires_revoke_then_hides_but_keeps_record() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, fs_root) = service(tmp.path());
        let shared = fs_root.join("doc");
        std::fs::create_dir_all(&shared).unwrap();
        let res = svc
            .create(
                CreateGuestShareRequest {
                    paths: vec![shared.to_string_lossy().into_owned()],
                    expires_at: None,
                    password: None,
                    max_downloads: None,
                    note: None,
                },
                "admin",
            )
            .await
            .unwrap();
        let id = res.share.id;

        // An active share cannot be removed — must be revoked first.
        assert!(matches!(
            svc.remove(&id).await,
            Err(GuestShareError::NotRevoked(_))
        ));

        svc.revoke(&id).await.unwrap();
        svc.remove(&id).await.unwrap();

        // The record is kept on disk and still returned by list() (the WebUI
        // hides it behind a "Show removed" toggle), but marked hidden.
        let rec = svc.get(&id).await.unwrap();
        assert!(rec.hidden);
        assert!(rec.revoked);
        assert!(
            svc.list()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == id && s.hidden)
        );
        // remove is idempotent.
        svc.remove(&id).await.unwrap();
    }

    #[tokio::test]
    async fn create_hashes_password_and_rejects_no_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, fs_root) = service(tmp.path());
        let shared = fs_root.join("private");
        std::fs::create_dir_all(&shared).unwrap();

        let res = svc
            .create(
                CreateGuestShareRequest {
                    paths: vec![shared.to_string_lossy().into_owned()],
                    expires_at: None,
                    password: Some("hunter2".into()),
                    max_downloads: None,
                    note: None,
                },
                "bob",
            )
            .await
            .unwrap();

        assert!(res.share.password_protected);
        let hash = svc
            .get(&res.share.id)
            .await
            .unwrap()
            .password_hash
            .expect("password should be hashed");
        assert!(crate::auth::verify_password("hunter2", &hash).is_ok());
        assert!(crate::auth::verify_password("wrong", &hash).is_err());

        // Empty path list is rejected outright.
        assert!(matches!(
            svc.create(
                CreateGuestShareRequest {
                    paths: vec![],
                    expires_at: None,
                    password: None,
                    max_downloads: None,
                    note: None,
                },
                "bob",
            )
            .await,
            Err(GuestShareError::NoPaths)
        ));
        assert!(matches!(
            svc.create(
                CreateGuestShareRequest {
                    paths: vec![shared.to_string_lossy().into_owned(); MAX_SHARE_ROOTS + 1],
                    expires_at: None,
                    password: None,
                    max_downloads: None,
                    note: None,
                },
                "bob",
            )
            .await,
            Err(GuestShareError::TooManyPaths)
        ));
    }

    /// Build and persist a share with field overrides, returning it. Lets a
    /// test set expiry/downloads/revoked directly without driving `create`.
    async fn put_share(
        svc: &GuestShareService,
        token: &str,
        mutate: impl FnOnce(&mut GuestShare),
    ) -> GuestShare {
        let mut s = GuestShare {
            id: format!("id-{token}"),
            token_hash: sha256_hex(token),
            paths: vec![],
            created_by: "t".into(),
            created_at: 0,
            expires_at: None,
            password_hash: None,
            max_downloads: None,
            downloads: 0,
            views: 0,
            revoked: false,
            hidden: false,
            note: None,
        };
        mutate(&mut s);
        svc.state_dir().save(&s.id, &s).await.unwrap();
        s
    }

    #[tokio::test]
    async fn lookup_active_gates_on_token_expiry_revoke_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let now = 1000;

        put_share(&svc, "ok", |_| {}).await;
        put_share(&svc, "exp", |s| s.expires_at = Some(500)).await;
        put_share(&svc, "rev", |s| s.revoked = true).await;
        put_share(&svc, "lim", |s| {
            s.max_downloads = Some(3);
            s.downloads = 3;
        })
        .await;
        put_share(&svc, "under", |s| {
            s.max_downloads = Some(3);
            s.downloads = 2;
        })
        .await;

        assert!(svc.lookup_active("ok", now).await.is_some());
        // Unknown token, expired, revoked, and exhausted all look identical.
        assert!(svc.lookup_active("nope", now).await.is_none());
        assert!(svc.lookup_active("exp", now).await.is_none());
        assert!(svc.lookup_active("rev", now).await.is_none());
        assert!(svc.lookup_active("lim", now).await.is_none());
        // Not-yet-expired and under-cap remain available.
        assert!(svc.lookup_active("exp", 100).await.is_some());
        assert!(svc.lookup_active("under", now).await.is_some());
    }

    #[tokio::test]
    async fn register_download_enforces_cap_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let now = 10;
        let s = put_share(&svc, "dl", |s| s.max_downloads = Some(2)).await;

        assert!(svc.register_download(&s.id, now).await.is_ok());
        assert!(svc.register_download(&s.id, now).await.is_ok());
        // Third exceeds the cap and is refused; share is now exhausted.
        assert!(svc.register_download(&s.id, now).await.is_err());
        assert!(svc.lookup_active("dl", now).await.is_none());
        assert_eq!(svc.get(&s.id).await.unwrap().downloads, 2);
    }

    #[tokio::test]
    async fn cancelled_request_does_not_cancel_state_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, fs_root) = service(tmp.path());
        let svc = Arc::new(svc);
        let file = fs_root.join("cancel.txt");
        std::fs::write(&file, b"content").unwrap();
        let share = put_share(&svc, "cancel", |share| {
            share.paths = vec![file.to_string_lossy().into_owned()];
        })
        .await;
        let opened = svc.open_download(&share, None, "").await.unwrap();
        assert_eq!(
            svc.active_downloads.available_permits(),
            MAX_CONCURRENT_DOWNLOADS - 1
        );
        let guard = svc.state_lock.lock().await;

        let task = {
            let svc = Arc::clone(&svc);
            let id = share.id.clone();
            tokio::spawn(async move { svc.register_opened_download(&id, 10, opened).await })
        };
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        assert_eq!(
            svc.active_downloads.available_permits(),
            MAX_CONCURRENT_DOWNLOADS - 1,
            "cancelled admission must retain its descriptor slot"
        );
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if svc.get(&share.id).await.unwrap().downloads == 1
                    && svc.active_downloads.available_permits() == MAX_CONCURRENT_DOWNLOADS
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached state mutation should complete");
    }

    #[tokio::test]
    async fn concurrent_mutations_preserve_revocation_and_counters() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let svc = Arc::new(svc);
        let share = put_share(&svc, "race", |s| s.max_downloads = Some(8)).await;

        svc.record_view(&share.id).await;
        svc.record_view(&share.id).await;

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let svc = Arc::clone(&svc);
            let id = share.id.clone();
            tasks.push(tokio::spawn(async move {
                let _ = svc.register_download(&id, 10).await;
            }));
        }
        let revoke = {
            let svc = Arc::clone(&svc);
            let id = share.id.clone();
            tokio::spawn(async move {
                svc.revoke(&id).await.unwrap();
            })
        };

        for task in tasks {
            task.await.unwrap();
        }
        revoke.await.unwrap();

        let stored = svc.get(&share.id).await.unwrap();
        assert!(stored.revoked, "counter updates must not resurrect a share");
        assert_eq!(stored.views, 2, "other mutations must preserve view counts");
        assert!(stored.downloads <= 8, "download cap must not be exceeded");
    }

    #[tokio::test]
    async fn record_view_does_not_queue_behind_security_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let share = put_share(&svc, "view-contention", |_| {}).await;

        let guard = svc.state_lock.lock().await;
        svc.record_view(&share.id).await;
        drop(guard);
        assert_eq!(svc.get(&share.id).await.unwrap().views, 0);

        svc.record_view(&share.id).await;
        assert_eq!(svc.get(&share.id).await.unwrap().views, 1);
    }

    #[tokio::test]
    async fn record_view_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let s = put_share(&svc, "v", |_| {}).await;
        svc.record_view(&s.id).await;
        svc.record_view(&s.id).await;
        assert_eq!(svc.get(&s.id).await.unwrap().views, 2);
    }

    #[test]
    fn grants_bind_to_share_and_expire() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let now = 1000;
        let grant = svc.mint_grant("share-a", now);

        assert!(svc.check_grant(&grant, "share-a", now));
        // Wrong share, unknown grant, and past-expiry all fail.
        assert!(!svc.check_grant(&grant, "share-b", now));
        assert!(!svc.check_grant("bogus", "share-a", now));
        assert!(!svc.check_grant(&grant, "share-a", now + GRANT_TTL_SECS + 1));
    }

    #[test]
    fn unlock_rate_limit_reserves_atomically_then_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let now = 1000;

        for _ in 0..UNLOCK_MAX_FAILURES {
            assert!(svc.reserve_unlock_attempt("1.2.3.4", "tok", now));
        }
        assert!(!svc.reserve_unlock_attempt("1.2.3.4", "tok", now));
        // A different IP is unaffected; a successful clear resets the counter.
        assert!(svc.reserve_unlock_attempt("5.6.7.8", "tok", now));
        svc.clear_unlock_failures("1.2.3.4", "tok");
        assert!(svc.reserve_unlock_attempt("1.2.3.4", "tok", now));
    }

    #[test]
    fn concurrent_unlock_reservations_enforce_exact_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let svc = Arc::new(svc);
        let barrier = Arc::new(std::sync::Barrier::new(33));
        let mut workers = Vec::new();

        for _ in 0..32 {
            let svc = Arc::clone(&svc);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                svc.reserve_unlock_attempt("192.0.2.1", "token", 1000)
            }));
        }
        barrier.wait();
        let admitted: usize = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum();
        assert_eq!(admitted, UNLOCK_MAX_FAILURES);
    }

    #[test]
    fn unlock_rate_limit_prunes_outside_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        // Failures older than the window don't count toward the lockout.
        let old = 1000;
        for _ in 0..UNLOCK_MAX_FAILURES {
            assert!(svc.reserve_unlock_attempt("1.2.3.4", "tok", old));
        }
        let later = old + UNLOCK_WINDOW_SECS + 1;
        assert!(svc.reserve_unlock_attempt("1.2.3.4", "tok", later));
    }

    #[test]
    fn unlock_rate_limit_is_a_true_sliding_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let start = 1000;

        for offset in 0..UNLOCK_MAX_FAILURES {
            assert!(svc.reserve_unlock_attempt("1.2.3.4", "tok", start + offset as i64));
        }
        let boundary = start + UNLOCK_WINDOW_SECS;
        assert!(svc.reserve_unlock_attempt("1.2.3.4", "tok", boundary));
        assert!(!svc.reserve_unlock_attempt("1.2.3.4", "tok", boundary));
    }

    #[test]
    fn unlock_rate_limit_trackers_are_bounded_and_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());

        for i in 0..UNLOCK_MAX_TRACKED_IPS {
            let ip = format!("192.0.2.{i}");
            assert!(svc.allow_unlock_request(&ip, 1000));
        }
        assert!(!svc.allow_unlock_request("203.0.113.1", 1000));
        assert_eq!(
            svc.unlock_requests.lock().unwrap().entries.len(),
            UNLOCK_MAX_TRACKED_IPS
        );
        assert!(svc.allow_unlock_request("203.0.113.1", 1000 + UNLOCK_REQUEST_WINDOW_SECS));
        assert_eq!(svc.unlock_requests.lock().unwrap().entries.len(), 1);

        for i in 0..UNLOCK_MAX_TRACKED_KEYS {
            let ip = format!("198.51.100.{i}");
            assert!(svc.reserve_unlock_attempt(&ip, "valid-token", 1000));
        }
        assert!(!svc.reserve_unlock_attempt("203.0.113.2", "valid-token", 1000));
        assert_eq!(
            svc.unlock_failures.lock().unwrap().entries.len(),
            UNLOCK_MAX_TRACKED_KEYS
        );
        assert!(svc.reserve_unlock_attempt(
            "203.0.113.2",
            "valid-token",
            1000 + UNLOCK_WINDOW_SECS
        ));
        assert_eq!(svc.unlock_failures.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn unlock_request_limit_runs_before_password_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let now = 1000;

        for _ in 0..UNLOCK_REQUEST_MAX {
            assert!(svc.allow_unlock_request("192.0.2.1", now));
        }
        assert!(!svc.allow_unlock_request("192.0.2.1", now));
        assert!(svc.allow_unlock_request("192.0.2.1", now + UNLOCK_REQUEST_WINDOW_SECS + 1));
    }

    #[tokio::test]
    async fn verify_share_password_matches_and_open_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let hash = crate::auth::hash_password("s3cret").unwrap();
        let protected = GuestShare {
            id: "p".into(),
            token_hash: String::new(),
            paths: vec![],
            created_by: String::new(),
            created_at: 0,
            expires_at: None,
            password_hash: Some(hash),
            max_downloads: None,
            downloads: 0,
            views: 0,
            revoked: false,
            hidden: false,
            note: None,
        };
        assert!(GuestShareService::needs_password(&protected));
        assert!(
            svc.verify_share_password(&protected, "s3cret")
                .await
                .unwrap()
        );
        assert!(
            !svc.verify_share_password(&protected, "wrong")
                .await
                .unwrap()
        );

        let permits: Vec<_> = (0..MAX_CONCURRENT_PASSWORD_CHECKS)
            .map(|_| svc.password_checks.clone().try_acquire_owned().unwrap())
            .collect();
        assert!(matches!(
            svc.verify_share_password(&protected, "s3cret").await,
            Err(GuestShareError::PasswordCheckBusy)
        ));
        drop(permits);

        let mut open = protected;
        open.password_hash = None;
        assert!(!GuestShareService::needs_password(&open));
        // No password set => nothing to prove, any input "passes".
        assert!(svc.verify_share_password(&open, "anything").await.unwrap());
    }

    #[tokio::test]
    async fn open_download_stays_within_share_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let dir = root.join("folder");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hi").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.txt"), b"yo").unwrap();
        let outside = root.join("secret.txt");
        std::fs::write(&outside, b"nope").unwrap();
        let single = root.join("lone.txt");
        std::fs::write(&single, b"solo").unwrap();

        let folder_share = GuestShare {
            paths: vec![dir.to_string_lossy().into_owned()],
            ..bare("f")
        };
        // Relative file inside the shared folder (including a subdir): ok.
        assert!(
            svc.open_download(&folder_share, None, "a.txt")
                .await
                .is_some()
        );
        assert!(
            svc.open_download(&folder_share, None, "sub/b.txt")
                .await
                .is_some()
        );
        // Escapes and non-files: rejected.
        assert!(
            svc.open_download(&folder_share, None, "../secret.txt")
                .await
                .is_none()
        );
        assert!(
            svc.open_download(&folder_share, None, "sub")
                .await
                .is_none()
        ); // a dir, not a file
        assert!(
            svc.open_download(&folder_share, None, "missing.txt")
                .await
                .is_none()
        );
        assert!(
            svc.open_download(&folder_share, None, "a\\b")
                .await
                .is_none()
        ); // backslash rejected

        // Single-file share: empty path resolves to the file itself.
        let file_share = GuestShare {
            paths: vec![single.to_string_lossy().into_owned()],
            ..bare("s")
        };
        assert!(svc.open_download(&file_share, None, "").await.is_some());
    }

    #[tokio::test]
    async fn open_download_limits_active_descriptors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let file = root.join("file.txt");
        std::fs::write(&file, b"content").unwrap();
        let share = GuestShare {
            paths: vec![file.to_string_lossy().into_owned()],
            ..bare("limit")
        };

        let mut opened = Vec::new();
        for _ in 0..MAX_CONCURRENT_DOWNLOADS {
            opened.push(svc.open_download(&share, None, "").await.unwrap());
        }
        assert!(svc.open_download(&share, None, "").await.is_none());

        opened.pop();
        assert!(svc.open_download(&share, None, "").await.is_some());
    }

    #[tokio::test]
    async fn public_meta_lists_basenames_not_abspaths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let dir = root.join("docs");
        std::fs::create_dir_all(&dir).unwrap();
        let file = root.join("report.pdf");
        std::fs::write(&file, b"%PDF-1.4").unwrap();

        let hash = crate::auth::hash_password("x").unwrap();
        let share = GuestShare {
            paths: vec![
                dir.to_string_lossy().into_owned(),
                file.to_string_lossy().into_owned(),
            ],
            password_hash: Some(hash),
            expires_at: Some(42),
            ..bare("m")
        };
        let svc = GuestShareService::with_dirs(root.join("state"), root);
        let locked = svc.meta(&share, false).await.unwrap();
        assert!(locked.entries.is_empty());
        assert!(!locked.unlocked);

        let meta = svc.meta(&share, true).await.unwrap();
        assert!(meta.password_required);
        assert!(meta.unlocked);
        assert_eq!(meta.expires_at, Some(42));
        assert_eq!(meta.entries.len(), 2);
        let names: Vec<&str> = meta.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"docs"));
        assert!(names.contains(&"report.pdf"));
        // No entry leaks an absolute path.
        assert!(meta.entries.iter().all(|e| !e.name.contains('/')));
        let pdf = meta
            .entries
            .iter()
            .find(|e| e.name == "report.pdf")
            .unwrap();
        assert!(!pdf.is_dir);
        assert_eq!(pdf.size, 8);
        assert_eq!(pdf.root, 1);
    }

    #[tokio::test]
    async fn browse_directory_is_sorted_and_stays_within_selected_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(first.join("sub")).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("z.txt"), b"z").unwrap();
        std::fs::write(first.join("A.txt"), b"alpha").unwrap();
        std::fs::write(first.join("sub/nested.txt"), b"nested").unwrap();
        std::fs::write(second.join("other.txt"), b"other").unwrap();
        std::fs::write(first.join("bad\"name.txt"), b"hidden").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            std::os::unix::fs::symlink(&second, first.join("escape")).unwrap();
            let fifo = first.join("pipe");
            let fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        }
        let svc = GuestShareService::with_dirs(root.join("state"), root);
        let share = GuestShare {
            paths: vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            ..bare("browse")
        };

        let listing = svc.browse_directory(&share, 0, "").await.unwrap();
        assert_eq!(listing.root, 0);
        assert_eq!(listing.path, "");
        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["sub", "A.txt", "z.txt"]);
        assert!(listing.entries[0].is_dir);
        assert_eq!(listing.entries[1].size, 5);

        let nested = svc.browse_directory(&share, 0, "sub").await.unwrap();
        assert_eq!(nested.path, "sub");
        assert_eq!(nested.entries[0].name, "nested.txt");
        assert!(svc.browse_directory(&share, 0, "../second").await.is_none());
        assert!(svc.browse_directory(&share, 2, "").await.is_none());
        assert!(svc.browse_directory(&share, 0, "z.txt").await.is_none());
    }

    #[tokio::test]
    async fn download_root_index_disambiguates_shared_roots() {
        use tokio::io::AsyncReadExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("same.txt"), b"first").unwrap();
        std::fs::write(second.join("same.txt"), b"second").unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root);
        let share = GuestShare {
            paths: vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            ..bare("root-index")
        };

        let opened = svc
            .open_download(&share, Some(1), "same.txt")
            .await
            .unwrap();
        let (mut reader, _, _) = opened.into_parts();
        let mut content = Vec::new();
        reader.read_to_end(&mut content).await.unwrap();
        assert_eq!(content, b"second");
        assert!(
            svc.open_download(&share, Some(2), "same.txt")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn zip_stream_includes_files_and_skips_symlink_escape() {
        use tokio::io::AsyncReadExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let folder = root.join("photos");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("a.txt"), b"alpha").unwrap();
        std::fs::write(folder.join("sub/b.txt"), b"bravo").unwrap();

        // A secret outside the share, reachable only via a symlink planted
        // inside it. The walk must NOT follow the symlink.
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"TOPSECRET").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, folder.join("leak.txt")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let fifo = folder.join("pipe");
            let fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        }

        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let share = GuestShare {
            paths: vec![folder.to_string_lossy().into_owned()],
            ..bare("z")
        };

        let mut reader = svc.prepare_archive(&share).await.unwrap().into_stream();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();

        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..2], b"PK", "should be a zip archive");
        let entries = read_zip_entries(bytes).await;
        assert_eq!(entries.get("photos/a.txt").unwrap(), b"alpha");
        assert_eq!(entries.get("photos/sub/b.txt").unwrap(), b"bravo");
        assert!(!entries.contains_key("photos/leak.txt"));
        assert!(!entries.contains_key("photos/pipe"));
        assert!(entries.values().all(|content| content != b"TOPSECRET"));
    }

    #[tokio::test]
    async fn zip_stream_sanitizes_backslashes_in_entry_names() {
        use tokio::io::AsyncReadExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let folder = root.join("docs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("..\\escape.txt"), b"safe").unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let share = GuestShare {
            paths: vec![folder.to_string_lossy().into_owned()],
            ..bare("safe-name")
        };

        let mut reader = svc.prepare_archive(&share).await.unwrap().into_stream();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        let entries = read_zip_entries(bytes).await;
        assert_eq!(entries.get("docs/..%5Cescape.txt").unwrap(), b"safe");
        assert!(entries.keys().all(|name| !name.contains("../")));
    }

    #[tokio::test]
    async fn zip_stream_disambiguates_duplicate_root_names() {
        use tokio::io::AsyncReadExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let first = root.join("one/docs");
        let second = root.join("two/docs");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("first.txt"), b"first").unwrap();
        std::fs::write(second.join("second.txt"), b"second").unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let share = GuestShare {
            paths: vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            ..bare("duplicate-roots")
        };

        let mut reader = svc.prepare_archive(&share).await.unwrap().into_stream();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        let entries = read_zip_entries(bytes).await;
        assert_eq!(entries.get("docs/first.txt").unwrap(), b"first");
        assert_eq!(entries.get("docs (2)/second.txt").unwrap(), b"second");
    }

    #[test]
    fn zip_components_are_relative_and_collision_resistant() {
        assert_eq!(safe_zip_component(std::ffi::OsStr::new("C:")), "C%3A");
        assert_eq!(
            safe_zip_component(std::ffi::OsStr::new("..\\file")),
            "..%5Cfile"
        );
        assert_eq!(safe_zip_component(std::ffi::OsStr::new("100%")), "100%25");

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let invalid = std::ffi::OsString::from_vec(vec![0xff]);
            assert_eq!(safe_zip_component(&invalid), "__nasty_raw_FF");
            assert_ne!(
                safe_zip_component(&invalid),
                safe_zip_component(std::ffi::OsStr::new("__nasty_raw_FF"))
            );
        }
    }

    #[tokio::test]
    async fn archive_streams_have_an_independent_concurrency_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("file.txt");
        std::fs::write(&file, b"content").unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root);
        let share = GuestShare {
            paths: vec![file.to_string_lossy().into_owned()],
            ..bare("archive-limit")
        };

        let mut archives = Vec::new();
        for _ in 0..MAX_CONCURRENT_ARCHIVES {
            archives.push(svc.prepare_archive(&share).await.unwrap());
        }
        assert!(svc.prepare_archive(&share).await.is_none());
        assert_eq!(
            svc.active_downloads.available_permits(),
            MAX_CONCURRENT_DOWNLOADS,
            "archive load must not consume single-file slots"
        );

        archives.pop();
        assert!(svc.prepare_archive(&share).await.is_some());
    }

    #[tokio::test]
    async fn archive_preparation_rejects_missing_or_excess_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("file.txt");
        std::fs::write(&file, b"content").unwrap();
        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());

        let missing = GuestShare {
            paths: vec![
                file.to_string_lossy().into_owned(),
                root.join("missing.txt").to_string_lossy().into_owned(),
            ],
            ..bare("missing-root")
        };
        assert!(svc.prepare_archive(&missing).await.is_none());

        let excessive = GuestShare {
            paths: vec![file.to_string_lossy().into_owned(); MAX_SHARE_ROOTS + 1],
            ..bare("excess-roots")
        };
        assert!(svc.prepare_archive(&excessive).await.is_none());
    }

    #[tokio::test]
    async fn dropping_archive_reader_releases_its_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let folder = root.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        for index in 0..100 {
            std::fs::create_dir(folder.join(format!("empty-{index}"))).unwrap();
        }
        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let share = GuestShare {
            paths: vec![folder.to_string_lossy().into_owned()],
            ..bare("cancel-archive")
        };

        let reader = svc.prepare_archive(&share).await.unwrap().into_stream();
        drop(reader);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while svc.active_archives.available_permits() != MAX_CONCURRENT_ARCHIVES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled archive should release its stream slot");
    }

    async fn read_zip_entries(bytes: Vec<u8>) -> HashMap<String, Vec<u8>> {
        let archive = async_zip::base::read::mem::ZipFileReader::new(bytes)
            .await
            .unwrap();
        let mut entries = HashMap::new();
        for (index, stored) in archive.file().entries().iter().enumerate() {
            let name = stored.filename().as_str().unwrap().to_string();
            let mut reader = archive.reader_without_entry(index).await.unwrap();
            let mut content = Vec::new();
            futures_util::io::AsyncReadExt::read_to_end(&mut reader, &mut content)
                .await
                .unwrap();
            entries.insert(name, content);
        }
        entries
    }

    /// A throwaway share with everything empty/default but a distinct id.
    fn bare(id: &str) -> GuestShare {
        GuestShare {
            id: id.into(),
            token_hash: String::new(),
            paths: vec![],
            created_by: String::new(),
            created_at: 0,
            expires_at: None,
            password_hash: None,
            max_downloads: None,
            downloads: 0,
            views: 0,
            revoked: false,
            hidden: false,
            note: None,
        }
    }
}
