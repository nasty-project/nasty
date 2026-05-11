//! Import a VM image (qcow2/img/raw) onto a block subvolume by streaming
//! `qemu-img convert` over a WebSocket.
//!
//! Mirrors `app_deploy.rs`: the first WS message carries auth + params,
//! subsequent server frames are `{ type, data }` log/error/done plus a
//! `{ type: "progress", percent }` shape parsed off `qemu-img convert -p`.
//!
//! Why this exists: bootable distros that ship as qcow2 (Home Assistant
//! HAOS, OPNsense, CoreOS) can't be used as a "boot image" the way an ISO
//! installer can — they're already an installed disk. The user needs to
//! lay them down on a block subvolume and attach the subvolume as a VM
//! disk. Doing that by hand requires shelling into the appliance and
//! running qemu-img. This endpoint makes it a UI affordance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::AppState;
use crate::router::VM_IMAGE_EXTENSIONS;

pub async fn disk_import_handler(
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
    ws.on_upgrade(move |socket| handle_import(socket, state, client_ip, pre_auth_token))
}

#[derive(Deserialize)]
struct ImportRequest {
    #[serde(default)]
    token: Option<String>,
    /// Filesystem on which the source image lives (it's resolved to
    /// `<mount>/vms/images/<image_name>`, the same layout `vm.image.list`
    /// returns).
    image_filesystem: String,
    image_name: String,
    /// Block subvolume that receives the converted image. Must already
    /// exist and have its loop device attached (so a `block_device` is
    /// present). The UI nudges users through `subvolume.create` when no
    /// suitable target exists.
    target_filesystem: String,
    target_subvolume: String,
}

#[derive(Serialize)]
struct ImportMessage {
    #[serde(rename = "type")]
    msg_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    percent: Option<f64>,
}

impl ImportMessage {
    fn log(s: impl Into<String>) -> String {
        serde_json::to_string(&Self {
            msg_type: "log",
            data: Some(s.into()),
            percent: None,
        })
        .unwrap()
    }
    fn error(s: impl Into<String>) -> String {
        serde_json::to_string(&Self {
            msg_type: "error",
            data: Some(s.into()),
            percent: None,
        })
        .unwrap()
    }
    fn progress(pct: f64) -> String {
        serde_json::to_string(&Self {
            msg_type: "progress",
            data: None,
            percent: Some(pct),
        })
        .unwrap()
    }
    fn done(s: impl Into<String>) -> String {
        serde_json::to_string(&Self {
            msg_type: "done",
            data: Some(s.into()),
            percent: None,
        })
        .unwrap()
    }
}

async fn send_text(socket: &mut WebSocket, text: String) {
    let _ = socket.send(Message::Text(text.into())).await;
}

async fn handle_import(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    let req: ImportRequest = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                send_text(
                    &mut socket,
                    ImportMessage::error(format!("invalid request: {e}")),
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
            send_text(&mut socket, ImportMessage::error("missing session")).await;
            return;
        }
    };

    // Admin-only: this writes to a block device and runs a long-lived
    // qemu-img process — the same risk profile as compose deploys.
    let session = match state.auth.validate(&token, &client_ip).await {
        Ok(s) if s.role == crate::auth::Role::Admin => s,
        Ok(s) => {
            crate::auth::audit(
                "vm_disk_import_denied",
                &s.username,
                &client_ip,
                &format!("role={:?}", s.role),
            );
            send_text(
                &mut socket,
                ImportMessage::error("forbidden: admin role required"),
            )
            .await;
            return;
        }
        Err(_) => {
            send_text(&mut socket, ImportMessage::error("invalid token")).await;
            return;
        }
    };

    info!(
        "VM disk import started: image={}:{} target={}:{} user={}",
        req.image_filesystem,
        req.image_name,
        req.target_filesystem,
        req.target_subvolume,
        session.username,
    );

    let image_path = match resolve_image_path(&state, &req.image_filesystem, &req.image_name).await
    {
        Ok(p) => p,
        Err(e) => {
            send_text(&mut socket, ImportMessage::error(e)).await;
            return;
        }
    };

    let target =
        match resolve_target_block_device(&state, &req.target_filesystem, &req.target_subvolume)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                send_text(&mut socket, ImportMessage::error(e)).await;
                return;
            }
        };

    // Read image metadata up front so we can refuse early when the
    // virtual size doesn't fit the target — the alternative is letting
    // qemu-img run for several minutes and then fail with a write error,
    // which gives the user no useful information.
    let info = match read_image_info(&image_path).await {
        Ok(i) => i,
        Err(e) => {
            send_text(&mut socket, ImportMessage::error(e)).await;
            return;
        }
    };

    send_text(
        &mut socket,
        ImportMessage::log(format!(
            "Source: {} ({}, virtual {} bytes)",
            image_path.display(),
            info.format,
            info.virtual_size,
        )),
    )
    .await;

    if let Some(vs) = target.volsize_bytes
        && info.virtual_size > vs
    {
        send_text(
            &mut socket,
            ImportMessage::error(format!(
                "image is too large: virtual size {} bytes > target subvolume {} bytes. \
                 Recreate the subvolume with at least {} bytes (e.g. {} GiB).",
                info.virtual_size,
                vs,
                info.virtual_size,
                // Round up to whole GiB so the UI's hint is copyable.
                info.virtual_size.div_ceil(1024 * 1024 * 1024)
            )),
        )
        .await;
        return;
    }

    send_text(
        &mut socket,
        ImportMessage::log(format!(
            "Target: subvolume '{}' on '{}' → {}",
            target.name, target.filesystem, target.block_device
        )),
    )
    .await;

    crate::auth::audit(
        "vm.disk.import",
        &session.username,
        &client_ip,
        &format!(
            "image={}:{} target={}:{} dev={}",
            req.image_filesystem,
            req.image_name,
            req.target_filesystem,
            req.target_subvolume,
            target.block_device,
        ),
    );

    if let Err(e) = run_convert(&mut socket, &image_path, &target.block_device).await {
        send_text(
            &mut socket,
            ImportMessage::error(format!("qemu-img convert failed: {e}")),
        )
        .await;
        return;
    }

    send_text(&mut socket, ImportMessage::progress(100.0)).await;
    send_text(&mut socket, ImportMessage::done("ok")).await;
    info!(
        "VM disk import finished: image={}:{} target={}",
        req.image_filesystem, req.image_name, target.block_device,
    );
}

pub(crate) struct ResolvedTarget {
    pub name: String,
    pub filesystem: String,
    pub block_device: String,
    pub volsize_bytes: Option<u64>,
}

pub(crate) async fn resolve_target_block_device(
    state: &AppState,
    filesystem: &str,
    subvolume: &str,
) -> Result<ResolvedTarget, String> {
    let sv = state
        .subvolumes
        .get(filesystem, subvolume, None)
        .await
        .map_err(|e| format!("target subvolume not found: {e}"))?;

    if sv.subvolume_type != nasty_storage::subvolume::SubvolumeType::Block {
        return Err(format!(
            "target subvolume '{}' on '{}' is a filesystem subvolume — only block subvolumes can receive a disk image",
            sv.name, sv.filesystem
        ));
    }

    let block_device = sv
        .block_device
        .clone()
        .ok_or_else(|| {
            format!(
                "target subvolume '{}' on '{}' has no loop device attached. Attach it first via subvolume.attach.",
                sv.name, sv.filesystem,
            )
        })?;

    // Refuse if anything else is already exporting this device — writing
    // through it while iSCSI/NVMe-oF clients are connected would silently
    // corrupt them.
    if let Some(why) = crate::router::check_block_device_conflict(state, &block_device, "vm").await
    {
        return Err(format!("target busy: {why}"));
    }
    if let Ok(vms) = state.vms.list().await {
        for vm in &vms {
            for disk in &vm.config.disks {
                if disk.path == block_device && vm.running {
                    return Err(format!(
                        "target busy: device {} is attached to running VM '{}'. Stop the VM first.",
                        block_device, vm.config.name
                    ));
                }
            }
        }
    }

    Ok(ResolvedTarget {
        name: sv.name,
        filesystem: sv.filesystem,
        block_device,
        volsize_bytes: sv.volsize_bytes,
    })
}

/// Resolve `(filesystem, image_name)` to an absolute path under
/// `<mount>/vms/images/`. Rejects path traversal and unsupported
/// extensions before we hand the path to qemu-img.
pub(crate) async fn resolve_image_path(
    state: &AppState,
    filesystem: &str,
    image_name: &str,
) -> Result<PathBuf, String> {
    // file_name() strips directory components and rejects ".." / "/".
    let safe_name = Path::new(image_name)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid image name: '{image_name}'"))?;
    if safe_name != image_name {
        return Err(format!(
            "invalid image name: '{image_name}' (must not contain path separators)"
        ));
    }

    let ext = Path::new(safe_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !VM_IMAGE_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(&ext))
    {
        return Err(format!(
            "unsupported image extension '.{ext}' — expected one of {VM_IMAGE_EXTENSIONS:?}"
        ));
    }
    // ISOs are bootable installers, not pre-installed disk images — converting
    // one onto a block subvolume produces a non-bootable mess. Block the path
    // here so the UI's affordance can't trip on it.
    if ext == "iso" {
        return Err(
            "ISO images are installer media, not disk images — boot the VM from the ISO instead of importing it"
                .to_string(),
        );
    }

    let fs = state
        .filesystems
        .get(filesystem)
        .await
        .map_err(|e| format!("filesystem '{filesystem}' not found: {e}"))?;
    let mp = fs
        .mount_point
        .ok_or_else(|| format!("filesystem '{filesystem}' is not mounted"))?;
    let candidate = Path::new(&mp).join("vms").join("images").join(safe_name);
    if !candidate.is_file() {
        return Err(format!(
            "image '{safe_name}' not found at {}",
            candidate.display()
        ));
    }
    Ok(candidate)
}

#[derive(Debug, Serialize, Clone)]
pub struct ImageInfo {
    pub format: String,
    pub virtual_size: u64,
    pub actual_size: u64,
}

pub(crate) async fn read_image_info(path: &Path) -> Result<ImageInfo, String> {
    let out = Command::new("qemu-img")
        .args(["info", "--output=json", "--force-share"])
        .arg(path)
        .output()
        .await
        .map_err(|e| format!("failed to spawn qemu-img: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "qemu-img info failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("failed to parse qemu-img info: {e}"))?;
    let format = json
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let virtual_size = json
        .get("virtual-size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "qemu-img info: missing virtual-size".to_string())?;
    let actual_size = json
        .get("actual-size")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Ok(ImageInfo {
        format,
        virtual_size,
        actual_size,
    })
}

/// Spawn `qemu-img convert -p` and forward progress to the socket. The
/// `-p` flag writes lines like `    (12.34/100%)` to stdout, separated
/// by `\r` (in-place updates) — we read raw bytes and split on either
/// `\r` or `\n` so each progress tick gets surfaced.
async fn run_convert(socket: &mut WebSocket, input: &Path, output_dev: &str) -> Result<(), String> {
    let mut child = Command::new("qemu-img")
        .args(["convert", "-p", "-O", "raw"])
        .arg(input)
        .arg(output_dev)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn qemu-img: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "qemu-img stdout missing".to_string())?;
    let stderr = child.stderr.take();

    // Stderr drains in the background — qemu-img writes warnings here
    // and a stuck reader can backpressure the child.
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    });

    let mut reader = stdout;
    let mut buf = [0u8; 256];
    let mut line = String::new();
    let mut last_pct: Option<f64> = None;
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| format!("qemu-img stdout read: {e}"))?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                if !line.is_empty() {
                    if let Some(p) = parse_progress(&line) {
                        // Throttle: forward whole percent ticks (and the
                        // final 100). qemu-img emits 0.50, 1.00, 1.50…
                        // — without throttling we'd flood the socket
                        // with hundreds of frames per second.
                        let send = match last_pct {
                            None => true,
                            Some(prev) => p - prev >= 1.0 || (p >= 100.0 && prev < 100.0),
                        };
                        if send {
                            send_text(socket, ImportMessage::progress(p)).await;
                            last_pct = Some(p);
                        }
                    }
                    line.clear();
                }
            } else {
                line.push(b as char);
            }
        }
    }

    // Flush any trailing partial line (qemu-img may not terminate the
    // last update on success).
    if !line.is_empty()
        && let Some(p) = parse_progress(&line)
    {
        send_text(socket, ImportMessage::progress(p)).await;
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("qemu-img wait: {e}"))?;
    let stderr_buf = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr_buf);
        let detail = stderr_text.lines().last().unwrap_or("").trim();
        if detail.is_empty() {
            warn!("qemu-img convert exited {status}");
            return Err(format!("exit {status}"));
        }
        warn!("qemu-img convert exited {status}: {detail}");
        return Err(format!("exit {status}: {detail}"));
    }
    Ok(())
}

/// Extract `12.34` from a line like `    (12.34/100%)`. Returns None for
/// any other shape (qemu-img occasionally emits informational warnings
/// on stdout that don't carry a percent).
fn parse_progress(line: &str) -> Option<f64> {
    let trimmed = line.trim();
    let inside = trimmed.strip_prefix('(')?.strip_suffix(')')?;
    let pct = inside.strip_suffix("/100%")?;
    pct.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_progress;

    #[test]
    fn parses_qemu_img_progress() {
        assert_eq!(parse_progress("    (0.50/100%)"), Some(0.5));
        assert_eq!(parse_progress("(12.34/100%)"), Some(12.34));
        assert_eq!(parse_progress(" (100.00/100%) "), Some(100.0));
    }

    #[test]
    fn rejects_other_lines() {
        assert!(parse_progress("Warning: ...").is_none());
        assert!(parse_progress("").is_none());
        assert!(parse_progress("(abc/100%)").is_none());
        assert!(parse_progress("(50/99%)").is_none());
    }
}
