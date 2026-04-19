//! App runtime management — Docker-based container management via bollard.
//!
//! Two modes:
//! - **Simple**: single-container apps configured via the WebUI form
//!   (image, ports, env, volumes) — managed directly through the Docker API.
//! - **Compose**: multi-container apps from a user-provided docker-compose.yml
//!   — managed via the `docker compose` CLI.
//!
//! All NASty-managed containers are labeled with `nasty.managed=true` so we
//! can distinguish them from other containers on the host.

use std::collections::HashMap;
use std::path::Path;

use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, ListContainersOptions, LogsOptions,
    RemoveContainerOptions, StopContainerOptions,
};
use bollard::Docker;
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
        }
    }
}

impl From<bollard::errors::Error> for AppsError {
    fn from(e: bollard::errors::Error) -> Self {
        Self::DockerFailed(e.to_string())
    }
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
    /// Container image.
    pub image: String,
    /// Current status: "running", "stopped", "restarting", "created", "exited".
    pub status: String,
    /// ISO 8601 timestamp of when the container was created.
    pub created: String,
    /// App kind: "simple" or "compose".
    pub kind: String,
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

// ── Service ─────────────────────────────────────────────────────

pub struct AppsService {
    docker: Docker,
}

impl AppsService {
    pub fn new() -> Self {
        // Connect to Docker socket. If Docker isn't running yet, individual
        // operations will fail with a clear error rather than crashing at startup.
        let docker = Docker::connect_with_unix_defaults()
            .unwrap_or_else(|_| Docker::connect_with_unix("/var/run/docker.sock", 120, &bollard::API_DEFAULT_VERSION).unwrap());
        Self { docker }
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
                if let Ok(docker) = Docker::connect_with_unix_defaults() {
                    if docker.ping().await.is_ok() {
                        ready = true;
                        break;
                    }
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
                if app.status == "running" {
                    let _ = self.docker.stop_container(
                        &container_name(&app.name),
                        Some(StopContainerOptions { t: Some(10), signal: None }),
                    ).await;
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
            };
        }

        let (apps_result, docker_version) =
            tokio::join!(self.list_internal(), self.docker_version());
        let app_count = apps_result.map(|a| a.len()).unwrap_or(0);

        AppsStatus {
            enabled,
            running,
            app_count,
            memory_bytes: None, // TODO: aggregate container stats if needed
            storage_path,
            storage_ok,
            docker_version,
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

        // Pull the image first
        self.pull_image(&req.image).await?;

        // Build port bindings
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        let mut exposed_ports: Vec<String> = Vec::new();

        for p in &req.ports {
            let key = format!("{}/{}", p.container_port, p.protocol.to_lowercase());
            exposed_ports.push(key.clone());
            port_bindings.insert(
                key,
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: p.host_port.map(|hp| hp.to_string()),
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
        let env: Vec<String> = req.env.iter().map(|e| format!("{}={}", e.name, e.value)).collect();

        // Resource limits
        let nano_cpus = req.cpu_limit.as_ref().and_then(|c| parse_cpu_limit(c));
        let memory = req.memory_limit.as_ref().and_then(|m| parse_memory_limit(m));

        // Build labels
        let mut labels = HashMap::new();
        labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
        labels.insert(LABEL_APP_NAME.to_string(), req.name.clone());
        labels.insert(LABEL_APP_KIND.to_string(), "simple".to_string());

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

        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(cname.clone()),
                    platform: String::new(),
                }),
                config,
            )
            .await?;

        self.docker.start_container(&cname, None::<bollard::query_parameters::StartContainerOptions>).await?;

        info!("Installed app '{}' (image: {})", req.name, req.image);

        // Auto-create ingress for the first port
        if let Some(first_port) = req.ports.first() {
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
            .docker
            .stop_container(&cname, Some(StopContainerOptions { t: Some(10), signal: None }))
            .await;
        let _ = self
            .docker
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
            .docker
            .stop_container(&cname, Some(StopContainerOptions { t: Some(10), signal: None }))
            .await;
        self.docker
            .remove_container(
                &cname,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true, // remove anonymous volumes
                    ..Default::default()
                }),
            )
            .await?;

        // Clean up ingress
        let _ = self.ingress_remove(name).await;

        info!("Removed app '{name}'");
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<App>, AppsError> {
        self.require_ready().await?;
        self.list_internal().await
    }

    async fn list_internal(&self) -> Result<Vec<App>, AppsError> {
        // List simple apps (labeled by us)
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_MANAGED}=true")]);

        let labeled = self
            .docker
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
            let status = c
                .state
                .as_ref()
                .map(|s| format!("{:?}", s).to_lowercase())
                .unwrap_or_else(|| "unknown".to_string());
            let image = c.image.as_deref().unwrap_or("").to_string();
            let created = c.created.map(chrono_from_timestamp).unwrap_or_default();

            apps.push(App {
                name: app_name,
                image,
                status,
                created,
                kind,
            });
        }

        // Also discover compose apps from the compose directory
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

                // Find the first container from this compose project
                let mut pf = HashMap::new();
                pf.insert(
                    "label".to_string(),
                    vec![format!("com.docker.compose.project={name}")],
                );
                let compose_containers = self
                    .docker
                    .list_containers(Some(ListContainersOptions {
                        all: true,
                        filters: Some(pf),
                        ..Default::default()
                    }))
                    .await
                    .unwrap_or_default();

                let (status, image, created) = if let Some(c) = compose_containers.first() {
                    (
                        c.state
                            .as_ref()
                            .map(|s| format!("{:?}", s).to_lowercase())
                            .unwrap_or_else(|| "unknown".to_string()),
                        c.image.as_deref().unwrap_or("").to_string(),
                        c.created.map(chrono_from_timestamp).unwrap_or_default(),
                    )
                } else {
                    ("stopped".to_string(), String::new(), String::new())
                };

                seen_names.insert(name.clone());
                apps.push(App {
                    name,
                    image,
                    status,
                    created,
                    kind: "compose".to_string(),
                });
            }
        }

        Ok(apps)
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
            .docker
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
        let mut idx = 0;
        for (key, bindings) in port_source {
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
            idx += 1;
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
        let cpu_limit = host_config.nano_cpus.map(|n| format!("{:.1}", n as f64 / 1_000_000_000.0));
        let memory_limit = host_config.memory.and_then(|m| {
            if m <= 0 {
                None
            } else {
                Some(format_memory_limit(m as u64))
            }
        });

        Ok(AppConfig {
            name: name.to_string(),
            image,
            ports,
            env,
            volumes,
            cpu_limit,
            memory_limit,
        })
    }

    pub async fn logs(&self, name: &str, tail: Option<u32>) -> Result<String, AppsError> {
        self.require_ready().await?;

        let cname = container_name(name);
        let tail_str = tail.unwrap_or(100).to_string();

        let logs = self
            .docker
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

        let output: String = logs.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("");
        Ok(output)
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

        // Write a .env file with NASty labels so containers are tracked
        let env_content = format!(
            "COMPOSE_PROJECT_NAME={name}\n",
            name = req.name,
        );
        tokio::fs::write(format!("{}/.env", project_dir), &env_content).await?;

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
                    "--pull", "missing",
                ])
                .output(),
        )
        .await;

        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                let _ = tokio::fs::remove_dir_all(&project_dir).await;
                return Err(AppsError::CommandFailed(e.to_string()));
            }
            Err(_) => {
                let _ = tokio::fs::remove_dir_all(&project_dir).await;
                return Err(AppsError::DockerFailed(
                    "docker compose timed out after 5 minutes".to_string(),
                ));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Clean up on failure
            let _ = tokio::fs::remove_dir_all(&project_dir).await;
            return Err(AppsError::DockerFailed(stderr.to_string()));
        }

        // Label the containers after creation (docker compose doesn't support
        // DOCKER_DEFAULT_LABELS reliably across all versions)
        self.label_compose_containers(&req.name).await;

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
                    "--pull", "missing",
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

        self.label_compose_containers(&req.name).await;

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
                    if let Some(port_str) = parts[1].strip_prefix("port:") {
                        if let Ok(port) = port_str.parse::<u16>() {
                            rules.push(AppIngress {
                                path: format!("/apps/{name}/"),
                                name,
                                host_port: port,
                            });
                        }
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

    // ── Image inspection ────────────────────────────────────

    pub async fn inspect_image(&self, image: &str) -> Result<ImageInspectResult, AppsError> {
        let ports = inspect_image_ports(image).await.map_err(|e| {
            AppsError::CommandFailed(format!("image inspect failed: {e}"))
        })?;
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
        }
    }

    // ── Internal helpers ────────────────────────────────────

    async fn is_docker_ready(&self) -> bool {
        self.docker.ping().await.is_ok()
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
        let version = self.docker.version().await.ok()?;
        version.version
    }

    async fn container_exists(&self, name: &str) -> bool {
        self.docker.inspect_container(name, None).await.is_ok()
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

        self.docker
            .create_image(Some(options), None, None)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
    }

    /// Look up the host port Docker actually assigned for a given container port.
    async fn get_mapped_port(&self, container: &str, container_port: u16) -> Option<u16> {
        let info = self.docker.inspect_container(container, None).await.ok()?;
        let ports = info.network_settings?.ports?;
        let key = format!("{container_port}/tcp");
        let bindings = ports.get(&key)?.as_ref()?;
        bindings
            .first()?
            .host_port
            .as_ref()?
            .parse::<u16>()
            .ok()
    }

    async fn label_compose_containers(&self, project_name: &str) {
        // Find containers belonging to this compose project and add NASty labels
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("com.docker.compose.project={project_name}")],
        );

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .unwrap_or_default();

        for c in &containers {
            if let Some(_id) = &c.id {
                // Docker doesn't support adding labels to running containers directly,
                // so we need to read existing labels and check if already labeled.
                let existing_labels = c.labels.as_ref();
                if existing_labels
                    .and_then(|l| l.get(LABEL_MANAGED))
                    .is_some()
                {
                    continue;
                }

                // For compose containers, we label them at image/compose level instead.
                // The labels are added via the compose file's labels section in a
                // wrapper .yml that we generate.
                // Fallback: just note that compose containers are identified by the
                // com.docker.compose.project label, which we also filter on in list_internal.
            }
        }

        // Simpler approach: also list by compose project label in list_internal
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

async fn reload_nginx() {
    let _ = Command::new("systemctl")
        .args(["reload", "nginx"])
        .output()
        .await;
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
    if bytes >= 1024 * 1024 * 1024 && bytes % (1024 * 1024 * 1024) == 0 {
        format!("{}g", bytes / (1024 * 1024 * 1024))
    } else if bytes >= 1024 * 1024 {
        format!("{}m", bytes / (1024 * 1024))
    } else {
        format!("{bytes}")
    }
}

/// Convert a Unix timestamp (seconds) to a simple ISO 8601-ish string.
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
            if entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
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

    let apps_path = format!("/fs/{fs_name}/.nasty/apps-data");

    if Path::new(&apps_path).exists() {
        info!("Apps storage already exists at {apps_path}");
        return Some(apps_path);
    }

    match tokio::fs::create_dir_all(&apps_path).await {
        Ok(()) => {
            info!("Created apps storage directory at {apps_path}");
            Some(apps_path)
        }
        Err(e) => {
            error!("Failed to create apps storage at {apps_path}: {e}");
            None
        }
    }
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
            if let Some(port_str) = parts.first() {
                if let Ok(port) = port_str.parse::<u16>() {
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
    }

    ports.sort_by_key(|p| p.container_port);
    Ok(ports)
}
