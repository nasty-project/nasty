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
/// Bound descriptors held by guest downloads, including slow clients.
const MAX_CONCURRENT_DOWNLOADS: usize = 32;

#[derive(Debug, Error)]
pub enum GuestShareError {
    #[error("share not found: {0}")]
    NotFound(String),
    #[error("share must be revoked before it can be removed: {0}")]
    NotRevoked(String),
    #[error("no paths supplied")]
    NoPaths,
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
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Public metadata for a share — deliberately minimal and leaks no absolute
/// server paths. Returned only for shares that exist and are still active.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PublicShareMeta {
    pub entries: Vec<PublicEntry>,
    pub password_required: bool,
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

/// Build the guest-visible metadata for a share. Lists each shared root by
/// basename only (name/is_dir/size) — never the absolute `/fs/...` path.
fn public_meta(share: &GuestShare) -> PublicShareMeta {
    let entries = share
        .paths
        .iter()
        .map(|p| {
            let path = Path::new(p);
            let md = std::fs::metadata(path).ok();
            PublicEntry {
                name: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string(),
                is_dir: md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                size: md.as_ref().map(|m| m.len()).unwrap_or(0),
            }
        })
        .collect();
    PublicShareMeta {
        entries,
        password_required: share.password_hash.is_some(),
        expires_at: share.expires_at,
    }
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

/// Write a ZIP of every `root` into `writer`, entries named relative to each
/// root's parent (so a share of `/fs/tank/photos` yields `photos/img.jpg`).
/// Iterative DFS — no async recursion — and symlinks are skipped so the
/// archive stays within the roots.
async fn write_share_zip(
    writer: tokio::io::DuplexStream,
    roots: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use async_zip::{Compression, ZipEntryBuilder};

    let mut zip = async_zip::tokio::write::ZipFileWriter::with_tokio(writer);

    for root in &roots {
        // Entry names are relative to the root's parent so the root's own
        // basename appears as the top-level archive folder.
        let base = root.parent().unwrap_or(root.as_path());
        let mut stack = vec![root.clone()];
        while let Some(path) = stack.pop() {
            let meta = match tokio::fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Never follow symlinks — that's the escape guard.
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                let mut rd = tokio::fs::read_dir(&path).await?;
                while let Some(entry) = rd.next_entry().await? {
                    stack.push(entry.path());
                }
            } else if meta.is_file() {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                let name = rel.to_string_lossy().replace('\\', "/");
                let builder = ZipEntryBuilder::new(name.into(), Compression::Deflate);
                let mut entry = zip.write_entry_stream(builder).await?;
                // async_zip's entry writer is futures-io; bridge the tokio
                // file into it with `.compat()` + futures copy.
                use tokio_util::compat::TokioAsyncReadCompatExt;
                let mut f = tokio::fs::File::open(&path).await?.compat();
                futures_util::io::copy(&mut f, &mut entry).await?;
                entry.close().await?;
            }
        }
    }

    zip.close().await?;
    Ok(())
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
            .find(|s| s.token_hash == hash && is_accessible(s, now))
    }

    /// Public metadata for the guest landing page.
    pub fn meta(share: &GuestShare) -> PublicShareMeta {
        public_meta(share)
    }

    /// Open a guest file beneath one of the share roots. The returned
    /// descriptor is the object that was authorized; callers never reopen a
    /// validated pathname.
    pub async fn open_download(&self, share: &GuestShare, rel: &str) -> Option<OpenedGuestFile> {
        if rel
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\'' | '\\'))
        {
            return None;
        }
        let permit = self.active_downloads.clone().try_acquire_owned().ok()?;
        let files_root = self.fs_root.clone();
        let roots = share.paths.clone();
        let relative = PathBuf::from(rel);
        tokio::task::spawn_blocking(move || {
            for root in roots {
                let root = PathBuf::from(root);
                let Ok(file) =
                    crate::file_boundary::open_regular_beneath(&files_root, &root, &relative)
                else {
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

    /// Stream a ZIP of all the share's roots into a pipe, returning the read
    /// end for the HTTP body. The archive is built lazily as the client
    /// reads, so a multi-gigabyte folder is never buffered in memory.
    ///
    /// Symlinks are skipped (never followed), so the archive cannot escape a
    /// share root — the ZIP-time analog of the download path guard.
    pub fn zip_stream(&self, share: &GuestShare) -> tokio::io::DuplexStream {
        let (reader, writer) = tokio::io::duplex(64 * 1024);
        let roots: Vec<PathBuf> = share.paths.iter().map(PathBuf::from).collect();
        tokio::spawn(async move {
            if let Err(e) = write_share_zip(writer, roots).await {
                // The client gets a truncated archive; nothing else we can do
                // once the response body has started streaming.
                tracing::warn!("guest share zip stream aborted: {e}");
            }
        });
        reader
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
        let opened = svc.open_download(&share, "").await.unwrap();
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
        assert!(svc.open_download(&folder_share, "a.txt").await.is_some());
        assert!(
            svc.open_download(&folder_share, "sub/b.txt")
                .await
                .is_some()
        );
        // Escapes and non-files: rejected.
        assert!(
            svc.open_download(&folder_share, "../secret.txt")
                .await
                .is_none()
        );
        assert!(svc.open_download(&folder_share, "sub").await.is_none()); // a dir, not a file
        assert!(
            svc.open_download(&folder_share, "missing.txt")
                .await
                .is_none()
        );
        assert!(svc.open_download(&folder_share, "a\\b").await.is_none()); // backslash rejected

        // Single-file share: empty path resolves to the file itself.
        let file_share = GuestShare {
            paths: vec![single.to_string_lossy().into_owned()],
            ..bare("s")
        };
        assert!(svc.open_download(&file_share, "").await.is_some());
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
            opened.push(svc.open_download(&share, "").await.unwrap());
        }
        assert!(svc.open_download(&share, "").await.is_none());

        opened.pop();
        assert!(svc.open_download(&share, "").await.is_some());
    }

    #[test]
    fn public_meta_lists_basenames_not_abspaths() {
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
        let meta = public_meta(&share);
        assert!(meta.password_required);
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

        let svc = GuestShareService::with_dirs(root.join("state"), root.clone());
        let share = GuestShare {
            paths: vec![folder.to_string_lossy().into_owned()],
            ..bare("z")
        };

        let mut reader = svc.zip_stream(&share);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();

        // Filenames live verbatim in the (uncompressed) local file headers,
        // so we can assert on the raw archive bytes without a zip reader.
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..2], b"PK", "should be a zip archive");
        let blob = String::from_utf8_lossy(&bytes);
        assert!(
            blob.contains("photos/a.txt"),
            "expected top-level file entry"
        );
        assert!(
            blob.contains("photos/sub/b.txt"),
            "expected nested file entry"
        );
        // The symlink itself is skipped and its target's content never appears.
        assert!(!blob.contains("leak.txt"), "symlink entry must be skipped");
        assert!(
            !blob.contains("TOPSECRET"),
            "symlink target must never be archived"
        );
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
