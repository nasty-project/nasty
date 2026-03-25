//! App runtime management — optional k3s + Helm integration.
//!
//! Disabled by default. When enabled, starts a single-node k3s cluster
//! and deploys nasty-csi for storage. Apps are deployed as Helm releases
//! using the bjw-s app-template chart for simple containers, or raw
//! Helm charts for advanced use cases.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{info, error};

const STATE_PATH: &str = "/var/lib/nasty/apps-enabled";
const KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
const K3S_SERVICE: &str = "k3s.service";
const APP_TEMPLATE_REPO: &str = "https://bjw-s-labs.github.io/helm-charts";
const APP_TEMPLATE_CHART: &str = "app-template";
const NAMESPACE: &str = "nasty-apps";

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
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct App {
    /// Helm release name (also used as the app identifier).
    pub name: String,
    /// Namespace (always "nasty-apps").
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

fn default_storage_class() -> String { "nasty-nfs".to_string() }

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

// ── Service ─────────────────────────────────────────────────────

pub struct AppsService;

impl AppsService {
    pub fn new() -> Self {
        Self
    }

    // ── Enable/Disable ──────────────────────────────────────

    pub fn is_enabled(&self) -> bool {
        Path::new(STATE_PATH).exists()
    }

    pub async fn enable(&self) -> Result<(), AppsError> {
        if self.is_enabled() {
            return Err(AppsError::AlreadyEnabled);
        }

        // Write state file
        tokio::fs::write(STATE_PATH, "1").await?;

        // Start k3s via systemd (non-blocking — k3s takes 30-60s to initialize)
        run_cmd("systemctl", &["start", K3S_SERVICE]).await?;

        info!("Apps runtime enabled — k3s starting (bootstrap will run in background)");

        // Bootstrap in background — don't block the RPC response
        tokio::spawn(async {
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

            // Create namespace
            let _ = tokio::process::Command::new("k3s")
                .args(["kubectl", "--kubeconfig", KUBECONFIG, "create", "namespace", NAMESPACE])
                .output()
                .await;

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

            info!("Apps bootstrap complete (namespace: {NAMESPACE}, repo: bjw-s)");
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
        let enabled = self.is_enabled();
        if !enabled {
            return AppsStatus { enabled, running: false, app_count: 0, memory_bytes: None };
        }

        let running = self.is_k3s_ready().await;
        if !running {
            return AppsStatus { enabled, running: false, app_count: 0, memory_bytes: None };
        }

        // Run helm list and memory check in parallel (skip require_ready — already checked)
        let (apps_result, memory_bytes) = tokio::join!(
            self.list_internal(),
            k3s_memory()
        );
        let app_count = apps_result.map(|apps| apps.len()).unwrap_or(0);

        AppsStatus { enabled, running, app_count, memory_bytes }
    }

    // ── App management (app-template) ───────────────────────

    pub async fn install(&self, req: InstallAppRequest) -> Result<App, AppsError> {
        self.require_ready().await?;

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

        // helm install
        let output = Command::new("helm")
            .args([
                "install", &req.name,
                &format!("bjw-s/{APP_TEMPLATE_CHART}"),
                "--namespace", NAMESPACE,
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

        // Return the installed app
        self.get(&req.name).await
    }

    pub async fn remove(&self, name: &str) -> Result<(), AppsError> {
        self.require_ready().await?;

        let output = Command::new("helm")
            .args([
                "uninstall", name,
                "--namespace", NAMESPACE,
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
                "list", "--namespace", NAMESPACE,
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

        let apps = releases.iter().map(|r| App {
            name: r["name"].as_str().unwrap_or("").to_string(),
            namespace: r["namespace"].as_str().unwrap_or(NAMESPACE).to_string(),
            image: "".to_string(), // Helm doesn't expose this directly
            chart: r["chart"].as_str().unwrap_or("").to_string(),
            status: r["status"].as_str().unwrap_or("unknown").to_string(),
            updated: r["updated"].as_str().unwrap_or("").to_string(),
        }).collect();

        Ok(apps)
    }

    pub async fn get(&self, name: &str) -> Result<App, AppsError> {
        let apps = self.list().await?;
        apps.into_iter()
            .find(|a| a.name == name)
            .ok_or_else(|| AppsError::AppNotFound(name.to_string()))
    }

    pub async fn logs(&self, name: &str, tail: Option<u32>) -> Result<String, AppsError> {
        self.require_ready().await?;

        let tail_str = tail.unwrap_or(100).to_string();
        let label = format!("app.kubernetes.io/instance={name}");

        let output = Command::new("k3s")
            .args([
                "kubectl",
                "logs",
                "--namespace", NAMESPACE,
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

        let mut args = vec![
            "install".to_string(),
            req.name.clone(),
            req.chart.clone(),
            "--namespace".to_string(), NAMESPACE.to_string(),
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
        info!("Apps runtime enabled — ensuring k3s is running");
        if let Err(e) = run_cmd("systemctl", &["start", K3S_SERVICE]).await {
            error!("Failed to start k3s: {e}");
        }
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

/// Generate values.yaml for the bjw-s app-template chart.
fn generate_app_template_values(req: &InstallAppRequest) -> serde_json::Value {
    let mut env_list = serde_json::Map::new();
    for e in &req.env {
        env_list.insert(e.name.clone(), serde_json::json!(e.value));
    }

    let mut ports = serde_json::Map::new();
    for p in &req.ports {
        ports.insert(p.name.clone(), serde_json::json!({
            "port": p.container_port,
            "protocol": p.protocol,
        }));
    }

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
        let mut port_def = serde_json::json!({
            "port": p.container_port,
            "protocol": p.protocol,
        });
        if let Some(np) = p.node_port {
            port_def["nodePort"] = serde_json::json!(np);
        }
        service_ports.insert(p.name.clone(), port_def);
    }

    serde_json::json!({
        "controllers": {
            "main": {
                "containers": {
                    "main": {
                        "image": {
                            "repository": req.image.rsplit_once(':').map(|(r, _)| r).unwrap_or(&req.image),
                            "tag": req.image.rsplit_once(':').map(|(_, t)| t).unwrap_or("latest"),
                        },
                        "env": env_list,
                        "ports": ports,
                        "resources": {
                            "limits": build_resource_limits(&req.cpu_limit, &req.memory_limit),
                        },
                    }
                }
            }
        },
        "service": {
            "main": {
                "type": if req.ports.iter().any(|p| p.node_port.is_some()) { "NodePort" } else { "ClusterIP" },
                "controller": "main",
                "ports": service_ports,
            }
        },
        "persistence": persistence,
    })
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
