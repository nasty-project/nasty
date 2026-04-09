use base64::Engine as _;
use rnix::ast::{self};
use rowan::ast::AstNode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use thiserror::Error;
use tracing::info;

/// Primary version path — writable by the update script, not managed by NixOS.
const VERSION_PATH: &str = "/var/lib/nasty/version";
/// Fallback version path — baked in by NixOS at build time (may be a local SHA).
const VERSION_PATH_FALLBACK: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const LOCAL_FLAKE_TARGET: &str = "/etc/nixos#nasty";
const LOCAL_REPO: &str = "/etc/nixos";
const NIXOS_FLAKE_DIR: &str = "/etc/nixos";
const UPDATE_WEBUI_CHANGED: &str = "/var/lib/nasty/update-webui-changed";
const RELEASE_CHANNEL_PATH: &str = "/var/lib/nasty/release-channel";
const GC_CONFIG_PATH: &str = "/var/lib/nasty/gc-config.json";
const VERSION_SWITCH_BACKUP_DIR: &str = "/var/lib/nasty/etc-nixos-backup";
const DEFAULT_NASTY_OWNER: &str = "nasty-project";
const DEFAULT_NASTY_REPO: &str = "nasty";
const DEFAULT_NASTY_REF: &str = "main";
const VERSION_INPUT_NAMES: [&str; 3] = ["nixpkgs", "bcachefs-tools", "nasty"];
const SYSTEM_FLAKE_TEMPLATE_PATH: &str = "nixos/system-flake/flake.nix.template";

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

fn default_keep_generations() -> u32 {
    20
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            keep_generations: 20,
            max_age_days: 0,
        }
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
        tokio::fs::write(GC_CONFIG_PATH, json)
            .await
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
            Self::Mild => "main",  // uses v* tags on main
            Self::Spicy => "main", // uses s* tags on main
            Self::Nasty => "main", // HEAD of main
        }
    }

    /// Tag glob pattern for tag-based channels.
    pub fn tag_pattern(&self) -> Option<&'static str> {
        match self {
            Self::Mild => Some("v*"),
            Self::Spicy => Some("s*"),
            Self::Nasty => None, // no tags, always HEAD
        }
    }

    /// GitHub API endpoint for checking latest commit.
    pub fn github_api_url(&self) -> String {
        match self {
            Self::Mild => {
                "https://api.github.com/repos/nasty-project/nasty/releases/latest".to_string()
            }
            _ => format!(
                "https://api.github.com/repos/nasty-project/nasty/commits/{}",
                self.git_ref()
            ),
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
pub struct VersionInputInfo {
    /// Flake input name (e.g. `nixpkgs`).
    pub name: String,
    /// Exact `input.url` string from `/etc/nixos/flake.nix`.
    pub url: String,
    /// Locked commit SHA from `/etc/nixos/flake.lock` (shortened to 12 chars).
    pub rev: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionInfo {
    /// Inputs shown on the Version page in fixed display order.
    pub inputs: Vec<VersionInputInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionTaggedReleaseStatus {
    /// Exact current `nasty.url` string from `/etc/nixos/flake.nix`.
    pub current_url: String,
    /// Latest official NASty release tag available upstream.
    pub latest_tag: String,
    /// Standard shorthand URL for the latest official tagged release.
    pub latest_url: String,
    /// True when `nasty.url` already matches the newest official tagged release.
    pub current_is_latest_standard_url: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BootstrapSystemFlakeResult {
    /// Path of the written flake.nix.
    pub flake_path: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct VersionSwitchInput {
    /// Flake input name.
    pub name: String,
    /// Replacement URL to write to `/etc/nixos/flake.nix`.
    pub url: String,
    /// Whether this input should be refreshed in `flake.lock`.
    #[serde(default)]
    pub update: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VersionSwitchRequest {
    /// Requested URLs and update flags for the Version page.
    pub inputs: Vec<VersionSwitchInput>,
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

#[derive(Debug, Clone)]
struct NastyInputSource {
    owner: String,
    repo: String,
    tracked_ref: String,
}

#[derive(Debug, Clone)]
struct ParsedFlakeInput {
    url: String,
    value_start: usize,
    value_end: usize,
}

impl NastyInputSource {
    fn repo_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.repo)
    }

    fn github_input(&self, git_ref: &str) -> String {
        format!("github:{}/{}/{}", self.owner, self.repo, git_ref)
    }
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
            channel: read_channel().await,
        }
    }

    /// Read the exact upstream input URLs and locked revs from the live
    /// `/etc/nixos` flake on the installed system.
    pub async fn version_info(&self) -> Result<VersionInfo, UpdateError> {
        let urls = read_flake_input_urls().await?;
        let revs = read_flake_lock_revs().await;

        let mut inputs = Vec::with_capacity(VERSION_INPUT_NAMES.len());
        for name in VERSION_INPUT_NAMES {
            let url = urls.get(name).cloned().ok_or_else(|| {
                UpdateError::CommandFailed(format!(
                    "missing {name}.url in {NIXOS_FLAKE_DIR}/flake.nix"
                ))
            })?;
            inputs.push(VersionInputInfo {
                name: name.to_string(),
                url,
                rev: revs.get(name).cloned(),
            });
        }

        Ok(VersionInfo { inputs })
    }

    /// Return the latest official tagged release and whether the current
    /// `nasty.url` already matches its standard GitHub shorthand form.
    pub async fn version_tagged_release_status(
        &self,
    ) -> Result<VersionTaggedReleaseStatus, UpdateError> {
        let urls = read_flake_input_urls().await?;
        let current_url = urls.get("nasty").cloned().ok_or_else(|| {
            UpdateError::CommandFailed(format!("missing nasty.url in {NIXOS_FLAKE_DIR}/flake.nix"))
        })?;
        let latest_tag = latest_official_nasty_release_tag().await?;
        let latest_url = official_nasty_release_url(&latest_tag);

        Ok(VersionTaggedReleaseStatus {
            current_is_latest_standard_url: current_url.trim() == latest_url,
            current_url,
            latest_tag,
            latest_url,
        })
    }

    /// Bootstrap `/etc/nixos/flake.nix` from the latest official tagged
    /// release's wrapper-flake template, then run a switch rebuild.
    pub async fn upgrade_tagged_release(&self) -> Result<(), UpdateError> {
        let update_status = self.status().await;
        if update_status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        self.purge_stale_version_backup().await?;

        let release_status = self.version_tagged_release_status().await?;
        if release_status.current_is_latest_standard_url {
            return Err(UpdateError::CommandFailed(
                "system already tracks the newest official tagged NASty release".to_string(),
            ));
        }

        let local_system = detect_local_system().await?;
        let token = read_github_token().await;
        let template = fetch_github_text_file(
            token.as_deref(),
            DEFAULT_NASTY_OWNER,
            DEFAULT_NASTY_REPO,
            SYSTEM_FLAKE_TEMPLATE_PATH,
            &release_status.latest_tag,
        )
        .await?;
        let bootstrapped_flake =
            render_system_flake_template(&template, &release_status.latest_url, &local_system)?;

        let _ = tokio::process::Command::new("systemctl")
            .args(["reset-failed", UPDATE_UNIT])
            .output()
            .await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["stop", UPDATE_UNIT])
            .output()
            .await;

        let flake_temp_path = "/tmp/nasty-upgrade-flake.nix";
        tokio::fs::write(flake_temp_path, &bootstrapped_flake)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("write {flake_temp_path}: {e}")))?;

        let local_flake = local_flake();
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
_nginx_conf() {{
    grep -o "/nix/store/[^' ]*nginx\.conf" \
        /run/current-system/etc/systemd/system/nginx.service 2>/dev/null | head -1 || true
}}
_NGINX_CONF_BEFORE=$(_nginx_conf)
WEBUI_BEFORE=$([ -n "$_NGINX_CONF_BEFORE" ] && grep 'nasty-webui' "$_NGINX_CONF_BEFORE" 2>/dev/null | head -1 || echo "")
echo "false" > {UPDATE_WEBUI_CHANGED}

echo "==> Updating local system flake..."
cd {NIXOS_FLAKE_DIR}
cp {flake_temp_path} flake.nix
nix flake update nixpkgs
nix flake update bcachefs-tools
nix flake update nasty

echo "==> Rebuilding system..."
NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}

_NGINX_CONF_AFTER=$(_nginx_conf)
WEBUI_AFTER=$([ -n "$_NGINX_CONF_AFTER" ] && grep 'nasty-webui' "$_NGINX_CONF_AFTER" 2>/dev/null | head -1 || echo "")
if [ -n "$WEBUI_BEFORE" ] && [ "$WEBUI_BEFORE" != "$WEBUI_AFTER" ]; then
    echo "true" > {UPDATE_WEBUI_CHANGED}
fi

echo "{latest_tag}" > {VERSION_PATH}
echo "==> Update complete!"
"#,
            latest_tag = release_status.latest_tag,
        );

        let script_path = "/tmp/nasty-upgrade-tagged-release.sh";
        tokio::fs::write(script_path, &script).await.map_err(|e| {
            UpdateError::CommandFailed(format!(
                "failed to write tagged release upgrade script: {e}"
            ))
        })?;

        let path = std::env::var("PATH").unwrap_or_default();
        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit",
                UPDATE_UNIT,
                "--no-block",
                "--description",
                "NASty tagged release upgrade",
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--setenv",
                &format!("PATH={path}"),
                "--",
                "bash",
                script_path,
            ])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemd-run: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "failed to start tagged release upgrade: {stderr}"
            )));
        }

        info!(
            "Tagged release upgrade started: {} -> {}",
            release_status.current_url.trim(),
            release_status.latest_tag
        );
        Ok(())
    }

    /// Legacy endpoint kept for compatibility with older web UIs.
    /// Newer builds do not restore from a backup; they only purge any stale
    /// backup directory left behind by an older implementation.
    pub async fn version_cleanup(&self) -> Result<(), UpdateError> {
        self.purge_stale_version_backup().await
    }

    /// Get or set the release channel.
    pub async fn get_channel(&self) -> ReleaseChannel {
        read_channel().await
    }

    pub async fn set_channel(
        &self,
        channel: ReleaseChannel,
    ) -> Result<ReleaseChannel, UpdateError> {
        write_channel(channel)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("write channel: {e}")))?;
        info!("Release channel set to {}", channel.display_name());
        Ok(channel)
    }

    /// Check if an update is available by comparing local rev to GitHub
    pub async fn check(&self) -> Result<UpdateInfo, UpdateError> {
        let current = read_current_version().await;
        let channel = read_channel().await;
        let nasty_input = read_nasty_input_source().await;

        // Mild/Spicy: find latest matching tag (v* or s*) on the configured repo.
        // Nasty: track the wrapper flake's configured branch/ref.
        let latest = match channel {
            ReleaseChannel::Mild | ReleaseChannel::Spicy => {
                let pattern = channel.tag_pattern().unwrap(); // "v*" or "s*"
                let token = read_github_token().await;
                match check_latest_tag(
                    token.as_deref(),
                    &nasty_input.owner,
                    &nasty_input.repo,
                    pattern,
                )
                .await
                {
                    Ok(tag) => tag,
                    Err(_) => "unknown".to_string(),
                }
            }
            ReleaseChannel::Nasty => {
                match check_via_github_api_branch(
                    &nasty_input.owner,
                    &nasty_input.repo,
                    &nasty_input.tracked_ref,
                )
                .await
                {
                    Ok(sha) => sha,
                    Err(_) => {
                        let token = read_github_token().await;
                        check_via_git_ls_remote(
                            token.as_deref(),
                            &nasty_input.repo_url(),
                            &format!("refs/heads/{}", nasty_input.tracked_ref),
                        )
                        .await?
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
        // 1. Update the local wrapper flake input for nasty
        // 2. Rebuild from local flake (which keeps hardware-configuration.nix)
        let channel = read_channel().await;
        let token = read_github_token().await;
        let nasty_input = read_nasty_input_source().await;

        // TODO: Remove token env var once the repo access model is finalized.
        let token_env = token
            .as_ref()
            .map(|t| format!("access-tokens = github.com={t}"))
            .unwrap_or_default();

        let (update_step, installed_version_expr) = match channel {
            ReleaseChannel::Mild | ReleaseChannel::Spicy => {
                let pattern = channel.tag_pattern().unwrap();
                let latest_tag = check_latest_tag(
                    token.as_deref(),
                    &nasty_input.owner,
                    &nasty_input.repo,
                    pattern,
                )
                .await?;
                (
                    format!(
                        "echo \"==> Pinning NASty to release {latest_tag}...\"\n\
                         nix flake lock --override-input nasty \"{}\"",
                        nasty_input.github_input(&latest_tag)
                    ),
                    format!("echo \"{latest_tag}\" > {VERSION_PATH}"),
                )
            }
            ReleaseChannel::Nasty => (
                format!(
                    "echo \"==> Updating NASty input ({})...\"\n\
                     nix flake update nasty",
                    nasty_input.tracked_ref
                ),
                format!(
                    "NASTY_REV=$(jq -r '.nodes[\"nasty\"].locked.rev // empty' flake.lock 2>/dev/null || true)\n\
                     [ -n \"$NASTY_REV\" ] && echo \"${{NASTY_REV:0:7}}\" > {VERSION_PATH}"
                ),
            ),
        };

        let local_flake = local_flake();
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
echo "==> Updating local system flake..."
cd {LOCAL_REPO}
{update_step}

# Generation cleanup is handled by nix.gc (systemd timer) in the NixOS config.
# No custom GC logic needed here — just rebuild.

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

# Write the active nasty input version to the writable version path.
{installed_version_expr}

echo "==> Update complete!"
"#
        );

        // Write script to a temp file
        let script_path = "/tmp/nasty-update.sh";
        tokio::fs::write(script_path, &script).await.map_err(|e| {
            UpdateError::CommandFailed(format!("failed to write update script: {e}"))
        })?;

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

        cmd.args(["--", "bash", script_path]);

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

    /// Update selected flake inputs on the installed system and rebuild if the
    /// lock file changed.
    pub async fn version_switch(&self, req: VersionSwitchRequest) -> Result<(), UpdateError> {
        let update_status = self.status().await;
        if update_status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        self.purge_stale_version_backup().await?;

        let current_urls = read_flake_input_urls().await?;
        let mut seen = HashSet::new();
        let mut requested = HashMap::new();
        for input in req.inputs {
            if !VERSION_INPUT_NAMES.contains(&input.name.as_str()) {
                return Err(UpdateError::CommandFailed(format!(
                    "unknown input: {}",
                    input.name
                )));
            }
            if !seen.insert(input.name.clone()) {
                return Err(UpdateError::CommandFailed(format!(
                    "duplicate input: {}",
                    input.name
                )));
            }
            let url = input.url.trim().to_string();
            if url.is_empty() {
                return Err(UpdateError::CommandFailed(format!(
                    "{} url must not be empty",
                    input.name
                )));
            }
            requested.insert(input.name.clone(), VersionSwitchInput { url, ..input });
        }

        let mut updates = Vec::new();
        let mut url_changes = Vec::new();
        for name in VERSION_INPUT_NAMES {
            let current_url = current_urls.get(name).ok_or_else(|| {
                UpdateError::CommandFailed(format!(
                    "missing {name}.url in {NIXOS_FLAKE_DIR}/flake.nix"
                ))
            })?;
            let input = requested.get(name).ok_or_else(|| {
                UpdateError::CommandFailed(format!("missing request entry for {name}"))
            })?;
            let url_changed = input.url != *current_url;
            if input.update || url_changed {
                updates.push(name.to_string());
            }
            if url_changed {
                url_changes.push((name.to_string(), input.url.clone()));
            }
        }

        if updates.is_empty() {
            return Err(UpdateError::CommandFailed(
                "nothing to switch: enable at least one update or change an input URL".into(),
            ));
        }

        let _ = tokio::process::Command::new("systemctl")
            .args(["reset-failed", UPDATE_UNIT])
            .output()
            .await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["stop", UPDATE_UNIT])
            .output()
            .await;

        let flake_path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
        let current_flake = tokio::fs::read_to_string(&flake_path)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("read {flake_path}: {e}")))?;
        let flake_replacements = url_changes
            .iter()
            .map(|(name, url)| (name.clone(), url.clone()))
            .collect::<HashMap<_, _>>();
        let rewritten_flake = rewrite_flake_input_urls(&current_flake, &flake_replacements)?;
        let flake_temp_path = "/tmp/nasty-version-flake.nix";
        tokio::fs::write(flake_temp_path, &rewritten_flake)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("write {flake_temp_path}: {e}")))?;

        let update_steps = updates
            .iter()
            .map(|name| format!("nix flake update {name}"))
            .collect::<Vec<_>>()
            .join("\n");

        let local_flake = local_flake();
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
_nginx_conf() {{
    grep -o "/nix/store/[^' ]*nginx\.conf" \
        /run/current-system/etc/systemd/system/nginx.service 2>/dev/null | head -1 || true
}}
_NGINX_CONF_BEFORE=$(_nginx_conf)
WEBUI_BEFORE=$([ -n "$_NGINX_CONF_BEFORE" ] && grep 'nasty-webui' "$_NGINX_CONF_BEFORE" 2>/dev/null | head -1 || echo "")
echo "false" > {UPDATE_WEBUI_CHANGED}

echo "==> Updating local system flake..."
cd {NIXOS_FLAKE_DIR}
LOCK_BEFORE=$(sha256sum flake.lock 2>/dev/null | awk '{{print $1}}' || true)
cp {flake_temp_path} flake.nix
{update_steps}
LOCK_AFTER=$(sha256sum flake.lock 2>/dev/null | awk '{{print $1}}' || true)

if [ "$LOCK_BEFORE" != "$LOCK_AFTER" ]; then
    echo "==> Rebuilding system..."
    NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}
    _NGINX_CONF_AFTER=$(_nginx_conf)
    WEBUI_AFTER=$([ -n "$_NGINX_CONF_AFTER" ] && grep 'nasty-webui' "$_NGINX_CONF_AFTER" 2>/dev/null | head -1 || echo "")
    if [ -n "$WEBUI_BEFORE" ] && [ "$WEBUI_BEFORE" != "$WEBUI_AFTER" ]; then
        echo "true" > {UPDATE_WEBUI_CHANGED}
    fi
    NASTY_REV=$(jq -r '.nodes["nasty"].locked.rev // empty' flake.lock 2>/dev/null || true)
    [ -n "$NASTY_REV" ] && echo "${{NASTY_REV:0:7}}" > {VERSION_PATH}
else
    echo "==> No flake.lock changes detected; skipping rebuild."
fi
echo "==> Update complete!"
"#
        );

        let script_path = "/tmp/nasty-version-switch.sh";
        tokio::fs::write(script_path, &script).await.map_err(|e| {
            UpdateError::CommandFailed(format!("failed to write version switch script: {e}"))
        })?;

        let path = std::env::var("PATH").unwrap_or_default();
        let output = tokio::process::Command::new("systemd-run")
            .args([
                "--unit",
                UPDATE_UNIT,
                "--no-block",
                "--description",
                "NASty version switch",
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--setenv",
                &format!("PATH={path}"),
                "--",
                "bash",
                script_path,
            ])
            .output()
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("systemd-run: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UpdateError::CommandFailed(format!(
                "failed to start version switch: {stderr}"
            )));
        }

        info!("Version switch started for inputs: {}", updates.join(", "));
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
            .map_err(|e| {
                UpdateError::CommandFailed(format!("nixos-rebuild list-generations: {e}"))
            })?;

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
                "--unit",
                UPDATE_UNIT,
                "--no-block",
                "--description",
                &format!("NASty switch to generation {gen_id}"),
                "--property=Type=oneshot",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--setenv",
                &format!("PATH={path}"),
                "--",
                "bash",
                script_path,
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
    pub async fn label_generation(
        &self,
        gen_id: u64,
        label: Option<String>,
    ) -> Result<(), UpdateError> {
        let mut labels = load_generation_labels().await;
        match label {
            Some(l) if !l.is_empty() => {
                labels.insert(gen_id, l);
            }
            _ => {
                labels.remove(&gen_id);
            }
        }
        save_generation_labels(&labels).await
    }

    /// Delete old generations (garbage collect).
    pub async fn delete_generation(&self, gen_id: u64) -> Result<(), UpdateError> {
        // Don't allow deleting the current generation
        let profile_link = format!("/nix/var/nix/profiles/system-{gen_id}-link");
        let current_link = "/nix/var/nix/profiles/system";

        let gen_target = tokio::fs::read_link(&profile_link).await.map_err(|_| {
            UpdateError::CommandFailed(format!("generation {gen_id} does not exist"))
        })?;
        let current_target = tokio::fs::read_link(current_link)
            .await
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
        tokio::fs::remove_file(&profile_link).await.map_err(|e| {
            UpdateError::CommandFailed(format!("failed to remove generation {gen_id}: {e}"))
        })?;

        // Clean up the label if any
        let mut labels = load_generation_labels().await;
        if labels.remove(&gen_id).is_some() {
            let _ = save_generation_labels(&labels).await;
        }

        info!("Deleted generation {gen_id}");
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

        // Read hint file written by the update script.
        // Default to true when state is success and the file is missing (conservative).
        let webui_changed = if state == "success" {
            tokio::fs::read_to_string(UPDATE_WEBUI_CHANGED)
                .await
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

    async fn purge_stale_version_backup(&self) -> Result<(), UpdateError> {
        if tokio::fs::metadata(VERSION_SWITCH_BACKUP_DIR)
            .await
            .is_err()
        {
            return Ok(());
        }

        tokio::fs::remove_dir_all(VERSION_SWITCH_BACKUP_DIR)
            .await
            .map_err(|e| {
                UpdateError::CommandFailed(format!("remove stale {VERSION_SWITCH_BACKUP_DIR}: {e}"))
            })?;
        Ok(())
    }
}

/// TODO: Remove once repo access no longer requires token fallbacks.
/// Find the latest tag matching a glob pattern (e.g. "v*", "s*") via git ls-remote.
async fn check_latest_tag(
    token: Option<&str>,
    owner: &str,
    repo: &str,
    pattern: &str,
) -> Result<String, UpdateError> {
    let ref_pattern = format!("refs/tags/{pattern}");
    let mut args = vec!["ls-remote", "--tags", "--sort=-v:refname"];
    let url = match token {
        Some(t) => format!("https://x-access-token:{t}@github.com/{owner}/{repo}.git"),
        None => format!("https://github.com/{owner}/{repo}.git"),
    };
    args.push(&url);
    args.push(&ref_pattern);

    let output = tokio::process::Command::new("git")
        .args(&args)
        .output()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("git ls-remote: {e}")))?;

    if !output.status.success() {
        return Err(UpdateError::CommandFailed("git ls-remote failed".into()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // First line is the latest tag (sorted by version descending)
    // Format: "sha\trefs/tags/v0.0.1"
    for line in stdout.lines() {
        if let Some(tag_ref) = line.split('\t').nth(1) {
            let tag = normalize_git_tag_ref(tag_ref);
            if !tag.is_empty() {
                return Ok(tag.to_string());
            }
        }
    }

    Err(UpdateError::CommandFailed(format!(
        "no tags matching '{pattern}' found"
    )))
}

async fn latest_official_nasty_release_tag() -> Result<String, UpdateError> {
    let token = read_github_token().await;
    let latest_tag = tokio::time::timeout(
        Duration::from_secs(60),
        check_latest_tag(
            token.as_deref(),
            DEFAULT_NASTY_OWNER,
            DEFAULT_NASTY_REPO,
            "v*",
        ),
    )
    .await
    .map_err(|_| UpdateError::CommandFailed("timed out fetching latest tagged release".into()))??;

    if parse_release_tag_version(&latest_tag).is_none() {
        return Err(UpdateError::CommandFailed(format!(
            "latest official tagged release is not a semantic vX.Y.Z tag: {latest_tag}"
        )));
    }

    Ok(latest_tag)
}

pub async fn bootstrap_system_flake_from_template_path(
    template_path: &str,
    dest_dir: &str,
    nasty_version: &str,
    local_system: &str,
) -> Result<BootstrapSystemFlakeResult, UpdateError> {
    let template = tokio::fs::read_to_string(template_path)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("read {template_path}: {e}")))?;
    bootstrap_system_flake_from_template(&template, dest_dir, nasty_version, local_system).await
}

pub async fn bootstrap_system_flake_from_template(
    template: &str,
    dest_dir: &str,
    nasty_version: &str,
    local_system: &str,
) -> Result<BootstrapSystemFlakeResult, UpdateError> {
    let rendered = render_system_flake_template(template, nasty_version, local_system)?;
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("mkdir {dest_dir}: {e}")))?;
    let flake_path = format!("{dest_dir}/flake.nix");
    tokio::fs::write(&flake_path, rendered)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("write {flake_path}: {e}")))?;
    Ok(BootstrapSystemFlakeResult { flake_path })
}

fn render_system_flake_template(
    template: &str,
    nasty_version: &str,
    local_system: &str,
) -> Result<String, UpdateError> {
    if !template.contains("@NASTY_VERSION@") {
        return Err(UpdateError::CommandFailed(
            "system flake template is missing @NASTY_VERSION@ placeholder".into(),
        ));
    }
    if !template.contains("@LOCAL_SYSTEM@") {
        return Err(UpdateError::CommandFailed(
            "system flake template is missing @LOCAL_SYSTEM@ placeholder".into(),
        ));
    }
    let nasty_tag = normalize_release_tag(nasty_version)?;

    Ok(template
        .replace("@NASTY_VERSION@", &nasty_tag)
        .replace("@LOCAL_SYSTEM@", local_system))
}

fn normalize_release_tag(version_or_tag: &str) -> Result<String, UpdateError> {
    let trimmed = version_or_tag.trim();
    let tag = if trimmed.starts_with('v') {
        trimmed.to_string()
    } else {
        format!("v{trimmed}")
    };

    if parse_release_tag_version(&tag).is_none() {
        return Err(UpdateError::CommandFailed(format!(
            "invalid tagged release version: {version_or_tag}"
        )));
    }

    Ok(tag)
}

async fn detect_local_system() -> Result<String, UpdateError> {
    let output = tokio::process::Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "eval",
            "--impure",
            "--raw",
            "--expr",
            "builtins.currentSystem",
        ])
        .output()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("detect local system: {e}")))?;

    if !output.status.success() {
        return Err(UpdateError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let system = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if system.is_empty() {
        return Err(UpdateError::CommandFailed(
            "failed to detect local system identifier".into(),
        ));
    }
    Ok(system)
}

async fn fetch_github_text_file(
    token: Option<&str>,
    owner: &str,
    repo: &str,
    path: &str,
    git_ref: &str,
) -> Result<String, UpdateError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/contents/{path}?ref={git_ref}");
    let mut req = reqwest::Client::new()
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", "nasty-engine");
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let body: serde_json::Value = req
        .send()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("GitHub API request failed: {e}")))?
        .json()
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("failed to parse GitHub response: {e}")))?;

    let encoding = body["encoding"].as_str().unwrap_or_default();
    let content = body["content"].as_str().ok_or_else(|| {
        UpdateError::CommandFailed("missing file content in GitHub response".into())
    })?;
    if encoding != "base64" {
        return Err(UpdateError::CommandFailed(format!(
            "unsupported GitHub content encoding: {encoding}"
        )));
    }
    let normalized = content.replace('\n', "");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(normalized)
        .map_err(|e| UpdateError::CommandFailed(format!("failed to decode GitHub file: {e}")))?;
    String::from_utf8(decoded)
        .map_err(|e| UpdateError::CommandFailed(format!("GitHub file is not valid UTF-8: {e}")))
}

/// Check latest commit on a branch via GitHub API.
async fn check_via_github_api_branch(
    owner: &str,
    repo: &str,
    branch: &str,
) -> Result<String, UpdateError> {
    let token = tokio::fs::read_to_string(GITHUB_TOKEN_PATH)
        .await
        .map(|s| s.trim().to_string())
        .map_err(|_| UpdateError::CommandFailed("no github token configured".into()))?;

    if token.is_empty() {
        return Err(UpdateError::CommandFailed("empty github token".into()));
    }

    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{branch}");
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
async fn check_via_git_ls_remote(
    token: Option<&str>,
    repo_url: &str,
    git_ref: &str,
) -> Result<String, UpdateError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args(["-c", "credential.helper="]);
    if let Some(t) = token {
        cmd.arg("-c").arg(format!(
            "url.https://x-access-token:{t}@github.com/.insteadOf=https://github.com/"
        ));
    }
    cmd.args(["ls-remote", repo_url, git_ref]);

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
    // selected tag or branch commit). Fall back to the nasty input locked in the
    // local flake, then to the NixOS-baked /etc/nasty-version as a last resort.
    if let Ok(s) = tokio::fs::read_to_string(VERSION_PATH).await {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    if let Some(version) = read_locked_nasty_version().await {
        return version;
    }
    for path in &[VERSION_PATH_FALLBACK] {
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
        (
            "/run/booted-system/kernel-modules",
            "/run/current-system/kernel-modules",
        ),
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

/// Build the full flake reference for nixos-rebuild.
fn local_flake() -> &'static str {
    LOCAL_FLAKE_TARGET
}

pub async fn read_flake_nix_default_ref_pub() -> String {
    read_flake_nix_default_ref().await
}

pub async fn read_flake_lock_bcachefs_pub() -> (Option<String>, Option<String>) {
    read_flake_lock_bcachefs().await
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

fn parse_flake_input_urls(content: &str) -> Result<HashMap<String, ParsedFlakeInput>, UpdateError> {
    let parsed = rnix::Root::parse(content);
    if !parsed.errors().is_empty() {
        let first = parsed.errors()[0].to_string();
        return Err(UpdateError::CommandFailed(format!(
            "failed to parse {NIXOS_FLAKE_DIR}/flake.nix: {first}"
        )));
    }

    let mut urls = HashMap::new();
    let root = parsed.tree();
    for node in root
        .syntax()
        .descendants()
        .filter_map(ast::AttrpathValue::cast)
    {
        let Some(attrpath) = node.attrpath() else {
            continue;
        };
        let normalized_path = attrpath
            .syntax()
            .text()
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        let Some(name) = VERSION_INPUT_NAMES
            .iter()
            .find(|candidate| normalized_path == format!("{candidate}.url"))
        else {
            continue;
        };
        let Some(value) = node.value() else { continue };
        let raw_value = value.syntax().text().to_string();
        let Some(url) = unquote_nix_string(&raw_value) else {
            continue;
        };
        let range = value.syntax().text_range();
        urls.insert(
            (*name).to_string(),
            ParsedFlakeInput {
                url,
                value_start: u32::from(range.start()) as usize,
                value_end: u32::from(range.end()) as usize,
            },
        );
    }

    for name in VERSION_INPUT_NAMES {
        if !urls.contains_key(name) {
            return Err(UpdateError::CommandFailed(format!(
                "missing {name}.url in {NIXOS_FLAKE_DIR}/flake.nix"
            )));
        }
    }

    Ok(urls)
}

fn rewrite_flake_input_urls(
    content: &str,
    replacements: &HashMap<String, String>,
) -> Result<String, UpdateError> {
    if replacements.is_empty() {
        return Ok(content.to_string());
    }

    let parsed = parse_flake_input_urls(content)?;
    let mut edits = Vec::new();
    for (name, replacement) in replacements {
        let current = parsed.get(name).ok_or_else(|| {
            UpdateError::CommandFailed(format!("missing {name}.url in {NIXOS_FLAKE_DIR}/flake.nix"))
        })?;
        edits.push((
            current.value_start,
            current.value_end,
            serde_json::to_string(replacement).map_err(|e| {
                UpdateError::CommandFailed(format!("serialize replacement URL for {name}: {e}"))
            })?,
        ));
    }

    edits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut rewritten = content.to_string();
    for (start, end, replacement) in edits {
        rewritten.replace_range(start..end, &replacement);
    }
    Ok(rewritten)
}

fn unquote_nix_string(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw).ok()
}

async fn read_flake_input_urls() -> Result<HashMap<String, String>, UpdateError> {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("read {path}: {e}")))?;
    Ok(parse_flake_input_urls(&content)?
        .into_iter()
        .map(|(name, parsed)| (name, parsed.url))
        .collect())
}

async fn read_flake_lock_revs() -> HashMap<String, String> {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.lock");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut revs = HashMap::new();
    for name in VERSION_INPUT_NAMES {
        if let Some(rev) = v["nodes"][name]["locked"]["rev"].as_str() {
            revs.insert(name.to_string(), rev[..rev.len().min(12)].to_string());
        }
    }
    revs
}

/// Parse flake.nix to extract the default bcachefs-tools ref from the input URL.
async fn read_flake_nix_default_ref() -> String {
    read_flake_input_urls()
        .await
        .ok()
        .and_then(|urls| urls.get("bcachefs-tools").cloned())
        .and_then(|url| url.rsplit('/').next().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

async fn read_nasty_input_source() -> NastyInputSource {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.lock");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => {
            return NastyInputSource {
                owner: DEFAULT_NASTY_OWNER.to_string(),
                repo: DEFAULT_NASTY_REPO.to_string(),
                tracked_ref: DEFAULT_NASTY_REF.to_string(),
            };
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            return NastyInputSource {
                owner: DEFAULT_NASTY_OWNER.to_string(),
                repo: DEFAULT_NASTY_REPO.to_string(),
                tracked_ref: DEFAULT_NASTY_REF.to_string(),
            };
        }
    };
    let node = &v["nodes"]["nasty"];
    let owner = node["original"]["owner"]
        .as_str()
        .or_else(|| node["locked"]["owner"].as_str())
        .unwrap_or(DEFAULT_NASTY_OWNER)
        .to_string();
    let repo = node["original"]["repo"]
        .as_str()
        .or_else(|| node["locked"]["repo"].as_str())
        .unwrap_or(DEFAULT_NASTY_REPO)
        .to_string();
    let tracked_ref = node["original"]["ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_NASTY_REF)
        .to_string();
    NastyInputSource {
        owner,
        repo,
        tracked_ref,
    }
}

fn normalize_git_tag_ref(tag_ref: &str) -> &str {
    tag_ref
        .strip_prefix("refs/tags/")
        .unwrap_or(tag_ref)
        .strip_suffix("^{}")
        .unwrap_or_else(|| tag_ref.strip_prefix("refs/tags/").unwrap_or(tag_ref))
}

fn official_nasty_release_url(tag: &str) -> String {
    format!("github:{DEFAULT_NASTY_OWNER}/{DEFAULT_NASTY_REPO}/{tag}")
}

#[cfg(test)]
fn parse_official_nasty_release_tag(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let rest = trimmed.strip_prefix("github:")?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let git_ref = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if owner != DEFAULT_NASTY_OWNER || repo != DEFAULT_NASTY_REPO {
        return None;
    }
    parse_release_tag_version(git_ref).map(|_| git_ref.to_string())
}

fn parse_release_tag_version(tag: &str) -> Option<(u64, u64, u64)> {
    let raw = tag.strip_prefix('v')?;
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

async fn read_locked_nasty_version() -> Option<String> {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.lock");
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let node = &v["nodes"]["nasty"];
    let rev = node["locked"]["rev"].as_str()?;
    Some(rev[..rev.len().min(7)].to_string())
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
    let pinned_rev = node["locked"]["rev"]
        .as_str()
        .map(|s| s[..s.len().min(12)].to_string()); // short rev, 12 chars
    (pinned_ref, pinned_rev)
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_git_tag_ref, parse_official_nasty_release_tag, parse_release_tag_version,
    };

    #[test]
    fn normalizes_annotated_git_tag_refs() {
        assert_eq!(normalize_git_tag_ref("refs/tags/v0.0.3^{}"), "v0.0.3");
        assert_eq!(normalize_git_tag_ref("refs/tags/v0.0.3"), "v0.0.3");
    }

    #[test]
    fn parses_only_official_release_tags() {
        assert_eq!(
            parse_official_nasty_release_tag("github:nasty-project/nasty/v0.0.2"),
            Some("v0.0.2".to_string())
        );
        assert_eq!(
            parse_official_nasty_release_tag("github:nasty-project/nasty/main"),
            None
        );
        assert_eq!(
            parse_official_nasty_release_tag("github:someone-else/nasty/v0.0.2"),
            None
        );
        assert_eq!(
            parse_official_nasty_release_tag("github:nasty-project/nasty/v0.0.2-rc1"),
            None
        );
    }

    #[test]
    fn parses_semver_release_tags() {
        assert_eq!(parse_release_tag_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_release_tag_version("v1.2"), None);
        assert_eq!(parse_release_tag_version("main"), None);
    }

    #[test]
    fn renders_system_flake_template() {
        let template = r#"
inputs = { nasty.url = "github:nasty-project/nasty/@NASTY_VERSION@"; };
"#
        .to_string()
            + r#"
outputs = { nixpkgs, nasty, ... }: {
  nixosConfigurations.nasty = nixpkgs.lib.nixosSystem { system = "@LOCAL_SYSTEM@"; };
};
"#;
        let rendered = super::render_system_flake_template(
            &template,
            "0.0.3",
            "x86_64-linux",
        )
        .expect("rendered");
        assert!(rendered.contains("github:nasty-project/nasty/v0.0.3"));
        assert!(rendered.contains("\"x86_64-linux\""));
    }
}
