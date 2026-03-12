use serde::Serialize;
use thiserror::Error;
use tracing::info;

const VERSION_PATH: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const LOCAL_FLAKE: &str = "/etc/nixos/nixos#nasty";
const REPO_URL: &str = "https://github.com/fenio/nasty.git";
const LOCAL_REPO: &str = "/etc/nixos";

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
    /// True when the activated system has a different kernel than the booted one
    pub reboot_required: bool,
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

        // Strip "-dirty" suffix for comparison — the local build has a dirty
        // git tree (hardware-configuration.nix) but the commit is the same
        let current_clean = current.trim_end_matches("-dirty");
        let available = if latest == "unknown" {
            None
        } else if current_clean == "dev" {
            Some(true) // dev builds should always offer to update
        } else {
            Some(current_clean != latest)
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

        // Build the update script:
        // 1. Pull latest source into /etc/nixos
        // 2. Rebuild from local flake (which has hardware-configuration.nix)
        let token = read_github_token().await;
        let repo_url = if let Some(ref t) = token {
            format!("https://{}@github.com/fenio/nasty.git", t)
        } else {
            REPO_URL.to_string()
        };

        // TODO: Remove token env var once repo is public.
        let token_env = token
            .map(|t| format!("access-tokens = github.com={t}"))
            .unwrap_or_default();

        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
echo "==> Pulling latest source..."
cd {LOCAL_REPO}

# Preserve machine-specific hardware config
HW_CFG="nixos/hardware-configuration.nix"
[ -f "$HW_CFG" ] && cp "$HW_CFG" /tmp/nasty-hw-config.nix

git remote set-url origin "{repo_url}"
git fetch origin
git reset --hard origin/main

# Restore hardware config
[ -f /tmp/nasty-hw-config.nix ] && cp /tmp/nasty-hw-config.nix "$HW_CFG"

# Flakes require all files to be tracked
git add -A

echo "==> Rebuilding system..."
# Exit code 4 means "switched OK but some units failed" (e.g. smartd on VMs)
set +e
nixos-rebuild switch --flake {LOCAL_FLAKE}
rc=$?
set -e
if [ $rc -ne 0 ] && [ $rc -ne 4 ]; then
  echo "==> Rebuild failed (exit code $rc)"
  exit $rc
fi
echo "==> Update complete!"
"#
        );

        // Write script to a temp file
        let script_path = "/tmp/nasty-update.sh";
        tokio::fs::write(script_path, &script).await
            .map_err(|e| UpdateError::CommandFailed(format!("failed to write update script: {e}")))?;

        // Launch as a transient systemd service
        // This avoids the engine's ProtectSystem restrictions
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args([
                "--unit",
                UPDATE_UNIT,
                "--no-block",
                "--description",
                "NASty system update",
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
            ]);

        // Pass engine's PATH so the script can find git, nixos-rebuild, etc.
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.args(["--setenv", &format!("PATH={path}")]);

        if !token_env.is_empty() {
            cmd.args(["--setenv", &format!("NIX_CONFIG={token_env}")]);
        }

        cmd.args([
                "--",
                "bash",
                script_path,
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

        let path = std::env::var("PATH").unwrap_or_default();
        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit",
                UPDATE_UNIT,
                "--no-block",
                "--description",
                "NASty system rollback",
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--setenv",
                &format!("PATH={path}"),
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

    /// Reboot the system
    pub async fn reboot(&self) -> Result<(), UpdateError> {
        info!("System reboot requested");
        let output = tokio::process::Command::new("systemctl")
            .arg("reboot")
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemctl reboot: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "reboot failed: {stderr}"
            )));
        }
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

        // Get the current invocation ID to only show logs from this run
        let invocation_id = tokio::process::Command::new("systemctl")
            .args(["show", UPDATE_UNIT, "--property=InvocationID", "--value"])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        // Read journal output for the update unit (current invocation only)
        let mut journal_args = vec![
            "-u".to_string(),
            UPDATE_UNIT.to_string(),
            "--no-pager".to_string(),
            "--output=cat".to_string(),
        ];
        if !invocation_id.is_empty() {
            journal_args.push(format!("_SYSTEMD_INVOCATION_ID={invocation_id}"));
        } else {
            journal_args.extend(["-n".to_string(), "200".to_string()]);
        }

        let log = tokio::process::Command::new("journalctl")
            .args(&journal_args)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        UpdateStatus {
            state,
            log,
            reboot_required: is_reboot_required().await,
        }
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

/// Check if the booted kernel differs from the activated system's kernel.
/// On NixOS, /run/booted-system is the system we booted into and
/// /run/current-system is the latest activated profile (after nixos-rebuild switch).
async fn is_reboot_required() -> bool {
    let booted = tokio::fs::read_link("/run/booted-system/kernel").await;
    let current = tokio::fs::read_link("/run/current-system/kernel").await;
    match (booted, current) {
        (Ok(b), Ok(c)) => b != c,
        _ => false,
    }
}

/// TODO: Remove once repo is public.
async fn read_github_token() -> Option<String> {
    tokio::fs::read_to_string(GITHUB_TOKEN_PATH)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
