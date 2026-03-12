use serde::Serialize;
use thiserror::Error;
use tracing::info;

const VERSION_PATH: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const FLAKE_URL: &str = "github:fenio/nasty?dir=nixos#nasty";
const REPO_URL: &str = "https://github.com/fenio/nasty.git";

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("update already in progress")]
    AlreadyRunning,
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct UpdateStatus {
    /// "idle", "running", "success", "failed"
    pub state: String,
    pub log: String,
}

pub struct UpdateService;

impl UpdateService {
    pub fn new() -> Self {
        Self
    }

    /// Get current installed version
    pub async fn version(&self) -> UpdateInfo {
        UpdateInfo {
            current_version: read_current_version().await,
            latest_version: None,
            update_available: None,
        }
    }

    /// Check if an update is available by comparing local rev to GitHub
    pub async fn check(&self) -> Result<UpdateInfo, UpdateError> {
        let current = read_current_version().await;

        // Use git ls-remote to get the latest commit SHA from main branch
        let output = tokio::process::Command::new("git")
            .args(["ls-remote", REPO_URL, "refs/heads/main"])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("git ls-remote: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "git ls-remote failed: {stderr}"
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let latest = stdout
            .split_whitespace()
            .next()
            .map(|sha| sha[..7.min(sha.len())].to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let available = if current == "dev" || latest == "unknown" {
            None
        } else {
            Some(current != latest)
        };

        Ok(UpdateInfo {
            current_version: current,
            latest_version: Some(latest),
            update_available: available,
        })
    }

    /// Start a system update via nixos-rebuild
    pub async fn apply(&self) -> Result<(), UpdateError> {
        let status = self.status().await;
        if status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        // Clean up any previous update unit
        let _ = tokio::process::Command::new("systemctl")
            .args(["reset-failed", UPDATE_UNIT])
            .output()
            .await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["stop", UPDATE_UNIT])
            .output()
            .await;

        // Launch nixos-rebuild as a transient systemd service
        // This avoids the middleware's ProtectSystem restrictions
        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit",
                UPDATE_UNIT,
                "--description",
                "NASty system update",
                "--property=Type=oneshot",
                "--",
                "nixos-rebuild",
                "switch",
                "--flake",
                FLAKE_URL,
            ])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemd-run: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "failed to start update: {stderr}"
            )));
        }

        info!("System update started");
        Ok(())
    }

    /// Rollback to previous NixOS generation
    pub async fn rollback(&self) -> Result<(), UpdateError> {
        let status = self.status().await;
        if status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        let _ = tokio::process::Command::new("systemctl")
            .args(["reset-failed", UPDATE_UNIT])
            .output()
            .await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["stop", UPDATE_UNIT])
            .output()
            .await;

        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit",
                UPDATE_UNIT,
                "--description",
                "NASty system rollback",
                "--property=Type=oneshot",
                "--",
                "nixos-rebuild",
                "switch",
                "--rollback",
            ])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemd-run: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "failed to start rollback: {stderr}"
            )));
        }

        info!("System rollback started");
        Ok(())
    }

    /// Get the current status of a running/completed update
    pub async fn status(&self) -> UpdateStatus {
        // Use systemctl show to get detailed state
        let output = tokio::process::Command::new("systemctl")
            .args([
                "show",
                UPDATE_UNIT,
                "--property=ActiveState,SubState,Result",
            ])
            .output()
            .await;

        let state = match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let mut active_state = "";
                let mut result = "";

                for line in text.lines() {
                    if let Some(val) = line.strip_prefix("ActiveState=") {
                        active_state = val.trim();
                    }
                    if let Some(val) = line.strip_prefix("Result=") {
                        result = val.trim();
                    }
                }

                match active_state {
                    "active" | "activating" | "reloading" => "running".to_string(),
                    "inactive" | "deactivating" => {
                        if result == "success" {
                            "success".to_string()
                        } else {
                            // Unit never ran or was cleaned up
                            "idle".to_string()
                        }
                    }
                    "failed" => "failed".to_string(),
                    _ => "idle".to_string(),
                }
            }
            Err(_) => "idle".to_string(),
        };

        // Read journal output for the update unit
        let log = tokio::process::Command::new("journalctl")
            .args([
                "-u",
                UPDATE_UNIT,
                "--no-pager",
                "-n",
                "200",
                "--output=cat",
            ])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        UpdateStatus { state, log }
    }
}

async fn read_current_version() -> String {
    tokio::fs::read_to_string(VERSION_PATH)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "dev".to_string())
}
