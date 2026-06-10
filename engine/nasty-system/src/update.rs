use base64::Engine as _;
use rnix::ast::{self};
use rowan::ast::AstNode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use thiserror::Error;
use tracing::{info, warn};

/// Primary version path — writable by the update script, not managed by NixOS.
const VERSION_PATH: &str = "/var/lib/nasty/version";
/// File holding the operator-chosen path that upgrade scripts should
/// use as Nix's `build-dir`. Empty or missing = default (sandbox lives
/// on `/tmp` → tmpfs → root). When set, scripts run with
/// `NIX_REMOTE=local` and `--option build-dir <path>` so the daemon is
/// bypassed and the sandbox spills onto the configured path. The
/// hard-won lesson from `nasty.fenski.pl` (`#293`-class disks): the
/// option only takes effect in single-user mode; the daemon ignores
/// client-side `--option build-dir`.
const UPDATE_BUILD_DIR_PATH: &str = "/var/lib/nasty/update-build-dir";
/// Fallback version path — baked in by NixOS at build time (may be a local SHA).
const VERSION_PATH_FALLBACK: &str = "/etc/nasty-version";
const UPDATE_UNIT: &str = "nasty-update";
const LOCAL_FLAKE_TARGET: &str = "/etc/nixos#nasty";
const LOCAL_REPO: &str = "/etc/nixos";
const NIXOS_FLAKE_DIR: &str = "/etc/nixos";
/// Per-box opt-in Secure Boot overlay written by the SB enrollment
/// ceremony (see `secure_boot_enrollment`). Its on-disk presence is
/// the signal that the wrapper-flake should also carry a lanzaboote
/// input — `wrapper_is_canonical_shape` rejects either-or drift,
/// `migrate_wrapper_to_canonical_shape` reconciles by re-injecting
/// lanzaboote during a re-render when the overlay is present.
const SECURE_BOOT_OVERLAY_PATH: &str = "/etc/nixos/secure-boot.nix";
const UPDATE_WEBUI_CHANGED: &str = "/var/lib/nasty/update-webui-changed";
const RELEASE_CHANNEL_PATH: &str = "/var/lib/nasty/release-channel";
const DEFAULT_NASTY_OWNER: &str = "nasty-project";
const DEFAULT_NASTY_REPO: &str = "nasty";
const DEFAULT_NASTY_REF: &str = "main";
/// Top-level wrapper-flake inputs the operator can edit via the
/// Update page's Upstream section. nixpkgs is intentionally absent
/// — it's declared as `nixpkgs.follows = "nasty/nixpkgs"` in the
/// canonical wrapper shape, so there's no meaningful URL or update
/// flag for it; editing it would be silently ignored. Showing it
/// would only invite false expectations. Same reasoning applies to
/// the engine's `version_info` payload: returning a row for nixpkgs
/// would imply operator agency that doesn't exist.
const VERSION_INPUT_NAMES: [&str; 2] = ["bcachefs-tools", "nasty"];
const SYSTEM_FLAKE_TEMPLATE_PATH: &str = "nixos/system-flake/flake.nix.template";

/// Snapshot of the wrapper-flake template this engine binary was
/// built with. Drives the legacy-wrapper migration: when an
/// existing install still has the pre-#304 wrapper shape (owns its
/// own `nixpkgs.url` and `bcachefs-tools.url`), the engine
/// re-renders the wrapper from this embedded copy on the next
/// engine-driven upgrade — independent of GitHub reachability, no
/// dependency on tagged-release rebootstrap firing. Since the
/// template ships with the engine, every release pins a known-good
/// migration target.
const EMBEDDED_WRAPPER_TEMPLATE: &str =
    include_str!("../../../nixos/system-flake/flake.nix.template");

/// nasty's own `flake.nix`, embedded at engine build time. Used to
/// resolve the default `bcachefs-tools` ref for fresh installs and
/// migrations: the canonical pin lives in `bcachefs-tools.url = ...`
/// here, and the engine reads it back via `parse_flake_input_urls`
/// rather than carrying a duplicate constant that could drift from
/// the actual flake. When the maintainer bumps the bcachefs-tools
/// version in `flake.nix`, the embedded copy here updates at the
/// next engine build, and every install that uses this engine binary
/// to render its wrapper gets the new default automatically.
const EMBEDDED_NASTY_FLAKE: &str = include_str!("../../../flake.nix");

const GITHUB_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Read the operator-chosen build-dir spillover path, if any. Trims
/// whitespace and treats empty / missing as `None`.
pub async fn read_update_build_dir() -> Option<String> {
    let raw = tokio::fs::read_to_string(UPDATE_BUILD_DIR_PATH)
        .await
        .ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Persist the build-dir spillover path. `None` clears it (the file
/// is removed so absence is the canonical "unset" state).
pub async fn write_update_build_dir(path: Option<&str>) -> Result<(), std::io::Error> {
    match path {
        Some(p) => tokio::fs::write(UPDATE_BUILD_DIR_PATH, p).await,
        None => match tokio::fs::remove_file(UPDATE_BUILD_DIR_PATH).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
    }
}

/// Enumerate mounted bcachefs filesystems under `/fs/`. Used as the
/// candidate list the WebUI offers in the build-dir dropdown — only
/// bcachefs pools are surfaced because they're the only filesystems
/// the engine guarantees are NASty-managed (we don't want to suggest
/// the operator spill builds onto a random NFS mount they happened
/// to bind under `/fs/`). Returns mount points (e.g. `/fs/first`),
/// not the underlying devices.
pub async fn list_bcachefs_pool_mounts() -> Vec<String> {
    let content = match tokio::fs::read_to_string("/proc/mounts").await {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    parse_bcachefs_mounts(&content)
}

/// Pure parser for `/proc/mounts` rows. Each line has six
/// space-separated columns: `<source> <mountpoint> <fstype> <options>
/// <freq> <passno>`. We want column 2 where column 3 is `bcachefs`
/// and column 2 starts with `/fs/`. Extracted so the parsing rules
/// can be pinned by unit tests without touching the real /proc.
fn parse_bcachefs_mounts(proc_mounts: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in proc_mounts.lines() {
        let mut cols = line.split_whitespace();
        let _source = cols.next();
        let mountpoint = match cols.next() {
            Some(m) => m,
            None => continue,
        };
        let fstype = match cols.next() {
            Some(f) => f,
            None => continue,
        };
        if fstype == "bcachefs" && mountpoint.starts_with("/fs/") {
            out.push(mountpoint.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Default subdirectory the spillover lands under so we never write
/// to the root of a bcachefs pool that may host user data. Kept as a
/// helper rather than a const so the rendered path is visible in the
/// API surface (`<pool>/.nasty-nix-build`) and operators recognize it.
fn build_dir_under_pool(pool: &str) -> String {
    format!("{}/.nasty-nix-build", pool.trim_end_matches('/'))
}

/// Resolve the stored build-dir value into the actual path scripts
/// should use. The stored value may be a bare pool mountpoint
/// (`/fs/first`) or a fully-qualified spillover dir
/// (`/fs/first/.nasty-nix-build`); the former is rewritten to the
/// latter so `.nasty-nix-build` always wraps Nix's sandbox traffic.
fn resolve_build_dir(stored: &str) -> String {
    let trimmed = stored.trim();
    if trimmed.contains("/.nasty-nix-build") || trimmed.ends_with("/.nasty-nix-build") {
        trimmed.to_string()
    } else {
        build_dir_under_pool(trimmed)
    }
}

/// Script fragments rendered into the upgrade bash template when a
/// build-dir spillover is configured. All four strings are empty when
/// no spillover is configured, so unconditional `{...}` interpolation
/// stays a no-op for default installs.
struct BuildDirFragments {
    /// Multi-line bash block, run once before the rebuild, that
    /// ensures the spillover directory exists with the strict 0755
    /// perms Nix demands (it refuses world-writable build-dirs).
    /// Empty when no spillover is configured.
    setup: String,
    /// Inline env prefix for the `nixos-rebuild` line, including the
    /// trailing space — e.g. `"NIX_REMOTE=local "`. Empty otherwise.
    /// Needed because client-side `--option build-dir` is only honored
    /// in single-user mode; the daemon ignores it.
    env_prefix: String,
    /// CLI suffix for the `nixos-rebuild` line, including the leading
    /// space — e.g. `" --option build-dir /fs/first/.nasty-nix-build"`.
    opt_suffix: String,
    /// Cleanup block run after the rebuild (regardless of outcome) to
    /// reclaim the spillover space once the build is done. Empty
    /// otherwise.
    cleanup: String,
}

fn build_dir_fragments(stored: Option<&str>) -> BuildDirFragments {
    match stored {
        None => BuildDirFragments {
            setup: String::new(),
            env_prefix: String::new(),
            opt_suffix: String::new(),
            cleanup: String::new(),
        },
        Some(stored) => {
            let path = resolve_build_dir(stored);
            BuildDirFragments {
                setup: format!(
                    "echo \"==> Preparing build-dir spillover at {path}...\"\n\
                     mkdir -p \"{path}\"\n\
                     chmod 0755 \"{path}\"\n"
                ),
                env_prefix: "NIX_REMOTE=local ".to_string(),
                opt_suffix: format!(" --option build-dir \"{path}\""),
                cleanup: format!("rm -rf \"{path}\"/* 2>/dev/null || true\n"),
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateBuildDirConfig {
    /// Persisted spillover path (`None` = unset, builds use the
    /// default `/tmp` → tmpfs → root path). Returned verbatim from
    /// the on-disk setting, **not** the auto-resolved
    /// `<pool>/.nasty-nix-build` derivation — the WebUI dropdown
    /// stores pool roots, the engine resolves at script-render time.
    pub path: Option<String>,
    /// Mounted bcachefs pool roots under `/fs/` discovered live from
    /// `/proc/mounts`. Used by the WebUI to populate the dropdown of
    /// viable spillover targets. Empty on single-disk (mode-1)
    /// installs that don't have a separate data pool — the feature
    /// can't help those boxes and the UI hides the option.
    pub available_pools: Vec<String>,
    /// Resolved sandbox path the engine would actually pass to
    /// `nixos-rebuild` (i.e. `<pool>/.nasty-nix-build`). Surfaced so
    /// the WebUI can show operators where the spillover will land
    /// without re-implementing the derivation rule.
    pub resolved: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionInputInfo {
    /// Flake input name (e.g. `bcachefs-tools`, `nasty`).
    pub name: String,
    /// Exact `input.url` string from `/etc/nixos/flake.nix`.
    pub url: String,
    /// Locked commit SHA from `/etc/nixos/flake.lock` (shortened to 12 chars).
    pub rev: Option<String>,
    /// Human-meaningful ref string from `flake.lock`'s
    /// `nodes[<name>].original.ref` — typically a tag like `v1.38.3`
    /// or a branch name like `main`. When present, prefer this for
    /// display over `rev` (which is just a 12-char SHA prefix).
    /// `None` when the lock node has no `original.ref` set (e.g.
    /// inputs referenced by raw commit hash, or inputs the lock
    /// doesn't carry an `original` block for).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
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
    /// Result of the last upgrade-unit invocation: `"success"`, `"failed"`,
    /// or `None` if no upgrade has ever been kicked off (or it's still
    /// running). When `"failed"`, the engine forces `update_available =
    /// Some(true)` regardless of the tag comparison so the WebUI keeps
    /// offering Upgrade — a failed rebuild often leaves `flake.lock`
    /// pointing at the target tag, which would otherwise make the check
    /// look like a no-op.
    pub last_attempt: Option<String>,
    /// Human-readable explanation when the latest-version lookup failed
    /// (GitHub unreachable, rate-limited, refused token, …). Populated
    /// by `check()`; `version()` leaves it `None`. Surfaced in the UI
    /// as an amber banner so operators don't see a silent dash when
    /// GitHub is misbehaving — previously the failure mode was
    /// indistinguishable from "no check has ever run".
    pub error: Option<String>,
    /// Snapshot of each tracked flake input (`nasty`, `nixpkgs`,
    /// `bcachefs-tools`) — name, URL, locked rev. Embedded here so the
    /// Version page can render all three pinned components in the
    /// summary card without making a second RPC. None when the
    /// engine can't read the local flake (parse error, fresh install
    /// pre-bootstrap, etc); the UI falls back to the nasty rev alone
    /// in that case.
    pub inputs: Option<Vec<VersionInputInfo>>,
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

impl Default for UpdateService {
    fn default() -> Self {
        Self::new()
    }
}

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
            last_attempt: last_upgrade_attempt_result().await,
            error: None,
            inputs: self.version_info().await.ok().map(|v| v.inputs),
        }
    }

    /// Read the exact upstream input URLs and locked revs from the live
    /// `/etc/nixos` flake on the installed system.
    ///
    /// Only the inputs the operator can edit are returned: `nasty`
    /// and `bcachefs-tools`. nixpkgs is excluded — it always follows
    /// nasty (canonical 0.0.9 shape), so there's no operator-facing
    /// choice to surface; including it would imply agency that
    /// doesn't exist.
    ///
    /// Inputs the wrapper *owns* a `.url` for (the steady state for
    /// both `nasty` and `bcachefs-tools` after migration) are
    /// reported with their literal URL. During the transient window
    /// where a post-#308 wrapper hasn't yet been migrated to canonical
    /// shape, `bcachefs-tools` may still be a follows declaration —
    /// in that case it's reported with a synthetic
    /// `follows:nasty/bcachefs-tools` marker. `rev` always comes from
    /// `flake.lock`, which carries resolved revs for followed inputs
    /// too.
    pub async fn version_info(&self) -> Result<VersionInfo, UpdateError> {
        let urls = read_flake_input_urls().await?;
        let lock_entries = read_flake_lock_entries_async().await;

        let mut inputs = Vec::with_capacity(VERSION_INPUT_NAMES.len());
        for name in VERSION_INPUT_NAMES {
            let url = match urls.get(name) {
                Some(u) => u.clone(),
                None if *name == *"nasty" => {
                    // `nasty.url` is the one input the wrapper must
                    // own — there's nothing to follow it from.
                    return Err(UpdateError::CommandFailed(format!(
                        "missing nasty.url in {NIXOS_FLAKE_DIR}/flake.nix"
                    )));
                }
                None => format!("follows:nasty/{name}"),
            };
            let entry = lock_entries.get(name).cloned().unwrap_or_default();
            inputs.push(VersionInputInfo {
                name: name.to_string(),
                url,
                rev: entry.rev,
                tag: entry.tag,
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

    /// Rewrite `/etc/nixos/flake.nix` in place if it's not in the
    /// canonical 0.0.9 shape (`nixpkgs.follows = "nasty/nixpkgs"` and
    /// `bcachefs-tools.url = "github:.../<tag>"`). Handles every
    /// historical starting state:
    ///
    /// - Pre-#304 wrappers (own `.url` for both): nixpkgs converted to
    ///   follows; bcachefs-tools.url preserved with the operator's
    ///   existing ref.
    /// - Post-#308 wrappers (`.follows` for both): nixpkgs stays
    ///   follows; bcachefs-tools rewritten back to `.url`, ref
    ///   resolved from the current flake.lock's `original.ref` on
    ///   the followed node.
    /// - Already-canonical wrappers: no-op, returns Ok(false) without
    ///   touching disk.
    ///
    /// Returns `Ok(true)` if a write happened, `Ok(false)` if no
    /// migration was needed.
    pub async fn maybe_migrate_wrapper_shape(&self) -> Result<bool, UpdateError> {
        let path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
        let current = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("read {path}: {e}")))?;
        let overlay_present = tokio::fs::try_exists(SECURE_BOOT_OVERLAY_PATH)
            .await
            .unwrap_or(false);
        if wrapper_is_canonical_shape(&current, overlay_present) {
            return Ok(false);
        }
        let local_system = detect_local_system().await?;
        let migrated =
            migrate_wrapper_to_canonical_shape(&current, &local_system, overlay_present).await?;
        tokio::fs::write(&path, &migrated)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("write {path}: {e}")))?;
        info!(
            "Migrated wrapper-flake at {path} to canonical 0.0.9 shape \
             (nixpkgs follows nasty for cachix coverage; bcachefs-tools \
             pinned independently so the operator can stick with a \
             known-good rev across nasty bumps)"
        );
        Ok(true)
    }

    pub async fn upgrade_tagged_release(&self) -> Result<(), UpdateError> {
        let update_status = self.status().await;
        if update_status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        // Belt-and-suspenders to the rebootstrap path further down:
        // if the wrapper is in legacy (own-url) shape, migrate it to
        // the follows shape before anything else. The existing
        // rebootstrap path also detects this case via content-hash
        // drift between the local wrapper and the upstream template
        // — but only AFTER the "already at latest tag" early-exit
        // below. Running the migration up front means a user pinned
        // at the latest tag can still get reshaped even when there's
        // no version bump to apply. No-op when already in follows
        // shape.
        if let Err(e) = self.maybe_migrate_wrapper_shape().await {
            warn!(
                target: "nasty::update",
                "wrapper migration to follows shape skipped: {e}"
            );
        }

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
        let current_flake_path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
        let current_flake = tokio::fs::read_to_string(&current_flake_path)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("read {current_flake_path}: {e}")))?;
        // Pin the bcachefs-tools rev that the target release was tested
        // with — overriding whatever the operator had pinned locally.
        // Release = atomic bundle of (nasty, bcachefs-tools); switching
        // to a release means accepting the tested combination. Reads
        // the target release's flake.lock from GitHub, falls back to
        // the engine's embedded default if the fetch / parse misses
        // (see fetch_release_bcachefs_ref's doc for the rationale).
        let bcachefs_ref = fetch_release_bcachefs_ref(
            token.as_deref(),
            DEFAULT_NASTY_OWNER,
            DEFAULT_NASTY_REPO,
            &release_status.latest_tag,
        )
        .await?;
        // Render is now placeholder-free for bcachefs (template
        // carries a hardcoded default). The release's actual
        // bcachefs ref gets baked in via a post-render
        // `rewrite_flake_input_urls` pass below — works on every
        // historical engine version because rewrite_flake_input_urls
        // predates the placeholder mechanism. Same code path
        // whether we rebootstrapped or just URL-rewrote.
        let next_flake = if should_rebootstrap_wrapper_flake(&current_flake, &template)? {
            render_system_flake_template(&template, &release_status.latest_tag, &local_system)?
        } else {
            current_flake.clone()
        };
        let next_flake = rewrite_flake_input_urls(
            &next_flake,
            &HashMap::from([
                (String::from("nasty"), release_status.latest_url.clone()),
                (
                    String::from("bcachefs-tools"),
                    format!("github:koverstreet/bcachefs-tools/{bcachefs_ref}"),
                ),
            ]),
        )?;

        // Best-effort cleanup of any prior unit state. `try_run` logs spawn
        // failures and non-zero exits at warn! — a missing-unit "exited 5"
        // shows up in the journal but doesn't propagate.
        nasty_common::cmd::try_run("systemctl", &["reset-failed", UPDATE_UNIT]).await;
        nasty_common::cmd::try_run("systemctl", &["stop", UPDATE_UNIT]).await;

        let flake_temp_path = "/tmp/nasty-upgrade-flake.nix";
        tokio::fs::write(flake_temp_path, &next_flake)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("write {flake_temp_path}: {e}")))?;

        let local_flake = local_flake();
        let bd = build_dir_fragments(read_update_build_dir().await.as_deref());
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
_proxy_conf() {{
    # Resolve the active Caddyfile via the etc-symlink the Caddy
    # NixOS module sets up.  Each generation has its own /etc/caddy/
    # tree, so the resolved path doubles as a generation identity —
    # comparing it before/after a rebuild tells us whether the WebUI
    # closure (or anything else in the Caddyfile) changed.
    readlink -f /run/current-system/etc/caddy/Caddyfile 2>/dev/null || true
}}
_PROXY_CONF_BEFORE=$(_proxy_conf)
WEBUI_BEFORE=$([ -n "$_PROXY_CONF_BEFORE" ] && grep 'nasty-webui' "$_PROXY_CONF_BEFORE" 2>/dev/null | head -1 || echo "")
echo "false" > {UPDATE_WEBUI_CHANGED}

{bd_setup}
echo "==> Updating local system flake..."
cd {NIXOS_FLAKE_DIR}
cp {flake_temp_path} flake.nix
# Only refresh the `nasty` input by default. Operators who want a
# kernel / system bump pin nixpkgs and bcachefs-tools explicitly
# via the Version page (system.version.switch RPC) — the upgrade
# flow is NASty-only so a small release tag doesn't drag the whole
# distro along with it.
nix flake update nasty

echo "==> Rebuilding system..."
_RC=0
{bd_env}NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}{bd_opt} || _RC=$?
{bd_cleanup}
if [ "$_RC" -ne 0 ]; then
    echo
    echo "==> nixos-rebuild switch failed (exit $_RC)."
    echo "==> Below is the tail of the switch-to-configuration journal — it"
    echo "    usually carries the real error (systemd-boot tracebacks, failed"
    echo "    service starts, ENOSPC on /boot, etc):"
    echo "--- journalctl -u nixos-rebuild-switch-to-configuration -n 60 ---"
    journalctl -u nixos-rebuild-switch-to-configuration --no-pager -n 60 || true
    echo "--- end journal dump ---"
    exit "$_RC"
fi

_PROXY_CONF_AFTER=$(_proxy_conf)
WEBUI_AFTER=$([ -n "$_PROXY_CONF_AFTER" ] && grep 'nasty-webui' "$_PROXY_CONF_AFTER" 2>/dev/null | head -1 || echo "")
if [ -n "$WEBUI_BEFORE" ] && [ "$WEBUI_BEFORE" != "$WEBUI_AFTER" ]; then
    echo "true" > {UPDATE_WEBUI_CHANGED}
fi

echo "{latest_tag}" > {VERSION_PATH}
echo "==> Update complete!"
"#,
            latest_tag = release_status.latest_tag,
            bd_setup = bd.setup,
            bd_env = bd.env_prefix,
            bd_opt = bd.opt_suffix,
            bd_cleanup = bd.cleanup,
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

    /// Snapshot the upgrade-script build-dir setting + the live list
    /// of bcachefs pools the operator could spill to.
    pub async fn get_update_build_dir(&self) -> UpdateBuildDirConfig {
        let path = read_update_build_dir().await;
        UpdateBuildDirConfig {
            resolved: path.as_deref().map(resolve_build_dir),
            path,
            available_pools: list_bcachefs_pool_mounts().await,
        }
    }

    /// Persist the operator's build-dir choice. The stored value is
    /// validated lightly: it must be either `None` (unset) or one of
    /// the currently-mounted bcachefs pool roots. We don't accept
    /// arbitrary paths because the upgrade script will `chmod 0755`
    /// the directory and clean it after the build — pointing it at
    /// `/etc` would be a disaster.
    pub async fn set_update_build_dir(
        &self,
        path: Option<String>,
    ) -> Result<UpdateBuildDirConfig, UpdateError> {
        if let Some(ref p) = path {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                write_update_build_dir(None)
                    .await
                    .map_err(|e| UpdateError::CommandFailed(format!("clear build-dir: {e}")))?;
            } else {
                let pools = list_bcachefs_pool_mounts().await;
                if !pools.iter().any(|m| m == trimmed) {
                    return Err(UpdateError::CommandFailed(format!(
                        "build-dir must be one of the mounted bcachefs pools \
                         ({}); got `{trimmed}`",
                        if pools.is_empty() {
                            "none currently mounted".to_string()
                        } else {
                            pools.join(", ")
                        }
                    )));
                }
                write_update_build_dir(Some(trimmed))
                    .await
                    .map_err(|e| UpdateError::CommandFailed(format!("write build-dir: {e}")))?;
                info!("Update build-dir spillover set to {trimmed}");
            }
        } else {
            write_update_build_dir(None)
                .await
                .map_err(|e| UpdateError::CommandFailed(format!("clear build-dir: {e}")))?;
            info!("Update build-dir spillover cleared");
        }
        Ok(self.get_update_build_dir().await)
    }

    /// Check if an update is available by comparing local rev to GitHub
    pub async fn check(&self) -> Result<UpdateInfo, UpdateError> {
        let current = read_current_version().await;
        let channel = read_channel().await;
        let nasty_input = read_nasty_input_source().await;

        // Mild/Spicy: find latest matching tag (v* or s*) on the configured repo.
        // Nasty: track the wrapper flake's configured branch/ref.
        //
        // We collect every failure into `lookup_error` so the UI can
        // surface *why* the check came back empty instead of just
        // dropping the user on a blank "Latest" column. Returning the
        // sentinel string "unknown" (the previous behaviour) was
        // indistinguishable from "this is a fresh box and check has
        // never run", which is exactly the silent-failure mode the
        // operator on nasty.0f.ee couldn't see through.
        let mut lookup_error: Option<String> = None;
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
                    Err(e) => {
                        warn!(target: "nasty::update", "tagged-release check failed: {e}");
                        lookup_error = Some(e.to_string());
                        "unknown".to_string()
                    }
                }
            }
            ReleaseChannel::Nasty => match check_via_github_api_branch(
                &nasty_input.owner,
                &nasty_input.repo,
                &nasty_input.tracked_ref,
            )
            .await
            {
                Ok(sha) => sha,
                Err(api_err) => {
                    warn!(
                        target: "nasty::update",
                        "GitHub API branch check failed ({api_err}); falling back to git ls-remote",
                    );
                    let token = read_github_token().await;
                    match check_via_git_ls_remote(
                        token.as_deref(),
                        &nasty_input.repo_url(),
                        &format!("refs/heads/{}", nasty_input.tracked_ref),
                    )
                    .await
                    {
                        Ok(sha) => sha,
                        Err(ls_err) => {
                            warn!(target: "nasty::update", "git ls-remote also failed: {ls_err}");
                            lookup_error =
                                Some(format!("GitHub API: {api_err}; git ls-remote: {ls_err}"));
                            "unknown".to_string()
                        }
                    }
                }
            },
        };

        // Strip "-dirty" suffix for comparison — the local build has a dirty
        // git tree (hardware-configuration.nix) but the commit is the same
        let current_clean = current.trim_end_matches("-dirty");
        let mut available = if latest == "unknown" {
            None
        } else if current_clean == "dev" {
            Some(true) // dev builds should always offer to update
        } else {
            Some(current_clean != latest)
        };

        // Revs match? The wrapper-flake template can still have drifted
        // (e.g. a maintainer-side bcachefs URL bump landed without
        // moving the lock here). Surface that as "update available" so
        // the Upgrade button appears — clicking it triggers a
        // rebootstrap of /etc/nixos/flake.nix from the new template.
        if available == Some(false)
            && let Some(drifted) = check_wrapper_template_drift(
                &nasty_input.owner,
                &nasty_input.repo,
                &nasty_input.tracked_ref,
            )
            .await
            && drifted
        {
            available = Some(true);
        }

        // If the previous upgrade attempt failed (ENOSPC on /boot, panic
        // during activation, …) keep the Upgrade button visible so the
        // operator can retry. The version comparison alone would say
        // "up to date" in this state because `nix flake update` rewrites
        // flake.lock *before* the rebuild runs.
        let last_attempt = last_upgrade_attempt_result().await;
        if last_attempt.as_deref() == Some("failed") {
            available = Some(true);
        }

        let inputs = self.version_info().await.ok().map(|v| v.inputs);

        Ok(UpdateInfo {
            current_version: current,
            latest_version: Some(latest),
            update_available: available,
            channel,
            last_attempt,
            error: lookup_error,
            inputs,
        })
    }

    /// Start a system update via nixos-rebuild
    pub async fn apply(&self) -> Result<(), UpdateError> {
        let status = self.status().await;
        if status.state == "running" {
            return Err(UpdateError::AlreadyRunning);
        }

        // Clean up any previous update unit
        // Best-effort cleanup of any prior unit state. `try_run` logs spawn
        // failures and non-zero exits at warn! — a missing-unit "exited 5"
        // shows up in the journal but doesn't propagate.
        nasty_common::cmd::try_run("systemctl", &["reset-failed", UPDATE_UNIT]).await;
        nasty_common::cmd::try_run("systemctl", &["stop", UPDATE_UNIT]).await;

        // Opportunistically migrate legacy-shape wrappers (own
        // nixpkgs.url + bcachefs-tools.url) to the follows shape
        // (#304) BEFORE running the upgrade. After this the
        // wrapper's single `nix flake update nasty` step also
        // resolves the followed inputs, putting the resolved
        // closure back in cachix range and ending the per-bump
        // 92-Rust-crate recompile that drifted boxes had been
        // hitting. No-op when the wrapper is already in follows
        // shape, so safe to call every time.
        if let Err(e) = self.maybe_migrate_wrapper_shape().await {
            warn!(
                target: "nasty::update",
                "wrapper migration to follows shape skipped: {e}"
            );
        }

        // Build the update script:
        // 1. Update the local wrapper flake inputs (channel-specific:
        //    Mild/Spicy pin nasty to a release tag, Nasty refreshes
        //    nasty only — followed nixpkgs + bcachefs-tools move
        //    transitively via nasty's lock)
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
                // Only refresh the `nasty` input. We used to also pull
                // fresh nixpkgs and bcachefs-tools here so main-tracking
                // users would get the weekly nixpkgs bump, but that
                // turned a "ship the latest NASty commits" click into
                // an unscheduled distro upgrade — exactly the kind of
                // surprise that ate a /boot's worth of space on more
                // than one box. Operators who want a kernel / system
                // bump pin those inputs explicitly via the Version
                // page (system.version.switch RPC); for everyone else
                // a small NASty bump now means a small NASty bump.
                format!(
                    "echo \"==> Updating nasty input ({})...\"\n\
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
        let bd = build_dir_fragments(read_update_build_dir().await.as_deref());
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
# Capture the active Caddyfile store path before rebuild so we can
# detect whether the WebUI closure changed.  After nixos-rebuild
# switch the /run/current-system symlink updates atomically before
# we read the AFTER value, so the BEFORE/AFTER comparison always
# spans old vs new closure.
_proxy_conf() {{
    # Resolve the active Caddyfile via the etc-symlink the Caddy
    # NixOS module sets up.  Each generation has its own /etc/caddy/
    # tree, so the resolved path doubles as a generation identity —
    # comparing it before/after a rebuild tells us whether the WebUI
    # closure (or anything else in the Caddyfile) changed.
    readlink -f /run/current-system/etc/caddy/Caddyfile 2>/dev/null || true
}}
_PROXY_CONF_BEFORE=$(_proxy_conf)
WEBUI_BEFORE=$([ -n "$_PROXY_CONF_BEFORE" ] && grep 'nasty-webui' "$_PROXY_CONF_BEFORE" 2>/dev/null | head -1 || echo "")
{bd_setup}
echo "==> Updating local system flake..."
cd {LOCAL_REPO}
{update_step}

# Generation cleanup is handled by nix.gc (systemd timer) in the NixOS config.
# No custom GC logic needed here — just rebuild.

echo "==> Rebuilding system..."
_RC=0
{bd_env}NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}{bd_opt} || _RC=$?
{bd_cleanup}
if [ "$_RC" -ne 0 ]; then
    echo
    echo "==> nixos-rebuild switch failed (exit $_RC)."
    echo "==> Below is the tail of the switch-to-configuration journal — it"
    echo "    usually carries the real error (systemd-boot tracebacks, failed"
    echo "    service starts, ENOSPC on /boot, etc):"
    echo "--- journalctl -u nixos-rebuild-switch-to-configuration -n 60 ---"
    journalctl -u nixos-rebuild-switch-to-configuration --no-pager -n 60 || true
    echo "--- end journal dump ---"
    exit "$_RC"
fi

# Detect if the webui store path changed so the frontend knows whether to prompt a reload.
# /run/current-system now points to the newly activated closure.
_PROXY_CONF_AFTER=$(_proxy_conf)
WEBUI_AFTER=$([ -n "$_PROXY_CONF_AFTER" ] && grep 'nasty-webui' "$_PROXY_CONF_AFTER" 2>/dev/null | head -1 || echo "")
if [ -n "$WEBUI_BEFORE" ] && [ "$WEBUI_BEFORE" != "$WEBUI_AFTER" ]; then
    echo "true" > {UPDATE_WEBUI_CHANGED}
else
    echo "false" > {UPDATE_WEBUI_CHANGED}
fi

# Write the active nasty input version to the writable version path.
{installed_version_expr}

echo "==> Update complete!"
"#,
            bd_setup = bd.setup,
            bd_env = bd.env_prefix,
            bd_opt = bd.opt_suffix,
            bd_cleanup = bd.cleanup,
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

        // Migrate the wrapper to the canonical 0.0.9 shape before
        // anything else — the input-presence checks below (and
        // rewrite_flake_input_urls deeper in the call chain) demand
        // `.url` declarations for both editable inputs. Without this
        // upfront migration a post-#308 follows-shape wrapper (no
        // `bcachefs-tools.url`) would error "missing bcachefs-tools.url"
        // on the very first WebUI dev-build click — the same
        // self-update strand that #312 set out to fix for nixpkgs but
        // missed for bcachefs-tools. apply() and upgrade_tagged_release()
        // already do this migration up-front; doing it here too closes
        // the third edge of the triangle.
        //
        // Best-effort: failure logged as warn, doesn't abort the
        // request. If the wrapper can't be migrated (fork URL, parse
        // failure, etc.) the operator gets a clearer error from the
        // input-presence check below.
        if let Err(e) = self.maybe_migrate_wrapper_shape().await {
            warn!(
                target: "nasty::update",
                "wrapper migration to canonical shape skipped during version_switch: {e}"
            );
        }

        let current_urls = read_flake_input_urls().await?;
        let mut seen = HashSet::new();
        let mut requested = HashMap::new();
        for input in req.inputs {
            // nixpkgs is deliberately excluded from VERSION_INPUT_NAMES
            // — the wrapper declares `nixpkgs.follows = "nasty/nixpkgs"`
            // for cachix-coverage reasons, so editing its URL or
            // toggling its update flag here would be inert. Reject
            // the request rather than silently dropping the entry so
            // clients fail loudly during development. (WebUI is wired
            // not to send nixpkgs in the canonical 0.0.9 UI.)
            if input.name == "nixpkgs" {
                return Err(UpdateError::CommandFailed(
                    "nixpkgs is not an editable input — it follows nasty's lock for cachix \
                     coverage. Remove the nixpkgs entry from this version_switch request."
                        .into(),
                ));
            }
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

        // Best-effort cleanup of any prior unit state. `try_run` logs spawn
        // failures and non-zero exits at warn! — a missing-unit "exited 5"
        // shows up in the journal but doesn't propagate.
        nasty_common::cmd::try_run("systemctl", &["reset-failed", UPDATE_UNIT]).await;
        nasty_common::cmd::try_run("systemctl", &["stop", UPDATE_UNIT]).await;

        let flake_path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
        let current_flake = tokio::fs::read_to_string(&flake_path)
            .await
            .map_err(|e| UpdateError::CommandFailed(format!("read {flake_path}: {e}")))?;
        let requested_nasty_url = requested
            .get("nasty")
            .map(|input| input.url.clone())
            .ok_or_else(|| UpdateError::CommandFailed("missing request entry for nasty".into()))?;
        // Whether we rebootstrap (re-render the wrapper flake from the
        // upstream template) depends on whether the user's nasty URL
        // points at a canonical ref in nasty-project/nasty. For any
        // canonical ref we fetch the template from that ref and compare
        // wrapper-flake content hashes; on drift, re-render with the
        // operator's chosen nasty + bcachefs-tools refs as inputs.
        // nixpkgs isn't an input the operator controls — it follows
        // nasty's lock unconditionally, see the template comment for
        // the cachix-coverage rationale.
        let rewritten_flake = match parse_official_nasty_ref(&requested_nasty_url) {
            Some((nasty_ref, _is_release_tag)) => {
                let token = read_github_token().await;
                let template = fetch_github_text_file(
                    token.as_deref(),
                    DEFAULT_NASTY_OWNER,
                    DEFAULT_NASTY_REPO,
                    SYSTEM_FLAKE_TEMPLATE_PATH,
                    &nasty_ref,
                )
                .await?;

                // Render is placeholder-free — the template carries
                // a hardcoded bcachefs default. The operator's
                // requested bcachefs URL is applied as a post-render
                // URL rewrite (same code path that handles the
                // no-rebootstrap case), which works on every
                // historical engine version.
                let base = if should_rebootstrap_wrapper_flake(&current_flake, &template)? {
                    let local_system = detect_local_system().await?;
                    render_system_flake_template_with_ref(&template, &nasty_ref, &local_system)?
                } else {
                    current_flake.clone()
                };
                // Preserve operator's existing bcachefs-tools pin
                // across rebootstrap. Without this, a template-hash
                // change (e.g., a maintainer-side bcachefs default
                // bump) would silently overwrite the operator's
                // custom pin with the template's new default. The
                // request's bcachefs URL is what the operator
                // actually wants (the WebUI populates it from
                // version_info, which read the current wrapper);
                // ensure it's in the rewrite map even when the
                // request URL matches current state (so url_changes
                // is empty for it).
                let mut flake_replacements: HashMap<String, String> = url_changes
                    .iter()
                    .map(|(name, url)| (name.clone(), url.clone()))
                    .collect();
                if let Some(bcachefs_input) = requested.get("bcachefs-tools") {
                    flake_replacements
                        .entry(String::from("bcachefs-tools"))
                        .or_insert_with(|| bcachefs_input.url.clone());
                }
                if flake_replacements.is_empty() {
                    base
                } else {
                    rewrite_flake_input_urls(&base, &flake_replacements)?
                }
            }
            None => {
                // Fork or non-canonical URL: we can't safely fetch a
                // template from an unknown source, so just rewrite the
                // input URLs the user explicitly requested.
                let flake_replacements = url_changes
                    .iter()
                    .map(|(name, url)| (name.clone(), url.clone()))
                    .collect::<HashMap<_, _>>();
                rewrite_flake_input_urls(&current_flake, &flake_replacements)?
            }
        };
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
        let bd = build_dir_fragments(read_update_build_dir().await.as_deref());
        let script = format!(
            r#"#!/bin/bash
set -euo pipefail
export PATH="/run/current-system/sw/bin:$PATH"
_proxy_conf() {{
    # Resolve the active Caddyfile via the etc-symlink the Caddy
    # NixOS module sets up.  Each generation has its own /etc/caddy/
    # tree, so the resolved path doubles as a generation identity —
    # comparing it before/after a rebuild tells us whether the WebUI
    # closure (or anything else in the Caddyfile) changed.
    readlink -f /run/current-system/etc/caddy/Caddyfile 2>/dev/null || true
}}
_PROXY_CONF_BEFORE=$(_proxy_conf)
WEBUI_BEFORE=$([ -n "$_PROXY_CONF_BEFORE" ] && grep 'nasty-webui' "$_PROXY_CONF_BEFORE" 2>/dev/null | head -1 || echo "")
echo "false" > {UPDATE_WEBUI_CHANGED}

{bd_setup}
echo "==> Updating local system flake..."
cd {NIXOS_FLAKE_DIR}
LOCK_BEFORE=$(sha256sum flake.lock 2>/dev/null | awk '{{print $1}}' || true)
cp {flake_temp_path} flake.nix
{update_steps}
LOCK_AFTER=$(sha256sum flake.lock 2>/dev/null | awk '{{print $1}}' || true)

if [ "$LOCK_BEFORE" != "$LOCK_AFTER" ]; then
    echo "==> Rebuilding system..."
    cp flake.lock flake.lock.pre-rebuild
    if {bd_env}NIXOS_INSTALL_BOOTLOADER=0 nixos-rebuild switch --flake {local_flake}{bd_opt}; then
        rm -f flake.lock.pre-rebuild
        {bd_cleanup}
    else
        RC=$?
        {bd_cleanup}
        echo "==> Rebuild failed (exit $RC). Restoring previous flake.lock so update can be retried."
        cp flake.lock.pre-rebuild flake.lock
        rm -f flake.lock.pre-rebuild
        echo
        echo "==> Below is the tail of the switch-to-configuration journal — it"
        echo "    usually carries the real error (systemd-boot tracebacks, failed"
        echo "    service starts, ENOSPC on /boot, etc):"
        echo "--- journalctl -u nixos-rebuild-switch-to-configuration -n 60 ---"
        journalctl -u nixos-rebuild-switch-to-configuration --no-pager -n 60 || true
        echo "--- end journal dump ---"
        exit "$RC"
    fi
    _PROXY_CONF_AFTER=$(_proxy_conf)
    WEBUI_AFTER=$([ -n "$_PROXY_CONF_AFTER" ] && grep 'nasty-webui' "$_PROXY_CONF_AFTER" 2>/dev/null | head -1 || echo "")
    if [ -n "$WEBUI_BEFORE" ] && [ "$WEBUI_BEFORE" != "$WEBUI_AFTER" ]; then
        echo "true" > {UPDATE_WEBUI_CHANGED}
    fi
    NASTY_REV=$(jq -r '.nodes["nasty"].locked.rev // empty' flake.lock 2>/dev/null || true)
    [ -n "$NASTY_REV" ] && echo "${{NASTY_REV:0:7}}" > {VERSION_PATH}
else
    echo "==> No flake.lock changes detected; skipping rebuild."
fi
echo "==> Update complete!"
"#,
            bd_setup = bd.setup,
            bd_env = bd.env_prefix,
            bd_opt = bd.opt_suffix,
            bd_cleanup = bd.cleanup,
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

        // Best-effort cleanup of any prior unit state. `try_run` logs spawn
        // failures and non-zero exits at warn! — a missing-unit "exited 5"
        // shows up in the journal but doesn't propagate.
        nasty_common::cmd::try_run("systemctl", &["reset-failed", UPDATE_UNIT]).await;
        nasty_common::cmd::try_run("systemctl", &["stop", UPDATE_UNIT]).await;

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

            let booted =
                is_booted_generation(booted_store_path.as_deref(), gen_store_path.as_deref());

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

        // Best-effort cleanup of any prior unit state. `try_run` logs spawn
        // failures and non-zero exits at warn! — a missing-unit "exited 5"
        // shows up in the journal but doesn't propagate.
        nasty_common::cmd::try_run("systemctl", &["reset-failed", UPDATE_UNIT]).await;
        nasty_common::cmd::try_run("systemctl", &["stop", UPDATE_UNIT]).await;

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

        let gen_target = tokio::fs::read_link(&profile_link).await.map_err(|e| {
            // Keep the io::Error in the message — "permission denied" or
            // any other failure mode shouldn't be misreported as "doesn't
            // exist" (that drove a debug-cycle once already).
            UpdateError::CommandFailed(format!(
                "generation {gen_id}: read_link({profile_link}): {e}"
            ))
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
        if let Ok(booted) = tokio::fs::read_link("/run/booted-system").await
            && gen_target == booted
        {
            return Err(UpdateError::CommandFailed(
                "cannot delete the booted generation".into(),
            ));
        }

        // Remove the profile link
        tokio::fs::remove_file(&profile_link).await.map_err(|e| {
            UpdateError::CommandFailed(format!("failed to remove generation {gen_id}: {e}"))
        })?;

        // Clean up the label if any. A persistence failure here
        // leaves a stale label pointing at a deleted generation —
        // surface it so the user can match a "phantom label" report
        // to the underlying save error.
        let mut labels = load_generation_labels().await;
        if labels.remove(&gen_id).is_some()
            && let Err(e) = save_generation_labels(&labels).await
        {
            warn!("save_generation_labels after delete of gen {gen_id} failed: {e}");
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

                map_systemd_state(active_state, result).to_string()
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
}

///
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
        GITHUB_FETCH_TIMEOUT,
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
    // Template carries a hardcoded bcachefs-tools URL default; no
    // per-render substitution needed here. Caller can mutate the
    // bcachefs ref later via `rewrite_flake_input_urls` if they
    // need a non-default pin (the tagged-release path does this).
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

/// The default `bcachefs-tools` ref to use when bootstrapping a
/// fresh wrapper or when no explicit ref is available — extracted
/// from the embedded copy of nasty's own `flake.nix` so it always
/// matches what the engine binary was built against. This is the
/// "canonical bundled" rev: every release tag of nasty has its own
/// embedded flake.nix and thus its own default; fresh installs land
/// on the rev nasty's CI tested with.
///
/// Returns `Err` if the embedded flake.nix doesn't parse or doesn't
/// declare a `bcachefs-tools.url` — both of which should be caught
/// by tests in this same module before any binary ships.
pub(crate) fn embedded_default_bcachefs_tools_ref() -> Result<String, UpdateError> {
    let urls = parse_flake_input_urls(EMBEDDED_NASTY_FLAKE)?;
    let input = urls.get("bcachefs-tools").ok_or_else(|| {
        UpdateError::CommandFailed(
            "embedded nasty flake.nix has no bcachefs-tools.url to extract default ref from".into(),
        )
    })?;
    input
        .url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            UpdateError::CommandFailed(format!(
                "embedded nasty flake.nix bcachefs-tools.url ({}) has no ref path segment",
                input.url
            ))
        })
}

/// Render the wrapper-flake template, normalizing the nasty version
/// to a leading-`v` semver tag. Used by the install-time bootstrap
/// path (and a few legacy callers) that pass the engine's Cargo
/// version like "0.0.7" rather than a pre-formatted ref.
fn render_system_flake_template(
    template: &str,
    nasty_version: &str,
    local_system: &str,
) -> Result<String, UpdateError> {
    let nasty_tag = normalize_release_tag(nasty_version)?;
    render_system_flake_template_with_ref(template, &nasty_tag, local_system)
}

/// Render the wrapper-flake template with the nasty ref passed
/// verbatim. Branch refs like `main` aren't semver — the tag-path
/// `render_system_flake_template` would reject them — but they're
/// valid GitHub refs and we want main-trackers to be able to
/// rebootstrap too.
///
/// **Backwards-compat invariant** (the v0.0.8-strand lesson): the
/// only placeholders this renderer substitutes are the three below
/// — `@NASTY_VERSION@`, `@LOCAL_SYSTEM@`, `@WRAPPER_FLAKE_VERSION@`.
/// Older engines (notably v0.0.8) substitute the same three and
/// silently no-op on anything else, leaving any unknown `@FOO@`
/// literal in the wrapper they write to disk. So the canonical
/// template MUST NOT add a fourth placeholder — operator inputs
/// that need to vary per render (like the bcachefs-tools ref) are
/// handled by a post-render `rewrite_flake_input_urls` pass on
/// the caller side, NOT here.
fn render_system_flake_template_with_ref(
    template: &str,
    nasty_ref: &str,
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
    if !template.contains("@WRAPPER_FLAKE_VERSION@") {
        return Err(UpdateError::CommandFailed(
            "system flake template is missing @WRAPPER_FLAKE_VERSION@ placeholder".into(),
        ));
    }
    if nasty_ref.is_empty() {
        return Err(UpdateError::CommandFailed(
            "nasty ref must not be empty".into(),
        ));
    }
    let wrapper_version = wrapper_flake_content_hash(template);

    let rendered = template
        .replace("@NASTY_VERSION@", nasty_ref)
        .replace("@LOCAL_SYSTEM@", local_system)
        .replace("@WRAPPER_FLAKE_VERSION@", &wrapper_version);

    Ok(rendered)
}

/// Whether the wrapper is in the canonical 0.0.9 shape:
/// `nixpkgs.follows = "nasty/nixpkgs"` (no `.url`) AND
/// `bcachefs-tools.url = "..."` (no `.follows`) AND lanzaboote's
/// presence matches the SB overlay's presence (declared iff
/// `secure-boot.nix` exists on disk, since lanzaboote is engine-
/// injected per-box at enrollment).
///
/// Returns true when all invariants hold, OR when the content
/// fails to parse, OR when no inputs are declared at all. The two
/// "can't reason" cases are conservative on purpose: don't rewrite
/// a wrapper we can't make sense of, leave operator customizations
/// alone.
fn wrapper_is_canonical_shape(content: &str, overlay_present: bool) -> bool {
    let Ok(urls) = parse_flake_input_urls(content) else {
        return true;
    };
    // Empty (no inputs found at all) → can't reason about shape;
    // claim canonical so the migration skips it.
    if urls.is_empty() {
        return true;
    }
    // nixpkgs must have no .url (so it's either follows or missing).
    if urls.contains_key("nixpkgs") {
        return false;
    }
    // bcachefs-tools must have a .url.
    if !urls.contains_key("bcachefs-tools") {
        return false;
    }
    // Lanzaboote must be present iff the SB overlay is on disk.
    // Drift in either direction (overlay without input, or input
    // without overlay) is non-canonical and needs reconciliation
    // by `migrate_wrapper_to_canonical_shape`.
    let lanzaboote_present = urls.contains_key("lanzaboote");
    if overlay_present != lanzaboote_present {
        return false;
    }
    true
}

/// Re-render the wrapper-flake into the canonical 0.0.9 shape by
/// reusing the embedded template. Preserves the operator's chosen
/// `nasty` ref and `bcachefs-tools` ref. No-op when the wrapper is
/// already canonical.
///
/// `bcachefs-tools` ref resolution priority:
///   1. Existing `bcachefs-tools.url` in the wrapper (legacy /
///      pre-#304 wrappers — preserve the ref the operator had,
///      including any custom pin).
///   2. `nodes["bcachefs-tools"].original.ref` from
///      `/etc/nixos/flake.lock` (post-#308 follows-shape wrappers —
///      no `.url` in flake.nix, but Nix wrote the resolved ref on
///      the followed node).
///   3. Engine's embedded default (fresh / lockless boxes — falls
///      back to whatever nasty's bundled flake.nix declares for
///      this engine binary).
///
/// Caller is responsible for writing the result to disk and
/// triggering `nix flake lock` (or relying on the next
/// `nixos-rebuild`'s implicit lock refresh) to settle the new
/// resolution.
///
/// Fails when the wrapper's `nasty.url` is a non-canonical (fork)
/// URL we can't parse a ref out of — in that case the operator is
/// running an unusual setup we shouldn't second-guess; they can
/// migrate by hand.
async fn migrate_wrapper_to_canonical_shape(
    current_content: &str,
    local_system: &str,
    overlay_present: bool,
) -> Result<String, UpdateError> {
    if wrapper_is_canonical_shape(current_content, overlay_present) {
        return Ok(current_content.to_string());
    }
    let urls = parse_flake_input_urls(current_content)?;
    let nasty_url = &urls
        .get("nasty")
        .ok_or_else(|| UpdateError::CommandFailed("wrapper has no nasty.url to migrate".into()))?
        .url;
    let (nasty_ref, _is_tag) = parse_official_nasty_ref(nasty_url).ok_or_else(|| {
        UpdateError::CommandFailed(format!(
            "wrapper's nasty.url ({nasty_url}) is not a canonical \
             github:nasty-project/nasty/<ref> URL — refusing to migrate \
             automatically. Edit /etc/nixos/flake.nix by hand if you \
             want the canonical-shape inputs."
        ))
    })?;
    // Resolve the bcachefs ref to PRESERVE across migration. Three
    // sources, in priority:
    //   1. operator's existing `bcachefs-tools.url` (legacy / pre-#304
    //      wrappers that had their own pin),
    //   2. `flake.lock`'s `nodes["bcachefs-tools"].original.ref`
    //      (post-#308 follows-shape wrappers — Nix wrote the
    //      resolved ref on the followed node),
    //   3. embedded default (lock-less fresh installs).
    let bcachefs_ref = if let Some(input) = urls.get("bcachefs-tools") {
        input
            .url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                UpdateError::CommandFailed(format!(
                    "wrapper has bcachefs-tools.url ({}) but its \
                     trailing path segment is empty",
                    input.url
                ))
            })?
    } else {
        let (pinned_ref, _) = read_flake_lock_bcachefs().await;
        match pinned_ref {
            Some(r) => r,
            None => embedded_default_bcachefs_tools_ref()?,
        }
    };
    // Render is placeholder-free (template has a hardcoded bcachefs
    // default). Then rewrite the bcachefs URL to the operator's
    // preserved ref — same rewrite-after-render pattern as
    // upgrade_tagged_release and version_switch.
    let rendered =
        render_system_flake_template_with_ref(EMBEDDED_WRAPPER_TEMPLATE, &nasty_ref, local_system)?;
    let after_bcachefs = rewrite_flake_input_urls(
        &rendered,
        &HashMap::from([(
            String::from("bcachefs-tools"),
            format!("github:koverstreet/bcachefs-tools/{bcachefs_ref}"),
        )]),
    )?;
    // Lanzaboote reconciliation: the wrapper template no longer
    // declares lanzaboote unconditionally (per-box opt-in — engine
    // injects on enrollment, strips on abort). If the SB overlay is
    // present on this box, re-inject the input here so a re-render
    // doesn't strip the enrolled box's SB stack out from under it.
    // If the overlay is absent and a stale lanzaboote.url somehow
    // survives from an earlier wrapper shape, the freshly-rendered
    // template won't carry it forward (the template's inputs block
    // has no lanzaboote line to preserve). So the symmetric "strip
    // when no overlay" branch is implicit in the render itself.
    if overlay_present {
        inject_lanzaboote_input(&after_bcachefs)
    } else {
        Ok(after_bcachefs)
    }
}

/// Lanzaboote input lines as injected into the wrapper-flake
/// `inputs` block by `add_lanzaboote_input_to_wrapper`. Pinned to
/// `v1.0.0` here (same rev nasty's own flake.nix pins) so the lock
/// stays consistent across nasty/wrapper. The `.inputs.nixpkgs.follows`
/// keeps lanzaboote on nasty's pinned nixpkgs so cachix substitution
/// hits.
const LANZABOOTE_INPUT_BLOCK: &str = "\
    lanzaboote.url = \"github:nix-community/lanzaboote/v1.0.0\";\n\
    lanzaboote.inputs.nixpkgs.follows = \"nasty/nixpkgs\";\n";

/// Inject lanzaboote as a top-level flake input into the wrapper at
/// `wrapper_path`. Called by the SecureBoot enrollment ceremony when
/// the operator begins enrollment; pair with
/// `remove_lanzaboote_input_from_wrapper` on abort.
///
/// Idempotent: returns `Ok(())` with no change when lanzaboote is
/// already declared. Does NOT run `nix flake lock` — caller is
/// responsible (typically immediately after this, so the lanzaboote
/// sources actually get fetched).
pub async fn add_lanzaboote_input_to_wrapper(wrapper_path: &str) -> Result<(), UpdateError> {
    let content = tokio::fs::read_to_string(wrapper_path)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("read {wrapper_path}: {e}")))?;
    let urls = parse_flake_input_urls(&content)?;
    if urls.contains_key("lanzaboote") {
        return Ok(());
    }
    let mutated = inject_lanzaboote_input(&content)?;
    tokio::fs::write(wrapper_path, mutated)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("write {wrapper_path}: {e}")))
}

/// Strip lanzaboote from the wrapper at `wrapper_path`. Removes the
/// `lanzaboote.url` line and any `lanzaboote.inputs.*` lines.
/// Called by the SecureBoot enrollment ceremony on abort.
///
/// Idempotent: returns `Ok(())` with no change when lanzaboote is
/// already absent. Does NOT run `nix flake lock` — the dead lock
/// entries get pruned on the operator's next update; eager pruning
/// here would add a network requirement to abort, which we'd rather
/// keep best-effort.
pub async fn remove_lanzaboote_input_from_wrapper(wrapper_path: &str) -> Result<(), UpdateError> {
    let content = tokio::fs::read_to_string(wrapper_path)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("read {wrapper_path}: {e}")))?;
    let urls = parse_flake_input_urls(&content)?;
    if !urls.contains_key("lanzaboote") {
        return Ok(());
    }
    let mutated = strip_lanzaboote_input(&content);
    tokio::fs::write(wrapper_path, mutated)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("write {wrapper_path}: {e}")))
}

/// Locate the wrapper's `inputs = { ... };` attrset and splice the
/// two lanzaboote lines in just before the closing brace. Uses the
/// same rnix-based parsing as `parse_flake_input_urls` so the
/// insertion point is robust to whatever the operator did to the
/// inputs block (extra comments, reordering, etc.).
fn inject_lanzaboote_input(content: &str) -> Result<String, UpdateError> {
    let parsed = rnix::Root::parse(content);
    if !parsed.errors().is_empty() {
        let first = parsed.errors()[0].to_string();
        return Err(UpdateError::CommandFailed(format!(
            "failed to parse flake.nix: {first}"
        )));
    }
    let root = parsed.tree();
    // Find the top-level `inputs = { ... };` attrset's closing
    // brace position. The inputs attrset is the AttrSet value of an
    // AttrpathValue whose attrpath is literally "inputs".
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
        if normalized_path != "inputs" {
            continue;
        }
        let Some(value) = node.value() else { continue };
        // The value is the `{ ... }` AttrSet. We want to insert
        // immediately before the line that holds the closing `}`,
        // so the existing indentation of that closing-brace line
        // stays put and the new lanzaboote lines sit at the same
        // indent as their siblings inside the block. Inserting at
        // `end - 1` (the `}` byte itself) would pull the
        // closing-brace line's leading whitespace into our
        // injection and leave `};` un-indented.
        let range = value.syntax().text_range();
        let end = u32::from(range.end()) as usize;
        if content.as_bytes().get(end.wrapping_sub(1)) != Some(&b'}') {
            return Err(UpdateError::CommandFailed(
                "wrapper inputs block doesn't end in '}' as expected".into(),
            ));
        }
        // Walk backward from the `}` to find the newline that
        // starts the closing-brace line. If there's no newline
        // (single-line `inputs = { ... };`) we fall back to
        // inserting just before the `}` — produces ugly formatting
        // but stays correct.
        let insert_at = content[..end - 1]
            .rfind('\n')
            .map(|n| n + 1)
            .unwrap_or(end - 1);
        let mut out = String::with_capacity(content.len() + LANZABOOTE_INPUT_BLOCK.len());
        out.push_str(&content[..insert_at]);
        out.push_str(LANZABOOTE_INPUT_BLOCK);
        out.push_str(&content[insert_at..]);
        return Ok(out);
    }
    Err(UpdateError::CommandFailed(
        "wrapper has no top-level `inputs = { ... }` attrset to inject into".into(),
    ))
}

/// Strip every line that declares a lanzaboote input attribute
/// (`lanzaboote.url`, `lanzaboote.inputs.<…>`). Pure text op — keeps
/// surrounding whitespace and comments untouched.
fn strip_lanzaboote_input(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("lanzaboote.url") || trimmed.starts_with("lanzaboote.inputs.") {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Check whether the upstream wrapper-flake template at the given
/// canonical ref differs from what's baked into the local
/// /etc/nixos/flake.nix. Used by system.update.check to surface
/// template-only drift as "update available" — without this,
/// dev-build trackers whose locked rev already matches main HEAD
/// would never see the Upgrade button despite the template having
/// new bcachefs URLs (etc) to adopt.
///
/// Returns `Some(true)` for "drift detected", `Some(false)` for
/// "in sync", `None` on any error (network, parse, fork repos that
/// our placeholder substitution can't safely reason about). None is
/// treated as "don't change the answer" upstream of here.
async fn check_wrapper_template_drift(owner: &str, repo: &str, git_ref: &str) -> Option<bool> {
    // Only nasty-project/nasty templates carry the @WRAPPER_FLAKE_VERSION@
    // placeholder we know how to hash — refuse to reason about forks.
    if owner != DEFAULT_NASTY_OWNER || repo != DEFAULT_NASTY_REPO {
        return None;
    }
    let token = read_github_token().await;
    let template = fetch_github_text_file(
        token.as_deref(),
        owner,
        repo,
        SYSTEM_FLAKE_TEMPLATE_PATH,
        git_ref,
    )
    .await
    .ok()?;
    // Pre-content-hash templates (no placeholder) → no drift signal
    // available, defer to the rev-based check alone.
    if !template.contains("@WRAPPER_FLAKE_VERSION@") {
        return None;
    }
    let expected = wrapper_flake_content_hash(&template);
    let flake_path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
    let local_flake = tokio::fs::read_to_string(&flake_path).await.ok()?;
    let local = read_wrapper_flake_version(&local_flake).ok().flatten();
    Some(local.as_deref() != Some(expected.as_str()))
}

/// Content-hash of the template body, used as wrapperFlakeVersion.
///
/// The line carrying `@WRAPPER_FLAKE_VERSION@` is excluded from the
/// hash — otherwise the hash would depend on itself, which would
/// always change after substitution. Excluded too: the `@NASTY_VERSION@`
/// and `@LOCAL_SYSTEM@` placeholders are NOT stripped because they get
/// substituted *consistently* on both upstream and local renders
/// (so two installs of the same release tag hash to the same value;
/// installs of *different* tags get different hashes, which correctly
/// triggers rebootstrap when the user upgrades).
///
/// The hash output is shortened to 16 hex chars and prefixed with
/// `sha256-` for human readability — 64 bits of identifier is plenty
/// for change detection.
fn wrapper_flake_content_hash(template: &str) -> String {
    use sha2::{Digest, Sha256};
    let body: String = template
        .lines()
        .filter(|line| !line.contains("@WRAPPER_FLAKE_VERSION@"))
        .collect::<Vec<_>>()
        .join("\n");
    let digest = Sha256::digest(body.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("sha256-{hex}")
}

/// Compare the local flake's stored wrapperFlakeVersion against the
/// hash the upstream template *would render to*. Any difference (or
/// missing local value) triggers a rebootstrap. A malformed upstream
/// template skips rebootstrap — we'd rather preserve the user's
/// working flake than re-render from garbage.
fn should_rebootstrap_wrapper_flake(
    local_flake: &str,
    upstream_template: &str,
) -> Result<bool, UpdateError> {
    if !upstream_template.contains("@WRAPPER_FLAKE_VERSION@") {
        // Old-style template with no placeholder — pre-content-hash
        // releases. Skip rebootstrap; their wrapperFlakeVersion is a
        // literal semver we no longer know how to compare cleanly.
        return Ok(false);
    }
    let expected = wrapper_flake_content_hash(upstream_template);
    let local = read_wrapper_flake_version(local_flake)?;
    Ok(local.as_deref() != Some(expected.as_str()))
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
    let mut req = github_http_client()?
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

/// Fetch the bcachefs-tools ref pinned by nasty at the given canonical
/// release ref. Reads the project's `flake.lock` from GitHub at that
/// ref and pulls `nodes["bcachefs-tools"].original.ref` out of it
/// (the tag string the maintainer pinned, e.g. `v1.38.3`).
///
/// Returns the engine's embedded default on any failure — network,
/// parse, missing node — because switching to a release shouldn't
/// fail just because the bcachefs-rev lookup hit a snag. The embedded
/// default is the rev THIS engine binary's nasty pinned at build
/// time, which is usually close enough to what the target release
/// pins (most release-to-release bcachefs changes are small or none).
/// Warns to the journal when falling back so a degraded result is
/// visible at debug time.
async fn fetch_release_bcachefs_ref(
    token: Option<&str>,
    owner: &str,
    repo: &str,
    release_ref: &str,
) -> Result<String, UpdateError> {
    let fallback = || embedded_default_bcachefs_tools_ref();
    let content = match fetch_github_text_file(token, owner, repo, "flake.lock", release_ref).await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "couldn't fetch flake.lock at {release_ref} ({e}); \
                 using engine's embedded default bcachefs-tools ref"
            );
            return fallback();
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "flake.lock at {release_ref} didn't parse ({e}); \
                 using engine's embedded default bcachefs-tools ref"
            );
            return fallback();
        }
    };
    match v["nodes"]["bcachefs-tools"]["original"]["ref"].as_str() {
        Some(r) if !r.is_empty() => Ok(r.to_string()),
        _ => {
            warn!(
                "flake.lock at {release_ref} has no \
                 nodes[bcachefs-tools].original.ref; using engine's \
                 embedded default bcachefs-tools ref"
            );
            fallback()
        }
    }
}

fn github_http_client() -> Result<reqwest::Client, UpdateError> {
    reqwest::Client::builder()
        .timeout(GITHUB_FETCH_TIMEOUT)
        .build()
        .map_err(|e| UpdateError::CommandFailed(format!("failed to build GitHub HTTP client: {e}")))
}

/// Check latest commit on a branch via GitHub API.
async fn check_via_github_api_branch(
    owner: &str,
    repo: &str,
    branch: &str,
) -> Result<String, UpdateError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{branch}");
    let body: serde_json::Value = github_http_client()?
        .get(&url)
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

/// The git commit this engine binary was built from, baked in at
/// build time by `engine/nasty-system/build.rs`. `None` when the
/// build environment couldn't determine the SHA (no Nix-set env var
/// AND no usable git checkout — extremely unusual but possible for
/// dev-loop builds in weird sandboxes).
const ENGINE_BUILD_REV: Option<&str> = option_env!("NASTY_GIT_SHA");

async fn read_current_version() -> String {
    // The engine binary's own embedded build commit is the ground
    // truth: this binary literally was built from that commit, so
    // by definition that's what's running. No proxy chain to drift
    // or lag — in particular, no waiting on `/run/booted-system` to
    // refresh after a reboot (which used to be the top priority
    // here and caused the "Upgrade keeps being available even after
    // I clicked it" loop: the operator activates a new generation,
    // engine restarts on the new closure, but `/run/booted-system`
    // still points at the previously-booted closure until reboot →
    // engine reports the OLD rev as "current" → check() says
    // upgrade available → forever).
    //
    // The fallback chain below is kept for boxes whose engine was
    // built without the SHA bake (dev-loop cargo builds in weird
    // sandboxes, or pre-fix engine binaries doing a self-update).
    // None of the fallback sources is as good as the bake — they
    // all drift in one direction or another — but they're better
    // than "dev" sentinel.
    if let Some(rev) = ENGINE_BUILD_REV {
        let trimmed = rev.trim();
        if !trimmed.is_empty() && trimmed != "unknown" {
            return trimmed[..7.min(trimmed.len())].to_string();
        }
    }
    if let Some(rev) = read_booted_nasty_rev().await {
        return rev;
    }
    if let Ok(s) = tokio::fs::read_to_string(VERSION_PATH).await {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(s) = tokio::fs::read_to_string(VERSION_PATH_FALLBACK).await {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    if let Some(version) = read_locked_nasty_version().await {
        return version;
    }
    "dev".to_string()
}

/// Read the locked `nasty` input rev from the wrapper-flake
/// snapshot baked into the currently-booted NixOS generation.
/// Returns the short (7-char) SHA, or None if the snapshot path
/// doesn't exist (older systems pre-`recover-generation-flake`).
async fn read_booted_nasty_rev() -> Option<String> {
    let lock_path = "/run/booted-system/etc/nasty-system-flake/flake.lock";
    let content = tokio::fs::read_to_string(lock_path).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let rev = v["nodes"]["nasty"]["locked"]["rev"].as_str()?;
    Some(rev[..rev.len().min(7)].to_string())
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
        if let (Ok(b), Ok(c)) = (booted, current)
            && b != c
        {
            return true;
        }
    }
    false
}

/// TODO: Remove once repo is public.
async fn read_github_token() -> Option<String> {
    None
}

/// Build the full flake reference for nixos-rebuild.
fn local_flake() -> &'static str {
    LOCAL_FLAKE_TARGET
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

/// Parse every top-level `<input>.url = "..."` assignment out of a
/// flake.nix-style file. Name-agnostic: returns all URL declarations
/// (nasty, nixpkgs, bcachefs-tools, forks, whatever) so callers can
/// decide which ones they care about. Pure parser — no presence
/// invariants imposed here; the wrapper-specific async wrapper
/// `read_flake_input_urls` enforces `nasty.url` must exist on the
/// operator's `/etc/nixos/flake.nix`.
fn parse_flake_input_urls(content: &str) -> Result<HashMap<String, ParsedFlakeInput>, UpdateError> {
    let parsed = rnix::Root::parse(content);
    if !parsed.errors().is_empty() {
        let first = parsed.errors()[0].to_string();
        return Err(UpdateError::CommandFailed(format!(
            "failed to parse flake.nix: {first}"
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
        // Match exactly `<name>.url` where <name> is a single attribute
        // segment (no nested paths like `nasty.inputs.nixpkgs.follows`).
        // Splitting on '.' and requiring exactly two parts ending in
        // "url" enforces the top-level shape we care about.
        let parts: Vec<&str> = normalized_path.split('.').collect();
        if parts.len() != 2 || parts[1] != "url" {
            continue;
        }
        let name = parts[0].to_string();
        let Some(value) = node.value() else { continue };
        let raw_value = value.syntax().text().to_string();
        let Some(url) = unquote_nix_string(&raw_value) else {
            continue;
        };
        let range = value.syntax().text_range();
        urls.insert(
            name,
            ParsedFlakeInput {
                url,
                value_start: u32::from(range.start()) as usize,
                value_end: u32::from(range.end()) as usize,
            },
        );
    }

    Ok(urls)
}

/// Extract the literal string value of `wrapperFlakeVersion` from a
/// rendered flake.nix. Returns `None` on parse errors or when the
/// attribute is missing — both cases get treated as "needs rebootstrap"
/// upstream of here.
fn read_wrapper_flake_version(content: &str) -> Result<Option<String>, UpdateError> {
    let parsed = rnix::Root::parse(content);
    if !parsed.errors().is_empty() {
        return Ok(None);
    }

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
        if normalized_path != "wrapperFlakeVersion" {
            continue;
        }
        let Some(value) = node.value() else { continue };
        let raw_value = value.syntax().text().to_string();
        return Ok(unquote_nix_string(&raw_value));
    }

    Ok(None)
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

    edits.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut rewritten = content.to_string();
    for (start, end, replacement) in edits {
        rewritten.replace_range(start..end, &replacement);
    }
    Ok(rewritten)
}

fn unquote_nix_string(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw).ok()
}

/// What the most recent invocation of the upgrade unit ended in.
///
/// Returns `Some("success")` or `Some("failed")` when the unit has
/// completed, `None` while it's still running or if it has never been
/// invoked. The signal is taken from systemd's own bookkeeping — both
/// `ActiveState` (so we know the unit is at rest) and `Result` (so we
/// distinguish a clean exit from a crash/timeout/oom).
///
/// Used by [`UpdateService::check`] to keep the Upgrade button visible
/// after a half-applied upgrade, where the version comparison alone
/// would say "up to date" because `nix flake update` updates the lock
/// BEFORE the rebuild runs.
async fn last_upgrade_attempt_result() -> Option<String> {
    let out = tokio::process::Command::new("systemctl")
        .args(["show", UPDATE_UNIT, "--property=ActiveState,Result"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut active = "";
    let mut result = "";
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            active = v.trim();
        }
        if let Some(v) = line.strip_prefix("Result=") {
            result = v.trim();
        }
    }
    Some(classify_last_upgrade_attempt(active, result)?.to_string())
}

/// Pure classifier extracted so `last_upgrade_attempt_result` can be
/// unit-tested without a live systemd. `ActiveState=active|activating`
/// means we shouldn't draw any conclusion yet (still running). An
/// empty `Result` paired with `inactive` means the unit was never
/// invoked. Anything else collapses into `success` vs `failed`.
fn classify_last_upgrade_attempt(active: &str, result: &str) -> Option<&'static str> {
    match (active, result) {
        ("active" | "activating" | "reloading", _) => None,
        (_, "") => None,
        (_, "success") => Some("success"),
        _ => Some("failed"),
    }
}

/// Classify the systemd unit state into one of the four states the WebUI
/// understands: `running`, `idle`, `success`, `failed`. Pure: takes the raw
/// `ActiveState` and `Result` properties from `systemctl show`.
fn map_systemd_state(active_state: &str, result: &str) -> &'static str {
    match active_state {
        "active" | "activating" | "reloading" => "running",
        "inactive" | "deactivating" if result == "success" => "success",
        // Unit never ran or was cleaned up.
        "inactive" | "deactivating" => "idle",
        "failed" => "failed",
        _ => "idle",
    }
}

/// Whether a generation's store path matches the currently-booted system.
/// Used to (a) flag generations in the listing UI and (b) refuse to delete
/// the booted one. A missing path on either side counts as "not the booted
/// generation" — the safe default.
fn is_booted_generation(
    booted_path: Option<&std::path::Path>,
    gen_path: Option<&std::path::Path>,
) -> bool {
    match (booted_path, gen_path) {
        (Some(b), Some(g)) => b == g,
        _ => false,
    }
}

async fn read_flake_input_urls() -> Result<HashMap<String, String>, UpdateError> {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.nix");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| UpdateError::CommandFailed(format!("read {path}: {e}")))?;
    let urls = parse_flake_input_urls(&content)?;
    // `nasty.url` is the one input the wrapper *must* own — it's what
    // pins the release the operator is tracking. Other inputs may be
    // declared as `<input>.follows = "nasty/<input>"` (no `.url`),
    // which is fine and expected for nixpkgs in canonical 0.0.9
    // shape. Callers handle that explicitly by checking the returned
    // map for presence of each name they care about.
    if !urls.contains_key("nasty") {
        return Err(UpdateError::CommandFailed(format!(
            "missing nasty.url in {NIXOS_FLAKE_DIR}/flake.nix"
        )));
    }
    Ok(urls
        .into_iter()
        .map(|(name, parsed)| (name, parsed.url))
        .collect())
}

/// Per-input lock-file snapshot for the editable inputs: the
/// 12-char rev SHA (always present when the node is locked) and the
/// human-meaningful `original.ref` (a tag like `v1.38.3` or a branch
/// name like `main`) when the node carries one. Returns the empty
/// map on read / parse failure so callers degrade gracefully.
fn read_flake_lock_entries(content: &str) -> HashMap<String, FlakeLockEntry> {
    let v: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut entries = HashMap::new();
    for name in VERSION_INPUT_NAMES {
        let node = &v["nodes"][name];
        let rev = node["locked"]["rev"]
            .as_str()
            .map(|r| r[..r.len().min(12)].to_string());
        let tag = node["original"]["ref"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if rev.is_some() || tag.is_some() {
            entries.insert(name.to_string(), FlakeLockEntry { rev, tag });
        }
    }
    entries
}

#[derive(Debug, Clone, Default)]
struct FlakeLockEntry {
    rev: Option<String>,
    tag: Option<String>,
}

async fn read_flake_lock_entries_async() -> HashMap<String, FlakeLockEntry> {
    let path = format!("{NIXOS_FLAKE_DIR}/flake.lock");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    read_flake_lock_entries(&content)
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

/// Extract the git ref from a canonical `github:nasty-project/nasty/<ref>`
/// URL, returning `(ref, is_release_tag)`. Forks and non-canonical URLs
/// return None. Branch refs (e.g. `main`) return `(ref, false)` so
/// callers can distinguish "user is pinned to a release" from "user is
/// tracking a development branch" — the rebootstrap policy differs.
fn parse_official_nasty_ref(url: &str) -> Option<(String, bool)> {
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
    if git_ref.is_empty() {
        return None;
    }
    let is_tag = parse_release_tag_version(git_ref).is_some();
    Some((git_ref.to_string(), is_tag))
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
        is_booted_generation, map_systemd_state, normalize_git_tag_ref, parse_flake_input_urls,
        parse_official_nasty_ref, parse_release_tag_version, read_wrapper_flake_version,
        rewrite_flake_input_urls, should_rebootstrap_wrapper_flake, unquote_nix_string,
        wrapper_flake_content_hash,
    };
    use std::collections::HashMap;
    use std::path::Path;

    #[test]
    fn normalizes_annotated_git_tag_refs() {
        assert_eq!(normalize_git_tag_ref("refs/tags/v0.0.3^{}"), "v0.0.3");
        assert_eq!(normalize_git_tag_ref("refs/tags/v0.0.3"), "v0.0.3");
    }

    #[test]
    fn parses_official_canonical_refs_with_tag_vs_branch_distinction() {
        // Release tag → (ref, true).
        assert_eq!(
            parse_official_nasty_ref("github:nasty-project/nasty/v0.0.2"),
            Some(("v0.0.2".to_string(), true))
        );
        // Branch ref → (ref, false). Distinguishing main from v0.0.X
        // matters because the rebootstrap path forks on it: tag-trackers
        // preserve request URLs, branch-trackers adopt the template's.
        assert_eq!(
            parse_official_nasty_ref("github:nasty-project/nasty/main"),
            Some(("main".to_string(), false))
        );
        // Non-canonical owner — refuse so we never fetch a template
        // from a random fork.
        assert_eq!(
            parse_official_nasty_ref("github:someone-else/nasty/v0.0.2"),
            None
        );
        // Pre-release tag isn't a semver match, so is_tag = false.
        assert_eq!(
            parse_official_nasty_ref("github:nasty-project/nasty/v0.0.2-rc1"),
            Some(("v0.0.2-rc1".to_string(), false))
        );
        // Empty ref → None.
        assert_eq!(
            parse_official_nasty_ref("github:nasty-project/nasty/"),
            None
        );
        // Path with extra slashes → None.
        assert_eq!(
            parse_official_nasty_ref("github:nasty-project/nasty/main/extra"),
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
    fn reads_wrapper_flake_version_returns_raw_string() {
        let flake = r#"
{
  outputs = { self, nixpkgs, nasty, ... }: {
    wrapperFlakeVersion = "sha256-abc123";
  };
}
"#;
        assert_eq!(
            read_wrapper_flake_version(flake).expect("parsed"),
            Some("sha256-abc123".to_string())
        );
    }

    #[test]
    fn ignores_unparseable_wrapper_flake_source() {
        let broken_flake = r#"
{
  outputs = { self, nixpkgs, nasty, ... }: {
    wrapperFlakeVersion = "v0.1"
"#;
        assert_eq!(
            read_wrapper_flake_version(broken_flake).expect("graceful fallback"),
            None
        );
    }

    #[test]
    fn content_hash_excludes_placeholder_line_only() {
        // wrapper_flake_content_hash is meant to be called on the
        // upstream template (with the placeholder). It excludes the
        // placeholder-bearing line so the hash doesn't depend on
        // itself after substitution.
        let with_placeholder = "\
inputs = { foo = 1; };
wrapperFlakeVersion = \"@WRAPPER_FLAKE_VERSION@\";
body = true;
";
        let without_that_line = "\
inputs = { foo = 1; };
body = true;
";
        assert_eq!(
            wrapper_flake_content_hash(with_placeholder),
            wrapper_flake_content_hash(without_that_line)
        );

        // A body change must produce a different hash so re-renders
        // trigger on template content drift.
        let body_changed = "\
inputs = { foo = 2; };
wrapperFlakeVersion = \"@WRAPPER_FLAKE_VERSION@\";
body = true;
";
        assert_ne!(
            wrapper_flake_content_hash(with_placeholder),
            wrapper_flake_content_hash(body_changed)
        );
    }

    #[test]
    fn rebootstrap_when_local_hash_differs_from_upstream() {
        let upstream = r#"
{
  outputs = { self, nixpkgs, nasty, ... }: {
    foo = 1;
    wrapperFlakeVersion = "@WRAPPER_FLAKE_VERSION@";
  };
}
"#;
        let expected = wrapper_flake_content_hash(upstream);
        let local_matching = format!(
            r#"
{{
  outputs = {{ self, nixpkgs, nasty, ... }}: {{
    foo = 1;
    wrapperFlakeVersion = "{expected}";
  }};
}}
"#
        );
        let local_stale = r#"
{
  outputs = { self, nixpkgs, nasty, ... }: {
    foo = 1;
    wrapperFlakeVersion = "sha256-deadbeef";
  };
}
"#;
        let local_missing = r#"
{
  outputs = { self, nixpkgs, nasty, ... }: {
    foo = 1;
  };
}
"#;

        // Local matches what the upstream renders to → no rebootstrap.
        assert!(!should_rebootstrap_wrapper_flake(&local_matching, upstream).expect("comparison"));
        // Local has stale hash (e.g. older template) → rebootstrap.
        assert!(should_rebootstrap_wrapper_flake(local_stale, upstream).expect("comparison"));
        // Local missing wrapperFlakeVersion entirely → rebootstrap.
        assert!(should_rebootstrap_wrapper_flake(local_missing, upstream).expect("comparison"));
        // Upstream without the placeholder is treated as "pre-content-hash"
        // (older release tag) and skips rebootstrap rather than guessing.
        let legacy_upstream = r#"
{
  outputs = { self, ... }: {
    foo = 1;
  };
}
"#;
        assert!(
            !should_rebootstrap_wrapper_flake(&local_matching, legacy_upstream)
                .expect("legacy upstream skips rebootstrap")
        );
    }

    #[test]
    fn renders_system_flake_template() {
        let template = r#"
inputs = {
  nasty.url = "github:nasty-project/nasty/@NASTY_VERSION@";
  bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
};
wrapperFlakeVersion = "@WRAPPER_FLAKE_VERSION@";
"#
        .to_string()
            + r#"
outputs = { nixpkgs, nasty, ... }: {
  nixosConfigurations.nasty = nixpkgs.lib.nixosSystem { system = "@LOCAL_SYSTEM@"; };
};
"#;
        let rendered = super::render_system_flake_template(&template, "0.0.3", "x86_64-linux")
            .expect("rendered");
        assert!(rendered.contains("github:nasty-project/nasty/v0.0.3"));
        // bcachefs-tools URL is hardcoded in the template (no
        // placeholder) — passes through unchanged.
        assert!(rendered.contains("github:koverstreet/bcachefs-tools/v1.38.3"));
        assert!(rendered.contains("\"x86_64-linux\""));
        // The hash placeholder must be substituted with a content hash.
        assert!(!rendered.contains("@WRAPPER_FLAKE_VERSION@"));
        assert!(rendered.contains("wrapperFlakeVersion = \"sha256-"));
        // Rendering twice yields the same hash (deterministic).
        let rendered2 = super::render_system_flake_template(&template, "0.0.3", "x86_64-linux")
            .expect("rendered");
        assert_eq!(rendered, rendered2);
    }

    #[test]
    fn render_fails_without_wrapper_placeholder() {
        let template = "inputs = {\n  nasty.url = \"github:nasty-project/nasty/@NASTY_VERSION@\";\n  bcachefs-tools.url = \"github:koverstreet/bcachefs-tools/v1.38.3\";\n};\nsystem = \"@LOCAL_SYSTEM@\";\n";
        let err = super::render_system_flake_template(template, "0.0.3", "x86_64-linux")
            .expect_err("placeholder enforcement");
        assert!(format!("{err:?}").contains("@WRAPPER_FLAKE_VERSION@"));
    }

    // ── systemd state mapping ──────────────────────────────────────

    #[test]
    fn map_systemd_state_running_active_states() {
        for active in ["active", "activating", "reloading"] {
            assert_eq!(map_systemd_state(active, ""), "running", "{active}");
        }
    }

    #[test]
    fn map_systemd_state_inactive_with_success_is_success() {
        assert_eq!(map_systemd_state("inactive", "success"), "success");
        assert_eq!(map_systemd_state("deactivating", "success"), "success");
    }

    #[test]
    fn map_systemd_state_inactive_without_success_is_idle() {
        // Unit never ran or was cleaned up — not a failure, just nothing to report.
        assert_eq!(map_systemd_state("inactive", ""), "idle");
        assert_eq!(map_systemd_state("inactive", "exit-code"), "idle");
    }

    #[test]
    fn map_systemd_state_failed_is_failed() {
        assert_eq!(map_systemd_state("failed", "exit-code"), "failed");
        assert_eq!(map_systemd_state("failed", ""), "failed");
    }

    #[test]
    fn map_systemd_state_unknown_states_default_to_idle() {
        assert_eq!(map_systemd_state("", ""), "idle");
        assert_eq!(map_systemd_state("maintenance", ""), "idle");
    }

    // ── last-upgrade-attempt classifier ───────────────────────────

    #[test]
    fn classify_last_upgrade_returns_none_while_running() {
        use super::classify_last_upgrade_attempt;
        for active in ["active", "activating", "reloading"] {
            assert!(classify_last_upgrade_attempt(active, "").is_none());
            assert!(classify_last_upgrade_attempt(active, "success").is_none());
        }
    }

    #[test]
    fn classify_last_upgrade_returns_none_when_never_invoked() {
        use super::classify_last_upgrade_attempt;
        // systemd reports ActiveState=inactive with an empty Result for a
        // unit that has never been started. Don't claim "succeeded" in
        // that state — the absence of evidence isn't evidence of success.
        assert!(classify_last_upgrade_attempt("inactive", "").is_none());
    }

    #[test]
    fn classify_last_upgrade_returns_success_on_clean_exit() {
        use super::classify_last_upgrade_attempt;
        assert_eq!(
            classify_last_upgrade_attempt("inactive", "success"),
            Some("success")
        );
        assert_eq!(
            classify_last_upgrade_attempt("deactivating", "success"),
            Some("success")
        );
    }

    #[test]
    fn classify_last_upgrade_returns_failed_for_any_nonsuccess_completion() {
        use super::classify_last_upgrade_attempt;
        // exit-code (rebuild returned non-zero), oom-kill, timeout — all
        // surface as "failed" so the WebUI keeps offering Retry. The
        // precise reason already lives in the journal-fed status log.
        assert_eq!(
            classify_last_upgrade_attempt("failed", "exit-code"),
            Some("failed")
        );
        assert_eq!(
            classify_last_upgrade_attempt("inactive", "oom-kill"),
            Some("failed")
        );
        assert_eq!(
            classify_last_upgrade_attempt("failed", "timeout"),
            Some("failed")
        );
    }

    // ── wrapper-shape migration (→ canonical 0.0.9 shape) ──────────

    #[test]
    fn wrapper_is_canonical_shape_accepts_canonical() {
        // The 0.0.9 canonical shape: nasty.url, nixpkgs.follows,
        // bcachefs-tools.url. nixpkgs must NOT have a .url; bcachefs
        // MUST have one. No SB overlay on disk → no lanzaboote.
        let canonical = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        assert!(super::wrapper_is_canonical_shape(canonical, false));
    }

    #[test]
    fn wrapper_is_canonical_shape_accepts_canonical_with_lanzaboote_when_overlay_present() {
        // Enrolled box: SB overlay on disk AND lanzaboote injected
        // into the wrapper inputs. Both halves match → canonical.
        let enrolled = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
    lanzaboote.url = "github:nix-community/lanzaboote/v1.0.0";
    lanzaboote.inputs.nixpkgs.follows = "nasty/nixpkgs";
  };
}"#;
        assert!(super::wrapper_is_canonical_shape(enrolled, true));
    }

    #[test]
    fn wrapper_is_canonical_shape_rejects_lanzaboote_without_overlay() {
        // Lanzaboote in the wrapper but no SB overlay → drift.
        // Migration should strip lanzaboote (the re-render won't
        // carry it forward).
        let drift = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
    lanzaboote.url = "github:nix-community/lanzaboote/v1.0.0";
  };
}"#;
        assert!(!super::wrapper_is_canonical_shape(drift, false));
    }

    #[test]
    fn wrapper_is_canonical_shape_rejects_overlay_without_lanzaboote() {
        // SB overlay on disk but no lanzaboote input → drift.
        // Migration should re-inject lanzaboote.
        let drift = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        assert!(!super::wrapper_is_canonical_shape(drift, true));
    }

    #[test]
    fn wrapper_is_canonical_shape_rejects_legacy_own_nixpkgs() {
        // Pre-#304 wrapper: nixpkgs has its own .url. Needs migration
        // to follows-shape for cachix-coverage reasons.
        let legacy = r#"{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
    nasty.url = "github:nasty-project/nasty/main";
  };
}"#;
        assert!(!super::wrapper_is_canonical_shape(legacy, false));
    }

    #[test]
    fn wrapper_is_canonical_shape_rejects_post_308_follows_for_bcachefs() {
        // Post-#308 wrapper: BOTH inputs were follows. bcachefs needs
        // to come back to .url so the operator can independently pin
        // a known-good rev across nasty bumps.
        let follows_both = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.follows = "nasty/bcachefs-tools";
  };
}"#;
        assert!(!super::wrapper_is_canonical_shape(follows_both, false));
    }

    #[test]
    fn wrapper_is_canonical_shape_returns_true_for_unparseable_content() {
        // Garbage in → don't claim "needs migration" and accidentally
        // rewrite whatever the operator has there. Safer default.
        assert!(super::wrapper_is_canonical_shape(
            "not a flake at all",
            false
        ));
    }

    // ── lanzaboote injection / stripping (per-box opt-in SB) ────────

    #[test]
    fn inject_lanzaboote_input_adds_both_lines_before_closing_brace() {
        let wrapper = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        let out = super::inject_lanzaboote_input(wrapper).expect("inject");
        assert!(out.contains("lanzaboote.url = \"github:nix-community/lanzaboote/v1.0.0\""));
        assert!(out.contains("lanzaboote.inputs.nixpkgs.follows = \"nasty/nixpkgs\""));
        // Pre-existing inputs must still be there.
        assert!(out.contains("nasty.url = \"github:nasty-project/nasty/v0.0.9\""));
        assert!(out.contains("bcachefs-tools.url ="));
    }

    #[test]
    fn strip_lanzaboote_input_removes_url_and_follows_lines() {
        let wrapper = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
    lanzaboote.url = "github:nix-community/lanzaboote/v1.0.0";
    lanzaboote.inputs.nixpkgs.follows = "nasty/nixpkgs";
  };
}"#;
        let out = super::strip_lanzaboote_input(wrapper);
        assert!(!out.contains("lanzaboote.url"));
        assert!(!out.contains("lanzaboote.inputs"));
        assert!(out.contains("nasty.url"));
        assert!(out.contains("bcachefs-tools.url"));
    }

    #[test]
    fn strip_lanzaboote_input_is_idempotent_on_clean_wrapper() {
        let wrapper = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        assert_eq!(super::strip_lanzaboote_input(wrapper), wrapper);
    }

    #[test]
    fn inject_then_strip_lanzaboote_returns_original_content() {
        let wrapper = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        let injected = super::inject_lanzaboote_input(wrapper).expect("inject");
        let round_tripped = super::strip_lanzaboote_input(&injected);
        assert_eq!(round_tripped, wrapper);
    }

    #[test]
    fn inject_lanzaboote_input_preserves_canonical_shape_when_overlay_present() {
        // After injection the wrapper plus an overlay-present claim
        // should reach canonical. The previous canonical shape (no
        // lanzaboote) is non-canonical under overlay-present.
        let pre = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        assert!(!super::wrapper_is_canonical_shape(pre, true));
        let injected = super::inject_lanzaboote_input(pre).expect("inject");
        assert!(super::wrapper_is_canonical_shape(&injected, true));
    }

    #[tokio::test]
    async fn migrate_wrapper_preserves_legacy_bcachefs_ref() {
        // Legacy/pre-#304 wrapper that had its OWN bcachefs.url pinned
        // to a specific rev — possibly the operator's preferred
        // version, possibly just the original install default. Either
        // way, the migration must NOT silently swap to nasty's
        // bundled default; preserve what the operator had.
        let legacy = r#"{
  description = "NASty local system configuration";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.37.2";
    nasty.url = "github:nasty-project/nasty/main";
    nasty.inputs.nixpkgs.follows = "nixpkgs";
  };
  outputs = { ... }: {};
}"#;
        let migrated = super::migrate_wrapper_to_canonical_shape(legacy, "x86_64-linux", false)
            .await
            .expect("migration succeeds");
        // The operator's chosen nasty ref survives:
        assert!(migrated.contains("github:nasty-project/nasty/main"));
        // The legacy nixpkgs.url is gone (replaced by follows) but the
        // legacy bcachefs.url ref is preserved exactly.
        let parsed = super::parse_flake_input_urls(&migrated).expect("migrated output parses");
        assert!(
            !parsed.contains_key("nixpkgs"),
            "migrated wrapper must not own nixpkgs.url"
        );
        let bcachefs = parsed
            .get("bcachefs-tools")
            .expect("migrated wrapper must declare bcachefs-tools.url");
        assert_eq!(
            bcachefs.url, "github:koverstreet/bcachefs-tools/v1.37.2",
            "operator's bcachefs pin must survive the migration"
        );
        // Follows is in place for nixpkgs:
        assert!(migrated.contains(r#"nixpkgs.follows = "nasty/nixpkgs""#));
    }

    #[tokio::test]
    async fn migrate_wrapper_is_a_noop_for_canonical_shape() {
        let canonical = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        let out = super::migrate_wrapper_to_canonical_shape(canonical, "x86_64-linux", false)
            .await
            .expect("noop succeeds");
        assert_eq!(out, canonical, "canonical wrapper passes through unchanged");
    }

    #[tokio::test]
    async fn migrate_wrapper_reinjects_lanzaboote_when_overlay_present() {
        // Re-render path with the SB overlay on disk: the freshly-
        // rendered template (no lanzaboote) gets the lanzaboote
        // input re-injected so an enrolled box doesn't lose its SB
        // stack on a wrapper-shape migration.
        let bare = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
  };
}"#;
        let out = super::migrate_wrapper_to_canonical_shape(bare, "x86_64-linux", true)
            .await
            .expect("migration with overlay present succeeds");
        let parsed = super::parse_flake_input_urls(&out).expect("output parses");
        assert!(
            parsed.contains_key("lanzaboote"),
            "overlay-present box must end up with lanzaboote declared after migration"
        );
    }

    #[tokio::test]
    async fn migrate_wrapper_strips_lanzaboote_when_overlay_absent() {
        // Drift case: wrapper has lanzaboote but no SB overlay on
        // disk. Re-render drops lanzaboote (the template doesn't
        // carry it forward), and with overlay_present=false the
        // migrator doesn't re-inject. Net effect: dead lanzaboote
        // gets cleaned up.
        let drift = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
    lanzaboote.url = "github:nix-community/lanzaboote/v1.0.0";
  };
}"#;
        let out = super::migrate_wrapper_to_canonical_shape(drift, "x86_64-linux", false)
            .await
            .expect("migration without overlay succeeds");
        let parsed = super::parse_flake_input_urls(&out).expect("output parses");
        assert!(
            !parsed.contains_key("lanzaboote"),
            "no-overlay box must end up without a lanzaboote declaration after migration"
        );
    }

    #[tokio::test]
    async fn migrate_wrapper_refuses_non_canonical_nasty_url() {
        // Fork URLs (or anything that isn't github:nasty-project/nasty)
        // get a clear error instead of being silently rewritten —
        // operators running forks know what they're doing and can
        // migrate by hand.
        let fork = r#"{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.3";
    nasty.url = "github:my-fork/nasty/main";
  };
}"#;
        let err = super::migrate_wrapper_to_canonical_shape(fork, "x86_64-linux", false)
            .await
            .expect_err("fork URL should refuse migration");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("my-fork") || msg.contains("canonical"),
            "got: {msg}"
        );
    }

    #[test]
    fn embedded_default_bcachefs_tools_ref_parses_from_nasty_flake() {
        // The engine's embedded copy of nasty's flake.nix MUST declare
        // a parseable `bcachefs-tools.url`. If a maintainer ever
        // accidentally converts it to a follows declaration, this
        // test fires before binaries ship.
        let r = super::embedded_default_bcachefs_tools_ref()
            .expect("embedded default ref extraction works");
        assert!(
            r.starts_with('v') && r.contains('.'),
            "embedded default ref should look like a semver tag, got: {r}"
        );
    }

    // ── build-dir spillover ────────────────────────────────────────

    #[test]
    fn parse_bcachefs_mounts_filters_to_fs_prefixed_bcachefs() {
        // Realistic /proc/mounts excerpt with mixed fstypes. Only the
        // two bcachefs rows under /fs/ should survive — the bcachefs
        // mounted at /mnt/external is filtered out (we don't want to
        // suggest random mountpoints as spillover targets), and the
        // ext4/tmpfs/proc rows are noise.
        let mounts = "\
/dev/sda2 / ext4 rw,relatime 0 0\n\
tmpfs /tmp tmpfs rw 0 0\n\
proc /proc proc rw 0 0\n\
/dev/sda3 /fs/first bcachefs rw,relatime 0 0\n\
/dev/sdb1 /fs/archive bcachefs rw 0 0\n\
/dev/sdc1 /mnt/external bcachefs rw 0 0\n\
";
        assert_eq!(
            super::parse_bcachefs_mounts(mounts),
            vec!["/fs/archive".to_string(), "/fs/first".to_string()]
        );
    }

    #[test]
    fn parse_bcachefs_mounts_returns_empty_when_no_bcachefs() {
        let mounts = "/dev/sda2 / ext4 rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n";
        assert!(super::parse_bcachefs_mounts(mounts).is_empty());
    }

    #[test]
    fn resolve_build_dir_appends_subdir_to_bare_pool() {
        assert_eq!(
            super::resolve_build_dir("/fs/first"),
            "/fs/first/.nasty-nix-build"
        );
        assert_eq!(
            super::resolve_build_dir("/fs/first/"),
            "/fs/first/.nasty-nix-build"
        );
    }

    #[test]
    fn resolve_build_dir_passes_fully_qualified_path_through() {
        // Already a spillover dir — don't double-wrap.
        assert_eq!(
            super::resolve_build_dir("/fs/first/.nasty-nix-build"),
            "/fs/first/.nasty-nix-build"
        );
    }

    #[test]
    fn build_dir_fragments_are_empty_when_unset() {
        let bd = super::build_dir_fragments(None);
        assert!(bd.setup.is_empty());
        assert!(bd.env_prefix.is_empty());
        assert!(bd.opt_suffix.is_empty());
        assert!(bd.cleanup.is_empty());
    }

    #[test]
    fn build_dir_fragments_render_single_user_mode_when_set() {
        // The empirically-proven combo: NIX_REMOTE=local + --option
        // build-dir <path>. Anything else (TMPDIR alone, daemon-side
        // build-dir, --option without single-user mode) was tested
        // live on .59 and didn't actually divert sandbox traffic.
        let bd = super::build_dir_fragments(Some("/fs/first"));
        assert!(bd.setup.contains("/fs/first/.nasty-nix-build"));
        assert!(bd.setup.contains("mkdir -p"));
        assert!(bd.setup.contains("chmod 0755")); // Nix refuses world-writable build-dirs
        assert_eq!(bd.env_prefix, "NIX_REMOTE=local ");
        assert!(
            bd.opt_suffix
                .contains("--option build-dir \"/fs/first/.nasty-nix-build\"")
        );
        assert!(bd.cleanup.contains("rm -rf"));
    }

    // ── booted-generation comparison ───────────────────────────────

    #[test]
    fn is_booted_generation_matches_only_when_paths_equal() {
        let a = Path::new("/nix/store/xyz-nixos-system");
        let b = Path::new("/nix/store/abc-nixos-system");
        assert!(is_booted_generation(Some(a), Some(a)));
        assert!(!is_booted_generation(Some(a), Some(b)));
    }

    #[test]
    fn is_booted_generation_returns_false_when_either_path_missing() {
        let a = Path::new("/nix/store/xyz");
        // A missing path is the safe default — treat as "not the booted one".
        // (delete_generation relies on this so it never wrongly blocks deletion.)
        assert!(!is_booted_generation(None, None));
        assert!(!is_booted_generation(Some(a), None));
        assert!(!is_booted_generation(None, Some(a)));
    }

    // ── flake.nix input URL parsing / rewriting ────────────────────

    fn sample_flake() -> &'static str {
        r#"{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.2";
    nasty.url = "github:nasty-project/nasty/v0.0.5";
  };
  outputs = { self, nixpkgs, ... }: {};
}"#
    }

    #[test]
    fn parse_flake_input_urls_extracts_all_three_inputs() {
        let parsed = parse_flake_input_urls(sample_flake()).expect("parses");
        assert_eq!(
            parsed.get("nixpkgs").map(|p| p.url.as_str()),
            Some("github:NixOS/nixpkgs/nixos-unstable")
        );
        assert_eq!(
            parsed.get("bcachefs-tools").map(|p| p.url.as_str()),
            Some("github:koverstreet/bcachefs-tools/v1.38.2")
        );
        assert_eq!(
            parsed.get("nasty").map(|p| p.url.as_str()),
            Some("github:nasty-project/nasty/v0.0.5")
        );
    }

    #[test]
    fn parse_flake_input_urls_is_pure_no_required_input() {
        // The parser is name-agnostic: it picks up whatever top-level
        // `<input>.url = "..."` declarations are present, and that's
        // it. "nasty must exist" is a wrapper-specific invariant
        // enforced one level up in `read_flake_input_urls` (the async
        // wrapper that's only called against `/etc/nixos/flake.nix`).
        // Decoupling means the same parser can run against nasty's
        // own flake.nix (which has no `nasty.url`) for the
        // embedded-default-ref lookup.
        let no_nasty = r#"{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.2";
  };
}"#;
        let parsed = parse_flake_input_urls(no_nasty).expect("parser doesn't require nasty");
        assert!(
            parsed.contains_key("nixpkgs"),
            "parser should still pick up other inputs"
        );
        assert!(
            parsed.contains_key("bcachefs-tools"),
            "parser should still pick up other inputs"
        );
        assert!(
            !parsed.contains_key("nasty"),
            "parser shouldn't fabricate inputs that aren't declared"
        );
    }

    /// The current wrapper-flake shape (post wrapper-follows refactor)
    /// declares only `nasty.url`; nixpkgs and bcachefs-tools come in
    /// via `follows`. The parser must accept that shape — returning
    /// just the nasty entry, no error.
    #[test]
    fn parse_flake_input_urls_accepts_follows_only_wrapper() {
        let follows_shape = r#"{
  inputs = {
    nasty.url = "github:nasty-project/nasty/v0.0.9";
    nixpkgs.follows = "nasty/nixpkgs";
    bcachefs-tools.follows = "nasty/bcachefs-tools";
  };
}"#;
        let parsed = parse_flake_input_urls(follows_shape).expect("follows-shape parses");
        assert_eq!(parsed.len(), 1, "only nasty.url is present");
        assert_eq!(
            parsed.get("nasty").map(|p| p.url.as_str()),
            Some("github:nasty-project/nasty/v0.0.9")
        );
        assert!(!parsed.contains_key("nixpkgs"));
        assert!(!parsed.contains_key("bcachefs-tools"));
    }

    #[test]
    fn rewrite_flake_input_urls_replaces_only_the_targeted_input() {
        let mut replacements = HashMap::new();
        replacements.insert(
            "nasty".to_string(),
            "github:nasty-project/nasty/v0.0.6".to_string(),
        );
        let rewritten = rewrite_flake_input_urls(sample_flake(), &replacements).expect("rewrite");
        // Target replaced.
        assert!(rewritten.contains("github:nasty-project/nasty/v0.0.6"));
        assert!(!rewritten.contains("github:nasty-project/nasty/v0.0.5"));
        // Other inputs untouched.
        assert!(rewritten.contains("github:NixOS/nixpkgs/nixos-unstable"));
        assert!(rewritten.contains("github:koverstreet/bcachefs-tools/v1.38.2"));
        // File still parses with the same three inputs.
        let reparsed = parse_flake_input_urls(&rewritten).expect("still parses");
        assert_eq!(reparsed.len(), 3);
    }

    #[test]
    fn rewrite_flake_input_urls_with_no_replacements_is_identity() {
        let rewritten = rewrite_flake_input_urls(sample_flake(), &HashMap::new()).unwrap();
        assert_eq!(rewritten, sample_flake());
    }

    #[test]
    fn rewrite_flake_input_urls_errors_when_target_input_is_missing() {
        let mut replacements = HashMap::new();
        replacements.insert("does-not-exist".to_string(), "github:x/y/z".to_string());
        // Validation happens via parse_flake_input_urls which only knows about
        // nixpkgs/bcachefs-tools/nasty — but it succeeds for our well-formed
        // sample. The lookup against the replacements map then finds nothing.
        // Either way: an unknown input must not silently no-op.
        let err = rewrite_flake_input_urls(sample_flake(), &replacements).unwrap_err();
        assert!(format!("{err:?}").contains("does-not-exist"));
    }

    // ── unquote_nix_string ─────────────────────────────────────────

    #[test]
    fn unquote_nix_string_unwraps_quoted_strings() {
        assert_eq!(unquote_nix_string("\"hello\""), Some("hello".to_string()));
        assert_eq!(
            unquote_nix_string("\"with\\nescape\""),
            Some("with\nescape".to_string())
        );
    }

    #[test]
    fn unquote_nix_string_returns_none_for_non_strings() {
        assert_eq!(unquote_nix_string("hello"), None);
        assert_eq!(unquote_nix_string("42"), None);
        assert_eq!(unquote_nix_string(""), None);
    }

    // ── NixosGeneration JSON shape ─────────────────────────────────

    #[test]
    fn nixos_generation_parses_real_nixos_rebuild_output() {
        // Shape lifted from `nixos-rebuild list-generations --json`.
        // Pinning camelCase field names protects against silent breakage if
        // `serde(rename = ...)` is dropped during a refactor.
        let json = r#"[
          {"generation":42,"date":"2026-04-12T10:30:00Z","nixosVersion":"24.11","kernelVersion":"6.12.0","current":true},
          {"generation":41,"date":"2026-04-10T08:15:00Z","nixosVersion":"24.11","kernelVersion":"6.11.5","current":false}
        ]"#;
        let gens: Vec<super::NixosGeneration> = serde_json::from_str(json).expect("parses");
        assert_eq!(gens.len(), 2);
        assert_eq!(gens[0].generation, 42);
        assert_eq!(gens[0].nixos_version, "24.11");
        assert_eq!(gens[0].kernel_version, "6.12.0");
        assert!(gens[0].current);
        assert!(!gens[1].current);
    }
}
