use serde::Serialize;
use thiserror::Error;
use tracing::info;

const VERSION_PATH: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const FLAKE_URL: &str = "github:fenio/nasty?dir=nixos#nasty";
const REPO_URL: &str = "https://github.com/fenio/nasty.git";

// TODO: Remove token-based auth once the repo is public.
// The token file is only needed for private repo access.
// When removing, delete check_via_github_api(), GITHUB_TOKEN_PATH,
// and revert check() to use git ls-remote directly.
const GITHUB_TOKEN_PATH: &str = "/var/lib/nasty/github-token";
const GITHUB_API_REPO: &str = "https://api.github.com/repos/fenio/nasty/commits/main";

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

        // Try GitHub API with token first (for private repo), fall back to git ls-remote
        let latest = match check_via_github_api().await {
            Ok(sha) => sha,
            Err(_) => check_via_git_ls_remote().await?,
        };

        let available = if latest == "unknown" {
            None
        } else if current == "dev" {
            Some(true) // dev builds should always offer to update
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

        // TODO: Remove token env var once repo is public.
        // For private repos, nix needs a GitHub access token to fetch the flake.
        let token_env = read_github_token().await
            .map(|t| format!("access-tokens = github.com={t}"))
            .unwrap_or_default();

        // Launch nixos-rebuild as a transient systemd service
        // This avoids the middleware's ProtectSystem restrictions
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args([
                "--unit",
                UPDATE_UNIT,
                "--description",
                "NASty system update",
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
            ]);

        if !token_env.is_empty() {
            cmd.args(["--setenv", &format!("NIX_CONFIG={token_env}")]);
        }

        cmd.args([
                "--",
                "nixos-rebuild",
                "switch",
                "--flake",
                FLAKE_URL,
            ]);

        let output = cmd
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
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
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

/// TODO: Remove once repo is public — only needed for private repo access.
async fn check_via_github_api() -> Result<String, UpdateError> {
    let token = tokio::fs::read_to_string(GITHUB_TOKEN_PATH)
        .await
        .map(|s| s.trim().to_string())
        .map_err(|_| UpdateError::CommandFailed("no github token configured".into()))?;

    if token.is_empty() {
        return Err(UpdateError::CommandFailed("empty github token".into()));
    }

    let output = tokio::process::Command::new("curl")
        .args([
            "-sf",
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "Accept: application/vnd.github.v3+json",
            GITHUB_API_REPO,
        ])
        .output()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("curl: {e}")))?;

    if !output.status.success() {
        return Err(UpdateError::CommandFailed("GitHub API request failed".into()));
    }

    // Parse just the "sha" field from the JSON response
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| UpdateError::CommandFailed(format!("failed to parse GitHub response: {e}")))?;

    let sha = body["sha"]
        .as_str()
        .map(|s| s[..7.min(s.len())].to_string())
        .ok_or_else(|| UpdateError::CommandFailed("no sha in GitHub response".into()))?;

    Ok(sha)
}

/// Direct git ls-remote — works for public repos without auth.
async fn check_via_git_ls_remote() -> Result<String, UpdateError> {
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
    Ok(stdout
        .split_whitespace()
        .next()
        .map(|sha| sha[..7.min(sha.len())].to_string())
        .unwrap_or_else(|| "unknown".to_string()))
}

async fn read_current_version() -> String {
    tokio::fs::read_to_string(VERSION_PATH)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "dev".to_string())
}

/// TODO: Remove once repo is public.
async fn read_github_token() -> Option<String> {
    tokio::fs::read_to_string(GITHUB_TOKEN_PATH)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
