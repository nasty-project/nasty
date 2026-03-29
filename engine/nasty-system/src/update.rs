use std::sync::Arc;
use tokio::sync::RwLock;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

/// Primary version path — writable by the update script, not managed by NixOS.
const VERSION_PATH: &str = "/var/lib/nasty/version";
/// Fallback version path — baked in by NixOS at build time (may be a local SHA).
const VERSION_PATH_FALLBACK: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const LOCAL_FLAKE_DIR: &str = "/etc/nixos/nixos";
const SYSTEM_CONFIG_PATH: &str = "/var/lib/nasty/system-config";
const DEFAULT_CONFIG: &str = "nasty";
const REPO_URL: &str = "https://github.com/nasty-project/nasty.git";
const LOCAL_REPO: &str = "/etc/nixos";
const BCACHEFS_SWITCH_UNIT: &str = "nasty-bcachefs-switch";
const NIXOS_FLAKE_DIR: &str = "/etc/nixos/nixos";
const BCACHEFS_TOOLS_REPO: &str = "github:koverstreet/bcachefs-tools";
const BCACHEFS_REF_STATE: &str = "/var/lib/nasty/bcachefs-tools-ref";
const BCACHEFS_DEBUG_CHECKS_STATE: &str = "/var/lib/nasty/bcachefs-debug-checks";
const BCACHEFS_SWITCH_RESULT: &str = "/var/lib/nasty/bcachefs-switch-result";
const UPDATE_WEBUI_CHANGED: &str = "/var/lib/nasty/update-webui-changed";
const RELEASE_CHANNEL_PATH: &str = "/var/lib/nasty/release-channel";
const GC_CONFIG_PATH: &str = "/var/lib/nasty/gc-config.json";

// ── Garbage collection config ────────────────────────────────────

/// Configuration for NixOS generation garbage collection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GcConfig {
    /// Minimum number of generations to keep (default 10).
    #[serde(default = "default_keep_generations")]
    pub keep_generations: u32,
    /// Delete generations older than this many days (0 = disabled).
    /// `keep_generations` is always respected as a minimum.
    #[serde(default)]
    pub max_age_days: u32,
}

fn default_keep_generations() -> u32 { 20 }

impl Default for GcConfig {
    fn default() -> Self {
        Self { keep_generations: 20, max_age_days: 0 }
    }
}

impl GcConfig {
    pub fn load() -> Self {
        std::fs::read_to_string(GC_CONFIG_PATH)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default()
    }

    pub async fn save(&self) -> Result<(), UpdateError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| UpdateError::CommandFailed(e.to_string()))?;
        tokio::fs::write(GC_CONFIG_PATH, json).await
            .map_err(|e| UpdateError::CommandFailed(e.to_string()))
    }
}

// ── Release channels ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseChannel {
    /// Tagged releases only. Safe, tested, boring.
    Mild,
    /// Pre-release branch. New features, occasional heartburn.
    Spicy,
    /// Latest main branch. Bleeding edge — you asked for it.
    Nasty,
}

impl ReleaseChannel {
    /// Git ref to track for this channel.
    pub fn git_ref(&self) -> &'static str {
        match self {
            Self::Mild => "stable",
            Self::Spicy => "beta",
            Self::Nasty => "main",
        }
    }

    /// GitHub API endpoint for checking latest commit.
    pub fn github_api_url(&self) -> String {
        match self {
            Self::Mild => "https://api.github.com/repos/nasty-project/nasty/releases/latest".to_string(),
            _ => format!("https://api.github.com/repos/nasty-project/nasty/commits/{}", self.git_ref()),
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Mild => "Mild",
            Self::Spicy => "Spicy",
            Self::Nasty => "Nasty",
        }
    }
}

impl std::fmt::Display for ReleaseChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mild => write!(f, "mild"),
            Self::Spicy => write!(f, "spicy"),
            Self::Nasty => write!(f, "nasty"),
        }
    }
}

impl std::str::FromStr for ReleaseChannel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "mild" => Ok(Self::Mild),
            "spicy" => Ok(Self::Spicy),
            "nasty" => Ok(Self::Nasty),
            other => Err(format!("unknown channel: {other}")),
        }
    }
}

pub async fn read_channel() -> ReleaseChannel {
    tokio::fs::read_to_string(RELEASE_CHANNEL_PATH)
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(ReleaseChannel::Nasty)
}

async fn write_channel(channel: ReleaseChannel) -> Result<(), std::io::Error> {
    tokio::fs::write(RELEASE_CHANNEL_PATH, channel.to_string()).await
}

// TODO: Remove token-based auth once the repo is public.
// The token file is only needed for private repo access.
// When removing, delete check_via_github_api(), GITHUB_TOKEN_PATH,
// and revert check() to use git ls-remote directly.
const GITHUB_TOKEN_PATH: &str = "/var/lib/nasty/github-token";


#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BcachefsToolsInfo {
    /// The ref in flake.lock original (e.g. "v1.37.0", "master", commit sha)
    pub pinned_ref: Option<String>,
    /// The resolved full commit sha from flake.lock locked
    pub pinned_rev: Option<String>,
    /// Version of the bcachefs kernel module currently loaded (from modinfo)
    pub running_version: String,
    /// True when the user has configured a non-default bcachefs-tools version
    pub is_custom: bool,
    /// True when the actually loaded module differs from the default version
    pub is_custom_running: bool,
    /// The default ref from flake.nix (e.g. "v1.37.0")
    pub default_ref: String,
    /// Whether the running kernel was built with Rust support (CONFIG_RUST=y)
    pub kernel_rust: Option<bool>,
    /// Whether the loaded module has debug symbols (-g)
    pub debug_symbols: bool,
    /// Whether debug checks are configured for the next build
    pub debug_checks: bool,
    /// Whether the loaded module has CONFIG_BCACHEFS_DEBUG
    pub debug_checks_running: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BcachefsToolsSwitchRequest {
    /// A git ref: tag (v1.37.0), branch (master), or commit hash
    pub git_ref: String,
    /// Build with CONFIG_BCACHEFS_DEBUG for extra runtime assertions. Has performance cost.
    #[serde(default)]
    pub debug_checks: bool,
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
    /// Active release channel.
    pub channel: ReleaseChannel,
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

// ── Generation management ──────────────────────────────────────

const GENERATION_LABELS_PATH: &str = "/var/lib/nasty/generation-labels.json";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Generation {
    /// NixOS generation number.
    pub generation: u64,
    /// Build date (e.g. "2026-03-21 11:15:37").
    pub date: String,
    /// NixOS version string (e.g. "26.05.20260318.b40629e").
    pub nixos_version: String,
    /// Kernel version string.
    pub kernel_version: String,
    /// NASty version baked into this generation (from /etc/nasty-version).
    pub nasty_version: Option<String>,
    /// Whether this is the currently activated generation.
    pub current: bool,
    /// Whether this is the generation the system booted into.
    pub booted: bool,
    /// User-assigned label (e.g. "known good", "stable").
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NixosGeneration {
    generation: u64,
    date: String,
    #[serde(rename = "nixosVersion")]
    nixos_version: String,
    #[serde(rename = "kernelVersion")]
    kernel_version: String,
    current: bool,
}

pub struct UpdateService {
    cached_info: Arc<RwLock<Option<BcachefsToolsInfo>>>,
}

impl UpdateService {
    pub fn new() -> Self {
        Self {
            cached_info: Arc::new(RwLock::new(None)),
        }
    }

    /// Invalidate cached bcachefs info — call after switch or rebuild.
    pub async fn invalidate_bcachefs_cache(&self) {
        *self.cached_info.write().await = None;
    }

    /// Get current installed version
    pub async fn version(&self) -> UpdateInfo {
        UpdateInfo {
            current_version: read_current_version().await,
            latest_version: None,
            update_available: None,
            channel: read_channel().await,
        }
    }

    /// Get or set the release channel.
    pub async fn get_channel(&self) -> ReleaseChannel {
        read_channel().await
    }

    pub async fn set_channel(&self, channel: ReleaseChannel) -> Result<ReleaseChannel, UpdateError> {
        write_channel(channel).await
            .map_err(|e| UpdateError::CommandFailed(format!("write channel: {e}")))?;
        info!("Release channel set to {}", channel.display_name());
        Ok(channel)
    }

    /// Check if an update is available by comparing local rev to GitHub
    pub async fn check(&self) -> Result<UpdateInfo, UpdateError> {
        let current = read_current_version().await;
        let channel = read_channel().await;

        // For stable channel, check latest GitHub release tag.
        // For beta/edge, check the branch commit.
        let latest = match channel {
            ReleaseChannel::Mild => {
                match check_latest_release().await {
                    Ok(tag) => tag,
                    Err(_) => {
                        let token = read_github_token().await;
                        check_via_git_ls_remote(token.as_deref(), "refs/heads/stable").await?
                    }
                }
            }
            _ => {
                let git_ref = format!("refs/heads/{}", channel.git_ref());
                match check_via_github_api_branch(channel.git_ref()).await {
                    Ok(sha) => sha,
                    Err(_) => {
                        let token = read_github_token().await;
                        check_via_git_ls_remote(token.as_deref(), &git_ref).await?
                    }
                }
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
            channel,
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
        // those fileSystems entries would block boot after filesystem destruction.
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
        let channel = read_channel().await;
        let branch = channel.git_ref();
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

        let local_flake = local_flake().await;
        let gc = GcConfig::load();
        let gc_keep = gc.keep_generations;
        let gc_max_age = gc.max_age_days;
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
CHANNEL={branch}
echo "==> Channel: $CHANNEL"
GIT_TERMINAL_PROMPT=0 git -c credential.helper= {git_insteadof} fetch origin

# Disable sparse checkout — Nix treats missing tracked files as dirty,
# which causes every build to get a "-dirty" version suffix.
git sparse-checkout disable 2>/dev/null || true

# For stable channel, checkout the latest tag on the stable branch.
# For beta/edge, track the branch head.
if [ "$CHANNEL" = "stable" ]; then
    LATEST_TAG=$(git tag -l 'v*' --sort=-v:refname | head -1)
    if [ -n "$LATEST_TAG" ]; then
        echo "==> Checking out release $LATEST_TAG"
        git checkout "$LATEST_TAG" --detach
    else
        echo "==> No release tags found, falling back to origin/stable"
        git reset --hard origin/stable
    fi
else
    git reset --hard origin/$CHANNEL
fi

# Remove dev-only files not needed on the appliance.
# These stay in the repo for CI/dev but shouldn't ship to users.
for f in .github .claude CLAUDE.md; do
    [ -e "$f" ] && git rm -rf --quiet "$f" 2>/dev/null || true
done

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

# Re-apply debug checks flag if the user had it enabled
if [ -f "{BCACHEFS_DEBUG_CHECKS_STATE}" ]; then
    echo "==> Re-applying bcachefs debug checks..."
    cd {NIXOS_FLAKE_DIR}
    sed -i 's|.*@NASTY_DEBUG_CHECKS_LINE@.*|                echo "\tccflags-y += -DCONFIG_BCACHEFS_DEBUG" >> src/fs/bcachefs/Makefile  # @NASTY_DEBUG_CHECKS_LINE@|' flake.nix
    cd {LOCAL_REPO}
fi

# Commit local changes (hw-config, removed dev files) so the tree is clean for Nix
git add -A
git -c user.email="nasty@localhost" -c user.name="NASty" \
  commit -m "local: appliance adjustments" || true

# Garbage-collect old generations.
# The booted generation is always protected — even if it's older than KEEP.
KEEP={gc_keep}
MAX_AGE={gc_max_age}
# Garbage-collect old NixOS generations.
# nix-env --list-generations is the reliable way to enumerate them.
GEN_LIST=$(nix-env --profile /nix/var/nix/profiles/system --list-generations 2>/dev/null | awk '{{print $1}}' || true)
GENS=$(echo "$GEN_LIST" | grep -c '[0-9]' || echo 0)
if [ "$GENS" -gt "$KEEP" ]; then
    DELETE_COUNT=$(( GENS - KEEP ))
    TO_DELETE=$(echo "$GEN_LIST" | head -n "$DELETE_COUNT" | tr '\n' ' ')
    if [ -n "$TO_DELETE" ]; then
        echo "==> Cleaning up old generations ($GENS found, keeping $KEEP, deleting $DELETE_COUNT)..."
        # shellcheck disable=SC2086
        nix-env --profile /nix/var/nix/profiles/system --delete-generations $TO_DELETE 2>&1 || true
        nix-collect-garbage 2>&1 || true
    fi
fi

echo "==> Rebuilding system..."
NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}

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
if [ "$CHANNEL" = "stable" ] && [ -n "$LATEST_TAG" ]; then
    echo "$LATEST_TAG" > {VERSION_PATH}
else
    git rev-parse --short origin/$CHANNEL > {VERSION_PATH}
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

    // ── Generation management ──────────────────────────────

    /// List all NixOS generations with metadata and labels.
    pub async fn list_generations(&self) -> Result<Vec<Generation>, UpdateError> {
        let output = tokio::process::Command::new("nixos-rebuild")
            .args(["list-generations", "--json"])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("nixos-rebuild list-generations: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "list-generations failed: {stderr}"
            )));
        }

        let nix_gens: Vec<NixosGeneration> = serde_json::from_slice(&output.stdout)
            .map_err(|e| UpdateError::CommandFailed(format!("parse generations: {e}")))?;

        // Load user labels
        let labels = load_generation_labels().await;

        // Find booted generation by comparing /run/booted-system symlink
        let booted_store_path = tokio::fs::read_link("/run/booted-system").await.ok();

        let mut generations = Vec::new();
        for g in nix_gens {
            // Read NASty version from this generation's profile
            let profile_path = format!(
                "/nix/var/nix/profiles/system-{}-link/etc/nasty-version",
                g.generation
            );
            let nasty_version = tokio::fs::read_to_string(&profile_path)
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            // Check if this generation is the booted one
            let gen_store_path = tokio::fs::read_link(format!(
                "/nix/var/nix/profiles/system-{}-link",
                g.generation
            ))
            .await
            .ok();

            let booted = match (&booted_store_path, &gen_store_path) {
                (Some(b), Some(g)) => b == g,
                _ => false,
            };

            let label = labels.get(&g.generation).cloned();

            generations.push(Generation {
                generation: g.generation,
                date: g.date,
                nixos_version: g.nixos_version,
                kernel_version: g.kernel_version,
                nasty_version,
                current: g.current,
                booted,
                label,
            });
        }

        Ok(generations)
    }

    /// Switch to a specific NixOS generation.
    pub async fn switch_generation(&self, gen_id: u64) -> Result<(), UpdateError> {
        let status = self.status().await;
        if status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        // Verify the generation exists
        let profile_link = format!("/nix/var/nix/profiles/system-{gen_id}-link");
        if tokio::fs::metadata(&profile_link).await.is_err() {
            return Err(UpdateError::CommandFailed(format!(
                "generation {gen_id} does not exist"
            )));
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
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
echo "==> Switching to generation {gen_id}..."
nix-env --switch-generation {gen_id} --profile /nix/var/nix/profiles/system
echo "==> Activating generation {gen_id}..."
/nix/var/nix/profiles/system/bin/switch-to-configuration switch
echo "==> Switch to generation {gen_id} complete!"
"#
        );

        let script_path = "/tmp/nasty-switch-generation.sh";
        tokio::fs::write(script_path, &script)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("write script: {e}")))?;

        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit", UPDATE_UNIT,
                "--no-block",
                "--description", &format!("NASty switch to generation {gen_id}"),
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--setenv", &format!("PATH={path}"),
                "--", "bash", script_path,
            ])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemd-run: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "failed to start generation switch: {stderr}"
            )));
        }

        info!("Switch to generation {gen_id} started");
        Ok(())
    }

    /// Set or clear a label on a generation.
    pub async fn label_generation(&self, gen_id: u64, label: Option<String>) -> Result<(), UpdateError> {
        let mut labels = load_generation_labels().await;
        match label {
            Some(l) if !l.is_empty() => { labels.insert(gen_id, l); }
            _ => { labels.remove(&gen_id); }
        }
        save_generation_labels(&labels).await
    }

    /// Delete old generations (garbage collect).
    pub async fn delete_generation(&self, gen_id: u64) -> Result<(), UpdateError> {
        // Don't allow deleting the current generation
        let profile_link = format!("/nix/var/nix/profiles/system-{gen_id}-link");
        let current_link = "/nix/var/nix/profiles/system";

        let gen_target = tokio::fs::read_link(&profile_link).await
            .map_err(|_| UpdateError::CommandFailed(format!("generation {gen_id} does not exist")))?;
        let current_target = tokio::fs::read_link(current_link).await
            .map_err(|e| UpdateError::CommandFailed(format!("cannot read current profile: {e}")))?;

        if gen_target == current_target {
            return Err(UpdateError::CommandFailed(
                "cannot delete the currently active generation".into(),
            ));
        }

        // Check if it's the booted generation
        if let Ok(booted) = tokio::fs::read_link("/run/booted-system").await {
            if gen_target == booted {
                return Err(UpdateError::CommandFailed(
                    "cannot delete the booted generation".into(),
                ));
            }
        }

        // Remove the profile link
        tokio::fs::remove_file(&profile_link).await
            .map_err(|e| UpdateError::CommandFailed(format!("failed to remove generation {gen_id}: {e}")))?;

        // Clean up the label if any
        let mut labels = load_generation_labels().await;
        if labels.remove(&gen_id).is_some() {
            let _ = save_generation_labels(&labels).await;
        }

        info!("Deleted generation {gen_id}");
        Ok(())
    }

    pub async fn bcachefs_info(&self, system: &crate::SystemService) -> BcachefsToolsInfo {
        {
            let guard = self.cached_info.read().await;
            if let Some(ref cached) = *guard {
                return cached.clone();
            }
        }
        let info = self.bcachefs_info_uncached(system).await;
        *self.cached_info.write().await = Some(info.clone());
        info
    }

    async fn bcachefs_info_uncached(&self, system: &crate::SystemService) -> BcachefsToolsInfo {
        // Run subprocess calls and file reads concurrently.
        let ((_, kernel_rust), running_version, (lock_ref, pinned_rev), default_ref, debug_checks, (debug_symbols, debug_checks_running)) = tokio::join!(
            bcachefs_version(),
            bcachefs_loaded_module_version(),
            read_flake_lock_bcachefs(),
            read_flake_nix_default_ref(),
            // debug_checks from state file: reflects what will be built next (controls toggle)
            read_debug_checks_enabled(),
            // debug_symbols + debug_checks from cached module inspection (avoids
            // expensive xz decompression on every page load)
            system.cached_debug_flags(),
        );
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
        // Compare loaded module version against default (strip 'v' prefix for comparison)
        let default_bare = default_ref.strip_prefix('v').unwrap_or(&default_ref);
        let is_custom_running = running_version != default_bare && running_version != "unknown";
        BcachefsToolsInfo { pinned_ref, pinned_rev, running_version, is_custom, is_custom_running, default_ref, kernel_rust, debug_symbols, debug_checks, debug_checks_running }
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

        // Toggle debug checks in flake.nix by replacing the marker line.
        // When enabled: marker becomes an echo that appends the flag to the DKMS Makefile.
        // When disabled: marker is restored to a plain comment.
        let debug_checks_sed = if req.debug_checks {
            r#"sed -i 's|.*@NASTY_DEBUG_CHECKS_LINE@.*|                echo "\tccflags-y += -DCONFIG_BCACHEFS_DEBUG" >> src/fs/bcachefs/Makefile  # @NASTY_DEBUG_CHECKS_LINE@|' flake.nix"#
        } else {
            r#"sed -i 's|.*@NASTY_DEBUG_CHECKS_LINE@.*|                # @NASTY_DEBUG_CHECKS_LINE@|' flake.nix"#
        };
        let debug_checks_state = if req.debug_checks {
            format!(r#"echo "1" > {BCACHEFS_DEBUG_CHECKS_STATE}"#)
        } else {
            format!(r#"rm -f {BCACHEFS_DEBUG_CHECKS_STATE}"#)
        };

        let local_flake = local_flake().await;
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
echo "==> Switching bcachefs to {git_ref}..."
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
# Update debug checks flag in flake.nix and persist state
{debug_checks_sed}
{debug_checks_state}
# Commit the updated flake.lock and flake.nix so the tree stays clean for the next rebuild
git add flake.lock flake.nix
git -c user.email="nasty@localhost" -c user.name="NASty" \
  commit -m "bcachefs-tools: switch to {git_ref} (${{RESOLVED_SHA:-unknown}})" || true
echo "==> Rebuilding system..."
NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}
echo "success" > {BCACHEFS_SWITCH_RESULT}
printf '%s  success  %s\n' "$(date -u '+%Y-%m-%d %H:%M:%S')" "{git_ref}" >> "$SWITCH_LOG"
echo "==> bcachefs switch complete!"
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
                "--description", "NASty bcachefs version switch",
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
        info!("bcachefs switch to {git_ref} started");
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

/// Remove any `fileSystems."/fs/..."` blocks from hardware-configuration.nix.
///
/// Filesystem mounts are managed at runtime by the engine. If a user ran
/// `nixos-generate-config` while pools were mounted, those entries end up in
/// hardware-configuration.nix and will block boot after the filesystem is destroyed
/// (systemd waits forever for a device UUID that no longer exists).
async fn sanitize_hardware_config() {
    let path = format!("{LOCAL_REPO}/nixos/hardware-configuration.nix");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return,
    };
    let sanitized = strip_pool_mounts(&content);
    if sanitized != content {
        info!("Removed filesystem mount entries from hardware-configuration.nix to prevent boot failure");
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
/// Check latest release tag via GitHub API (for stable channel).
async fn check_latest_release() -> Result<String, UpdateError> {
    let token = read_github_token().await;
    let mut req = reqwest::Client::new()
        .get("https://api.github.com/repos/nasty-project/nasty/releases/latest")
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", "nasty-engine");

    if let Some(ref t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    let body: serde_json::Value = req
        .send()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("GitHub API: {e}")))?
        .json()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("parse: {e}")))?;

    body["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| UpdateError::CommandFailed("no tag_name in release response".into()))
}

/// Check latest commit on a branch via GitHub API.
async fn check_via_github_api_branch(branch: &str) -> Result<String, UpdateError> {
    let token = tokio::fs::read_to_string(GITHUB_TOKEN_PATH)
        .await
        .map(|s| s.trim().to_string())
        .map_err(|_| UpdateError::CommandFailed("no github token configured".into()))?;

    if token.is_empty() {
        return Err(UpdateError::CommandFailed("empty github token".into()));
    }

    let url = format!("https://api.github.com/repos/nasty-project/nasty/commits/{branch}");
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
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
async fn check_via_git_ls_remote(token: Option<&str>, git_ref: &str) -> Result<String, UpdateError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args(["-c", "credential.helper="]);
    if let Some(t) = token {
        cmd.arg("-c").arg(format!(
            "url.https://x-access-token:{t}@github.com/.insteadOf=https://github.com/"
        ));
    }
    cmd.args(["ls-remote", REPO_URL, git_ref]);

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

/// Public wrapper for use by lib.rs cached info.
/// Read the system config name from the state file, defaulting to "nasty" (bare metal).
async fn read_system_config() -> String {
    tokio::fs::read_to_string(SYSTEM_CONFIG_PATH).await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string())
}

/// Build the full flake reference for nixos-rebuild.
async fn local_flake() -> String {
    let config = read_system_config().await;
    format!("{LOCAL_FLAKE_DIR}#{config}")
}

pub async fn read_flake_nix_default_ref_pub() -> String {
    read_flake_nix_default_ref().await
}

/// Public wrapper for use by lib.rs cached info.
pub async fn is_reboot_required_pub() -> bool {
    is_reboot_required().await
}

// ── Generation labels persistence ──────────────────────────────

async fn load_generation_labels() -> std::collections::HashMap<u64, String> {
    tokio::fs::read_to_string(GENERATION_LABELS_PATH)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

async fn save_generation_labels(
    labels: &std::collections::HashMap<u64, String>,
) -> Result<(), UpdateError> {
    let json = serde_json::to_string_pretty(labels)
        .map_err(|e| UpdateError::CommandFailed(format!("serialize labels: {e}")))?;
    tokio::fs::write(GENERATION_LABELS_PATH, json)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("write labels: {e}")))?;
    Ok(())
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

/// Read debug checks *configured* state (what the next DKMS build will use).
/// State file is the sole source of truth — survives git reset --hard.
/// Note: the *running* module state is detected separately via modinfo in lib.rs.
async fn read_debug_checks_enabled() -> bool {
    tokio::fs::metadata(BCACHEFS_DEBUG_CHECKS_STATE).await.is_ok()
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
