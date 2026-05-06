//! QMP (QEMU Machine Protocol) client.
//!
//! QMP is a JSON-based protocol over a Unix socket that QEMU exposes for
//! machine control. Every QMP session starts with a greeting from QEMU,
//! after which the client must send `qmp_capabilities` to enter command mode.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Perform the QMP handshake (read greeting, send qmp_capabilities).
pub async fn negotiate(socket_path: &str) -> Result<(), String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect {socket_path}: {e}"))?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read greeting: {e}"))?;

    // Send qmp_capabilities
    let cmd = json!({"execute": "qmp_capabilities"}).to_string() + "\n";
    writer
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| format!("write qmp_capabilities: {e}"))?;

    // Read response
    line.clear();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read capabilities response: {e}"))?;

    let resp: Value = serde_json::from_str(&line).map_err(|e| format!("parse response: {e}"))?;

    if resp.get("return").is_some() {
        Ok(())
    } else {
        Err(format!("qmp_capabilities failed: {line}"))
    }
}

/// Execute a QMP command and return the result.
pub async fn execute(
    socket_path: &str,
    command: &str,
    arguments: Option<Value>,
) -> Result<Value, String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect {socket_path}: {e}"))?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read greeting: {e}"))?;

    // Send qmp_capabilities
    let cap_cmd = json!({"execute": "qmp_capabilities"}).to_string() + "\n";
    writer
        .write_all(cap_cmd.as_bytes())
        .await
        .map_err(|e| format!("write qmp_capabilities: {e}"))?;

    // Read capabilities response
    line.clear();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read cap response: {e}"))?;

    // Send the actual command
    let mut cmd_obj = json!({"execute": command});
    if let Some(args) = arguments {
        cmd_obj["arguments"] = args;
    }
    let cmd_str = cmd_obj.to_string() + "\n";
    writer
        .write_all(cmd_str.as_bytes())
        .await
        .map_err(|e| format!("write command: {e}"))?;

    // Read response — skip any async events (they have "event" key)
    loop {
        line.clear();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read response: {e}"))?;

        if line.trim().is_empty() {
            return Err("connection closed".to_string());
        }

        let resp: Value = serde_json::from_str(&line).map_err(|e| format!("parse: {e}"))?;

        if resp.get("event").is_some() {
            continue; // Skip async events
        }

        if let Some(ret) = resp.get("return") {
            return Ok(ret.clone());
        }

        if let Some(err) = resp.get("error") {
            return Err(format!("QMP error: {err}"));
        }

        return Err(format!("unexpected QMP response: {line}"));
    }
}

/// Ping the QMP socket — just connect and read greeting.
pub async fn ping(socket_path: &str) -> Result<(), String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (reader, _) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut line),
    )
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(|e| format!("read: {e}"))?;

    if line.contains("QMP") {
        Ok(())
    } else {
        Err(format!("unexpected greeting: {line}"))
    }
}

/// Get the QEMU process PID via QMP.
pub async fn get_pid(socket_path: &str) -> Result<u32, String> {
    // query-status returns process info but not PID directly.
    // Instead, we can get it from the /proc approach by checking who owns the socket.
    // Find the QEMU process that owns this socket via fuser.
    let output = tokio::process::Command::new("fuser")
        .arg(socket_path)
        .output()
        .await
        .map_err(|e| format!("fuser: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // fuser outputs to stderr on some systems
    let combined = format!("{stdout} {stderr}");

    for token in combined.split_whitespace() {
        // Strip trailing characters like 'u' (unix socket)
        let cleaned: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(pid) = cleaned.parse::<u32>() {
            return Ok(pid);
        }
    }

    Err("could not determine PID".to_string())
}
