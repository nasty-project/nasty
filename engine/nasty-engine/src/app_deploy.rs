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
    /// For compose: optional `.env` file contents (Immich/Nextcloud-style
    /// config). Written to the project directory for `${VAR}` substitution.
    #[serde(default)]
    env_file: Option<String>,
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
    /// Named registry credentials pinned to this app. Secrets remain in the
    /// engine; the deployment stream carries opaque IDs only.
    #[serde(default)]
    registry_credential_ids: Option<Vec<String>>,
}

#[derive(Serialize, Default)]
struct DeployMessage {
    /// "log" for output lines, "error" for errors, "done" for completion,
    /// "action" for a structured actionable hint the UI can render as a button.
    #[serde(rename = "type")]
    msg_type: String,
    data: String,
    /// Action key for `type:"action"` messages (e.g. "create_macvlan").
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    /// Host bridge name the action applies to (for "create_macvlan").
    #[serde(skip_serializing_if = "Option::is_none")]
    bridge: Option<String>,
}

impl DeployMessage {
    fn log(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "log".into(),
            data: s.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    fn error(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "error".into(),
            data: s.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    fn done(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "done".into(),
            data: s.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    /// Actionable hint (#429/#435): the referenced external network is a
    /// host bridge, so offer to create a macvlan network on it and retry.
    fn action_create_macvlan(bridge: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "action".into(),
            data: format!(
                "Create a macvlan network on '{bridge}' to put this app on your LAN, then retry."
            ),
            action: Some("create_macvlan".into()),
            bridge: Some(bridge.to_string()),
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

    if let Err(e) = nasty_apps::validate_app_name(&req.name) {
        report_error(&mut socket, "<invalid>", "validate", &e.to_string()).await;
        return;
    }

    let token = match pre_auth_token.or_else(|| req.token.clone()) {
        Some(t) => t,
        None => {
            report_error(&mut socket, &req.name, "auth", "missing session").await;
            return;
        }
    };

    // Authenticate — admin only. Compose deploys can mount host paths and run
    // privileged containers, so this is effectively root-equivalent.
    let session = match state.auth.validate(&token, &client_ip).await {
        Ok(s) => {
            if let Err(reason) =
                crate::auth::authorize_session(&s, crate::auth::EndpointAccess::RootEquivalent)
            {
                crate::auth::audit(
                    "app_deploy_denied",
                    &s.username,
                    &client_ip,
                    &format!("reason={reason:?}"),
                );
                report_error(&mut socket, &req.name, "auth", reason.message()).await;
                return;
            }
            s
        }
        Err(_) => {
            report_error(&mut socket, &req.name, "auth", "invalid token").await;
            return;
        }
    };

    crate::auth::audit(
        "app_deploy_requested",
        &session.username,
        &client_ip,
        &format!(
            "app={} kind={} allow_unsafe={}",
            req.name, req.kind, req.allow_unsafe
        ),
    );

    info!(
        "Deploy stream started for '{}' (kind: {})",
        req.name, req.kind
    );

    match req.kind.as_str() {
        "simple" => deploy_simple(&mut socket, &state, &req, &session, &client_ip).await,
        "compose" => deploy_compose(&mut socket, &state, &req, &session, &client_ip).await,
        "pull" => deploy_pull(&mut socket, &state, &req).await,
        _ => {
            report_error(&mut socket, &req.name, "dispatch", "unknown deploy kind").await;
        }
    }
    if let Err(e) = crate::router::apps::sync_published_firewall_ports(&state).await {
        crate::auth::audit(
            "app_firewall_sync_failed",
            &session.username,
            &client_ip,
            &format!("app={} error={e}", req.name),
        );
        warn!("deploy '{}': firewall reconciliation failed: {e}", req.name);
    }
}

async fn deploy_simple(
    socket: &mut WebSocket,
    state: &AppState,
    req: &DeployRequest,
    session: &crate::auth::Session,
    client_ip: &str,
) {
    let app_registry_guard = state.apps.app_registry_read_guard().await;
    if let Err(e) = nasty_apps::validate_new_app_name(&req.name) {
        report_error(socket, &req.name, "validate", &e.to_string()).await;
        return;
    }
    let image = match &req.image {
        Some(img) => img.clone(),
        None => {
            report_error(socket, &req.name, "validate", "missing image").await;
            return;
        }
    };

    // Parse install params + subdomain conflict check up front, *before*
    // the pull, so the operator doesn't sit through a 60-second image
    // download just to discover the chosen hostname is already taken.
    // Same conflict logic the apps.ingress.set RPC runs in #231.
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
    params.image = image.clone();
    // Top-level flag wins over anything embedded in install_params, so the
    // privileged-deploy decision is plainly visible in the deploy request.
    params.allow_unsafe = req.allow_unsafe;
    if let Some(ids) = &req.registry_credential_ids {
        params.registry_credential_ids = Some(ids.clone());
    }
    let registry_credential_ids = params
        .registry_credential_ids
        .as_deref()
        .unwrap_or_default();
    if let Err(e) = nasty_apps::validate_new_app_request(&params) {
        report_error(socket, &req.name, "validate", &e.to_string()).await;
        return;
    }
    if let Err(error) = state
        .apps
        .validate_registry_bindings(std::slice::from_ref(&image), registry_credential_ids)
        .await
    {
        report_error(
            socket,
            &req.name,
            "validate-credentials",
            &error.to_string(),
        )
        .await;
        return;
    }
    if let Some(ref s) = params.subdomain
        && let Some(reason) =
            crate::ingress_conflict::find_subdomain_conflict(state, &params.name, s).await
    {
        report_error(
            socket,
            &req.name,
            "validate",
            &format!("subdomain conflict: {reason}"),
        )
        .await;
        return;
    }

    // Step 1: Pull image via bollard with structured progress
    let _ = socket
        .send(Message::Text(
            DeployMessage::log(&format!("Pulling image: {image}")).into(),
        ))
        .await;

    if let Err(e) = pull_image_with_progress(socket, state, &image, registry_credential_ids).await {
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

    if req.allow_unsafe {
        crate::auth::audit(
            "simple_unsafe_deploy",
            &session.username,
            client_ip,
            &format!("app={}", req.name),
        );
        let _ = socket
            .send(Message::Text(DeployMessage::log(
                "WARNING: deploying with allow_unsafe — bind mounts outside the sandbox are permitted",
            ).into()))
            .await;
    }

    // Did this deploy opt into a subdomain ingress? install() wires the
    // Caddy route internally, but the TLS-automation layer lives in
    // nasty-system (which nasty-apps can't call without a dep cycle), so
    // the refresh has to be kicked from here — mirroring the
    // apps.ingress.set arm. Without it a deploy-time subdomain wouldn't
    // land in Caddy's certificates.automate until the next engine
    // restart, leaving the host on Caddy's default automation policy
    // (no configured DNS-01 provider ⇒ no cert at all on a DNS-challenge
    // box).
    let chose_subdomain = params
        .subdomain
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    match state
        .apps
        .install_with_app_registry_guard(params, app_registry_guard)
        .await
    {
        Ok(app) => {
            if chose_subdomain {
                tokio::spawn(nasty_system::settings::reapply_tls_from_disk());
            }
            if let Err(e) = crate::router::apps::sync_published_firewall_ports(state).await {
                report_error(socket, &req.name, "firewall", &e).await;
                return;
            }
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

async fn deploy_compose(
    socket: &mut WebSocket,
    state: &AppState,
    req: &DeployRequest,
    session: &crate::auth::Session,
    client_ip: &str,
) {
    let app_registry_guard = state.apps.app_registry_read_guard().await;
    let _compose_guard = state.apps.compose_mutation_guard().await;
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
            &session.username,
            client_ip,
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
    if std::path::Path::new(&compose_dir).exists() && !is_update {
        report_error(
            socket,
            &req.name,
            "validate",
            "app state directory already exists without a compose file",
        )
        .await;
        return;
    }
    if !is_update && let Err(e) = state.apps.ensure_new_app_name_available(&req.name).await {
        report_error(socket, &req.name, "validate", &e.to_string()).await;
        return;
    }

    let registry_credential_ids = match &req.registry_credential_ids {
        Some(ids) => ids.clone(),
        None if is_update => state.apps.app_registry_credential_ids(&req.name).await,
        None => Vec::new(),
    };
    if let Err(error) = state
        .apps
        .validate_registry_credential_ids(&registry_credential_ids)
        .await
    {
        report_error(
            socket,
            &req.name,
            "validate-credentials",
            &error.to_string(),
        )
        .await;
        return;
    }

    let attempt = uuid::Uuid::new_v4();
    let staged_compose_path = format!("{compose_dir}/.nasty-compose-{attempt}.yml");
    let staged_env_path = format!("{compose_dir}/.nasty-env-{attempt}");
    let compose_backup_path = format!("{compose_dir}/.nasty-compose-{attempt}.backup");
    let env_backup_path = format!("{compose_dir}/.nasty-env-{attempt}.backup");

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
    if let Err(error) = state
        .apps
        .begin_compose_deployment_transaction(&req.name, &app_registry_guard)
        .await
    {
        discard_compose_stage(&compose_dir, &[], !is_update).await;
        report_error(
            socket,
            &req.name,
            "stage-transaction",
            &format!("could not reserve compose transaction: {error}"),
        )
        .await;
        return;
    }
    if let Err(e) = tokio::fs::write(&staged_compose_path, &compose_content).await {
        rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
        report_error(
            socket,
            &req.name,
            "write-compose",
            &format!("failed to write compose file: {e}"),
        )
        .await;
        return;
    }
    // Stage and swap `.env` before validation. Compose's global --env-file
    // handles interpolation, while a service-level `env_file: .env` reads the
    // canonical project path directly. The backup is restored on any failure.
    let env_path = format!("{}/.env", compose_dir);
    if let Err(e) = tokio::fs::write(
        &staged_env_path,
        nasty_apps::render_env_file(&req.name, req.env_file.as_deref()),
    )
    .await
    {
        rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
        report_error(
            socket,
            &req.name,
            "write-env",
            &format!("failed to write compose environment: {e}"),
        )
        .await;
        return;
    }
    let had_env = match install_staged_file(&staged_env_path, &env_path, &env_backup_path).await {
        Ok(had_env) => had_env,
        Err(error) => {
            rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
            report_error(socket, &req.name, "stage-env", &error).await;
            return;
        }
    };

    // Validate
    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Validating compose file...").into(),
        ))
        .await;
    if let Err(e) = stream_command(
        socket,
        "docker",
        &[
            "compose",
            "--env-file",
            &env_path,
            "-f",
            &staged_compose_path,
            "config",
            "--quiet",
        ],
    )
    .await
    {
        abort_compose_stage(
            &compose_dir,
            &staged_compose_path,
            &env_path,
            &env_backup_path,
            had_env,
            !is_update,
        )
        .await;
        rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
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
    let image_plan = match state
        .apps
        .compose_image_plan_from_path(
            std::path::Path::new(&staged_compose_path),
            Some(std::path::Path::new(&env_path)),
        )
        .await
    {
        Ok(plan) => plan,
        Err(error) => {
            report_error(
                socket,
                &req.name,
                "resolve-images",
                &format!("could not resolve effective compose images: {error}"),
            )
            .await;
            abort_compose_stage(
                &compose_dir,
                &staged_compose_path,
                &env_path,
                &env_backup_path,
                had_env,
                !is_update,
            )
            .await;
            rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
            return;
        }
    };
    if let Err(error) = state
        .apps
        .validate_registry_bindings(&image_plan.images, &registry_credential_ids)
        .await
    {
        report_error(
            socket,
            &req.name,
            "validate-credentials",
            &error.to_string(),
        )
        .await;
        abort_compose_stage(
            &compose_dir,
            &staged_compose_path,
            &env_path,
            &env_backup_path,
            had_env,
            !is_update,
        )
        .await;
        rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
        return;
    }
    if image_plan.pull_images.is_empty() {
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("No image pulls are due.").into(),
            ))
            .await;
    } else {
        for image in &image_plan.pull_images {
            let _ = socket
                .send(Message::Text(
                    DeployMessage::log(&format!("Pulling: {image}")).into(),
                ))
                .await;
            if let Err(e) =
                pull_image_with_progress(socket, state, image, &registry_credential_ids).await
            {
                abort_compose_stage(
                    &compose_dir,
                    &staged_compose_path,
                    &env_path,
                    &env_backup_path,
                    had_env,
                    !is_update,
                )
                .await;
                rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
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

    let previous_registry_credential_ids = state.apps.app_registry_credential_ids(&req.name).await;
    if let Err(error) = state
        .apps
        .set_app_registry_credential_ids_with_guard(
            &req.name,
            &registry_credential_ids,
            &app_registry_guard,
        )
        .await
    {
        abort_compose_stage(
            &compose_dir,
            &staged_compose_path,
            &env_path,
            &env_backup_path,
            had_env,
            !is_update,
        )
        .await;
        rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
        report_error(
            socket,
            &req.name,
            "write-credentials",
            &format!("credential binding could not be reserved: {error}"),
        )
        .await;
        return;
    }

    let _had_compose = match install_staged_file(
        &staged_compose_path,
        &compose_path,
        &compose_backup_path,
    )
    .await
    {
        Ok(had_compose) => had_compose,
        Err(error) => {
            if let Err(restore_error) = state
                .apps
                .set_app_registry_credential_ids_with_guard(
                    &req.name,
                    &previous_registry_credential_ids,
                    &app_registry_guard,
                )
                .await
            {
                warn!(app = %req.name, %restore_error, "failed to restore registry bindings after staging failure");
            }
            abort_compose_stage(
                &compose_dir,
                &staged_compose_path,
                &env_path,
                &env_backup_path,
                had_env,
                !is_update,
            )
            .await;
            rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
            report_error(socket, &req.name, "stage-compose", &error).await;
            return;
        }
    };

    if let Err(error) = state.apps.sync_compose_deployment_files(&req.name).await {
        rollback_compose_transaction(state, &req.name, &app_registry_guard, false).await;
        report_error(
            socket,
            &req.name,
            "sync-compose",
            &format!("could not persist compose transaction: {error}"),
        )
        .await;
        return;
    }

    // Start containers
    let _ = socket
        .send(Message::Text(
            DeployMessage::log("Starting containers...").into(),
        ))
        .await;
    let mut owned_args = vec![
        "compose".to_string(),
        "--env-file".to_string(),
        env_path.clone(),
    ];
    owned_args.extend(nasty_apps::compose_file_args(&req.name));
    owned_args.extend([
        "--project-name".to_string(),
        req.name.clone(),
        "up".to_string(),
        "-d".to_string(),
        "--pull".to_string(),
        "never".to_string(),
        "--no-build".to_string(),
    ]);
    if is_update {
        owned_args.push("--remove-orphans".to_string());
    }
    let args: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    if let Err(e) = stream_command(socket, "docker", &args).await {
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Cleaning up failed deployment...").into(),
            ))
            .await;
        rollback_compose_transaction(state, &req.name, &app_registry_guard, true).await;
        // #429: docker's "network <x> declared as external, but could not
        // be found" is opaque when <x> is a host bridge (Settings →
        // Network) rather than a Docker network. Surface a targeted hint.
        let hint = host_bridge_network_hint(&e);
        if let Some(hint) = &hint {
            let _ = socket
                .send(Message::Text(
                    DeployMessage::log(&format!("Hint: {hint}")).into(),
                ))
                .await;
            // Structured signal so the UI can offer a one-click "create
            // macvlan on <bridge> and retry" instead of just prose.
            if let Some(bridge) = external_network_name(&e) {
                let _ = socket
                    .send(Message::Text(
                        DeployMessage::action_create_macvlan(bridge).into(),
                    ))
                    .await;
            }
        }
        let msg = match hint {
            Some(hint) => format!("deploy failed: {e}\n\nHint: {hint}"),
            None => format!("deploy failed: {e}"),
        };
        report_error(socket, &req.name, "compose-up", &msg).await;
        return;
    }

    if let Err(error) = write_app_meta(&compose_dir, req.allow_unsafe).await {
        rollback_compose_transaction(state, &req.name, &app_registry_guard, true).await;
        report_error(
            socket,
            &req.name,
            "commit-compose",
            &format!("app started but its deployment metadata could not be saved: {error}"),
        )
        .await;
        return;
    }
    if let Err(error) = state
        .apps
        .finish_compose_deployment_transaction(&req.name)
        .await
    {
        report_error(
            socket,
            &req.name,
            "commit-compose",
            &format!("app started but its transaction could not be committed: {error}"),
        )
        .await;
        return;
    }
    for backup in [&compose_backup_path, &env_backup_path] {
        if let Err(error) = tokio::fs::remove_file(backup).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, %backup, "failed to remove compose deployment backup");
        }
    }

    // Fresh installs start in path-prefix mode. Updates preserve the saved
    // hostname and retarget it to the requested, pinned, or first TCP port.
    if let Err(error) = state
        .apps
        .refresh_compose_ingress_after_deploy(&req.name, req.ingress_host_port, is_update)
        .await
    {
        warn!(
            app = %req.name,
            %error,
            "compose deployment succeeded but ingress refresh failed"
        );
        let _ = socket
            .send(Message::Text(
                DeployMessage::log(&format!(
                    "WARNING: app deployment succeeded, but ingress could not be refreshed: {error}"
                ))
                .into(),
            ))
            .await;
    }

    if let Err(e) = crate::router::apps::sync_published_firewall_ports(state).await {
        report_error(socket, &req.name, "firewall", &e).await;
        return;
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

async fn discard_compose_stage(compose_dir: &str, paths: &[&str], remove_project_dir: bool) {
    if remove_project_dir {
        if let Err(error) = tokio::fs::remove_dir_all(compose_dir).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, path = compose_dir, "failed to remove staged compose project");
        }
        return;
    }
    for path in paths {
        if let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, %path, "failed to remove staged compose file");
        }
    }
}

async fn rollback_compose_transaction(
    state: &AppState,
    app_name: &str,
    guard: &nasty_apps::AppRegistryReadGuard,
    reconcile_docker: bool,
) {
    if let Err(error) = state
        .apps
        .rollback_compose_deployment_transaction(app_name, guard, reconcile_docker)
        .await
    {
        warn!(%error, app = app_name, "failed to roll back compose deployment transaction; recovery marker retained");
    }
}

async fn install_staged_file(staged: &str, canonical: &str, backup: &str) -> Result<bool, String> {
    let had_original = std::path::Path::new(canonical).exists();
    if had_original {
        tokio::fs::rename(canonical, backup)
            .await
            .map_err(|error| format!("backup {canonical}: {error}"))?;
    }
    if let Err(error) = tokio::fs::rename(staged, canonical).await {
        if had_original && let Err(restore_error) = tokio::fs::rename(backup, canonical).await {
            warn!(%restore_error, %canonical, "failed to restore file after staging failure");
        }
        return Err(format!("install staged {canonical}: {error}"));
    }
    Ok(had_original)
}

async fn restore_staged_file(canonical: &str, backup: &str, had_original: bool) {
    if let Err(error) = tokio::fs::remove_file(canonical).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(%error, %canonical, "failed to remove staged file during rollback");
    }
    if had_original && let Err(error) = tokio::fs::rename(backup, canonical).await {
        warn!(%error, %canonical, "failed to restore original file during rollback");
    }
}

async fn abort_compose_stage(
    compose_dir: &str,
    staged_compose_path: &str,
    env_path: &str,
    env_backup_path: &str,
    had_env: bool,
    remove_project_dir: bool,
) {
    // The transaction snapshot owns fresh-install cleanup. Keeping the marker
    // until it runs makes a failed cleanup recoverable on boot.
    if !remove_project_dir {
        restore_staged_file(env_path, env_backup_path, had_env).await;
        discard_compose_stage(compose_dir, &[staged_compose_path], false).await;
    }
}

async fn deploy_pull(socket: &mut WebSocket, state: &AppState, req: &DeployRequest) {
    let app_registry_guard = state.apps.app_registry_read_guard().await;
    let compose_path = format!("/var/lib/nasty/apps/{}/docker-compose.yml", req.name);

    if std::path::Path::new(&compose_path).exists() {
        let _compose_guard = state.apps.compose_mutation_guard().await;
        // Compose app: pull via bollard + recreate
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Pulling latest images...").into(),
            ))
            .await;
        let credential_ids = state.apps.app_registry_credential_ids(&req.name).await;
        let images = match state.apps.compose_images(&req.name).await {
            Ok(images) => images,
            Err(error) => {
                report_error(socket, &req.name, "resolve-images", &error.to_string()).await;
                return;
            }
        };
        if let Err(error) = state
            .apps
            .validate_registry_bindings(&images, &credential_ids)
            .await
        {
            report_error(
                socket,
                &req.name,
                "validate-credentials",
                &error.to_string(),
            )
            .await;
            return;
        }
        for image in images {
            let _ = socket
                .send(Message::Text(
                    DeployMessage::log(&format!("Pulling: {image}")).into(),
                ))
                .await;
            if let Err(e) = pull_image_with_progress(socket, state, &image, &credential_ids).await {
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
        let mut up_args = vec!["compose".to_string()];
        up_args.extend(nasty_apps::compose_file_args(&req.name));
        up_args.extend([
            "--project-name".to_string(),
            req.name.clone(),
            "up".to_string(),
            "-d".to_string(),
            "--pull".to_string(),
            "never".to_string(),
            "--no-build".to_string(),
            "--remove-orphans".to_string(),
        ]);
        let up_refs: Vec<&str> = up_args.iter().map(String::as_str).collect();
        if let Err(e) = stream_command(socket, "docker", &up_refs).await {
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
        let credential_ids = state.apps.app_registry_credential_ids(&req.name).await;
        if let Err(error) = state
            .apps
            .validate_registry_bindings(std::slice::from_ref(&image), &credential_ids)
            .await
        {
            report_error(
                socket,
                &req.name,
                "validate-credentials",
                &error.to_string(),
            )
            .await;
            return;
        }
        if let Err(e) = pull_image_with_progress(socket, state, &image, &credential_ids).await {
            report_error(socket, &req.name, "pull", &format!("pull failed: {e}")).await;
            return;
        }

        // Recreate container
        let _ = socket
            .send(Message::Text(
                DeployMessage::log("Recreating container...").into(),
            ))
            .await;
        match state
            .apps
            .recreate_after_pull_with_app_registry_guard(&req.name, app_registry_guard)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                report_error(socket, &req.name, "recreate", &e.to_string()).await;
                return;
            }
        }
    }

    if let Err(e) = crate::router::apps::sync_published_firewall_ports(state).await {
        report_error(socket, &req.name, "firewall", &e).await;
        return;
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
    let mut command = Command::new(cmd);
    command.args(args);
    if cmd == "docker" {
        let docker_config = "/run/nasty-docker-config";
        tokio::fs::create_dir_all(docker_config)
            .await
            .map_err(|error| format!("create isolated Docker config: {error}"))?;
        command.env("DOCKER_CONFIG", docker_config);
    }
    let mut child = command
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

/// Parse the network name out of docker compose's
/// "network <name> declared as external, but could not be found" error.
/// Returns `None` if the error isn't that shape, or the name isn't a
/// plausible Linux interface name — the latter guards the sysfs probe in
/// [`host_bridge_network_hint`] against path traversal / junk.
fn external_network_name(err: &str) -> Option<&str> {
    let line = err
        .lines()
        .find(|l| l.contains("declared as external, but could not be found"))?;
    let name = line
        .split_once("network ")?
        .1
        .split_once(" declared as external")?
        .0
        .trim();
    let plausible_ifname = !name.is_empty()
        && name.len() <= 15 // IFNAMSIZ - 1
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    plausible_ifname.then_some(name)
}

/// If a compose deploy failed because it referenced an external Docker
/// network that is actually a *host* Linux bridge (Settings → Network),
/// return a hint explaining the mismatch (#429). A host bridge isn't a
/// Docker network, so `networks: { br0: { external: true } }` can never
/// resolve it — docker's own error ("declared as external, but could not
/// be found") doesn't say why. We only add the hint when a bridge of that
/// exact name actually exists, so it can't mislead in the ordinary
/// typo'd-network-name case.
fn host_bridge_network_hint(err: &str) -> Option<String> {
    let name = external_network_name(err)?;
    // A Linux bridge exposes a `bridge/` subdir under its sysfs node.
    if !std::path::Path::new(&format!("/sys/class/net/{name}/bridge")).is_dir() {
        return None;
    }
    Some(format!(
        "'{name}' is a host network bridge (Settings → Network), not a Docker network, so a \
         compose `networks: {{ {name}: {{ external: true }} }}` can't attach containers to it — \
         host bridges are for VMs. Either remove the external network and reach the app through \
         NASty's ingress, or first create a Docker macvlan network on '{name}': \
         `docker network create -d macvlan --parent={name} --subnet=<cidr> --gateway=<gw> {name}`."
    ))
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
    if std::path::Path::new(source)
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Some(BindReject::Hard(
            "'..' traversal is never allowed in bind sources",
        ));
    }
    if source == "/" {
        return Some(BindReject::Hard(
            "'/' (host root) is never allowed as a bind source",
        ));
    }
    let engine_state = projected_canonical_path("/var/lib/nasty");
    let source_path = std::path::Path::new(source);
    if source == "/var/lib/nasty"
        || source.starts_with("/var/lib/nasty/")
        || (source_path.is_absolute() && projected_canonical_path(source).starts_with(engine_state))
    {
        return Some(BindReject::EngineState);
    }
    None
}

fn projected_canonical_path(path: &str) -> std::path::PathBuf {
    let original = std::path::PathBuf::from(path);
    if !original.is_absolute() {
        return original;
    }

    let mut existing = original.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }

    let mut resolved = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    resolved
}

fn projected_compose_path(path: &str, project_dir: &str) -> std::path::PathBuf {
    if std::path::Path::new(path).is_absolute() {
        projected_canonical_path(path)
    } else {
        projected_canonical_path(
            std::path::Path::new(project_dir)
                .join(path)
                .to_string_lossy()
                .as_ref(),
        )
    }
}

fn operator_bind_allowed(source: &str, allowed_app_dir: &str) -> bool {
    if !source.starts_with('/') && !source.starts_with('.') {
        return false;
    }

    let source = projected_compose_path(source, allowed_app_dir);
    [allowed_app_dir, "/appdata", "/fs"]
        .iter()
        .map(|root| projected_canonical_path(root))
        .any(|root| root != std::path::Path::new("/") && source.starts_with(root))
}

fn value_has_unsafe_host_reference(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => {
            value.starts_with('/')
                || value.starts_with('~')
                || value.contains('$')
                || value.contains("..")
        }
        serde_json::Value::Array(values) => values.iter().any(value_has_unsafe_host_reference),
        _ => false,
    }
}

fn value_has_protected_host_reference(value: &serde_json::Value, allowed_app_dir: &str) -> bool {
    match value {
        serde_json::Value::String(path) => {
            if path.contains('$') {
                return true;
            }
            let resolved = projected_compose_path(path, allowed_app_dir);
            let resolved_app_dir = projected_canonical_path(allowed_app_dir);
            let in_app_dir = resolved.starts_with(&resolved_app_dir);
            matches!(always_forbidden_bind(path), Some(BindReject::Hard(_)))
                || match always_forbidden_bind(&resolved.to_string_lossy()) {
                    Some(BindReject::Hard(_)) => true,
                    Some(BindReject::EngineState) => !in_app_dir,
                    None => false,
                }
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| value_has_protected_host_reference(value, allowed_app_dir)),
        serde_json::Value::Object(values) => values
            .values()
            .any(|value| value_has_protected_host_reference(value, allowed_app_dir)),
        _ => false,
    }
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
pub(crate) fn validate_compose(
    yaml: &str,
    app_name: &str,
    allow_unsafe: bool,
) -> Result<(), String> {
    let parsed = nasty_apps::parse_compose_yaml(yaml)
        .map_err(|e| format!("compose YAML failed to parse: {e}"))?;

    let services = parsed
        .get("services")
        .and_then(|s| s.as_object())
        .ok_or_else(|| "compose file has no `services:` map".to_string())?;

    let allowed_app_dir = format!("/var/lib/nasty/apps/{app_name}");

    for (svc_name, svc) in services {
        let scope = |field: &str| format!("services.{svc_name}.{field}");

        if !allow_unsafe {
            if svc.get("privileged").is_some_and(|value| {
                value.as_bool() == Some(true)
                    || value.as_str().is_some_and(|value| value.contains('$'))
            }) {
                return Err(format!(
                    "{} sets privileged: true (host-root equivalent). Set allow_unsafe to override.",
                    scope("privileged")
                ));
            }

            for field in [
                "pid",
                "ipc",
                "uts",
                "userns_mode",
                "cgroup",
                "cgroupns_mode",
                "network_mode",
            ] {
                if let Some(v) = svc.get(field).and_then(|v| v.as_str()) {
                    let v_lower = v.to_ascii_lowercase();
                    if v.contains('$') || v_lower == "host" || v_lower.starts_with("host:") {
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
                        if s.contains('$') || FORBIDDEN_CAPS.contains(&bare.as_str()) {
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
                        if s.contains('$')
                            || FORBIDDEN_SECURITY_OPTS.iter().any(|f| normalized == *f)
                        {
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

            if svc.get("use_api_socket").is_some_and(|value| {
                value.as_bool() == Some(true)
                    || value.as_str().is_some_and(|value| value.contains('$'))
            }) {
                return Err(format!(
                    "{} exposes the Docker API socket. Set allow_unsafe to override.",
                    scope("use_api_socket")
                ));
            }

            for field in ["env_file", "label_file"] {
                if svc.get(field).is_some_and(value_has_unsafe_host_reference) {
                    return Err(format!(
                        "{} reads an absolute, escaping, or interpolated host path. Set allow_unsafe to override.",
                        scope(field)
                    ));
                }
            }
        }

        for field in ["env_file", "label_file"] {
            if svc
                .get(field)
                .is_some_and(|value| value_has_protected_host_reference(value, &allowed_app_dir))
            {
                return Err(format!(
                    "{} reads an interpolated or protected host path",
                    scope(field)
                ));
            }
        }

        if svc.get("extends").is_some() || svc.get("volumes_from").is_some() {
            return Err(format!(
                "{} imports content that cannot be checked against the protected host-path policy",
                scope("extends/volumes_from")
            ));
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
                        if kind.contains('$') {
                            return Err(format!(
                                "{} has an interpolated mount type, which cannot be safety-checked",
                                scope("volumes")
                            ));
                        }
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

                // Compose expands these after parsing, so this validator cannot
                // prove an interpolated source avoids protected host state.
                if source.contains('$') {
                    return Err(format!(
                        "{} bind source '{}' is interpolated and cannot be safety-checked",
                        scope("volumes"),
                        source
                    ));
                }

                // Named volume short-form ("data:/var/lib/foo") — no host path.
                if !source.starts_with('/') && !source.starts_with('.') && !source.starts_with('~')
                {
                    continue;
                }

                let resolved_source = projected_compose_path(&source, &allowed_app_dir);
                let policy_source = resolved_source.to_string_lossy();
                let resolved_app_dir = projected_canonical_path(&allowed_app_dir);
                let in_app_dir = source.starts_with(&format!("{allowed_app_dir}/"))
                    || source == allowed_app_dir
                    || resolved_source.starts_with(&resolved_app_dir);

                match always_forbidden_bind(&source)
                    .or_else(|| always_forbidden_bind(&policy_source))
                {
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
                    // `/appdata` is the stable symlink onto the appdata
                    // subvolume (#436) — sandbox territory by definition.
                    let allowed = operator_bind_allowed(&source, &allowed_app_dir);
                    if !allowed {
                        return Err(format!(
                            "{} bind-mounts '{}' — only paths under '{}/', '/appdata/' or '/fs/' are allowed. Set allow_unsafe to override.",
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
            if ["type", "o", "device"].iter().any(|key| {
                opts.get(*key)
                    .and_then(|value| value.as_str())
                    .is_some_and(|value| value.contains('$'))
            }) {
                return Err(format!(
                    "volumes.{vol_name}.driver_opts contains interpolation that cannot be safety-checked"
                ));
            }
            if !is_bind {
                continue;
            }
            let device = opts.get("device").and_then(|v| v.as_str()).unwrap_or("");
            if device.is_empty() {
                continue;
            }

            let resolved_device = projected_compose_path(device, &allowed_app_dir);
            let policy_device = resolved_device.to_string_lossy();
            let resolved_app_dir = projected_canonical_path(&allowed_app_dir);
            let in_app_dir = device.starts_with(&format!("{allowed_app_dir}/"))
                || device == allowed_app_dir
                || resolved_device.starts_with(&resolved_app_dir);

            match always_forbidden_bind(device).or_else(|| always_forbidden_bind(&policy_device)) {
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
                let allowed = operator_bind_allowed(device, &allowed_app_dir);
                if !allowed {
                    return Err(format!(
                        "volumes.{vol_name} bind-mounts '{device}' — only paths under '{allowed_app_dir}/', '/appdata/' or '/fs/' are allowed. Set allow_unsafe to override."
                    ));
                }
            }
        }
    }

    if parsed.get("include").is_some() {
        return Err(
            "top-level include imports content that cannot be checked against the protected host-path policy"
                .to_string(),
        );
    }

    for section in ["configs", "secrets"] {
        if let Some(entries) = parsed.get(section).and_then(|value| value.as_object()) {
            for (name, entry) in entries {
                if let Some(file) = entry.get("file") {
                    if value_has_protected_host_reference(file, &allowed_app_dir) {
                        return Err(format!(
                            "{section}.{name}.file reads an interpolated or protected host path"
                        ));
                    }
                    if !allow_unsafe && value_has_unsafe_host_reference(file) {
                        return Err(format!(
                            "{section}.{name}.file reads an absolute or escaping host path and requires Admin access"
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Pull a Docker image using bollard's API with structured per-layer progress.
async fn pull_image_with_progress(
    socket: &mut WebSocket,
    state: &AppState,
    image: &str,
    registry_credential_ids: &[String],
) -> Result<(), String> {
    let docker = state
        .apps
        .docker_client()
        .map_err(|e| format!("Docker not ready: {e}"))?;

    let parsed = nasty_apps::parse_image_ref(image).map_err(|error| error.to_string())?;

    let options = CreateImageOptions {
        from_image: Some(parsed.from_image),
        tag: Some(parsed.reference),
        ..Default::default()
    };
    let credentials = state
        .apps
        .registry_credentials_for_image(image, registry_credential_ids)
        .await
        .map_err(|error| error.to_string())?;

    let mut stream = docker.create_image(Some(options), None, credentials);
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
    use super::{
        BindReject, always_forbidden_bind, external_network_name, operator_bind_allowed,
        projected_canonical_path, projected_compose_path, validate_compose,
    };

    #[test]
    fn external_network_name_parses_docker_error() {
        let err =
            "Starting containers...\nnetwork br0 declared as external, but could not be found";
        assert_eq!(external_network_name(err), Some("br0"));
    }

    #[test]
    fn external_network_name_handles_error_prefixed_line() {
        // The line as it can appear wrapped with docker's ERROR: prefix.
        let err = "ERROR: network mylan declared as external, but could not be found";
        assert_eq!(external_network_name(err), Some("mylan"));
    }

    #[test]
    fn external_network_name_none_for_unrelated_errors() {
        assert_eq!(external_network_name("pull failed: manifest unknown"), None);
        assert_eq!(external_network_name(""), None);
    }

    #[test]
    fn external_network_name_rejects_implausible_names() {
        // Guards the sysfs probe: no path traversal, no over-long names.
        let traversal = "network ../../etc/shadow declared as external, but could not be found";
        assert_eq!(external_network_name(traversal), None);
        let too_long =
            "network thisnameiswaytoolongforaniface declared as external, but could not be found";
        assert_eq!(external_network_name(too_long), None);
    }

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
    fn compose_accepts_periodic_pull_policies() {
        for policy in ["daily", "weekly", "every_12h", "every_1h30m"] {
            ok_strict(&format!(
                "services:\n  web:\n    image: nginx\n    pull_policy: {policy}\n"
            ));
        }
    }

    #[test]
    fn strict_rejects_privileged() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    privileged: true\n",
            "privileged",
        );
    }

    #[test]
    fn compose_policy_inspects_expanded_merge_keys() {
        err_strict(
            "x-base: &base\n  image: alpine\n  privileged: true\nservices:\n  app:\n    <<: *base\n",
            "privileged",
        );
        err_strict(
            "x-base: &base\n  image: alpine\n  network_mode: host\nservices:\n  app:\n    <<: *base\n",
            "network_mode",
        );
        err_strict(
            "x-base: &base\n  image: alpine\n  volumes: [\"/etc:/host-etc\"]\nservices:\n  app:\n    <<: *base\n",
            "/etc",
        );
        err_unsafe(
            "x-base: &base\n  image: alpine\n  volumes: [\"/:/host\"]\nservices:\n  app:\n    <<: *base\n",
            "root",
        );
        ok_strict(
            "x-base: &base\n  image: alpine\n  privileged: true\nservices:\n  app:\n    <<: *base\n    privileged: false\n",
        );
    }

    #[test]
    fn compose_policy_rejects_duplicate_privilege_keys() {
        for yaml in [
            "services:\n  app:\n    image: alpine\n    privileged: true\n    privileged: false\n",
            "services:\n  app:\n    image: alpine\n    privileged: false\n    privileged: true\n",
        ] {
            err_strict(yaml, "failed to parse");
            err_unsafe(yaml, "failed to parse");
        }
    }

    #[test]
    fn strict_rejects_interpolated_privilege_fields() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    privileged: ${PRIVILEGED:-true}\n",
            "privileged",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    network_mode: ${MODE:-host}\n",
            "network_mode",
        );
        err_strict(
            "services:\n  bad:\n    image: alpine\n    cap_add: [\"${CAPABILITY:-SYS_ADMIN}\"]\n",
            "cap_add",
        );
    }

    #[test]
    fn strict_rejects_compose_indirection_and_api_socket() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    use_api_socket: true\n",
            "Docker API socket",
        );
        err_strict(
            "include:\n  - privileged.yml\nservices:\n  app:\n    image: alpine\n",
            "include",
        );
        err_strict(
            "services:\n  app:\n    image: alpine\nsecrets:\n  passwd:\n    file: /etc/passwd\n",
            "secrets.passwd.file",
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_bind_policy_resolves_symlinks_before_allowing_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = allowed.join("escape");
        symlink(&outside, &link).unwrap();

        assert!(!operator_bind_allowed(
            link.to_str().unwrap(),
            allowed.to_str().unwrap()
        ));
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
    fn strict_allows_appdata_bind() {
        // The stable /appdata symlink (#436) is sandbox territory, and
        // — unlike /fs/<X>/ paths — has no mounted-filesystem segment
        // to verify (the symlink target carries that).
        ok_strict(
            "services:\n  ok:\n    image: alpine\n    volumes:\n      - \"/appdata/jellyfin/config:/config\"\n",
        );
    }

    #[test]
    fn strict_rejects_appdata_prefix_lookalike() {
        err_strict(
            "services:\n  bad:\n    image: alpine\n    volumes:\n      - \"/appdata-evil/x:/x\"\n",
            "/appdata-evil/x",
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
    fn unsafe_rejects_unvalidated_compose_indirection() {
        err_unsafe(
            "include:\n  - imported.yml\nservices:\n  app:\n    image: alpine\n",
            "include",
        );
        err_unsafe(
            "services:\n  app:\n    extends:\n      file: imported.yml\n      service: base\n",
            "extends",
        );
        err_unsafe(
            "services:\n  app:\n    image: alpine\n    volumes_from: [base]\n",
            "volumes_from",
        );
    }

    #[test]
    fn unsafe_rejects_protected_host_file_references() {
        err_unsafe(
            "services:\n  app:\n    image: alpine\n    env_file: /var/lib/nasty/auth.json\n",
            "protected host path",
        );
        err_unsafe(
            "services:\n  app:\n    image: alpine\nsecrets:\n  auth:\n    file: /var/lib/nasty/auth.json\n",
            "protected host path",
        );
        err_unsafe(
            "services:\n  app:\n    image: alpine\nconfigs:\n  auth:\n    file: ${AUTH_FILE}\n",
            "interpolated",
        );
    }

    #[test]
    fn dotdot_policy_checks_path_components() {
        assert!(always_forbidden_bind("/fs/tank/archive..old").is_none());
        assert!(matches!(
            always_forbidden_bind("/fs/tank/../private"),
            Some(BindReject::Hard(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn relative_compose_paths_resolve_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, project.join("escape")).unwrap();

        assert_eq!(
            projected_compose_path("./escape/data", project.to_str().unwrap()),
            projected_canonical_path(outside.to_str().unwrap()).join("data")
        );
    }

    #[test]
    fn unsafe_still_rejects_unparseable_yaml() {
        err_unsafe("services: [unbalanced\n", "parse");
    }
}
