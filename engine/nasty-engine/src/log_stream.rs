//! WebSocket endpoint for streaming journalctl output (follow mode).

use std::sync::Arc;

use axum::extract::{
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::info;

use crate::AppState;

pub async fn logs_handler(
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
    ws.on_upgrade(move |socket| handle_logs(socket, state, client_ip, pre_auth_token))
}

#[derive(Deserialize)]
struct LogRequest {
    /// Optional when the WS upgrade carried a session cookie or Bearer token.
    #[serde(default)]
    token: Option<String>,
    unit: String,
    #[serde(default = "default_lines")]
    lines: u32,
    #[serde(default)]
    grep: Option<String>,
}

fn default_lines() -> u32 {
    100
}

#[derive(Serialize)]
struct LogMessage {
    #[serde(rename = "type")]
    msg_type: String,
    data: String,
}

impl LogMessage {
    fn line(s: &str) -> String {
        serde_json::to_string(&Self {
            msg_type: "line".into(),
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
}

async fn handle_logs(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    // First message: params (token optional now — cookie may have provided it)
    let req: LogRequest = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(
                        LogMessage::error(&format!("invalid request: {e}")).into(),
                    ))
                    .await;
                return;
            }
        },
        _ => return,
    };

    let token = match pre_auth_token.or_else(|| req.token.clone()) {
        Some(t) => t,
        None => {
            let _ = socket
                .send(Message::Text(LogMessage::error("missing session").into()))
                .await;
            return;
        }
    };

    // Admin only — system journals can leak secrets, IPs, and audit detail.
    match state.auth.validate(&token, &client_ip).await {
        Ok(s) if s.role == crate::auth::Role::Admin => {
            // Comment above says journals can leak secrets / IPs /
            // audit detail — so a live stream of them is sensitive and
            // every open deserves an audit entry, not just denials.
            crate::auth::audit(
                "log_stream_opened",
                &s.username,
                &client_ip,
                "",
            );
        }
        Ok(s) => {
            crate::auth::audit(
                "log_stream_denied",
                &s.username,
                &client_ip,
                &format!("role={:?}", s.role),
            );
            let _ = socket
                .send(Message::Text(
                    LogMessage::error("forbidden: admin role required").into(),
                ))
                .await;
            return;
        }
        Err(_) => {
            let _ = socket
                .send(Message::Text(LogMessage::error("invalid token").into()))
                .await;
            return;
        }
    }

    info!("Log stream started for unit '{}' (follow mode)", req.unit);

    // Build journalctl command
    let mut args = vec![
        "-u".to_string(),
        req.unit.clone(),
        "-n".to_string(),
        req.lines.to_string(),
        "-f".to_string(),
        "--no-pager".to_string(),
        "--output".to_string(),
        "short-iso".to_string(),
    ];
    if let Some(ref grep) = req.grep
        && !grep.is_empty()
    {
        args.push("--grep".to_string());
        args.push(grep.clone());
    }

    let mut child = match tokio::process::Command::new("journalctl")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    LogMessage::error(&format!("spawn journalctl: {e}")).into(),
                ))
                .await;
            return;
        }
    };

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    // Stream lines until client disconnects or process exits
    loop {
        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        if socket.send(Message::Text(LogMessage::line(&text).into())).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Ok(None) => break, // journalctl exited
                    Err(_) => break,
                }
            }
            // Check if client sent a close or any message (to detect disconnect)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore other messages
                }
            }
        }
    }

    // Clean up
    let _ = child.kill().await;
    info!("Log stream ended for unit '{}'", req.unit);
}
