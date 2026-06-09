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
    ContainerCreateBody, EndpointIpamConfig, EndpointSettings, HostConfig, Ipam, IpamConfig,
    NetworkCreateRequest, NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectNetworkOptions, ListContainersOptions,
    ListNetworksOptions, LogsOptions, RemoveContainerOptions, StatsOptions, StopContainerOptions,
};
use futures_util::{StreamExt, TryStreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{error, info, warn};

pub mod caddy;

use caddy::{AppRoute, CaddyApi};
pub use caddy::{CaddyRouteSummary, HostCert};

const STATE_PATH: &str = "/var/lib/nasty/apps-enabled";
const COMPOSE_DIR: &str = "/var/lib/nasty/apps";
const DOCKER_SERVICE: &str = "docker.service";
/// Persisted definitions of NASty-managed Docker networks.
const NETWORKS_PATH: &str = "/var/lib/nasty/apps-networks.json";

/// Label applied to all NASty-managed containers.
const LABEL_MANAGED: &str = "nasty.managed";
/// Label storing the app name.
const LABEL_APP_NAME: &str = "nasty.app.name";
/// Label storing the app kind: "simple" or "compose".
const LABEL_APP_KIND: &str = "nasty.app.kind";
/// Label set to "true" when the app was deployed with allow_unsafe.
const LABEL_APP_UNSAFE: &str = "nasty.app.unsafe";
/// Label marking a NASty-managed Docker network (value "true").
const LABEL_NET_MANAGED: &str = "nasty.managed";
/// Label storing a managed network's parent host interface.
const LABEL_NET_PARENT: &str = "nasty.net.parent";
/// Label on a container recording the managed network it's attached to.
const LABEL_APP_NETWORK: &str = "nasty.app.network";
/// Label on a container recording its static IP on the managed network.
const LABEL_APP_NETWORK_IP: &str = "nasty.app.network_ip";

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
    #[error("invalid network: {0}")]
    InvalidNetwork(String),
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
            Self::InvalidNetwork(_) => -33010,
        }
    }
}

// ── Managed Docker networks ─────────────────────────────────────

/// Lightweight host-interface fact the network validator needs. Built
/// by the engine layer from `nasty-system`'s interface list so this
/// crate stays free of a `nasty-system` dependency.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IfaceInfo {
    pub name: String,
    /// "physical" | "bond" | "vlan" | "bridge" | "virtual".
    pub kind: String,
    /// True when this NIC is enslaved to a bridge (so it's an invalid
    /// macvlan/ipvlan parent — the bridge should be used instead).
    pub bridge_member: bool,
}

/// A NASty-managed Docker network. Persisted to [`NETWORKS_PATH`] and
/// materialized via bollard. `host_shim` is honored only for macvlan
/// (see the engine's shim wiring) and defaults off.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ManagedNetwork {
    pub name: String,
    /// "bridge" | "macvlan" | "ipvlan".
    pub driver: String,
    /// Host interface/bridge for macvlan/ipvlan; absent for bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_range: Option<String>,
    /// 802.1q tag; the effective docker parent becomes `parent.vlan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan: Option<u16>,
    /// macvlan only: create a host-side shim so host↔container works.
    #[serde(default)]
    pub host_shim: bool,
    /// The host's address on the container subnet (CIDR) for the shim.
    /// Required when `host_shim` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shim_ip: Option<String>,
}

/// On-disk shape for [`NETWORKS_PATH`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ManagedNetworksFile {
    #[serde(default)]
    networks: Vec<ManagedNetwork>,
}

/// `apps.networks.list` row: the spec plus live-state annotations.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NetworkSummary {
    #[serde(flatten)]
    pub spec: ManagedNetwork,
    /// Present in Docker right now.
    pub exists: bool,
    /// Created/labeled by NASty (vs a pre-existing Docker network).
    pub managed: bool,
    /// Container names currently attached (drives remove-refusal + UI).
    #[serde(default)]
    pub attached_apps: Vec<String>,
}

fn invalid_net(msg: impl Into<String>) -> AppsError {
    AppsError::InvalidNetwork(msg.into())
}

/// Parse `addr/prefix` into `(IpAddr, prefix_len)`.
fn parse_cidr(s: &str) -> Option<(std::net::IpAddr, u8)> {
    let (addr, prefix) = s.split_once('/')?;
    let ip: std::net::IpAddr = addr.trim().parse().ok()?;
    let p: u8 = prefix.trim().parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (p <= max).then_some((ip, p))
}

/// Network address of `ip` masked to `prefix` bits, as a u128 (v4 in low bits).
fn mask_ip(ip: std::net::IpAddr, prefix: u8) -> u128 {
    let (bits, total) = match ip {
        std::net::IpAddr::V4(v4) => (u32::from(v4) as u128, 32u8),
        std::net::IpAddr::V6(v6) => (u128::from(v6), 128u8),
    };
    if prefix == 0 {
        return 0;
    }
    let shift = total - prefix;
    (bits >> shift) << shift
}

/// True iff `ip` falls within `cidr`. `None` on malformed input.
pub fn cidr_contains_ip(cidr: &str, ip: &str) -> Option<bool> {
    let (net, prefix) = parse_cidr(cidr)?;
    let addr: std::net::IpAddr = ip.trim().parse().ok()?;
    if net.is_ipv4() != addr.is_ipv4() {
        return Some(false);
    }
    Some(mask_ip(net, prefix) == mask_ip(addr, prefix))
}

/// True iff `inner` CIDR is fully contained in `outer` CIDR. `None` on malformed input.
fn cidr_contains_net(outer: &str, inner: &str) -> Option<bool> {
    let (_, outer_prefix) = parse_cidr(outer)?;
    let (inner_ip, inner_prefix) = parse_cidr(inner)?;
    if inner_prefix < outer_prefix {
        return Some(false);
    }
    cidr_contains_ip(outer, &inner_ip.to_string())
}

/// macvlan/ipvlan containers get their own LAN IP and have no presence
/// at `127.0.0.1:<host_port>`, so published host ports (and the Caddy
/// ingress that depends on them) are meaningless. Returns the rejection
/// reason when the combination is incompatible.
fn ingress_incompatible(driver: &str, has_host_ports: bool) -> Option<&'static str> {
    if matches!(driver, "macvlan" | "ipvlan") && has_host_ports {
        Some(
            "macvlan/ipvlan apps get their own LAN IP and cannot also publish host ports; \
             remove host-port mappings or choose a bridge network",
        )
    } else {
        None
    }
}

/// Validate a network spec against the set of host interfaces. Pure +
/// dependency-free for unit testing.
fn validate_network_spec(spec: &ManagedNetwork, ifaces: &[IfaceInfo]) -> Result<(), AppsError> {
    if spec.name.is_empty()
        || spec.name.len() > 64
        || !spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(invalid_net(
            "network name must be 1-64 chars of [A-Za-z0-9._-]",
        ));
    }
    if !matches!(spec.driver.as_str(), "bridge" | "macvlan" | "ipvlan") {
        return Err(invalid_net("driver must be bridge, macvlan, or ipvlan"));
    }
    // Host↔container shim (#448): only valid on macvlan, needs a subnet to
    // route and a host address (shim_ip) inside it, distinct from the
    // gateway. The mgmt-interface refusal lives in the engine RPC arm,
    // which knows the management iface.
    if spec.host_shim {
        if spec.driver != "macvlan" {
            return Err(invalid_net(
                "host shim is only supported on macvlan networks",
            ));
        }
        let subnet = spec
            .subnet
            .as_deref()
            .ok_or_else(|| invalid_net("host shim requires the network to have a subnet"))?;
        let shim_ip = spec
            .shim_ip
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                invalid_net("host shim requires a shim_ip (the host's address on the subnet)")
            })?;
        // shim_ip is stored as a static host address (CIDR, e.g. 192.168.1.2/24);
        // the containment and gateway checks compare the bare address.
        let shim_addr = shim_ip.split('/').next().unwrap_or(shim_ip);
        if cidr_contains_ip(subnet, shim_addr) != Some(true) {
            return Err(invalid_net(format!(
                "shim_ip '{shim_ip}' is not within subnet '{subnet}'"
            )));
        }
        if spec.gateway.as_deref() == Some(shim_addr) {
            return Err(invalid_net("shim_ip must differ from the gateway"));
        }
    }
    let needs_parent = matches!(spec.driver.as_str(), "macvlan" | "ipvlan");
    match (&spec.parent, needs_parent) {
        (Some(p), true) => {
            if let Some(v) = spec.vlan
                && !(1..=4094).contains(&v)
            {
                return Err(invalid_net("vlan tag must be 1-4094"));
            }
            match ifaces.iter().find(|i| &i.name == p) {
                None => return Err(invalid_net(format!("parent interface '{p}' not found"))),
                Some(i) if i.bridge_member => {
                    return Err(invalid_net(format!(
                        "'{p}' is enslaved to a bridge; use the bridge itself as the parent"
                    )));
                }
                Some(_) => {}
            }
        }
        (Some(_), false) => return Err(invalid_net("bridge driver does not take a parent")),
        (None, true) => {
            return Err(invalid_net(format!(
                "{} requires a parent interface",
                spec.driver
            )));
        }
        (None, false) => {}
    }
    match &spec.subnet {
        Some(subnet) => {
            if parse_cidr(subnet).is_none() {
                return Err(invalid_net(format!("invalid subnet CIDR '{subnet}'")));
            }
            if let Some(gw) = &spec.gateway
                && cidr_contains_ip(subnet, gw) != Some(true)
            {
                return Err(invalid_net(format!(
                    "gateway '{gw}' is not within subnet '{subnet}'"
                )));
            }
            if let Some(range) = &spec.ip_range
                && cidr_contains_net(subnet, range) != Some(true)
            {
                return Err(invalid_net(format!(
                    "ip_range '{range}' is not within subnet '{subnet}'"
                )));
            }
        }
        None if spec.gateway.is_some() || spec.ip_range.is_some() => {
            return Err(invalid_net("gateway/ip_range require a subnet"));
        }
        None => {}
    }
    Ok(())
}

/// Boot-reconcile decision (pure, testable): which persisted networks
/// are missing from Docker and recreatable, vs skipped because their
/// parent interface is gone.
#[derive(Debug, PartialEq, Default)]
struct ReconcilePlan {
    to_create: Vec<String>,
    skipped_missing_parent: Vec<String>,
}

fn reconcile_plan(
    persisted: &[ManagedNetwork],
    docker_names: &[String],
    iface_names: &[String],
) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();
    for n in persisted {
        if docker_names.iter().any(|d| d == &n.name) {
            continue; // already present
        }
        match &n.parent {
            Some(p) if !iface_names.iter().any(|i| i == p) => {
                plan.skipped_missing_parent.push(n.name.clone());
            }
            _ => plan.to_create.push(n.name.clone()),
        }
    }
    plan
}

fn load_networks() -> ManagedNetworksFile {
    match std::fs::read_to_string(NETWORKS_PATH) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            warn!("failed to parse {NETWORKS_PATH}, ignoring: {e}");
            ManagedNetworksFile::default()
        }),
        Err(_) => ManagedNetworksFile::default(),
    }
}

async fn save_networks(f: &ManagedNetworksFile) -> Result<(), AppsError> {
    let json = serde_json::to_string_pretty(f)
        .map_err(|e| AppsError::CommandFailed(format!("serialize networks: {e}")))?;
    tokio::fs::write(NETWORKS_PATH, json).await?;
    Ok(())
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

/// Parse a Docker image `User` field (e.g. `1000`, `1000:1000`, `nonroot`,
/// `nonroot:nonroot`) into a numeric (uid, gid). Returns `None` when the
/// image runs as root or we can't resolve a named user without reading
/// the image's `/etc/passwd` — see the well-known-names table below for
/// the named users we *can* resolve cheaply.
///
/// Used both by the simple-install chown step and (planned) as a fallback
/// for compose deploys whose service has no explicit `user:` field. The
/// compose path's existing logic in `extract_compose_binds` only reads
/// numeric users from the YAML; this is image-level fallback.
fn parse_image_user(user_field: &str) -> Option<(u32, u32)> {
    let s = user_field.trim();
    if s.is_empty() {
        return None;
    }
    let (u_part, g_part) = match s.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (s, None),
    };
    // root → no chown needed (root can always write).
    if u_part == "0" || u_part == "root" {
        return None;
    }
    // Numeric path.
    if let Ok(uid) = u_part.parse::<u32>() {
        let gid = g_part.and_then(|g| g.parse::<u32>().ok()).unwrap_or(uid);
        return Some((uid, gid));
    }
    // Named user — resolve via a small well-known table. We deliberately
    // don't `docker create` a temp container to read /etc/passwd: that
    // adds a side-effect on Docker state for what should be a pure
    // metadata lookup. Apps using non-well-known named users still work
    // if the operator pre-creates a host_path with correct ownership and
    // re-deploys with that host_path filled in.
    let uid = well_known_user_uid(u_part)?;
    let gid = match g_part {
        None => uid,
        Some(g) => g
            .parse::<u32>()
            .ok()
            .or_else(|| well_known_user_uid(g))
            .unwrap_or(uid),
    };
    Some((uid, gid))
}

/// Maps a small set of conventional named users to their canonical UIDs.
/// Currently just `nonroot` (the distroless convention, UID 65532) — the
/// case that brought us here via the haze launch.
fn well_known_user_uid(name: &str) -> Option<u32> {
    match name {
        "nonroot" => Some(65532),
        _ => None,
    }
}

/// Look up the image's `Config.User` via the local Docker daemon and
/// translate it through [`parse_image_user`]. Returns `None` when the
/// image runs as root, the User field is unset/unresolvable, or the
/// local inspect itself fails (the install will still proceed; we'll
/// just leave the auto-created volume dirs root-owned and the container
/// may or may not work depending on its expectations).
async fn resolve_image_chown_target(docker: &Docker, image: &str) -> Option<(u32, u32)> {
    let inspected = match docker.inspect_image(image).await {
        Ok(i) => i,
        Err(e) => {
            warn!("apps: inspect_image({image}) failed: {e} — skipping auto-chown");
            return None;
        }
    };
    let user_field = inspected.config?.user?;
    let resolved = parse_image_user(&user_field);
    if resolved.is_none() && !user_field.is_empty() && user_field != "root" && user_field != "0" {
        // Surface the case where we have a User but couldn't translate it.
        // Without this hint the operator just sees the container fail to
        // write and has to guess what went wrong.
        warn!(
            "apps: image {image} runs as '{user_field}' which isn't numeric or in the \
             well-known table — auto-created volume dirs will be root-owned. \
             Re-deploy with a pre-chowned host_path to fix."
        );
    }
    resolved
}

/// Poll the just-started container for a short grace window and report
/// crash-loop conditions back to the caller. Returns `Some(reason)` when
/// the container exited (with the exit code), is actively cycling
/// (`RestartCount > 0`), or is dead — all of which mean the install
/// should be rolled back. Returns `None` when the container is running
/// at the end of the window, which is the happy path.
///
/// The 5s window is enough to catch fast-failures (bad env vars, missing
/// permissions on a bind mount, image entrypoint bombing out) without
/// false-positiving slow-starters: a real app should at minimum survive
/// past its argv parsing in 5 seconds.
async fn detect_install_crash(docker: &Docker, cname: &str) -> Option<String> {
    const GRACE_TICKS: u32 = 10;
    const TICK_MS: u64 = 500;
    for _ in 0..GRACE_TICKS {
        tokio::time::sleep(std::time::Duration::from_millis(TICK_MS)).await;
        let inspect = match docker.inspect_container(cname, None).await {
            Ok(i) => i,
            // Inspect failures during startup are transient enough that
            // bailing here would over-rollback; just try again next tick.
            Err(_) => continue,
        };
        // `restart_count` lives on the inspect response itself, not on the
        // nested `state` struct (it counts restarts across the container's
        // lifetime, not the current state's lifetime).
        let restart_count = inspect.restart_count.unwrap_or(0);
        let Some(state) = inspect.state else { continue };
        let status = state.status.map(|s| format!("{s:?}").to_lowercase());
        match status.as_deref() {
            Some("exited") | Some("dead") => {
                let exit_code = state.exit_code.unwrap_or(-1);
                let tail = recent_logs(docker, cname, 20).await;
                return Some(format!(
                    "exited with status {exit_code}; recent logs:\n{tail}"
                ));
            }
            Some("restarting") if restart_count >= 1 => {
                let tail = recent_logs(docker, cname, 20).await;
                return Some(format!(
                    "container is in restart loop ({restart_count} restarts so far); recent logs:\n{tail}"
                ));
            }
            _ => {}
        }
    }
    None
}

/// Fetch the last N log lines from a container, joined as a single string.
/// Best-effort: returns `(no logs)` on any failure so callers can embed
/// the result in error messages without further error handling.
async fn recent_logs(docker: &Docker, cname: &str, tail: u32) -> String {
    use bollard::query_parameters::LogsOptions;
    let opts = LogsOptions {
        stdout: true,
        stderr: true,
        tail: tail.to_string(),
        ..Default::default()
    };
    let mut stream = docker.logs(cname, Some(opts));
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => buf.push_str(&String::from_utf8_lossy(c.as_ref())),
            Err(_) => break,
        }
    }
    if buf.trim().is_empty() {
        "(no logs)".to_string()
    } else {
        buf
    }
}

/// chown a path to the given uid/gid. Async wrapper around libc::chown
/// via spawn_blocking so we don't shell out for a single syscall.
async fn chown_path(path: &str, uid: u32, gid: u32) -> Result<(), AppsError> {
    let p = path.to_string();
    tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(p.as_str())
            .map_err(|e| AppsError::CommandFailed(format!("path contains NUL: {e}")))?;
        // SAFETY: c_path is a valid C string for the lifetime of the call;
        // uid/gid are passed by value; chown is async-signal-safe.
        let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
        if rc != 0 {
            return Err(AppsError::CommandFailed(format!(
                "chown({p}, {uid}, {gid}): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    })
    .await
    .map_err(|e| AppsError::CommandFailed(format!("chown task join error: {e}")))?
}

// ── Reverse-proxy compatibility probe ───────────────────────────

/// Result of [`probe_proxy_compat`]. The probe answers a narrow question:
/// "if the user clicks the WebUI's `Open` button (which goes to
/// `/apps/<name>/` via Caddy), will their browser render the app or just
/// a blank page because the upstream's HTML emits absolute root-relative
/// asset paths?"
pub enum ProxyCompat {
    /// The probed HTML doesn't reference absolute root-path assets, or
    /// it does and a sampled asset's Content-Type matches what the
    /// browser would expect. Ingress works.
    Ok,
    /// The HTML references an absolute root-path asset (e.g.
    /// `/assets/index.js`) that, fetched through the proxy, comes back
    /// as `text/html` — the signature of the NASty WebUI SPA fallback
    /// catching what the upstream expected to be a JS/CSS/image asset.
    /// The browser refuses to execute HTML as a JS module and the page
    /// renders blank. Caught with haze, whose HTML is full of absolute
    /// asset paths and which has no `--base-path`-style config.
    Broken {
        /// One example absolute path the upstream's HTML emitted that
        /// the proxy returned as HTML. Shown to the user as a hint
        /// (e.g. `/assets/index-Xy.js`).
        sample_path: String,
        /// Content-Type the proxy actually returned for that path.
        /// Usually `text/html; charset=utf-8`.
        content_type: String,
    },
    /// Probe didn't reach a usable answer — container slow to respond,
    /// non-HTML root, network error inside Caddy. Don't touch ingress
    /// either way (silence is safer than guessing).
    Unknown,
}

impl ProxyCompat {
    /// Returns the human-readable reason when this is `Broken`. Used by
    /// install() to persist the reason into the manifest and surface
    /// it in the apps list as a "direct-port only" hint.
    pub fn broken_reason(&self) -> Option<String> {
        match self {
            ProxyCompat::Broken {
                sample_path,
                content_type,
            } => Some(format!(
                "app emits absolute root-path assets that don't route through \
                 /apps/<name>/ (sample: {sample_path} returned {content_type})"
            )),
            _ => None,
        }
    }
}

/// Probe whether path-prefix reverse proxying actually works for an app.
///
/// Algorithm:
///   1. Fetch `https://127.0.0.1/apps/<name>/` (the route that Caddy just
///      learned about) with up to 3 retries — the container may still be
///      initialising. Cert validation is off because the appliance's TLS
///      cert is self-signed on first boot.
///   2. Find an absolute-path asset reference in the returned HTML that
///      isn't already under `/apps/<name>/`. Static `href=` / `src=`
///      attributes only; that's good enough to catch the haze-class
///      case where every CSS/JS/icon URL starts with a bare slash.
///   3. Re-fetch that asset through the proxy. If the response's
///      Content-Type is `text/html`, NASty's WebUI SPA fallback caught
///      the request instead of the upstream — the page will render
///      blank in a real browser, so report `Broken`.
///
/// Probing on `127.0.0.1` rather than the LAN IP avoids depending on
/// hostname resolution, and keeps the request inside the box where
/// Caddy is guaranteed to be reachable.
async fn probe_proxy_compat(app_name: &str) -> ProxyCompat {
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(5))
        // We follow up to one redirect — Caddy issues 301 http→https
        // when probing the bare HTTP port, but we go straight to https
        // below so this is mostly defensive.
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return ProxyCompat::Unknown,
    };

    let root_url = format!("https://127.0.0.1/apps/{app_name}/");
    let prefix = format!("/apps/{app_name}/");

    let mut html: Option<String> = None;
    for _ in 0..3 {
        let resp = match client.get(&root_url).send().await {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        if !resp.status().is_success() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.starts_with("text/html") {
            // Non-HTML root: the app is an API or an asset server, not
            // a browser-target SPA. Nothing for us to validate.
            return ProxyCompat::Ok;
        }
        match resp.text().await {
            Ok(body) => {
                html = Some(body);
                break;
            }
            Err(_) => continue,
        }
    }
    let Some(html) = html else {
        return ProxyCompat::Unknown;
    };

    let Some(asset_path) = find_absolute_asset_path(&html, &prefix) else {
        return ProxyCompat::Ok;
    };

    let asset_url = format!("https://127.0.0.1{asset_path}");
    let asset_resp = match client.get(&asset_url).send().await {
        Ok(r) => r,
        Err(_) => return ProxyCompat::Unknown,
    };
    let content_type = asset_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if content_type.starts_with("text/html") {
        ProxyCompat::Broken {
            sample_path: asset_path,
            content_type,
        }
    } else {
        ProxyCompat::Ok
    }
}

/// Scan HTML for `href="/…"` and `src="/…"` attributes referring to
/// absolute paths from the domain root, and return the first one that
/// doesn't already live under `prefix`. Intentionally a hand-rolled scan
/// rather than a full HTML parser — we only need to find one example to
/// then verify dynamically, and adding `scraper`/`html5ever` for this
/// would dwarf the rest of the function.
fn find_absolute_asset_path(html: &str, prefix: &str) -> Option<String> {
    for attr in ["href=", "src="] {
        let mut idx = 0;
        while let Some(pos) = html[idx..].find(attr) {
            let abs = idx + pos + attr.len();
            let bytes = html.as_bytes();
            if abs >= bytes.len() {
                break;
            }
            let quote = bytes[abs];
            if quote != b'"' && quote != b'\'' {
                idx = abs;
                continue;
            }
            let value_start = abs + 1;
            let end_rel = match html[value_start..].find(quote as char) {
                Some(e) => e,
                None => break,
            };
            let value = &html[value_start..value_start + end_rel];
            idx = value_start + end_rel + 1;
            // Skip protocol-relative URLs, fragment-only, or non-absolute
            // paths — only `/foo` style references are the failure mode
            // we're hunting.
            if !value.starts_with('/') || value.starts_with("//") {
                continue;
            }
            // Drop query and fragment before comparing — `?v=1` and `#x`
            // wouldn't change which file the proxy serves.
            let path: &str = value.split(['?', '#']).next().unwrap_or(value);
            if path.starts_with(prefix) {
                continue;
            }
            return Some(path.to_string());
        }
    }
    None
}

/// Persist a `proxy_disabled_reason` into the per-app manifest JSON so
/// the apps list can surface it later as a "direct-port only" badge.
/// Read-modify-write of `/var/lib/nasty/apps/<name>.json` — the file is
/// already written at install time with the rest of the manifest, so
/// we re-read, splice in the new field, and re-write.
async fn save_proxy_disabled_reason(app_name: &str, reason: &str) -> Result<(), AppsError> {
    let manifest_path = format!("{}/{}.json", COMPOSE_DIR, app_name);
    let mut manifest: serde_json::Value = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| AppsError::CommandFailed(format!("manifest parse: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(AppsError::CommandFailed(format!("manifest read: {e}"))),
    };
    let map = match manifest.as_object_mut() {
        Some(m) => m,
        None => return Err(AppsError::CommandFailed("manifest not an object".into())),
    };
    map.insert(
        "proxy_disabled_reason".to_string(),
        serde_json::Value::String(reason.to_string()),
    );
    tokio::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .await
    .map_err(|e| AppsError::CommandFailed(format!("manifest write: {e}")))?;
    Ok(())
}

/// Drop the `proxy_disabled_reason` field from an app's manifest.
/// Called when the operator moves the app to subdomain mode — the
/// recorded reason describes a path-prefix-mode failure that doesn't
/// apply to subdomain mode, so the verdict is stale. Idempotent: if
/// the manifest doesn't exist or the field isn't set, returns Ok.
async fn clear_proxy_disabled_reason(app_name: &str) -> Result<(), AppsError> {
    let manifest_path = format!("{}/{}.json", COMPOSE_DIR, app_name);
    let mut manifest: serde_json::Value = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| AppsError::CommandFailed(format!("manifest parse: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(AppsError::CommandFailed(format!("manifest read: {e}"))),
    };
    let Some(map) = manifest.as_object_mut() else {
        return Ok(());
    };
    if map.remove("proxy_disabled_reason").is_none() {
        return Ok(());
    }
    tokio::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .await
    .map_err(|e| AppsError::CommandFailed(format!("manifest write: {e}")))?;
    Ok(())
}

/// Read `proxy_disabled_reason` from an app's manifest. Returns `None`
/// when the manifest is missing, unparseable, or simply has no such
/// field — all of which are the "no hint to surface" cases for the
/// WebUI. Errors don't bubble up; the apps list is best-effort here.
async fn load_proxy_disabled_reason(app_name: &str) -> Option<String> {
    let manifest_path = format!("{}/{}.json", COMPOSE_DIR, app_name);
    let content = tokio::fs::read_to_string(&manifest_path).await.ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed
        .get("proxy_disabled_reason")?
        .as_str()
        .map(String::from)
}

/// Persist (or clear) the per-app `ingress_subdomain` in the manifest.
/// Pass `Some(host)` to record subdomain mode, `None` to drop the field
/// entirely (used by `ingress_remove` so the next tenant of the same
/// name doesn't inherit a stale subdomain). Mirrors
/// `save_proxy_disabled_reason` — read-modify-write of the same JSON.
async fn save_ingress_subdomain(app_name: &str, subdomain: Option<&str>) -> Result<(), AppsError> {
    let manifest_path = format!("{}/{}.json", COMPOSE_DIR, app_name);
    let mut manifest: serde_json::Value = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| AppsError::CommandFailed(format!("manifest parse: {e}")))?,
        // Compose apps don't have a flat manifest (their storage lives
        // under `<COMPOSE_DIR>/<name>/docker-compose.yml`), but we
        // still need a place to persist their ingress_subdomain so it
        // survives a reboot — without that, set_app_route puts the
        // host-match route in Caddy and `compute_desired_routes` then
        // forgets the choice on restart (the "subdomain ingress
        // reverts on reboot" half of #247 for compose apps). Create
        // a stub `<name>.json` next to the compose dir to hold the
        // field; the on-remove path (lib.rs ~2015) already deletes
        // it, so no leak. Mirrors how `save_proxy_disabled_reason`
        // handles the same NotFound case.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(AppsError::CommandFailed(format!("manifest read: {e}"))),
    };
    let map = match manifest.as_object_mut() {
        Some(m) => m,
        None => return Err(AppsError::CommandFailed("manifest not an object".into())),
    };
    match subdomain {
        Some(s) => {
            map.insert(
                "ingress_subdomain".to_string(),
                serde_json::Value::String(s.to_string()),
            );
        }
        None => {
            map.remove("ingress_subdomain");
        }
    }
    tokio::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .await
    .map_err(|e| AppsError::CommandFailed(format!("manifest write: {e}")))?;
    Ok(())
}

/// Read `ingress_subdomain` from an app's manifest. Returns `None` when
/// the manifest is missing, unparseable, has no such field, or carries
/// an empty string — same "best-effort, default to path-prefix" failure
/// mode the rest of the apps list reconcile uses.
async fn load_ingress_subdomain(app_name: &str) -> Option<String> {
    let manifest_path = format!("{}/{}.json", COMPOSE_DIR, app_name);
    let content = tokio::fs::read_to_string(&manifest_path).await.ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed
        .get("ingress_subdomain")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Validate a subdomain hostname before we hand it to Caddy. RFC 1123-ish:
/// labels are 1–63 chars of `[A-Za-z0-9-]`, no leading/trailing hyphen,
/// dots between labels, total length ≤ 253. Reject anything else upfront
/// so the operator gets a clear error rather than a cryptic Caddy 4xx.
fn validate_subdomain(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("subdomain must not be empty".into());
    }
    if host.len() > 253 {
        return Err(format!("subdomain too long ({} > 253 chars)", host.len()));
    }
    // Require at least one dot — a single-label hostname (`jellyfin`)
    // would still route, but the cert story falls apart (no TLD = no
    // ACME) and it's almost certainly a user typo. Caddy itself will
    // serve them but the UX is bad enough that we'd rather reject and
    // surface a clear message.
    if !host.contains('.') {
        return Err(format!(
            "subdomain '{host}' must be a fully-qualified hostname (contain at least one dot)"
        ));
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(format!("subdomain '{host}' has an empty label"));
        }
        if label.len() > 63 {
            return Err(format!(
                "subdomain '{host}' has a label longer than 63 chars: '{label}'"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "subdomain '{host}' label '{label}' may not start or end with '-'"
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!(
                "subdomain '{host}' label '{label}' contains invalid characters \
                 (only A-Z, a-z, 0-9, '-' allowed)"
            ));
        }
    }
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
    /// Human-readable reason the reverse-proxy ingress was disabled for
    /// this app — set when the post-install probe detects that the
    /// upstream emits absolute root-path assets that path-prefix proxying
    /// can't route (haze-class apps). The WebUI hides the "Open" button
    /// when this is set and surfaces the text as a tooltip explaining
    /// why only the direct host-port link is offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_disabled_reason: Option<String>,
    /// Name of the NASty-managed Docker network this app is attached to,
    /// if any. The WebUI shows a badge and (for macvlan/ipvlan) suppresses
    /// the reverse-proxy "Open" link since the app is reached on its own IP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The app's IP on that network, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_ip: Option<String>,
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

/// Aggregated live resource usage for a NASty-managed app. For compose
/// apps with multiple containers the per-container values are summed,
/// matching the rest of the WebUI which treats compose apps as a single
/// unit.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppStats {
    /// App name. Matches the `name` of an entry in `apps.list`.
    pub name: String,
    /// CPU percentage averaged over the Docker stats sample window
    /// (Docker stats CLI semantics — capped only by num-CPUs * 100).
    pub cpu_percent: f64,
    /// Memory in use, with page cache / inactive-file subtracted to
    /// match `docker stats`. Sum across compose containers.
    pub memory_bytes: u64,
    /// Memory limit reported by cgroup. Equals total host memory when
    /// no explicit limit is set; the WebUI decides what to render when
    /// the value matches host memory.
    pub memory_limit_bytes: u64,
    /// Total bytes received across all container interfaces.
    pub net_rx_bytes: u64,
    /// Total bytes transmitted across all container interfaces.
    pub net_tx_bytes: u64,
    /// Cumulative block-device bytes read (cgroup v2 io_service_bytes).
    pub block_read_bytes: u64,
    /// Cumulative block-device bytes written.
    pub block_write_bytes: u64,
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
    /// NASty-managed Docker network the app is attached to (from label).
    /// Round-tripped through Edit/pull so a reinstall keeps the attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The static IP requested at install (from label), if any. Distinct
    /// from a live auto-assigned address — re-applied verbatim on reinstall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImageInspectResult {
    pub ports: Vec<AppPort>,
    /// Bind-mount paths the image declares via `VOLUME` in its Dockerfile.
    /// The WebUI installer prefills these as Volume rows so the user
    /// doesn't have to know that e.g. ghcr.io/consi/haze needs
    /// `/var/lib/haze` to be persistent for SQLite to work.
    #[serde(default)]
    pub volumes: Vec<AppVolume>,
    /// Image's runtime user as declared in `Config.User`. May be numeric
    /// (`1000` / `1000:1000`) or named (`nonroot:nonroot`). The WebUI
    /// surfaces this so the user knows the host volume dirs will be
    /// chowned to that identity by the install pipeline. `None` = root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Known recipe for configuring this image to serve under
    /// `/apps/<name>/` behind NASty's path-prefix reverse proxy. When
    /// present, the WebUI offers an "Apply" button that appends the
    /// recipe's env entries to the form (the user can still edit them).
    /// Catches apps that *could* run behind a sub-path but only with
    /// specific env vars set — e.g. Grafana needs `GF_SERVER_ROOT_URL`
    /// plus `GF_SERVER_SERVE_FROM_SUB_PATH=true`; without those, our
    /// post-install probe would (correctly) disable ingress and the
    /// user would only see the direct-port link, even though a one-line
    /// env change would have made the proxy work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath_recipe: Option<SubPathRecipe>,
}

/// A small curated mapping of "set these env vars to make this image
/// serve under `/apps/<name>/`". Hand-curated rather than heuristic —
/// the value formats differ wildly across apps (Grafana takes a URL
/// with `%(protocol)s`/`%(domain)s` placeholders; Vaultwarden takes a
/// bare full URL; etc.) so guessing risks setting something that breaks
/// the app in non-obvious ways. The WebUI shows the recipe behind an
/// opt-in button so the user always confirms before it's applied.
///
/// Templates: `{name}` is substituted with the App Name field on the
/// engine side; `{host}` and `{scheme}` are substituted in the WebUI
/// from `window.location` (the engine can't know which IP/hostname the
/// user reaches NASty on). Both forms are surfaced as editable env
/// rows so the user can adjust before installing.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SubPathRecipe {
    /// Short label shown next to the "Apply" button (e.g. "Grafana
    /// sub-path mode"). Not the env var key — purely human-readable.
    pub display_name: String,
    /// Env vars to add to the install form when the user applies this
    /// recipe. Values may contain `{name}`, `{host}`, `{scheme}` —
    /// see template note above.
    pub env: Vec<AppEnv>,
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
    /// Optional FQDN to serve the app at via subdomain mode (e.g.
    /// `jellyfin.example.com`). When set, the install pipeline emits a
    /// host-matching Caddy route instead of the default path-prefix
    /// route, and skips the post-install probe (subdomain mode roots
    /// the app at `/`, so the absolute-asset-path failure mode the
    /// probe catches can't happen). Empty/omitted = path-prefix
    /// behaviour, the historical default.
    ///
    /// Conflict detection happens at the engine-binary layer before
    /// install runs (see deploy_simple in app_deploy.rs) so the
    /// operator doesn't pay for an image pull just to discover the
    /// hostname is taken.
    #[serde(default)]
    pub subdomain: Option<String>,
    /// Attach the container to a NASty-managed Docker network instead of
    /// (only) the default bridge. For a macvlan/ipvlan network the
    /// container gets its own LAN IP and is *not* reachable at
    /// `127.0.0.1:<host_port>`, so publishing host ports and reverse-proxy
    /// ingress are rejected/skipped for it (see install's mutual-exclusion).
    #[serde(default)]
    pub network: Option<String>,
    /// Optional static IPv4 within the chosen network's subnet.
    #[serde(default)]
    pub static_ip: Option<String>,
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
    /// Set by `apps.config` when this entry's value matches the image's
    /// own `Config.Env` default for the same key — i.e. the user didn't
    /// set it explicitly, it just came along with the image. The WebUI
    /// greys these rows out in Edit and shows an "Override" button so
    /// the user sees what the image provides without being misled into
    /// thinking they own it. Always `false` when the WebUI submits env
    /// back to the engine (install/update) — the engine doesn't read
    /// this field for create_container; it's purely an Edit-side hint.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_image_default: bool,
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
    /// URL path prefix (e.g. "/apps/plex/"). Always set; in subdomain
    /// mode it's purely informational (the WebUI prefers the
    /// `subdomain`-derived URL for the Open button) since the app
    /// answers at root under the configured hostname.
    pub path: String,
    /// Fully-qualified hostname the app is served under when subdomain
    /// mode is on (e.g. `jellyfin.example.com`). When set, Caddy
    /// matches the route by `host` rather than path-prefix, and the
    /// app sees itself rooted at `/` — sidestepping the absolute-asset
    /// failure mode that #219's probe disables path-prefix ingress for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdomain: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetIngressRequest {
    /// App name.
    pub name: String,
    /// Host port to proxy to.
    pub host_port: u16,
    /// Opt into subdomain mode by providing a fully-qualified hostname
    /// (e.g. `jellyfin.example.com`). When set, the engine emits a
    /// host-matching Caddy route instead of the default
    /// `/apps/<name>/` path-prefix route, and persists the choice in
    /// the app manifest so engine restarts preserve it. Set to `null`
    /// or omit to use path-prefix mode (the historical default).
    #[serde(default)]
    pub subdomain: Option<String>,
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
    /// What is using this port (e.g. "caddy", "app:plex").
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
    /// Previous-sample cache for `apps.stats`, keyed by container ID.
    /// The Docker stats endpoint needs *two* samples to compute deltas
    /// (CPU %, network rate, etc.); the natural API for that
    /// (`one_shot: false`) makes Docker wait internally for a second
    /// sample, taking 1-2 seconds per call. Instead we ask for a
    /// single instant frame and remember the previous one here, so
    /// every poll responds immediately and the delta math is done
    /// locally.
    prev_stats: tokio::sync::Mutex<
        std::collections::HashMap<String, bollard::models::ContainerStatsResponse>,
    >,
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
            prev_stats: tokio::sync::Mutex::new(std::collections::HashMap::new()),
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
        // Try to connect (Docker may have started after engine). Keep the
        // bollard error in the message so a permission-denied vs
        // socket-not-found can be distinguished without re-running.
        let d = Docker::connect_with_unix_defaults()
            .or_else(|_| {
                Docker::connect_with_unix("/var/run/docker.sock", 120, bollard::API_DEFAULT_VERSION)
            })
            .map_err(|e| AppsError::NotReady(format!("Docker socket not available: {e}")))?;
        *self.docker.lock().unwrap() = Some(d.clone());
        info!("Connected to Docker socket");
        Ok(d)
    }

    fn docker(&self) -> Result<Docker, AppsError> {
        self.docker_conn()
    }

    // ── Managed Docker networks ─────────────────────────────

    /// List Docker networks (excluding the host/null built-ins),
    /// annotated with managed/exists/attached_apps. Managed networks
    /// show their authoritative persisted spec; others are derived from
    /// live Docker state. Persisted networks absent from Docker are
    /// appended with `exists=false` (boot reconcile will recreate them).
    pub async fn network_list(&self) -> Result<Vec<NetworkSummary>, AppsError> {
        self.require_ready().await?;
        let docker = self.docker()?;
        let nets = docker
            .list_networks(None::<ListNetworksOptions>)
            .await
            .map_err(|e| AppsError::DockerFailed(format!("list networks: {e}")))?;
        let persisted = load_networks().networks;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for n in nets {
            let name = match &n.name {
                Some(s) if !s.is_empty() => s.clone(),
                _ => continue,
            };
            let driver = n.driver.clone().unwrap_or_default();
            if matches!(driver.as_str(), "host" | "null") {
                continue;
            }
            let labels = n.labels.clone().unwrap_or_default();
            let managed = labels.get(LABEL_NET_MANAGED).map(|v| v == "true") == Some(true);
            let spec = match persisted.iter().find(|p| p.name == name) {
                Some(p) => p.clone(),
                None => {
                    let (subnet, gateway, ip_range) = n
                        .ipam
                        .as_ref()
                        .and_then(|i| i.config.as_ref())
                        .and_then(|c| c.first())
                        .map(|c| (c.subnet.clone(), c.gateway.clone(), c.ip_range.clone()))
                        .unwrap_or((None, None, None));
                    ManagedNetwork {
                        name: name.clone(),
                        driver,
                        parent: n.options.as_ref().and_then(|o| o.get("parent")).cloned(),
                        subnet,
                        gateway,
                        ip_range,
                        vlan: None,
                        host_shim: false,
                        shim_ip: None,
                    }
                }
            };
            let attached_apps = if managed {
                docker
                    .inspect_network(&name, None::<InspectNetworkOptions>)
                    .await
                    .ok()
                    .and_then(|ni| ni.containers)
                    .map(|c| c.into_values().filter_map(|e| e.name).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            seen.insert(name);
            out.push(NetworkSummary {
                spec,
                exists: true,
                managed,
                attached_apps,
            });
        }
        for spec in persisted {
            if !seen.contains(&spec.name) {
                out.push(NetworkSummary {
                    spec,
                    exists: false,
                    managed: true,
                    attached_apps: Vec::new(),
                });
            }
        }
        Ok(out)
    }

    /// Create a NASty-managed Docker network and persist its spec.
    pub async fn network_create(
        &self,
        spec: ManagedNetwork,
        host_ifaces: &[IfaceInfo],
    ) -> Result<(), AppsError> {
        self.require_ready().await?;
        validate_network_spec(&spec, host_ifaces)?;
        let docker = self.docker()?;
        if docker
            .inspect_network(&spec.name, None::<InspectNetworkOptions>)
            .await
            .is_ok()
        {
            return Err(invalid_net(format!(
                "a Docker network named '{}' already exists",
                spec.name
            )));
        }

        let mut options = std::collections::HashMap::new();
        if let Some(p) = &spec.parent {
            let parent = match spec.vlan {
                Some(v) => format!("{p}.{v}"),
                None => p.clone(),
            };
            options.insert("parent".to_string(), parent);
        }
        let ipam = spec.subnet.as_ref().map(|subnet| Ipam {
            driver: Some("default".to_string()),
            config: Some(vec![IpamConfig {
                subnet: Some(subnet.clone()),
                gateway: spec.gateway.clone(),
                ip_range: spec.ip_range.clone(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let mut labels = std::collections::HashMap::new();
        labels.insert(LABEL_NET_MANAGED.to_string(), "true".to_string());
        if let Some(p) = &spec.parent {
            labels.insert(LABEL_NET_PARENT.to_string(), p.clone());
        }
        let body = NetworkCreateRequest {
            name: spec.name.clone(),
            driver: Some(spec.driver.clone()),
            options: (!options.is_empty()).then_some(options),
            ipam,
            labels: Some(labels),
            ..Default::default()
        };
        docker
            .create_network(body)
            .await
            .map_err(|e| AppsError::DockerFailed(format!("create network '{}': {e}", spec.name)))?;

        let mut f = load_networks();
        f.networks.retain(|n| n.name != spec.name);
        f.networks.push(spec);
        save_networks(&f).await
    }

    /// Remove a managed network. Refuses while containers are attached.
    pub async fn network_remove(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;
        let docker = self.docker()?;
        if let Ok(ni) = docker
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
        {
            if ni.containers.map(|c| !c.is_empty()) == Some(true) {
                return Err(invalid_net(format!(
                    "network '{name}' is in use by attached containers"
                )));
            }
            docker
                .remove_network(name)
                .await
                .map_err(|e| AppsError::DockerFailed(format!("remove network '{name}': {e}")))?;
        }
        // Drop the persisted spec whether or not Docker still had it.
        let mut f = load_networks();
        let before = f.networks.len();
        f.networks.retain(|n| n.name != name);
        if f.networks.len() != before {
            save_networks(&f).await?;
        }
        Ok(())
    }

    /// Resolve a network name to its spec — persisted first, else a
    /// pre-existing (unmanaged) Docker network. Errors if neither.
    async fn resolve_managed_network(&self, name: &str) -> Result<ManagedNetwork, AppsError> {
        if let Some(spec) = load_networks()
            .networks
            .into_iter()
            .find(|n| n.name == name)
        {
            return Ok(spec);
        }
        let ni = self
            .docker()?
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
            .map_err(|_| invalid_net(format!("network '{name}' does not exist")))?;
        Ok(ManagedNetwork {
            name: name.to_string(),
            driver: ni.driver.unwrap_or_default(),
            parent: ni.options.as_ref().and_then(|o| o.get("parent")).cloned(),
            subnet: ni
                .ipam
                .as_ref()
                .and_then(|i| i.config.as_ref())
                .and_then(|c| c.first())
                .and_then(|c| c.subnet.clone()),
            gateway: None,
            ip_range: None,
            vlan: None,
            host_shim: false,
            shim_ip: None,
        })
    }

    /// Boot reconcile: recreate persisted managed networks missing from
    /// Docker; skip (warn) any whose parent interface has vanished.
    /// No-op when Docker is off or there's nothing persisted.
    pub async fn reconcile_networks(&self, host_ifaces: Vec<IfaceInfo>) {
        if !self.is_enabled() || !self.is_docker_ready().await {
            return;
        }
        let persisted = load_networks().networks;
        if persisted.is_empty() {
            return;
        }
        let docker = match self.docker() {
            Ok(d) => d,
            Err(_) => return,
        };
        let docker_names: Vec<String> = docker
            .list_networks(None::<ListNetworksOptions>)
            .await
            .map(|v| v.into_iter().filter_map(|n| n.name).collect())
            .unwrap_or_default();
        let iface_names: Vec<String> = host_ifaces.iter().map(|i| i.name.clone()).collect();
        let plan = reconcile_plan(&persisted, &docker_names, &iface_names);
        for name in &plan.skipped_missing_parent {
            warn!("apps: managed network '{name}' parent interface is gone; not recreating");
        }
        for name in plan.to_create {
            if let Some(spec) = persisted.iter().find(|n| n.name == name).cloned()
                && let Err(e) = self.network_create(spec, &host_ifaces).await
            {
                warn!("apps: failed to reconcile network '{name}': {e}");
            }
        }
    }

    // ── Enable/Disable ──────────────────────────────────────

    pub fn is_enabled(&self) -> bool {
        Path::new(STATE_PATH).exists()
    }

    pub fn load_config() -> AppsConfig {
        let content = match std::fs::read_to_string(STATE_PATH) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AppsConfig::default(),
            Err(e) => {
                // A non-NotFound read error (permissions, IO) means we
                // *might* be silently resetting a real config. Log
                // before returning defaults so the user can match a
                // "my installed apps disappeared" report to the
                // underlying cause.
                warn!("apps.json read from {STATE_PATH} failed: {e} — using defaults");
                return AppsConfig::default();
            }
        };
        match serde_json::from_str(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(
                    "apps.json parse failed: {e} — file may be corrupt; \
                     using defaults (any persisted config will be lost)"
                );
                AppsConfig::default()
            }
        }
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
            // Wait for Docker to be ready (up to 30s). Track the last
            // failure reason so the "didn't become ready" log can say
            // *why* (permission denied, connection refused, etc.) instead
            // of just timing out silently.
            let mut ready = false;
            let mut last_err: Option<String> = None;
            for _ in 0..15 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                match Docker::connect_with_unix_defaults() {
                    Ok(docker) => match docker.ping().await {
                        Ok(_) => {
                            ready = true;
                            break;
                        }
                        Err(e) => last_err = Some(format!("ping: {e}")),
                    },
                    Err(e) => last_err = Some(format!("connect: {e}")),
                }
            }

            if !ready {
                error!(
                    "Docker did not become ready within 30s — last error: {}",
                    last_err.unwrap_or_else(|| "(no error captured)".into())
                );
                return;
            }

            // Set up storage directory
            let storage_path = setup_apps_storage(filesystem.as_deref()).await;

            // Create compose directory. A failure here means every
            // subsequent compose-app deploy will hit a confusing
            // "no such directory" error — the root cause needs to be
            // visible *now*, in the bootstrap log.
            if let Err(e) = tokio::fs::create_dir_all(COMPOSE_DIR).await {
                error!("create_dir_all({COMPOSE_DIR}) failed: {e} — compose deploys will fail");
            }

            // Persist storage path in config. A failure here means the
            // chosen storage path won't survive an engine restart, which
            // breaks app data persistence — log loudly.
            if let Some(ref path) = storage_path {
                let config = AppsConfig {
                    enabled: true,
                    storage_path: Some(path.clone()),
                };
                if let Err(e) = AppsService::save_config(&config).await {
                    error!(
                        "AppsConfig save failed: {e} — apps storage path won't \
                         survive engine restart"
                    );
                }
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

        // Resolve a requested managed network up front. A macvlan/ipvlan
        // network gives the container its own LAN IP, which is incompatible
        // with published host ports + reverse-proxy ingress (it has no
        // presence at 127.0.0.1:<port>) — reject early and skip ingress.
        let net_spec = match req.network.as_deref().filter(|s| !s.is_empty()) {
            Some(name) => Some(self.resolve_managed_network(name).await?),
            None => None,
        };
        let net_driver = net_spec
            .as_ref()
            .map(|s| s.driver.as_str())
            .unwrap_or("bridge");
        let lan_ip_network = matches!(net_driver, "macvlan" | "ipvlan");
        if let Some(reason) = ingress_incompatible(net_driver, !req.ports.is_empty()) {
            return Err(invalid_net(reason));
        }
        match (&req.static_ip, &net_spec) {
            (Some(_), None) => return Err(invalid_net("static_ip requires a network")),
            (Some(ip), Some(spec)) => {
                if let Some(subnet) = &spec.subnet
                    && cidr_contains_ip(subnet, ip) != Some(true)
                {
                    return Err(invalid_net(format!(
                        "static_ip '{ip}' is not within network subnet '{subnet}'"
                    )));
                }
            }
            (None, _) => {}
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

        // Resolve the image's runtime user → numeric uid/gid for chowning
        // auto-created volume dirs. Without this, dirs we mkdir below are
        // root-owned and a non-root container (e.g. distroless `nonroot`)
        // can't open files inside them — see haze, which crash-looped on
        // "unable to open database file" because it runs as nonroot:nonroot
        // (UID 65532) but /var/lib/haze was root-owned. Returns None when
        // the image runs as root or we can't resolve a named user.
        let chown_target = resolve_image_chown_target(&self.docker()?, &req.image).await;

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
            let (host_path, auto_created) = if v.host_path.is_empty() {
                // Auto-generate path under apps storage
                let base = storage_path
                    .as_deref()
                    .unwrap_or("/var/lib/nasty/apps-data");
                let path = format!("{}/{}/{}", base, req.name, v.name);
                // Ensure the directory exists. A failure here means the
                // bind-mount will land on a missing path and Docker will
                // either refuse to start the container or auto-create a
                // root-owned dir — either way, a logged failure now is
                // way easier to debug than the resulting "container
                // exits immediately" report later.
                let mut created = false;
                match tokio::fs::create_dir_all(&path).await {
                    Ok(()) => created = true,
                    Err(e) => warn!("apps volume: create_dir_all({path}) failed: {e}"),
                }
                (path, created)
            } else {
                (v.host_path.clone(), false)
            };
            // Chown the auto-created dir to the image's runtime uid:gid.
            // We deliberately skip user-supplied host_paths — the user
            // already owns those and we shouldn't mutate ownership of a
            // pre-existing path on the operator's behalf.
            if auto_created
                && let Some((uid, gid)) = chown_target
                && let Err(e) = chown_path(&host_path, uid, gid).await
            {
                warn!(
                    "apps volume: chown({host_path}, {uid}:{gid}) failed: {e} \
                     — the container may not be able to write to this volume"
                );
            }
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
        if let Some(spec) = &net_spec {
            labels.insert(LABEL_APP_NETWORK.to_string(), spec.name.clone());
            if let Some(ip) = req.static_ip.as_deref().filter(|s| !s.is_empty()) {
                labels.insert(LABEL_APP_NETWORK_IP.to_string(), ip.to_string());
            }
        }

        // Attach to the chosen managed network (with an optional static
        // IP). Supplying endpoints_config at create time attaches the
        // container to that network instead of the default bridge.
        let networking_config = net_spec.as_ref().map(|spec| {
            let ipam_config = req
                .static_ip
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|ip| EndpointIpamConfig {
                    ipv4_address: Some(ip.to_string()),
                    ..Default::default()
                });
            let mut endpoints = HashMap::new();
            endpoints.insert(
                spec.name.clone(),
                EndpointSettings {
                    ipam_config,
                    ..Default::default()
                },
            );
            NetworkingConfig {
                endpoints_config: Some(endpoints),
            }
        });

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
            networking_config,
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

        // Watch for an immediate crash loop. Without this the RPC returns
        // success the instant Docker accepts `start_container`, even when
        // the container exits 200ms later and starts cycling — the user
        // then sees "Installed" in the WebUI and only finds out the app
        // is broken by noticing the "restarting" status badge later.
        // Rolling back on early crash keeps the install transactional:
        // either it's healthy or you're back to a clean slate.
        if let Some(reason) = detect_install_crash(&self.docker()?, &cname).await {
            warn!(
                "Install '{}' crash-looped within startup grace window: {reason} — rolling back",
                req.name
            );
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
            return Err(AppsError::DockerFailed(format!(
                "container exited shortly after start: {reason}"
            )));
        }

        info!("Installed app '{}' (image: {})", req.name, req.image);

        // Save manifest so the app is visible even when Docker is not running
        let manifest = serde_json::json!({
            "name": req.name,
            "image": req.image,
            "kind": "simple",
            "allow_unsafe": req.allow_unsafe,
        });
        let manifest_path = format!("{}/{}.json", COMPOSE_DIR, req.name);
        if let Err(e) = tokio::fs::create_dir_all(COMPOSE_DIR).await {
            warn!("create_dir_all({COMPOSE_DIR}) failed: {e}");
        }
        if let Err(e) = tokio::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        {
            warn!("Failed to save app manifest: {e}");
        }

        // Auto-create ingress for the first TCP port. UDP can't serve
        // HTTP (Caddy's `reverse_proxy`, like every other HTTP proxy,
        // is TCP-only), so a UDP port is never a valid ingress target
        // — picking it would publish a dead /apps/<name>/ route. If
        // the app exposes only UDP, no ingress is created; the user
        // can still reach the container directly on the LAN.
        // ...but never for a LAN-IP (macvlan/ipvlan) app: it has its own
        // address and no presence at 127.0.0.1:<port> for Caddy to proxy.
        if let Some(first_port) = (!lan_ip_network)
            .then(|| {
                req.ports
                    .iter()
                    .find(|p| p.protocol.eq_ignore_ascii_case("tcp"))
            })
            .flatten()
        {
            let host_port = if let Some(hp) = first_port.host_port {
                hp
            } else {
                // Look up the actual assigned port from Docker
                self.get_mapped_port(&cname, first_port.container_port)
                    .await
                    .unwrap_or(first_port.container_port)
            };
            // Operator can opt into subdomain mode at install time via
            // the install form's "Subdomain (optional)" field; absence
            // (None / empty) keeps path-prefix mode as the historical
            // default. Conflict detection runs upstream in
            // deploy_simple — see app_deploy.rs.
            let install_subdomain = req
                .subdomain
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let chose_subdomain = install_subdomain.is_some();
            if let Err(e) = self
                .ingress_set(SetIngressRequest {
                    name: req.name.clone(),
                    host_port,
                    subdomain: install_subdomain,
                })
                .await
            {
                warn!("Failed to auto-create ingress for '{}': {e}", req.name);
            } else if chose_subdomain {
                // Subdomain mode roots the app at `/`, so the absolute-
                // asset-path failure the probe catches can't happen. Skip
                // the probe — running it would hit the subdomain URL
                // (instead of /apps/<name>/), see an Ok answer, and waste
                // a few seconds doing nothing useful.
            } else if let Some(reason) = probe_proxy_compat(&req.name).await.broken_reason() {
                // The app's HTML emits absolute root-relative paths (e.g.
                // /assets/index.js). Loaded via /apps/<name>/, those asset
                // requests miss the proxy entirely and hit NASty's WebUI
                // SPA fallback — the user sees a blank page. Caught with
                // haze, which has no --base-path config and so can't be
                // made to work behind a path prefix. Pull the ingress so
                // the WebUI's "Open" button doesn't link to a dead page,
                // and record the reason in the manifest so the apps list
                // can surface it as a "direct-port only" hint.
                warn!(
                    "apps: '{}' is not compatible with path-prefix reverse proxy ({reason}) — \
                     removing ingress; access via the direct host port instead",
                    req.name
                );
                if let Err(e) = self.ingress_remove(&req.name).await {
                    warn!(
                        "ingress_remove({}) after proxy-compat probe failed: {e}",
                        req.name
                    );
                }
                if let Err(e) = save_proxy_disabled_reason(&req.name, &reason).await {
                    warn!(
                        "could not persist proxy_disabled_reason for '{}': {e}",
                        req.name
                    );
                }
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

        // Force-remove. The previous version did a graceful `stop_container`
        // with t=10s first, but `remove_container(force: true)` already
        // does SIGKILL+remove in one call — chaining stop in front just
        // added a 10s SIGTERM grace that the user already opted out of by
        // pressing Remove (they want it gone, not gracefully shut down).
        // The webui's RPC timeout fires at ~10s, so the old code reliably
        // tripped "request timed out" toasts on every Remove of a healthy
        // container that ignores SIGTERM.
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

        // Clean up ingress and manifest. ingress_remove failures get
        // logged here because an orphaned reverse-proxy entry survives
        // until the next restart and keeps proxying to a dead container.
        if let Err(e) = self.ingress_remove(name).await {
            warn!("ingress_remove({name}) failed during app removal: {e}");
        }
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

    /// Live resource usage for every NASty-managed app that has at
    /// least one running container.  Returns an entry per app, not per
    /// container — compose-app values are summed across services to
    /// match how `list` presents them.
    ///
    /// We fetch one stats frame per container with `stream=false` and
    /// `one_shot=false`, which gives a frame whose `precpu_stats` are
    /// populated by the daemon's internal sampler so CPU % is real
    /// (rather than 0 as with `one_shot=true`).  Calls are issued
    /// concurrently — Docker's stats endpoint walks cgroups on every
    /// request, so a serial loop would dominate latency once the user
    /// has more than a handful of apps installed.
    pub async fn stats(&self) -> Result<Vec<AppStats>, AppsError> {
        self.require_ready().await?;
        let docker = self.docker()?;

        // Pull running containers we manage (simple or compose).  We
        // skip non-running ones up front — stats on a stopped container
        // returns an empty frame and we'd waste a request per app.
        let mut filters = HashMap::new();
        filters.insert("status".to_string(), vec!["running".to_string()]);
        let running = docker
            .list_containers(Some(ListContainersOptions {
                all: false,
                filters: Some(filters),
                ..Default::default()
            }))
            .await?;

        // (container_id, app_name) for everything we care about.  An
        // app_name of None means the container isn't part of a NASty
        // app and we ignore it.
        let targets: Vec<(String, String)> = running
            .iter()
            .filter_map(|c| {
                let id = c.id.clone()?;
                let labels = c.labels.as_ref()?;
                let app = labels
                    .get(LABEL_APP_NAME)
                    .or_else(|| labels.get("com.docker.compose.project"))
                    .cloned()?;
                Some((id, app))
            })
            .collect();

        // Concurrent fan-out — each per-container call is independent.
        // join_all preserves order so we can zip back to app_name below.
        //
        // `one_shot: true` makes Docker emit a single frame *instantly*
        // with raw cumulative counters. The natural API (`one_shot:
        // false`) would have Docker wait internally for a second sample
        // to populate `precpu_stats` so deltas could be computed —
        // exactly the 1-2 s per call that made the WebUI's 2 s poll
        // back-to-back saturate, and made the engine look like it was
        // hung. We do the delta math ourselves by remembering the
        // previous frame per container (`prev_stats`).
        let stats_opts = StatsOptions {
            stream: false,
            one_shot: true,
        };
        let frames = futures_util::future::join_all(targets.iter().map(|(id, _)| {
            let docker = docker.clone();
            let id = id.clone();
            let opts = stats_opts.clone();
            async move {
                // The endpoint streams in general but with stream=false
                // emits exactly one frame; .next() gives us that frame.
                docker.stats(&id, Some(opts)).next().await
            }
        }))
        .await;

        // Splice in the cached previous frame's CPU counters so
        // `accumulate_stats` can compute the cpu_delta the same way it
        // always did. Then update the cache with the current frame for
        // next poll. On the first call after install there's no previous
        // frame and CPU % shows as 0 — same behaviour as the second
        // sample with `one_shot: false`.
        let mut prev_map = self.prev_stats.lock().await;
        let mut by_app: HashMap<String, AppStats> = HashMap::new();
        for ((container_id, app_name), frame) in targets.iter().zip(frames) {
            let Some(Ok(mut frame)) = frame else { continue };
            if let Some(prev) = prev_map.get(container_id)
                && let Some(prev_cpu) = prev.cpu_stats.as_ref()
            {
                // `precpu_stats` from the one_shot frame is zero; replace
                // it with the previous frame's cpu_stats so the
                // (cpu_delta / sys_delta) calculation has real numbers
                // to subtract from.
                frame.precpu_stats = Some(prev_cpu.clone());
            }
            let entry = by_app.entry(app_name.clone()).or_insert_with(|| AppStats {
                name: app_name.clone(),
                cpu_percent: 0.0,
                memory_bytes: 0,
                memory_limit_bytes: 0,
                net_rx_bytes: 0,
                net_tx_bytes: 0,
                block_read_bytes: 0,
                block_write_bytes: 0,
            });
            accumulate_stats(entry, &frame);
            prev_map.insert(container_id.clone(), frame);
        }

        // Garbage-collect cache entries for containers that are no
        // longer in the running set, so the map doesn't grow forever
        // across the lifetime of the engine as apps come and go.
        let current_ids: std::collections::HashSet<&str> =
            targets.iter().map(|(id, _)| id.as_str()).collect();
        prev_map.retain(|id, _| current_ids.contains(id.as_str()));
        drop(prev_map);

        let mut out: Vec<AppStats> = by_app.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
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
                    let proxy_disabled_reason = load_proxy_disabled_reason(&name).await;
                    apps.push(App {
                        name,
                        image,
                        status: "stopped".to_string(),
                        created: String::new(),
                        kind: "compose".to_string(),
                        containers: Vec::new(),
                        ports: Vec::new(),
                        unsafe_mode,
                        proxy_disabled_reason,
                        network: None,
                        network_ip: None,
                    });
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                // Simple app: manifest JSON. Log read/parse failures —
                // a corrupt manifest silently disappearing from the
                // app list is exactly the "I installed it, where did
                // it go" debug story we're trying to prevent.
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("apps: read manifest {} failed: {e}", path.display());
                        continue;
                    }
                };
                let manifest: serde_json::Value = match serde_json::from_str(&content) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("apps: parse manifest {} failed: {e}", path.display());
                        continue;
                    }
                };
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
                    let proxy_disabled_reason = manifest
                        .get("proxy_disabled_reason")
                        .and_then(|s| s.as_str())
                        .map(String::from);
                    apps.push(App {
                        name: app_name,
                        image,
                        status: "stopped".to_string(),
                        created: String::new(),
                        kind: "simple".to_string(),
                        containers: Vec::new(),
                        ports: Vec::new(),
                        unsafe_mode,
                        proxy_disabled_reason,
                        network: None,
                        network_ip: None,
                    });
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

            let proxy_disabled_reason = load_proxy_disabled_reason(&app_name).await;
            // Managed-network attachment: name from our label, IP from the
            // live endpoint (real assigned address) falling back to the
            // static-IP label.
            let network = labels.and_then(|l| l.get(LABEL_APP_NETWORK)).cloned();
            let network_ip = network
                .as_ref()
                .and_then(|net| {
                    c.network_settings
                        .as_ref()
                        .and_then(|ns| ns.networks.as_ref())
                        .and_then(|nets| nets.get(net))
                        .and_then(|ep| ep.ip_address.clone())
                        .filter(|s| !s.is_empty())
                })
                .or_else(|| labels.and_then(|l| l.get(LABEL_APP_NETWORK_IP)).cloned());
            apps.push(App {
                name: app_name,
                image: c.image.as_deref().unwrap_or("").to_string(),
                status: container_status_str(c),
                created: c.created.map(chrono_from_timestamp).unwrap_or_default(),
                kind,
                containers: vec![],
                ports: extract_ports(c),
                unsafe_mode,
                proxy_disabled_reason,
                network,
                network_ip,
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
                let proxy_disabled_reason = load_proxy_disabled_reason(&name).await;
                apps.push(App {
                    name,
                    image: primary_image,
                    status: overall_status,
                    created,
                    kind: "compose".to_string(),
                    containers,
                    ports: all_ports,
                    unsafe_mode,
                    proxy_disabled_reason,
                    network: None,
                    network_ip: None,
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

        // Parse env. The container's `Config.Env` is `image defaults
        // + values we passed at create_container` with no marker for
        // which is which — so we inspect the image's own Config.Env
        // here and tag every row that matches a default with
        // `is_image_default: true`. The WebUI then renders those rows
        // greyed out with an "Override" button instead of an "x",
        // so the user can see what the image provides and only opts
        // into overriding what they actually want to change.
        //
        // If the image inspect fails we fall back to flagging nothing
        // — the Edit form will look like the pre-fix behaviour (image
        // defaults shown as user-set), which is at worst noisy. The
        // failure is logged so it's grep-able.
        let image_env_defaults: HashMap<String, String> =
            match self.docker()?.inspect_image(&image).await {
                Ok(img) => img
                    .config
                    .and_then(|c| c.env)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|e| {
                        e.split_once('=')
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                    })
                    .collect(),
                Err(e) => {
                    warn!(
                        "apps.get_config: image inspect for '{image}' failed: {e} \
                         — Edit will not distinguish image-default env entries"
                    );
                    HashMap::new()
                }
            };
        let env: Vec<AppEnv> = config
            .env
            .unwrap_or_default()
            .iter()
            .filter_map(|e| {
                let (k, v) = e.split_once('=')?;
                let is_image_default = image_env_defaults.get(k).map(|d| d == v).unwrap_or(false);
                Some(AppEnv {
                    name: k.to_string(),
                    value: v.to_string(),
                    is_image_default,
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
        // Managed-network attachment + the *requested* static IP (label,
        // not the live address) so a reinstall re-applies it verbatim.
        let network = config
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_APP_NETWORK))
            .cloned();
        let static_ip = config
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_APP_NETWORK_IP))
            .cloned();

        Ok(AppConfig {
            name: name.to_string(),
            image,
            ports,
            env,
            volumes,
            cpu_limit,
            memory_limit,
            allow_unsafe,
            network,
            static_ip,
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
            // Tear down any partially created containers before removing the dir.
            // `try_run` logs failures so a stuck container that prevents the
            // dir-removal from completing is debuggable.
            nasty_common::cmd::try_run(
                "docker",
                &[
                    "compose",
                    "-f",
                    &compose_path,
                    "--project-name",
                    &name,
                    "down",
                    "-v",
                    "--remove-orphans",
                ],
            )
            .await;
            if let Err(e) = tokio::fs::remove_dir_all(&project_dir).await {
                tracing::warn!("cleanup: remove_dir_all({project_dir}) failed: {e}");
            }
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
                    subdomain: None,
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

        if let Err(e) = self.ingress_remove(name).await {
            warn!("ingress_remove({name}) failed during compose app removal: {e}");
        }

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
    //
    // Per-app `/apps/<name>/` ingress lives in Caddy's admin-API
    // config — POST to install, DELETE by `@id` to remove.  The
    // engine doesn't keep its own source-of-truth file; what Caddy
    // has IS the truth.  See `engine/nasty-apps/src/caddy.rs`.

    pub async fn ingress_list(&self) -> Result<Vec<AppIngress>, AppsError> {
        let routes = CaddyApi::new()
            .list_app_routes()
            .await
            .map_err(AppsError::CommandFailed)?;
        Ok(routes
            .into_iter()
            .map(|r| AppIngress {
                path: format!("/apps/{}/", r.name),
                name: r.name,
                host_port: r.host_port,
                subdomain: r.subdomain,
            })
            .collect())
    }

    /// Every route Caddy is serving — engine-owned app ingresses plus
    /// the Caddyfile-baked WebUI / API / WS routes — for the Ingress
    /// overview page. Read-only; no per-row mutation, the operator
    /// changes app routes through `apps.ingress.set` and static routes
    /// through the NixOS config.
    pub async fn list_caddy_routes(&self) -> Result<Vec<CaddyRouteSummary>, AppsError> {
        CaddyApi::new()
            .list_all_route_summaries()
            .await
            .map_err(AppsError::CommandFailed)
    }

    /// At engine startup, push the engine-known ingress set to Caddy.
    /// Caddy's admin-API config is in-memory — on `systemctl restart
    /// caddy` (or a fresh boot where Caddy comes up before the engine
    /// has had a chance to reapply) our routes vanish.  This pass
    /// makes Docker / labelled-container state the source of truth
    /// and pushes the resulting set to Caddy.
    ///
    /// Best-effort: log and continue on failure.  Engine startup
    /// must not block on Caddy being healthy.
    pub async fn reconcile_app_routes(&self) {
        let api = CaddyApi::new();
        if let Err(e) = api.wait_ready(30).await {
            warn!(
                "apps: Caddy admin API not ready ({e}); skipping ingress \
                 reconcile — apps will keep working but `/apps/<name>/` \
                 routes won't until the next engine restart"
            );
            return;
        }

        let desired = self.compute_desired_routes().await;
        if desired.is_empty() {
            info!("apps: no ingress routes to reconcile");
            return;
        }
        info!(
            "apps: reconciling {} ingress route(s) into Caddy",
            desired.len()
        );
        if let Err(e) = api.replace_app_routes(&desired).await {
            warn!("apps: ingress reconcile failed: {e}");
        }
    }

    /// Source-of-truth resolver for what app ingresses should exist:
    /// ask Docker about managed-labelled containers and derive the
    /// route set from their host-port mappings.
    async fn compute_desired_routes(&self) -> Vec<AppRoute> {
        if let Ok(apps) = self.list().await {
            let mut routes = Vec::new();
            for app in apps {
                // The post-install probe disables path-prefix ingress
                // for haze-class apps that emit absolute root-path
                // assets — that's `proxy_disabled_reason` being set.
                // BUT: the reason only describes a failure in
                // path-prefix mode. If the operator later set a
                // subdomain ingress, that mode serves the app at its
                // own root and bypasses the absolute-path-asset
                // problem entirely. In that case the manifest is the
                // source of truth and the probe's verdict is stale.
                //
                // Skip the reconcile only when proxy_disabled_reason
                // is set AND the operator hasn't opted into subdomain
                // mode. Honouring a persisted subdomain here is what
                // makes "Path-prefix → subdomain later" survive a
                // reboot (issue #247): without it, the subdomain
                // disappears on every restart because the probe-set
                // reason wins over the manifest.
                let subdomain = load_ingress_subdomain(&app.name).await;
                if app.proxy_disabled_reason.is_some() && subdomain.is_none() {
                    info!(
                        "apps: '{}' has proxy_disabled_reason and no subdomain — skipping ingress reconcile",
                        app.name
                    );
                    continue;
                }
                let Some(port) = app
                    .ports
                    .iter()
                    .find(|p| p.protocol.eq_ignore_ascii_case("tcp"))
                    .map(|p| p.host_port)
                else {
                    continue;
                };
                routes.push(AppRoute {
                    name: app.name,
                    host_port: port,
                    subdomain,
                });
            }
            return routes;
        }
        Vec::new()
    }

    pub async fn ingress_set(&self, req: SetIngressRequest) -> Result<AppIngress, AppsError> {
        // Treat "" as None — the WebUI submits an empty string when
        // the operator clears the field rather than omitting it.
        let subdomain = req
            .subdomain
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(ref host) = subdomain {
            validate_subdomain(host).map_err(AppsError::CommandFailed)?;
        }
        let route = AppRoute {
            name: req.name.clone(),
            host_port: req.host_port,
            subdomain: subdomain.clone(),
        };
        CaddyApi::new()
            .set_app_route(&route)
            .await
            .map_err(AppsError::CommandFailed)?;
        // Persist subdomain choice so the reconcile path
        // (engine restart) rebuilds the same Caddy shape rather than
        // silently downgrading to path-prefix mode.
        if let Err(e) = save_ingress_subdomain(&req.name, subdomain.as_deref()).await {
            warn!(
                "Could not persist ingress_subdomain for '{}': {e} — \
                 the route is set in Caddy but may revert on engine restart",
                req.name
            );
        }
        // Switching from path-prefix to subdomain mode invalidates
        // any prior `proxy_disabled_reason` the post-install probe
        // may have recorded: that reason describes a failure in
        // path-prefix mode (absolute root-path assets that don't
        // route through `/apps/<name>/`), and subdomain mode serves
        // the app at its own root where the problem doesn't apply.
        // Clearing it here is what makes the "install with default
        // path-prefix, fail the probe, then move to subdomain"
        // workflow survive a reboot — without this, the reconcile
        // pass keeps seeing the stale reason and skipping the
        // subdomain ingress (issue #247).
        if subdomain.is_some()
            && let Err(e) = clear_proxy_disabled_reason(&req.name).await
        {
            warn!(
                "Could not clear proxy_disabled_reason for '{}': {e} — \
                 the subdomain route is set in Caddy but reconcile may \
                 still skip it on engine restart",
                req.name
            );
        }
        info!(
            "Ingress set for '{}' -> port {} (mode: {})",
            req.name,
            req.host_port,
            match subdomain {
                Some(ref h) => h.as_str(),
                None => "/apps/<name>/",
            }
        );
        Ok(AppIngress {
            path: format!("/apps/{}/", req.name),
            name: req.name,
            host_port: req.host_port,
            subdomain,
        })
    }

    pub async fn ingress_remove(&self, name: &str) -> Result<(), AppsError> {
        // Check existence first so the API contract (404 on
        // unknown) stays the same — caddy's DELETE is idempotent
        // by design (404 → Ok).
        let existing = self.ingress_list().await?;
        if !existing.iter().any(|r| r.name == name) {
            return Err(AppsError::AppNotFound(name.to_string()));
        }
        CaddyApi::new()
            .remove_app_route(name)
            .await
            .map_err(AppsError::CommandFailed)?;
        // Clear the persisted subdomain so the next install / ingress_set
        // on the same name starts from path-prefix default rather than
        // inheriting whatever the previous tenant had.
        if let Err(e) = save_ingress_subdomain(name, None).await {
            warn!("ingress_remove: could not clear ingress_subdomain for '{name}': {e}");
        }
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
                Err(e) => {
                    // Per the doc comment above: stat hiccups other
                    // than ENOENT are deliberately not flagged to the
                    // user (don't block a deploy on a transient
                    // permission error). But the operator should at
                    // least see WHY a path the WebUI shows as fine
                    // would fail at docker-run time — log it.
                    warn!("check_devices: stat({path}) failed: {e}; treating as existing");
                }
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
                Err(e) => {
                    // Same rationale as check_devices: don't block a
                    // deploy on a non-ENOENT stat hiccup, but log so
                    // the operator can correlate a later docker-run
                    // permission error with the silent skip here.
                    warn!(
                        "check_volumes: stat({}) failed: {e}; skipping permission check",
                        bind.host_path
                    );
                }
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
        let meta = inspect_image_metadata(image)
            .await
            .map_err(|e| AppsError::CommandFailed(format!("image inspect failed: {e}")))?;
        Ok(ImageInspectResult {
            ports: meta.ports,
            volumes: meta.volumes,
            user: meta.user,
            subpath_recipe: meta.subpath_recipe,
        })
    }

    // ── Restore on boot ─────────────────────────────────────

    pub async fn restore(&self) {
        if !self.is_enabled() {
            return;
        }
        // #424: never start Docker against a dangling data-root symlink.
        // v0.0.10 points /var/lib/docker at the apps bcachefs FS; if that
        // FS isn't mounted/unlocked yet at boot, dockerd crash-loops on
        // `mkdir /var/lib/docker: file exists` and wedges the apps UI. The
        // enable path runs configure_docker_data_root first; this boot path
        // did not — so guard it here and surface the cause instead of
        // looping into start-limit-hit.
        //
        // This guard only covers the start *we* drive. docker.service is
        // TriggeredBy=docker.socket, so a client connecting to the socket
        // can socket-activate dockerd behind our back — that path is
        // backstopped by `ConditionPathIsDirectory=/var/lib/docker` on
        // docker.service (nixos/modules/nasty.nix), which skips the unit
        // cleanly when the symlink dangles. This log line is the operator-
        // facing half: it explains *why* (filesystem not mounted/unlocked).
        if let Err(target) = docker_data_root_status(Path::new("/var/lib/docker")) {
            error!(
                "Apps enabled but Docker data-root /var/lib/docker -> {target} does not resolve \
                 — the apps filesystem is not mounted (failed mount or still-locked encrypted FS). \
                 Not starting Docker to avoid a crash loop; unlock/mount the filesystem, then \
                 re-enable apps or reboot."
            );
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
                // `try_run` logs failures so a compose app that won't
                // come back up after reboot is debuggable from the
                // journal — the previous `let _ =` form left this case
                // completely silent.
                nasty_common::cmd::try_run(
                    "docker",
                    &[
                        "compose",
                        "-f",
                        &path,
                        "--project-name",
                        &name,
                        "up",
                        "-d",
                        "--no-build",
                    ],
                )
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
        let docker = self.docker()?;

        // Short-circuit when the image is already in the local store.
        // Skips a round-trip to the registry every install, lets
        // airgapped boxes install from `docker load`-imported tarballs,
        // and lets the appliance-smoke nixosTest run without network.
        if docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }

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

        docker
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

            // Preserve subdomain mode across pull-and-reinstall. AppConfig
            // doesn't carry the ingress shape (it lives separately) so we
            // read it back from the manifest here — without this, every
            // `apps.pull` would silently downgrade a subdomain-mode ingress
            // to path-prefix.
            let existing_subdomain = load_ingress_subdomain(name).await;
            let req = InstallAppRequest {
                name: name.to_string(),
                image,
                ports: config.ports,
                env: config.env,
                volumes: config.volumes,
                cpu_limit: config.cpu_limit,
                memory_limit: config.memory_limit,
                allow_unsafe: config.allow_unsafe,
                subdomain: existing_subdomain,
                // Preserve the managed-network attachment + static IP across
                // pull-and-reinstall (else apps.pull would detach the app).
                network: config.network,
                static_ip: config.static_ip,
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

/// Fold one container's stats frame into the running aggregate for its
/// app. Implements the same CPU-percent + memory-working-set + I/O
/// derivations that `docker stats` uses, so users see numbers that
/// match the CLI.
fn accumulate_stats(out: &mut AppStats, frame: &bollard::models::ContainerStatsResponse) {
    // CPU %: ((cpu_delta / system_delta) * num_cpus * 100).  num_cpus
    // falls back to percpu_usage.len() on cgroup v1 where online_cpus
    // is sometimes absent.  precpu values are zero on the first sample
    // ever taken — the result then evaluates to 0 and the next poll
    // shows a real value, which is acceptable.
    if let (Some(cpu), Some(precpu)) = (frame.cpu_stats.as_ref(), frame.precpu_stats.as_ref()) {
        let cpu_total = cpu
            .cpu_usage
            .as_ref()
            .and_then(|u| u.total_usage)
            .unwrap_or(0);
        let pre_total = precpu
            .cpu_usage
            .as_ref()
            .and_then(|u| u.total_usage)
            .unwrap_or(0);
        let sys = cpu.system_cpu_usage.unwrap_or(0);
        let pre_sys = precpu.system_cpu_usage.unwrap_or(0);
        let cpu_delta = cpu_total.saturating_sub(pre_total) as f64;
        let sys_delta = sys.saturating_sub(pre_sys) as f64;
        let num_cpus = cpu.online_cpus.unwrap_or_else(|| {
            cpu.cpu_usage
                .as_ref()
                .and_then(|u| u.percpu_usage.as_ref().map(|v| v.len() as u32))
                .unwrap_or(1)
        }) as f64;
        if cpu_delta > 0.0 && sys_delta > 0.0 {
            out.cpu_percent += (cpu_delta / sys_delta) * num_cpus * 100.0;
        }
    }

    // Memory working set: usage minus page-cache (cgroup v1 "cache" /
    // cgroup v2 "inactive_file"), matching `docker stats`. Fall back to
    // raw usage if the breakdown isn't present.
    if let Some(mem) = frame.memory_stats.as_ref() {
        let usage = mem.usage.unwrap_or(0);
        let cache_like = mem
            .stats
            .as_ref()
            .and_then(|m| m.get("inactive_file").or_else(|| m.get("cache")).copied())
            .unwrap_or(0);
        out.memory_bytes += usage.saturating_sub(cache_like);
        out.memory_limit_bytes = out.memory_limit_bytes.max(mem.limit.unwrap_or(0));
    }

    if let Some(nets) = frame.networks.as_ref() {
        for net in nets.values() {
            out.net_rx_bytes += net.rx_bytes.unwrap_or(0);
            out.net_tx_bytes += net.tx_bytes.unwrap_or(0);
        }
    }

    // cgroup v2 only populates io_service_bytes_recursive; bucket by
    // the `op` field which Docker normalises to "read" / "write".
    if let Some(blkio) = frame.blkio_stats.as_ref()
        && let Some(entries) = blkio.io_service_bytes_recursive.as_ref()
    {
        for e in entries {
            let v = e.value.unwrap_or(0);
            match e.op.as_deref() {
                Some(op) if op.eq_ignore_ascii_case("read") => out.block_read_bytes += v,
                Some(op) if op.eq_ignore_ascii_case("write") => out.block_write_bytes += v,
                _ => {}
            }
        }
    }
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

/// Whether Docker's data-root is safe to start dockerd against.
///
/// v0.0.10 (`configure_docker_data_root`) replaced `/var/lib/docker`
/// with a symlink onto the apps bcachefs filesystem. When that
/// filesystem isn't mounted at boot — a failed mount, a still-locked
/// encrypted FS, a slow/absent device — the symlink dangles. dockerd's
/// startup `os.MkdirAll("/var/lib/docker")` then stat-fails the target
/// and falls through to `mkdir()` on the link inode itself, returning
/// `mkdir /var/lib/docker: file exists`; systemd retries, hits
/// `start-limit-hit`, and the apps UI wedges (#424).
///
/// The enable path guards this by running `configure_docker_data_root`
/// first; the boot `restore()` path historically did not. This check
/// closes that gap so `restore()` starts Docker only when the data-root
/// is a real directory or a symlink whose target resolves.
///
/// Returns `Err(target)` carrying the unresolved link target when the
/// data-root is a dangling symlink; `Ok(())` otherwise — a real dir, a
/// resolving symlink, or an absent path (dockerd then creates a plain
/// dir on the root FS itself, same as a fresh pre-v0.0.10 install).
fn docker_data_root_status(docker_lib: &Path) -> Result<(), String> {
    let Ok(link_meta) = std::fs::symlink_metadata(docker_lib) else {
        // Absent — dockerd creates it. Safe.
        return Ok(());
    };
    if !link_meta.file_type().is_symlink() {
        // Plain directory (pre-v0.0.10 layout, or non-bcachefs box). Safe.
        return Ok(());
    }
    // Symlink: only safe if the target resolves. `metadata` follows the
    // link, so an error here means the target is missing (dangling).
    if std::fs::metadata(docker_lib).is_ok() {
        return Ok(());
    }
    let target = std::fs::read_link(docker_lib)
        .map(|t| t.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<unreadable>".to_string());
    Err(target)
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

struct ImageMetadata {
    ports: Vec<AppPort>,
    volumes: Vec<AppVolume>,
    user: Option<String>,
    subpath_recipe: Option<SubPathRecipe>,
}

/// Look up a sub-path recipe for the given normalized image repo (e.g.
/// `grafana/grafana`, `vaultwarden/server`, `library/redis`). Matching
/// on the repo path rather than the full image ref means tagged
/// variants (`grafana/grafana:11.5`), the OCI registry prefix, and
/// the `:latest` shortcut all share the same recipe.
///
/// Keep this table small and audited: every entry is a vouched-for
/// recipe with values verified against the upstream's own docs. When
/// in doubt, *don't* add an entry — leaving an app to fall through to
/// `probe_proxy_compat` (which disables ingress with a clear reason)
/// is strictly safer than guessing env values that might
/// silently break the app behind the scenes.
fn match_subpath_recipe(repo: &str) -> Option<SubPathRecipe> {
    match repo {
        // Grafana: serves correctly behind a sub-path when both vars
        // are set. Using `%(protocol)s`/`%(domain)s` lets Grafana fill
        // in scheme + host from its own [server] section at render
        // time, so the recipe doesn't have to know the operator's
        // hostname. `serve_from_sub_path` is the toggle that actually
        // makes Grafana strip the prefix when matching internal routes.
        // Source: https://grafana.com/tutorials/run-grafana-behind-a-proxy/
        "grafana/grafana" | "grafana/grafana-oss" | "grafana/grafana-enterprise" => {
            Some(SubPathRecipe {
                display_name: "Grafana sub-path mode".to_string(),
                env: vec![
                    AppEnv {
                        name: "GF_SERVER_ROOT_URL".to_string(),
                        value: "%(protocol)s://%(domain)s/apps/{name}/".to_string(),
                        is_image_default: false,
                    },
                    AppEnv {
                        name: "GF_SERVER_SERVE_FROM_SUB_PATH".to_string(),
                        value: "true".to_string(),
                        is_image_default: false,
                    },
                ],
            })
        }
        // Vaultwarden: DOMAIN is the *full* URL (scheme + host + path,
        // no trailing slash). It's used for CSRF/origin checks and
        // WebAuthn rpId, so it has to match exactly what the browser
        // sees — that's why we template both scheme and host and let
        // the WebUI fill them in from window.location rather than
        // hard-coding http://. Source:
        // https://github.com/dani-garcia/vaultwarden/wiki/Proxy-examples
        "vaultwarden/server" => Some(SubPathRecipe {
            display_name: "Vaultwarden sub-path mode".to_string(),
            env: vec![AppEnv {
                name: "DOMAIN".to_string(),
                value: "{scheme}://{host}/apps/{name}".to_string(),
                is_image_default: false,
            }],
        }),
        _ => None,
    }
}

async fn inspect_image_metadata(image: &str) -> Result<ImageMetadata, String> {
    let (registry, repo, tag) = parse_image_ref(image);
    let client = reqwest::Client::new();

    let registry_url = if registry.starts_with("http") {
        registry.clone()
    } else {
        format!("https://{registry}")
    };

    // Fetch an anonymous bearer token for registries that require one even
    // for public images. We previously hard-coded the Docker Hub flow
    // (auth.docker.io / service=registry.docker.io), which meant any
    // ghcr.io / quay.io / etc. image silently 401'd — the unauth response
    // came back as an error JSON without `config.digest`, and the user
    // saw "no config digest in manifest" with no ports prefilled. This
    // generalises to any registry that publishes the standard Bearer
    // realm at `https://<registry>/token?…`, which is what Docker Hub,
    // ghcr.io, and quay.io all do.
    let token = fetch_registry_token(&client, &registry, &repo).await;

    // Fetch manifest. Accept both single-arch manifests and multi-arch
    // manifest lists / OCI indexes; for the latter we pick the linux/amd64
    // entry and re-fetch its manifest.
    let manifest_url = format!("{registry_url}/v2/{repo}/manifests/{tag}");
    let manifest = fetch_manifest_json(&client, &manifest_url, token.as_deref()).await?;
    let manifest = match manifest["manifests"].as_array() {
        Some(entries) => {
            // Multi-arch list: pick linux/amd64 (matches NASty's target).
            let chosen = entries
                .iter()
                .find(|m| {
                    m["platform"]["os"].as_str() == Some("linux")
                        && m["platform"]["architecture"].as_str() == Some("amd64")
                })
                .or_else(|| entries.first())
                .ok_or("manifest list is empty")?;
            let digest = chosen["digest"]
                .as_str()
                .ok_or("manifest list entry missing digest")?;
            let sub_url = format!("{registry_url}/v2/{repo}/manifests/{digest}");
            fetch_manifest_json(&client, &sub_url, token.as_deref()).await?
        }
        None => manifest,
    };

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

    // ── ExposedPorts ──────────────────────────────────────────
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

    // ── Volumes ───────────────────────────────────────────────
    // `VOLUME /var/lib/haze` in the Dockerfile lands here. The WebUI
    // prefills these as Volume rows so the user doesn't have to know which
    // paths the image needs to be persistent — which is the exact gap that
    // had haze crash-looping on an empty install (SQLite couldn't open
    // /var/lib/haze/haze.sqlite because the path lived on the writable layer
    // owned by root, not on a bind mount).
    let declared_volumes = config["config"]["Volumes"]
        .as_object()
        .or_else(|| config["container_config"]["Volumes"].as_object());
    let mut volumes = Vec::new();
    if let Some(vol_map) = declared_volumes {
        for (mount_path, _) in vol_map {
            // Synthesize a friendly name from the last path segment
            // (e.g. /var/lib/haze → "haze", /data → "data").
            let name = mount_path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("data")
                .to_string();
            volumes.push(AppVolume {
                name,
                mount_path: mount_path.clone(),
                host_path: String::new(),
            });
        }
    }
    volumes.sort_by(|a, b| a.mount_path.cmp(&b.mount_path));

    // ── User ──────────────────────────────────────────────────
    let user = config["config"]["User"]
        .as_str()
        .or_else(|| config["container_config"]["User"].as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    // ── Sub-path recipe ──────────────────────────────────────
    // Look up by repo (e.g. `grafana/grafana`). The `library/` prefix
    // that parse_image_ref adds for bare Docker Hub images is stripped
    // back out here so a hypothetical `library/foo` recipe wouldn't be
    // necessary (it'd just be `foo` in the table).
    let lookup_repo = repo.strip_prefix("library/").unwrap_or(&repo);
    let subpath_recipe = match_subpath_recipe(lookup_repo);

    Ok(ImageMetadata {
        ports,
        volumes,
        user,
        subpath_recipe,
    })
}

/// Ask a registry for an anonymous pull token. Best-effort: returns
/// `None` if the registry isn't one we know how to talk to or the token
/// endpoint doesn't respond. The caller then makes the manifest request
/// without auth, which works for genuinely open registries and produces
/// a clearer 401 for ones that need auth we don't have.
///
/// Each registry hosts its own token endpoint at a different path; we
/// keep a small table rather than chasing `WWW-Authenticate` headers
/// from a 401 (which would be the technically correct approach but
/// doubles the round-trips for the common case).
async fn fetch_registry_token(
    client: &reqwest::Client,
    registry: &str,
    repo: &str,
) -> Option<String> {
    let token_url = match registry {
        "registry-1.docker.io" => format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"
        ),
        "ghcr.io" => format!("https://ghcr.io/token?service=ghcr.io&scope=repository:{repo}:pull"),
        "quay.io" => {
            format!("https://quay.io/v2/auth?service=quay.io&scope=repository:{repo}:pull")
        }
        _ => return None,
    };
    let resp: serde_json::Value = client
        .get(&token_url)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    // Both `token` and `access_token` are seen in the wild — ghcr returns
    // `token`, some Docker Hub flows have returned `access_token`. Try both.
    resp["token"]
        .as_str()
        .or_else(|| resp["access_token"].as_str())
        .map(String::from)
}

async fn fetch_manifest_json(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut req = client.get(url).header(
        "Accept",
        "application/vnd.oci.image.index.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.v2+json",
    );
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{AppVolume, docker_data_root_status, validate_simple_volumes};
    use std::path::PathBuf;

    /// A process-unique scratch path under the temp dir. The crate has
    /// no tempfile dev-dep; this keeps tests self-contained.
    fn unique_tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("nasty-apps-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn data_root_absent_is_ok() {
        // Fresh box: no /var/lib/docker yet — dockerd creates it.
        let missing = unique_tmp("absent").join("docker");
        assert!(docker_data_root_status(&missing).is_ok());
    }

    #[test]
    fn data_root_plain_dir_is_ok() {
        // Pre-v0.0.10 / non-bcachefs layout: a real directory.
        let dir = unique_tmp("plaindir");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(docker_data_root_status(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_root_resolving_symlink_is_ok() {
        // v0.0.10 happy path: symlink → an existing dir on a mounted FS.
        let base = unique_tmp("goodlink");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("apps-docker");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("docker");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(docker_data_root_status(&link).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn data_root_dangling_symlink_is_err() {
        // #424: FS not mounted/unlocked → symlink target absent. This is
        // the case that must NOT start Docker.
        let base = unique_tmp("danglink");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("not-mounted/apps-docker");
        let link = base.join("docker");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = docker_data_root_status(&link).expect_err("dangling link must be rejected");
        assert!(err.contains("not-mounted/apps-docker"), "got: {err}");
        let _ = std::fs::remove_dir_all(&base);
    }

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

    // ── parse_image_user ──

    use super::parse_image_user;

    #[test]
    fn parse_image_user_empty_is_none() {
        assert_eq!(parse_image_user(""), None);
        assert_eq!(parse_image_user("   "), None);
    }

    #[test]
    fn parse_image_user_root_is_none() {
        // Both spellings — chowning to root is always a no-op so we
        // shouldn't bother walking the dir.
        assert_eq!(parse_image_user("root"), None);
        assert_eq!(parse_image_user("0"), None);
        assert_eq!(parse_image_user("0:0"), None);
    }

    #[test]
    fn parse_image_user_numeric_uid_only() {
        // No group → gid defaults to uid (matches docker's behaviour
        // when only User is set in a Dockerfile).
        assert_eq!(parse_image_user("1000"), Some((1000, 1000)));
    }

    #[test]
    fn parse_image_user_numeric_uid_gid() {
        assert_eq!(parse_image_user("1000:2000"), Some((1000, 2000)));
        assert_eq!(parse_image_user("65532:65532"), Some((65532, 65532)));
    }

    #[test]
    fn parse_image_user_named_nonroot() {
        // Well-known distroless convention — the specific case that
        // brought us here via haze. Both bare and uid:gid spellings
        // resolve to 65532.
        assert_eq!(parse_image_user("nonroot"), Some((65532, 65532)));
        assert_eq!(parse_image_user("nonroot:nonroot"), Some((65532, 65532)));
    }

    #[test]
    fn parse_image_user_named_mixed_uid_gid() {
        // Mix-and-match: numeric uid + named group, or vice versa.
        assert_eq!(parse_image_user("65532:nonroot"), Some((65532, 65532)));
        assert_eq!(parse_image_user("nonroot:65532"), Some((65532, 65532)));
    }

    #[test]
    fn parse_image_user_unknown_named_is_none() {
        // No well-known table entry → we refuse to guess, returning
        // None so install() leaves the dir root-owned and warns.
        assert_eq!(parse_image_user("plex"), None);
        assert_eq!(parse_image_user("ubuntu"), None);
    }

    // ── find_absolute_asset_path ──

    use super::find_absolute_asset_path;

    #[test]
    fn absolute_asset_found_in_link_href() {
        // The exact failure mode from haze: <link href="/favicon.svg"> in
        // HTML served at /apps/haze/. We should pick up `/favicon.svg`
        // because it's an absolute path outside the prefix.
        let html = r#"<link rel="icon" href="/favicon.svg"><script src="/assets/a.js"></script>"#;
        assert_eq!(
            find_absolute_asset_path(html, "/apps/haze/"),
            Some("/favicon.svg".to_string())
        );
    }

    #[test]
    fn relative_paths_are_ok() {
        // Apps that emit relative or prefix-scoped asset paths can serve
        // happily behind /apps/<name>/, so we shouldn't flag them.
        let html = r#"<link href="./style.css"><script src="assets/a.js"></script>"#;
        assert_eq!(find_absolute_asset_path(html, "/apps/haze/"), None);
    }

    #[test]
    fn paths_already_under_prefix_are_ok() {
        // If the upstream is smart enough to honour a base-path config and
        // already emits paths under our prefix, the proxy works as-is.
        let html =
            r#"<link href="/apps/grafana/public/style.css"><script src="/apps/grafana/main.js">"#;
        assert_eq!(find_absolute_asset_path(html, "/apps/grafana/"), None);
    }

    #[test]
    fn protocol_relative_urls_are_ok() {
        // `//cdn.example.com/foo` is an external URL the browser fetches
        // straight from the CDN — not routed through our proxy, so it
        // can't fail the way the haze case does.
        let html = r#"<script src="//cdn.example.com/lib.js"></script>"#;
        assert_eq!(find_absolute_asset_path(html, "/apps/x/"), None);
    }

    #[test]
    fn query_and_fragment_are_stripped_for_comparison() {
        // `/main.js?v=1#x` is the same file as `/main.js`, so the prefix
        // check has to operate on the path part.
        let html = r#"<script src="/main.js?v=1#x"></script>"#;
        assert_eq!(
            find_absolute_asset_path(html, "/apps/foo/"),
            Some("/main.js".to_string())
        );
    }

    #[test]
    fn single_quoted_attributes_work() {
        // HTML allows both quote styles; the scanner has to track which
        // quote opened the value to close on the same one.
        let html = "<script src='/main.js'></script>";
        assert_eq!(
            find_absolute_asset_path(html, "/apps/foo/"),
            Some("/main.js".to_string())
        );
    }

    // ── match_subpath_recipe ──

    use super::match_subpath_recipe;

    #[test]
    fn subpath_recipe_grafana_all_variants() {
        // grafana, grafana-oss, grafana-enterprise are the three image
        // names Grafana ships; all need the same env to serve under a
        // sub-path.
        for repo in [
            "grafana/grafana",
            "grafana/grafana-oss",
            "grafana/grafana-enterprise",
        ] {
            let r = match_subpath_recipe(repo).unwrap_or_else(|| panic!("no recipe for {repo}"));
            assert_eq!(r.display_name, "Grafana sub-path mode");
            let keys: Vec<_> = r.env.iter().map(|e| e.name.as_str()).collect();
            assert!(
                keys.contains(&"GF_SERVER_ROOT_URL"),
                "missing GF_SERVER_ROOT_URL for {repo}"
            );
            assert!(
                keys.contains(&"GF_SERVER_SERVE_FROM_SUB_PATH"),
                "missing GF_SERVER_SERVE_FROM_SUB_PATH for {repo}"
            );
            let root = r
                .env
                .iter()
                .find(|e| e.name == "GF_SERVER_ROOT_URL")
                .unwrap();
            // Must contain the {name} placeholder so the WebUI can
            // substitute the app name (otherwise a Grafana installed as
            // "grafana-prod" would still emit /apps/grafana/ URLs).
            assert!(
                root.value.contains("{name}"),
                "GF_SERVER_ROOT_URL missing {{name}} placeholder"
            );
        }
    }

    #[test]
    fn subpath_recipe_vaultwarden() {
        let r = match_subpath_recipe("vaultwarden/server").expect("vaultwarden recipe missing");
        assert_eq!(r.display_name, "Vaultwarden sub-path mode");
        assert_eq!(r.env.len(), 1);
        assert_eq!(r.env[0].name, "DOMAIN");
        // Both {scheme} and {host} placeholders are required so the
        // WebUI can render the exact origin the browser sees (Vaultwarden
        // CSRF-checks against DOMAIN).
        assert!(r.env[0].value.contains("{scheme}"));
        assert!(r.env[0].value.contains("{host}"));
        assert!(r.env[0].value.contains("{name}"));
        // Trailing slash must NOT be present — Vaultwarden docs are
        // explicit on this, and a trailing slash breaks the CSRF check.
        assert!(!r.env[0].value.ends_with('/'));
    }

    #[test]
    fn subpath_recipe_unknown_returns_none() {
        // Unknown apps fall through to the post-install proxy probe
        // (probe_proxy_compat) — never silently guess.
        assert_eq!(
            match_subpath_recipe("library/redis").map(|r| r.display_name),
            None
        );
        assert_eq!(
            match_subpath_recipe("ghcr.io/consi/haze").map(|r| r.display_name),
            None
        );
        assert_eq!(match_subpath_recipe("").map(|r| r.display_name), None);
    }

    // ── validate_subdomain ──

    use super::validate_subdomain;

    #[test]
    fn validate_subdomain_accepts_normal_fqdns() {
        // The common-case operator input — anything with a dot and
        // hyphens-not-at-edges should pass. Caddy will pick up the
        // hostname from the match block and obtain a cert via the
        // operator's existing ACME config.
        assert!(validate_subdomain("jellyfin.example.com").is_ok());
        assert!(validate_subdomain("vault.nasty.local").is_ok());
        assert!(validate_subdomain("grafana-prod.lab.example.com").is_ok());
        assert!(validate_subdomain("a.b").is_ok());
    }

    #[test]
    fn validate_subdomain_rejects_single_label() {
        // No dot → likely a user typo. Caddy would technically serve
        // it but the cert story collapses (no TLD = no ACME), so we
        // reject and surface a clear message.
        let err = validate_subdomain("jellyfin").unwrap_err();
        assert!(err.contains("fully-qualified"), "msg was: {err}");
    }

    #[test]
    fn validate_subdomain_rejects_empty_and_garbage() {
        assert!(validate_subdomain("").is_err());
        assert!(validate_subdomain("a..b").is_err()); // empty label
        assert!(validate_subdomain("foo.example.com.").is_err()); // trailing-dot empty label
        assert!(validate_subdomain("-foo.example.com").is_err()); // leading hyphen
        assert!(validate_subdomain("foo-.example.com").is_err()); // trailing hyphen
        assert!(validate_subdomain("foo bar.example.com").is_err()); // space
        assert!(validate_subdomain("foo_bar.example.com").is_err()); // underscore
        assert!(validate_subdomain("foo.example.com:8080").is_err()); // port suffix
        assert!(validate_subdomain("http://foo.example.com").is_err()); // scheme
    }

    #[test]
    fn validate_subdomain_rejects_too_long() {
        // 64-char label
        let too_long_label = format!("{}.example.com", "a".repeat(64));
        assert!(validate_subdomain(&too_long_label).is_err());
        // 254-char total
        let too_long_total = format!("{}.example.com", "a".repeat(254 - ".example.com".len()));
        assert!(validate_subdomain(&too_long_total).is_err());
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    fn ifaces() -> Vec<IfaceInfo> {
        vec![
            IfaceInfo {
                name: "eth0".into(),
                kind: "physical".into(),
                bridge_member: true,
            },
            IfaceInfo {
                name: "br0".into(),
                kind: "bridge".into(),
                bridge_member: false,
            },
            IfaceInfo {
                name: "eth1".into(),
                kind: "physical".into(),
                bridge_member: false,
            },
        ]
    }
    fn net(name: &str, driver: &str) -> ManagedNetwork {
        ManagedNetwork {
            name: name.into(),
            driver: driver.into(),
            parent: None,
            subnet: None,
            gateway: None,
            ip_range: None,
            vlan: None,
            host_shim: false,
            shim_ip: None,
        }
    }
    fn macvlan_on(parent: &str) -> ManagedNetwork {
        let mut n = net("x", "macvlan");
        n.parent = Some(parent.into());
        n
    }

    #[test]
    fn bridge_driver_rejects_parent() {
        let mut n = net("x", "bridge");
        n.parent = Some("br0".into());
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }
    #[test]
    fn macvlan_requires_parent() {
        assert!(validate_network_spec(&net("x", "macvlan"), &ifaces()).is_err());
    }
    #[test]
    fn unknown_parent_rejected() {
        assert!(validate_network_spec(&macvlan_on("eth9"), &ifaces()).is_err());
    }
    #[test]
    fn bridge_member_parent_rejected() {
        // eth0 is enslaved to a bridge — must use the bridge instead.
        assert!(validate_network_spec(&macvlan_on("eth0"), &ifaces()).is_err());
    }
    #[test]
    fn macvlan_on_bridge_ok() {
        let mut n = macvlan_on("br0");
        n.subnet = Some("192.168.1.0/24".into());
        n.gateway = Some("192.168.1.1".into());
        assert!(validate_network_spec(&n, &ifaces()).is_ok());
    }
    #[test]
    fn macvlan_on_standalone_nic_ok() {
        assert!(validate_network_spec(&macvlan_on("eth1"), &ifaces()).is_ok());
    }
    #[test]
    fn gateway_outside_subnet_rejected() {
        let mut n = macvlan_on("br0");
        n.subnet = Some("192.168.1.0/24".into());
        n.gateway = Some("10.0.0.1".into());
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }
    #[test]
    fn ip_range_outside_subnet_rejected() {
        let mut n = macvlan_on("br0");
        n.subnet = Some("192.168.1.0/24".into());
        n.ip_range = Some("10.0.0.0/28".into());
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }
    #[test]
    fn vlan_range_enforced() {
        let mut n = macvlan_on("br0");
        n.vlan = Some(5000);
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }
    #[test]
    fn bad_subnet_rejected() {
        let mut n = macvlan_on("br0");
        n.subnet = Some("not-a-cidr".into());
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }
    fn macvlan_shim(parent: &str) -> ManagedNetwork {
        let mut n = macvlan_on(parent);
        n.subnet = Some("192.168.1.0/24".into());
        n.host_shim = true;
        n.shim_ip = Some("192.168.1.2/24".into());
        n
    }

    #[test]
    fn host_shim_accepted_with_valid_shim_ip() {
        assert!(validate_network_spec(&macvlan_shim("br0"), &ifaces()).is_ok());
    }

    #[test]
    fn host_shim_rejected_without_shim_ip() {
        let mut n = macvlan_shim("br0");
        n.shim_ip = None;
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }

    #[test]
    fn host_shim_rejected_shim_ip_outside_subnet() {
        let mut n = macvlan_shim("br0");
        n.shim_ip = Some("10.9.9.9/24".into());
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }

    #[test]
    fn host_shim_rejected_without_subnet() {
        let mut n = macvlan_shim("br0");
        n.subnet = None;
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }

    #[test]
    fn host_shim_rejected_on_bridge_driver() {
        // bridge driver can't carry a host shim (and forbids a parent anyway).
        let mut n = net("x", "bridge");
        n.host_shim = true;
        n.subnet = Some("192.168.1.0/24".into());
        n.shim_ip = Some("192.168.1.2/24".into());
        assert!(validate_network_spec(&n, &ifaces()).is_err());
    }

    #[test]
    fn cidr_contains_ip_v4() {
        assert_eq!(
            cidr_contains_ip("192.168.1.0/24", "192.168.1.50"),
            Some(true)
        );
        assert_eq!(
            cidr_contains_ip("192.168.1.0/24", "192.168.2.50"),
            Some(false)
        );
        assert_eq!(cidr_contains_ip("10.0.0.0/8", "10.255.1.2"), Some(true));
        assert_eq!(cidr_contains_ip("192.168.1.0/24", "nope"), None);
    }
    #[test]
    fn cidr_contains_net_v4() {
        assert_eq!(
            cidr_contains_net("192.168.1.0/24", "192.168.1.64/27"),
            Some(true)
        );
        // less-specific inner can't be contained
        assert_eq!(
            cidr_contains_net("192.168.1.0/24", "192.168.0.0/23"),
            Some(false)
        );
        assert_eq!(
            cidr_contains_net("192.168.1.0/24", "10.0.0.0/28"),
            Some(false)
        );
    }
    #[test]
    fn cidr_family_mismatch_is_false() {
        assert_eq!(cidr_contains_ip("192.168.1.0/24", "fd00::1"), Some(false));
        assert_eq!(cidr_contains_ip("fd00::/64", "fd00::1"), Some(true));
    }

    #[test]
    fn ingress_incompatible_only_lan_ip_with_ports() {
        assert!(ingress_incompatible("macvlan", true).is_some());
        assert!(ingress_incompatible("ipvlan", true).is_some());
        assert!(ingress_incompatible("macvlan", false).is_none());
        assert!(ingress_incompatible("bridge", true).is_none());
    }

    #[test]
    fn reconcile_recreates_missing_skips_vanished_parent() {
        let persisted = vec![macvlan_named("present"), macvlan_named("missing"), {
            let mut n = macvlan_named("orphan");
            n.parent = Some("gone0".into());
            n
        }];
        let docker_names = vec!["present".to_string()];
        let iface_names = vec!["br0".to_string(), "eth1".to_string()];
        let plan = reconcile_plan(&persisted, &docker_names, &iface_names);
        assert_eq!(plan.to_create, vec!["missing".to_string()]);
        assert_eq!(plan.skipped_missing_parent, vec!["orphan".to_string()]);
    }
    fn macvlan_named(name: &str) -> ManagedNetwork {
        let mut n = net(name, "macvlan");
        n.parent = Some("br0".into());
        n
    }

    #[test]
    fn serde_omits_unset_optionals_and_roundtrips() {
        let n = net("x", "bridge");
        let j = serde_json::to_string(&n).unwrap();
        assert!(!j.contains("\"parent\""));
        assert!(!j.contains("\"subnet\""));
        assert_eq!(serde_json::from_str::<ManagedNetwork>(&j).unwrap(), n);
        // Minimal/legacy JSON loads with defaults.
        let min: ManagedNetwork =
            serde_json::from_str(r#"{"name":"y","driver":"macvlan","parent":"br0"}"#).unwrap();
        assert!(!min.host_shim);
        assert!(min.subnet.is_none());
    }
}
