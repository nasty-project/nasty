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
use tracing::{info, warn};

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
    let pre_auth_token = crate::token_from_headers(&headers);
    ws.on_upgrade(move |socket| handle_deploy(socket, state, client_ip, pre_auth_token))
}

#[derive(Deserialize)]
struct DeployRequest {
    /// Optional when the WS upgrade carried a session cookie or Bearer token.
    #[serde(default)]
    token: Option<String>,
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
    /// Compose only: opt out of the strict sandbox so containers can request
    /// caps like NET_ADMIN, host devices like /dev/dri, or host namespaces.
    /// Admin-only, audited, and surfaced as a badge in the UI. Engine-state
    /// bind mounts and the host root are still rejected even with this set.
    #[serde(default)]
    allow_unsafe: bool,
    /// Compose only: pin the reverse-proxy ingress to this host port
    /// instead of the engine's auto-pick. The WebUI surfaces this when
    /// the compose exposes more than one TCP port (e.g. a web UI and a
    /// metrics endpoint) so the user picks which one `/apps/<name>/`
    /// reaches. Ignored for simple apps (their ingress flows through
    /// `apps.install`). Falls back to the first TCP port when None or
    /// when the requested port isn't actually a TCP port on the
    /// running app.
    #[serde(default)]
    ingress_host_port: Option<u16>,
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

/// Send an error message to the deploy websocket AND log it on the engine.
///
/// The websocket-send is best-effort (`let _`) — by the time we report an
/// error the client may have already navigated away — but the journal
/// always gets the line so an "it failed and I don't know why" report can
/// always be diagnosed without re-running. `app` is the deploy name (or
/// "<unknown>" for the pre-request errors) and `stage` is a short tag
/// like "pull" or "install" that helps grep across deploy attempts.
///
/// This exists because the pre-rule version of this file silently
/// swallowed install errors when the WS happened to close first — exactly
/// the failure mode CONTRIBUTING.md's logging rule is meant to prevent.
async fn report_error(socket: &mut WebSocket, app: &str, stage: &str, msg: &str) {
    warn!("deploy '{app}' {stage} failed: {msg}");
    let _ = socket
        .send(Message::Text(DeployMessage::error(msg).into()))
        .await;
}

async fn handle_deploy(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    // Wait for deploy request (first message must contain params; token is
    // optional now that the upgrade may have carried a session cookie).
    let req: DeployRequest = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                report_error(
                    &mut socket,
                    "<unknown>",
                    "parse-request",
                    &format!("invalid request: {e}"),
                )
                .await;
                return;
            }
        },
        _ => return,
    };

    let token = match pre_auth_token.or_else(|| req.token.clone()) {
        Some(t) => t,
        None => {
            report_error(&mut socket, &req.name, "auth", "missing session").await;
            return;
        }
    };

    // Authenticate — admin only. Compose deploys can mount host paths and run
    // privileged containers, so this is effectively root-equivalent.
    match state.auth.validate(&token, &client_ip).await {
        Ok(s) if s.role == crate::auth::Role::Admin => {}
        Ok(s) => {
            crate::auth::audit(
                "app_deploy_denied",
                &s.username,
                &client_ip,
                &format!("role={:?}", s.role),
            );
            report_error(
                &mut socket,
                &req.name,
                "auth",
                "forbidden: admin role required",
            )
            .await;
            return;
        }
        Err(_) => {
            report_error(&mut socket, &req.name, "auth", "invalid token").await;
            return;
        }
    }

    info!(
        "Deploy stream started for '{}' (kind: {})",
        req.name, req.kind
    );

    match req.kind.as_str() {
        "simple" => deploy_simple(&mut socket, &state, &req).await,
        "compose" => deploy_compose(&mut socket, &state, &req).await,
        "pull" => deploy_pull(&mut socket, &state, &req).await,
        _ => {
            report_error(&mut socket, &req.name, "dispatch", "unknown deploy kind").await;
        }
    }
}

async fn deploy_simple(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {
    let image = match &req.image {
        Some(img) => img.clone(),
        None => {
            report_error(socket, &req.name, "validate", "missing image").await;
            return;
        }
    };

    // Step 1: Pull image via bollard with structured progress
    let _ = socket
        .send(Message::Text(
            DeployMessage::log(&format!("Pulling image: {image}")).into(),
        ))
        .await;

    if let Err(e) = pull_image_with_progress(socket, state, &image).await {
        report_error(socket, &req.name, "pull", &format!("pull failed: {e}")).await;
        return;
    }

    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Image pulled successfully").into(),
        ))
        .await;

    // Step 2: Install via the engine's install method
    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Creating container...").into(),
        ))
        .await;

    let install_params = req.install_params.clone().unwrap_or(serde_json::json!({}));
    let mut params: nasty_apps::InstallAppRequest = match serde_json::from_value(install_params) {
        Ok(p) => p,
        Err(e) => {
            report_error(
                socket,
                &req.name,
                "parse-params",
                &format!("invalid params: {e}"),
            )
            .await;
            return;
        }
    };
    params.name = req.name.clone();
    params.image = image;
    // Top-level flag wins over anything embedded in install_params, so the
    // privileged-deploy decision is plainly visible in the deploy request.
    params.allow_unsafe = req.allow_unsafe;

    if req.allow_unsafe {
        crate::auth::audit(
            "simple_unsafe_deploy",
            "<websocket>",
            "<websocket>",
            &format!("app={}", req.name),
        );
        let _ = socket
            .send(Message::Text(DeployMessage::log(
                "WARNING: deploying with allow_unsafe — bind mounts outside the sandbox are permitted",
            ).into()))
            .await;
    }

    match state.apps.install(params).await {
        Ok(app) => {
            let _ = socket
                .send(Message::Text(
                    DeployMessage::log(&format!("Container '{}' started", app.name)).into(),
                ))
                .await;
            let _ = socket
                .send(Message::Text(DeployMessage::done("ok").into()))
                .await;
        }
        Err(e) => {
            report_error(socket, &req.name, "install", &e.to_string()).await;
        }
    }
}

async fn deploy_compose(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {
    let compose_content = match &req.compose_file {
        Some(c) => c.clone(),
        None => {
            report_error(socket, &req.name, "validate", "missing compose_file").await;
            return;
        }
    };

    // Reject dangerous compose directives before anything touches disk or
    // docker. Without this, an authenticated admin (or anyone who steals an
    // admin token) can mount '/' into a container and walk out with every
    // secret on the host. allow_unsafe relaxes most checks but never permits
    // bind-mounting the engine's state dir or the host root.
    if let Err(e) = validate_compose(&compose_content, &req.name, req.allow_unsafe) {
        report_error(
            socket,
            &req.name,
            "validate",
            &format!("compose rejected: {e}"),
        )
        .await;
        return;
    }

    if req.allow_unsafe {
        crate::auth::audit(
            "compose_unsafe_deploy",
            "<websocket>",
            "<websocket>",
            &format!("app={}", req.name),
        );
        let _ = socket
            .send(Message::Text(
                DeployMessage::log(
                    "WARNING: deploying with allow_unsafe — container has elevated privileges",
                )
                .into(),
            ))
            .await;
    }

    let compose_dir = format!("/var/lib/nasty/apps/{}", req.name);
    let compose_path = format!("{}/docker-compose.yml", compose_dir);

    // Check if already exists (for new installs)
    let is_update = std::path::Path::new(&compose_path).exists();

    // Write compose file
    if let Err(e) = tokio::fs::create_dir_all(&compose_dir).await {
        report_error(
            socket,
            &req.name,
            "write-compose",
            &format!("failed to create dir: {e}"),
        )
        .await;
        return;
    }
    if let Err(e) = tokio::fs::write(&compose_path, &compose_content).await {
        report_error(
            socket,
            &req.name,
            "write-compose",
            &format!("failed to write compose file: {e}"),
        )
        .await;
        return;
    }
    // Persist the unsafe flag next to the compose file so list/get can surface
    // it. Marker is the presence of `allow_unsafe: true` in the JSON file.
    if let Err(e) = write_app_meta(&compose_dir, req.allow_unsafe).await {
        report_error(
            socket,
            &req.name,
            "write-meta",
            &format!("failed to write app meta: {e}"),
        )
        .await;
        return;
    }

    // Write .env. Failure here is non-fatal for `docker compose` (it
    // falls back to the project name from --project-name) but it can
    // mask why a `${COMPOSE_PROJECT_NAME}` interpolation came back
    // empty in user-supplied YAML — log so it's debuggable.
    let env_content = format!("COMPOSE_PROJECT_NAME={}\n", req.name);
    let env_path = format!("{}/.env", compose_dir);
    if let Err(e) = tokio::fs::write(&env_path, &env_content).await {
        tracing::warn!("compose .env write to {env_path} failed: {e}");
    }

    // Validate
    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Validating compose file...").into(),
        ))
        .await;
    if let Err(e) = stream_command(
        socket,
        "docker",
        &["compose", "-f", &compose_path, "config", "--quiet"],
    )
    .await
    {
        if !is_update {
            let _ = tokio::fs::remove_dir_all(&compose_dir).await;
        }
        report_error(
            socket,
            &req.name,
            "validate-compose",
            &format!("invalid compose file: {e}"),
        )
        .await;
        return;
    }

    // Pull images via bollard (parse compose YAML for image refs)
    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Pulling images...").into(),
        ))
        .await;
    let images = extract_compose_images(&compose_content);
    if images.is_empty() {
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("No images to pull (all built locally?)").into(),
            ))
            .await;
    } else {
        for image in &images {
            let _ = socket
                .send(Message::Text(
                    DeployMessage::log(&format!("Pulling: {image}")).into(),
                ))
                .await;
            if let Err(e) = pull_image_with_progress(socket, state, image).await {
                if !is_update {
                    let _ = tokio::fs::remove_dir_all(&compose_dir).await;
                }
                report_error(
                    socket,
                    &req.name,
                    "pull",
                    &format!("pull failed for {image}: {e}"),
                )
                .await;
                return;
            }
        }
    }

    // Start containers
    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Starting containers...").into(),
        ))
        .await;
    let mut args = vec![
        "compose",
        "-f",
        &compose_path,
        "--project-name",
        &req.name,
        "up",
        "-d",
        "--no-build",
    ];
    if is_update {
        args.push("--remove-orphans");
    }
    if let Err(e) = stream_command(socket, "docker", &args).await {
        // Clean up partially created containers before removing the compose dir
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Cleaning up failed deployment...").into(),
            ))
            .await;
        // Best-effort cleanup of partially-created containers. `try_run`
        // logs failures so a leak (containers/volumes that didn't get
        // removed) is debuggable from the journal rather than mysterious
        // disk-space loss.
        nasty_common::cmd::try_run(
            "docker",
            &[
                "compose",
                "-f",
                &compose_path,
                "--project-name",
                &req.name,
                "down",
                "-v",
                "--remove-orphans",
            ],
        )
        .await;
        if !is_update {
            let _ = tokio::fs::remove_dir_all(&compose_dir).await;
        }
        report_error(
            socket,
            &req.name,
            "compose-up",
            &format!("deploy failed: {e}"),
        )
        .await;
        return;
    }

    // Pick the ingress host port: caller's choice if it's actually a
    // published TCP port on the resulting app, else the first TCP port.
    // UDP can't serve HTTP — Caddy's reverse_proxy (like every HTTP
    // proxy) is TCP-only — so we never auto-assign a UDP port even
    // if the compose only publishes UDP. The user can still reach the
    // container directly on the LAN in that edge case.
    if let Ok(app) = state.apps.get(&req.name).await {
        let tcp = |p: &nasty_apps::MappedPort| p.protocol.eq_ignore_ascii_case("tcp");
        let chosen = req
            .ingress_host_port
            .and_then(|hp| app.ports.iter().find(|p| p.host_port == hp && tcp(p)))
            .or_else(|| app.ports.iter().find(|p| tcp(p)));
        if let Some(p) = chosen {
            let _ = state
                .apps
                .ingress_set(nasty_apps::SetIngressRequest {
                    name: req.name.clone(),
                    host_port: p.host_port,
                })
                .await;
        }
    }

    let action = if is_update { "updated" } else { "deployed" };
    let _ = socket
        .send(Message::Text(
            DeployMessage::log(&format!("Compose app '{}' {action} successfully", req.name)).into(),
        ))
        .await;
    let _ = socket
        .send(Message::Text(DeployMessage::done("ok").into()))
        .await;
}

async fn deploy_pull(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {
    let compose_path = format!("/var/lib/nasty/apps/{}/docker-compose.yml", req.name);

    if std::path::Path::new(&compose_path).exists() {
        // Compose app: pull via bollard + recreate
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Pulling latest images...").into(),
            ))
            .await;
        let compose_content = match tokio::fs::read_to_string(&compose_path).await {
            Ok(c) => c,
            Err(e) => {
                report_error(
                    socket,
                    &req.name,
                    "read-compose",
                    &format!("read compose file: {e}"),
                )
                .await;
                return;
            }
        };
        for image in extract_compose_images(&compose_content) {
            let _ = socket
                .send(Message::Text(
                    DeployMessage::log(&format!("Pulling: {image}")).into(),
                ))
                .await;
            if let Err(e) = pull_image_with_progress(socket, state, &image).await {
                report_error(
                    socket,
                    &req.name,
                    "pull",
                    &format!("pull failed for {image}: {e}"),
                )
                .await;
                return;
            }
        }

        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Recreating containers...").into(),
            ))
            .await;
        if let Err(e) = stream_command(
            socket,
            "docker",
            &[
                "compose",
                "-f",
                &compose_path,
                "--project-name",
                &req.name,
                "up",
                "-d",
                "--no-build",
                "--remove-orphans",
            ],
        )
        .await
        {
            report_error(
                socket,
                &req.name,
                "compose-recreate",
                &format!("recreate failed: {e}"),
            )
            .await;
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
                        report_error(socket, &req.name, "get-config", &e.to_string()).await;
                        return;
                    }
                }
            }
        };

        let _ = socket
            .send(Message::Text(
                DeployMessage::log(&format!("Pulling image: {image}")).into(),
            ))
            .await;
        if let Err(e) = pull_image_with_progress(socket, state, &image).await {
            report_error(socket, &req.name, "pull", &format!("pull failed: {e}")).await;
            return;
        }

        // Recreate container
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Recreating container...").into(),
            ))
            .await;
        match state.apps.pull(&req.name).await {
            Ok(_) => {}
            Err(e) => {
                report_error(socket, &req.name, "recreate", &e.to_string()).await;
                return;
            }
        }
    }

    let _ = socket
        .send(Message::Text(
            DeployMessage::log(&format!("Image update complete for '{}'", req.name)).into(),
        ))
        .await;
    let _ = socket
        .send(Message::Text(DeployMessage::done("ok").into()))
        .await;
}

/// Run a command and stream its combined stdout+stderr line by line over the WebSocket.
/// Returns Ok(()) if the command exits successfully, Err(message) otherwise.
async fn stream_command(socket: &mut WebSocket, cmd: &str, args: &[&str]) -> Result<(), String> {
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

    // Wait for reader tasks to finish. If either panics (BufReader,
    // channel send, line decode), the deployment status the user sees
    // is missing the tail of docker output — log so it's debuggable.
    if let Err(e) = stdout_task.await {
        tracing::warn!("compose stdout reader task panicked / cancelled: {e}");
    }
    if let Err(e) = stderr_task.await {
        tracing::warn!("compose stderr reader task panicked / cancelled: {e}");
    }

    let status = child.wait().await.map_err(|e| e.to_string())?;

    if !status.success() {
        let err_lines: Vec<_> = all_lines
            .iter()
            .filter(|l| l.contains("Error") || l.contains("error") || l.contains("failed"))
            .cloned()
            .collect();
        return Err(if err_lines.is_empty() {
            all_lines
                .last()
                .cloned()
                .unwrap_or_else(|| "command failed".to_string())
        } else {
            err_lines.join("\n")
        });
    }

    Ok(())
}

/// Sibling file alongside docker-compose.yml (or simple-app json manifest)
/// recording per-app deploy flags that aren't part of the user-supplied
/// content itself. Today there's only one flag — allow_unsafe.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AppMeta {
    #[serde(default)]
    allow_unsafe: bool,
}

async fn write_app_meta(app_dir: &str, allow_unsafe: bool) -> Result<(), String> {
    let path = format!("{app_dir}/.nasty-meta.json");
    if !allow_unsafe {
        // Don't keep stale "true" markers around when an admin redeploys
        // safely — the file's absence is the safe default.
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(());
    }
    let meta = AppMeta { allow_unsafe };
    let json = serde_json::to_string(&meta).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Capabilities that effectively grant host root if added to a container.
/// CAP_SYS_ADMIN is "the new root" — kernel module loading, mount, etc.
/// CAP_SYS_PTRACE lets you peek into other processes (including the engine).
/// CAP_SYS_MODULE/_RAWIO/_BOOT/_TIME are direct kernel takeover.
/// CAP_NET_ADMIN reconfigures the host network from inside a container.
/// CAP_DAC_READ_SEARCH/CAP_DAC_OVERRIDE bypass host filesystem permissions.
/// CAP_MAC_ADMIN/_OVERRIDE bypass MAC (SELinux/AppArmor).
const FORBIDDEN_CAPS: &[&str] = &[
    "ALL",
    "SYS_ADMIN",
    "SYS_PTRACE",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_BOOT",
    "SYS_TIME",
    "NET_ADMIN",
    "DAC_READ_SEARCH",
    "DAC_OVERRIDE",
    "MAC_ADMIN",
    "MAC_OVERRIDE",
    "AUDIT_CONTROL",
    "AUDIT_WRITE",
];

/// security_opt values that disable the default container sandbox layers.
const FORBIDDEN_SECURITY_OPTS: &[&str] = &[
    "seccomp=unconfined",
    "seccomp:unconfined",
    "apparmor=unconfined",
    "apparmor:unconfined",
    "label=disable",
    "label:disable",
    "label=type:spc_t",
    "label:type:spc_t",
    "no-new-privileges=false",
    "no-new-privileges:false",
    "systempaths=unconfined",
    "systempaths:unconfined",
];

/// Reasons a bind-mount source must be rejected outright, regardless of
/// allow_unsafe. Returns `Some(reason)` when one applies.
///
/// The order matters: `..` and the host root are checked first because they
/// shouldn't be reachable even via the app-dir carve-out. The engine-state
/// prefix is reported separately so the caller can decide whether the app's
/// own subtree under `/var/lib/nasty/apps/<name>/` is fine.
enum BindReject {
    /// Always rejected — caller cannot override.
    Hard(&'static str),
    /// Inside engine state — caller may carve out the app's own subtree.
    EngineState,
}

fn always_forbidden_bind(source: &str) -> Option<BindReject> {
    if source.contains("..") {
        return Some(BindReject::Hard(
            "'..' traversal is never allowed in bind sources",
        ));
    }
    if source == "/" {
        return Some(BindReject::Hard(
            "'/' (host root) is never allowed as a bind source",
        ));
    }
    if source == "/var/lib/nasty" || source.starts_with("/var/lib/nasty/") {
        return Some(BindReject::EngineState);
    }
    None
}

/// Validate a docker-compose YAML before it's written to disk and handed to
/// `docker compose up`. Returns Err with a human-readable message on the
/// first dangerous directive found.
///
/// `allow_unsafe = false` (the default) is the strict mode: rejects any
/// directive that grants host-equivalent privilege.
///
/// `allow_unsafe = true` is opt-in (admin-only, audited, surfaced in the UI):
/// the strict checks are relaxed so workloads that legitimately need elevated
/// access work — Tailscale (NET_ADMIN + /dev/net/tun), Plex/Jellyfin GPU
/// transcoding (/dev/dri), Frigate (USB cameras), etc. A small core of checks
/// stays on either way: nothing may bind-mount the engine's state dir, the
/// root filesystem, or use `..` to escape.
fn validate_compose(yaml: &str, app_name: &str, allow_unsafe: bool) -> Result<(), String> {
    let parsed: serde_json::Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("compose YAML failed to parse: {e}"))?;

    let services = parsed
        .get("services")
        .and_then(|s| s.as_object())
        .ok_or_else(|| "compose file has no `services:` map".to_string())?;

    let allowed_app_dir = format!("/var/lib/nasty/apps/{app_name}");

    for (svc_name, svc) in services {
        let scope = |field: &str| format!("services.{svc_name}.{field}");

        if !allow_unsafe {
            if svc.get("privileged").and_then(|v| v.as_bool()) == Some(true) {
                return Err(format!(
                    "{} sets privileged: true (host-root equivalent). Set allow_unsafe to override.",
                    scope("privileged")
                ));
            }

            for field in ["pid", "ipc", "uts", "userns_mode", "cgroup", "network_mode"] {
                if let Some(v) = svc.get(field).and_then(|v| v.as_str()) {
                    let v_lower = v.to_ascii_lowercase();
                    if v_lower == "host" || v_lower.starts_with("host:") {
                        return Err(format!(
                            "{} = '{}' shares the host namespace. Set allow_unsafe to override.",
                            scope(field),
                            v
                        ));
                    }
                }
            }
            if let Some(v) = svc.get("ipc").and_then(|v| v.as_str())
                && v.eq_ignore_ascii_case("shareable")
            {
                return Err(format!(
                    "{} = 'shareable' lets other containers attach. Set allow_unsafe to override.",
                    scope("ipc")
                ));
            }

            if let Some(caps) = svc.get("cap_add").and_then(|v| v.as_array()) {
                for c in caps {
                    if let Some(s) = c.as_str() {
                        let bare = s.strip_prefix("CAP_").unwrap_or(s).to_ascii_uppercase();
                        if FORBIDDEN_CAPS.contains(&bare.as_str()) {
                            return Err(format!(
                                "{} includes '{}' — grants host-equivalent privilege. Set allow_unsafe to override.",
                                scope("cap_add"),
                                s
                            ));
                        }
                    }
                }
            }

            if let Some(opts) = svc.get("security_opt").and_then(|v| v.as_array()) {
                for o in opts {
                    if let Some(s) = o.as_str() {
                        let normalized = s.replace(' ', "").to_ascii_lowercase();
                        if FORBIDDEN_SECURITY_OPTS.iter().any(|f| normalized == *f) {
                            return Err(format!(
                                "{} includes '{}' — disables container sandbox. Set allow_unsafe to override.",
                                scope("security_opt"),
                                s
                            ));
                        }
                    }
                }
            }

            if let Some(devices) = svc.get("devices").and_then(|v| v.as_array())
                && !devices.is_empty()
            {
                return Err(format!(
                    "{} maps host devices. Set allow_unsafe to override.",
                    scope("devices")
                ));
            }

            if svc.get("device_cgroup_rules").is_some() {
                return Err(format!(
                    "{} bypasses the device cgroup. Set allow_unsafe to override.",
                    scope("device_cgroup_rules")
                ));
            }
        }

        if let Some(volumes) = svc.get("volumes").and_then(|v| v.as_array()) {
            for vol in volumes {
                let source = match vol {
                    serde_json::Value::String(s) => {
                        // "src:dst[:opts]" — split on the first colon.
                        s.split(':').next().unwrap_or("").to_string()
                    }
                    serde_json::Value::Object(map) => {
                        let kind = map.get("type").and_then(|v| v.as_str()).unwrap_or("volume");
                        if kind != "bind" {
                            // Named volumes, tmpfs, npipe — host filesystem isn't directly exposed.
                            continue;
                        }
                        map.get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    }
                    _ => continue,
                };

                if source.is_empty() {
                    continue;
                }

                // Named volume short-form ("data:/var/lib/foo") — no host path.
                if !source.starts_with('/') && !source.starts_with('.') && !source.starts_with('~')
                {
                    continue;
                }

                let in_app_dir =
                    source.starts_with(&format!("{allowed_app_dir}/")) || source == allowed_app_dir;

                match always_forbidden_bind(&source) {
                    Some(BindReject::Hard(why)) => {
                        return Err(format!(
                            "{} bind-mounts '{}' — {} (off-limits even with allow_unsafe)",
                            scope("volumes"),
                            source,
                            why
                        ));
                    }
                    Some(BindReject::EngineState) if !in_app_dir => {
                        return Err(format!(
                            "{} bind-mounts '{}' — engine state under /var/lib/nasty is off-limits even with allow_unsafe",
                            scope("volumes"),
                            source
                        ));
                    }
                    _ => {}
                }

                // `/fs/<X>/…` must reference a real mounted filesystem.
                // Without this gate, the pre-create step would `mkdir -p`
                // a phantom path on rootfs, leaving stale dirs under /fs
                // every time someone typos a filesystem name. Refuse the
                // deploy unconditionally — even with allow_unsafe, polluting
                // the rootfs with /fs/<typo>/... is never what the user
                // wanted.
                if let Some(rest) = source.strip_prefix("/fs/") {
                    let fs_name = rest.split('/').next().unwrap_or("");
                    if !fs_name.is_empty() {
                        let fs_path = format!("/fs/{fs_name}");
                        use std::os::unix::fs::MetadataExt;
                        let mounted = match (std::fs::metadata(&fs_path), std::fs::metadata("/fs"))
                        {
                            (Ok(c), Ok(p)) => c.dev() != p.dev(),
                            _ => false,
                        };
                        if !mounted {
                            return Err(format!(
                                "{} bind-mounts '{}' — no filesystem is mounted at /fs/{} (typo? pick an existing filesystem from Storage → Filesystems)",
                                scope("volumes"),
                                source,
                                fs_name,
                            ));
                        }
                    }
                }

                if !allow_unsafe {
                    let allowed = in_app_dir || source.starts_with("/fs/") || source == "/fs";
                    if !allowed {
                        return Err(format!(
                            "{} bind-mounts '{}' — only paths under '{}/' or '/fs/' are allowed. Set allow_unsafe to override.",
                            scope("volumes"),
                            source,
                            allowed_app_dir
                        ));
                    }
                }
            }
        }
    }

    // Top-level named volumes can be configured as bind mounts via `driver_opts`.
    // Treat those the same as inline binds.
    if let Some(top_volumes) = parsed.get("volumes").and_then(|v| v.as_object()) {
        for (vol_name, vol) in top_volumes {
            let opts = match vol.get("driver_opts").and_then(|v| v.as_object()) {
                Some(o) => o,
                None => continue,
            };
            let is_bind = opts.get("type").and_then(|v| v.as_str()) == Some("none")
                || opts
                    .get("o")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("bind"))
                    .unwrap_or(false);
            if !is_bind {
                continue;
            }
            let device = opts.get("device").and_then(|v| v.as_str()).unwrap_or("");
            if device.is_empty() {
                continue;
            }

            let in_app_dir =
                device.starts_with(&format!("{allowed_app_dir}/")) || device == allowed_app_dir;

            match always_forbidden_bind(device) {
                Some(BindReject::Hard(why)) => {
                    return Err(format!(
                        "volumes.{vol_name} bind-mounts '{device}' — {why} (off-limits even with allow_unsafe)"
                    ));
                }
                Some(BindReject::EngineState) if !in_app_dir => {
                    return Err(format!(
                        "volumes.{vol_name} bind-mounts '{device}' — engine state under /var/lib/nasty is off-limits even with allow_unsafe"
                    ));
                }
                _ => {}
            }

            if !allow_unsafe {
                let allowed = in_app_dir || device.starts_with("/fs/") || device == "/fs";
                if !allowed {
                    return Err(format!(
                        "volumes.{vol_name} bind-mounts '{device}' — only paths under '{allowed_app_dir}/' or '/fs/' are allowed. Set allow_unsafe to override."
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Extract image references from a docker-compose YAML string.
/// Looks for `image:` fields under `services:` — skips services that use `build:` instead.
fn extract_compose_images(yaml: &str) -> Vec<String> {
    // Simple YAML parsing — look for services with image: fields
    let parsed: serde_json::Value = match serde_yaml_ng::from_str(yaml) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut images = Vec::new();
    if let Some(services) = parsed.get("services").and_then(|s| s.as_object()) {
        for (_name, svc) in services {
            if let Some(image) = svc.get("image").and_then(|i| i.as_str())
                && !image.is_empty()
                && svc.get("build").is_none()
            {
                images.push(image.to_string());
            }
        }
    }

    // Deduplicate
    images.sort();
    images.dedup();
    images
}

/// Pull a Docker image using bollard's API with structured per-layer progress.
async fn pull_image_with_progress(
    socket: &mut WebSocket,
    state: &AppState,
    image: &str,
) -> Result<(), String> {
    let docker = state
        .apps
        .docker_client()
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

#[cfg(test)]
mod tests {
    use super::validate_compose;

    fn ok_strict(yaml: &str) {
        assert!(
            validate_compose(yaml, "myapp", false).is_ok(),
            "expected strict ok: {yaml}"
        );
    }

    fn err_strict(yaml: &str, needle: &str) {
        let e = validate_compose(yaml, "myapp", false)
            .expect_err(&format!("expected strict err: {yaml}"));
        assert!(e.contains(needle), "error '{e}' did not contain '{needle}'");
    }

    fn ok_unsafe(yaml: &str) {
        assert!(
            validate_compose(yaml, "myapp", true).is_ok(),
            "expected unsafe ok: {yaml}"
        );
    }

    fn err_unsafe(yaml: &str, needle: &str) {
        let e = validate_compose(yaml, "myapp", true)
            .expect_err(&format!("expected unsafe err: {yaml}"));
        assert!(e.contains(needle), "error '{e}' did not contain '{needle}'");
    }

    // ── Strict mode (default) ───────────────────────────────────

    #[test]
    fn strict_accepts_minimal_compose() {
        ok_strict("services:\n  web:\n    image: nginx\n");
    }

    #[test]
    fn strict_rejects_privileged() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    privileged: true\n",
            "privileged",
        );
    }

    #[test]
    fn strict_rejects_host_namespaces() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    pid: host\n",
            "host",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    network_mode: host\n",
            "host",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    ipc: host\n",
            "host",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    userns_mode: host\n",
            "host",
        );
    }

    #[test]
    fn strict_rejects_dangerous_caps() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    cap_add: [SYS_ADMIN]\n",
            "SYS_ADMIN",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    cap_add: [\"CAP_SYS_PTRACE\"]\n",
            "PTRACE",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    cap_add: [ALL]\n",
            "ALL",
        );
    }

    #[test]
    fn strict_allows_safe_caps() {
        ok_strict("services:\n  ok:\n    image: alpine\n    cap_add: [NET_BIND_SERVICE]\n");
    }

    #[test]
    fn strict_rejects_security_opt_unconfined() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    security_opt: [\"seccomp=unconfined\"]\n",
            "seccomp",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    security_opt: [\"apparmor=unconfined\"]\n",
            "apparmor",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    security_opt: [\"no-new-privileges=false\"]\n",
            "no-new-privileges",
        );
    }

    #[test]
    fn strict_rejects_devices() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    devices: [\"/dev/sda:/dev/sda\"]\n",
            "devices",
        );
    }

    #[test]
    fn strict_rejects_etc_bind() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    volumes: [\"/etc:/etc\"]\n",
            "/etc",
        );
    }

    #[test]
    fn strict_allows_app_dir_and_share_root_binds() {
        // `/fs/<X>` is now gated on `<X>` being a mounted filesystem
        // (see fs_root_mounted check in validate_compose) — the test
        // can't guarantee any `/fs/*` mount exists on the runner, so
        // the `/fs/photos` case lives in its own integration-style
        // test in nasty-apps where it'd need a real mount fixture.
        ok_strict(
            "services:\n  ok:\n    image: alpine\n    volumes:\n      - \"/var/lib/nasty/apps/myapp/data:/data\"\n      - \"named-vol:/x\"\n",
        );
    }

    #[test]
    fn strict_rejects_unmounted_fs_root() {
        // The "deploys would mkdir /fs/<typo>/... on rootfs" bug. The
        // strict-mode validator must catch this before any pre-create
        // step runs, with an error that names the offending fs.
        err_strict(
            "services:\n  bad:\n    image: alpine\n    volumes:\n      - \"/fs/this-fs-does-not-exist-nasty-test/media:/media\"\n",
            "this-fs-does-not-exist-nasty-test",
        );
    }

    #[test]
    fn unsafe_also_rejects_unmounted_fs_root() {
        // allow_unsafe relaxes the path-allowlist for `/fs/`, but it
        // does NOT relax the "filesystem must be mounted" check — a
        // typo'd fs name would still pollute rootfs.
        err_unsafe(
            "services:\n  bad:\n    image: alpine\n    volumes:\n      - \"/fs/this-fs-does-not-exist-nasty-test/media:/media\"\n",
            "this-fs-does-not-exist-nasty-test",
        );
    }

    #[test]
    fn strict_rejects_long_form_bind_outside_allowed() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    volumes:\n      - type: bind\n        source: /home/user\n        target: /x\n",
            "/home/user",
        );
    }

    #[test]
    fn strict_rejects_no_services_block() {
        err_strict("version: '3'\n", "services");
    }

    // ── Unsafe mode (admin opt-in) ──────────────────────────────

    #[test]
    fn unsafe_accepts_privileged() {
        ok_unsafe("services:\n  vpn:\n    image: tailscale\n    privileged: true\n");
    }

    #[test]
    fn unsafe_accepts_dangerous_caps() {
        ok_unsafe("services:\n  vpn:\n    image: tailscale\n    cap_add: [NET_ADMIN]\n");
    }

    #[test]
    fn unsafe_accepts_host_namespace() {
        ok_unsafe("services:\n  agent:\n    image: monitor\n    network_mode: host\n");
    }

    #[test]
    fn unsafe_accepts_devices() {
        ok_unsafe("services:\n  plex:\n    image: plex\n    devices: [\"/dev/dri:/dev/dri\"]\n");
    }

    #[test]
    fn unsafe_accepts_arbitrary_bind() {
        ok_unsafe(
            "services:\n  agent:\n    image: monitor\n    volumes: [\"/etc:/host-etc:ro\"]\n",
        );
        ok_unsafe("services:\n  s:\n    image: x\n    volumes: [\"/home/user/data:/data\"]\n");
    }

    // ── Invariants — rejected even in unsafe mode ───────────────

    #[test]
    fn unsafe_still_rejects_root_bind() {
        err_unsafe(
            "services:\n  bad:\n    image: alpine\n    volumes: [\"/:/host\"]\n",
            "off-limits",
        );
    }

    #[test]
    fn unsafe_still_rejects_engine_state_bind() {
        err_unsafe(
            "services:\n  bad:\n    image: alpine\n    volumes: [\"/var/lib/nasty:/secrets\"]\n",
            "off-limits",
        );
        err_unsafe(
            "services:\n  bad:\n    image: alpine\n    volumes: [\"/var/lib/nasty/auth.json:/x\"]\n",
            "off-limits",
        );
    }

    #[test]
    fn unsafe_still_allows_app_dir_under_engine_state() {
        // /var/lib/nasty/apps/<name>/ is a deliberate exception — the app
        // owns its own subtree of engine state.
        ok_unsafe(
            "services:\n  ok:\n    image: alpine\n    volumes: [\"/var/lib/nasty/apps/myapp/data:/data\"]\n",
        );
    }

    #[test]
    fn unsafe_still_rejects_dotdot_escape() {
        err_unsafe(
            "services:\n  bad:\n    image: alpine\n    volumes: [\"/var/lib/nasty/apps/myapp/../auth.json:/x\"]\n",
            "off-limits",
        );
    }

    #[test]
    fn unsafe_still_rejects_top_volume_to_root() {
        err_unsafe(
            "services:\n  s:\n    image: alpine\n    volumes: [evil:/x]\nvolumes:\n  evil:\n    driver_opts:\n      type: none\n      o: bind\n      device: /\n",
            "off-limits",
        );
    }

    #[test]
    fn unsafe_still_rejects_unparseable_yaml() {
        err_unsafe("services: [unbalanced\n", "parse");
    }
}
