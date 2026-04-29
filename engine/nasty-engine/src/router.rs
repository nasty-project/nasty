use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;
use tracing::debug;

use crate::AppState;
use crate::auth::{Role, Session};

/// Methods an operator token is allowed to call (in addition to all read-only methods)
fn is_operator_allowed(method: &str) -> bool {
    is_read_only(method)
        || matches!(
            method,
            "subvolume.create"
                | "subvolume.delete"
                | "subvolume.attach"
                | "subvolume.detach"
                | "subvolume.resize"
                | "subvolume.update"
                | "subvolume.clone"
                | "subvolume.set_properties"
                | "subvolume.remove_properties"
                | "snapshot.create"
                | "snapshot.delete"
                | "snapshot.clone"
                | "share.nfs.create"
                | "share.nfs.update"
                | "share.nfs.delete"
                | "share.smb.create"
                | "share.smb.update"
                | "share.smb.delete"
                | "smb.user.create"
                | "smb.user.delete"
                | "smb.user.set_password"
                | "smb.group.delete"
                | "share.iscsi.create"
                | "share.iscsi.delete"
                | "share.iscsi.add_lun"
                | "share.iscsi.remove_lun"
                | "share.iscsi.add_acl"
                | "share.iscsi.remove_acl"
                | "share.nvmeof.create"
                | "share.nvmeof.delete"
                | "share.nvmeof.add_namespace"
                | "share.nvmeof.remove_namespace"
                | "share.nvmeof.add_port"
                | "share.nvmeof.remove_port"
                | "share.nvmeof.add_host"
                | "share.nvmeof.remove_host"
                | "vm.create"
                | "vm.update"
                | "vm.start"
                | "vm.stop"
                | "vm.kill"
                | "vm.snapshot"
                | "vm.clone"
                | "apps.enable"
                | "apps.install"
                | "apps.update"
                | "apps.remove"
                | "apps.stop"
                | "apps.start"
                | "apps.restart"
                | "apps.pull"
                | "apps.prune"
                | "apps.compose.install"
                | "apps.compose.update"
                | "apps.compose.remove"
                | "apps.ingress.set"
                | "apps.ingress.remove"
                | "firmware.update"
        )
}

/// Extract a string param from JSON-RPC params
fn str_param<'a>(request: &'a Request, key: &str) -> Option<&'a str> {
    request
        .params
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
}

/// Parse typed params from JSON-RPC request
fn parse_params<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, String> {
    request
        .params
        .as_ref()
        .ok_or_else(|| "missing params".to_string())
        .and_then(|p| serde_json::from_value(p.clone()).map_err(|e| e.to_string()))
}

/// Check if a method is read-only (safe for ReadOnly role)
fn is_read_only(method: &str) -> bool {
    method.ends_with(".list")
        || method.ends_with(".get")
        || matches!(
            method,
            "system.info"
                | "system.health"
                | "system.stats"
                | "system.disks"
                | "system.network.get"
                | "system.logs"
                | "system.logs.units"
                | "system.ssh.status"
                | "system.alerts"
                | "system.settings.get"
                | "system.tuning.get"
                | "system.nut.config.get"
                | "system.nut.status"
                | "system.tailscale.get"
                | "system.acme.status"
                | "system.metrics.history"
                | "system.metrics.prometheus"
                | "alert.rules.list"
                | "device.list"
                | "auth.me"
                | "auth.list_users"
                | "auth.token.list"
                | "fs.usage"
                | "fs.scrub.status"
                | "fs.reconcile.status"
                | "bcachefs.usage"
                | "service.protocol.list"
                | "subvolume.list_all"
                | "subvolume.find_by_property"
                | "subvolume.children"
                | "smb.user.list"
                | "smb.group.list"
                | "system.update.version"
                | "system.update.status"
                | "system.reboot_required"
                | "system.generations.list"
                | "system.version.get"
                | "system.version.tagged_release_notice"
                | "system.log.level"
                | "system.settings.timezones"
                | "audit.list"
                | "apps.check_ports"
                | "apps.status"
                | "apps.logs"
                | "apps.compose.logs"
                | "apps.container.logs"
                | "apps.inspect"
                | "system.firewall.status"
                | "vm.capabilities"
                | "firmware.available"
                | "firmware.check"
                | "firmware.devices"
                | "notifications.config.get"
                | "apps.config"
                | "apps.inspect_image"
                | "bcachefs.timestats"
                | "bcachefs.top"
                | "backup.profile.list"
                | "backup.profile.get"
                | "backup.status"
                | "backup.snapshots"
        )
}

/// Derive the collection name for a mutation method, or None if read-only.
fn collection_for_method(method: &str) -> Option<&'static str> {
    match method {
        m if m.starts_with("fs.device.") => Some("filesystem"),
        m if m.starts_with("fs.") && !is_read_only(m) => Some("filesystem"),
        m if m.starts_with("device.") && !is_read_only(m) => Some("filesystem"),
        m if m.starts_with("subvolume.") && !is_read_only(m) => Some("subvolume"),
        m if m.starts_with("snapshot.") && !is_read_only(m) => Some("snapshot"),
        m if m.starts_with("share.nfs.") && !is_read_only(m) => Some("share.nfs"),
        m if m.starts_with("share.smb.") && !is_read_only(m) => Some("share.smb"),
        m if m.starts_with("share.iscsi.") && !is_read_only(m) => Some("share.iscsi"),
        m if m.starts_with("share.nvmeof.") && !is_read_only(m) => Some("share.nvmeof"),
        m if m.starts_with("service.protocol.") && !is_read_only(m) => Some("protocol"),
        m if m.starts_with("system.settings.") && !is_read_only(m) => Some("settings"),
        m if m.starts_with("system.tuning.") && !is_read_only(m) => Some("tuning"),
        m if m.starts_with("system.nut.") && !is_read_only(m) => Some("nut"),
        m if m.starts_with("system.tailscale.") && !is_read_only(m) => Some("tailscale"),
        m if m.starts_with("alert.rules.") && !is_read_only(m) => Some("alert"),
        _ => None,
    }
}

/// Extract a human-readable summary from mutation params for audit logging.
fn audit_detail(request: &Request) -> String {
    let params = match request.params.as_ref() {
        Some(p) => p,
        None => return String::new(),
    };

    // Try common identifier fields in order of specificity
    for key in ["name", "username", "filesystem", "target", "id", "path"] {
        if let Some(val) = params.get(key).and_then(|v| v.as_str()) {
            return val.to_string();
        }
    }

    // For device operations, show the device
    if let Some(val) = params.get("device").and_then(|v| v.as_str()) {
        return val.to_string();
    }

    String::new()
}

/// Route a JSON-RPC request to the appropriate handler
pub async fn handle_rpc_request(raw: &str, state: &AppState, session: &Session) -> String {
    let request: Request = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(_) => {
            let resp = Response::error(
                serde_json::Value::Null,
                ErrorCode::ParseError,
                "Failed to parse JSON-RPC request",
            );
            return serde_json::to_string(&resp).unwrap();
        }
    };

    debug!("RPC call: {} (user: {})", request.method, session.username);

    // Force password change — only allow auth methods until the password is changed
    if session.must_change_password
        && !matches!(
            request.method.as_str(),
            "auth.change_password" | "auth.me" | "auth.logout"
        )
    {
        let resp = Response::error(
            request.id,
            ErrorCode::InternalError,
            "Password change required",
        );
        return serde_json::to_string(&resp).unwrap();
    }

    // Enforce role permissions
    let denied = match session.role {
        Role::Admin => false,
        Role::ReadOnly => !is_read_only(&request.method),
        Role::Operator => !is_operator_allowed(&request.method),
    };
    if denied {
        let resp = Response::error(request.id, ErrorCode::InternalError, "Permission denied");
        return serde_json::to_string(&resp).unwrap();
    }

    let t0 = std::time::Instant::now();
    let response = route(&request, state, session).await;
    let elapsed = t0.elapsed();
    if elapsed.as_millis() > 5000 {
        tracing::error!(
            "RPC very slow: {} took {}ms",
            request.method,
            elapsed.as_millis()
        );
    } else if elapsed.as_millis() > 1000 {
        tracing::warn!(
            "RPC slow: {} took {}ms",
            request.method,
            elapsed.as_millis()
        );
    } else {
        debug!("RPC done: {} in {}ms", request.method, elapsed.as_millis());
    }

    // Audit log + broadcast event on successful mutations
    if response.error.is_none() {
        // Auth mutations are already audited in auth.rs — skip them here
        if !is_read_only(&request.method) && !request.method.starts_with("auth.") {
            let detail = audit_detail(&request);
            crate::auth::audit(
                &request.method,
                &session.username,
                session.client_ip.as_deref().unwrap_or("unknown"),
                &detail,
            );
        }
        if let Some(collection) = collection_for_method(&request.method) {
            let _ = state.events.send(collection.to_string());
        }
    }

    serde_json::to_string(&response).unwrap()
}

async fn route(req: &Request, state: &AppState, session: &Session) -> Response {
    match req.method.as_str() {
        // ── Auth ──────────────────────────────────────────────────
        "auth.me" => ok(
            req,
            serde_json::json!({
                "username": session.username,
                "role": session.role,
            }),
        ),
        "auth.logout" => match state.auth.logout(&session.token).await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "auth.change_password" => {
            #[derive(Deserialize)]
            struct P {
                username: String,
                new_password: String,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .auth
                    .change_password(session, &p.username, &p.new_password)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.create_user" => {
            #[derive(Deserialize)]
            struct P {
                username: String,
                password: String,
                role: Role,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .auth
                    .create_user(session, &p.username, &p.password, p.role)
                    .await
                {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.delete_user" => match require_str(req, "username") {
            Ok(username) => match state.auth.delete_user(session, username).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "auth.list_users" => ok(req, state.auth.list_users().await),
        "auth.token.list" => match state.auth.list_api_tokens(session).await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "auth.token.create" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
                role: Role,
                filesystem: Option<String>,
                expires_in_secs: Option<u64>,
                #[serde(default)]
                allowed_ips: Vec<String>,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state
                    .auth
                    .create_api_token(
                        session,
                        &p.name,
                        p.role,
                        p.filesystem,
                        p.expires_in_secs,
                        p.allowed_ips,
                    )
                    .await
                {
                    Ok(t) => ok(req, t),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "auth.token.delete" => match require_str(req, "id") {
            Ok(id) => match state.auth.delete_api_token(session, id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Audit ──────────────────────────────────────────────
        "audit.list" => {
            let limit = req.params.as_ref()
                .and_then(|p| p.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            ok(req, crate::auth::read_audit_log(limit).await)
        }

        // ── System ──────────────────────────────────────────────
        "system.info" => ok(req, state.system.info().await),
        "system.health" => ok(req, state.system.health().await),
        "system.stats" => match fetch_metrics_json::<nasty_system::SystemStats>(
            &state.metrics_client,
            "/api/stats",
        )
        .await
        {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.logs" => {
            let unit = str_param(req, "unit").unwrap_or("nasty-engine");
            let lines: u32 = req.params.as_ref()
                .and_then(|p| p.get("lines"))
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as u32;
            let grep = req.params.as_ref()
                .and_then(|p| p.get("grep"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let lines_str = lines.to_string();
            let mut args = vec![
                "-u", unit,
                "-n", lines_str.as_str(),
                "--no-pager",
                "--output", "short-iso",
            ];
            if let Some(ref g) = grep {
                args.push("--grep");
                args.push(g.as_str());
            }
            let output = tokio::process::Command::new("journalctl")
                .args(&args)
                .output()
                .await;
            match output {
                Ok(o) => ok(req, String::from_utf8_lossy(&o.stdout).to_string()),
                Err(e) => err(req, format!("journalctl: {e}")),
            }
        }
        "system.logs.units" => {
            // Return list of interesting systemd units
            let units = vec![
                "nasty-engine", "nasty-metrics", "nginx",
                "nfs-server", "samba-smbd", "samba-nmbd",
                "docker", "sshd", "avahi-daemon",
                "smartd", "nut-driver", "nut-server", "nut-monitor",
            ];
            let mut available = Vec::new();
            for unit in units {
                let svc = format!("{unit}.service");
                let exists = tokio::process::Command::new("systemctl")
                    .args(["cat", &svc])
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if exists {
                    available.push(unit);
                }
            }
            ok(req, available)
        }
        "system.ssh.status" => {
            // Read from the engine-managed override file (source of truth)
            let password_auth = std::fs::read_to_string("/var/lib/nasty/sshd_override.conf")
                .unwrap_or_default()
                .contains("yes");
            let keys = std::fs::read_to_string("/root/.ssh/authorized_keys")
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
                .map(|l| l.to_string())
                .collect::<Vec<_>>();
            ok(req, serde_json::json!({
                "password_auth": password_auth,
                "keys": keys,
            }))
        }
        "system.ssh.add_key" => {
            let key = match require_str(req, "key") {
                Ok(k) => k.trim().to_string(),
                Err(r) => return r,
            };
            if !key.starts_with("ssh-") && !key.starts_with("ecdsa-") {
                return err(req, "Invalid SSH public key — must start with ssh-rsa, ssh-ed25519, etc.");
            }
            let _ = tokio::fs::create_dir_all("/root/.ssh").await;
            let mut existing = tokio::fs::read_to_string("/root/.ssh/authorized_keys").await.unwrap_or_default();
            if !existing.contains(&key) {
                if !existing.ends_with('\n') && !existing.is_empty() { existing.push('\n'); }
                existing.push_str(&key);
                existing.push('\n');
                if let Err(e) = tokio::fs::write("/root/.ssh/authorized_keys", &existing).await {
                    return err(req, format!("write authorized_keys: {e}"));
                }
                // Set permissions
                let _ = tokio::process::Command::new("chmod").args(["600", "/root/.ssh/authorized_keys"]).status().await;
                let _ = tokio::process::Command::new("chmod").args(["700", "/root/.ssh"]).status().await;
            }
            ok(req, "Key added")
        }
        "system.ssh.remove_key" => {
            let key = match require_str(req, "key") {
                Ok(k) => k.trim().to_string(),
                Err(r) => return r,
            };
            let content = tokio::fs::read_to_string("/root/.ssh/authorized_keys").await.unwrap_or_default();
            let filtered: String = content.lines()
                .filter(|l| l.trim() != key)
                .map(|l| format!("{l}\n"))
                .collect();
            if let Err(e) = tokio::fs::write("/root/.ssh/authorized_keys", &filtered).await {
                return err(req, format!("write authorized_keys: {e}"));
            }
            ok(req, "Key removed")
        }
        "system.ssh.set_password_auth" => {
            let enabled = req.params.as_ref()
                .and_then(|p| p.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // Check that at least one key exists before disabling password auth
            if !enabled {
                let keys = tokio::fs::read_to_string("/root/.ssh/authorized_keys").await.unwrap_or_default();
                let key_count = keys.lines().filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).count();
                if key_count == 0 {
                    return err(req, "Cannot disable password authentication without at least one SSH key — you would be locked out");
                }
            }
            // Write sshd override file and reload — takes effect immediately
            // and survives reboots + rebuilds (NixOS sshd_config includes this file)
            let val = if enabled { "yes" } else { "no" };
            if let Err(e) = tokio::fs::write(
                "/var/lib/nasty/sshd_override.conf",
                format!("PasswordAuthentication {val}\n"),
            ).await {
                tracing::warn!("Failed to write sshd override: {e}");
            }
            let _ = tokio::process::Command::new("systemctl")
                .args(["reload", "sshd"])
                .status().await;
            ok(req, format!("Password authentication {}", if enabled { "enabled" } else { "disabled" }))
        }
        "system.network.get" => ok(req, state.network.get().await),
        "system.network.update" => {
            match parse_params::<nasty_system::network::NetworkConfig>(req) {
                Ok(p) => match state.network.update(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "system.metrics.prometheus" => {
            let url = format!("{}/metrics", crate::METRICS_BASE);
            match state.metrics_client.get(&url).send().await {
                Ok(resp) => match resp.text().await {
                    Ok(text) => ok(req, text),
                    Err(e) => err(req, format!("metrics read error: {e}")),
                },
                Err(e) => err(req, format!("metrics service unavailable: {e}")),
            }
        }
        "system.metrics.history" => {
            let kind = str_param(req, "kind").unwrap_or("net");
            let name = str_param(req, "name");
            let range = str_param(req, "range").unwrap_or("5m");
            let offset = req.params.as_ref()
                .and_then(|p| p.get("offset"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0);
            let mut url = format!(
                "{}/api/history?kind={kind}&range={range}&offset={offset}",
                crate::METRICS_BASE
            );
            if let Some(n) = name {
                url.push_str(&format!("&name={n}"));
            }
            match state
                .metrics_client
                .get(&url)
                .send()
                .await
                .and_then(|r| Ok(r.error_for_status()?))
            {
                Ok(resp) => match resp
                    .json::<Vec<nasty_common::metrics_types::ResourceHistory>>()
                    .await
                {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, format!("metrics parse error: {e}")),
                },
                Err(e) => err(req, format!("metrics service error: {e}")),
            }
        }
        "system.disks" => {
            if state
                .protocols
                .is_enabled(nasty_system::protocol::Protocol::Smart)
                .await
            {
                match fetch_metrics_json::<Vec<nasty_system::DiskHealth>>(
                    &state.metrics_client,
                    "/api/disks",
                )
                .await
                {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                }
            } else {
                ok(req, Vec::<nasty_system::DiskHealth>::new())
            }
        }

        // ── Settings ──────────────────────────────────────────────
        "system.settings.timezones" => match nasty_system::settings::list_timezones().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.settings.get" => ok(req, state.settings.get().await),
        "system.settings.update" => match parse_params(req) {
            Ok(p) => match state.settings.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        "system.acme.status" => ok(req, nasty_system::settings::get_acme_status()),
        "system.acme.reset" => {
            nasty_system::settings::reset_acme_status();
            ok(req, "ok")
        }
        "system.acme.retry" => {
            match nasty_system::settings::retry_acme().await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            }
        }

        // ── Tuning ───────────────────────────────────────────────
        "system.tuning.get" => ok(req, state.tuning.get().await),
        "system.tuning.update" => match parse_params(req) {
            Ok(p) => match state.tuning.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── NUT (UPS) ─────────────────────────────────────────────
        "system.nut.config.get" => ok(req, state.nut.get_config().await),
        "system.nut.config.update" => match parse_params(req) {
            Ok(p) => match state.nut.update_config(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "system.nut.status" => ok(req, state.nut.status().await),

        // ── Tailscale VPN ────────────────────────────────────────
        "system.tailscale.get" => ok(req, state.tailscale.get().await),
        "system.tailscale.connect" => match parse_params(req) {
            Ok(p) => match state.tailscale.connect(p).await {
                Ok(v) => {
                    // Sync NVMe-oF ports for the new Tailscale IP
                    if let Some(ref ip) = v.ip {
                        let nvmeof = state.nvmeof.clone();
                        let ip = ip.clone();
                        tokio::spawn(async move {
                            nvmeof.ensure_tailscale_ports(&ip).await;
                        });
                    }
                    ok(req, v)
                }
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "system.tailscale.disconnect" => match state.tailscale.disconnect().await {
            Ok(v) => {
                // Clean up NVMe-oF ports that were on the Tailscale IP
                // (Tailscale IPs are in the 100.x.y.z range)
                let nvmeof = state.nvmeof.clone();
                tokio::spawn(async move {
                    let subsystems = nvmeof.list().await.unwrap_or_default();
                    for subsys in &subsystems {
                        for port in &subsys.ports {
                            if port.addr.starts_with("100.") {
                                nvmeof.remove_ports_for_ip(&port.addr).await;
                                return; // All Tailscale ports share the same IP
                            }
                        }
                    }
                });
                ok(req, v)
            }
            Err(e) => err(req, e),
        },

        // ── System Update ─────────────────────────────────────────
        "system.update.version" => ok(req, state.updates.version().await),
        "system.update.check" => match state.updates.check().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.update.apply" => match state.updates.apply().await {
            Ok(()) => {
                state.system.invalidate_bcachefs_cache().await;
                ok(req, "ok")
            }
            Err(e) => err(req, e),
        },
        "system.update.rollback" => match state.updates.rollback().await {
            Ok(()) => {
                state.system.invalidate_bcachefs_cache().await;
                ok(req, "ok")
            }
            Err(e) => err(req, e),
        },
        "system.update.status" => ok(req, state.updates.status().await),
        "system.version.get" => match state.updates.version_info().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.version.tagged_release_notice" => {
            match state.updates.version_tagged_release_status().await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "system.version.upgrade_tagged_release" => {
            match state.updates.upgrade_tagged_release().await {
                Ok(()) => {
                    state.system.invalidate_bcachefs_cache().await;
                    ok(req, serde_json::json!({"status": "started"}))
                }
                Err(e) => err(req, e),
            }
        }
        "system.version.cleanup" => match state.updates.version_cleanup().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "system.version.switch" => {
            match parse_params::<nasty_system::update::VersionSwitchRequest>(req) {
                Ok(p) => match state.updates.version_switch(p).await {
                    Ok(()) => {
                        state.system.invalidate_bcachefs_cache().await;
                        ok(req, serde_json::json!({"status": "started"}))
                    }
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "system.update.channel.get" => ok(req, state.updates.get_channel().await),
        "firmware.available" => ok(req, state.firmware.is_available().await),
        "firmware.devices" => ok(req, state.firmware.list_devices().await),
        "firmware.check" => ok(req, state.firmware.check_updates().await),
        "firmware.update" => match require_str(req, "device_id") {
            Ok(id) => ok(req, state.firmware.update_device(id).await),
            Err(r) => r,
        },
        "system.update.channel.set" => match require_str(req, "channel") {
            Ok(ch) => match ch.parse::<nasty_system::update::ReleaseChannel>() {
                Ok(channel) => match state.updates.set_channel(channel).await {
                    Ok(c) => ok(req, c),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            },
            Err(r) => r,
        },
        "system.reboot_required" => ok(req, state.updates.reboot_required().await),

        // ── Logging ───────────────────────────────────────────────
        "system.log.level" => {
            // Return the current filter as a string — not easily available from reload handle,
            // so just return a placeholder. The set method is more useful.
            ok(req, "use system.log.set_level to change")
        }
        "system.log.set_level" => {
            #[derive(Deserialize)]
            struct P {
                filter: String,
            }
            match parse_params::<P>(req) {
                Ok(p) => match tracing_subscriber::EnvFilter::try_new(&p.filter) {
                    Ok(new_filter) => match state.log_reload.reload(new_filter) {
                        Ok(()) => {
                            tracing::info!("Log filter changed to: {}", p.filter);
                            ok(req, "ok")
                        }
                        Err(e) => err(req, format!("failed to reload filter: {e}")),
                    },
                    Err(e) => err(req, format!("invalid filter: {e}")),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── Generations ──────────────────────────────────────────
        "system.generations.list" => match state.updates.list_generations().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "system.generations.switch" => {
            #[derive(Deserialize)]
            struct P {
                generation: u64,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state.updates.switch_generation(p.generation).await {
                    Ok(()) => {
                        state.system.invalidate_bcachefs_cache().await;
                        ok(req, serde_json::json!({"status": "started"}))
                    }
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "system.generations.label" => {
            #[derive(Deserialize)]
            struct P {
                generation: u64,
                label: Option<String>,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state.updates.label_generation(p.generation, p.label).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "system.generations.delete" => {
            #[derive(Deserialize)]
            struct P {
                generation: u64,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state.updates.delete_generation(p.generation).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        "system.reboot" => match state.updates.reboot().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "system.shutdown" => match state.updates.shutdown().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },

        // ── Protocols ────────────────────────────────────────────
        "service.protocol.list" => ok(req, state.protocols.list().await),
        "service.protocol.enable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.enable(name).await {
                Ok(v) => {
                    if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                        state.firewall.open(proto).await;
                    }
                    ok(req, v)
                },
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "service.protocol.disable" => match require_str(req, "name") {
            Ok(name) => match state.protocols.disable(name).await {
                Ok(v) => {
                    if let Some(proto) = nasty_system::protocol::Protocol::from_name(name) {
                        state.firewall.close(proto).await;
                    }
                    ok(req, v)
                },
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "system.firewall.status" => ok(req, state.firewall.status().await),
        "system.firewall.restrict" => {
            let service = match require_str(req, "service") {
                Ok(s) => s.to_string(),
                Err(r) => return r,
            };
            let sources: Vec<String> = req.params.as_ref()
                .and_then(|p| p.get("sources"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let interfaces: Vec<String> = req.params.as_ref()
                .and_then(|p| p.get("interfaces"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            match state.firewall.set_restriction(&service, sources, interfaces).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            }
        },

        // ── Alerts ───────────────────────────────────────────────
        "telemetry.send" => {
            let sent = crate::telemetry::send_report(&state).await;
            ok(req, serde_json::json!({ "sent": sent }))
        }

        "system.alerts" => {
            // Return cached result if fresh (< 10s old)
            {
                let cache = state.alerts_cache.lock().await;
                if let Some((ts, ref cached)) = *cache {
                    if ts.elapsed() < std::time::Duration::from_secs(20) {
                        return ok(req, cached.clone());
                    }
                }
            }

            // Evaluate current alert rules against live system state
            let stats = match fetch_metrics_json::<nasty_system::SystemStats>(
                &state.metrics_client,
                "/api/stats",
            )
            .await
            {
                Ok(v) => v,
                Err(e) => return err(req, e),
            };
            let fs_list = state.filesystems.list().await;
            let disk_health: Vec<nasty_system::DiskHealth> = if state
                .protocols
                .is_enabled(nasty_system::protocol::Protocol::Smart)
                .await
            {
                fetch_metrics_json(&state.metrics_client, "/api/disks")
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            let filesystems = fs_list.unwrap_or_default();
            let fs_usage_list: Vec<nasty_system::alerts::FsUsage> = filesystems
                .iter()
                .map(|p| nasty_system::alerts::FsUsage {
                    name: p.name.clone(),
                    used_bytes: p.used_bytes,
                    total_bytes: p.total_bytes,
                })
                .collect();

            let disk_summary: Vec<nasty_system::alerts::DiskHealthSummary> = disk_health
                .into_iter()
                .map(|d| nasty_system::alerts::DiskHealthSummary {
                    device: d.device,
                    temperature_c: d.temperature_c,
                    health_passed: d.health_passed,
                })
                .collect();

            // Collect bcachefs health from mounted filesystems — run all checks in parallel
            let mut health_tasks = tokio::task::JoinSet::new();
            for fs in filesystems.iter().filter(|fs| fs.mounted) {
                let fs_service = state.filesystems.clone();
                let fs = fs.clone();
                health_tasks.spawn(async move {
                    let degraded = fs.options.degraded.unwrap_or(false);
                    let devices: Vec<nasty_system::alerts::BcachefsDeviceHealth> = fs
                        .devices
                        .iter()
                        .map(|d| nasty_system::alerts::BcachefsDeviceHealth {
                            path: d.path.clone(),
                            state: d.state.clone().unwrap_or_else(|| "rw".into()),
                            has_errors: d
                                .has_data
                                .as_deref()
                                .map_or(false, |s| s.contains("error")),
                        })
                        .collect();

                    // Run IO error count, scrub status, and reconcile status in parallel
                    let (io_error_count, scrub_result, reconcile_result) = tokio::join!(
                        read_bcachefs_error_count(&fs.uuid),
                        fs_service.scrub_status(&fs.name),
                        fs_service.reconcile_status(&fs.name),
                    );

                    let scrub_errors = match scrub_result {
                        Ok(s) => s.raw.to_lowercase().contains("error"),
                        Err(_) => false,
                    };

                    let reconcile_stalled = match reconcile_result {
                        Ok(s) => {
                            let raw = s.raw.to_lowercase();
                            let scan_pending = raw
                                .lines()
                                .find(|l| l.contains("scan pending"))
                                .and_then(|l| l.split_whitespace().last())
                                .and_then(|n| n.parse::<u64>().ok())
                                .unwrap_or(0)
                                > 0;
                            let work_pending = raw
                                .lines()
                                .find(|l| l.trim().starts_with("pending:"))
                                .map(|l| l.split_whitespace().skip(1).any(|n| n != "0"))
                                .unwrap_or(false);
                            (scan_pending || work_pending) && !raw.contains("running")
                        }
                        Err(_) => false,
                    };

                    nasty_system::alerts::BcachefsHealth {
                        fs_name: fs.name.clone(),
                        degraded,
                        devices,
                        io_error_count,
                        scrub_errors,
                        reconcile_stalled,
                    }
                });
            }
            let mut bcachefs_health = Vec::new();
            while let Some(result) = health_tasks.join_next().await {
                if let Ok(health) = result {
                    bcachefs_health.push(health);
                }
            }

            // Collect kernel errors from metrics service
            let kernel_summary: nasty_common::metrics_types::KernelErrorSummary =
                fetch_metrics_json(&state.metrics_client, "/api/kernel_errors")
                    .await
                    .unwrap_or_default();
            let kernel_alert = nasty_system::alerts::KernelErrorAlert {
                total_count: kernel_summary.total_count,
                categories: kernel_summary
                    .by_category
                    .iter()
                    .map(|c| c.category.clone())
                    .collect(),
            };

            let mut alerts = state
                .alerts
                .evaluate(
                    &stats,
                    &fs_usage_list,
                    &disk_summary,
                    &bcachefs_health,
                    &kernel_alert,
                )
                .await;

            // Inject mount failure alerts (from boot)
            let mount_failures = state.mount_failures.lock().await;
            for name in mount_failures.iter() {
                alerts.push(nasty_system::alerts::ActiveAlert {
                    rule_id: "mount-failure".into(),
                    rule_name: "Filesystem failed to mount".into(),
                    severity: nasty_system::alerts::AlertSeverity::Critical,
                    metric: nasty_system::alerts::AlertMetric::BcachefsDegraded,
                    message: format!("Filesystem \"{name}\" failed to mount after boot. Check disk connectivity and logs."),
                    current_value: 1.0,
                    threshold: 0.0,
                    source: name.clone(),
                });
            }
            drop(mount_failures);

            // Cache for subsequent polls
            let value = serde_json::to_value(&alerts).unwrap_or_default();
            *state.alerts_cache.lock().await = Some((std::time::Instant::now(), value));

            ok(req, alerts)
        }
        "alert.rules.list" => ok(req, state.alerts.list_rules().await),
        "alert.rules.create" => match parse_params(req) {
            Ok(rule) => match state.alerts.create_rule(rule).await {
                Ok(r) => ok(req, r),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "alert.rules.update" => match parse_params::<nasty_system::alerts::AlertRuleUpdate>(req) {
            Ok(update) => match state.alerts.update_rule(&update.id.clone(), update).await {
                Ok(r) => ok(req, r),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "alert.rules.delete" => match require_str(req, "id") {
            Ok(id) => match state.alerts.delete_rule(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Notifications ──────────────────────────────────────────
        "notifications.config.get" => {
            ok(req, nasty_system::notifications::NotificationConfig::load())
        }
        "notifications.config.update" => match parse_params::<nasty_system::notifications::NotificationConfig>(req) {
            Ok(config) => match config.save().await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => err(req, e),
        },
        "notifications.test" => match parse_params::<nasty_system::notifications::ChannelType>(req) {
            Ok(channel) => match nasty_system::notifications::test_channel(&channel).await {
                Ok(msg) => ok(req, msg),
                Err(e) => err(req, e),
            },
            Err(e) => err(req, e),
        },

        // ── Backups ─────────────────────────────────────────────
        "backup.profile.list" => ok(req, state.backups.list_profiles().await),
        "backup.profile.get" => match require_str(req, "id") {
            Ok(id) => match state.backups.get_profile(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.profile.create" => match parse_params::<nasty_backup::BackupProfile>(req) {
            Ok(p) => match state.backups.create_profile(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(e) => err(req, e),
        },
        "backup.profile.update" => {
            let id = match require_str(req, "id") { Ok(s) => s.to_string(), Err(r) => return r };
            match parse_params::<nasty_backup::BackupProfile>(req) {
                Ok(p) => match state.backups.update_profile(&id, p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e.to_rpc_error()),
                },
                Err(e) => err(req, e),
            }
        }
        "backup.profile.delete" => match require_str(req, "id") {
            Ok(id) => match state.backups.delete_profile(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.status" => ok(req, state.backups.status().await),
        "backup.repo.init" => match require_str(req, "id") {
            Ok(id) => match state.backups.init_repo(id).await {
                Ok(msg) => ok(req, msg),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.run" => {
            let id = match require_str(req, "id") { Ok(s) => s.to_string(), Err(r) => return r };
            // Run in background, return immediately
            let backups = state.backups.clone_for_task();
            tokio::spawn(async move {
                let _ = backups.run_backup(&id).await;
            });
            ok(req, "Backup started")
        }
        "backup.snapshots" => match require_str(req, "id") {
            Ok(id) => match state.backups.list_snapshots(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },
        "backup.repo.check" => match require_str(req, "id") {
            Ok(id) => match state.backups.check_repo(id).await {
                Ok(msg) => ok(req, msg),
                Err(e) => err(req, e.to_rpc_error()),
            },
            Err(r) => r,
        },

        // ── Pools ───────────────────────────────────────────────
        "fs.list" => match state.filesystems.list().await {
            Ok(mut v) => {
                if let Some(ref fs_name) = session.filesystem {
                    v.retain(|p| &p.name == fs_name);
                }
                ok(req, v)
            }
            Err(e) => err(req, e),
        },
        "fs.get" => match require_str(req, "name") {
            Ok(name) => {
                if session.filesystem.as_deref().map_or(false, |p| p != name) {
                    err(req, "access denied")
                } else {
                    match state.filesystems.get(name).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "fs.create" => match parse_params(req) {
            Ok(p) => match state.filesystems.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.destroy" => {
            match parse_params::<nasty_storage::filesystem::DestroyFilesystemRequest>(req) {
                Ok(p) => {
                    if let Some(reason) = check_filesystem_in_use(state, &p.name).await {
                        err(req, reason)
                    } else {
                        match state.filesystems.destroy(p).await {
                            Ok(()) => ok(req, "ok"),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "fs.mount" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.mount(name).await {
                Ok(v) => {
                    // Cascade: restore block devices on this filesystem
                    let _ = state.subvolumes.restore_block_devices().await;
                    ok(req, v)
                }
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.unmount" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.unmount(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.unlock" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let passphrase = p.get("passphrase").and_then(|v| v.as_str()).unwrap_or("");
                match state.filesystems.unlock(name, passphrase).await {
                    Ok(fs) => ok(req, fs),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "fs.key.export" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.export_key(name).await {
                Ok(key) => ok(req, key),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.key.delete" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.delete_key(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "device.list" => match state.filesystems.list_devices().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "device.wipe" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                let path = p
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match state.filesystems.device_wipe(&path).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "fs.options.update" => match parse_params(req) {
            Ok(p) => match state.filesystems.update_options(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.add" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_add(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.remove" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_remove(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.evacuate" => {
            match parse_params::<nasty_storage::filesystem::DeviceActionRequest>(req) {
                Ok(p) => {
                    // Validate synchronously before returning
                    match state.filesystems.get(&p.filesystem).await {
                        Err(e) => err(req, e),
                        Ok(fs) if !fs.mounted => err(
                            req,
                            nasty_storage::FilesystemError::CommandFailed(
                                "filesystem must be mounted to evacuate a device".into(),
                            ),
                        ),
                        Ok(_) => {
                            // Run in background — bcachefs evacuate can take many minutes.
                            // Emit filesystem events every 3 s so UI shows live device state.
                            let fs_svc = state.filesystems.clone();
                            let events = state.events.clone();
                            tokio::spawn(async move {
                                let poll_events = events.clone();
                                let poll = tokio::spawn(async move {
                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                        let _ = poll_events.send("filesystem".to_string());
                                    }
                                });
                                let _ = fs_svc.device_evacuate(p).await;
                                poll.abort();
                                let _ = events.send("filesystem".to_string());
                            });
                            ok(req, serde_json::json!({"status": "started"}))
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "fs.device.set_state" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_set_state(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.online" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_online(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.offline" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_offline(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "fs.device.set_label" => match parse_params(req) {
            Ok(p) => match state.filesystems.device_set_label(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // ── Filesystem health & monitoring ─────────────────────────────
        "fs.usage" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.usage(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.scrub.start" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.scrub_start(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.scrub.status" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.scrub_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.reconcile.status" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.reconcile_status(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.reconcile.enable" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.set_reconcile_enabled(name, true).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "fs.reconcile.disable" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.set_reconcile_enabled(name, false).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── bcachefs diagnostics ────────────────────────────────
        "bcachefs.usage" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.bcachefs_usage(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "bcachefs.top" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.bcachefs_top(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "bcachefs.timestats" => match require_str(req, "name") {
            Ok(name) => match state.filesystems.bcachefs_timestats(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Subvolumes ──────────────────────────────────────────
        "subvolume.list_all" => {
            let fs_filter = session.filesystem.as_deref();
            let owner_filter = session.owner.as_deref();
            match state.subvolumes.list_all(fs_filter, owner_filter).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "subvolume.list" => match require_str(req, "filesystem") {
            Ok(fs_name) => {
                if session
                    .filesystem
                    .as_deref()
                    .map_or(false, |p| p != fs_name)
                {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .list(fs_name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "subvolume.get" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session
                    .filesystem
                    .as_deref()
                    .map_or(false, |p| p != fs_name)
                {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .get(fs_name, name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.children" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => match state.subvolumes.list_children(fs_name, name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.create" => {
            match parse_params::<nasty_storage::subvolume::CreateSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        let owner = session.owner.clone();
                        match state.subvolumes.create(p, owner).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.delete" => {
            match parse_params::<nasty_storage::subvolume::DeleteSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else if let Some(conflict) =
                        check_subvolume_in_use(state, &p.filesystem, &p.name).await
                    {
                        err(req, conflict)
                    } else {
                        match state.subvolumes.delete(p, session.owner.as_deref()).await {
                            Ok(()) => ok(req, "ok"),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.attach" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session
                    .filesystem
                    .as_deref()
                    .map_or(false, |p| p != fs_name)
                {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .attach(fs_name, name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },
        "subvolume.detach" => match (require_str(req, "filesystem"), require_str(req, "name")) {
            (Ok(fs_name), Ok(name)) => {
                if session
                    .filesystem
                    .as_deref()
                    .map_or(false, |p| p != fs_name)
                {
                    err(req, "access denied")
                } else {
                    match state
                        .subvolumes
                        .detach(fs_name, name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            (Err(r), _) | (_, Err(r)) => r,
        },

        "subvolume.resize" => {
            match parse_params::<nasty_storage::subvolume::ResizeSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state.subvolumes.resize(p, session.owner.as_deref()).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }

        "subvolume.update" => {
            match parse_params::<nasty_storage::subvolume::UpdateSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state.subvolumes.update(p, session.owner.as_deref()).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }

        "subvolume.clone" => {
            match parse_params::<nasty_storage::subvolume::CloneSubvolumeRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |f| f != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state
                            .subvolumes
                            .clone_subvolume(p, session.owner.as_deref())
                            .await
                        {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }

        "subvolume.set_properties" => {
            match parse_params::<nasty_storage::subvolume::SetPropertiesRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |sp| sp != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state
                            .subvolumes
                            .set_properties(p, session.owner.as_deref())
                            .await
                        {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.remove_properties" => {
            match parse_params::<nasty_storage::subvolume::RemovePropertiesRequest>(req) {
                Ok(p) => {
                    if session
                        .filesystem
                        .as_deref()
                        .map_or(false, |sp| sp != p.filesystem)
                    {
                        err(req, "access denied")
                    } else {
                        match state
                            .subvolumes
                            .remove_properties(p, session.owner.as_deref())
                            .await
                        {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "subvolume.find_by_property" => {
            match parse_params::<nasty_storage::subvolume::FindByPropertyRequest>(req) {
                Ok(p) => {
                    // Enforce filesystem-scoped token restriction
                    let effective_fs = match (&session.filesystem, &p.filesystem) {
                        (Some(sp), Some(rp)) if sp != rp => {
                            return err(req, "access denied");
                        }
                        (Some(sp), None) => Some(nasty_storage::subvolume::FindByPropertyRequest {
                            filesystem: Some(sp.clone()),
                            key: p.key.clone(),
                            value: p.value.clone(),
                        }),
                        _ => None,
                    };
                    let req_effective = effective_fs.unwrap_or(p);
                    match state
                        .subvolumes
                        .find_by_property(req_effective, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }

        // ── Snapshots ───────────────────────────────────────────
        "snapshot.list" => match require_str(req, "filesystem") {
            Ok(fs_name) => {
                if session
                    .filesystem
                    .as_deref()
                    .map_or(false, |p| p != fs_name)
                {
                    err(req, "access denied")
                } else {
                    match state
                        .snapshots
                        .list(fs_name, session.owner.as_deref())
                        .await
                    {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
            }
            Err(r) => r,
        },
        "snapshot.create" => match parse_params(req) {
            Ok(p) => match state.snapshots.create(p, session.owner.as_deref()).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "snapshot.delete" => match parse_params(req) {
            Ok(p) => match state.snapshots.delete(p, session.owner.as_deref()).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "snapshot.clone" => match parse_params(req) {
            Ok(p) => match state
                .snapshots
                .clone_snapshot(p, session.owner.as_deref())
                .await
            {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        // ── NFS Shares ──────────────────────────────────────────
        "share.nfs.list" => match state.nfs.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.nfs.get" => match require_str(req, "id") {
            Ok(id) => match state.nfs.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.nfs.create" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nfs).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nfs.create(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nfs.update" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nfs).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nfs.update(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nfs.delete" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nfs).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nfs.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── SMB Shares ──────────────────────────────────────────
        "share.smb.list" => match state.smb.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.smb.get" => match require_str(req, "id") {
            Ok(id) => match state.smb.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.smb.create" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.smb.create(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.smb.update" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.smb.update(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.smb.delete" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.smb.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── SMB Users ──────────────────────────────────────────
        "smb.user.list" => match state.smb.list_users().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "smb.user.create" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await {
                return r;
            }
            match parse_params::<nasty_sharing::smb::CreateSmbUserRequest>(req) {
                Ok(p) => match state.smb.create_user(p).await {
                    Ok(u) => ok(req, u),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "smb.user.delete" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await {
                return r;
            }
            match require_str(req, "username") {
                Ok(username) => match state.smb.delete_user(username).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(r) => r,
            }
        }
        "smb.user.set_password" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Smb).await {
                return r;
            }
            #[derive(Deserialize)]
            struct P {
                username: String,
                password: String,
            }
            match parse_params::<P>(req) {
                Ok(p) => match state.smb.set_user_password(&p.username, &p.password).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── SMB Groups ─────────────────────────────────────────
        "smb.group.list" => match state.smb.list_groups().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "smb.group.create" => {
            match require_str(req, "name") {
                Ok(name) => match state.smb.create_group(name).await {
                    Ok(g) => ok(req, g),
                    Err(e) => err(req, e),
                },
                Err(r) => r,
            }
        }
        "smb.group.delete" => {
            match require_str(req, "name") {
                Ok(name) => match state.smb.delete_group(name).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(r) => r,
            }
        }
        "smb.group.add_member" => {
            #[derive(Deserialize)]
            struct P { group: String, user: String }
            match parse_params::<P>(req) {
                Ok(p) => match state.smb.add_group_member(&p.group, &p.user).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "smb.group.remove_member" => {
            #[derive(Deserialize)]
            struct P { group: String, user: String }
            match parse_params::<P>(req) {
                Ok(p) => match state.smb.remove_group_member(&p.group, &p.user).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── iSCSI Targets ───────────────────────────────────────
        "share.iscsi.list" => match state.iscsi.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.iscsi.get" => match require_str(req, "id") {
            Ok(id) => match state.iscsi.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.iscsi.create" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await {
                return r;
            }
            match parse_params::<nasty_sharing::iscsi::CreateTargetRequest>(req) {
                Ok(p) => {
                    if let Some(ref dp) = p.device_path {
                        if let Some(conflict) =
                            check_block_device_conflict(state, dp, "iscsi").await
                        {
                            return err(req, conflict);
                        }
                    }
                    match state.iscsi.create(p).await {
                        Ok(v) => ok(req, v),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.delete" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.add_lun" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await {
                return r;
            }
            match parse_params::<nasty_sharing::iscsi::AddLunRequest>(req) {
                Ok(p) => {
                    if let Some(conflict) =
                        check_block_device_conflict(state, &p.backstore_path, "iscsi").await
                    {
                        err(req, conflict)
                    } else {
                        match state.iscsi.add_lun(p).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.remove_lun" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.remove_lun(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.add_acl" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.add_acl(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.iscsi.remove_acl" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Iscsi).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.iscsi.remove_acl(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── NVMe-oF Subsystems ─────────────────────────────────
        "share.nvmeof.list" => match state.nvmeof.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "share.nvmeof.get" => match require_str(req, "id") {
            Ok(id) => match state.nvmeof.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "share.nvmeof.create" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params::<nasty_sharing::nvmeof::CreateSubsystemRequest>(req) {
                Ok(p) => {
                    if let Some(ref device_path) = p.device_path {
                        if let Some(conflict) =
                            check_block_device_conflict(state, device_path, "nvmeof").await
                        {
                            return err(req, conflict);
                        }
                    }
                    match state.nvmeof.create(p).await {
                        Ok(v) => {
                            // If Tailscale is connected, add a port for its IP too
                            if !v.ports.is_empty() {
                                let ts = state.tailscale.get().await;
                                if ts.connected {
                                    if let Some(ref ip) = ts.ip {
                                        let nvmeof = state.nvmeof.clone();
                                        let id = v.id.clone();
                                        let ip = ip.clone();
                                        tokio::spawn(async move {
                                            let _ = nvmeof.add_port(nasty_sharing::nvmeof::AddPortRequest {
                                                subsystem_id: id,
                                                transport: Some("tcp".to_string()),
                                                addr: Some(ip),
                                                service_id: Some(4420),
                                                addr_family: Some("ipv4".to_string()),
                                            }).await;
                                        });
                                    }
                                }
                            }
                            ok(req, v)
                        }
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.delete" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.delete(p).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.add_namespace" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params::<nasty_sharing::nvmeof::AddNamespaceRequest>(req) {
                Ok(p) => {
                    if let Some(conflict) =
                        check_block_device_conflict(state, &p.device_path, "nvmeof").await
                    {
                        err(req, conflict)
                    } else {
                        match state.nvmeof.add_namespace(p).await {
                            Ok(v) => ok(req, v),
                            Err(e) => err(req, e),
                        }
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.remove_namespace" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.remove_namespace(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.add_port" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.add_port(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.remove_port" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.remove_port(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.add_host" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.add_host(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "share.nvmeof.remove_host" => {
            if let Some(r) = require_protocol(state, req, nasty_system::protocol::Protocol::Nvmeof).await {
                return r;
            }
            match parse_params(req) {
                Ok(p) => match state.nvmeof.remove_host(p).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }

        // ── Virtual Machines ───────────────────────────────────
        "vm.capabilities" => match state.vms.capabilities().await {
            c => ok(req, c),
        },
        "vm.list" => match state.vms.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "vm.get" => match require_str(req, "id") {
            Ok(id) => match state.vms.get(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.create" => match parse_params(req) {
            Ok(p) => match state.vms.create(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.update" => match parse_params(req) {
            Ok(p) => match state.vms.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.delete" => match require_str(req, "id") {
            Ok(id) => match state.vms.delete(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.start" => match require_str(req, "id") {
            Ok(id) => match state.vms.start(id).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.stop" => match require_str(req, "id") {
            Ok(id) => match state.vms.stop(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.kill" => match require_str(req, "id") {
            Ok(id) => match state.vms.kill(id).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "vm.snapshot" => match parse_params::<nasty_vm::SnapshotVmRequest>(req) {
            Ok(p) => match vm_snapshot(state, &p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "vm.clone" => match parse_params::<nasty_vm::CloneVmRequest>(req) {
            Ok(p) => match vm_clone(state, &p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },

        // List VM images (ISO, qcow2, img, raw) from "images" subvolumes
        "vm.images.list" => ok(req, list_vm_images(state).await),

        // Ensure an "images" subvolume exists on the given filesystem
        "vm.images.ensure" => match require_str(req, "filesystem") {
            Ok(fs) => match ensure_images_subvolume(state, fs).await {
                Ok(path) => ok(req, path),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Apps ──────────────────────────────────────────────
        "apps.status" => ok(req, state.apps.status().await),
        "apps.enable" => {
            let p: nasty_apps::EnableAppsRequest = parse_params(req).unwrap_or_default();
            match state.apps.enable(p).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            }
        }
        "apps.disable" => match state.apps.disable().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        "apps.list" => match state.apps.list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.get" => match require_str(req, "name") {
            Ok(name) => match state.apps.get(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.inspect" => match require_str(req, "name") {
            Ok(name) => match state.apps.inspect(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.install" => match parse_params(req) {
            Ok(p) => match state.apps.install(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.update" => match parse_params(req) {
            Ok(p) => match state.apps.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.inspect_image" => match require_str(req, "image") {
            Ok(image) => match state.apps.inspect_image(image).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.check_ports" => match parse_params(req) {
            Ok(p) => ok(req, state.apps.check_ports(p).await),
            Err(e) => invalid(req, e),
        },
        "apps.config" => match require_str(req, "name") {
            Ok(name) => match state.apps.get_config(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.remove(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.stop" => match require_str(req, "name") {
            Ok(name) => match state.apps.stop(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.start" => match require_str(req, "name") {
            Ok(name) => match state.apps.start(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.restart" => match require_str(req, "name") {
            Ok(name) => match state.apps.restart(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.pull" => match require_str(req, "name") {
            Ok(name) => match state.apps.pull(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.prune" => match state.apps.prune().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.exec_command" => match require_str(req, "name") {
            Ok(name) => match state.apps.exec_command(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.logs" => {
            let name = match require_str(req, "name") {
                Ok(n) => n,
                Err(r) => return r,
            };
            let tail = req
                .params
                .as_ref()
                .and_then(|p| p.get("tail"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            match state.apps.logs(name, tail).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "apps.container.logs" => {
            let container_id = match require_str(req, "container_id") {
                Ok(n) => n,
                Err(r) => return r,
            };
            let tail = req
                .params
                .as_ref()
                .and_then(|p| p.get("tail"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            match state.apps.container_logs(container_id, tail).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "apps.compose.install" => match parse_params(req) {
            Ok(p) => match state.apps.compose_install(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.compose.update" => match parse_params(req) {
            Ok(p) => match state.apps.compose_update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.compose.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.compose_remove(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.compose.get" => match require_str(req, "name") {
            Ok(name) => match state.apps.compose_get(name).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },
        "apps.compose.logs" => {
            let name = match require_str(req, "name") {
                Ok(n) => n,
                Err(r) => return r,
            };
            let tail = req
                .params
                .as_ref()
                .and_then(|p| p.get("tail"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            match state.apps.compose_logs(name, tail).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            }
        }
        "apps.ingress.list" => match state.apps.ingress_list().await {
            Ok(v) => ok(req, v),
            Err(e) => err(req, e),
        },
        "apps.ingress.set" => match parse_params(req) {
            Ok(p) => match state.apps.ingress_set(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "apps.ingress.remove" => match require_str(req, "name") {
            Ok(name) => match state.apps.ingress_remove(name).await {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            },
            Err(r) => r,
        },

        // ── Unknown ─────────────────────────────────────────────
        _ => Response::error(
            req.id.clone(),
            ErrorCode::MethodNotFound,
            format!("Unknown method: {}", req.method),
        ),
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn ok(req: &Request, val: impl serde::Serialize) -> Response {
    Response::success(req.id.clone(), serde_json::to_value(val).unwrap())
}

fn err(req: &Request, e: impl std::fmt::Display) -> Response {
    Response::error(req.id.clone(), ErrorCode::InternalError, e.to_string())
}

fn invalid(req: &Request, msg: impl std::fmt::Display) -> Response {
    Response::error(
        req.id.clone(),
        ErrorCode::InvalidParams,
        format!("Invalid params: {msg}"),
    )
}

/// Return an error response if the given protocol is not enabled.
async fn require_protocol(
    state: &AppState,
    req: &Request,
    proto: nasty_system::protocol::Protocol,
) -> Option<Response> {
    if !state.protocols.is_enabled(proto).await {
        Some(Response::error(
            req.id.clone(),
            ErrorCode::InternalError,
            format!(
                "{} protocol is not enabled — enable it first via service.protocol.enable",
                proto.display_name()
            ),
        ))
    } else {
        None
    }
}

fn require_str<'a>(req: &'a Request, key: &str) -> Result<&'a str, Response> {
    str_param(req, key).ok_or_else(|| {
        Response::error(
            req.id.clone(),
            ErrorCode::InvalidParams,
            format!("Missing required param: {key}"),
        )
    })
}

/// Fetch JSON from the nasty-metrics service.
async fn fetch_metrics_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
) -> Result<T, String> {
    let url = format!("{}{path}", crate::METRICS_BASE);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("metrics service unavailable: {e}"))?
        .error_for_status()
        .map_err(|e| format!("metrics service error: {e}"))?;
    resp.json::<T>()
        .await
        .map_err(|e| format!("metrics parse error: {e}"))
}

/// Check if a block device is already exported by another block protocol.
/// Returns an error message if the device is in use, None if it's free.
async fn check_block_device_conflict(
    state: &AppState,
    device_path: &str,
    exclude_protocol: &str,
) -> Option<String> {
    if exclude_protocol != "iscsi" {
        if let Ok(targets) = state.iscsi.list().await {
            for target in &targets {
                for lun in &target.luns {
                    if lun.backstore_path == device_path {
                        return Some(format!(
                            "device {} is already exported via iSCSI (target '{}')",
                            device_path, target.iqn
                        ));
                    }
                }
            }
        }
    }

    if exclude_protocol != "nvmeof" {
        if let Ok(subsystems) = state.nvmeof.list().await {
            for sub in &subsystems {
                for ns in &sub.namespaces {
                    if ns.device_path == device_path {
                        return Some(format!(
                            "device {} is already exported via NVMe-oF (subsystem '{}')",
                            device_path, sub.nqn
                        ));
                    }
                }
            }
        }
    }

    None
}

// ── VM image management ─────────────────────────────────────────

const VM_IMAGE_EXTENSIONS: &[&str] = &["iso", "qcow2", "img", "raw"];

#[derive(serde::Serialize)]
struct VmImageListResult {
    subvolume_exists: bool,
    images: Vec<serde_json::Value>,
}

/// List all VM images from `vms/images` directories across all filesystems.
async fn list_vm_images(state: &AppState) -> VmImageListResult {
    let filesystems = state.filesystems.list().await.unwrap_or_default();
    let mut images = Vec::new();
    let mut subvolume_exists = false;

    for fs in &filesystems {
        if !fs.mounted {
            continue;
        }
        let Some(ref mp) = fs.mount_point else {
            continue;
        };
        let dir = format!("{mp}/vms/images");
        if !std::path::Path::new(&dir).is_dir() {
            continue;
        }
        subvolume_exists = true;

        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let is_image = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| {
                        VM_IMAGE_EXTENSIONS
                            .iter()
                            .any(|ext| e.eq_ignore_ascii_case(ext))
                    })
                    .unwrap_or(false);

                if is_image {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let size = tokio::fs::metadata(&path)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    images.push(serde_json::json!({
                        "name": name,
                        "path": path.to_string_lossy(),
                        "filesystem": fs.name,
                        "size_bytes": size,
                    }));
                }
            }
        }
    }

    VmImageListResult {
        subvolume_exists,
        images,
    }
}

/// Ensure the `vms/images` directory exists on a filesystem. Creates it if missing.
/// Migrates from legacy `.nasty/images` path if present.
async fn ensure_images_subvolume(state: &AppState, filesystem: &str) -> Result<String, String> {
    let mount_point = state
        .filesystems
        .get(filesystem)
        .await
        .map_err(|e| e.to_string())?
        .mount_point
        .ok_or_else(|| "filesystem not mounted".to_string())?;

    let images_path = format!("{mount_point}/vms/images");
    let legacy_path = format!("{mount_point}/.nasty/images");

    // Migrate legacy path
    if !std::path::Path::new(&images_path).exists() && std::path::Path::new(&legacy_path).exists() {
        tokio::fs::create_dir_all(format!("{mount_point}/vms")).await
            .map_err(|e| format!("failed to create vms dir: {e}"))?;
        if let Err(e) = tokio::fs::rename(&legacy_path, &images_path).await {
            tracing::warn!("Failed to migrate VM images from {legacy_path}: {e}, using legacy path");
            return Ok(legacy_path);
        }
        tracing::info!("Migrated VM images from {legacy_path} to {images_path}");
    }

    tokio::fs::create_dir_all(&images_path)
        .await
        .map_err(|e| format!("failed to create vms/images: {e}"))?;

    Ok(images_path)
}

// ── Subvolume in-use check ───────────────────────────────────────

/// Check if a subvolume is in use by a VM, iSCSI target, or NVMe-oF subsystem.
/// Returns an error message if in use, None if safe to delete.
async fn check_subvolume_in_use(state: &AppState, filesystem: &str, name: &str) -> Option<String> {
    let sv = match state.subvolumes.get(filesystem, name, None).await.ok() {
        Some(sv) => sv,
        None => return None,
    };
    let block_device = sv.block_device.as_deref();
    let subvol_path = &sv.path;

    // ── Block device checks (VMs, iSCSI, NVMe-oF) ──

    if let Some(bd) = block_device {
        // Check VMs
        if let Ok(vms) = state.vms.list().await {
            for vm in &vms {
                for disk in &vm.config.disks {
                    if disk.path == bd {
                        return Some(format!(
                            "subvolume is in use as a disk by VM '{}'. Detach the disk first.",
                            vm.config.name
                        ));
                    }
                }
            }
        }

        // Check iSCSI targets
        if let Ok(targets) = state.iscsi.list().await {
            for target in &targets {
                for lun in &target.luns {
                    if lun.backstore_path == bd {
                        return Some(format!(
                            "subvolume is in use by iSCSI target '{}'. Delete the target first.",
                            target.iqn
                        ));
                    }
                }
            }
        }

        // Check NVMe-oF subsystems
        if let Ok(subsystems) = state.nvmeof.list().await {
            for subsys in &subsystems {
                for ns in &subsys.namespaces {
                    if ns.device_path == bd {
                        return Some(format!(
                            "subvolume is in use by NVMe-oF subsystem '{}'. Delete the subsystem first.",
                            subsys.nqn
                        ));
                    }
                }
            }
        }
    }

    // ── Path-based checks (NFS, SMB shares) ──

    if let Ok(nfs_shares) = state.nfs.list().await {
        for share in &nfs_shares {
            if share.path == *subvol_path || share.path.starts_with(&format!("{subvol_path}/")) {
                return Some(format!(
                    "subvolume is shared via NFS (path: {}). Delete the NFS share first.",
                    share.path
                ));
            }
        }
    }

    if let Ok(smb_shares) = state.smb.list().await {
        for share in &smb_shares {
            if share.path == *subvol_path || share.path.starts_with(&format!("{subvol_path}/")) {
                return Some(format!(
                    "subvolume is shared via SMB as '{}'. Delete the SMB share first.",
                    share.name
                ));
            }
        }
    }

    None
}

/// Check if a filesystem has any subvolumes with dependencies that would prevent destruction.
async fn check_filesystem_in_use(state: &AppState, name: &str) -> Option<String> {
    // Get all subvolumes on this filesystem
    let subvols = state
        .subvolumes
        .list_all(None, None)
        .await
        .unwrap_or_default();
    let fs_subvols: Vec<_> = subvols.iter().filter(|sv| sv.filesystem == name).collect();

    if fs_subvols.is_empty() {
        return None;
    }

    // Check each subvolume for dependencies
    for sv in &fs_subvols {
        if let Some(reason) = check_subvolume_in_use(state, name, &sv.name).await {
            return Some(format!(
                "filesystem '{}' cannot be destroyed: subvolume '{}' is in use — {}",
                name, sv.name, reason
            ));
        }
    }

    // Check if apps runtime uses this filesystem
    if state.apps.is_enabled() {
        let config = nasty_apps::AppsService::load_config();
        if let Some(ref path) = config.storage_path {
            if path.starts_with(&format!("/fs/{name}/")) {
                return Some(format!(
                    "filesystem '{}' cannot be destroyed: apps runtime storage is on this filesystem. Disable Apps first.",
                    name
                ));
            }
        }
    }

    None
}

// ── VM storage integration ──────────────────────────────────────

/// Resolve VM disk paths to filesystem/subvolume pairs by matching
/// against all block subvolumes' attached loop devices.
async fn resolve_vm_disks(
    state: &AppState,
    vm: &nasty_vm::VmConfig,
) -> Vec<nasty_vm::VmDiskSubvolume> {
    let all_subvols = state
        .subvolumes
        .list_all(None, None)
        .await
        .unwrap_or_default();
    let mut resolved = Vec::new();
    for disk in &vm.disks {
        for sv in &all_subvols {
            if let Some(ref bd) = sv.block_device {
                if bd == &disk.path {
                    resolved.push(nasty_vm::VmDiskSubvolume {
                        filesystem: sv.filesystem.clone(),
                        subvolume: sv.name.clone(),
                        device: disk.path.clone(),
                    });
                    break;
                }
            }
        }
    }
    resolved
}

/// Snapshot all block subvolumes belonging to a VM.
async fn vm_snapshot(
    state: &AppState,
    req: &nasty_vm::SnapshotVmRequest,
) -> Result<Vec<nasty_vm::VmDiskSubvolume>, String> {
    let vm_status = state.vms.get(&req.id).await.map_err(|e| e.to_string())?;
    let disks = resolve_vm_disks(state, &vm_status.config).await;

    if disks.is_empty() {
        return Err("no block subvolumes found for this VM".to_string());
    }

    // VM should ideally be stopped or paused for consistent snapshots
    if vm_status.running {
        // Send sync to guest via QMP if possible (best-effort)
        let _ = nasty_vm::qmp::execute(
            &format!("/run/nasty/vm/{}.qmp", req.id),
            "guest-fsfreeze-freeze",
            None,
        )
        .await;
    }

    for disk in &disks {
        let snap_req = nasty_storage::subvolume::CreateSnapshotRequest {
            filesystem: disk.filesystem.clone(),
            subvolume: disk.subvolume.clone(),
            name: req.name.clone(),
            read_only: Some(true),
        };
        state.snapshots.create(snap_req, None).await.map_err(|e| {
            format!(
                "failed to snapshot {}/{}: {e}",
                disk.filesystem, disk.subvolume
            )
        })?;
    }

    // Thaw if we froze
    if vm_status.running {
        let _ = nasty_vm::qmp::execute(
            &format!("/run/nasty/vm/{}.qmp", req.id),
            "guest-fsfreeze-thaw",
            None,
        )
        .await;
    }

    Ok(disks)
}

/// Clone a VM: create a new VM config with COW-cloned disk subvolumes.
async fn vm_clone(
    state: &AppState,
    req: &nasty_vm::CloneVmRequest,
) -> Result<nasty_vm::VmConfig, String> {
    let vm_status = state.vms.get(&req.id).await.map_err(|e| e.to_string())?;

    if vm_status.running {
        return Err("stop the VM before cloning".to_string());
    }

    let disks = resolve_vm_disks(state, &vm_status.config).await;

    // Clone each block subvolume
    let mut new_disks = Vec::new();
    for disk in &disks {
        let clone_name = format!("{}-{}", disk.subvolume, req.new_name);
        let clone_req = nasty_storage::subvolume::CloneSubvolumeRequest {
            filesystem: disk.filesystem.clone(),
            name: disk.subvolume.clone(),
            new_name: clone_name.clone(),
        };
        let cloned = state
            .subvolumes
            .clone_subvolume(clone_req, None)
            .await
            .map_err(|e| {
                format!(
                    "failed to clone {}/{}: {e}",
                    disk.filesystem, disk.subvolume
                )
            })?;

        new_disks.push(nasty_vm::VmDisk {
            path: cloned.block_device.unwrap_or_default(),
            interface: "virtio".to_string(),
            readonly: false,
            cache: None,
            aio: None,
            discard: None,
            iops_rd: None,
            iops_wr: None,
        });
    }

    // Create new VM config based on the source, with cloned disks
    let src = &vm_status.config;
    let create_req = nasty_vm::CreateVmRequest {
        name: req.new_name.clone(),
        cpus: Some(src.cpus),
        memory_mib: Some(src.memory_mib),
        disks: if new_disks.is_empty() {
            None
        } else {
            Some(new_disks)
        },
        networks: Some(src.networks.clone()),
        passthrough_devices: None, // Don't clone passthrough — can't share devices
        boot_iso: None,
        boot_order: Some(src.boot_order.clone()),
        uefi: Some(src.uefi),
        description: Some(format!("Clone of {}", src.name)),
        autostart: Some(false),
    };

    state
        .vms
        .create(create_req)
        .await
        .map_err(|e| e.to_string())
}

/// Read bcachefs error counters from sysfs. Returns total read+write error count.
async fn read_bcachefs_error_count(uuid: &str) -> u64 {
    let counters_dir = format!("/sys/fs/bcachefs/{uuid}/counters");
    let mut total = 0u64;
    for name in ["io_read_errors", "io_write_errors", "io_checksum_errors"] {
        let path = format!("{counters_dir}/{name}");
        if let Ok(val) = tokio::fs::read_to_string(&path).await {
            if let Ok(n) = val.trim().parse::<u64>() {
                total += n;
            }
        }
    }
    total
}
