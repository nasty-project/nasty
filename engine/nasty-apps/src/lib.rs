//! App runtime management — Docker-based container management via bollard.
//!
//! Two modes:
//! - **Simple**: single-container apps configured via the WebUI form
//!   (image, ports, env, volumes) — managed directly through the Docker API.
//! - **Compose**: multi-container apps from a user-provided docker-compose.yml
//!   — managed via the `docker compose` CLI.
//!
//! Simple apps are labeled with `nasty.managed=true` for identification.
//! Compose apps are discovered by scanning `/var/lib/nasty/apps/` for
//! docker-compose.yml files and using Docker's `com.docker.compose.project` label.

use std::collections::HashMap;
use std::path::Path;

use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, ListContainersOptions, LogsOptions,
    RemoveContainerOptions, StopContainerOptions,
};
use futures_util::TryStreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{error, info, warn};

const STATE_PATH: &str = "/var/lib/nasty/apps-enabled";
const PROXY_CONF: &str = "/var/lib/nasty/apps-proxy.conf";
const COMPOSE_DIR: &str = "/var/lib/nasty/apps";
const DOCKER_SERVICE: &str = "docker.service";

/// Label applied to all NASty-managed containers.
const LABEL_MANAGED: &str = "nasty.managed";
/// Label storing the app name.
const LABEL_APP_NAME: &str = "nasty.app.name";
/// Label storing the app kind: "simple" or "compose".
const LABEL_APP_KIND: &str = "nasty.app.kind";
/// Label set to "true" when the app was deployed with allow_unsafe.
const LABEL_APP_UNSAFE: &str = "nasty.app.unsafe";

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum AppsError {
    #[error("apps runtime is not enabled")]
    NotEnabled,
    #[error("apps runtime is already enabled")]
    AlreadyEnabled,
    #[error("docker is not ready: {0}")]
    NotReady(String),
    #[error("app not found: {0}")]
    AppNotFound(String),
    #[error("app already exists: {0}")]
    AppAlreadyExists(String),
    #[error("docker error: {0}")]
    DockerFailed(String),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("forbidden bind mount: {0}")]
    ForbiddenBind(String),
}

impl AppsError {
    pub fn code(&self) -> i64 {
        match self {
            Self::NotEnabled => -33001,
            Self::AlreadyEnabled => -33002,
            Self::NotReady(_) => -33003,
            Self::AppNotFound(_) => -33004,
            Self::AppAlreadyExists(_) => -33005,
            Self::DockerFailed(_) => -33006,
            Self::CommandFailed(_) => -33007,
            Self::Io(_) => -33008,
            Self::ForbiddenBind(_) => -33009,
        }
    }
}

/// Validate the `host_path` of every volume in a simple-app install/update.
/// Mirrors the compose-side validator: bind mounts to `/`, anything containing
/// `..`, and any path under `/var/lib/nasty/` (except the app's own dir) are
/// rejected outright. In strict mode (allow_unsafe == false), bind mounts
/// must additionally fall under the app's data dir or `/fs/`.
pub fn validate_simple_volumes(
    app_name: &str,
    storage_base: &str,
    volumes: &[AppVolume],
    allow_unsafe: bool,
) -> Result<(), AppsError> {
    let app_data_dir = format!("{}/{}", storage_base.trim_end_matches('/'), app_name);
    for v in volumes {
        // Empty host_path is fine — the install path auto-generates one
        // under the storage base.
        if v.host_path.is_empty() {
            continue;
        }
        let src = &v.host_path;

        if src.contains("..") {
            return Err(AppsError::ForbiddenBind(format!(
                "'{src}' escapes via '..'"
            )));
        }
        if src == "/" {
            return Err(AppsError::ForbiddenBind(
                "host root '/' is never allowed as a bind mount".to_string(),
            ));
        }
        let in_app_dir = src == &app_data_dir || src.starts_with(&format!("{app_data_dir}/"));
        // Engine state dir is off-limits even with allow_unsafe — that's where
        // auth.json, settings.json, audit.log, OIDC client secrets live.
        let in_engine_state = src == "/var/lib/nasty" || src.starts_with("/var/lib/nasty/");
        if in_engine_state && !in_app_dir {
            return Err(AppsError::ForbiddenBind(format!(
                "'{src}' targets engine state — not allowed even with allow_unsafe"
            )));
        }

        if !allow_unsafe {
            let allowed = in_app_dir || src == "/fs" || src.starts_with("/fs/");
            if !allowed {
                return Err(AppsError::ForbiddenBind(format!(
                    "'{src}' is outside '{app_data_dir}/' and '/fs/'. Set allow_unsafe to override."
                )));
            }
        }
    }
    Ok(())
}

impl From<bollard::errors::Error> for AppsError {
    fn from(e: bollard::errors::Error) -> Self {
        Self::DockerFailed(e.to_string())
    }
}

/// Parse `user:` into (uid, gid). Compose accepts `1000`, `1000:1000`,
/// `username`, `username:groupname`. We resolve only numeric forms —
/// resolving usernames would require reading /etc/passwd off the host
/// (which is fine in principle, just punted for v1 to keep the surface
/// small). Returns None for non-numeric or missing.
fn parse_user_field(v: &serde_json::Value) -> Option<(u32, Option<u32>)> {
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    let (uid_part, gid_part) = match s.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (s, None),
    };
    let uid: u32 = uid_part.parse().ok()?;
    let gid: Option<u32> = gid_part.and_then(|g| g.parse().ok());
    Some((uid, gid))
}

/// Pull PUID/PGID out of an `environment:` block (LinuxServer.io
/// convention). Both list form (`- PUID=1000`) and map form
/// (`PUID: 1000`) are supported.
fn parse_puid_pgid(env: &serde_json::Value) -> Option<(u32, Option<u32>)> {
    let mut puid: Option<u32> = None;
    let mut pgid: Option<u32> = None;
    let assign = |key: &str, value: &str, puid: &mut Option<u32>, pgid: &mut Option<u32>| match key
    {
        "PUID" => *puid = value.parse().ok(),
        "PGID" => *pgid = value.parse().ok(),
        _ => {}
    };
    if let Some(map) = env.as_object() {
        for (k, v) in map {
            let value = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => continue,
            };
            assign(k, &value, &mut puid, &mut pgid);
        }
    } else if let Some(list) = env.as_array() {
        for item in list {
            if let Some(s) = item.as_str()
                && let Some((k, v)) = s.split_once('=')
            {
                assign(k, v, &mut puid, &mut pgid);
            }
        }
    }
    puid.map(|u| (u, pgid))
}

/// Extract the host path of one volume entry. Compose accepts:
/// short-form `"src:dst[:opts]"` and long-form
/// `{ type: bind, source: ..., target: ... }`. Named volumes (`data:/x`)
/// and tmpfs/npipe are skipped.
fn extract_bind_source_target(entry: &serde_json::Value) -> Option<(String, String)> {
    match entry {
        serde_json::Value::String(s) => {
            // "src:dst[:opts]" — only the host bind case has an absolute
            // src starting with '/'. Named volumes look like "data:/path".
            let mut parts = s.splitn(3, ':');
            let src = parts.next()?.to_string();
            let dst = parts.next()?.to_string();
            if !src.starts_with('/') {
                return None;
            }
            Some((src, dst))
        }
        serde_json::Value::Object(map) => {
            let kind = map.get("type").and_then(|v| v.as_str()).unwrap_or("volume");
            if kind != "bind" {
                return None;
            }
            let src = map.get("source").and_then(|v| v.as_str())?.to_string();
            let dst = map.get("target").and_then(|v| v.as_str())?.to_string();
            if !src.starts_with('/') {
                return None;
            }
            Some((src, dst))
        }
        _ => None,
    }
}

/// Walk a parsed compose YAML and extract every bind-mount paired with
/// its service's expected (uid, gid). The expected ids come from the
/// service's `user:` field, falling back to PUID/PGID env vars when
/// `user:` isn't set (LinuxServer.io style images).
pub fn extract_compose_binds(yaml: &str) -> Vec<ComposeBind> {
    let parsed: serde_json::Value = match serde_yaml_ng::from_str(yaml) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let services = match parsed.get("services").and_then(|s| s.as_object()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut binds = Vec::new();
    for (svc_name, svc) in services {
        let user = svc
            .get("user")
            .and_then(parse_user_field)
            .or_else(|| svc.get("environment").and_then(parse_puid_pgid));
        let (expected_uid, expected_gid) = match user {
            Some((u, g)) => (Some(u), g),
            None => (None, None),
        };
        let volumes = match svc.get("volumes").and_then(|v| v.as_array()) {
            Some(v) => v,
            None => continue,
        };
        for entry in volumes {
            if let Some((src, dst)) = extract_bind_source_target(entry) {
                binds.push(ComposeBind {
                    service: svc_name.clone(),
                    host_path: src,
                    mount_path: dst,
                    expected_uid,
                    expected_gid,
                });
            }
        }
    }
    binds
}

/// Best-effort 1-based line number of the first occurrence of `needle`
/// in `haystack`. Used to underline the offending volume entry in the
/// compose editor.
fn find_line(haystack: &str, needle: &str) -> Option<u32> {
    for (i, line) in haystack.lines().enumerate() {
        if line.contains(needle) {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// If `path` is `/fs/<X>/…`, return `Some("X")`. Otherwise `None`
/// (bare `/fs`, `/fs/`, or anything outside `/fs/`). This is just
/// string parsing — `fs_root_is_mounted` below does the actual
/// stat() check. Split out so it can be unit-tested without
/// touching the real `/fs`.
fn fs_root_segment(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/fs/")?;
    let seg = rest.split('/').next()?;
    if seg.is_empty() { None } else { Some(seg) }
}

/// True when a bind path's first `/fs/<X>/` segment names an actual
/// mounted filesystem. Compares the `st_dev` of `/fs/<X>` against
/// `/fs` — they only differ when `<X>` is a separate mountpoint, so
/// a stale rootfs directory from a previous buggy run still reports
/// as not-mounted. Paths outside `/fs/` (allow_unsafe territory)
/// pass through as true — they're the user's responsibility, not
/// ours to manage.
fn fs_root_is_mounted(path: &str) -> bool {
    let Some(fs_name) = fs_root_segment(path) else {
        // Either the path isn't under /fs/ at all, or it's the bare
        // /fs root — pre-creating that is not a thing we'd ever do.
        return path.strip_prefix("/fs/").is_none();
    };
    use std::os::unix::fs::MetadataExt;
    let fs_path = format!("/fs/{fs_name}");
    let (Ok(child), Ok(parent)) = (std::fs::metadata(&fs_path), std::fs::metadata("/fs")) else {
        return false;
    };
    child.dev() != parent.dev()
}

/// Security half of the bind validator: no `..` traversal, no host
/// root, no engine state, must be absolute. These failures are
/// admin-policy violations the user can't fix from the wizard — the
/// volume-warning code skips them silently because surfacing them
/// would just be noise.
fn validate_chown_target_security(host_path: &str) -> Result<(), AppsError> {
    if host_path.contains("..") {
        return Err(AppsError::ForbiddenBind(format!(
            "'{host_path}' contains '..'"
        )));
    }
    if host_path == "/" {
        return Err(AppsError::ForbiddenBind(
            "host root '/' is never allowed".to_string(),
        ));
    }
    if host_path == "/var/lib/nasty" || host_path.starts_with("/var/lib/nasty/") {
        return Err(AppsError::ForbiddenBind(format!(
            "'{host_path}' targets engine state"
        )));
    }
    if !host_path.starts_with('/') {
        return Err(AppsError::ForbiddenBind(format!(
            "'{host_path}' is not absolute"
        )));
    }
    Ok(())
}

/// Filesystem-existence half of the bind validator: a path of the
/// form `/fs/<X>/…` requires `<X>` to be a real mounted bcachefs
/// filesystem. Without this guard, `precreate_compose_binds` would
/// `mkdir -p` the path on rootfs, which a) pollutes `/fs` with
/// phantom entries from typos and b) silently masks "wrong
/// filesystem name" mistakes. Unlike the security checks, this one
/// is user-fixable — the volume-warning code surfaces it as a hard
/// error in the wizard so the user sees what's wrong with their
/// compose.
fn validate_fs_root_mounted(host_path: &str) -> Result<(), AppsError> {
    if let Some(fs_name) = fs_root_segment(host_path)
        && !fs_root_is_mounted(host_path)
    {
        return Err(AppsError::ForbiddenBind(format!(
            "filesystem '{fs_name}' is not mounted at /fs/{fs_name} — fix the bind source path"
        )));
    }
    Ok(())
}

/// Full bind validator used by `fix_volume_perms` and the
/// pre-create step. Combines the security and FS-mounted checks.
fn validate_chown_target(host_path: &str) -> Result<(), AppsError> {
    validate_chown_target_security(host_path)?;
    validate_fs_root_mounted(host_path)?;
    Ok(())
}

// ── Types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
pub struct AppsStatus {
    /// Whether the apps runtime is enabled.
    pub enabled: bool,
    /// Whether Docker is currently running and responsive.
    pub running: bool,
    /// Number of managed apps (running or stopped).
    pub app_count: usize,
    /// Total memory usage of managed containers in bytes.
    pub memory_bytes: Option<u64>,
    /// Path to the apps storage directory on bcachefs.
    pub storage_path: Option<String>,
    /// Whether the storage directory exists on disk.
    pub storage_ok: bool,
    /// Docker server version.
    pub docker_version: Option<String>,
    /// Docker disk usage: images + containers + volumes in bytes.
    pub disk_usage_bytes: Option<u64>,
}

/// Result of apps.prune — how much space was reclaimed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PruneResult {
    pub images_removed: usize,
    pub space_reclaimed_bytes: u64,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EnableAppsRequest {
    /// Filesystem to store app data on.
    pub filesystem: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct App {
    /// App name (container name for simple, project name for compose).
    pub name: String,
    /// Container image (primary image for compose apps).
    pub image: String,
    /// Current status: "running", "stopped", "restarting", "created", "exited".
    pub status: String,
    /// ISO 8601 timestamp of when the container was created.
    pub created: String,
    /// App kind: "simple" or "compose".
    pub kind: String,
    /// Individual containers (for compose apps with multiple services).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<AppContainer>,
    /// Host ports mapped by this app.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<MappedPort>,
    /// True if the app was deployed with allow_unsafe — i.e. it has elevated
    /// privileges (caps, host devices, host namespaces, or bind mounts
    /// outside the standard sandbox). Surfaced as a badge in the WebUI.
    #[serde(default)]
    pub unsafe_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppContainer {
    /// Service name (compose service or container name).
    pub name: String,
    /// Docker container ID (short).
    pub container_id: String,
    /// Container image.
    pub image: String,
    /// Container status.
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MappedPort {
    /// Host port.
    pub host_port: u16,
    /// Container port.
    pub container_port: u16,
    /// Protocol (tcp/udp).
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AppConfig {
    pub name: String,
    pub image: String,
    pub ports: Vec<AppPort>,
    pub env: Vec<AppEnv>,
    pub volumes: Vec<AppVolume>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    /// Whether the app was deployed with allow_unsafe (read from container label).
    #[serde(default)]
    pub allow_unsafe: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImageInspectResult {
    pub ports: Vec<AppPort>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallAppRequest {
    /// App name. Must be DNS-safe.
    pub name: String,
    /// Container image (e.g. "lscr.io/linuxserver/plex:latest").
    pub image: String,
    /// Ports to expose.
    #[serde(default)]
    pub ports: Vec<AppPort>,
    /// Environment variables.
    #[serde(default)]
    pub env: Vec<AppEnv>,
    /// Bind-mount volumes.
    #[serde(default)]
    pub volumes: Vec<AppVolume>,
    /// CPU limit (e.g. "0.5" for half a core, "2" for 2 cores).
    pub cpu_limit: Option<String>,
    /// Memory limit (e.g. "256m", "1g").
    pub memory_limit: Option<String>,
    /// Opt out of the strict bind-mount allowlist. Admin-only / audited /
    /// surfaced as a badge in the UI. Engine state and the host root are
    /// still rejected even with this set.
    #[serde(default)]
    pub allow_unsafe: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppPort {
    /// Port name (e.g. "http", "webui").
    pub name: String,
    /// Container port number.
    pub container_port: u16,
    /// Host port to map to (optional, auto-assigned if omitted).
    pub host_port: Option<u16>,
    /// Protocol: "TCP" or "UDP" (default: TCP).
    #[serde(default = "default_tcp")]
    pub protocol: String,
}

fn default_tcp() -> String {
    "TCP".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppEnv {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppVolume {
    /// Volume name (e.g. "config", "data").
    pub name: String,
    /// Mount path inside the container.
    pub mount_path: String,
    /// Host path (auto-generated under apps storage if empty).
    #[serde(default)]
    pub host_path: String,
}

// ── Compose types ──────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallComposeRequest {
    /// App name (used as compose project name).
    pub name: String,
    /// Contents of docker-compose.yml.
    pub compose_file: String,
}

// ── Ingress types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppIngress {
    /// App name.
    pub name: String,
    /// Host port to proxy to.
    pub host_port: u16,
    /// URL path prefix (e.g. "/apps/plex/").
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetIngressRequest {
    /// App name.
    pub name: String,
    /// Host port to proxy to.
    pub host_port: u16,
}

// ── Port check types ──────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckPortsRequest {
    /// Ports to check for conflicts.
    pub ports: Vec<u16>,
    /// App name to exclude from conflict check (for updates).
    #[serde(default)]
    pub exclude_app: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PortConflict {
    /// The port that has a conflict.
    pub port: u16,
    /// What is using this port (e.g. "nginx", "app:plex").
    pub used_by: String,
}

// ── Device check types ────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckDevicesRequest {
    /// Host device paths to check, e.g. `/dev/dri/renderD128`. Anything
    /// after the first colon (the in-container path or cgroup perms) is
    /// the caller's job to strip — this RPC only stat()s host paths.
    pub paths: Vec<String>,
}

/// One missing device. `parent_exists` lets the UI distinguish between
/// "device doesn't exist but its parent dir does" (likely the host has
/// some GPU but not the requested one) and "parent dir is missing too"
/// (likely the relevant kernel module isn't loaded — `/dev/dri` not
/// being present means no DRM driver bound any display device).
#[derive(Debug, Serialize, JsonSchema)]
pub struct DeviceMissing {
    /// The device path the caller asked about, echoed back.
    pub path: String,
    /// True when the path's parent directory exists on the host.
    pub parent_exists: bool,
}

// ── Volume permission check types ─────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckVolumesRequest {
    /// Full docker-compose YAML text. Server parses it and stat()s each
    /// bind-mount source. Sent in full (rather than per-volume) so the
    /// server can correlate sources with their owning service's `user:`
    /// field — that's the comparison we make.
    pub compose: String,
}

/// One bind-mount whose host owner doesn't match what the container
/// will run as (or whose host path is missing). Returned by
/// `apps.check_volumes`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct VolumeMismatch {
    pub service: String,
    pub host_path: String,
    pub mount_path: String,
    pub expected_uid: u32,
    pub expected_gid: Option<u32>,
    /// Owner UID on the host. None when the path doesn't exist yet.
    pub current_uid: Option<u32>,
    pub current_gid: Option<u32>,
    /// True when the source path exists on the host. False = the
    /// directory will be created by the deploy pipeline; we'll chown
    /// it to expected at create time, so it's informational rather
    /// than an error.
    pub exists: bool,
    /// True when the path is `/fs/<X>/…` and `<X>` is not a mounted
    /// filesystem. Distinct from `!exists` because pre-create would
    /// `mkdir -p` it on rootfs — a hard error the user must fix in
    /// their compose, not a "we'll create it for you" hint.
    #[serde(default)]
    pub filesystem_missing: bool,
    /// 1-based line number of the volume entry in the compose file
    /// (for editor underlining). Best-effort: we substring-match the
    /// host path against the source; ambiguous duplicates resolve to
    /// the first occurrence.
    pub line: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FixVolumePermsRequest {
    /// Host bind-mount source to chown. Validated against the same
    /// forbidden-bind rules as compose deploys (no `..`, no `/`, no
    /// engine state).
    pub host_path: String,
    pub uid: u32,
    pub gid: u32,
    /// When true, recurse into the directory tree. Off by default
    /// because recursive chown on a path like `/fs/tank/media` rewrites
    /// ownership on every existing file under it — almost never what
    /// the user wants if the path was pre-populated.
    #[serde(default)]
    pub recursive: bool,
}

/// One bind-mount extracted from a compose file. Used both by
/// `check_volumes` (read-only) and the install pipeline's auto-chown
/// step (it pre-creates missing dirs with the right ownership).
#[derive(Debug, Clone)]
pub struct ComposeBind {
    pub service: String,
    pub host_path: String,
    pub mount_path: String,
    pub expected_uid: Option<u32>,
    pub expected_gid: Option<u32>,
}

// ── Service ─────────────────────────────────────────────────────

pub struct AppsService {
    docker: std::sync::Mutex<Option<Docker>>,
}

impl Default for AppsService {
    fn default() -> Self {
        Self::new()
    }
}

impl AppsService {
    pub fn new() -> Self {
        let docker = Docker::connect_with_unix_defaults()
            .or_else(|_| {
                Docker::connect_with_unix("/var/run/docker.sock", 120, bollard::API_DEFAULT_VERSION)
            })
            .ok();
        if docker.is_none() {
            info!("Docker not available at startup — will connect on demand");
        }
        Self {
            docker: std::sync::Mutex::new(docker),
        }
    }

    /// Get a reference to the bollard Docker client (for use by deploy streaming).
    pub fn docker_client(&self) -> Result<Docker, AppsError> {
        self.docker_conn()
    }

    fn docker_conn(&self) -> Result<Docker, AppsError> {
        let guard = self.docker.lock().unwrap();
        if let Some(ref d) = *guard {
            return Ok(d.clone());
        }
        drop(guard);
        // Try to connect (Docker may have started after engine)
        let d = Docker::connect_with_unix_defaults()
            .or_else(|_| {
                Docker::connect_with_unix("/var/run/docker.sock", 120, bollard::API_DEFAULT_VERSION)
            })
            .map_err(|_| AppsError::NotReady("Docker socket not available".into()))?;
        *self.docker.lock().unwrap() = Some(d.clone());
        info!("Connected to Docker socket");
        Ok(d)
    }

    fn docker(&self) -> Result<Docker, AppsError> {
        self.docker_conn()
    }

    // ── Enable/Disable ──────────────────────────────────────

    pub fn is_enabled(&self) -> bool {
        Path::new(STATE_PATH).exists()
    }

    pub fn load_config() -> AppsConfig {
        let content = match std::fs::read_to_string(STATE_PATH) {
            Ok(c) => c,
            Err(_) => return AppsConfig::default(),
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    async fn save_config(config: &AppsConfig) -> Result<(), AppsError> {
        let json = serde_json::to_string_pretty(config)
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
        tokio::fs::write(STATE_PATH, json).await?;
        Ok(())
    }

    pub async fn enable(&self, req: EnableAppsRequest) -> Result<(), AppsError> {
        if self.is_enabled() {
            return Err(AppsError::AlreadyEnabled);
        }

        let config = AppsConfig {
            enabled: true,
            storage_path: None,
        };
        Self::save_config(&config).await?;

        // Configure Docker data-root on bcachefs before starting
        if let Err(e) = configure_docker_data_root(req.filesystem.as_deref()).await {
            warn!("Could not configure Docker data-root on bcachefs: {e}");
        }

        // Start Docker via systemd
        run_cmd("systemctl", &["start", DOCKER_SERVICE]).await?;

        info!("Apps runtime enabled — Docker starting");

        let filesystem = req.filesystem.clone();

        // Bootstrap in background
        tokio::spawn(async move {
            // Wait for Docker to be ready (up to 30s)
            let mut ready = false;
            for _ in 0..15 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if let Ok(docker) = Docker::connect_with_unix_defaults()
                    && docker.ping().await.is_ok()
                {
                    ready = true;
                    break;
                }
            }

            if !ready {
                error!("Docker did not become ready within 30s");
                return;
            }

            // Set up storage directory
            let storage_path = setup_apps_storage(filesystem.as_deref()).await;

            // Create compose directory
            let _ = tokio::fs::create_dir_all(COMPOSE_DIR).await;

            // Persist storage path in config
            if let Some(ref path) = storage_path {
                let config = AppsConfig {
                    enabled: true,
                    storage_path: Some(path.clone()),
                };
                let _ = AppsService::save_config(&config).await;
            }

            info!("Apps bootstrap complete");
        });

        Ok(())
    }

    pub async fn disable(&self) -> Result<(), AppsError> {
        if !self.is_enabled() {
            return Err(AppsError::NotEnabled);
        }

        // Stop all managed containers
        if let Ok(apps) = self.list().await {
            for app in &apps {
                if app.status == "running"
                    && let Ok(docker) = self.docker()
                {
                    let _ = docker
                        .stop_container(
                            &container_name(&app.name),
                            Some(StopContainerOptions {
                                t: Some(10),
                                signal: None,
                            }),
                        )
                        .await;
                }
            }
        }

        // Stop Docker
        run_cmd("systemctl", &["stop", DOCKER_SERVICE]).await?;

        // Remove state file
        let _ = tokio::fs::remove_file(STATE_PATH).await;

        info!("Apps runtime disabled — Docker stopped");
        Ok(())
    }

    // ── Status ──────────────────────────────────────────────

    pub async fn status(&self) -> AppsStatus {
        let config = Self::load_config();
        let enabled = self.is_enabled();
        let storage_path = config.storage_path.clone();
        let storage_ok = storage_path
            .as_ref()
            .map(|p| Path::new(p).is_dir())
            .unwrap_or(false);

        if !enabled {
            return AppsStatus {
                enabled,
                running: false,
                app_count: 0,
                memory_bytes: None,
                storage_path,
                storage_ok,
                docker_version: None,
                disk_usage_bytes: None,
            };
        }

        let running = self.is_docker_ready().await;
        if !running {
            return AppsStatus {
                enabled,
                running: false,
                app_count: 0,
                memory_bytes: None,
                storage_path,
                storage_ok,
                docker_version: None,
                disk_usage_bytes: None,
            };
        }

        let (apps_result, docker_version, memory_bytes, disk_usage_bytes) = tokio::join!(
            self.list_internal(),
            self.docker_version(),
            self.total_memory_usage(),
            self.docker_disk_usage(),
        );
        let app_count = apps_result.map(|a| a.len()).unwrap_or(0);

        AppsStatus {
            enabled,
            running,
            app_count,
            memory_bytes,
            storage_path,
            storage_ok,
            docker_version,
            disk_usage_bytes,
        }
    }

    // ── Simple app management ───────────────────────────────

    pub async fn install(&self, req: InstallAppRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        let cname = container_name(&req.name);

        // Check if already exists
        if self.container_exists(&cname).await {
            return Err(AppsError::AppAlreadyExists(req.name));
        }

        // Validate bind mounts before we pull anything. Engine state and the
        // host root are blocked unconditionally; the broader allowlist only
        // applies in safe mode.
        let storage_base = Self::load_config()
            .storage_path
            .unwrap_or_else(|| "/var/lib/nasty/apps-data".to_string());
        validate_simple_volumes(&req.name, &storage_base, &req.volumes, req.allow_unsafe)?;

        // Pull the image first
        self.pull_image(&req.image).await?;

        // Build port bindings — default host_port to container_port if not specified
        let used_ports = self.used_host_ports().await;
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        let mut exposed_ports: Vec<String> = Vec::new();

        for p in &req.ports {
            let host_port = p.host_port.unwrap_or(p.container_port);
            if used_ports.contains(&host_port) {
                return Err(AppsError::DockerFailed(format!(
                    "host port {} is already in use by another app",
                    host_port
                )));
            }
            let key = format!("{}/{}", p.container_port, p.protocol.to_lowercase());
            exposed_ports.push(key.clone());
            port_bindings.insert(
                key,
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(host_port.to_string()),
                }]),
            );
        }

        // Build mounts
        let storage_path = Self::load_config().storage_path;
        let mut binds = Vec::new();
        for v in &req.volumes {
            let host_path = if v.host_path.is_empty() {
                // Auto-generate path under apps storage
                let base = storage_path
                    .as_deref()
                    .unwrap_or("/var/lib/nasty/apps-data");
                let path = format!("{}/{}/{}", base, req.name, v.name);
                // Ensure the directory exists
                let _ = tokio::fs::create_dir_all(&path).await;
                path
            } else {
                v.host_path.clone()
            };
            binds.push(format!("{}:{}:rw", host_path, v.mount_path));
        }

        // Build env
        let env: Vec<String> = req
            .env
            .iter()
            .map(|e| format!("{}={}", e.name, e.value))
            .collect();

        // Resource limits
        let nano_cpus = req.cpu_limit.as_ref().and_then(|c| parse_cpu_limit(c));
        let memory = req
            .memory_limit
            .as_ref()
            .and_then(|m| parse_memory_limit(m));

        // Build labels
        let mut labels = HashMap::new();
        labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
        labels.insert(LABEL_APP_NAME.to_string(), req.name.clone());
        labels.insert(LABEL_APP_KIND.to_string(), "simple".to_string());
        if req.allow_unsafe {
            labels.insert(LABEL_APP_UNSAFE.to_string(), "true".to_string());
        }

        let host_config = HostConfig {
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            binds: if binds.is_empty() { None } else { Some(binds) },
            nano_cpus,
            memory,
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(req.image.clone()),
            env: if env.is_empty() { None } else { Some(env) },
            exposed_ports: if exposed_ports.is_empty() {
                None
            } else {
                Some(exposed_ports)
            },
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        };

        self.docker()?
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(cname.clone()),
                    platform: String::new(),
                }),
                config,
            )
            .await?;

        self.docker()?
            .start_container(
                &cname,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await?;

        info!("Installed app '{}' (image: {})", req.name, req.image);

        // Save manifest so the app is visible even when Docker is not running
        let manifest = serde_json::json!({
            "name": req.name,
            "image": req.image,
            "kind": "simple",
            "allow_unsafe": req.allow_unsafe,
        });
        let manifest_path = format!("{}/{}.json", COMPOSE_DIR, req.name);
        let _ = tokio::fs::create_dir_all(COMPOSE_DIR).await;
        if let Err(e) = tokio::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        {
            warn!("Failed to save app manifest: {e}");
        }

        // Auto-create ingress for the first TCP port. UDP can't serve
        // HTTP (nginx's `proxy_pass` is TCP-only), so a UDP port is
        // never a valid ingress target — picking it would publish a
        // dead /apps/<name>/ route. If the app exposes only UDP, no
        // ingress is created; the user can still reach the container
        // directly on the LAN.
        if let Some(first_port) = req
            .ports
            .iter()
            .find(|p| p.protocol.eq_ignore_ascii_case("tcp"))
        {
            let host_port = if let Some(hp) = first_port.host_port {
                hp
            } else {
                // Look up the actual assigned port from Docker
                self.get_mapped_port(&cname, first_port.container_port)
                    .await
                    .unwrap_or(first_port.container_port)
            };
            if let Err(e) = self
                .ingress_set(SetIngressRequest {
                    name: req.name.clone(),
                    host_port,
                })
                .await
            {
                warn!("Failed to auto-create ingress for '{}': {e}", req.name);
            }
        }

        self.get(&req.name).await
    }

    pub async fn update(&self, req: InstallAppRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        let cname = container_name(&req.name);

        // Verify app exists
        if !self.container_exists(&cname).await {
            return Err(AppsError::AppNotFound(req.name));
        }

        // Stop and remove the old container
        let _ = self
            .docker()?
            .stop_container(
                &cname,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: None,
                }),
            )
            .await;
        let _ = self
            .docker()?
            .remove_container(
                &cname,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;

        // Reinstall with new config
        self.install(req).await
    }

    pub async fn remove(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        let cname = container_name(name);

        // Check if it's a compose app
        let compose_dir = format!("{}/{}", COMPOSE_DIR, name);
        if Path::new(&compose_dir).join("docker-compose.yml").exists() {
            return self.compose_remove(name).await;
        }

        if !self.container_exists(&cname).await {
            return Err(AppsError::AppNotFound(name.to_string()));
        }

        // Stop and remove
        let _ = self
            .docker()?
            .stop_container(
                &cname,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: None,
                }),
            )
            .await;
        self.docker()?
            .remove_container(
                &cname,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true, // remove anonymous volumes
                    ..Default::default()
                }),
            )
            .await?;

        // Clean up ingress and manifest
        let _ = self.ingress_remove(name).await;
        let manifest_path = format!("{}/{}.json", COMPOSE_DIR, name);
        let _ = tokio::fs::remove_file(&manifest_path).await;

        info!("Removed app '{name}'");
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<App>, AppsError> {
        if self.require_ready().await.is_ok() {
            return self.list_internal().await;
        }
        // Docker not running — return offline list from filesystem
        Self::list_offline().await
    }

    /// List apps from on-disk state when Docker is not running.
    /// Compose apps are detected by docker-compose.yml files.
    /// Simple apps are detected by {name}.json manifest files.
    async fn list_offline() -> Result<Vec<App>, AppsError> {
        let apps_dir = std::path::Path::new(COMPOSE_DIR);
        if !apps_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut apps = Vec::new();
        let mut entries = tokio::fs::read_dir(apps_dir)
            .await
            .map_err(|e| AppsError::CommandFailed(format!("read apps dir: {e}")))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // Compose app: directory with docker-compose.yml
                let compose_path = path.join("docker-compose.yml");
                if compose_path.exists() {
                    let images: Vec<String> = tokio::fs::read_to_string(&compose_path)
                        .await
                        .ok()
                        .and_then(|content| {
                            let parsed: serde_json::Value =
                                serde_yaml_ng::from_str(&content).ok()?;
                            Some(
                                parsed
                                    .get("services")?
                                    .as_object()?
                                    .values()
                                    .filter_map(|svc| {
                                        svc.get("image")?.as_str().map(|s| s.to_string())
                                    })
                                    .collect(),
                            )
                        })
                        .unwrap_or_default();
                    let image = if images.len() == 1 {
                        images[0].clone()
                    } else {
                        format!("{} images", images.len())
                    };
                    let unsafe_mode = tokio::fs::read_to_string(path.join(".nasty-meta.json"))
                        .await
                        .ok()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                        .and_then(|v| v.get("allow_unsafe").and_then(|b| b.as_bool()))
                        .unwrap_or(false);
                    apps.push(App {
                        name,
                        image,
                        status: "stopped".to_string(),
                        created: String::new(),
                        kind: "compose".to_string(),
                        containers: Vec::new(),
                        ports: Vec::new(),
                        unsafe_mode,
                    });
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                // Simple app: manifest JSON
                if let Ok(content) = tokio::fs::read_to_string(&path).await
                    && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content)
                {
                    let app_name = manifest
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let image = manifest
                        .get("image")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let unsafe_mode = manifest
                        .get("allow_unsafe")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    if !app_name.is_empty() {
                        apps.push(App {
                            name: app_name,
                            image,
                            status: "stopped".to_string(),
                            created: String::new(),
                            kind: "simple".to_string(),
                            containers: Vec::new(),
                            ports: Vec::new(),
                            unsafe_mode,
                        });
                    }
                }
            }
        }
        apps.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(apps)
    }

    async fn list_internal(&self) -> Result<Vec<App>, AppsError> {
        // List simple apps (labeled by us)
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_MANAGED}=true")]);

        let labeled = self
            .docker()?
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await?;

        let mut apps = Vec::new();
        let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        for c in &labeled {
            let labels = c.labels.as_ref();
            let app_name = labels
                .and_then(|l| l.get(LABEL_APP_NAME))
                .cloned()
                .unwrap_or_default();

            if app_name.is_empty() || seen_names.contains(&app_name) {
                continue;
            }
            seen_names.insert(app_name.clone());

            let kind = labels
                .and_then(|l| l.get(LABEL_APP_KIND))
                .cloned()
                .unwrap_or_else(|| "simple".to_string());
            let unsafe_mode = labels
                .and_then(|l| l.get(LABEL_APP_UNSAFE))
                .map(|v| v == "true")
                .unwrap_or(false);

            apps.push(App {
                name: app_name,
                image: c.image.as_deref().unwrap_or("").to_string(),
                status: container_status_str(c),
                created: c.created.map(chrono_from_timestamp).unwrap_or_default(),
                kind,
                containers: vec![],
                ports: extract_ports(c),
                unsafe_mode,
            });
        }

        // Discover compose apps from the compose directory
        if let Ok(mut entries) = tokio::fs::read_dir(COMPOSE_DIR).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                if seen_names.contains(&name) {
                    continue;
                }
                let compose_path = entry.path().join("docker-compose.yml");
                if !compose_path.exists() {
                    continue;
                }

                // Find all containers from this compose project
                let mut pf = HashMap::new();
                pf.insert(
                    "label".to_string(),
                    vec![format!("com.docker.compose.project={name}")],
                );
                let compose_containers = self
                    .docker()?
                    .list_containers(Some(ListContainersOptions {
                        all: true,
                        filters: Some(pf),
                        ..Default::default()
                    }))
                    .await
                    .unwrap_or_default();

                // Collect all containers, ports, and derive overall status
                let mut containers = Vec::new();
                let mut all_ports = Vec::new();
                let mut any_running = false;
                let mut primary_image = String::new();
                let mut created = String::new();

                for c in &compose_containers {
                    let svc_name = c
                        .labels
                        .as_ref()
                        .and_then(|l| l.get("com.docker.compose.service"))
                        .cloned()
                        .unwrap_or_default();
                    let image = c.image.as_deref().unwrap_or("").to_string();
                    let status = container_status_str(c);

                    if primary_image.is_empty() {
                        primary_image = image.clone();
                        created = c.created.map(chrono_from_timestamp).unwrap_or_default();
                    }
                    if status == "running" {
                        any_running = true;
                    }

                    let container_id =
                        c.id.as_deref()
                            .unwrap_or("")
                            .chars()
                            .take(12)
                            .collect::<String>();
                    all_ports.extend(extract_ports(c));
                    containers.push(AppContainer {
                        name: svc_name,
                        container_id,
                        image,
                        status,
                    });
                }

                all_ports.sort_by_key(|p| p.host_port);
                all_ports.dedup_by_key(|p| p.host_port);

                let overall_status = if compose_containers.is_empty() {
                    "stopped".to_string()
                } else if any_running {
                    "running".to_string()
                } else {
                    "exited".to_string()
                };

                let unsafe_mode = tokio::fs::read_to_string(entry.path().join(".nasty-meta.json"))
                    .await
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| v.get("allow_unsafe").and_then(|b| b.as_bool()))
                    .unwrap_or(false);

                seen_names.insert(name.clone());
                apps.push(App {
                    name,
                    image: primary_image,
                    status: overall_status,
                    created,
                    kind: "compose".to_string(),
                    containers,
                    ports: all_ports,
                    unsafe_mode,
                });
            }
        }

        Ok(apps)
    }

    pub async fn inspect(&self, name: &str) -> Result<serde_json::Value, AppsError> {
        self.require_ready().await?;
        let cname = container_name(name);
        let info = self
            .docker()?
            .inspect_container(&cname, None)
            .await
            .map_err(|_| AppsError::AppNotFound(name.to_string()))?;
        serde_json::to_value(&info)
            .map_err(|e| AppsError::CommandFailed(format!("serialize inspect: {e}")))
    }

    pub async fn get(&self, name: &str) -> Result<App, AppsError> {
        let apps = self.list().await?;
        apps.into_iter()
            .find(|a| a.name == name)
            .ok_or_else(|| AppsError::AppNotFound(name.to_string()))
    }

    pub async fn get_config(&self, name: &str) -> Result<AppConfig, AppsError> {
        self.require_ready().await?;

        let cname = container_name(name);
        let info = self
            .docker()?
            .inspect_container(&cname, None)
            .await
            .map_err(|_| AppsError::AppNotFound(name.to_string()))?;

        let config = info.config.unwrap_or_default();
        let host_config = info.host_config.unwrap_or_default();
        let network_ports = info
            .network_settings
            .and_then(|ns| ns.ports)
            .unwrap_or_default();

        // Image
        let image = config.image.unwrap_or_default();

        // Parse ports — prefer network_settings.ports (has actual runtime mappings)
        // over host_config.port_bindings (may have None for auto-assigned ports)
        let mut ports = Vec::new();
        let port_source = if !network_ports.is_empty() {
            &network_ports
        } else {
            host_config.port_bindings.as_ref().unwrap_or(&network_ports)
        };
        for (idx, (key, bindings)) in port_source.iter().enumerate() {
            let parts: Vec<&str> = key.split('/').collect();
            let container_port: u16 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let protocol = parts
                .get(1)
                .map(|p| p.to_uppercase())
                .unwrap_or_else(|| "TCP".to_string());
            let host_port = bindings
                .as_ref()
                .and_then(|b| b.first())
                .and_then(|b| b.host_port.as_ref())
                .and_then(|p| p.parse::<u16>().ok());
            let port_name = if idx == 0 {
                "http".to_string()
            } else {
                format!("port-{idx}")
            };
            ports.push(AppPort {
                name: port_name,
                container_port,
                host_port,
                protocol,
            });
        }
        ports.sort_by_key(|p| p.container_port);

        // Parse env
        let env: Vec<AppEnv> = config
            .env
            .unwrap_or_default()
            .iter()
            .filter_map(|e| {
                let (k, v) = e.split_once('=')?;
                Some(AppEnv {
                    name: k.to_string(),
                    value: v.to_string(),
                })
            })
            .collect();

        // Parse volumes from binds
        let mut volumes = Vec::new();
        if let Some(ref binds) = host_config.binds {
            for (i, bind) in binds.iter().enumerate() {
                let parts: Vec<&str> = bind.splitn(3, ':').collect();
                if parts.len() >= 2 {
                    let host_path = parts[0].to_string();
                    let mount_path = parts[1].to_string();
                    let vol_name = Path::new(&host_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&format!("vol-{i}"))
                        .to_string();
                    volumes.push(AppVolume {
                        name: vol_name,
                        mount_path,
                        host_path,
                    });
                }
            }
        }

        // Resource limits
        let cpu_limit = host_config
            .nano_cpus
            .map(|n| format!("{:.1}", n as f64 / 1_000_000_000.0));
        let memory_limit = host_config.memory.and_then(|m| {
            if m <= 0 {
                None
            } else {
                Some(format_memory_limit(m as u64))
            }
        });

        let allow_unsafe = config
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_APP_UNSAFE))
            .map(|v| v == "true")
            .unwrap_or(false);

        Ok(AppConfig {
            name: name.to_string(),
            image,
            ports,
            env,
            volumes,
            cpu_limit,
            memory_limit,
            allow_unsafe,
        })
    }

    pub async fn logs(&self, name: &str, tail: Option<u32>) -> Result<String, AppsError> {
        self.require_ready().await?;

        let cname = container_name(name);
        let tail_str = tail.unwrap_or(100).to_string();

        let logs = self
            .docker()?
            .logs(
                &cname,
                Some(LogsOptions {
                    stdout: true,
                    stderr: true,
                    tail: tail_str,
                    ..Default::default()
                }),
            )
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| AppsError::AppNotFound(name.to_string()))?;

        let output: String = logs
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("");
        Ok(output)
    }

    /// Get logs for a specific container by ID or name (no nasty- prefix).
    pub async fn container_logs(
        &self,
        container_id: &str,
        tail: Option<u32>,
    ) -> Result<String, AppsError> {
        self.require_ready().await?;

        let tail_str = tail.unwrap_or(100).to_string();
        let logs = self
            .docker()?
            .logs(
                container_id,
                Some(LogsOptions {
                    stdout: true,
                    stderr: true,
                    tail: tail_str,
                    ..Default::default()
                }),
            )
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| AppsError::AppNotFound(container_id.to_string()))?;

        let output: String = logs
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("");
        Ok(output)
    }

    // ── Stop / Start ────────────────────────────────────────

    pub async fn stop(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        // Check if it's a compose app
        let compose_file = format!("{}/{}/docker-compose.yml", COMPOSE_DIR, name);
        if Path::new(&compose_file).exists() {
            let output = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "stop",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(AppsError::DockerFailed(stderr.to_string()));
            }
        } else {
            let cname = container_name(name);
            if !self.container_exists(&cname).await {
                return Err(AppsError::AppNotFound(name.to_string()));
            }
            self.docker()?
                .stop_container(
                    &cname,
                    Some(StopContainerOptions {
                        t: Some(10),
                        signal: None,
                    }),
                )
                .await?;
        }

        info!("Stopped app '{name}'");
        Ok(())
    }

    pub async fn start(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        // Check if it's a compose app
        let compose_file = format!("{}/{}/docker-compose.yml", COMPOSE_DIR, name);
        if Path::new(&compose_file).exists() {
            let output = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "start",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(AppsError::DockerFailed(stderr.to_string()));
            }
        } else {
            let cname = container_name(name);
            if !self.container_exists(&cname).await {
                return Err(AppsError::AppNotFound(name.to_string()));
            }
            self.docker()?
                .start_container(
                    &cname,
                    None::<bollard::query_parameters::StartContainerOptions>,
                )
                .await?;
        }

        info!("Started app '{name}'");
        Ok(())
    }

    // ── Compose app management ──────────────────────────────

    pub async fn compose_install(&self, req: InstallComposeRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        let project_dir = format!("{}/{}", COMPOSE_DIR, req.name);

        // Check if already exists
        if Path::new(&project_dir).join("docker-compose.yml").exists() {
            return Err(AppsError::AppAlreadyExists(req.name));
        }

        // Write compose file
        tokio::fs::create_dir_all(&project_dir).await?;
        tokio::fs::write(
            format!("{}/docker-compose.yml", project_dir),
            &req.compose_file,
        )
        .await?;

        // Write a .env file with project name
        let env_content = format!("COMPOSE_PROJECT_NAME={name}\n", name = req.name,);
        tokio::fs::write(format!("{}/.env", project_dir), &env_content).await?;

        // Validate compose file before deploying
        let compose_path = format!("{}/docker-compose.yml", project_dir);
        if let Err(e) = Self::validate_compose(&compose_path).await {
            let _ = tokio::fs::remove_dir_all(&project_dir).await;
            return Err(e);
        }

        // Pre-create missing bind-mount source dirs with the service's
        // expected ownership. Existing dirs are left alone — they go
        // through the explicit "Fix permissions" path.
        self.precreate_compose_binds(&req.compose_file).await;

        // Run docker compose up — pull only, no building from source
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &format!("{}/docker-compose.yml", project_dir),
                    "--project-name",
                    &req.name,
                    "up",
                    "-d",
                    "--no-build",
                    "--pull",
                    "missing",
                ])
                .output(),
        )
        .await;

        let compose_path = format!("{}/docker-compose.yml", project_dir);
        let cleanup = |project_dir: String, name: String, compose_path: String| async move {
            // Tear down any partially created containers before removing the dir
            let _ = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_path,
                    "--project-name",
                    &name,
                    "down",
                    "-v",
                    "--remove-orphans",
                ])
                .output()
                .await;
            let _ = tokio::fs::remove_dir_all(&project_dir).await;
        };

        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                cleanup(project_dir, req.name, compose_path).await;
                return Err(AppsError::CommandFailed(e.to_string()));
            }
            Err(_) => {
                cleanup(project_dir, req.name, compose_path).await;
                return Err(AppsError::DockerFailed(
                    "docker compose timed out after 5 minutes".to_string(),
                ));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            cleanup(project_dir, req.name, compose_path).await;
            return Err(AppsError::DockerFailed(stderr.to_string()));
        }

        // Auto-create ingress for the first exposed TCP port. See the
        // matching comment in `install()` for why UDP is skipped.
        if let Ok(app) = self.get(&req.name).await
            && let Some(first_port) = app
                .ports
                .iter()
                .find(|p| p.protocol.eq_ignore_ascii_case("tcp"))
        {
            let _ = self
                .ingress_set(SetIngressRequest {
                    name: req.name.clone(),
                    host_port: first_port.host_port,
                })
                .await;
        }

        info!("Installed compose app '{}'", req.name);
        self.get(&req.name).await
    }

    pub async fn compose_update(&self, req: InstallComposeRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        let project_dir = format!("{}/{}", COMPOSE_DIR, req.name);
        if !Path::new(&project_dir).join("docker-compose.yml").exists() {
            return Err(AppsError::AppNotFound(req.name));
        }

        // Overwrite compose file
        tokio::fs::write(
            format!("{}/docker-compose.yml", project_dir),
            &req.compose_file,
        )
        .await?;

        // Same pre-create pass as install: any newly added bind-mount
        // sources get created with the right ownership. Existing dirs
        // are untouched.
        self.precreate_compose_binds(&req.compose_file).await;

        // Bring up with new config — pull only, no building from source
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &format!("{}/docker-compose.yml", project_dir),
                    "--project-name",
                    &req.name,
                    "up",
                    "-d",
                    "--no-build",
                    "--pull",
                    "missing",
                    "--remove-orphans",
                ])
                .output(),
        )
        .await;

        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(AppsError::CommandFailed(e.to_string())),
            Err(_) => {
                return Err(AppsError::DockerFailed(
                    "docker compose timed out after 5 minutes".to_string(),
                ));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::DockerFailed(stderr.to_string()));
        }

        info!("Updated compose app '{}'", req.name);
        self.get(&req.name).await
    }

    pub async fn compose_remove(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        let project_dir = format!("{}/{}", COMPOSE_DIR, name);
        let compose_file = format!("{}/docker-compose.yml", project_dir);

        if Path::new(&compose_file).exists() {
            let output = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "down",
                    "-v",
                    "--remove-orphans",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(AppsError::DockerFailed(stderr.to_string()));
            }

            let _ = tokio::fs::remove_dir_all(&project_dir).await;
        } else {
            return Err(AppsError::AppNotFound(name.to_string()));
        }

        let _ = self.ingress_remove(name).await;

        info!("Removed compose app '{name}'");
        Ok(())
    }

    pub async fn compose_get(&self, name: &str) -> Result<String, AppsError> {
        let path = format!("{}/{}/docker-compose.yml", COMPOSE_DIR, name);
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|_| AppsError::AppNotFound(name.to_string()))
    }

    pub async fn compose_logs(&self, name: &str, tail: Option<u32>) -> Result<String, AppsError> {
        self.require_ready().await?;

        let project_dir = format!("{}/{}", COMPOSE_DIR, name);
        let compose_file = format!("{}/docker-compose.yml", project_dir);

        if !Path::new(&compose_file).exists() {
            return Err(AppsError::AppNotFound(name.to_string()));
        }

        let tail_str = tail.unwrap_or(100).to_string();
        let output = Command::new("docker")
            .args([
                "compose",
                "-f",
                &compose_file,
                "--project-name",
                name,
                "logs",
                "--tail",
                &tail_str,
                "--no-color",
            ])
            .output()
            .await
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ── Ingress management ──────────────────────────────────

    pub async fn ingress_list(&self) -> Result<Vec<AppIngress>, AppsError> {
        let content = tokio::fs::read_to_string(PROXY_CONF)
            .await
            .unwrap_or_default();
        let mut rules = Vec::new();
        for line in content.lines() {
            if let Some(comment) = line.strip_prefix("# app:") {
                let parts: Vec<&str> = comment.split_whitespace().collect();
                if parts.len() >= 2 {
                    let name = parts[0].to_string();
                    if let Some(port_str) = parts[1].strip_prefix("port:")
                        && let Ok(port) = port_str.parse::<u16>()
                    {
                        rules.push(AppIngress {
                            path: format!("/apps/{name}/"),
                            name,
                            host_port: port,
                        });
                    }
                }
            }
        }
        Ok(rules)
    }

    pub async fn ingress_set(&self, req: SetIngressRequest) -> Result<AppIngress, AppsError> {
        let mut rules = self.ingress_list().await?;
        rules.retain(|r| r.name != req.name);

        rules.push(AppIngress {
            name: req.name.clone(),
            host_port: req.host_port,
            path: format!("/apps/{}/", req.name),
        });

        self.write_proxy_conf(&rules).await?;
        reload_nginx().await;

        info!("Ingress set for '{}' -> port {}", req.name, req.host_port);
        Ok(rules.into_iter().find(|r| r.name == req.name).unwrap())
    }

    pub async fn ingress_remove(&self, name: &str) -> Result<(), AppsError> {
        let mut rules = self.ingress_list().await?;
        let before = rules.len();
        rules.retain(|r| r.name != name);

        if rules.len() == before {
            return Err(AppsError::AppNotFound(name.to_string()));
        }

        self.write_proxy_conf(&rules).await?;
        reload_nginx().await;

        info!("Ingress removed for '{name}'");
        Ok(())
    }

    // ── Port conflict checking ─────────────────────────────

    pub async fn check_ports(&self, req: CheckPortsRequest) -> Vec<PortConflict> {
        let mut conflicts = Vec::new();

        // Check against other managed apps
        if let Ok(apps) = self.list_internal().await {
            for app in &apps {
                // Skip the app being updated
                if req.exclude_app.as_deref() == Some(&app.name) {
                    continue;
                }
                for p in &app.ports {
                    if req.ports.contains(&p.host_port) {
                        conflicts.push(PortConflict {
                            port: p.host_port,
                            used_by: format!("app:{}", app.name),
                        });
                    }
                }
            }
        }

        // Check against system listeners via ss
        if let Ok(listeners) = system_listeners().await {
            for (port, process) in &listeners {
                if req.ports.contains(port) {
                    // Don't double-report ports already flagged as app conflicts
                    if !conflicts.iter().any(|c| c.port == *port) {
                        conflicts.push(PortConflict {
                            port: *port,
                            used_by: process.clone(),
                        });
                    }
                }
            }
        }

        conflicts
    }

    // ── Device existence check ────────────────────────────────────

    /// Stat each requested host path; report the ones that don't exist.
    /// Cheap enough to call on every keystroke (it's just `stat(2)` per
    /// path, no Docker round-trip), so the WebUI debounces it the same
    /// way as `check_ports`. Errors other than ENOENT (permission
    /// denied, etc.) are treated as "exists" — we'd rather miss a
    /// warning than block a legitimate deploy because of a stat hiccup.
    pub async fn check_devices(&self, req: CheckDevicesRequest) -> Vec<DeviceMissing> {
        let mut missing = Vec::new();
        for raw in &req.paths {
            let path = raw.trim();
            if path.is_empty() {
                continue;
            }
            match tokio::fs::metadata(path).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let parent_exists = match std::path::Path::new(path).parent() {
                        Some(p) if !p.as_os_str().is_empty() => {
                            tokio::fs::metadata(p).await.is_ok()
                        }
                        _ => false,
                    };
                    missing.push(DeviceMissing {
                        path: raw.clone(),
                        parent_exists,
                    });
                }
                Err(_) => {}
            }
        }
        missing
    }

    // ── Volume permission check ───────────────────────────────────

    /// Pre-deploy check: parse the compose, find every bind-mount whose
    /// host owner doesn't match its service's `user:` (or PUID/PGID env
    /// fallback), and report missing-source paths so the UI can show
    /// they'll be created with the right ownership.
    pub async fn check_volumes(&self, req: CheckVolumesRequest) -> Vec<VolumeMismatch> {
        use std::os::unix::fs::MetadataExt;
        let mut out = Vec::new();
        for bind in extract_compose_binds(&req.compose) {
            // No `user:` and no PUID — the container will run as root,
            // and root can write anywhere on the host filesystem so
            // there's nothing for us to flag.
            let expected_uid = match bind.expected_uid {
                Some(u) => u,
                None => continue,
            };
            // Engine-state / `..` / root binds are admin-policy
            // violations the user can't fix from the wizard — silently
            // skip those. Filesystem-missing failures (`/fs/<X>/…`
            // where `<X>` isn't mounted) ARE user-fixable and get
            // surfaced as a distinct mismatch below.
            if validate_chown_target_security(&bind.host_path).is_err() {
                continue;
            }
            let line = find_line(&req.compose, &bind.host_path);
            if validate_fs_root_mounted(&bind.host_path).is_err() {
                // Don't even stat() — the path's prefix is invalid and
                // would normally turn into a rootfs mkdir at deploy
                // time. Flag it as filesystem_missing so the WebUI
                // shows a hard "fix your compose" message instead of
                // the friendly "will be created" hint.
                out.push(VolumeMismatch {
                    service: bind.service,
                    host_path: bind.host_path,
                    mount_path: bind.mount_path,
                    expected_uid,
                    expected_gid: bind.expected_gid,
                    current_uid: None,
                    current_gid: None,
                    exists: false,
                    filesystem_missing: true,
                    line,
                });
                continue;
            }
            match tokio::fs::metadata(&bind.host_path).await {
                Ok(md) => {
                    let cur_uid = md.uid();
                    let cur_gid = md.gid();
                    let uid_mismatch = cur_uid != expected_uid;
                    let gid_mismatch = bind.expected_gid.is_some_and(|g| g != cur_gid);
                    if uid_mismatch || gid_mismatch {
                        out.push(VolumeMismatch {
                            service: bind.service,
                            host_path: bind.host_path,
                            mount_path: bind.mount_path,
                            expected_uid,
                            expected_gid: bind.expected_gid,
                            current_uid: Some(cur_uid),
                            current_gid: Some(cur_gid),
                            exists: true,
                            filesystem_missing: false,
                            line,
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Missing dir — the install pipeline will create
                    // and chown it. Surface as "will be created" so
                    // the user understands what'll happen.
                    out.push(VolumeMismatch {
                        service: bind.service,
                        host_path: bind.host_path,
                        mount_path: bind.mount_path,
                        expected_uid,
                        expected_gid: bind.expected_gid,
                        current_uid: None,
                        current_gid: None,
                        exists: false,
                        filesystem_missing: false,
                        line,
                    });
                }
                Err(_) => {}
            }
        }
        out
    }

    /// Chown a bind-mount source to (uid, gid). Optionally recursive.
    /// Path is validated against the same forbidden-bind rules as the
    /// deploy pipeline, so a malicious WebUI session can't wipe `/etc`
    /// ownership through this entrypoint.
    pub async fn fix_volume_perms(&self, req: FixVolumePermsRequest) -> Result<(), AppsError> {
        validate_chown_target(&req.host_path)?;
        // If the target doesn't exist, create it. Mode 0o755 mirrors
        // what `mkdir -p` produces; the chown below sets ownership.
        if tokio::fs::metadata(&req.host_path).await.is_err() {
            tokio::fs::create_dir_all(&req.host_path)
                .await
                .map_err(|e| {
                    AppsError::CommandFailed(format!("create_dir_all({}): {e}", req.host_path))
                })?;
        }
        let owner = format!("{}:{}", req.uid, req.gid);
        let mut cmd = Command::new("chown");
        if req.recursive {
            cmd.arg("-R");
        }
        cmd.arg(&owner).arg(&req.host_path);
        let output = cmd
            .output()
            .await
            .map_err(|e| AppsError::CommandFailed(format!("chown: {e}")))?;
        if !output.status.success() {
            return Err(AppsError::CommandFailed(format!(
                "chown {} {}: {}",
                owner,
                req.host_path,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        info!(
            "Chowned {} to {} (recursive={})",
            req.host_path, owner, req.recursive
        );
        Ok(())
    }

    /// Pre-create missing bind-mount source dirs and chown them to the
    /// service's expected user. Called from `compose_install` /
    /// `compose_update` right before `docker compose up`. Existing dirs
    /// are left alone — the explicit "Fix permissions" button is the
    /// path for those, so we never silently chown a tree the user
    /// already populated.
    async fn precreate_compose_binds(&self, yaml: &str) {
        for bind in extract_compose_binds(yaml) {
            if validate_chown_target(&bind.host_path).is_err() {
                continue;
            }
            // Skip dirs that already exist — auto-chown is "B" only.
            if tokio::fs::metadata(&bind.host_path).await.is_ok() {
                continue;
            }
            if let Err(e) = tokio::fs::create_dir_all(&bind.host_path).await {
                warn!("Failed to pre-create bind source '{}': {e}", bind.host_path);
                continue;
            }
            let (Some(uid), gid_opt) = (bind.expected_uid, bind.expected_gid) else {
                continue;
            };
            // chown to uid:gid (gid defaults to uid when compose only
            // specifies `user: 1000` — that's what Docker does, too).
            let gid = gid_opt.unwrap_or(uid);
            let owner = format!("{uid}:{gid}");
            let res = Command::new("chown")
                .arg(&owner)
                .arg(&bind.host_path)
                .output()
                .await;
            match res {
                Ok(o) if o.status.success() => {
                    info!(
                        "Pre-created '{}' owned by {} for service '{}'",
                        bind.host_path, owner, bind.service
                    );
                }
                Ok(o) => warn!(
                    "Pre-created '{}' but chown to {} failed: {}",
                    bind.host_path,
                    owner,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => warn!("chown {} {} failed: {e}", owner, bind.host_path),
            }
        }
    }

    // ── Image inspection ────────────────────────────────────

    pub async fn inspect_image(&self, image: &str) -> Result<ImageInspectResult, AppsError> {
        let ports = inspect_image_ports(image)
            .await
            .map_err(|e| AppsError::CommandFailed(format!("image inspect failed: {e}")))?;
        Ok(ImageInspectResult { ports })
    }

    // ── Restore on boot ─────────────────────────────────────

    pub async fn restore(&self) {
        if !self.is_enabled() {
            return;
        }
        info!("Apps runtime enabled — ensuring Docker is running");
        if let Err(e) = run_cmd("systemctl", &["start", DOCKER_SERVICE]).await {
            error!("Failed to start Docker: {e}");
            return;
        }

        // Bring up compose apps (their containers may not have restart:always)
        if let Ok(mut entries) = tokio::fs::read_dir(COMPOSE_DIR).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let compose_file = entry.path().join("docker-compose.yml");
                if !compose_file.exists() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let path = compose_file.to_string_lossy().to_string();
                info!("Restoring compose app '{name}'");
                let _ = Command::new("docker")
                    .args([
                        "compose",
                        "-f",
                        &path,
                        "--project-name",
                        &name,
                        "up",
                        "-d",
                        "--no-build",
                    ])
                    .output()
                    .await;
            }
        }
    }

    // ── Internal helpers ────────────────────────────────────

    async fn is_docker_ready(&self) -> bool {
        match self.docker() {
            Ok(d) => d.ping().await.is_ok(),
            Err(_) => false,
        }
    }

    async fn require_ready(&self) -> Result<(), AppsError> {
        if !self.is_enabled() {
            return Err(AppsError::NotEnabled);
        }
        if !self.is_docker_ready().await {
            return Err(AppsError::NotReady("Docker not responding".to_string()));
        }
        Ok(())
    }

    async fn docker_version(&self) -> Option<String> {
        let version = self.docker().ok()?.version().await.ok()?;
        version.version
    }

    async fn container_exists(&self, name: &str) -> bool {
        match self.docker() {
            Ok(d) => d.inspect_container(name, None).await.is_ok(),
            Err(_) => false,
        }
    }

    /// Collect all host ports currently in use by managed containers.
    async fn used_host_ports(&self) -> std::collections::HashSet<u16> {
        let mut used = std::collections::HashSet::new();
        if let Ok(apps) = self.list_internal().await {
            for app in &apps {
                for p in &app.ports {
                    used.insert(p.host_port);
                }
            }
        }
        used
    }

    async fn pull_image(&self, image: &str) -> Result<(), AppsError> {
        let (from_image, tag) = if let Some((img, tag)) = image.rsplit_once(':') {
            (img.to_string(), tag.to_string())
        } else {
            (image.to_string(), "latest".to_string())
        };

        let options = CreateImageOptions {
            from_image: Some(from_image.clone()),
            tag: Some(tag.clone()),
            ..Default::default()
        };

        self.docker()?
            .create_image(Some(options), None, None)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
    }

    /// Look up the host port Docker actually assigned for a given container port.
    async fn get_mapped_port(&self, container: &str, container_port: u16) -> Option<u16> {
        let info = self
            .docker()
            .ok()?
            .inspect_container(container, None)
            .await
            .ok()?;
        let ports = info.network_settings?.ports?;
        let key = format!("{container_port}/tcp");
        let bindings = ports.get(&key)?.as_ref()?;
        bindings.first()?.host_port.as_ref()?.parse::<u16>().ok()
    }

    /// Total memory usage of Docker (daemon + containers), excluding page cache.
    /// Reads anon + kernel from cgroup memory.stat to avoid counting reclaimable
    /// filesystem cache that inflates MemoryCurrent.
    async fn total_memory_usage(&self) -> Option<u64> {
        let stat_path = "/sys/fs/cgroup/system.slice/docker.service/memory.stat";
        let content = tokio::fs::read_to_string(stat_path).await.ok()?;

        let mut anon: u64 = 0;
        let mut kernel: u64 = 0;
        for line in content.lines() {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some("anon"), Some(v)) => anon = v.parse().unwrap_or(0),
                (Some("kernel"), Some(v)) => kernel = v.parse().unwrap_or(0),
                _ => {}
            }
        }

        let total = anon + kernel;
        if total > 0 { Some(total) } else { None }
    }

    /// Total Docker disk usage (images + containers + volumes).
    async fn docker_disk_usage(&self) -> Option<u64> {
        let df = self
            .docker()
            .ok()?
            .df(None::<bollard::query_parameters::DataUsageOptions>)
            .await
            .ok()?;
        let mut total: u64 = 0;
        if let Some(ref images) = df.image_usage {
            total += images.total_size.unwrap_or(0) as u64;
        }
        if let Some(ref volumes) = df.volume_usage {
            total += volumes.total_size.unwrap_or(0) as u64;
        }
        Some(total)
    }

    // ── Restart ──────────────────────────────────────────────

    pub async fn restart(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        let compose_file = format!("{}/{}/docker-compose.yml", COMPOSE_DIR, name);
        if Path::new(&compose_file).exists() {
            let output = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "restart",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(AppsError::DockerFailed(stderr.to_string()));
            }
        } else {
            let cname = container_name(name);
            if !self.container_exists(&cname).await {
                return Err(AppsError::AppNotFound(name.to_string()));
            }
            self.docker()?
                .restart_container(
                    &cname,
                    Some(bollard::query_parameters::RestartContainerOptions {
                        t: Some(10),
                        signal: None,
                    }),
                )
                .await?;
        }

        info!("Restarted app '{name}'");
        Ok(())
    }

    // ── Pull (update image) ─────────────────────────────────

    pub async fn pull(&self, name: &str) -> Result<App, AppsError> {
        self.require_ready().await?;

        let compose_file = format!("{}/{}/docker-compose.yml", COMPOSE_DIR, name);
        if Path::new(&compose_file).exists() {
            // docker compose pull + up -d (recreates with new images)
            let pull = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "pull",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
            if !pull.status.success() {
                let stderr = String::from_utf8_lossy(&pull.stderr);
                return Err(AppsError::DockerFailed(stderr.to_string()));
            }

            let up = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "up",
                    "-d",
                    "--no-build",
                    "--remove-orphans",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
            if !up.status.success() {
                let stderr = String::from_utf8_lossy(&up.stderr);
                return Err(AppsError::DockerFailed(stderr.to_string()));
            }

            info!("Pulled latest images for compose app '{name}'");
        } else {
            let cname = container_name(name);
            let info = self
                .docker()?
                .inspect_container(&cname, None)
                .await
                .map_err(|_| AppsError::AppNotFound(name.to_string()))?;
            let image = info.config.and_then(|c| c.image).unwrap_or_default();
            if image.is_empty() {
                return Err(AppsError::DockerFailed(
                    "container has no image".to_string(),
                ));
            }

            // Pull latest
            self.pull_image(&image).await?;

            // Recreate container with same config but new image
            // Stop + remove + start from the pulled image
            let _ = self
                .docker()?
                .stop_container(
                    &cname,
                    Some(StopContainerOptions {
                        t: Some(10),
                        signal: None,
                    }),
                )
                .await;
            // We need the full config to recreate — get_config then re-install
            let config = self.get_config(name).await?;
            let _ = self
                .docker()?
                .remove_container(
                    &cname,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;

            let req = InstallAppRequest {
                name: name.to_string(),
                image,
                ports: config.ports,
                env: config.env,
                volumes: config.volumes,
                cpu_limit: config.cpu_limit,
                memory_limit: config.memory_limit,
                allow_unsafe: config.allow_unsafe,
            };
            return self.install(req).await;
        }

        self.get(name).await
    }

    // ── Prune ───────────────────────────────────────────────

    pub async fn prune(&self) -> Result<PruneResult, AppsError> {
        self.require_ready().await?;

        let result = self
            .docker()?
            .prune_images(None::<bollard::query_parameters::PruneImagesOptions>)
            .await?;
        let images_removed = result.images_deleted.map(|v| v.len()).unwrap_or(0);
        let space_reclaimed = result.space_reclaimed.unwrap_or(0) as u64;

        // Also prune volumes
        let _ = self
            .docker()?
            .prune_volumes(None::<bollard::query_parameters::PruneVolumesOptions>)
            .await;

        info!(
            "Pruned {images_removed} images, reclaimed {} bytes",
            space_reclaimed
        );
        Ok(PruneResult {
            images_removed,
            space_reclaimed_bytes: space_reclaimed,
        })
    }

    // ── Compose validation ──────────────────────────────────

    async fn validate_compose(compose_file_path: &str) -> Result<(), AppsError> {
        let output = Command::new("docker")
            .args(["compose", "-f", compose_file_path, "config", "--quiet"])
            .output()
            .await
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::DockerFailed(format!(
                "invalid compose file: {stderr}"
            )));
        }
        Ok(())
    }

    // ── Container exec ──────────────────────────────────────

    /// Return the docker exec command string for a given app.
    /// The WebUI can use this to pre-fill the Terminal page.
    pub async fn exec_command(&self, name: &str) -> Result<String, AppsError> {
        let compose_file = format!("{}/{}/docker-compose.yml", COMPOSE_DIR, name);
        let container = if Path::new(&compose_file).exists() {
            // Look up the first running container in the compose project
            let output = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file,
                    "--project-name",
                    name,
                    "ps",
                    "-q",
                ])
                .output()
                .await
                .map_err(|e| AppsError::CommandFailed(e.to_string()))?;
            let id = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                return Err(AppsError::DockerFailed(
                    "no running containers in this app".to_string(),
                ));
            }
            id
        } else {
            container_name(name)
        };

        // Probe for an available shell
        match find_container_shell(&container).await {
            Some(shell) => Ok(format!("docker exec -it {} {}", container, shell)),
            None => Err(AppsError::DockerFailed(
                "this container has no shell (scratch/distroless image)".to_string(),
            )),
        }
    }

    async fn write_proxy_conf(&self, rules: &[AppIngress]) -> Result<(), AppsError> {
        let mut conf = String::from("# Auto-generated by NASty engine — do not edit\n");
        for rule in rules {
            conf.push_str(&format!(
                "# app:{} port:{}\nlocation /apps/{}/ {{\n\
                 \x20   proxy_pass http://127.0.0.1:{}/;\n\
                 \x20   proxy_set_header Host $host;\n\
                 \x20   proxy_set_header X-Real-IP $remote_addr;\n\
                 \x20   proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n\
                 \x20   proxy_set_header X-Forwarded-Proto $scheme;\n\
                 \x20   proxy_http_version 1.1;\n\
                 \x20   proxy_set_header Upgrade $http_upgrade;\n\
                 \x20   proxy_set_header Connection \"upgrade\";\n\
                 }}\n\n",
                rule.name, rule.host_port, rule.name, rule.host_port
            ));
        }
        tokio::fs::write(PROXY_CONF, &conf).await?;
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// Container name for simple apps: "nasty-{name}"
fn container_name(app_name: &str) -> String {
    format!("nasty-{app_name}")
}

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<(), AppsError> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| AppsError::CommandFailed(format!("{cmd}: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::CommandFailed(format!("{cmd}: {stderr}")));
    }
    Ok(())
}

/// Probe a running container for an available shell.
/// Returns None if the container has no shell (scratch/distroless images).
async fn find_container_shell(container: &str) -> Option<&'static str> {
    for shell in ["/bin/bash", "/bin/sh", "/bin/ash"] {
        let result = Command::new("docker")
            .args(["exec", container, "test", "-x", shell])
            .output()
            .await;
        if let Ok(output) = result
            && output.status.success()
        {
            return Some(match shell {
                "/bin/bash" => "/bin/bash",
                "/bin/ash" => "/bin/ash",
                _ => "/bin/sh",
            });
        }
    }
    None
}

async fn reload_nginx() {
    let _ = Command::new("systemctl")
        .args(["reload", "nginx"])
        .output()
        .await;
}

/// One row parsed from `ss -tlnp` output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SsListener {
    pub port: u16,
    pub process: String,
}

/// Parse a single non-header line of `ss -tlnp` output. The header
/// row and any line that doesn't carry a parseable `host:port` are
/// returned as `None`. Format example:
///
///   `LISTEN 0 4096 0.0.0.0:443 0.0.0.0:* users:(("nginx",pid=1753,fd=6))`
///
/// We name the columns we care about (local addr, users blob) rather
/// than accessing `fields[3]` and `fields[5]` positionally — that way
/// an `ss` column reorder in a future iproute2 release fails this
/// unit test instead of silently breaking port-conflict detection.
pub(crate) fn parse_ss_listener_line(line: &str) -> Option<SsListener> {
    let mut fields = line.split_whitespace();
    let _state = fields.next()?; // "LISTEN"
    let _recv_q = fields.next()?;
    let _send_q = fields.next()?;
    let local = fields.next()?; // local address:port
    let _peer = fields.next()?; // peer address:port

    // Local can be "0.0.0.0:443", "[::]:443", "*:443", "127.0.0.1:8080", etc.
    let port: u16 = local.rsplit(':').next()?.parse().ok()?;

    // Users blob looks like `users:(("nginx",pid=1753,fd=6))`. Some lines
    // have no `users:` column (e.g. when `ss` is run without enough
    // privilege to peek at PIDs); fall back to "unknown".
    let process = fields
        .next()
        .and_then(|users| users.split('"').nth(1))
        .unwrap_or("unknown")
        .to_string();

    Some(SsListener { port, process })
}

/// Query system TCP listeners via `ss -tlnp` and return (port, process_name) pairs.
async fn system_listeners() -> Result<Vec<(u16, String)>, AppsError> {
    let output = Command::new("ss")
        .args(["-tlnp"])
        .output()
        .await
        .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut listeners = Vec::new();

    for line in stdout.lines().skip(1) {
        let Some(l) = parse_ss_listener_line(line) else {
            continue;
        };
        // Deduplicate (IPv4 + IPv6 both show up)
        if !listeners.iter().any(|(p, _): &(u16, String)| *p == l.port) {
            listeners.push((l.port, l.process));
        }
    }

    Ok(listeners)
}

/// Parse CPU limit string to nanoseconds.
/// Accepts: "0.5" (half core), "2" (two cores), "500m" (millicores).
fn parse_cpu_limit(s: &str) -> Option<i64> {
    if let Some(millis) = s.strip_suffix('m') {
        let m: f64 = millis.parse().ok()?;
        Some((m * 1_000_000.0) as i64)
    } else {
        let cores: f64 = s.parse().ok()?;
        Some((cores * 1_000_000_000.0) as i64)
    }
}

/// Parse memory limit string to bytes.
/// Accepts: "256m", "1g", "512M", "2G", "1073741824" (raw bytes).
fn parse_memory_limit(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix(['g', 'G']) {
        (n.parse::<f64>().ok()?, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(n) = s.strip_suffix("Gi") {
        (n.parse::<f64>().ok()?, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(n) = s.strip_suffix(['m', 'M']) {
        (n.parse::<f64>().ok()?, 1024.0 * 1024.0)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n.parse::<f64>().ok()?, 1024.0 * 1024.0)
    } else {
        (s.parse::<f64>().ok()?, 1.0)
    };
    Some((num * mult) as i64)
}

/// Format bytes as a human-readable memory limit.
fn format_memory_limit(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 && bytes.is_multiple_of(1024 * 1024 * 1024) {
        format!("{}g", bytes / (1024 * 1024 * 1024))
    } else if bytes >= 1024 * 1024 {
        format!("{}m", bytes / (1024 * 1024))
    } else {
        format!("{bytes}")
    }
}

/// Convert a Unix timestamp (seconds) to a simple ISO 8601-ish string.
/// Extract host→container port mappings from a container summary.
fn extract_ports(c: &bollard::models::ContainerSummary) -> Vec<MappedPort> {
    let mut ports = Vec::new();
    if let Some(ref p) = c.ports {
        for port in p {
            if let (Some(public), Some(_)) = (port.public_port, Some(port.private_port)) {
                ports.push(MappedPort {
                    host_port: public,
                    container_port: port.private_port,
                    protocol: port
                        .typ
                        .as_ref()
                        .map(|t| format!("{:?}", t).to_lowercase())
                        .unwrap_or_else(|| "tcp".to_string()),
                });
            }
        }
    }
    ports.sort_by_key(|p| p.host_port);
    ports.dedup_by_key(|p| p.host_port);
    ports
}

fn container_status_str(c: &bollard::models::ContainerSummary) -> String {
    c.state
        .as_ref()
        .map(|s| format!("{:?}", s).to_lowercase())
        .unwrap_or_else(|| "unknown".to_string())
}

fn chrono_from_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    // Return seconds since epoch — the WebUI will format it
    format!("{ts}")
}

/// Create apps storage directory on bcachefs.
async fn setup_apps_storage(filesystem: Option<&str>) -> Option<String> {
    let fs_name = if let Some(name) = filesystem {
        let path = format!("/fs/{name}");
        if !Path::new(&path).is_dir() {
            error!("Specified filesystem '{name}' not found at {path}");
            return None;
        }
        name.to_string()
    } else {
        let fs_base = Path::new("/fs");
        let mut entries = match tokio::fs::read_dir(fs_base).await {
            Ok(e) => e,
            Err(_) => {
                error!("No /fs directory — cannot set up apps storage");
                return None;
            }
        };

        let mut found = None;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                found = Some(entry.file_name().to_string_lossy().to_string());
                break;
            }
        }

        match found {
            Some(n) => n,
            None => {
                error!("No filesystems found under /fs — cannot set up apps storage");
                return None;
            }
        }
    };

    let apps_path = format!("/fs/{fs_name}/apps");

    if Path::new(&apps_path).exists() {
        info!("Apps storage already exists at {apps_path}");
        return Some(apps_path);
    }

    match run_cmd("bcachefs", &["subvolume", "create", &apps_path]).await {
        Ok(()) => {
            info!("Created apps subvolume at {apps_path}");
            Some(apps_path)
        }
        Err(e) => {
            error!("Failed to create apps subvolume at {apps_path}: {e}");
            None
        }
    }
}

/// Configure Docker's data-root to store images/layers on bcachefs instead of root partition.
/// Ensure Docker stores data on bcachefs by symlinking /var/lib/docker.
/// Must be called before Docker starts — Docker reads data-root at startup.
async fn configure_docker_data_root(filesystem: Option<&str>) -> Result<(), AppsError> {
    let fs_name = if let Some(name) = filesystem {
        name.to_string()
    } else {
        let fs_base = Path::new("/fs");
        let mut entries = tokio::fs::read_dir(fs_base)
            .await
            .map_err(|e| AppsError::CommandFailed(format!("cannot read /fs: {e}")))?;
        let mut found = None;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                found = Some(entry.file_name().to_string_lossy().to_string());
                break;
            }
        }
        found.ok_or_else(|| AppsError::CommandFailed("no filesystems under /fs".into()))?
    };

    // Ensure the apps subvolume exists first
    let apps_path = format!("/fs/{fs_name}/apps");
    if !Path::new(&apps_path).exists() {
        run_cmd("bcachefs", &["subvolume", "create", &apps_path])
            .await
            .map_err(|e| AppsError::CommandFailed(format!("create apps subvolume: {e}")))?;
        info!("Created apps subvolume at {apps_path}");
    }

    let docker_data = format!("{apps_path}/docker");
    tokio::fs::create_dir_all(&docker_data)
        .await
        .map_err(|e| AppsError::CommandFailed(format!("create {docker_data}: {e}")))?;

    let docker_lib = Path::new("/var/lib/docker");

    // If /var/lib/docker is already a symlink to the right place, nothing to do
    if let Ok(target) = tokio::fs::read_link(docker_lib).await
        && target.to_string_lossy() == docker_data
    {
        info!("Docker data symlink already points to {docker_data}");
        return Ok(());
    }

    // Stop Docker if running (we need to move/replace its data dir)
    let _ = run_cmd("systemctl", &["stop", DOCKER_SERVICE]).await;

    // Remove existing /var/lib/docker (empty default dir or old data)
    if docker_lib.exists() {
        if docker_lib.is_symlink() {
            tokio::fs::remove_file(docker_lib)
                .await
                .map_err(|e| AppsError::CommandFailed(format!("remove old symlink: {e}")))?;
        } else {
            tokio::fs::remove_dir_all(docker_lib)
                .await
                .map_err(|e| AppsError::CommandFailed(format!("remove /var/lib/docker: {e}")))?;
        }
    }

    // Create symlink
    tokio::fs::symlink(&docker_data, docker_lib)
        .await
        .map_err(|e| {
            AppsError::CommandFailed(format!("symlink {docker_data} -> /var/lib/docker: {e}"))
        })?;

    info!("Symlinked /var/lib/docker -> {docker_data}");
    Ok(())
}

// ── Container image inspection ──────────────────────────────

fn parse_image_ref(image: &str) -> (String, String, String) {
    let (image_no_tag, tag) = if let Some((img, tag)) = image.rsplit_once(':') {
        (img.to_string(), tag.to_string())
    } else {
        (image.to_string(), "latest".to_string())
    };

    let parts: Vec<&str> = image_no_tag.splitn(2, '/').collect();
    if parts.len() == 1 {
        (
            "registry-1.docker.io".to_string(),
            format!("library/{}", parts[0]),
            tag,
        )
    } else if parts[0].contains('.') || parts[0].contains(':') {
        (parts[0].to_string(), parts[1].to_string(), tag)
    } else {
        ("registry-1.docker.io".to_string(), image_no_tag, tag)
    }
}

async fn inspect_image_ports(image: &str) -> Result<Vec<AppPort>, String> {
    let (registry, repo, tag) = parse_image_ref(image);
    let client = reqwest::Client::new();

    // Get auth token for Docker Hub
    let token = if registry == "registry-1.docker.io" {
        let token_url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            repo
        );
        let resp: serde_json::Value = client
            .get(&token_url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        resp["token"].as_str().map(String::from)
    } else {
        None
    };

    let registry_url = if registry.starts_with("http") {
        registry.clone()
    } else {
        format!("https://{registry}")
    };

    // Fetch manifest
    let manifest_url = format!("{registry_url}/v2/{repo}/manifests/{tag}");
    let mut req = client.get(&manifest_url).header(
        "Accept",
        "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
    );
    if let Some(ref t) = token {
        req = req.bearer_auth(t);
    }
    let manifest: serde_json::Value = req
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or("no config digest in manifest")?;

    // Fetch config blob
    let config_url = format!("{registry_url}/v2/{repo}/blobs/{config_digest}");
    let mut req = client.get(&config_url);
    if let Some(ref t) = token {
        req = req.bearer_auth(t);
    }
    let config: serde_json::Value = req
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    // Parse ExposedPorts
    let exposed = config["config"]["ExposedPorts"]
        .as_object()
        .or_else(|| config["container_config"]["ExposedPorts"].as_object());

    let mut ports = Vec::new();
    if let Some(exposed_ports) = exposed {
        for (key, _) in exposed_ports {
            let parts: Vec<&str> = key.split('/').collect();
            if let Some(port_str) = parts.first()
                && let Ok(port) = port_str.parse::<u16>()
            {
                let protocol = parts
                    .get(1)
                    .map(|p| p.to_uppercase())
                    .unwrap_or_else(|| "TCP".to_string());
                let name = if ports.is_empty() {
                    "http".to_string()
                } else {
                    format!("port-{}", ports.len())
                };
                ports.push(AppPort {
                    name,
                    container_port: port,
                    host_port: None,
                    protocol,
                });
            }
        }
    }

    ports.sort_by_key(|p| p.container_port);
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::{AppVolume, validate_simple_volumes};

    fn vol(host_path: &str) -> AppVolume {
        AppVolume {
            name: "v".to_string(),
            mount_path: "/data".to_string(),
            host_path: host_path.to_string(),
        }
    }

    #[test]
    fn empty_host_path_is_fine() {
        // Auto-generated path under storage base; install creates it.
        validate_simple_volumes("myapp", "/var/lib/nasty/apps-data", &[vol("")], false).unwrap();
    }

    #[test]
    fn strict_allows_app_data_dir_and_fs() {
        validate_simple_volumes(
            "myapp",
            "/var/lib/nasty/apps-data",
            &[vol("/var/lib/nasty/apps-data/myapp/cfg"), vol("/fs/photos")],
            false,
        )
        .unwrap();
    }

    #[test]
    fn strict_rejects_outside_allowlist() {
        let e = validate_simple_volumes(
            "myapp",
            "/var/lib/nasty/apps-data",
            &[vol("/home/user/data")],
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("/home/user/data"), "{e}");
    }

    #[test]
    fn strict_rejects_etc() {
        validate_simple_volumes("myapp", "/var/lib/nasty/apps-data", &[vol("/etc")], false)
            .unwrap_err();
    }

    #[test]
    fn unsafe_allows_arbitrary_paths() {
        validate_simple_volumes(
            "myapp",
            "/var/lib/nasty/apps-data",
            &[vol("/etc"), vol("/home/user/data"), vol("/dev/shm")],
            true,
        )
        .unwrap();
    }

    #[test]
    fn unsafe_still_rejects_root() {
        let e = validate_simple_volumes("myapp", "/var/lib/nasty/apps-data", &[vol("/")], true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("'/'"), "{e}");
    }

    #[test]
    fn unsafe_still_rejects_dotdot() {
        let e = validate_simple_volumes(
            "myapp",
            "/var/lib/nasty/apps-data",
            &[vol("/var/lib/nasty/apps-data/myapp/../auth.json")],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains(".."), "{e}");
    }

    #[test]
    fn unsafe_still_rejects_engine_state() {
        let e = validate_simple_volumes(
            "myapp",
            "/var/lib/nasty/apps-data",
            &[vol("/var/lib/nasty/auth.json")],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("engine state"), "{e}");
    }

    #[test]
    fn unsafe_still_allows_app_data_under_engine_state() {
        // app-data dir is /var/lib/nasty/apps-data/<name> and is the deliberate exception.
        validate_simple_volumes(
            "myapp",
            "/var/lib/nasty/apps-data",
            &[vol("/var/lib/nasty/apps-data/myapp/foo")],
            true,
        )
        .unwrap();
    }

    use super::{
        AppsService, CheckDevicesRequest, CheckVolumesRequest, extract_compose_binds,
        validate_chown_target,
    };

    #[tokio::test]
    async fn check_devices_reports_missing_with_parent_existing() {
        // /dev exists on every Linux host; the named device cannot.
        let svc = AppsService::new();
        let r = svc
            .check_devices(CheckDevicesRequest {
                paths: vec!["/dev/this-device-cannot-exist-nasty-test".to_string()],
            })
            .await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path, "/dev/this-device-cannot-exist-nasty-test");
        assert!(r[0].parent_exists, "/dev should exist");
    }

    #[tokio::test]
    async fn check_devices_flags_missing_parent() {
        let svc = AppsService::new();
        let r = svc
            .check_devices(CheckDevicesRequest {
                paths: vec!["/this-parent-cannot-exist-nasty-test/whatever".to_string()],
            })
            .await;
        assert_eq!(r.len(), 1);
        assert!(!r[0].parent_exists);
    }

    #[tokio::test]
    async fn check_devices_skips_existing() {
        // /dev/null is on every Linux host. Don't report it.
        let svc = AppsService::new();
        let r = svc
            .check_devices(CheckDevicesRequest {
                paths: vec!["/dev/null".to_string()],
            })
            .await;
        assert!(r.is_empty(), "expected empty, got {r:?}");
    }

    #[tokio::test]
    async fn check_devices_skips_blank_entries() {
        let svc = AppsService::new();
        let r = svc
            .check_devices(CheckDevicesRequest {
                paths: vec!["".to_string(), "   ".to_string()],
            })
            .await;
        assert!(r.is_empty());
    }

    // ── Compose bind parser ──

    #[test]
    fn extract_binds_short_form_with_user() {
        let yaml = r#"
services:
  jellyfin:
    image: jellyfin/jellyfin
    user: "3001:100"
    volumes:
      - /home/jellyfin:/config
      - /home/jellyfin/cache:/cache
"#;
        let binds = extract_compose_binds(yaml);
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].service, "jellyfin");
        assert_eq!(binds[0].host_path, "/home/jellyfin");
        assert_eq!(binds[0].mount_path, "/config");
        assert_eq!(binds[0].expected_uid, Some(3001));
        assert_eq!(binds[0].expected_gid, Some(100));
        assert_eq!(binds[1].host_path, "/home/jellyfin/cache");
    }

    #[test]
    fn extract_binds_long_form() {
        let yaml = r#"
services:
  app:
    image: foo
    user: "1000"
    volumes:
      - type: bind
        source: /fs/tank/media
        target: /media
"#;
        let binds = extract_compose_binds(yaml);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].host_path, "/fs/tank/media");
        assert_eq!(binds[0].mount_path, "/media");
        assert_eq!(binds[0].expected_uid, Some(1000));
        assert_eq!(binds[0].expected_gid, None);
    }

    #[test]
    fn extract_binds_skips_named_volume() {
        let yaml = r#"
services:
  db:
    image: postgres
    user: "1000:1000"
    volumes:
      - data:/var/lib/postgresql/data
volumes:
  data:
"#;
        let binds = extract_compose_binds(yaml);
        assert!(binds.is_empty(), "got {binds:?}");
    }

    #[test]
    fn extract_binds_falls_back_to_puid_pgid_list_form() {
        let yaml = r#"
services:
  jellyfin:
    image: lscr.io/linuxserver/jellyfin
    environment:
      - PUID=1000
      - PGID=1000
    volumes:
      - /home/jelly:/config
"#;
        let binds = extract_compose_binds(yaml);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].expected_uid, Some(1000));
        assert_eq!(binds[0].expected_gid, Some(1000));
    }

    #[test]
    fn extract_binds_falls_back_to_puid_pgid_map_form() {
        let yaml = r#"
services:
  jellyfin:
    image: lscr.io/linuxserver/jellyfin
    environment:
      PUID: 1000
      PGID: 1000
    volumes:
      - /home/jelly:/config
"#;
        let binds = extract_compose_binds(yaml);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].expected_uid, Some(1000));
        assert_eq!(binds[0].expected_gid, Some(1000));
    }

    #[test]
    fn extract_binds_skips_username_user_field() {
        // We don't resolve names server-side; treat as "no expected user".
        let yaml = r#"
services:
  app:
    image: foo
    user: "alice"
    volumes:
      - /home/alice:/data
"#;
        let binds = extract_compose_binds(yaml);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].expected_uid, None);
    }

    #[test]
    fn validate_chown_target_rejects_dangerous_paths() {
        for bad in [
            "/",
            "/etc/../passwd",
            "/var/lib/nasty",
            "/var/lib/nasty/auth.json",
            "relative/path",
        ] {
            assert!(
                validate_chown_target(bad).is_err(),
                "expected rejection for {bad}"
            );
        }
    }

    #[test]
    fn validate_chown_target_accepts_paths_outside_fs() {
        // Paths outside `/fs/` get past the FS-mounted check
        // unconditionally — the user opted into allow_unsafe territory
        // and we don't manage those mounts.
        for ok in ["/home/jellyfin", "/var/lib/foo"] {
            assert!(
                validate_chown_target(ok).is_ok(),
                "expected accept for {ok}"
            );
        }
    }

    #[test]
    fn fs_root_segment_extracts_first_path_segment() {
        assert_eq!(super::fs_root_segment("/fs/tank/media"), Some("tank"));
        assert_eq!(super::fs_root_segment("/fs/first"), Some("first"));
        assert_eq!(
            super::fs_root_segment("/fs/pool/subvol/nested/path"),
            Some("pool"),
        );
    }

    #[test]
    fn fs_root_segment_returns_none_outside_fs() {
        assert_eq!(super::fs_root_segment("/home/user"), None);
        assert_eq!(super::fs_root_segment("/etc"), None);
        assert_eq!(super::fs_root_segment("relative/path"), None);
    }

    #[test]
    fn fs_root_segment_returns_none_for_bare_fs() {
        // `/fs/` with nothing after isn't a valid bind source — we
        // never pre-create the root of /fs.
        assert_eq!(super::fs_root_segment("/fs/"), None);
        assert_eq!(super::fs_root_segment("/fs"), None);
    }

    #[test]
    fn validate_fs_root_mounted_passes_paths_outside_fs() {
        // Paths outside /fs aren't our concern — they're allow_unsafe
        // host paths, the user owns them.
        assert!(super::validate_fs_root_mounted("/home/jellyfin").is_ok());
        assert!(super::validate_fs_root_mounted("/var/lib/foo").is_ok());
    }

    #[test]
    fn validate_fs_root_mounted_rejects_unmounted_fs() {
        // No /fs/this-filesystem-does-not-exist-nasty-test on any
        // CI runner. The error message must name the missing fs so
        // the WebUI can show a clear hint.
        let e =
            super::validate_fs_root_mounted("/fs/this-filesystem-does-not-exist-nasty-test/media")
                .unwrap_err()
                .to_string();
        assert!(
            e.contains("this-filesystem-does-not-exist-nasty-test"),
            "error should name the missing fs: {e}",
        );
    }

    #[tokio::test]
    async fn check_volumes_returns_nothing_when_no_user_set() {
        // Without a `user:` field and no PUID/PGID, the container runs
        // as root and there's nothing to flag.
        let svc = AppsService::new();
        let r = svc
            .check_volumes(CheckVolumesRequest {
                compose: r#"
services:
  app:
    image: foo
    volumes:
      - /tmp:/data
"#
                .to_string(),
            })
            .await;
        assert!(r.is_empty(), "got {r:?}");
    }

    #[tokio::test]
    async fn check_volumes_flags_missing_source_dir() {
        // /tmp is outside /fs so the FS-mounted check passes and the
        // path falls into the normal "doesn't exist yet — will be
        // pre-created" branch.
        let svc = AppsService::new();
        let r = svc
            .check_volumes(CheckVolumesRequest {
                compose: r#"
services:
  app:
    image: foo
    user: "3001:100"
    volumes:
      - /tmp/this-source-does-not-exist-nasty-test:/data
"#
                .to_string(),
            })
            .await;
        assert_eq!(r.len(), 1);
        assert!(!r[0].exists);
        assert!(!r[0].filesystem_missing);
        assert_eq!(r[0].expected_uid, 3001);
    }

    #[tokio::test]
    async fn check_volumes_flags_filesystem_missing_for_unmounted_fs() {
        // `/fs/<X>/…` where `<X>` is not mounted: the original bug
        // would have led to `mkdir -p /fs/<X>/...` on rootfs during
        // pre-create. We surface this as a distinct mismatch with
        // filesystem_missing=true so the WebUI shows a "fix your
        // source path" warning instead of a friendly create hint.
        let svc = AppsService::new();
        let r = svc
            .check_volumes(CheckVolumesRequest {
                compose: r#"
services:
  app:
    image: foo
    user: "3001:100"
    volumes:
      - /fs/this-filesystem-does-not-exist-nasty-test/media:/media
"#
                .to_string(),
            })
            .await;
        assert_eq!(r.len(), 1);
        assert!(r[0].filesystem_missing);
        assert!(!r[0].exists);
    }

    // ── parse_ss_listener_line ──

    use super::{SsListener, parse_ss_listener_line};

    #[test]
    fn parse_ss_ipv4_with_process() {
        let l = parse_ss_listener_line(
            r#"LISTEN 0      4096   0.0.0.0:443 0.0.0.0:* users:(("nginx",pid=1753,fd=6))"#,
        )
        .expect("should parse");
        assert_eq!(
            l,
            SsListener {
                port: 443,
                process: "nginx".to_string()
            }
        );
    }

    #[test]
    fn parse_ss_ipv6_dual_bind() {
        // `[::]:443` is the IPv6-any form ss emits when a TCP socket is
        // bound on `::` and there's no explicit IPv4-only socket.
        let l = parse_ss_listener_line(
            r#"LISTEN 0      4096   [::]:80 [::]:* users:(("caddy",pid=42,fd=3))"#,
        )
        .expect("should parse");
        assert_eq!(l.port, 80);
        assert_eq!(l.process, "caddy");
    }

    #[test]
    fn parse_ss_wildcard_address() {
        let l = parse_ss_listener_line(r#"LISTEN 0 128 *:22 *:* users:(("sshd",pid=900,fd=4))"#)
            .expect("should parse");
        assert_eq!(l.port, 22);
        assert_eq!(l.process, "sshd");
    }

    #[test]
    fn parse_ss_without_users_column() {
        // Without `-p` or without privilege, `ss` omits the trailing
        // users column entirely. We still want the port; process
        // falls back to "unknown".
        let l = parse_ss_listener_line(r#"LISTEN 0 4096 127.0.0.1:9100 0.0.0.0:*"#)
            .expect("should parse");
        assert_eq!(l.port, 9100);
        assert_eq!(l.process, "unknown");
    }

    #[test]
    fn parse_ss_header_returns_none() {
        // The first row of `ss -tlnp` output is the column header. Our
        // caller `.skip(1)`s past it, but the parser should also bail
        // because "Local" isn't a parseable port.
        let h = parse_ss_listener_line(
            "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process",
        );
        assert!(h.is_none(), "header should not parse: {h:?}");
    }

    #[test]
    fn parse_ss_blank_line_returns_none() {
        assert!(parse_ss_listener_line("").is_none());
        assert!(parse_ss_listener_line("   ").is_none());
    }

    #[test]
    fn parse_ss_garbage_returns_none() {
        assert!(parse_ss_listener_line("not ss output").is_none());
    }
}
