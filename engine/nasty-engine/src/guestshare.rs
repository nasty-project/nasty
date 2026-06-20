//! Guest file sharing — share records + the authenticated CRUD that the
//! public access surface (a later PR) will read.
//!
//! This module is the *spine* of #474: it persists a [`GuestShare`] per
//! share and exposes [`GuestShareService`] for the operator-only
//! `guestshare.*` RPCs. There is deliberately **no public/unauthenticated
//! surface here** — no download endpoint, no password *verification*, no
//! download/view counting. Those land in the next PR. The record already
//! carries the fields they need (`downloads`, `views`, `max_downloads`,
//! `expires_at`, `password_hash`) so that follow-up needs no migration.
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
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

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

#[derive(Debug, Error)]
pub enum GuestShareError {
    #[error("share not found: {0}")]
    NotFound(String),
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
    /// Maximum number of downloads before the share stops working. `None` =
    /// unlimited.
    pub max_downloads: Option<u32>,
    /// Downloads served so far. Always 0 in this PR (no public surface yet).
    pub downloads: u32,
    /// Metadata views so far. Always 0 in this PR.
    pub views: u32,
    /// Whether the share has been revoked. Revoked records are kept (not
    /// deleted) so history/audit survive.
    pub revoked: bool,
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
    pub share: GuestShare,
    /// Plaintext URL token. Show it to the operator now; it cannot be
    /// recovered later (only its hash is persisted).
    pub token: String,
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

/// Resolve a guest-supplied relative path to a concrete file inside one of
/// the share's roots. Returns `None` unless the canonicalized target is a
/// regular file that stays within a root — the download-time analog of the
/// create-time `/fs` guard, blocking `..`/symlink escape out of the share.
///
/// `rel` is relative to the share (empty = a single-file share's own file),
/// so absolute server paths never cross the wire.
fn resolve_within(share: &GuestShare, rel: &str) -> Option<PathBuf> {
    if rel
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\'' | '\\'))
    {
        return None;
    }
    for root in &share.paths {
        let root = Path::new(root);
        let candidate = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel.trim_start_matches('/'))
        };
        let Ok(canonical) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if canonical.starts_with(root) && canonical.is_file() {
            return Some(canonical);
        }
    }
    None
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
    Ok(canonical.to_string_lossy().into_owned())
}

/// A live password-unlock grant: proof a guest entered the right password
/// for `share_id`, valid until `expires_at` (Unix seconds).
struct GrantEntry {
    share_id: String,
    expires_at: i64,
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
    /// "ip|token" -> failed-unlock timestamps (Unix seconds), pruned to the
    /// window on each touch.
    unlock_failures: StdMutex<HashMap<String, Vec<i64>>>,
    /// Serializes the load→increment→save of download counters so the
    /// `max_downloads` cap can't be raced past under concurrent downloads.
    download_lock: tokio::sync::Mutex<()>,
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
            unlock_failures: StdMutex::new(HashMap::new()),
            download_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn state_dir(&self) -> StateDir {
        StateDir::new(self.dir.clone())
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
            note: req.note,
        };

        self.state_dir().save(&share.id, &share).await?;
        Ok(CreateGuestShareResult { share, token })
    }

    /// Revoke a share. The record is kept (marked `revoked`) so history and
    /// audit survive — only the public surface stops honoring it.
    pub async fn revoke(&self, id: &str) -> Result<GuestShare, GuestShareError> {
        let mut share = self.get(id).await?;
        if !share.revoked {
            share.revoked = true;
            self.state_dir().save(&share.id, &share).await?;
        }
        Ok(share)
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

    /// Resolve a guest's relative path to a real file inside a share root,
    /// or `None` if it escapes / isn't a file.
    pub fn resolve_download(share: &GuestShare, rel: &str) -> Option<PathBuf> {
        resolve_within(share, rel)
    }

    /// Whether `share` is password-protected.
    pub fn needs_password(share: &GuestShare) -> bool {
        share.password_hash.is_some()
    }

    /// Verify a guest-supplied password against the share. `true` when the
    /// share has no password (nothing to prove).
    pub fn verify_share_password(share: &GuestShare, password: &str) -> bool {
        match &share.password_hash {
            Some(h) => crate::auth::verify_password(password, h).is_ok(),
            None => true,
        }
    }

    /// Count a metadata view. Best-effort; a lost increment is harmless.
    pub async fn record_view(&self, id: &str) {
        if let Some(mut s) = self.state_dir().load::<GuestShare>(id).await {
            s.views = s.views.saturating_add(1);
            let _ = self.state_dir().save(id, &s).await;
        }
    }

    /// Count a download, enforcing `max_downloads`. Serialized via
    /// `download_lock` so concurrent downloads can't race past the cap.
    /// Returns `Err(NotFound)` if the share went inactive between lookup and
    /// here — the handler maps that to the same generic "not available".
    pub async fn register_download(&self, id: &str, now: i64) -> Result<(), GuestShareError> {
        let _guard = self.download_lock.lock().await;
        let mut s = self.get(id).await?;
        if !is_accessible(&s, now) {
            return Err(GuestShareError::NotFound(id.to_string()));
        }
        s.downloads = s.downloads.saturating_add(1);
        self.state_dir().save(id, &s).await?;
        Ok(())
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

    /// Whether unlock attempts for this (ip, token) are currently locked out.
    pub fn unlock_locked(&self, ip: &str, token: &str, now: i64) -> bool {
        let key = format!("{ip}|{token}");
        let mut m = self.unlock_failures.lock().unwrap();
        let v = m.entry(key).or_default();
        v.retain(|&t| now - t < UNLOCK_WINDOW_SECS);
        v.len() >= UNLOCK_MAX_FAILURES
    }

    /// Record a failed unlock attempt for this (ip, token).
    pub fn record_unlock_failure(&self, ip: &str, token: &str, now: i64) {
        let key = format!("{ip}|{token}");
        let mut m = self.unlock_failures.lock().unwrap();
        let v = m.entry(key).or_default();
        v.retain(|&t| now - t < UNLOCK_WINDOW_SECS);
        v.push(now);
    }

    /// Clear the failure counter for this (ip, token) after a success.
    pub fn clear_unlock_failures(&self, ip: &str, token: &str) {
        let key = format!("{ip}|{token}");
        self.unlock_failures.lock().unwrap().remove(&key);
    }
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
        assert_eq!(res.share.token_hash, sha256_hex(&res.token));
        assert_ne!(res.share.token_hash, res.token);
        assert_eq!(res.share.created_by, "alice");
        assert!(res.share.password_hash.is_none());
        assert_eq!(res.share.downloads, 0);
        assert!(!res.share.revoked);

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

        let hash = res.share.password_hash.expect("password should be hashed");
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
    fn unlock_rate_limit_locks_then_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        let now = 1000;

        assert!(!svc.unlock_locked("1.2.3.4", "tok", now));
        for _ in 0..UNLOCK_MAX_FAILURES {
            svc.record_unlock_failure("1.2.3.4", "tok", now);
        }
        assert!(svc.unlock_locked("1.2.3.4", "tok", now));
        // A different IP is unaffected; a successful clear resets the counter.
        assert!(!svc.unlock_locked("5.6.7.8", "tok", now));
        svc.clear_unlock_failures("1.2.3.4", "tok");
        assert!(!svc.unlock_locked("1.2.3.4", "tok", now));
    }

    #[test]
    fn unlock_rate_limit_prunes_outside_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (svc, _) = service(tmp.path());
        // Failures older than the window don't count toward the lockout.
        let old = 1000;
        for _ in 0..UNLOCK_MAX_FAILURES {
            svc.record_unlock_failure("1.2.3.4", "tok", old);
        }
        let later = old + UNLOCK_WINDOW_SECS + 1;
        assert!(!svc.unlock_locked("1.2.3.4", "tok", later));
    }

    #[test]
    fn verify_share_password_matches_and_open_when_unset() {
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
            note: None,
        };
        assert!(GuestShareService::needs_password(&protected));
        assert!(GuestShareService::verify_share_password(
            &protected, "s3cret"
        ));
        assert!(!GuestShareService::verify_share_password(
            &protected, "wrong"
        ));

        let mut open = protected;
        open.password_hash = None;
        assert!(!GuestShareService::needs_password(&open));
        // No password set => nothing to prove, any input "passes".
        assert!(GuestShareService::verify_share_password(&open, "anything"));
    }

    #[test]
    fn resolve_download_stays_within_share_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
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
        assert!(resolve_within(&folder_share, "a.txt").is_some());
        assert!(resolve_within(&folder_share, "sub/b.txt").is_some());
        // Escapes and non-files: rejected.
        assert!(resolve_within(&folder_share, "../secret.txt").is_none());
        assert!(resolve_within(&folder_share, "sub").is_none()); // a dir, not a file
        assert!(resolve_within(&folder_share, "missing.txt").is_none());
        assert!(resolve_within(&folder_share, "a\\b").is_none()); // backslash rejected

        // Single-file share: empty path resolves to the file itself.
        let file_share = GuestShare {
            paths: vec![single.to_string_lossy().into_owned()],
            ..bare("s")
        };
        assert!(resolve_within(&file_share, "").is_some());
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
            note: None,
        }
    }
}
