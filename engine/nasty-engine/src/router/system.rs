//! RPC arms in the `system.*` domain. Extracted from the historical
//! 231-arm `match` in `router.rs`. Returns `Some(response)` when the
//! method matches, `None` when it falls through to another domain.

#![allow(unused_imports, unused_variables)]

use nasty_common::{ErrorCode, Request, Response};
use serde::Deserialize;

use super::*;
use crate::AppState;
use crate::auth::{Role, Session};

pub(super) async fn try_route(
    req: &Request,
    state: &AppState,
    session: &Session,
) -> Option<Response> {
    Some(match req.method.as_str() {
        "system.info" => ok(req, state.system.info().await),
        "system.health" => ok(req, state.system.health().await),
        "system.hardware.iommu" => ok(req, nasty_system::hardware::iommu_groups().await),
        "system.hardware.summary" => ok(req, nasty_system::hardware::system_summary().await),
        "system.secure_boot.readiness" => ok(req, nasty_system::secure_boot::readiness().await),
        "system.secure_boot.enrollment.status" => {
            ok(req, state.secure_boot_enrollment.status().await)
        }
        "system.secure_boot.enrollment.begin" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            match state.secure_boot_enrollment.begin(&session.username).await {
                Ok(s) => ok(req, s),
                Err(e) => err(req, e.to_string()),
            }
        }
        "system.secure_boot.enrollment.abort" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            let reason = str_param(req, "reason")
                .unwrap_or("operator aborted")
                .to_string();
            match state
                .secure_boot_enrollment
                .abort(&session.username, &reason)
                .await
            {
                Ok(s) => ok(req, s),
                Err(e) => err(req, e.to_string()),
            }
        }
        "system.secure_boot.enrollment.rebuild" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            match state.secure_boot_enrollment.rebuild().await {
                Ok(()) => ok(req, serde_json::json!({ "triggered": true })),
                Err(e) => err(req, e.to_string()),
            }
        }
        "system.secure_boot.enrollment.complete" => {
            if session.role != Role::Admin {
                return Some(err(req, "admin only".to_string()));
            }
            match state
                .secure_boot_enrollment
                .complete(&session.username)
                .await
            {
                Ok(s) => ok(req, s),
                Err(e) => err(req, e.to_string()),
            }
        }
        "system.passthrough.get" => ok(req, nasty_system::passthrough::load().await),
        "system.passthrough.update" => {
            match parse_params::<nasty_system::passthrough::PassthroughUpdate>(req) {
                Ok(p) => {
                    let mgmt = match session.client_ip.as_deref() {
                        Some(peer) => nasty_system::network::mgmt_iface_for_peer(peer).await,
                        None => None,
                    };
                    match nasty_system::passthrough::save_and_apply(p, mgmt.as_deref()).await {
                        Ok(cfg) => ok(req, cfg),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
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
            let lines: u32 = req
                .params
                .as_ref()
                .and_then(|p| p.get("lines"))
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as u32;
            let grep = req
                .params
                .as_ref()
                .and_then(|p| p.get("grep"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let lines_str = lines.to_string();
            let mut args = vec![
                "-u",
                unit,
                "-n",
                lines_str.as_str(),
                "--no-pager",
                "--output",
                "short-iso",
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
                "nasty-engine",
                "nasty-metrics",
                "caddy",
                "nfs-server",
                "samba-smbd",
                "samba-nmbd",
                "docker",
                "sshd",
                "avahi-daemon",
                "smartd",
                "nut-driver",
                "nut-server",
                "nut-monitor",
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
            ok(
                req,
                serde_json::json!({
                    "password_auth": password_auth,
                    "keys": keys,
                }),
            )
        }
        "system.ssh.add_key" => {
            let key = match require_str(req, "key") {
                Ok(k) => k.trim().to_string(),
                Err(r) => return Some(r),
            };
            if !key.starts_with("ssh-") && !key.starts_with("ecdsa-") {
                return Some(err(
                    req,
                    "Invalid SSH public key — must start with ssh-rsa, ssh-ed25519, etc.",
                ));
            }
            if let Err(e) = tokio::fs::create_dir_all("/root/.ssh").await {
                tracing::warn!("create_dir_all(/root/.ssh) failed: {e}");
            }
            let mut existing = tokio::fs::read_to_string("/root/.ssh/authorized_keys")
                .await
                .unwrap_or_default();
            if !existing.contains(&key) {
                if !existing.ends_with('\n') && !existing.is_empty() {
                    existing.push('\n');
                }
                existing.push_str(&key);
                existing.push('\n');
                if let Err(e) = tokio::fs::write("/root/.ssh/authorized_keys", &existing).await {
                    return Some(err(req, format!("write authorized_keys: {e}")));
                }
                // Set permissions. `try_run` logs failures so a chmod
                // that silently doesn't take effect (and would later
                // cause sshd to refuse the key) shows up in the journal.
                nasty_common::cmd::try_run("chmod", &["600", "/root/.ssh/authorized_keys"]).await;
                nasty_common::cmd::try_run("chmod", &["700", "/root/.ssh"]).await;
            }
            ok(req, "Key added")
        }
        "system.ssh.remove_key" => {
            let key = match require_str(req, "key") {
                Ok(k) => k.trim().to_string(),
                Err(r) => return Some(r),
            };
            let content = tokio::fs::read_to_string("/root/.ssh/authorized_keys")
                .await
                .unwrap_or_default();
            let filtered: String = content
                .lines()
                .filter(|l| l.trim() != key)
                .map(|l| format!("{l}\n"))
                .collect();
            if let Err(e) = tokio::fs::write("/root/.ssh/authorized_keys", &filtered).await {
                return Some(err(req, format!("write authorized_keys: {e}")));
            }
            ok(req, "Key removed")
        }
        "system.ssh.set_password_auth" => {
            let enabled = req
                .params
                .as_ref()
                .and_then(|p| p.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // Check that at least one key exists before disabling password auth
            if !enabled {
                let keys = tokio::fs::read_to_string("/root/.ssh/authorized_keys")
                    .await
                    .unwrap_or_default();
                let key_count = keys
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
                    .count();
                if key_count == 0 {
                    return Some(err(
                        req,
                        "Cannot disable password authentication without at least one SSH key — you would be locked out",
                    ));
                }
            }
            // Write sshd override file and reload — takes effect immediately
            // and survives reboots + rebuilds (NixOS sshd_config includes this file)
            let val = if enabled { "yes" } else { "no" };
            if let Err(e) = tokio::fs::write(
                "/var/lib/nasty/sshd_override.conf",
                format!("PasswordAuthentication {val}\n"),
            )
            .await
            {
                tracing::warn!("Failed to write sshd override: {e}");
            }
            nasty_common::cmd::try_run("systemctl", &["reload", "sshd"]).await;
            ok(
                req,
                format!(
                    "Password authentication {}",
                    if enabled { "enabled" } else { "disabled" }
                ),
            )
        }
        "system.network.get" => {
            let mgmt = match session.client_ip.as_deref() {
                Some(peer) => nasty_system::network::mgmt_iface_for_peer(peer).await,
                None => None,
            };
            ok(req, state.network.get(mgmt).await)
        }
        "system.network.update" => {
            match parse_params::<nasty_system::network::UpdateRequest>(req) {
                Ok(p) => {
                    // Resolve the management iface from the calling client's
                    // socket so the risk classifier knows what would
                    // disconnect the user.
                    let mgmt = match session.client_ip.as_deref() {
                        Some(peer) => nasty_system::network::mgmt_iface_for_peer(peer).await,
                        None => None,
                    };
                    match state.network.update(p, mgmt).await {
                        Ok(resp) => ok(req, resp),
                        Err(e) => err(req, e),
                    }
                }
                Err(e) => invalid(req, e),
            }
        }
        "system.network.confirm" => {
            match parse_params::<nasty_system::network::ConfirmRequest>(req) {
                Ok(p) => match state.network.confirm(&p.txn_id).await {
                    Ok(()) => ok(req, "ok"),
                    Err(e) => err(req, e),
                },
                Err(e) => invalid(req, e),
            }
        }
        "system.network.pending" => ok(req, state.network.pending().await),
        "system.network.nm_preview" => match state.network.nm_preview().await {
            Ok(diff) => ok(req, diff),
            Err(e) => err(req, e),
        },
        "system.network.nm_apply" => match state.network.nm_apply().await {
            Ok(outcome) => ok(req, outcome),
            Err(e) => err(req, e),
        },
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
            let offset = req
                .params
                .as_ref()
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
                .and_then(|r| r.error_for_status())
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
        "system.acme.retry" => match nasty_system::settings::retry_acme().await {
            Ok(()) => ok(req, "ok"),
            Err(e) => err(req, e),
        },
        // Per-host TLS automation status. Returns one entry per
        // managed hostname Caddy is tracking (from
        // `apps.tls.certificates.automate`), with state =
        // active / issuing / failed / pending and the latest log
        // message attached. Used by the /tls page's "Managed
        // certificates" section so the operator can see why a cert
        // is stuck instead of staring at an indefinite "pending".
        "system.tls.host_statuses" => {
            ok(req, nasty_system::settings::list_host_tls_statuses().await)
        }
        // Caddy's internal-CA root cert (PEM). The WebUI surfaces this
        // as a download so operators can import it into their OS or
        // browser trust store and stop getting fresh "untrusted cert"
        // warnings for every NASty box served via `tls internal`.
        // None ⇒ Caddy hasn't bootstrapped the CA yet (fresh box or
        // not running); return the engine-level error string so the
        // WebUI can render a clear "try again in a moment" message.
        "system.tls.local_ca_root" => {
            match nasty_system::settings::read_caddy_local_ca_root().await {
                Some(pem) => ok(req, pem),
                None => err(
                    req,
                    "Caddy local-CA root not available yet \
                 (Caddy may still be starting up). Try again in a moment."
                        .to_string(),
                ),
            }
        }
        "system.tuning.get" => ok(req, state.tuning.get().await),
        "system.tuning.update" => match parse_params(req) {
            Ok(p) => match state.tuning.update(p).await {
                Ok(v) => ok(req, v),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "system.nut.config.get" => ok(req, state.nut.get_config().await.redacted()),
        "system.nut.config.update" => match parse_params(req) {
            Ok(p) => match state.nut.update_config(p).await {
                Ok(v) => ok(req, v.redacted()),
                Err(e) => err(req, e),
            },
            Err(e) => invalid(req, e),
        },
        "system.nut.status" => ok(req, state.nut.status().await),
        "system.tailscale.get" => ok(req, state.tailscale.get().await),
        "system.tailscale.connect" => match parse_params(req) {
            Ok(p) => match state.tailscale.connect(p).await {
                Ok(v) => {
                    // Sync NVMe-oF ports for the new Tailscale IP.
                    // ensure_tailscale_ports() logs per-subsystem
                    // failures internally with warn!; the observer
                    // here logs the spawn-panic case so a missed
                    // port-sync isn't completely silent.
                    if let Some(ref ip) = v.ip {
                        let nvmeof = state.nvmeof.clone();
                        let ip = ip.clone();
                        let h = tokio::spawn(async move {
                            nvmeof.ensure_tailscale_ports(&ip).await;
                        });
                        tokio::spawn(async move {
                            if let Err(e) = h.await {
                                tracing::warn!(
                                    "tailscale-connect: ensure_tailscale_ports task panicked / cancelled: {e}"
                                );
                            }
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
                    let subsystems = match nvmeof.list().await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                "tailscale-disconnect cleanup: nvmeof.list() failed, \
                                 leaving any 100.x ports orphaned: {e}"
                            );
                            return;
                        }
                    };
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
        "system.update.build_dir.get" => ok(req, state.updates.get_update_build_dir().await),
        "system.update.build_dir.set" => match parse_params::<serde_json::Value>(req) {
            Ok(p) => {
                let path = p
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                match state.updates.set_update_build_dir(path).await {
                    Ok(v) => ok(req, v),
                    Err(e) => err(req, e),
                }
            }
            Err(e) => invalid(req, e),
        },
        "firmware.available" => ok(req, state.firmware.is_available().await),
        "firmware.constraints" => ok(req, state.firmware.constraints().await),
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
        "system.log.level" => {
            // Return the live filter so the WebUI's Log Level input can be
            // pre-populated with what's actually running. Without this the
            // input stays empty and shows a placeholder, which an operator
            // reasonably mistakes for the current value — and "Normal"
            // looks like it does something even when the running filter is
            // already the Normal preset.
            //
            // `with_current` returns Err only if the parent dispatcher has
            // been dropped — never happens in our lifecycle (the reload
            // handle outlives the dispatcher for the entire engine run),
            // so the Err arm is a safety surface, not an expected path.
            match state.log_reload.with_current(|f| f.to_string()) {
                Ok(s) => ok(req, s),
                Err(e) => err(req, format!("failed to read current filter: {e}")),
            }
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
        "system.firewall.status" => ok(req, state.firewall.status().await),
        "system.firewall.restrict" => {
            let service = match require_str(req, "service") {
                Ok(s) => s.to_string(),
                Err(r) => return Some(r),
            };
            let sources: Vec<String> = req
                .params
                .as_ref()
                .and_then(|p| p.get("sources"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let interfaces: Vec<String> = req
                .params
                .as_ref()
                .and_then(|p| p.get("interfaces"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            match state
                .firewall
                .set_restriction(&service, sources, interfaces)
                .await
            {
                Ok(()) => ok(req, "ok"),
                Err(e) => err(req, e),
            }
        }
        _ => return None,
    })
}
