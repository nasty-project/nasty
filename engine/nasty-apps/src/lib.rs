//! App runtime management — optional k3s + Helm integration.
//!
//! Disabled by default. When enabled, starts a single-node k3s cluster
//! with local-path-provisioner for storage (backed by a bcachefs subvolume).
//! Apps are deployed as Helm releases using the bjw-s app-template chart
//! for simple containers, or raw Helm charts for advanced use cases.

use std::collections::HashSet;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{info, warn, error};

const STATE_PATH: &str = "/var/lib/nasty/apps-enabled";
const KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
const K3S_SERVICE: &str = "k3s.service";
const APP_TEMPLATE_REPO: &str = "https://bjw-s-labs.github.io/helm-charts";
const APP_TEMPLATE_CHART: &str = "app-template";
/// Per-app namespace prefix. Each app gets its own namespace: nasty-{name}.
const NAMESPACE_PREFIX: &str = "nasty-";

fn app_namespace(name: &str) -> String {
    format!("{NAMESPACE_PREFIX}{name}")
}

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum AppsError {
    #[error("apps runtime is not enabled")]
    NotEnabled,
    #[error("apps runtime is already enabled")]
    AlreadyEnabled,
    #[error("k3s is not ready: {0}")]
    NotReady(String),
    #[error("app not found: {0}")]
    AppNotFound(String),
    #[error("app already exists: {0}")]
    AppAlreadyExists(String),
    #[error("helm command failed: {0}")]
    HelmFailed(String),
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
            Self::HelmFailed(_) => -33006,
            Self::CommandFailed(_) => -33007,
            Self::Io(_) => -33008,
        }
    }
}

// ── Types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
pub struct AppsStatus {
    /// Whether the apps runtime is enabled.
    pub enabled: bool,
    /// Whether k3s is currently running.
    pub running: bool,
    /// Number of deployed apps.
    pub app_count: usize,
    /// k3s memory usage in bytes (approximate).
    pub memory_bytes: Option<u64>,
    /// Path to the apps storage subvolume.
    pub storage_path: Option<String>,
    /// k3s version string.
    pub k3s_version: Option<String>,
    /// Node readiness status (e.g. "Ready", "NotReady").
    pub node_status: Option<String>,
    /// Whether the storage subvolume exists on disk.
    pub storage_ok: bool,
}

/// Request to enable the apps runtime.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EnableAppsRequest {
    /// Filesystem to create apps-data subvolume on.
    pub filesystem: Option<String>,
}

/// Persisted apps configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct App {
    /// Helm release name (also used as the app identifier).
    pub name: String,
    /// Namespace (nasty-{name}).
    pub namespace: String,
    /// Container image (e.g. "lscr.io/linuxserver/plex:latest").
    pub image: String,
    /// Helm chart used (e.g. "app-template" or custom).
    pub chart: String,
    /// Current status from Helm.
    pub status: String,
    /// Last updated timestamp.
    pub updated: String,
}

/// Current configuration of an installed app, parsed from Helm values.
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

/// Detected ports from inspecting a container image's EXPOSE directives.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImageInspectResult {
    pub ports: Vec<AppPort>,
}

/// Request to install a simple app via the bjw-s app-template chart.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallAppRequest {
    /// App name (becomes the Helm release name). Must be DNS-safe.
    pub name: String,
    /// Container image (e.g. "lscr.io/linuxserver/plex:latest").
    pub image: String,
    /// Container ports to expose. Key = port name, value = port number.
    #[serde(default)]
    pub ports: Vec<AppPort>,
    /// Environment variables.
    #[serde(default)]
    pub env: Vec<AppEnv>,
    /// Persistent volume claims.
    #[serde(default)]
    pub volumes: Vec<AppVolume>,
    /// CPU limit (e.g. "500m" for half a core, "2" for 2 cores).
    pub cpu_limit: Option<String>,
    /// Memory limit (e.g. "256Mi", "1Gi").
    pub memory_limit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppPort {
    /// Port name (e.g. "http", "webui").
    pub name: String,
    /// Container port number.
    pub container_port: u16,
    /// NodePort to expose on the host (optional, auto-assigned if omitted).
    pub node_port: Option<u16>,
    /// Protocol: "TCP" or "UDP" (default: TCP).
    #[serde(default = "default_tcp")]
    pub protocol: String,
}

fn default_tcp() -> String { "TCP".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppEnv {
    /// Environment variable name.
    pub name: String,
    /// Environment variable value.
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppVolume {
    /// Volume name (e.g. "config", "data").
    pub name: String,
    /// Mount path inside the container.
    pub mount_path: String,
    /// Size (e.g. "1Gi", "10Gi").
    pub size: String,
    /// Storage class name (default: "nasty-nfs").
    #[serde(default = "default_storage_class")]
    pub storage_class: String,
}

fn default_storage_class() -> String { "local-path".to_string() }

/// Request to install a custom Helm chart.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallHelmChartRequest {
    /// Release name.
    pub name: String,
    /// Chart reference (e.g. "bitnami/postgresql" or OCI URL).
    pub chart: String,
    /// Chart version (optional).
    pub version: Option<String>,
    /// Values as a JSON object (converted to YAML for Helm).
    pub values: Option<serde_json::Value>,
}

// ── Helm repo types ─────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddRepoRequest {
    /// Repository name (e.g. "bitnami").
    pub name: String,
    /// Repository URL (e.g. "https://charts.bitnami.com/bitnami").
    pub url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HelmRepo {
    /// Repository name.
    pub name: String,
    /// Repository URL.
    pub url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HelmChart {
    /// Chart name (e.g. "postgresql").
    pub name: String,
    /// Repository name (e.g. "bitnami").
    pub repo: String,
    /// Latest version.
    pub version: String,
    /// App version (e.g. "16.2.0").
    pub app_version: String,
    /// Short description.
    pub description: String,
}

// ── Ingress types ───────────────────────────────────────────────

const PROXY_CONF: &str = "/var/lib/nasty/apps-proxy.conf";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppIngress {
    /// App name.
    pub name: String,
    /// NodePort to proxy to.
    pub node_port: u16,
    /// URL path prefix (e.g. "/apps/plex/").
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetIngressRequest {
    /// App name.
    pub name: String,
    /// NodePort to proxy to.
    pub node_port: u16,
}

/// Active port-forward state.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PortForwardInfo {
    /// App name.
    pub name: String,
    /// Local port on the NASty host.
    pub local_port: u16,
    /// Container port being forwarded.
    pub container_port: u16,
    /// Pod name.
    pub pod: String,
}

// ── Service ─────────────────────────────────────────────────────

pub struct AppsService {
    port_forwards: std::sync::Mutex<std::collections::HashMap<String, (PortForwardInfo, u32)>>,
}

impl AppsService {
    pub fn new() -> Self {
        Self {
            port_forwards: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
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

        // Save config with chosen filesystem
        let config = AppsConfig {
            enabled: true,
            storage_path: None, // Will be set during bootstrap
        };
        Self::save_config(&config).await?;

        // Move k3s data to bcachefs so container images and etcd don't fill the root partition.
        if let Some(ref fs_name) = req.filesystem {
            ensure_k3s_symlink(fs_name).await;
        }

        // Start k3s via systemd (non-blocking — k3s takes 30-60s to initialize)
        run_cmd("systemctl", &["start", K3S_SERVICE]).await?;

        info!("Apps runtime enabled — k3s starting (bootstrap will run in background)");

        let filesystem = req.filesystem.clone();

        // Bootstrap in background — don't block the RPC response
        tokio::spawn(async move {
            // Wait for k3s to be ready (up to 90s)
            let mut ready = false;
            for _ in 0..45 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let output = tokio::process::Command::new("k3s")
                    .args(["kubectl", "--kubeconfig", KUBECONFIG, "get", "nodes", "-o", "name"])
                    .output()
                    .await;
                if let Ok(o) = output {
                    if o.status.success() && !o.stdout.is_empty() {
                        ready = true;
                        break;
                    }
                }
            }

            if !ready {
                error!("k3s did not become ready within 90s");
                return;
            }

            // Create apps-data subvolume
            let apps_data_path = setup_apps_storage(filesystem.as_deref()).await;

            // Configure local-path-provisioner to use bcachefs subvolume
            if let Some(ref path) = apps_data_path {
                let config_json = format!(
                    r#"{{"nodePathMap":[{{"node":"DEFAULT_PATH_FOR_NON_LISTED_NODES","paths":["{}"]}}]}}"#,
                    path
                );
                let patch = format!(
                    r#"{{"data":{{"config.json":"{}"}}}}"#,
                    config_json.replace('"', r#"\""#)
                );
                let _ = tokio::process::Command::new("k3s")
                    .args(["kubectl", "--kubeconfig", KUBECONFIG, "-n", "kube-system",
                           "patch", "configmap", "local-path-config", "-p", &patch])
                    .output()
                    .await;

                // Restart local-path-provisioner to pick up new config
                let _ = tokio::process::Command::new("k3s")
                    .args(["kubectl", "--kubeconfig", KUBECONFIG, "-n", "kube-system",
                           "rollout", "restart", "deployment/local-path-provisioner"])
                    .output()
                    .await;

                info!("local-path-provisioner configured to use {path}");
            }

            // Add bjw-s Helm repo
            let _ = tokio::process::Command::new("helm")
                .args(["repo", "add", "bjw-s", APP_TEMPLATE_REPO, "--kubeconfig", KUBECONFIG])
                .output()
                .await;

            // Update repos
            let _ = tokio::process::Command::new("helm")
                .args(["repo", "update", "--kubeconfig", KUBECONFIG])
                .output()
                .await;

            // Persist storage path in config
            if let Some(ref path) = apps_data_path {
                let config = AppsConfig {
                    enabled: true,
                    storage_path: Some(path.clone()),
                };
                let _ = AppsService::save_config(&config).await;
            }

            info!("Apps bootstrap complete (repo: bjw-s)");
        });

        Ok(())
    }

    pub async fn disable(&self) -> Result<(), AppsError> {
        if !self.is_enabled() {
            return Err(AppsError::NotEnabled);
        }

        // Stop k3s
        run_cmd("systemctl", &["stop", K3S_SERVICE]).await?;

        // Remove state file
        let _ = tokio::fs::remove_file(STATE_PATH).await;

        info!("Apps runtime disabled — k3s stopped");
        Ok(())
    }

    // ── Status ──────────────────────────────────────────────

    pub async fn status(&self) -> AppsStatus {
        let config = Self::load_config();
        let enabled = self.is_enabled();
        let storage_path = config.storage_path.clone();
        let storage_ok = storage_path.as_ref()
            .map(|p| Path::new(p).is_dir())
            .unwrap_or(false);

        if !enabled {
            return AppsStatus {
                enabled, running: false, app_count: 0, memory_bytes: None,
                storage_path, k3s_version: None, node_status: None, storage_ok,
            };
        }

        let running = self.is_k3s_ready().await;
        if !running {
            return AppsStatus {
                enabled, running: false, app_count: 0, memory_bytes: None,
                storage_path, k3s_version: None, node_status: None, storage_ok,
            };
        }

        // Run checks in parallel
        let (apps_result, memory_bytes, k3s_version, node_status) = tokio::join!(
            self.list_internal(),
            k3s_memory(),
            k3s_version(),
            k3s_node_status(),
        );
        let app_count = apps_result.map(|apps| apps.len()).unwrap_or(0);

        AppsStatus {
            enabled, running, app_count, memory_bytes,
            storage_path, k3s_version, node_status, storage_ok,
        }
    }

    // ── App management (app-template) ───────────────────────

    pub async fn install(&self, mut req: InstallAppRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        // Validate user-specified NodePorts and auto-assign missing ones
        let mut used = self.used_node_ports().await;
        for p in &req.ports {
            if let Some(np) = p.node_port {
                if !(30000..=32767).contains(&np) {
                    return Err(AppsError::HelmFailed(format!(
                        "NodePort {} is out of range (must be 30000-32767)",
                        np
                    )));
                }
                if used.contains(&np) {
                    return Err(AppsError::HelmFailed(format!(
                        "NodePort {} is already in use",
                        np
                    )));
                }
            }
        }
        let mut next_free = 30000u16;
        for p in &mut req.ports {
            if p.node_port.is_none() {
                while used.contains(&next_free) {
                    next_free += 1;
                    if next_free > 32767 {
                        return Err(AppsError::HelmFailed(
                            "no free NodePort available in 30000-32767 range".into(),
                        ));
                    }
                }
                p.node_port = Some(next_free);
                used.insert(next_free);
                next_free += 1;
            }
        }

        // Check if release already exists
        let existing = self.list().await?;
        if existing.iter().any(|a| a.name == req.name) {
            return Err(AppsError::AppAlreadyExists(req.name));
        }

        // Generate values.yaml for app-template
        let values = generate_app_template_values(&req);
        let values_json = serde_json::to_string(&values)
            .map_err(|e| AppsError::HelmFailed(format!("serialize values: {e}")))?;

        // Write temp values file
        let values_path = format!("/tmp/nasty-app-{}.json", req.name);
        tokio::fs::write(&values_path, &values_json).await?;

        // Create per-app namespace
        let ns = app_namespace(&req.name);
        self.ensure_namespace(&ns).await;

        // helm install
        let output = Command::new("helm")
            .args([
                "install", &req.name,
                &format!("bjw-s/{APP_TEMPLATE_CHART}"),
                "--namespace", &ns,
                "--values", &values_path,
                "--kubeconfig", KUBECONFIG,
            ])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        let _ = tokio::fs::remove_file(&values_path).await;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        info!("Installed app '{}' (image: {})", req.name, req.image);

        // Auto-create ingress using the first port's NodePort
        if let Some(first_port) = req.ports.first() {
            if let Some(node_port) = first_port.node_port {
                if let Err(e) = self.ingress_set(SetIngressRequest {
                    name: req.name.clone(),
                    node_port,
                }).await {
                    warn!("Failed to auto-create ingress for '{}': {e}", req.name);
                }
            }
        }

        // Return the installed app
        self.get(&req.name).await
    }

    pub async fn remove(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        let ns = app_namespace(name);
        let output = Command::new("helm")
            .args([
                "uninstall", name,
                "--namespace", &ns,
                "--kubeconfig", KUBECONFIG,
            ])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not found") {
                return Err(AppsError::AppNotFound(name.to_string()));
            }
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        // Clean up ingress rule
        let _ = self.ingress_remove(name).await;

        // Delete the per-app namespace (cleans up all remaining resources)
        let _ = Command::new("k3s")
            .args(["kubectl", "--kubeconfig", KUBECONFIG, "delete", "namespace", &ns])
            .output()
            .await;

        info!("Removed app '{name}'");
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<App>, AppsError> {
        self.require_ready().await?;
        self.list_internal().await
    }

    /// List apps without the require_ready check (used by status() which already checked).
    async fn list_internal(&self) -> Result<Vec<App>, AppsError> {
        let output = Command::new("helm")
            .args([
                "list", "--all-namespaces",
                "--kubeconfig", KUBECONFIG,
                "-o", "json",
            ])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        let releases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .unwrap_or_default();

        let apps = releases.iter().filter_map(|r| {
            let ns = r["namespace"].as_str().unwrap_or("");
            // Only include releases in nasty- namespaces
            if !ns.starts_with(NAMESPACE_PREFIX) {
                return None;
            }
            Some(App {
                name: r["name"].as_str().unwrap_or("").to_string(),
                namespace: ns.to_string(),
                image: "".to_string(), // Helm doesn't expose this directly
                chart: r["chart"].as_str().unwrap_or("").to_string(),
                status: r["status"].as_str().unwrap_or("unknown").to_string(),
                updated: r["updated"].as_str().unwrap_or("").to_string(),
            })
        }).collect();

        Ok(apps)
    }

    pub async fn get(&self, name: &str) -> Result<App, AppsError> {
        let apps = self.list().await?;
        apps.into_iter()
            .find(|a| a.name == name)
            .ok_or_else(|| AppsError::AppNotFound(name.to_string()))
    }

    /// Get the current configuration of an installed app by parsing its Helm values.
    pub async fn get_config(&self, name: &str) -> Result<AppConfig, AppsError> {
        self.require_ready().await?;

        // Verify app exists
        let _ = self.get(name).await?;

        let ns = app_namespace(name);
        let output = Command::new("helm")
            .args([
                "get", "values", name,
                "--namespace", &ns,
                "--kubeconfig", KUBECONFIG,
                "-o", "json",
            ])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        let values: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_default();

        // Parse image
        let container = &values["controllers"]["main"]["containers"]["main"];
        let repo = container["image"]["repository"].as_str().unwrap_or("");
        let tag = container["image"]["tag"].as_str().unwrap_or("latest");
        let image = if repo.is_empty() { String::new() } else { format!("{repo}:{tag}") };

        // Parse ports
        let mut ports = Vec::new();
        if let Some(svc_ports) = values["service"]["main"]["ports"].as_object() {
            for (port_name, port_def) in svc_ports {
                ports.push(AppPort {
                    name: port_name.clone(),
                    container_port: port_def["port"].as_u64().unwrap_or(80) as u16,
                    node_port: port_def["nodePort"].as_u64().map(|v| v as u16),
                    protocol: port_def["protocol"].as_str().unwrap_or("TCP").to_string(),
                });
            }
        }

        // Parse env
        let mut env = Vec::new();
        if let Some(env_map) = container["env"].as_object() {
            for (k, v) in env_map {
                env.push(AppEnv {
                    name: k.clone(),
                    value: v.as_str().unwrap_or("").to_string(),
                });
            }
        }

        // Parse volumes
        let mut volumes = Vec::new();
        if let Some(persistence) = values["persistence"].as_object() {
            for (vol_name, vol_def) in persistence {
                if vol_def["type"].as_str() == Some("persistentVolumeClaim") {
                    let mount_path = vol_def["globalMounts"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|m| m["path"].as_str())
                        .unwrap_or("")
                        .to_string();
                    volumes.push(AppVolume {
                        name: vol_name.clone(),
                        mount_path,
                        size: vol_def["size"].as_str().unwrap_or("1Gi").to_string(),
                        storage_class: vol_def["storageClass"].as_str().unwrap_or("local-path").to_string(),
                    });
                }
            }
        }

        // Parse resource limits
        let limits = &container["resources"]["limits"];
        let cpu_limit = limits["cpu"].as_str().map(String::from);
        let memory_limit = limits["memory"].as_str().map(String::from);

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

    /// Update an existing app with new configuration via helm upgrade.
    pub async fn update(&self, mut req: InstallAppRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        // Verify app exists
        let _ = self.get(&req.name).await?;

        // Validate and assign NodePorts (same logic as install)
        let mut used = self.used_node_ports().await;
        // Exclude ports currently used by this app (they can be reused)
        if let Ok(current) = self.get_config(&req.name).await {
            for p in &current.ports {
                if let Some(np) = p.node_port {
                    used.remove(&np);
                }
            }
        }
        for p in &req.ports {
            if let Some(np) = p.node_port {
                if !(30000..=32767).contains(&np) {
                    return Err(AppsError::HelmFailed(format!(
                        "NodePort {} is out of range (must be 30000-32767)", np
                    )));
                }
                if used.contains(&np) {
                    return Err(AppsError::HelmFailed(format!(
                        "NodePort {} is already in use", np
                    )));
                }
            }
        }
        let mut next_free = 30000u16;
        for p in &mut req.ports {
            if p.node_port.is_none() {
                while used.contains(&next_free) {
                    next_free += 1;
                    if next_free > 32767 {
                        return Err(AppsError::HelmFailed(
                            "no free NodePort available in 30000-32767 range".into(),
                        ));
                    }
                }
                p.node_port = Some(next_free);
                used.insert(next_free);
                next_free += 1;
            }
        }

        let values = generate_app_template_values(&req);
        let values_json = serde_json::to_string(&values)
            .map_err(|e| AppsError::HelmFailed(format!("serialize values: {e}")))?;

        let values_path = format!("/tmp/nasty-app-{}.json", req.name);
        tokio::fs::write(&values_path, &values_json).await?;

        let ns = app_namespace(&req.name);
        let output = Command::new("helm")
            .args([
                "upgrade", &req.name,
                &format!("bjw-s/{APP_TEMPLATE_CHART}"),
                "--namespace", &ns,
                "--values", &values_path,
                "--kubeconfig", KUBECONFIG,
            ])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        let _ = tokio::fs::remove_file(&values_path).await;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        info!("Updated app '{}'", req.name);

        // Update ingress
        if let Some(first_port) = req.ports.first() {
            if let Some(node_port) = first_port.node_port {
                let _ = self.ingress_set(SetIngressRequest {
                    name: req.name.clone(),
                    node_port,
                }).await;
            }
        } else {
            // No ports — remove ingress
            let _ = self.ingress_remove(&req.name).await;
        }

        self.get(&req.name).await
    }

    pub async fn logs(&self, name: &str, tail: Option<u32>) -> Result<String, AppsError> {
        self.require_ready().await?;

        let tail_str = tail.unwrap_or(100).to_string();
        let label = format!("app.kubernetes.io/instance={name}");

        let ns = app_namespace(name);
        let output = Command::new("k3s")
            .args([
                "kubectl",
                "logs",
                "--namespace", &ns,
                "-l", &label,
                "--tail", &tail_str,
                "--kubeconfig", KUBECONFIG,
            ])
            .output()
            .await
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::CommandFailed(stderr.to_string()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ── Helm chart management (BYOH) ────────────────────────

    pub async fn install_chart(&self, req: InstallHelmChartRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

        let ns = app_namespace(&req.name);
        self.ensure_namespace(&ns).await;

        let mut args = vec![
            "install".to_string(),
            req.name.clone(),
            req.chart.clone(),
            "--namespace".to_string(), ns,
            "--kubeconfig".to_string(), KUBECONFIG.to_string(),
        ];

        if let Some(ref version) = req.version {
            args.push("--version".to_string());
            args.push(version.clone());
        }

        // Write values to temp file if provided
        let values_path = if let Some(ref values) = req.values {
            let path = format!("/tmp/nasty-helm-{}.json", req.name);
            let json = serde_json::to_string(values)
                .map_err(|e| AppsError::HelmFailed(format!("serialize values: {e}")))?;
            tokio::fs::write(&path, &json).await?;
            args.push("--values".to_string());
            args.push(path.clone());
            Some(path)
        } else {
            None
        };

        let output = Command::new("helm")
            .args(&args)
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if let Some(path) = values_path {
            let _ = tokio::fs::remove_file(&path).await;
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        info!("Installed Helm chart '{}' as '{}'", req.chart, req.name);
        self.get(&req.name).await
    }

    // ── Helm repo management ───────────────────────────────

    pub async fn repo_list(&self) -> Result<Vec<HelmRepo>, AppsError> {
        let output = Command::new("helm")
            .args(["repo", "list", "--kubeconfig", KUBECONFIG, "-o", "json"])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            // No repos configured yet — return empty
            return Ok(vec![]);
        }

        let repos: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .unwrap_or_default();

        Ok(repos.iter().map(|r| HelmRepo {
            name: r["name"].as_str().unwrap_or("").to_string(),
            url: r["url"].as_str().unwrap_or("").to_string(),
        }).collect())
    }

    pub async fn repo_add(&self, req: AddRepoRequest) -> Result<HelmRepo, AppsError> {
        let output = Command::new("helm")
            .args(["repo", "add", &req.name, &req.url, "--kubeconfig", KUBECONFIG])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        // Update repo index
        let _ = Command::new("helm")
            .args(["repo", "update", "--kubeconfig", KUBECONFIG])
            .output()
            .await;

        info!("Added Helm repo '{}' ({})", req.name, req.url);
        Ok(HelmRepo { name: req.name, url: req.url })
    }

    pub async fn repo_remove(&self, name: &str) -> Result<(), AppsError> {
        let output = Command::new("helm")
            .args(["repo", "remove", name, "--kubeconfig", KUBECONFIG])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        info!("Removed Helm repo '{name}'");
        Ok(())
    }

    pub async fn repo_update(&self) -> Result<(), AppsError> {
        self.require_ready().await?;

        let output = Command::new("helm")
            .args(["repo", "update", "--kubeconfig", KUBECONFIG])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppsError::HelmFailed(stderr.to_string()));
        }

        info!("Helm repos updated");
        Ok(())
    }

    /// Search for charts across all configured repos.
    pub async fn search(&self, query: &str) -> Result<Vec<HelmChart>, AppsError> {
        self.require_ready().await?;

        let output = Command::new("helm")
            .args(["search", "repo", query, "--kubeconfig", KUBECONFIG, "-o", "json"])
            .output()
            .await
            .map_err(|e| AppsError::HelmFailed(e.to_string()))?;

        if !output.status.success() {
            return Ok(vec![]);
        }

        let results: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .unwrap_or_default();

        Ok(results.iter().map(|r| {
            let full_name = r["name"].as_str().unwrap_or("");
            let (repo, chart_name) = full_name.split_once('/').unwrap_or(("", full_name));
            HelmChart {
                name: chart_name.to_string(),
                repo: repo.to_string(),
                version: r["version"].as_str().unwrap_or("").to_string(),
                app_version: r["app_version"].as_str().unwrap_or("").to_string(),
                description: r["description"].as_str().unwrap_or("").to_string(),
            }
        }).collect())
    }

    // ── Ingress management ────────────────────────────────────

    /// List all app ingress rules.
    pub async fn ingress_list(&self) -> Result<Vec<AppIngress>, AppsError> {
        let content = tokio::fs::read_to_string(PROXY_CONF).await.unwrap_or_default();
        let mut rules = Vec::new();
        // Parse our generated format: "# app:<name> port:<port>"
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
                                node_port: port,
                            });
                        }
                    }
                }
            }
        }
        Ok(rules)
    }

    /// Enable ingress for an app — proxy /apps/{name}/ to its NodePort.
    pub async fn ingress_set(&self, req: SetIngressRequest) -> Result<AppIngress, AppsError> {
        let mut rules = self.ingress_list().await?;

        // Remove existing rule for this app
        rules.retain(|r| r.name != req.name);

        // Add new rule
        rules.push(AppIngress {
            name: req.name.clone(),
            node_port: req.node_port,
            path: format!("/apps/{}/", req.name),
        });

        self.write_proxy_conf(&rules).await?;
        reload_nginx().await;

        info!("Ingress set for '{}' → NodePort {}", req.name, req.node_port);
        Ok(rules.into_iter().find(|r| r.name == req.name).unwrap())
    }

    /// Remove ingress for an app.
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

    // ── Image inspection ─────────────────────────────────────

    /// Inspect a container image's EXPOSE directives via the Docker Registry API.
    pub async fn inspect_image(&self, image: &str) -> Result<ImageInspectResult, AppsError> {
        let ports = inspect_image_ports(image).await.map_err(|e| {
            AppsError::CommandFailed(format!("image inspect failed: {e}"))
        })?;
        Ok(ImageInspectResult { ports })
    }

    // ── Port Forwarding ──────────────────────────────────────

    /// Start a port-forward to an app's pod.
    pub async fn port_forward_start(&self, name: &str, local_port: Option<u16>) -> Result<PortForwardInfo, AppsError> {
        self.require_ready().await?;

        // Find the pod for this app
        let ns = app_namespace(name);
        let output = Command::new("k3s")
            .args(["kubectl", "--kubeconfig", KUBECONFIG, "-n", &ns,
                   "get", "pods", "-l", &format!("app.kubernetes.io/name={name}"),
                   "-o", "jsonpath={.items[0].metadata.name}"])
            .output()
            .await
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

        let pod = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if pod.is_empty() {
            return Err(AppsError::AppNotFound(name.to_string()));
        }

        // Find the container port from the pod spec
        let port_output = Command::new("k3s")
            .args(["kubectl", "--kubeconfig", KUBECONFIG, "-n", &ns,
                   "get", "pod", &pod,
                   "-o", "jsonpath={.spec.containers[0].ports[0].containerPort}"])
            .output()
            .await
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

        let container_port: u16 = String::from_utf8_lossy(&port_output.stdout)
            .trim()
            .parse()
            .unwrap_or(8080);

        let local = local_port.unwrap_or(container_port);

        // Kill existing forward for this app
        self.port_forward_stop(name).await.ok();

        // Start port-forward process
        let child = Command::new("k3s")
            .args(["kubectl", "--kubeconfig", KUBECONFIG, "-n", &ns,
                   "port-forward", &format!("pod/{pod}"), &format!("{local}:{container_port}"),
                   "--address", "0.0.0.0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| AppsError::CommandFailed(e.to_string()))?;

        let pid = child.id().unwrap_or(0);

        let info = PortForwardInfo {
            name: name.to_string(),
            local_port: local,
            container_port,
            pod: pod.clone(),
        };

        info!("Port-forward started: {}:{} → pod/{} ({}) [pid={}]", local, container_port, pod, name, pid);

        let mut forwards = self.port_forwards.lock().unwrap();
        forwards.insert(name.to_string(), (PortForwardInfo {
            name: name.to_string(),
            local_port: local,
            container_port,
            pod,
        }, pid));

        Ok(info)
    }

    /// Stop a port-forward for an app.
    pub async fn port_forward_stop(&self, name: &str) -> Result<(), AppsError> {
        let pid = {
            let mut forwards = self.port_forwards.lock().unwrap();
            forwards.remove(name).map(|(_, pid)| pid)
        };
        if let Some(pid) = pid {
            if pid > 0 {
                let _ = Command::new("kill").arg(pid.to_string()).output().await;
            }
            info!("Port-forward stopped for '{name}'");
        }
        Ok(())
    }

    /// List active port-forwards.
    pub fn port_forward_list(&self) -> Vec<PortForwardInfo> {
        let forwards = self.port_forwards.lock().unwrap();
        forwards.values().map(|(info, _)| PortForwardInfo {
            name: info.name.clone(),
            local_port: info.local_port,
            container_port: info.container_port,
            pod: info.pod.clone(),
        }).collect()
    }

    /// Write the nginx proxy config file.
    async fn write_proxy_conf(&self, rules: &[AppIngress]) -> Result<(), AppsError> {
        let mut conf = String::from("# Auto-generated by NASty engine — do not edit\n");
        for rule in rules {
            conf.push_str(&format!(
                "# app:{} port:{}\nlocation /apps/{}/ {{\n    proxy_pass http://127.0.0.1:{}/;\n    proxy_set_header Host $host;\n    proxy_set_header X-Real-IP $remote_addr;\n    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    proxy_set_header X-Forwarded-Proto $scheme;\n    proxy_http_version 1.1;\n    proxy_set_header Upgrade $http_upgrade;\n    proxy_set_header Connection \"upgrade\";\n}}\n\n",
                rule.name, rule.node_port, rule.name, rule.node_port
            ));
        }
        tokio::fs::write(PROXY_CONF, &conf).await?;
        Ok(())
    }

    // ── Restore on boot ─────────────────────────────────────

    pub async fn restore(&self) {
        if !self.is_enabled() {
            return;
        }
        // Ensure k3s data symlink is in place before starting
        let config = Self::load_config();
        if let Some(ref path) = config.storage_path {
            // storage_path is like /fs/first/.nasty/apps-data — derive filesystem name
            if let Some(fs_name) = path.strip_prefix("/fs/").and_then(|s| s.split('/').next()) {
                ensure_k3s_symlink(fs_name).await;
            }
        }
        info!("Apps runtime enabled — ensuring k3s is running");
        if let Err(e) = run_cmd("systemctl", &["start", K3S_SERVICE]).await {
            error!("Failed to start k3s: {e}");
        }
    }

    /// Collect all NodePorts currently in use by our apps.
    async fn used_node_ports(&self) -> HashSet<u16> {
        let mut used = HashSet::new();
        if let Ok(rules) = self.ingress_list().await {
            for rule in rules {
                used.insert(rule.node_port);
            }
        }
        // Also scan k8s services across all nasty- namespaces
        if let Ok(output) = Command::new("k3s")
            .args([
                "kubectl", "--kubeconfig", KUBECONFIG, "--all-namespaces",
                "get", "svc", "-o",
                "jsonpath={range .items[*].spec.ports[*]}{.nodePort}{\"\\n\"}{end}",
            ])
            .output()
            .await
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Ok(port) = line.trim().parse::<u16>() {
                    used.insert(port);
                }
            }
        }
        used
    }

    // ── Internal helpers ────────────────────────────────────

    async fn is_k3s_ready(&self) -> bool {
        let output = Command::new("k3s")
            .args(["kubectl", "--kubeconfig", KUBECONFIG, "get", "nodes", "-o", "name"])
            .output()
            .await;

        match output {
            Ok(o) => o.status.success() && !o.stdout.is_empty(),
            Err(_) => false,
        }
    }

    async fn require_ready(&self) -> Result<(), AppsError> {
        if !self.is_enabled() {
            return Err(AppsError::NotEnabled);
        }
        if !self.is_k3s_ready().await {
            return Err(AppsError::NotReady("k3s not responding".to_string()));
        }
        // Ensure bjw-s repo exists — re-add if bootstrap failed previously
        self.ensure_helm_repo().await;
        Ok(())
    }

    /// Ensure a namespace exists. Idempotent.
    async fn ensure_namespace(&self, ns: &str) {
        let output = Command::new("k3s")
            .args(["kubectl", "--kubeconfig", KUBECONFIG, "get", "namespace", ns])
            .output()
            .await;
        let exists = matches!(output, Ok(ref o) if o.status.success());
        if !exists {
            info!("Namespace {ns} missing, creating");
            let _ = Command::new("k3s")
                .args(["kubectl", "--kubeconfig", KUBECONFIG, "create", "namespace", ns])
                .output()
                .await;
        }
    }

    /// Ensure the bjw-s helm repo is configured. Idempotent.
    async fn ensure_helm_repo(&self) {
        let output = Command::new("helm")
            .args(["repo", "list", "-o", "json"])
            .output()
            .await;

        let has_bjws = match output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).contains("bjw-s")
            }
            _ => false,
        };

        if !has_bjws {
            info!("bjw-s helm repo missing — adding...");
            let _ = Command::new("helm")
                .args(["repo", "add", "bjw-s", APP_TEMPLATE_REPO])
                .output()
                .await;
            let _ = Command::new("helm")
                .args(["repo", "update"])
                .output()
                .await;
        }
    }

}

// ── Helpers ─────────────────────────────────────────────────────

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

/// Get k3s memory usage from systemd cgroup.
async fn k3s_memory() -> Option<u64> {
    let output = Command::new("systemctl")
        .args(["show", K3S_SERVICE, "--property=MemoryCurrent"])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Format: "MemoryCurrent=1234567"
    stdout.trim()
        .strip_prefix("MemoryCurrent=")?
        .parse::<u64>()
        .ok()
}

async fn k3s_version() -> Option<String> {
    let output = Command::new("k3s")
        .args(["--version"])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Format: "k3s version v1.xx.x+k3s1 (hash)"
    stdout.split_whitespace().nth(2).map(|s| s.to_string())
}

async fn k3s_node_status() -> Option<String> {
    let output = Command::new("k3s")
        .args(["kubectl", "--kubeconfig", KUBECONFIG, "get", "nodes",
               "-o", "jsonpath={.items[0].status.conditions[?(@.type==\"Ready\")].status}"])
        .output()
        .await
        .ok()?;
    let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if status == "True" {
        Some("Ready".to_string())
    } else if status == "False" {
        Some("NotReady".to_string())
    } else {
        Some(status)
    }
}

/// Generate values.yaml for the bjw-s app-template chart.
fn generate_app_template_values(req: &InstallAppRequest) -> serde_json::Value {
    let mut env_list = serde_json::Map::new();
    for e in &req.env {
        env_list.insert(e.name.clone(), serde_json::json!(e.value));
    }

    // Ensure port names are valid k8s identifiers (must contain at least one letter)
    let sanitize_port_name = |name: &str, port: u16| -> String {
        if name.chars().any(|c| c.is_ascii_alphabetic()) {
            name.to_string()
        } else {
            format!("port-{port}")
        }
    };

    let ports: Vec<serde_json::Value> = req.ports.iter().map(|p| {
        serde_json::json!({
            "containerPort": p.container_port,
            "name": sanitize_port_name(&p.name, p.container_port),
            "protocol": p.protocol,
        })
    }).collect();

    let mut persistence = serde_json::Map::new();
    for v in &req.volumes {
        persistence.insert(v.name.clone(), serde_json::json!({
            "enabled": true,
            "type": "persistentVolumeClaim",
            "accessMode": "ReadWriteOnce",
            "size": v.size,
            "storageClass": v.storage_class,
            "globalMounts": [{ "path": v.mount_path }],
        }));
    }

    let mut service_ports = serde_json::Map::new();
    for p in &req.ports {
        let svc_name = sanitize_port_name(&p.name, p.container_port);
        let mut port_def = serde_json::json!({
            "port": p.container_port,
            "protocol": p.protocol,
        });
        if let Some(np) = p.node_port {
            port_def["nodePort"] = serde_json::json!(np);
        }
        service_ports.insert(svc_name, port_def);
    }

    let mut container = serde_json::json!({
        "image": {
            "repository": req.image.rsplit_once(':').map(|(r, _)| r).unwrap_or(&req.image),
            "tag": req.image.rsplit_once(':').map(|(_, t)| t).unwrap_or("latest"),
        },
        "env": env_list,
        "resources": {
            "limits": build_resource_limits(&req.cpu_limit, &req.memory_limit),
        },
    });

    if !ports.is_empty() {
        container["ports"] = serde_json::json!(ports);
    }

    let mut values = serde_json::json!({
        "controllers": {
            "main": {
                "containers": {
                    "main": container,
                }
            }
        },
        "persistence": persistence,
    });

    if !service_ports.is_empty() {
        values["service"] = serde_json::json!({
            "main": {
                "type": "NodePort",
                "controller": "main",
                "ports": service_ports,
            }
        });
    }

    values
}

fn build_resource_limits(cpu: &Option<String>, memory: &Option<String>) -> serde_json::Value {
    let mut limits = serde_json::Map::new();
    if let Some(c) = cpu {
        limits.insert("cpu".to_string(), serde_json::json!(c));
    }
    if let Some(m) = memory {
        limits.insert("memory".to_string(), serde_json::json!(m));
    }
    serde_json::Value::Object(limits)
}

async fn reload_nginx() {
    let _ = Command::new("systemctl")
        .args(["reload", "nginx"])
        .output()
        .await;
}

/// Create an "apps-data" subvolume on the specified or first available bcachefs filesystem.
/// Returns the subvolume path if successful.
async fn setup_apps_storage(filesystem: Option<&str>) -> Option<String> {
    let fs_name = if let Some(name) = filesystem {
        // Verify the specified filesystem exists
        let path = format!("/fs/{name}");
        if !std::path::Path::new(&path).is_dir() {
            error!("Specified filesystem '{name}' not found at {path}");
            return None;
        }
        name.to_string()
    } else {
        // Find first mounted bcachefs filesystem
        let fs_base = std::path::Path::new("/fs");
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

    let subvol_path = format!("/fs/{fs_name}/.nasty/apps-data");

    let apps_path = subvol_path;
    if std::path::Path::new(&apps_path).exists() {
        info!("Apps storage already exists at {apps_path}");
        return Some(apps_path);
    }

    // Create as regular directory under .nasty/
    let nasty_dir = format!("/fs/{fs_name}/.nasty");
    match tokio::fs::create_dir_all(&apps_path).await {
        Ok(()) => {
            info!("Created apps storage directory at {apps_path}");
            Some(apps_path)
        }
        Err(e) => {
            error!("Failed to create apps storage at {apps_path}: {e}");
            let _ = tokio::fs::create_dir_all(&nasty_dir).await;
            None
        }
    }
}

/// Ensure /var/lib/rancher/k3s is symlinked to /fs/{fs}/.nasty/k3s.
/// Migrates existing data on first run. No-op if already set up.
async fn ensure_k3s_symlink(fs_name: &str) {
    let k3s_data = format!("/fs/{fs_name}/.nasty/k3s");
    let default_path = "/var/lib/rancher/k3s";

    // Already correct? Still verify the target directory exists.
    if let Ok(target) = tokio::fs::read_link(default_path).await {
        if target.to_string_lossy() == k3s_data {
            if std::path::Path::new(&k3s_data).exists() {
                return;
            }
            info!("k3s symlink target {k3s_data} missing, recreating");
        }
    }

    let nasty_dir = format!("/fs/{fs_name}/.nasty");
    let _ = tokio::fs::create_dir_all(&nasty_dir).await;
    if !std::path::Path::new(&k3s_data).exists() {
        let _ = tokio::fs::create_dir_all(&k3s_data).await;
    }

    // Migrate existing data (first-time only)
    if std::path::Path::new(default_path).is_dir()
        && !std::path::Path::new(default_path).is_symlink()
    {
        info!("Migrating k3s data from {default_path} to {k3s_data}");
        let _ = run_cmd("cp", &["-a", &format!("{default_path}/."), &k3s_data]).await;
        let _ = tokio::fs::remove_dir_all(default_path).await;
    }

    let _ = tokio::fs::remove_dir_all(default_path).await;
    let _ = tokio::fs::remove_file(default_path).await;
    let _ = tokio::fs::create_dir_all("/var/lib/rancher").await;
    match tokio::fs::symlink(&k3s_data, default_path).await {
        Ok(()) => info!("Symlinked {default_path} → {k3s_data}"),
        Err(e) => error!("Failed to symlink k3s data: {e}"),
    }
}

// ── Container image inspection ──────────────────────────────

/// Parse an image reference like "traefik/whoami:latest" or "ghcr.io/org/repo:v1"
/// into (registry, repository, tag).
fn parse_image_ref(image: &str) -> (String, String, String) {
    let (image_no_tag, tag) = if let Some((img, tag)) = image.rsplit_once(':') {
        (img.to_string(), tag.to_string())
    } else {
        (image.to_string(), "latest".to_string())
    };

    // If the first component has a dot or colon, it's a registry
    let parts: Vec<&str> = image_no_tag.splitn(2, '/').collect();
    if parts.len() == 1 {
        // e.g. "nginx" → Docker Hub library image
        ("registry-1.docker.io".to_string(), format!("library/{}", parts[0]), tag)
    } else if parts[0].contains('.') || parts[0].contains(':') {
        // e.g. "ghcr.io/org/repo"
        (parts[0].to_string(), parts[1].to_string(), tag)
    } else {
        // e.g. "traefik/whoami" → Docker Hub user image
        ("registry-1.docker.io".to_string(), image_no_tag, tag)
    }
}

/// Fetch EXPOSE ports from a container image via the Docker Registry HTTP API.
async fn inspect_image_ports(image: &str) -> Result<Vec<AppPort>, String> {
    let (registry, repo, tag) = parse_image_ref(image);
    let client = reqwest::Client::new();

    // Step 1: Get auth token (Docker Hub uses token auth, others may not)
    let token = if registry == "registry-1.docker.io" {
        let token_url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            repo
        );
        let resp: serde_json::Value = client.get(&token_url)
            .send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        resp["token"].as_str().map(String::from)
    } else {
        None
    };

    let registry_url = if registry.starts_with("http") {
        registry.clone()
    } else {
        format!("https://{registry}")
    };

    // Step 2: Fetch manifest to get config digest
    let manifest_url = format!("{registry_url}/v2/{repo}/manifests/{tag}");
    let mut req = client.get(&manifest_url)
        .header("Accept", "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json");
    if let Some(ref t) = token {
        req = req.bearer_auth(t);
    }
    let manifest: serde_json::Value = req
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or("no config digest in manifest")?;

    // Step 3: Fetch config blob
    let config_url = format!("{registry_url}/v2/{repo}/blobs/{config_digest}");
    let mut req = client.get(&config_url);
    if let Some(ref t) = token {
        req = req.bearer_auth(t);
    }
    let config: serde_json::Value = req
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    // Step 4: Parse ExposedPorts from config
    // Format: {"80/tcp": {}, "443/tcp": {}}
    let exposed = config["config"]["ExposedPorts"]
        .as_object()
        .or_else(|| config["container_config"]["ExposedPorts"].as_object());

    let mut ports = Vec::new();
    if let Some(exposed_ports) = exposed {
        for (key, _) in exposed_ports {
            // key is like "80/tcp" or "8080/udp"
            let parts: Vec<&str> = key.split('/').collect();
            if let Some(port_str) = parts.first() {
                if let Ok(port) = port_str.parse::<u16>() {
                    let protocol = parts.get(1)
                        .map(|p| p.to_uppercase())
                        .unwrap_or_else(|| "TCP".to_string());
                    let name = if ports.is_empty() { "http".to_string() } else { format!("port-{}", ports.len()) };
                    ports.push(AppPort {
                        name,
                        container_port: port,
                        node_port: None,
                        protocol,
                    });
                }
            }
        }
    }

    // Sort by port number for consistency
    ports.sort_by_key(|p| p.container_port);

    Ok(ports)
}
