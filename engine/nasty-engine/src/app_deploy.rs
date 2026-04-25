//! WebSocket endpoint for streaming app deployment output.
//!
//! Used by both simple app installs (docker pull + create + start) and
//! compose deploys (docker compose up). Streams stdout/stderr line by line
//! so the WebUI can show real-time progress.

use std::sync::Arc;

use axum::extract::{
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::response::IntoResponse;
use bollard::query_parameters::CreateImageOptions;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

use crate::AppState;

pub async fn deploy_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    ws.on_upgrade(move |socket| handle_deploy(socket, state, client_ip))
}

#[derive(Deserialize)]
struct DeployRequest {
    token: String,
    /// "simple" or "compose"
    kind: String,
    /// App name
    name: String,
    /// For simple: container image to pull and run
    image: Option<String>,
    /// For compose: docker-compose.yml content
    compose_file: Option<String>,
    /// For simple: JSON-encoded InstallAppRequest params (ports, env, volumes, etc.)
    install_params: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct DeployMessage {
    /// "log" for output lines, "error" for errors, "done" for completion
    #[serde(rename = "type")]
    msg_type: String,
    data: String,
}

impl DeployMessage {
    fn log(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "log".into(),
            data: s.to_string(),
        })
        .unwrap()
    }

    fn error(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "error".into(),
            data: s.to_string(),
        })
        .unwrap()
    }

    fn done(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "done".into(),
            data: s.to_string(),
        })
        .unwrap()
    }
}

async fn handle_deploy(mut socket: WebSocket, state: Arc<AppState>, client_ip: String) {

    // Wait for deploy request (first message must contain token + params)
    let req: DeployRequest = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(DeployMessage::error(&format!("invalid request: {e}")).into()))
                    .await;
                return;
            }
        },
        _ => return,
    };

    // Authenticate
    if state.auth.validate(&req.token, &client_ip).await.is_err() {
        let _ = socket
            .send(Message::Text(DeployMessage::error("invalid token").into()))
            .await;
        return;
    }

    info!("Deploy stream started for '{}' (kind: {})", req.name, req.kind);

    match req.kind.as_str() {
        "simple" => deploy_simple(&mut socket, &state, &req).await,
        "compose" => deploy_compose(&mut socket, &state, &req).await,
        "pull" => deploy_pull(&mut socket, &state, &req).await,
        _ => {
            let _ = socket
                .send(Message::Text(DeployMessage::error("unknown deploy kind").into()))
                .await;
        }
    }
}

async fn deploy_simple(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {

    let image = match &req.image {
        Some(img) => img.clone(),
        None => {
            let _ = socket.send(Message::Text(DeployMessage::error("missing image").into())).await;
            return;
        }
    };

    // Step 1: Pull image via bollard with structured progress
    let _ = socket.send(Message::Text(DeployMessage::log(&format!("Pulling image: {image}")).into())).await;

    if let Err(e) = pull_image_with_progress(socket, &state, &image).await {
        let _ = socket.send(Message::Text(DeployMessage::error(&format!("pull failed: {e}")).into())).await;
        return;
    }

    let _ = socket.send(Message::Text(DeployMessage::log("Image pulled successfully").into())).await;

    // Step 2: Install via the engine's install method
    let _ = socket.send(Message::Text(DeployMessage::log("Creating container...").into())).await;

    let install_params = req.install_params.clone().unwrap_or(serde_json::json!({}));
    let mut params: nasty_apps::InstallAppRequest = match serde_json::from_value(install_params) {
        Ok(p) => p,
        Err(e) => {
            let _ = socket.send(Message::Text(DeployMessage::error(&format!("invalid params: {e}")).into())).await;
            return;
        }
    };
    params.name = req.name.clone();
    params.image = image;

    match state.apps.install(params).await {
        Ok(app) => {
            let _ = socket.send(Message::Text(DeployMessage::log(&format!("Container '{}' started", app.name)).into())).await;
            let _ = socket.send(Message::Text(DeployMessage::done("ok").into())).await;
        }
        Err(e) => {
            let _ = socket.send(Message::Text(DeployMessage::error(&e.to_string()).into())).await;
        }
    }
}

async fn deploy_compose(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {

    let compose_content = match &req.compose_file {
        Some(c) => c.clone(),
        None => {
            let _ = socket.send(Message::Text(DeployMessage::error("missing compose_file").into())).await;
            return;
        }
    };

    let compose_dir = format!("/var/lib/nasty/apps/{}", req.name);
    let compose_path = format!("{}/docker-compose.yml", compose_dir);

    // Check if already exists (for new installs)
    let is_update = std::path::Path::new(&compose_path).exists();

    // Write compose file
    if let Err(e) = tokio::fs::create_dir_all(&compose_dir).await {
        let _ = socket.send(Message::Text(DeployMessage::error(&format!("failed to create dir: {e}")).into())).await;
        return;
    }
    if let Err(e) = tokio::fs::write(&compose_path, &compose_content).await {
        let _ = socket.send(Message::Text(DeployMessage::error(&format!("failed to write compose file: {e}")).into())).await;
        return;
    }

    // Write .env
    let env_content = format!("COMPOSE_PROJECT_NAME={}\n", req.name);
    let _ = tokio::fs::write(format!("{}/.env", compose_dir), &env_content).await;

    // Validate
    let _ = socket.send(Message::Text(DeployMessage::log("Validating compose file...").into())).await;
    if let Err(e) = stream_command(
        socket,
        "docker",
        &["compose", "-f", &compose_path, "config", "--quiet"],
    ).await {
        if !is_update {
            let _ = tokio::fs::remove_dir_all(&compose_dir).await;
        }
        let _ = socket.send(Message::Text(DeployMessage::error(&format!("invalid compose file: {e}")).into())).await;
        return;
    }

    // Pull images
    let _ = socket.send(Message::Text(DeployMessage::log("Pulling images...").into())).await;
    if let Err(e) = stream_command(
        socket,
        "docker",
        &["compose", "-f", &compose_path, "--project-name", &req.name, "pull"],
    ).await {
        if !is_update {
            let _ = tokio::fs::remove_dir_all(&compose_dir).await;
        }
        let _ = socket.send(Message::Text(DeployMessage::error(&format!("pull failed: {e}")).into())).await;
        return;
    }

    // Start containers
    let _ = socket.send(Message::Text(DeployMessage::log("Starting containers...").into())).await;
    let mut args = vec![
        "compose", "-f", &compose_path, "--project-name", &req.name,
        "up", "-d", "--no-build",
    ];
    if is_update {
        args.push("--remove-orphans");
    }
    if let Err(e) = stream_command(socket, "docker", &args).await {
        // Clean up partially created containers before removing the compose dir
        let _ = socket.send(Message::Text(DeployMessage::log("Cleaning up failed deployment...").into())).await;
        let _ = Command::new("docker")
            .args(["compose", "-f", &compose_path, "--project-name", &req.name, "down", "-v", "--remove-orphans"])
            .output()
            .await;
        if !is_update {
            let _ = tokio::fs::remove_dir_all(&compose_dir).await;
        }
        let _ = socket.send(Message::Text(DeployMessage::error(&format!("deploy failed: {e}")).into())).await;
        return;
    }

    // Auto-ingress for first exposed port
    if let Ok(app) = state.apps.get(&req.name).await {
        if let Some(first_port) = app.ports.first() {
            let _ = state.apps.ingress_set(nasty_apps::SetIngressRequest {
                name: req.name.clone(),
                host_port: first_port.host_port,
            }).await;
        }
    }

    let action = if is_update { "updated" } else { "deployed" };
    let _ = socket.send(Message::Text(DeployMessage::log(&format!("Compose app '{}' {action} successfully", req.name)).into())).await;
    let _ = socket.send(Message::Text(DeployMessage::done("ok").into())).await;
}

async fn deploy_pull(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {

    let compose_path = format!("/var/lib/nasty/apps/{}/docker-compose.yml", req.name);

    if std::path::Path::new(&compose_path).exists() {
        // Compose app: pull + recreate
        let _ = socket.send(Message::Text(DeployMessage::log("Pulling latest images...").into())).await;
        if let Err(e) = stream_command(
            socket, "docker",
            &["compose", "-f", &compose_path, "--project-name", &req.name, "pull"],
        ).await {
            let _ = socket.send(Message::Text(DeployMessage::error(&format!("pull failed: {e}")).into())).await;
            return;
        }

        let _ = socket.send(Message::Text(DeployMessage::log("Recreating containers...").into())).await;
        if let Err(e) = stream_command(
            socket, "docker",
            &["compose", "-f", &compose_path, "--project-name", &req.name,
              "up", "-d", "--no-build", "--remove-orphans"],
        ).await {
            let _ = socket.send(Message::Text(DeployMessage::error(&format!("recreate failed: {e}")).into())).await;
            return;
        }
    } else {
        // Simple app: pull image
        let image = match &req.image {
            Some(img) => img.clone(),
            None => {
                // Look up current image from container
                match state.apps.get_config(&req.name).await {
                    Ok(config) => config.image,
                    Err(e) => {
                        let _ = socket.send(Message::Text(DeployMessage::error(&e.to_string()).into())).await;
                        return;
                    }
                }
            }
        };

        let _ = socket.send(Message::Text(DeployMessage::log(&format!("Pulling image: {image}")).into())).await;
        if let Err(e) = pull_image_with_progress(socket, &state, &image).await {
            let _ = socket.send(Message::Text(DeployMessage::error(&format!("pull failed: {e}")).into())).await;
            return;
        }

        // Recreate container
        let _ = socket.send(Message::Text(DeployMessage::log("Recreating container...").into())).await;
        match state.apps.pull(&req.name).await {
            Ok(_) => {}
            Err(e) => {
                let _ = socket.send(Message::Text(DeployMessage::error(&e.to_string()).into())).await;
                return;
            }
        }
    }

    let _ = socket.send(Message::Text(DeployMessage::log(&format!("Image update complete for '{}'", req.name)).into())).await;
    let _ = socket.send(Message::Text(DeployMessage::done("ok").into())).await;
}

/// Run a command and stream its combined stdout+stderr line by line over the WebSocket.
/// Returns Ok(()) if the command exits successfully, Err(message) otherwise.
async fn stream_command(
    socket: &mut WebSocket,
    cmd: &str,
    args: &[&str],
) -> Result<(), String> {

    let mut child = Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start {cmd}: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Use a channel to merge stdout and stderr into a single stream.
    // Docker compose writes progress to stderr, so we must read both concurrently.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);

    let tx_out = tx.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx_out.send(line).await;
            }
        }
    });

    let tx_err = tx.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx_err.send(line).await;
            }
        }
    });

    // Drop our copy so rx closes when both tasks finish
    drop(tx);

    // Stream lines to WebSocket as they arrive
    let mut all_lines = Vec::new();
    while let Some(line) = rx.recv().await {
        let _ = socket
            .send(Message::Text(DeployMessage::log(&line).into()))
            .await;
        all_lines.push(line);
    }

    // Wait for reader tasks to finish
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let status = child.wait().await.map_err(|e| e.to_string())?;

    if !status.success() {
        let err_lines: Vec<_> = all_lines.iter()
            .filter(|l| l.contains("Error") || l.contains("error") || l.contains("failed"))
            .cloned()
            .collect();
        return Err(if err_lines.is_empty() {
            all_lines.last().cloned().unwrap_or_else(|| "command failed".to_string())
        } else {
            err_lines.join("\n")
        });
    }

    Ok(())
}

/// Pull a Docker image using bollard's API with structured per-layer progress.
async fn pull_image_with_progress(
    socket: &mut WebSocket,
    state: &AppState,
    image: &str,
) -> Result<(), String> {
    let docker = state.apps.docker_client()
        .map_err(|e| format!("Docker not ready: {e}"))?;

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

    let mut stream = docker.create_image(Some(options), None, None);
    let mut layers: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                let id = info.id.as_deref().unwrap_or("");
                let status = info.status.as_deref().unwrap_or("");

                let line = if let Some(ref detail) = info.progress_detail {
                    let current = detail.current.unwrap_or(0);
                    let total = detail.total.unwrap_or(0);
                    if total > 0 {
                        let pct = (current as f64 / total as f64 * 100.0) as u32;
                        let mb_current = current as f64 / 1_048_576.0;
                        let mb_total = total as f64 / 1_048_576.0;
                        format!("{id}: {status} {mb_current:.1}/{mb_total:.1} MB ({pct}%)")
                    } else {
                        format!("{id}: {status}")
                    }
                } else if !id.is_empty() {
                    format!("{id}: {status}")
                } else {
                    status.to_string()
                };

                // Only send if the line changed for this layer (avoid flooding)
                if id.is_empty() || layers.get(id) != Some(&line) {
                    if !id.is_empty() {
                        layers.insert(id.to_string(), line.clone());
                    }
                    let _ = socket
                        .send(Message::Text(DeployMessage::log(&line).into()))
                        .await;
                }
            }
            Err(e) => {
                return Err(format!("{e}"));
            }
        }
    }

    Ok(())
}
