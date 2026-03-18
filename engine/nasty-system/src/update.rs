use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

/// Primary version path — writable by the update script, not managed by NixOS.
const VERSION_PATH: &str = "/var/lib/nasty/version";
/// Fallback version path — baked in by NixOS at build time (may be a local SHA).
const VERSION_PATH_FALLBACK: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const LOCAL_FLAKE: &str = "/etc/nixos/nixos#nasty";
const REPO_URL: &str = "https://github.com/nasty-project/nasty.git";
const LOCAL_REPO: &str = "/etc/nixos";
const BCACHEFS_SWITCH_UNIT: &str = "nasty-bcachefs-switch";
const NIXOS_FLAKE_DIR: &str = "/etc/nixos/nixos";
const BCACHEFS_TOOLS_REPO: &str = "github:koverstreet/bcachefs-tools";
const BCACHEFS_REF_STATE: &str = "/var/lib/nasty/bcachefs-tools-ref";
const BCACHEFS_SWITCH_RESULT: &str = "/var/lib/nasty/bcachefs-switch-result";
const UPDATE_WEBUI_CHANGED: &str = "/var/lib/nasty/update-webui-changed";

// TODO: Remove token-based auth once the repo is public.
// The token file is only needed for private repo access.
// When removing, delete check_via_github_api(), GITHUB_TOKEN_PATH,
// and revert check() to use git ls-remote directly.
const GITHUB_TOKEN_PATH: &str = "/var/lib/nasty/github-token";
const GITHUB_API_REPO: &str = "https://api.github.com/repos/nasty-project/nasty/commits/main";

#[derive(Debug, Serialize, JsonSchema)]
pub struct BcachefsToolsInfo {
    /// The ref in flake.lock original (e.g. "v1.37.0", "master", commit sha)
    pub pinned_ref: Option<String>,
    /// The resolved full commit sha from flake.lock locked
    pub pinned_rev: Option<String>,
    /// Version of the bcachefs kernel module currently loaded (from modinfo)
    pub running_version: String,
    /// True when the user has overridden the default bcachefs-tools version
    pub is_custom: bool,
    /// The default ref from flake.nix (e.g. "v1.37.0")
    pub default_ref: String,
    /// Whether the running kernel was built with Rust support (CONFIG_RUST=y)
    pub kernel_rust: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BcachefsToolsSwitchRequest {
    /// A git ref: tag (v1.37.0), branch (master), or commit hash
    pub git_ref: String,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("update already in progress")]
    AlreadyRunning,
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpdateInfo {
    /// Currently installed version (short commit SHA or `dev`).
    pub current_version: String,
    /// Latest upstream version, if the check has been performed.
    pub latest_version: Option<String>,
    /// Whether a newer version is available. None if the check has not been run yet.
    pub update_available: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpdateStatus {
    /// "idle", "running", "success", "failed"
    pub state: String,
    pub log: String,
    /// True when the activated system has a different kernel than the booted one
    pub reboot_required: bool,
    /// True when the webui store path changed during this update (browser reload needed)
    pub webui_changed: bool,
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
            Err(_) => {
                let token = read_github_token().await;
                check_via_git_ls_remote(token.as_deref()).await?
            }
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

        // Sanitize hardware-configuration.nix before updating.
        // If the user ever ran nixos-generate-config while pools were mounted,
        // those fileSystems entries would block boot after pool destruction.
        sanitize_hardware_config().await;

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

        // TODO: Remove token env var once repo is public.
        let token_env = token.as_ref()
            .map(|t| format!("access-tokens = github.com={t}"))
            .unwrap_or_default();

        // Build the git credential config for non-interactive auth (engine has no TTY).
        // url.insteadOf rewrites the remote URL so x-access-token auth works without prompts.
        let git_insteadof = token.as_ref()
            .map(|t| format!("-c \"url.https://x-access-token:{t}@github.com/.insteadOf=https://github.com/\""))
            .unwrap_or_default();

        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
# Capture current webui store path before rebuild so we can detect if it changed.
# Read from /run/current-system/etc/systemd/system/nginx.service — the unit file
# uses single-quoted paths so the regex terminates cleanly at the closing quote.
# After nixos-rebuild switch the /run/current-system symlink is updated before we
# read the AFTER value, so we always compare old vs new closure.
_nginx_conf() {{
    grep -o "/nix/store/[^' ]*nginx\.conf" \
        /run/current-system/etc/systemd/system/nginx.service 2>/dev/null | head -1 || true
}}
_NGINX_CONF_BEFORE=$(_nginx_conf)
WEBUI_BEFORE=$([ -n "$_NGINX_CONF_BEFORE" ] && grep 'nasty-webui' "$_NGINX_CONF_BEFORE" 2>/dev/null | head -1 || echo "")
echo "==> Pulling latest source..."
cd {LOCAL_REPO}

# Preserve machine-specific hardware config
HW_CFG="nixos/hardware-configuration.nix"
[ -f "$HW_CFG" ] && cp "$HW_CFG" /tmp/nasty-hw-config.nix

git remote set-url origin "{REPO_URL}" 2>/dev/null || git remote add origin "{REPO_URL}"
GIT_TERMINAL_PROMPT=0 git -c credential.helper= {git_insteadof} fetch origin

# Ensure only appliance-relevant directories are materialized.
# This removes tests/, CLAUDE.md, build-iso.sh etc. on existing installs
# and keeps new ones clean going forward.
git sparse-checkout init --cone
git sparse-checkout set engine webui nixos

git reset --hard origin/main

# Restore hardware config
[ -f /tmp/nasty-hw-config.nix ] && cp /tmp/nasty-hw-config.nix "$HW_CFG"

# Re-apply custom bcachefs-tools version if the user has set one
if [ -f "{BCACHEFS_REF_STATE}" ]; then
    BCACHEFS_REF=$(cat "{BCACHEFS_REF_STATE}")
    echo "==> Re-applying custom bcachefs-tools: $BCACHEFS_REF..."
    cd {NIXOS_FLAKE_DIR}
    nix flake lock --override-input bcachefs-tools "{BCACHEFS_TOOLS_REPO}/$BCACHEFS_REF"
    cd {LOCAL_REPO}
fi

# Flakes require all files to be tracked; commit so the tree is clean (no dirty warning)
git add -A
git -c user.email="nasty@localhost" -c user.name="NASty" \
  commit -m "local: hardware-configuration.nix" || true

echo "==> Rebuilding system..."
nixos-rebuild switch --flake {LOCAL_FLAKE}

# Detect if the webui store path changed so the frontend knows whether to prompt a reload.
# /run/current-system now points to the newly activated closure.
_NGINX_CONF_AFTER=$(_nginx_conf)
WEBUI_AFTER=$([ -n "$_NGINX_CONF_AFTER" ] && grep 'nasty-webui' "$_NGINX_CONF_AFTER" 2>/dev/null | head -1 || echo "")
if [ -n "$WEBUI_BEFORE" ] && [ "$WEBUI_BEFORE" != "$WEBUI_AFTER" ]; then
    echo "true" > {UPDATE_WEBUI_CHANGED}
else
    echo "false" > {UPDATE_WEBUI_CHANGED}
fi

# Write the upstream SHA to the writable version path.
# The flake bakes the local hw-config commit SHA into /etc/nasty-version, which
# never matches origin/main. Writing the real upstream SHA to /var/lib/nasty/version
# lets the engine report the correct version and stop showing false update prompts.
git rev-parse --short origin/main > {VERSION_PATH}

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
    /// Returns true if the booted kernel or kernel modules differ from the current system.
    /// Indicates that a reboot is needed to activate a kernel or driver update.
    pub async fn reboot_required(&self) -> bool {
        is_reboot_required().await
    }

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

    pub async fn shutdown(&self) -> Result<(), UpdateError> {
        info!("System shutdown requested");
        let output = tokio::process::Command::new("systemctl")
            .arg("poweroff")
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemctl poweroff: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "shutdown failed: {stderr}"
            )));
        }
        Ok(())
    }

    pub async fn bcachefs_info(&self) -> BcachefsToolsInfo {
        let (_, kernel_rust) = bcachefs_version().await;
        let running_version = bcachefs_loaded_module_version().await;
        let (lock_ref, pinned_rev) = read_flake_lock_bcachefs().await;
        let default_ref = read_flake_nix_default_ref().await;
        // Use the state file as the canonical display ref when the user has switched.
        // flake.lock's original.ref always mirrors flake.nix (not updated by --override-input),
        // so it would show the old version even after a successful switch to a new rev.
        let state_ref = tokio::fs::read_to_string(BCACHEFS_REF_STATE).await
            .ok()
            .map(|s| {
                let s = s.trim().to_string();
                // Full 40-char SHA saved by the switch script — truncate for display
                if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                    s[..12].to_string()
                } else {
                    s
                }
            })
            .filter(|s| !s.is_empty());
        let pinned_ref = state_ref.clone().or(lock_ref);
        let is_custom = state_ref.as_deref().map(|r| r != default_ref).unwrap_or(false);
        BcachefsToolsInfo { pinned_ref, pinned_rev, running_version, is_custom, default_ref, kernel_rust }
    }

    pub async fn bcachefs_switch(&self, req: BcachefsToolsSwitchRequest) -> Result<(), UpdateError> {
        // Refuse if either update unit is running
        let update_status = self.status().await;
        let switch_status = self.bcachefs_status().await;
        if update_status.state == "running" || switch_status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        let git_ref = req.git_ref.trim().to_string();
        if git_ref.is_empty() {
            return Err(UpdateError::CommandFailed("git_ref must not be empty".into()));
        }

        // Clean up previous unit and result file
        for action in &["reset-failed", "stop"] {
            let _ = tokio::process::Command::new("systemctl")
                .args([action, BCACHEFS_SWITCH_UNIT])
                .output().await;
        }
        let _ = tokio::fs::remove_file(BCACHEFS_SWITCH_RESULT).await;

        let default_ref = read_flake_nix_default_ref().await;
        let is_default = git_ref == default_ref;

        // Persist the chosen ref so regular updates can re-apply it.
        // State file is written by the script after resolving the ref to a commit SHA.
        // For the default ref we clear it here; for custom refs the script overwrites it
        // with the pinned SHA so branch names like "master" don't drift on future updates.
        if is_default {
            let _ = tokio::fs::remove_file(BCACHEFS_REF_STATE).await;
        }

        let input_url = format!("{BCACHEFS_TOOLS_REPO}/{git_ref}");
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
# Write 'failed' up front; overwritten with 'success' at the end.
# Survives engine restarts so polling can read the outcome even after
# the transient systemd unit has been garbage-collected.
echo "failed" > {BCACHEFS_SWITCH_RESULT}
SWITCH_LOG=/var/lib/nasty/bcachefs-switch.log
PREV_REF=$(cat {BCACHEFS_REF_STATE} 2>/dev/null || echo "{default_ref}")
printf '%s  started  %s  (was: %s)\n' "$(date -u '+%Y-%m-%d %H:%M:%S')" "{git_ref}" "$PREV_REF" >> "$SWITCH_LOG"
echo "==> Switching bcachefs-tools to {git_ref}..."
cd {NIXOS_FLAKE_DIR}
nix flake lock --override-input bcachefs-tools "{input_url}"
# Resolve the symbolic ref (e.g. "master") to the exact commit SHA that was
# just pinned in flake.lock. Store the SHA so future system updates re-use
# the same commit rather than advancing with the branch tip.
RESOLVED_SHA=$(jq -r '.nodes["bcachefs-tools"].locked.rev' flake.lock 2>/dev/null || true)
if [ "{git_ref}" != "{default_ref}" ]; then
    if echo "{git_ref}" | grep -qE '^v[0-9]'; then
        echo "{git_ref}" > {BCACHEFS_REF_STATE}
        echo "==> Pinned to tag {git_ref}"
    elif [ -n "$RESOLVED_SHA" ]; then
        echo "$RESOLVED_SHA" > {BCACHEFS_REF_STATE}
        echo "==> Pinned to commit $RESOLVED_SHA"
    fi
fi
# Commit the updated flake.lock so the tree stays clean for the next rebuild
git add flake.lock
git -c user.email="nasty@localhost" -c user.name="NASty" \
  commit -m "bcachefs-tools: switch to {git_ref} (${{RESOLVED_SHA:-unknown}})" || true
echo "==> Rebuilding system..."
nixos-rebuild switch --flake {LOCAL_FLAKE}
echo "success" > {BCACHEFS_SWITCH_RESULT}
printf '%s  success  %s\n' "$(date -u '+%Y-%m-%d %H:%M:%S')" "{git_ref}" >> "$SWITCH_LOG"
echo "==> bcachefs-tools switch complete!"
"#
        );

        let script_path = "/tmp/nasty-bcachefs-switch.sh";
        tokio::fs::write(script_path, &script).await
            .map_err(|e| UpdateError::CommandFailed(format!("write script: {e}")))?;

        let path = std::env::var("PATH").unwrap_or_default();
        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit", BCACHEFS_SWITCH_UNIT,
                "--no-block",
                "--description", "NASty bcachefs-tools version switch",
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--setenv", &format!("PATH={path}"),
                "--", "bash", script_path,
            ])
            .output().await
            .map_err(|e| UpdateError::CommandFailed(format!("systemd-run: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!("failed to start: {stderr}")));
        }
        info!("bcachefs-tools switch to {git_ref} started");
        Ok(())
    }

    pub async fn bcachefs_status(&self) -> UpdateStatus {
        let output = tokio::process::Command::new("systemctl")
            .args(["show", BCACHEFS_SWITCH_UNIT, "--property=ActiveState,SubState,Result"])
            .output().await;

        let state = match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let mut active_state = "";
                let mut result = "";
                for line in text.lines() {
                    if let Some(val) = line.strip_prefix("ActiveState=") { active_state = val.trim(); }
                    if let Some(val) = line.strip_prefix("Result=") { result = val.trim(); }
                }
                match active_state {
                    "active" | "activating" | "reloading" => "running".to_string(),
                    _ => {
                        // Unit finished, missing, or never ran.
                        // systemd Result=success is reliable when the unit still exists;
                        // for cleaned-up or missing units it may be empty.
                        // The result file is the authoritative fallback: the script writes
                        // "failed" at the top and overwrites with "success" at the bottom,
                        // so it always reflects the true outcome.
                        // We remove it after reading a terminal state so stale results
                        // don't surface on the next page load.
                        let state = if result == "success" {
                            "success".to_string()
                        } else if active_state == "failed" {
                            "failed".to_string()
                        } else {
                            tokio::fs::read_to_string(BCACHEFS_SWITCH_RESULT).await
                                .ok()
                                .map(|s| match s.trim() {
                                    "success" => "success".to_string(),
                                    "failed"  => "failed".to_string(),
                                    _         => "idle".to_string(),
                                })
                                .unwrap_or_else(|| "idle".to_string())
                        };
                        if state == "success" || state == "failed" {
                            let _ = tokio::fs::remove_file(BCACHEFS_SWITCH_RESULT).await;
                        }
                        state
                    }
                }
            }
            Err(_) => "idle".to_string(),
        };

        let invocation_id = tokio::process::Command::new("systemctl")
            .args(["show", BCACHEFS_SWITCH_UNIT, "--property=InvocationID", "--value"])
            .output().await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        let mut journal_args = vec![
            "-u".to_string(), BCACHEFS_SWITCH_UNIT.to_string(),
            "--no-pager".to_string(), "--output=cat".to_string(),
        ];
        if !invocation_id.is_empty() {
            journal_args.push(format!("_SYSTEMD_INVOCATION_ID={invocation_id}"));
        } else {
            journal_args.extend(["-n".to_string(), "200".to_string()]);
        }

        let log = tokio::process::Command::new("journalctl")
            .args(&journal_args).output().await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        UpdateStatus { state, log, reboot_required: is_reboot_required().await, webui_changed: false }
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

        // Read hint file written by the update script.
        // Default to true when state is success and the file is missing (conservative).
        let webui_changed = if state == "success" {
            tokio::fs::read_to_string(UPDATE_WEBUI_CHANGED).await
                .ok()
                .map(|s| s.trim() == "true")
                .unwrap_or(true)
        } else {
            false
        };

        UpdateStatus {
            state,
            log,
            reboot_required: is_reboot_required().await,
            webui_changed,
        }
    }
}

/// Remove any `fileSystems."/storage/..."` blocks from hardware-configuration.nix.
///
/// Pool mounts are managed at runtime by the engine. If a user ran
/// `nixos-generate-config` while pools were mounted, those entries end up in
/// hardware-configuration.nix and will block boot after the pool is destroyed
/// (systemd waits forever for a device UUID that no longer exists).
async fn sanitize_hardware_config() {
    let path = format!("{LOCAL_REPO}/nixos/hardware-configuration.nix");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return,
    };
    let sanitized = strip_pool_mounts(&content);
    if sanitized != content {
        info!("Removed pool mount entries from hardware-configuration.nix to prevent boot failure");
        if let Err(e) = tokio::fs::write(&path, sanitized).await {
            tracing::warn!("Failed to write sanitized hardware-configuration.nix: {e}");
        }
    }
}

fn strip_pool_mounts(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut skip = false;
    let mut depth = 0i32;
    for line in content.lines() {
        if !skip && line.trim_start().starts_with("fileSystems.\"/storage/") {
            skip = true;
            depth = 0;
        }
        if skip {
            for c in line.chars() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth <= 0 {
                            skip = false;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
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

    let body: serde_json::Value = reqwest::Client::new()
        .get(GITHUB_API_REPO)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", "nasty-engine")
        .send()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("GitHub API request failed: {e}")))?
        .json()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("failed to parse GitHub response: {e}")))?;

    let sha = body["sha"]
        .as_str()
        .map(|s| s[..7.min(s.len())].to_string())
        .ok_or_else(|| UpdateError::CommandFailed("no sha in GitHub response".into()))?;

    Ok(sha)
}

/// Direct git ls-remote — works for public repos without auth.
/// If a token is provided, uses url.insteadOf for non-interactive x-access-token auth.
async fn check_via_git_ls_remote(token: Option<&str>) -> Result<String, UpdateError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args(["-c", "credential.helper="]);
    if let Some(t) = token {
        cmd.arg("-c").arg(format!(
            "url.https://x-access-token:{t}@github.com/.insteadOf=https://github.com/"
        ));
    }
    cmd.args(["ls-remote", REPO_URL, "refs/heads/main"]);

    let output = cmd
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
    // Prefer the writable version written by the update script (contains the real
    // upstream SHA). Fall back to the NixOS-baked /etc/nasty-version which may
    // contain a local hw-config commit SHA and is therefore less reliable.
    for path in &[VERSION_PATH, VERSION_PATH_FALLBACK] {
        if let Ok(s) = tokio::fs::read_to_string(path).await {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "dev".to_string()
}

/// Check if the booted kernel or kernel modules differ from the activated system.
/// On NixOS, /run/booted-system is the system we booted into and
/// /run/current-system is the latest activated profile (after nixos-rebuild switch).
/// kernel-modules includes boot.extraModulePackages (e.g. the bcachefs DKMS module),
/// so this catches module-only changes such as a new bcachefs build.
async fn is_reboot_required() -> bool {
    let paths = [
        ("/run/booted-system/kernel", "/run/current-system/kernel"),
        ("/run/booted-system/kernel-modules", "/run/current-system/kernel-modules"),
    ];
    for (booted_path, current_path) in paths {
        let booted = tokio::fs::read_link(booted_path).await;
        let current = tokio::fs::read_link(current_path).await;
        if let (Ok(b), Ok(c)) = (booted, current) {
            if b != c {
                return true;
            }
        }
    }
    false
}

/// TODO: Remove once repo is public.
async fn read_github_token() -> Option<String> {
    tokio::fs::read_to_string(GITHUB_TOKEN_PATH)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run `bcachefs version` and return the version string, or "unknown" on failure.
/// Strips trailing noise like "kernel: unable to read kernel config".
/// Returns (version_string, kernel_rust).
/// `bcachefs version` may emit extra lines, e.g.:
///   "1.37.1\nkernel: CONFIG_RUST=y"
///   "1.37.0\nkernel: unable to read kernel config"
/// Returns the version of the bcachefs kernel module that is currently loaded,
/// by reading the version field from modinfo. This is the authoritative running
/// version — it reflects what is actually mounted and active, not what is
/// installed in current-system (which may differ when a reboot is pending).
async fn bcachefs_loaded_module_version() -> String {
    tokio::process::Command::new("modinfo")
        .args(["bcachefs", "--field", "version"])
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if v.is_empty() { None } else { Some(v) }
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

async fn bcachefs_version() -> (String, Option<bool>) {
    let raw = tokio::process::Command::new("bcachefs")
        .arg("version")
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    let version = raw.split_whitespace().next().unwrap_or("unknown").to_string();

    // Parse optional "kernel: CONFIG_RUST=y" / "kernel: CONFIG_RUST=n" line
    let kernel_rust = raw.lines()
        .find(|l| l.contains("CONFIG_RUST"))
        .map(|l| l.contains("CONFIG_RUST=y"));

    (version, kernel_rust)
}

/// Parse flake.nix to extract the default bcachefs-tools ref from the input URL.
async fn read_flake_nix_default_ref() -> String {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("bcachefs-tools.url") {
            // e.g. bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.37.0";
            if let Some(slash_pos) = line.rfind('/') {
                let rest = &line[slash_pos + 1..];
                let end = rest.find('"').unwrap_or(rest.len());
                return rest[..end].to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Parse flake.lock to extract the bcachefs-tools pinned ref and rev.
async fn read_flake_lock_bcachefs() -> (Option<String>, Option<String>) {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.lock");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let node = &v["nodes"]["bcachefs-tools"];
    let pinned_ref = node["original"]["ref"].as_str().map(|s| s.to_string());
    let pinned_rev = node["locked"]["rev"].as_str()
        .map(|s| s[..s.len().min(12)].to_string()); // short rev, 12 chars
    (pinned_ref, pinned_rev)
}
