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

use std::path::{Path, PathBuf};

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

/// Operator-facing guest-share store. Holds its state directory and the
/// filesystem root so tests can point both at a tempdir.
pub struct GuestShareService {
    dir: PathBuf,
    fs_root: PathBuf,
}

impl Default for GuestShareService {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestShareService {
    pub fn new() -> Self {
        Self {
            dir: PathBuf::from(STATE_DIR),
            fs_root: PathBuf::from(FILES_ROOT),
        }
    }

    /// Test seam: store records under `dir` and require paths under `fs_root`.
    #[cfg(test)]
    fn with_dirs(dir: PathBuf, fs_root: PathBuf) -> Self {
        Self { dir, fs_root }
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
}
