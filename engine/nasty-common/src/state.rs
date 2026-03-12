//! Per-item state persistence with atomic writes.
//!
//! Each item is stored as a separate JSON file in a directory,
//! identified by its ID. Writes use write-to-temp-then-rename
//! for crash safety.

use std::path::PathBuf;

use serde::{de::DeserializeOwned, Serialize};

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

    /// Load a single item by its ID.
    pub async fn load<T: DeserializeOwned>(&self, id: &str) -> Option<T> {
        let path = self.item_path(id);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save a single item. Uses atomic write (temp file + rename).
    pub async fn save<T: Serialize>(&self, id: &str, item: &T) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;

        let json = serde_json::to_string_pretty(item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

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
