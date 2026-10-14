//! Opt-in OS-disk checkpoints, never raw journals or request data.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tokio::sync::Mutex;

const PATH: &str = "/var/lib/nasty/performance-diagnostics.json";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const CAPTURE_SECONDS: i64 = 30 * 60;
const RETAIN_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone, Default, Serialize, Deserialize)]
struct History {
    expires: i64,
    retain_until: i64,
    sessions: Vec<Value>,
}
pub struct Store {
    path: PathBuf,
    history: History,
    checkpoint_failed: bool,
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(Store {
            path: PATH.into(),
            history: History::default(),
            checkpoint_failed: false,
        })
    })
}
impl Store {
    async fn load(path: PathBuf) -> Self {
        let mut result = Self {
            path,
            history: History::default(),
            checkpoint_failed: false,
        };
        // Reject oversized, malformed or stale checkpoints; do not block boot
        // or interpret arbitrary log data as a diagnostic report.
        if let Ok(meta) = tokio::fs::symlink_metadata(&result.path).await
            && meta.is_file()
            && meta.len() <= MAX_BYTES as u64
            && let Ok(bytes) = tokio::fs::read(&result.path).await
            && let Ok(history) = serde_json::from_slice::<History>(&bytes)
            && history.sessions.len() <= 3
            && history.retain_until > now()
            && history.retain_until <= now() + RETAIN_SECONDS
            && history.expires <= now() + CAPTURE_SECONDS
        {
            result.history = history;
        } else {
            // Remove expired/invalid local checkpoints rather than silently
            // leaving supposedly expired history indefinitely on disk.
            let _ = tokio::fs::remove_file(&result.path).await;
        }
        result
    }
    fn active(&self) -> bool {
        self.history.expires > now()
    }
    async fn save(&self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let bytes = serde_json::to_vec(&self.history)?;
        if bytes.len() > MAX_BYTES {
            return Err(std::io::Error::other("checkpoint size limit"));
        }
        let temp = self.path.with_extension("tmp");
        // A cancelled checkpoint may leave our fixed temporary file behind.
        // All writes are serialized; never follow it or reuse its contents.
        match tokio::fs::remove_file(&temp).await {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        let result = async {
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            tokio::fs::rename(&temp, &self.path).await?;
            let parent = self.path.parent().unwrap_or(Path::new("."));
            tokio::fs::File::open(parent).await?.sync_all().await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result
    }
    fn checkpoint(&mut self, report: Value) {
        let epoch = report.get("epoch");
        if self
            .history
            .sessions
            .last()
            .is_some_and(|last| last.get("epoch") == epoch)
        {
            self.history.sessions.pop();
        }
        self.history.sessions.push(report);
        if self.history.sessions.len() > 3 {
            self.history.sessions.remove(0);
        }
    }
}

pub async fn initialize(state: std::sync::Arc<crate::AppState>) {
    if let Ok(loaded) =
        tokio::time::timeout(std::time::Duration::from_secs(2), Store::load(PATH.into())).await
    {
        *store().lock().await = loaded;
    } else {
        tracing::warn!("Performance diagnostic checkpoint load timed out; capture not restored");
    }
    // Recover only our fixed, private temporary checkpoint from an interrupted
    // atomic write. No data-pool scanning or storage recovery is involved.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::fs::remove_file(Path::new(PATH).with_extension("tmp")),
    )
    .await;
    tokio::spawn(async move {
        loop {
            let mut store = store().lock().await;
            if store.active() {
                nasty_common::diagnostics::detailed_for(std::time::Duration::from_secs(
                    (store.history.expires - now()).max(0) as u64,
                ));
                store.checkpoint(super::current_report(&state).await);
                // Do not abandon an in-flight rename: Clear must serialize
                // behind the completed write, not race an orphaned disk task.
                store.checkpoint_failed = store.save().await.is_err();
                if store.checkpoint_failed {
                    tracing::warn!(
                        "Performance diagnostic checkpoint failed; history may be incomplete"
                    );
                }
            } else if store.history.retain_until != 0 && store.history.retain_until <= now() {
                store.history = History::default();
                let _ = tokio::fs::remove_file(&store.path).await;
            }
            drop(store);
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}
pub async fn capture(enabled: bool, report: Option<Value>) -> std::io::Result<()> {
    let mut store = store().lock().await;
    let old_history = store.history.clone();
    store.history.expires = if enabled { now() + CAPTURE_SECONDS } else { 0 };
    if enabled {
        store.history.retain_until = now() + RETAIN_SECONDS;
        if let Some(report) = report {
            store.checkpoint(report);
        }
    }
    if let Err(error) = store.save().await {
        store.history = old_history;
        return Err(error);
    }
    nasty_common::diagnostics::detailed_for(if enabled {
        std::time::Duration::from_secs(CAPTURE_SECONDS as u64)
    } else {
        std::time::Duration::ZERO
    });
    Ok(())
}
pub async fn clear() -> std::io::Result<()> {
    let mut store = store().lock().await;
    match tokio::fs::remove_file(&store.path).await {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    store.history = History::default();
    store.checkpoint_failed = false;
    Ok(())
}
pub async fn status() -> Value {
    let store = store().lock().await;
    let previous: Vec<_> = if store.history.retain_until > now() {
        store
            .history
            .sessions
            .iter()
            .filter(|session| session.get("epoch").and_then(Value::as_str) != Some(super::epoch()))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    serde_json::json!({"active": store.active(), "checkpoint_failed": store.checkpoint_failed, "seconds_remaining": (store.history.expires - now()).max(0), "previous_sessions": previous})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_checkpoints_survive_restart_and_expire() {
        let dir = std::env::temp_dir().join(format!("nasty-diagnostics-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&dir).await.unwrap();
        let path = dir.join("checkpoint.json");
        let mut store = Store::load(path.clone()).await;
        store.history.expires = now() + 60;
        store.history.retain_until = now() + 3600;
        for i in 0..5 {
            store.checkpoint(serde_json::json!({"epoch": i.to_string(), "timings": []}));
        }
        store.checkpoint(serde_json::json!({"epoch": "4", "timings": [1]}));
        assert_eq!(store.history.sessions.len(), 3);
        store.save().await.unwrap();
        let loaded = Store::load(path.clone()).await;
        assert!(loaded.active());
        assert_eq!(loaded.history.sessions.len(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&path)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        store.history.retain_until = now() - 1;
        store.save().await.unwrap();
        assert!(Store::load(path.clone()).await.history.sessions.is_empty());
        tokio::fs::write(&path, b"malformed").await.unwrap();
        assert!(!Store::load(path.clone()).await.active());
        assert!(!path.exists());
        tokio::fs::remove_dir(dir).await.unwrap();
    }
}
