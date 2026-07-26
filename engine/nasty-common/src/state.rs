//! Per-item state persistence with atomic writes.
//!
//! Each item is stored as a separate JSON file in a directory,
//! identified by its ID. Writes use write-to-temp-then-rename
//! for crash safety.

use std::path::{Path, PathBuf};

use serde::{Serialize, de::DeserializeOwned};

/// Load a JSON-serialized state file, returning `Default::default()` if
/// the file is missing OR corrupted — with critical differences from
/// the naive pattern `from_str(&s).unwrap_or_default()`:
///
/// 1. **Loud logging on corruption.** A parse failure means the
///    on-disk file existed but couldn't be deserialized — typically a
///    disk error, partial write, or hand-edit gone wrong. Silently
///    falling back to defaults is data loss in disguise; this helper
///    emits a `WARN` so the operator sees it.
///
/// 2. **Side-saves the corrupt content.** The bad file is moved to
///    `<path>.corrupt.<unix-ts>` before defaults take over. That
///    keeps a forensic copy for manual recovery and prevents the
///    engine from re-reading the same corruption next boot. A failed
///    rename is itself logged but doesn't block startup.
///
/// 3. **Missing file is silently OK** — first-boot is normal.
///
/// Use this anywhere a singleton JSON state file is loaded into a
/// `T: Default + DeserializeOwned`. Replaces patterns like:
///
/// ```ignore
/// match tokio::fs::read_to_string(PATH).await {
///     Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
///     Err(_) => Default::default(),
/// }
/// ```
pub async fn load_singleton_or_recover<T>(path: impl AsRef<Path>) -> T
where
    T: Default + DeserializeOwned,
{
    let path = path.as_ref();
    let content = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            // Read failed for a non-missing reason (permission denied,
            // I/O error). Same downside as a parse failure — we have no
            // way to recover the data — so log loudly and fall through.
            tracing::warn!(
                "state read failed for {}: {e}; using defaults",
                path.display()
            );
            return T::default();
        }
    };
    match serde_json::from_str::<T>(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "state file at {} is corrupt: {e}; backing up and using defaults",
                path.display()
            );
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "unknown".into());
            let backup = path.with_extension(format!(
                "{}.corrupt.{}",
                path.extension().and_then(|s| s.to_str()).unwrap_or("dat"),
                ts
            ));
            if let Err(e) = tokio::fs::rename(path, &backup).await {
                tracing::warn!(
                    "could not move corrupt {} aside to {}: {e}",
                    path.display(),
                    backup.display()
                );
            } else {
                tracing::warn!("corrupt {} saved as {}", path.display(), backup.display());
            }
            T::default()
        }
    }
}

/// A directory-based state store where each item is a separate JSON file.
pub struct StateDir {
    dir: PathBuf,
}

impl StateDir {
    /// Create a new state directory handle. The directory is created lazily on first write.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Load all items from the state directory.
    pub async fn load_all<T: DeserializeOwned>(&self) -> Vec<T> {
        let mut items = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(e) => e,
            Err(_) => return items,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => match serde_json::from_str(&content) {
                    Ok(item) => items.push(item),
                    Err(e) => {
                        eprintln!("WARNING: failed to parse {}: {e}", path.display());
                    }
                },
                Err(e) => {
                    eprintln!("WARNING: failed to read {}: {e}", path.display());
                }
            }
        }

        items
    }

    /// Load every JSON item or fail the whole operation. Use this for safety-
    /// critical restoration where silently omitting one corrupt item could
    /// leave an independent runtime configuration pointing at stale devices.
    pub async fn load_all_strict<T: DeserializeOwned>(&self) -> std::io::Result<Vec<T>> {
        let mut items = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(items),
            Err(error) => return Err(error),
        };

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let content = tokio::fs::read_to_string(&path).await?;
            let item = serde_json::from_str(&content).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("parse {}: {error}", path.display()),
                )
            })?;
            items.push(item);
        }

        Ok(items)
    }

    /// Load a single item by its ID.
    pub async fn load<T: DeserializeOwned>(&self, id: &str) -> Option<T> {
        let path = self.item_path(id);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save a single item. Uses atomic write (temp file + rename).
    pub async fn save<T: Serialize>(&self, id: &str, item: &T) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;

        let json = serde_json::to_string_pretty(item).map_err(std::io::Error::other)?;

        let final_path = self.item_path(id);
        let tmp_path = self.dir.join(format!(".{id}.tmp"));

        tokio::fs::write(&tmp_path, &json).await?;
        tokio::fs::rename(&tmp_path, &final_path).await?;

        Ok(())
    }

    /// Remove an item by its ID.
    pub async fn remove(&self, id: &str) -> std::io::Result<()> {
        let path = self.item_path(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn item_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

/// Trait for items that have an ID field.
pub trait HasId {
    fn id(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    struct Sample {
        name: String,
        count: i64,
    }

    #[tokio::test]
    async fn load_singleton_returns_default_when_file_missing() {
        // Missing file is the "fresh install" case — no warning, just
        // defaults. (We do log on other errors, but absence shouldn't
        // be alarming.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let got: Sample = load_singleton_or_recover(&path).await;
        assert_eq!(got, Sample::default());
    }

    #[tokio::test]
    async fn load_singleton_reads_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.json");
        tokio::fs::write(&path, br#"{"name":"hi","count":3}"#)
            .await
            .unwrap();
        let got: Sample = load_singleton_or_recover(&path).await;
        assert_eq!(
            got,
            Sample {
                name: "hi".into(),
                count: 3
            }
        );
    }

    #[tokio::test]
    async fn load_singleton_backs_up_corrupt_file_and_returns_default() {
        // This is the property we care about most: a corrupt state
        // file must NOT silently degrade to defaults — it must be
        // moved aside so the operator can recover it, and the
        // engine continues with defaults. Without the backup, the
        // next save would overwrite the corrupt-but-recoverable
        // content with a fresh `Default::default()`-serialised file,
        // and the original data would be unrecoverable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        tokio::fs::write(&path, b"this is not json").await.unwrap();
        let got: Sample = load_singleton_or_recover(&path).await;
        assert_eq!(got, Sample::default());
        assert!(!path.exists(), "corrupt file should be renamed away");
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut found_backup = false;
        while let Ok(Some(e)) = entries.next_entry().await {
            let name = e.file_name();
            let n = name.to_string_lossy();
            if n.starts_with("bad.json.corrupt.") {
                found_backup = true;
                // Sanity: the backup carries the original bytes
                // verbatim — otherwise it's useless for recovery.
                let content = tokio::fs::read_to_string(e.path()).await.unwrap();
                assert_eq!(content, "this is not json");
            }
        }
        assert!(found_backup, "expected bad.json.corrupt.<ts> backup file");
    }

    #[tokio::test]
    async fn strict_directory_load_rejects_partial_state() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("good.json"), br#"{"name":"ok","count":1}"#)
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("bad.json"), b"not json")
            .await
            .unwrap();

        let result = StateDir::new(dir.path()).load_all_strict::<Sample>().await;

        assert!(result.is_err());
    }
}
