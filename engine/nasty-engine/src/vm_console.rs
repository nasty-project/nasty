use std::sync::Arc;

use axum::extract::{
    Path,
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{info, warn};

use crate::AppState;

const QMP_DIR: &str = "/run/nasty/vm";

/// WebSocket handler for VNC console (binary frames → VNC unix socket).
/// Used by noVNC in the browser. Auth is via the session cookie (browser)
/// or Authorization: Bearer header (CLI). The legacy `?token=` query
/// fallback was removed — cookies cover same-origin noVNC connections.
pub async fn vnc_handler(
    ws: WebSocketUpgrade,
    Path(vm_id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let client_ip = extract_client_ip(&headers);
    let pre_auth_token = crate::token_from_headers(&headers);
    ws.on_upgrade(move |socket| proxy_unix_socket(socket, format!("{QMP_DIR}/{vm_id}.vnc"), "vnc", vm_id, state, client_ip, pre_auth_token))
}

/// WebSocket handler for serial console (text frames → serial unix socket).
pub async fn serial_handler(
    ws: WebSocketUpgrade,
    Path(vm_id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let client_ip = extract_client_ip(&headers);
    let pre_auth_token = crate::token_from_headers(&headers);
    ws.on_upgrade(move |socket| proxy_unix_socket(socket, format!("{QMP_DIR}/{vm_id}.serial"), "serial", vm_id, state, client_ip, pre_auth_token))
}

fn extract_client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// Bidirectional proxy: WebSocket ↔ Unix socket.
///
/// For VNC: binary frames are forwarded as-is (noVNC speaks raw RFB).
/// For serial: text frames are forwarded as bytes.
async fn proxy_unix_socket(
    mut ws: WebSocket,
    socket_path: String,
    console_type: &str,
    vm_id: String,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    // Authenticate: either via query param (noVNC) or first WebSocket message (serial)
    let token = if let Some(t) = pre_auth_token {
        t
    } else {
        match ws.recv().await {
            Some(Ok(Message::Text(text))) => {
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(&text);
                match parsed {
                    Ok(v) => v.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                    Err(_) => text.to_string(),
                }
            }
            _ => return,
        }
    };

    // Admin or Operator — VM console is interactive root inside the guest.
    match state.auth.validate(&token, &client_ip).await {
        Ok(s) if s.role == crate::auth::Role::Admin || s.role == crate::auth::Role::Operator => {}
        Ok(s) => {
            crate::auth::audit("vm_console_denied", &s.username, &client_ip, &format!("role={:?} vm={}", s.role, vm_id));
            let _ = ws.send(Message::Text("forbidden".into())).await;
            return;
        }
        Err(_) => {
            let _ = ws.send(Message::Text("unauthorized".into())).await;
            return;
        }
    }

    // Verify VM exists
    if state.vms.get(&vm_id).await.is_err() {
        let _ = ws.send(Message::Text("VM not found".into())).await;
        return;
    }

    // Connect to the unix socket
    let unix = match UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("failed to connect to {console_type} console: {e}");
            warn!("{msg}");
            let _ = ws.send(Message::Text(msg.into())).await;
            return;
        }
    };

    info!("VM '{vm_id}' {console_type} console connected");

    let (unix_read, mut unix_write) = unix.into_split();
    let (mut ws_sender, mut ws_receiver) = ws.split();

    // Unix socket → WebSocket
    let vm_id_clone = vm_id.clone();
    let ct = console_type.to_string();
    let unix_to_ws = tokio::spawn(async move {
        let mut reader = unix_read;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let msg = Message::Binary(buf[..n].to_vec().into());
                    if ws_sender.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        info!("VM '{vm_id_clone}' {ct} console: unix socket closed");
    });

    // WebSocket → Unix socket
    let vm_id_clone2 = vm_id.clone();
    let ct2 = console_type.to_string();
    let ws_to_unix = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if unix_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Text(text)) => {
                    if unix_write.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        info!("VM '{vm_id_clone2}' {ct2} console: websocket closed");
    });

    // Wait for either direction to finish
    tokio::select! {
        _ = unix_to_ws => {}
        _ = ws_to_unix => {}
    }

    info!("VM '{vm_id}' {console_type} console disconnected");
}
