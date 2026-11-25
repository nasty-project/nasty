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
    let client_ip = crate::client_ip(&headers);
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
    /// Persistent shell to attach (1..=MAX_SHELLS), started if not running.
    /// Without it (and without `cmd`) the shell ends with the connection.
    #[serde(default)]
    shell: Option<u8>,
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

    let persistent = match (auth.cmd.is_none(), auth.shell) {
        (true, Some(n)) => match ensure_shell(&auth.username, n).await {
            Ok(socket_path) => Some(socket_path),
            Err(e) => {
                warn!("Failed to start shell {n} for '{}': {e}", auth.username);
                // Already authenticated: the WebUI prints this as terminal text.
                let _ = socket
                    .send(Message::Text(
                        format!("\r\n\x1b[31mFailed to start shell {n}: {e}\x1b[0m\r\n").into(),
                    ))
                    .await;
                return;
            }
        },
        _ => None,
    };

    // Terminal sessions can launch arbitrary sustained work. Demote the shell
    // before exec so those processes cannot inherit the engine's priority.
    let mut cmd = CommandBuilder::new("bash");
    cmd.args([
        "-c",
        "env -u POSIXLY_CORRECT renice -n 10 -p $$ >/dev/null && \
         ionice -c2 -n7 -p $$ && exec \"$@\"",
        "nasty-terminal",
    ]);
    if let Some(ref argv) = auth.cmd {
        cmd.args(argv);
    } else if let Some(ref socket_path) = persistent {
        // Only this tmux client belongs to the connection; the shell lives
        // in its own unit and outlives it.
        cmd.args([
            "tmux",
            "-S",
            socket_path.as_str(),
            "attach-session",
            "-t",
            "main",
        ]);
    } else {
        cmd.args(["bash", "--rcfile", "/etc/nasty/terminal-rc"]);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", "/root");
    // Marker for any in-PTY script to detect "this shell is hosted
    // by the engine; if you restart the engine, this shell dies and
    // takes me with it." Scripts that do `nixos-rebuild switch`
    // (nasty-sync in particular) check for this and detach via
    // systemd-run so the rebuild outlives the terminal session.
    cmd.env("NASTY_WEBUI_TERMINAL", "1");

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
    shell: Option<u8>,
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
        Ok(session) => {
            if let Err(reason) = crate::auth::authorize_session(
                &session,
                crate::auth::EndpointAccess::RootEquivalent,
            ) {
                warn!(
                    "Terminal access denied for '{}': {reason:?}",
                    session.username
                );
                crate::auth::audit(
                    "terminal_denied",
                    &session.username,
                    client_ip,
                    &format!("reason={reason:?}"),
                );
                let _ = socket
                    .send(Message::Text(
                        format!("{{\"error\":\"{}\"}}", reason.message()).into(),
                    ))
                    .await;
                let _ = socket.send(Message::Close(None)).await;
                return None;
            }
            // Opening an interactive root shell on the host is the
            // single highest-stakes admin action — record every open,
            // not just denials, so the audit trail can answer "who had
            // a shell at 03:42?" later.
            crate::auth::audit(
                "terminal_opened",
                &session.username,
                client_ip,
                &match (&auth.cmd, auth.shell) {
                    (Some(c), _) => format!("cmd={}", c.join(" ")),
                    (None, Some(n)) => format!("shell={n}"),
                    (None, None) => String::new(),
                },
            );
            let _ = socket
                .send(Message::Text(r#"{"authenticated":true}"#.into()))
                .await;
            Some(TerminalAuthResult {
                username: session.username,
                cols: auth.cols,
                rows: auth.rows,
                cmd: auth.cmd,
                shell: auth.shell,
            })
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

// ── Persistent shells (#1085) ──────────────────────────────────────────
//
// Each shell is a tmux server in its own transient unit, so leaving the page,
// the login expiring and engine restarts only end the attached client. `exit`,
// "Close" (systemctl stop) or a reboot end the shell.

/// Shells per user.
pub const MAX_SHELLS: u8 = 4;
const SHELL_SOCKET_DIR: &str = "/run/nasty-shells";
const SHELL_START_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Usernames may hold characters unit names and paths can't, so key by hex.
fn shell_key(username: &str) -> String {
    username.bytes().map(|b| format!("{b:02x}")).collect()
}

fn shell_unit(username: &str, n: u8) -> String {
    format!("nasty-shell-{}-{n}.service", shell_key(username))
}

fn shell_socket(username: &str, n: u8) -> String {
    format!("{SHELL_SOCKET_DIR}/{}-{n}", shell_key(username))
}

fn check_shell_number(n: u8) -> Result<(), String> {
    if (1..=MAX_SHELLS).contains(&n) {
        Ok(())
    } else {
        Err(format!("shell must be 1..={MAX_SHELLS}"))
    }
}

async fn shell_running(socket_path: &str) -> bool {
    tokio::process::Command::new("tmux")
        .args(["-S", socket_path, "has-session", "-t", "main"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Start shell `n` for `username` unless it runs; returns its tmux socket.
async fn ensure_shell(username: &str, n: u8) -> Result<String, String> {
    check_shell_number(n)?;
    let socket_path = shell_socket(username, n);
    if shell_running(&socket_path).await {
        return Ok(socket_path);
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(SHELL_SOCKET_DIR)
            .map_err(|e| format!("{SHELL_SOCKET_DIR}: {e}"))?;
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let output = tokio::process::Command::new("systemd-run")
        .arg("--unit")
        .arg(shell_unit(username, n))
        .args([
            "--collect",
            "--description",
            &format!("NASty shell {n} for {username}"),
            "--property=Type=forking",
            // Same demotion as a one-off terminal shell.
            "--property=Nice=10",
            "--property=IOSchedulingClass=best-effort",
            "--property=IOSchedulingPriority=7",
            "--working-directory=/root",
            "--setenv=TERM=xterm-256color",
            "--setenv=HOME=/root",
            "--setenv=NASTY_WEBUI_TERMINAL=1",
        ])
        .arg(format!("--setenv=NASTY_SHELL={n}"))
        .arg(format!("--setenv=PATH={path}"))
        .args([
            "--",
            "tmux",
            "-S",
            &socket_path,
            "-f",
            "/etc/nasty/tmux.conf",
        ])
        .args(["new-session", "-d", "-s", "main"])
        .args(["bash", "--rcfile", "/etc/nasty/terminal-rc"])
        .output()
        .await
        .map_err(|e| format!("systemd-run: {e}"))?;
    // A second tab racing this one gets "unit already exists"; both then wait
    // for the same session.
    let deadline = tokio::time::Instant::now() + SHELL_START_WAIT;
    while tokio::time::Instant::now() < deadline {
        if shell_running(&socket_path).await {
            return Ok(socket_path);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        "tmux did not start".into()
    } else {
        stderr
    })
}

/// Running shell numbers, from `systemctl list-units --plain --no-legend`.
fn parse_running_shells(output: &str, username: &str) -> Vec<u8> {
    let prefix = format!("nasty-shell-{}-", shell_key(username));
    let mut shells: Vec<u8> = output
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let unit = cols.next()?;
            let active = cols.nth(1)? == "active";
            let n = unit
                .strip_prefix(&prefix)?
                .strip_suffix(".service")?
                .parse()
                .ok()?;
            (active && check_shell_number(n).is_ok()).then_some(n)
        })
        .collect();
    shells.sort_unstable();
    shells
}

async fn running_shells(username: &str) -> Result<Vec<u8>, String> {
    let output = tokio::process::Command::new("systemctl")
        .args(["list-units", "--plain", "--no-legend", "--type=service"])
        .arg(format!("nasty-shell-{}-*", shell_key(username)))
        .output()
        .await
        .map_err(|e| format!("systemctl: {e}"))?;
    Ok(parse_running_shells(
        &String::from_utf8_lossy(&output.stdout),
        username,
    ))
}

/// GET /api/terminal/shells: the caller's running shells.
pub async fn shells_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    let authenticated = match crate::validate_bearer(
        &headers,
        &state.auth,
        crate::auth::EndpointAccess::RootEquivalent,
        "terminal.shells",
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    match running_shells(&authenticated.session.username).await {
        Ok(shells) => {
            axum::Json(serde_json::json!({ "shells": shells, "max": MAX_SHELLS })).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

/// DELETE /api/terminal/shells/{n}: end one of the caller's shells.
pub async fn close_shell_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Path(n): axum::extract::Path<u8>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let authenticated = match crate::validate_bearer(
        &headers,
        &state.auth,
        crate::auth::EndpointAccess::RootEquivalent,
        "terminal.shells.close",
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    let error = |status, e: String| (status, axum::Json(serde_json::json!({ "error": e })));
    if let Err(e) = check_shell_number(n) {
        return error(StatusCode::BAD_REQUEST, e).into_response();
    }
    let username = &authenticated.session.username;
    crate::auth::audit(
        "terminal_shell_closed",
        username,
        &authenticated.client_ip,
        &format!("shell={n}"),
    );
    match tokio::process::Command::new("systemctl")
        .args(["stop", &shell_unit(username, n)])
        .output()
        .await
    {
        Ok(o) if o.status.success() => StatusCode::NO_CONTENT.into_response(),
        Ok(o) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )
        .into_response(),
        Err(e) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, format!("systemctl: {e}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_names_are_safe_for_any_username() {
        assert_eq!(shell_unit("admin", 2), "nasty-shell-61646d696e-2.service");
        assert_eq!(
            shell_socket("a b/..", 1),
            "/run/nasty-shells/6120622f2e2e-1"
        );
        assert!(check_shell_number(0).is_err());
        assert!(check_shell_number(MAX_SHELLS + 1).is_err());
    }

    #[test]
    fn running_shells_only_lists_this_users_active_shells() {
        let admin = shell_key("admin");
        let other = shell_key("bob");
        let out = format!(
            "nasty-shell-{admin}-3.service loaded active running NASty shell 3\n\
             nasty-shell-{admin}-1.service loaded active running NASty shell 1\n\
             nasty-shell-{admin}-2.service loaded failed failed NASty shell 2\n\
             nasty-shell-{admin}-9.service loaded active running stray\n\
             nasty-shell-{other}-4.service loaded active running NASty shell 4\n"
        );
        assert_eq!(parse_running_shells(&out, "admin"), [1, 3]);
    }
}
