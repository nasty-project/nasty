use std::sync::Arc;

use axum::extract::{
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::response::IntoResponse;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::AppState;

pub async fn terminal_handler(
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
    ws.on_upgrade(move |socket| handle_terminal(socket, state, client_ip, pre_auth_token))
}

#[derive(Deserialize)]
struct TerminalAuth {
    /// Session token. Optional when the WS upgrade already carried a session
    /// cookie or Bearer header — in that case the first message just supplies
    /// terminal size and (optionally) the command.
    #[serde(default)]
    token: Option<String>,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    /// Optional command to run instead of bash. First element is the executable,
    /// rest are arguments. Session exits when the command finishes.
    cmd: Option<Vec<String>>,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Deserialize)]
struct ControlMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

async fn handle_terminal(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    // Phase 1: authenticate and get terminal size
    let auth = match wait_for_terminal_auth(&mut socket, &state, &client_ip, pre_auth_token).await {
        Some(a) => a,
        None => return,
    };

    info!("Terminal session started for '{}'", auth.username);

    // Phase 2: spawn PTY
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: auth.rows,
        cols: auth.cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to open PTY: {e}");
            let _ = socket
                .send(Message::Text(
                    format!(r#"{{"error":"failed to open terminal: {e}"}}"#).into(),
                ))
                .await;
            return;
        }
    };

    let mut cmd = if let Some(ref argv) = auth.cmd {
        let mut c = CommandBuilder::new(&argv[0]);
        c.args(&argv[1..]);
        c
    } else {
        let mut c = CommandBuilder::new("bash");
        c.args(["--rcfile", "/etc/nasty/terminal-rc"]);
        c
    };
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", "/root");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to spawn shell: {e}");
            let _ = socket
                .send(Message::Text(
                    format!(r#"{{"error":"failed to spawn shell: {e}"}}"#).into(),
                ))
                .await;
            return;
        }
    };

    // Drop slave in parent — only the child uses it
    drop(pair.slave);

    let reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to clone PTY reader: {e}");
            return;
        }
    };

    let writer = pair.master.take_writer().ok();
    if writer.is_none() {
        warn!("Failed to take PTY writer");
        return;
    }
    let writer = Arc::new(std::sync::Mutex::new(writer.unwrap()));
    let master = pair.master;

    // Channel: PTY output → WebSocket
    let (pty_tx, mut pty_rx) = mpsc::channel::<String>(64);

    // Blocking task: read PTY output
    tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if pty_tx.blocking_send(text).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Main loop: shuttle data between WebSocket and PTY
    loop {
        tokio::select! {
            // PTY output → WebSocket
            Some(output) = pty_rx.recv() => {
                if socket.send(Message::Text(output.into())).await.is_err() {
                    break;
                }
            }
            // WebSocket input → PTY
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Check if it's a control message (resize)
                        if let Ok(ctrl) = serde_json::from_str::<ControlMessage>(&text)
                            && ctrl.msg_type == "resize" {
                                let _ = master.resize(PtySize {
                                    rows: ctrl.rows,
                                    cols: ctrl.cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                                continue;
                            }
                        // Regular input — write to PTY
                        let writer = writer.clone();
                        let data: Vec<u8> = text.bytes().collect();
                        let _ = tokio::task::spawn_blocking(move || {
                            let mut w = writer.lock().unwrap();
                            std::io::Write::write_all(&mut *w, &data)
                        }).await;
                    }
                    Some(Ok(Message::Binary(data))) => {
                        let writer = writer.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let mut w = writer.lock().unwrap();
                            std::io::Write::write_all(&mut *w, &data)
                        }).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    // Cleanup
    info!("Terminal session ended for '{}'", auth.username);
    let _ = child.kill();
    let _ = child.wait();
}

struct TerminalAuthResult {
    username: String,
    cols: u16,
    rows: u16,
    cmd: Option<Vec<String>>,
}

async fn wait_for_terminal_auth(
    socket: &mut WebSocket,
    state: &AppState,
    client_ip: &str,
    pre_auth_token: Option<String>,
) -> Option<TerminalAuthResult> {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv())
        .await
        .ok()??
        .ok()?;

    let text = match msg {
        Message::Text(t) => t,
        _ => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"expected JSON auth message"}"#.into(),
                ))
                .await;
            return None;
        }
    };

    let auth: TerminalAuth = match serde_json::from_str(&text) {
        Ok(a) => a,
        Err(_) => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"expected {\"token\":\"...\",\"cols\":80,\"rows\":24}"}"#.into(),
                ))
                .await;
            return None;
        }
    };

    // Cookie-based browser flow → token is on the upgrade request.
    // Non-browser CLI flow → token is in the first message.
    let token = match pre_auth_token.or(auth.token) {
        Some(t) => t,
        None => {
            let _ = socket
                .send(Message::Text(r#"{"error":"missing session"}"#.into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return None;
        }
    };

    match state.auth.validate(&token, client_ip).await {
        Ok(session) if session.role == crate::auth::Role::Admin => {
            // Opening an interactive root shell on the host is the
            // single highest-stakes admin action — record every open,
            // not just denials, so the audit trail can answer "who had
            // a shell at 03:42?" later.
            crate::auth::audit(
                "terminal_opened",
                &session.username,
                client_ip,
                &auth
                    .cmd
                    .as_deref()
                    .map(|c| format!("cmd={}", c.join(" ")))
                    .unwrap_or_default(),
            );
            let _ = socket
                .send(Message::Text(r#"{"authenticated":true}"#.into()))
                .await;
            Some(TerminalAuthResult {
                username: session.username,
                cols: auth.cols,
                rows: auth.rows,
                cmd: auth.cmd,
            })
        }
        Ok(session) => {
            warn!(
                "Terminal access denied for '{}': role={:?}",
                session.username, session.role
            );
            crate::auth::audit(
                "terminal_denied",
                &session.username,
                client_ip,
                &format!("role={:?}", session.role),
            );
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"forbidden: admin role required"}"#.into(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            None
        }
        Err(_) => {
            let _ = socket
                .send(Message::Text(r#"{"error":"invalid token"}"#.into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            None
        }
    }
}
