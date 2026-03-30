//! Periodic backup of /var/lib/nasty config to bcachefs.
//!
//! Copies critical config files (auth, certs, share state, samba DBs) to
//! .nasty/config-backup/ on the first mounted bcachefs filesystem.
//! Runs once on startup and then every hour.

use std::path::Path;
use tracing::{info, warn};

/// Source → destination name mapping under .nasty/
const BACKUP_DIRS: &[(&str, &str)] = &[
    ("/var/lib/nasty", "engine"),
    ("/var/lib/samba", "samba"),
];
const INTERVAL_SECS: u64 = 3600; // 1 hour

/// Find the first mounted filesystem under /fs.
async fn find_first_fs() -> Option<String> {
    let mut entries = tokio::fs::read_dir("/fs").await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with('.') {
                return Some(name);
            }
        }
    }
    None
}

/// Run a single backup cycle.
async fn run_backup() {
    let fs_name = match find_first_fs().await {
        Some(n) => n,
        None => return, // No filesystem mounted yet
    };

    let nasty_dir = format!("/fs/{fs_name}/.nasty");
    if let Err(e) = tokio::fs::create_dir_all(&nasty_dir).await {
        warn!("Failed to create .nasty dir: {e}");
        return;
    }

    for (src, dest_name) in BACKUP_DIRS {
        if !Path::new(src).is_dir() {
            continue;
        }
        let dest = format!("{nasty_dir}/{dest_name}");
        let _ = tokio::fs::create_dir_all(&dest).await;

        let output = tokio::process::Command::new("rsync")
            .args([
                "-a", "--delete", "--quiet",
                &format!("{src}/"),
                &format!("{dest}/"),
            ])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {}
            Ok(o) => warn!("rsync {src} → {dest} failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => warn!("Failed to run rsync for {src}: {e}"),
        }
    }

    info!("Config backup complete → /fs/{fs_name}/.nasty/");
}

/// Spawn the periodic backup task. Runs immediately then every hour.
pub fn spawn_periodic() {
    tokio::spawn(async {
        // Initial delay — let filesystems mount first
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        loop {
            run_backup().await;
            tokio::time::sleep(std::time::Duration::from_secs(INTERVAL_SECS)).await;
        }
    });
}
