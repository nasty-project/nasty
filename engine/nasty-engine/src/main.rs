use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Multipart, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::Deserialize;
use tracing::{error, info};
use tracing_subscriber::{prelude::*, reload};

mod app_deploy;
mod auth;
mod auth_oidc;
mod auth_webauthn;
mod boot_status;
mod fs_dependents;
mod fs_lock;
mod guestshare;
mod ingress_conflict;
mod log_stream;
mod registry;
mod rest_gateway;
mod router;
mod subvol_rollback;
mod subvolume_dependents;
mod swagger_ui;
mod telemetry;
mod terminal;
mod vm_console;
mod vm_disk_import;

use auth::{AuthService, EndpointAccess, Session};
use router::handle_rpc_request;

/// Handle for dynamically reloading the tracing filter at runtime.
pub type LogReloadHandle =
    reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

/// Broadcast channel for notifying all WebSocket clients of state changes.
/// The payload is the collection name (e.g. "filesystem", "subvolume", "share.nfs").
pub type EventBus = tokio::sync::broadcast::Sender<String>;

pub struct AppState {
    pub auth: AuthService,
    pub oidc: auth_oidc::OidcHolder,
    pub webauthn: auth_webauthn::WebauthnService,
    pub events: EventBus,
    pub log_reload: LogReloadHandle,
    pub system: nasty_system::SystemService,
    pub settings: nasty_system::settings::SettingsService,
    /// Secure Boot enrollment ceremony state (ADR #324). Service
    /// is stateful — survives engine restarts via a small JSON file
    /// at /var/lib/nasty/secure-boot-enrollment.json — and auto-
    /// detects the SB transition on startup (`bootctl status` flips
    /// from disabled to enabled across a reboot ⇒ phase advances
    /// to PostEnrollment).
    pub secure_boot_enrollment: nasty_system::secure_boot_enrollment::SecureBootEnrollmentService,
    pub tuning: nasty_system::tuning::TuningService,
    pub nut: nasty_system::nut::NutService,
    pub alerts: nasty_system::alerts::AlertService,
    pub network: nasty_system::network::NetworkService,
    pub protocols: nasty_system::protocol::ProtocolService,
    pub firewall: nasty_system::firewall::FirewallService,
    pub updates: nasty_system::update::UpdateService,
    pub tailscale: nasty_system::tailscale::TailscaleService,
    pub metrics_client: reqwest::Client,
    pub filesystems: nasty_storage::FilesystemService,
    /// Filesystems that failed to mount on startup (persistent alert source).
    pub mount_failures: tokio::sync::Mutex<Vec<String>>,
    pub subvolumes: Arc<nasty_storage::SubvolumeService>,
    pub snapshots: nasty_snapshot::SnapshotService,
    pub nfs: nasty_sharing::NfsService,
    pub guest_shares: guestshare::GuestShareService,
    pub smb: nasty_sharing::SmbService,
    pub domain: nasty_system::domain::DomainService,
    pub dc: nasty_system::dc::DcService,
    pub iscsi: nasty_sharing::IscsiService,
    pub nvmeof: Arc<nasty_sharing::NvmeofService>,
    pub vms: nasty_vm::VmService,
    pub apps: nasty_apps::AppsService,
    pub backups: nasty_backup::BackupService,
    pub firmware: nasty_system::firmware::FirmwareService,
    /// Cached alerts result (timestamp, json value). Avoids re-evaluating
    /// all alert checks on every WebUI poll (called every few seconds).
    pub alerts_cache: tokio::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>>,
    /// Short-TTL cache for the aggregated `system.status` band (#528) — the
    /// sidebar polls it frequently and each miss re-scans per-fs scrub /
    /// reconcile / device state.
    pub status_cache: tokio::sync::Mutex<Option<(std::time::Instant, nasty_system::SystemStatus)>>,
    /// Boot-time per-phase status. Populated by main()'s restoration
    /// sequence; read by `/api/boot_status` so the WebUI can render
    /// a TrueNAS-style "starting up" overlay on the login screen
    /// while phases are still running. See #299.
    pub boot_status: boot_status::BootStatusTracker,
}

/// Base URL for the nasty-metrics service.
pub const METRICS_BASE: &str = "http://127.0.0.1:2138";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let built = env!("NASTY_BUILD_DATE");
    let args = std::env::args().collect::<Vec<_>>();

    // --version flag. Includes the git commit so it can be matched
    // against a deployed branch/commit — the version alone (0.0.10 on
    // every branch) can't tell two builds apart.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        match telemetry::build_commit() {
            Some(commit) => println!("nasty-engine {version} ({commit}, built: {built})"),
            None => println!("nasty-engine {version} (built: {built})"),
        }
        return Ok(());
    }

    if matches!(
        args.get(1).map(String::as_str),
        Some("bootstrap-system-flake")
    ) {
        run_bootstrap_system_flake_cli(&args[2..]).await?;
        return Ok(());
    }

    // Docs generator. Replaces the standalone `nasty-apidoc` binary —
    // writes docs/api.md straight out of the in-engine method registry, so
    // the docs and the dispatcher live in the same crate and stop drifting.
    if matches!(args.get(1).map(String::as_str), Some("--dump-docs")) {
        let out_dir = args
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("--dump-docs requires an output directory argument"))?;
        let (_g, groups) = registry::build_full_registry();
        std::fs::create_dir_all(out_dir)?;

        let md = registry::render_markdown(&groups);
        let md_path = std::path::Path::new(out_dir).join("api.md");
        std::fs::write(&md_path, &md)?;
        println!("Written {} ({} bytes)", md_path.display(), md.len());

        let openapi = registry::render_openapi(version, &groups);
        let openapi_text = serde_json::to_string_pretty(&openapi)?;
        let openapi_path = std::path::Path::new(out_dir).join("openapi.json");
        std::fs::write(&openapi_path, &openapi_text)?;
        println!(
            "Written {} ({} bytes)",
            openapi_path.display(),
            openapi_text.len()
        );
        return Ok(());
    }

    let default_filter = "nasty_engine=debug,nasty_storage=debug,nasty_sharing=debug,nasty_snapshot=debug,nasty_system=info,tower_http=debug";
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    let (filter_layer, reload_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (event_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    let subvolumes = Arc::new(nasty_storage::SubvolumeService::new(
        nasty_storage::FilesystemService::new(),
    ));
    let nvmeof = Arc::new(nasty_sharing::NvmeofService::new());

    // Settings service is built before AppState so we can derive the
    // WebAuthn RP ID from the operator's configured `tls_domain` (same
    // hostname Caddy issues certs for). Changes to tls_domain after
    // boot don't propagate to a running engine's WebauthnService —
    // doing so would void every registered credential (RP ID is part
    // of each credential's signed binding). Operators who rename their
    // domain re-register their security keys; PR #3 will surface this
    // explicitly in the settings UI.
    let settings_service = nasty_system::settings::SettingsService::new().await;
    let webauthn_rp_id = {
        let s = settings_service.get().await;
        auth_webauthn::WebauthnService::rp_id_from_settings(&s)
    };
    let webauthn = match auth_webauthn::WebauthnService::new(&webauthn_rp_id) {
        Ok(w) => w,
        Err(e) => {
            error!(
                "Failed to construct WebauthnService for RP ID '{webauthn_rp_id}': {e}; \
                 falling back to default '{}'",
                auth_webauthn::DEFAULT_RP_ID
            );
            auth_webauthn::WebauthnService::new(auth_webauthn::DEFAULT_RP_ID)
                .expect("default RP ID must always construct")
        }
    };

    let state = Arc::new(AppState {
        auth: AuthService::new().await,
        oidc: auth_oidc::OidcHolder::default(),
        webauthn,
        events: event_tx,
        log_reload: reload_handle,
        system: nasty_system::SystemService::new(None, Some(built.to_string())),
        settings: settings_service,
        secure_boot_enrollment:
            nasty_system::secure_boot_enrollment::SecureBootEnrollmentService::new().await,
        tuning: nasty_system::tuning::TuningService::new().await,
        nut: nasty_system::nut::NutService::new().await,
        alerts: nasty_system::alerts::AlertService::new().await,
        network: nasty_system::network::NetworkService::new(),
        protocols: nasty_system::protocol::ProtocolService::new(),
        firewall: nasty_system::firewall::FirewallService::new(),
        updates: nasty_system::update::UpdateService::new(),
        tailscale: nasty_system::tailscale::TailscaleService::new().await,
        metrics_client: reqwest::Client::new(),
        filesystems: nasty_storage::FilesystemService::new(),
        mount_failures: tokio::sync::Mutex::new(Vec::new()),
        snapshots: nasty_snapshot::SnapshotService::new(subvolumes.clone()),
        subvolumes,
        nfs: nasty_sharing::NfsService::new(),
        guest_shares: guestshare::GuestShareService::new(),
        smb: nasty_sharing::SmbService::new(),
        domain: nasty_system::domain::DomainService::new(),
        dc: nasty_system::dc::DcService::new(),
        iscsi: nasty_sharing::IscsiService::new(),
        nvmeof,
        vms: nasty_vm::VmService::new(),
        apps: nasty_apps::AppsService::new(),
        backups: nasty_backup::BackupService::new(),
        firmware: nasty_system::firmware::FirmwareService::new(),
        alerts_cache: tokio::sync::Mutex::new(None),
        status_cache: tokio::sync::Mutex::new(None),
        // Pre-register every phase name we'll feed into `run_phase`
        // below. Pre-registering makes the WebUI's checklist fully
        // visible from the moment the engine starts answering
        // /api/boot_status — rows transition Pending → Running →
        // Ok/Failed in place, no popping-into-existence reflow.
        // Keep this list in sync with the `run_phase` calls below.
        boot_status: boot_status::BootStatusTracker::new(&[
            "filesystems.restore_mounts",
            "subvolumes.restore_block_devices",
            "nvmeof.remap_device_paths",
            "iscsi.remap_device_paths",
            "protocols.restore",
            "smb.scaffold_config",
            "domain.restore",
            "dc.restore",
            "nvmeof.restore",
            "vms.restore",
            "apps.restore",
            "tailscale.restore",
            "network.restore_pending_revert",
            "network.reconcile_orphans",
            "subvolumes.reconcile_project_ids",
            "apps.reconcile_app_routes",
            "apps.reconcile_networks",
            "backups.migrate_secrets",
            "nut.migrate_secrets",
            "oidc.migrate_secrets",
            "iscsi.migrate_secrets",
            "notifications.migrate_secrets",
            "firewall.init",
            "nvmeof.ensure_tailscale_ports",
            "caches.warm",
        ]),
    });

    // Restore state from previous session:
    // 1. Mount filesystems tracked in fs-state.json
    // 2. Re-attach loop devices for block subvolumes
    // 3. Start enabled protocols (services + kernel modules)
    // 4. Restore NVMe-oF configfs (volatile, needs modules from step 3)
    //
    // Every restoration step gets a wall-clock budget via `run_phase`
    // so a single misbehaving subsystem (a hung mount, a slow Tailscale
    // login, an unreachable Docker socket) can't take the whole engine
    // down via systemd's `TimeoutStartSec`. See #299 for the design.
    //
    // Budgets are per-phase, sized for the worst realistic case
    // (many filesystems, many VMs/apps, big subvolume tree) rather
    // than the median, with generous headroom above that. If the
    // sum of every phase hitting its ceiling exceeds systemd's
    // default `TimeoutStartSec` (90 s), the engine still reaches
    // READY because each phase fires its own timeout independently
    // and we keep going — but the actual common-case boot is in
    // seconds and the ceilings are never reached.
    use std::time::Duration;
    let secs = Duration::from_secs;

    let mount_failures = state
        .boot_status
        .run_phase(
            "filesystems.restore_mounts",
            secs(300), // per-FS 60s device-wait × multi-disk pools, plus mount + possible fsck
            state.filesystems.restore_mounts(),
        )
        .await;
    if !mount_failures.is_empty() {
        error!(
            "CRITICAL: {} filesystem(s) failed to mount: {}",
            mount_failures.len(),
            mount_failures.join(", ")
        );
        *state.mount_failures.lock().await = mount_failures;
    }
    // Re-attach loop devices and get the current name→device mapping.
    // Loop device numbers change across reboots, so NVMe-oF and iSCSI state
    // files must be patched before their respective restore steps run.
    let dev_map = state
        .boot_status
        .run_phase(
            "subvolumes.restore_block_devices",
            secs(30), // losetup per block subvolume + state-file write
            state.subvolumes.restore_block_devices(),
        )
        .await;
    if !dev_map.is_empty() {
        state
            .boot_status
            .run_phase(
                "nvmeof.remap_device_paths",
                secs(15), // string substitution + state-file save
                state.nvmeof.remap_device_paths(&dev_map),
            )
            .await;
        state
            .boot_status
            .run_phase(
                "iscsi.remap_device_paths",
                secs(15),
                state.iscsi.remap_device_paths(&dev_map),
            )
            .await;
    }
    state
        .boot_status
        .run_phase(
            "protocols.restore",
            secs(90), // 9 systemd services × up to ~10s each on a bursty box
            state.protocols.restore(),
        )
        .await;

    // Rebuild the smb.nasty.conf include chain before anything AD-related
    // runs. tmpfiles ships that file empty and only a share mutation ever
    // rewrote it, so fresh and upgraded boxes carried no
    // `include = /etc/samba/nasty-domain.conf` — the domain join below (and
    // winbindd) would see no ADS globals. The rebuild is idempotent, so this
    // is safe on every boot; it must run BEFORE domain.restore.
    state
        .boot_status
        .run_phase("smb.scaffold_config", secs(15), {
            let state = state.clone();
            async move {
                match state.smb.ensure_config_scaffolding().await {
                    Ok(()) => {
                        tracing::info!("Rebuilt /etc/samba/smb.nasty.conf include chain at boot");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to rebuild smb.nasty.conf include chain: {e}");
                    }
                }
            }
        })
        .await;

    // If we're joined to an Active Directory domain, make sure winbindd is
    // running — `domain.join` already starts it, but a plain reboot doesn't
    // go through that path.
    state
        .boot_status
        .run_phase("domain.restore", secs(15), {
            let state = state.clone();
            async move {
                if state.domain.is_joined().await {
                    state.domain.ensure_winbindd().await;
                }
            }
        })
        .await;

    // If this box hosts an AD domain, bring the DC back up: rewrite the
    // /run resolved drop-in (tmpfs — empty after reboot) and start
    // samba-dc (Conflicts= swaps member-mode samba out). Must run after
    // the smb.nasty.conf reconcile above — the DC config includes it.
    // The DC firewall opens later, in firewall.init: at this point in
    // boot `state.rules` holds nothing yet, so opening here would
    // rebuild the nftables table before the base (webui/ssh) rules
    // exist, locking management out until firewall.init runs.
    state
        .boot_status
        .run_phase("dc.restore", secs(30), {
            let state = state.clone();
            async move {
                state.dc.ensure_running().await;
            }
        })
        .await;

    // SSH password auth is managed via /var/lib/nasty/sshd_override.conf
    // (created by tmpfiles with default "yes", toggled by the WebUI).

    state
        .boot_status
        .run_phase(
            "nvmeof.restore",
            // configfs writes are fast, but restore first waits (up to 45s)
            // for specific port addresses to come up so binds don't race the
            // network and fail with EADDRNOTAVAIL (#625). Budget covers the
            // wait plus the writes.
            secs(75),
            state.nvmeof.restore(),
        )
        .await;
    state
        .boot_status
        .run_phase(
            "vms.restore",
            secs(300), // 10-20s per autostart VM × N
            state.vms.restore(),
        )
        .await;
    state
        .boot_status
        .run_phase(
            "apps.restore",
            secs(300), // `docker compose up -d` per autostart app, may pull layers
            state.apps.restore(),
        )
        .await;
    state
        .boot_status
        .run_phase(
            "tailscale.restore",
            secs(60), // network round-trip to login.tailscale.com
            state.tailscale.restore(),
        )
        .await;

    // If the engine was killed mid-apply (or restarted before the user
    // confirmed a risky network change), restore the prior config from
    // /var/lib/nasty/networking.json.pending-revert. No-op if the file
    // doesn't exist. Runs before the HTTP server starts accepting calls
    // so a confirm can't race the rollback.
    state
        .boot_status
        .run_phase(
            "network.restore_pending_revert",
            secs(60), // file check is instant; revert path runs nmcli
            state.network.restore_pending_revert(),
        )
        .await;

    // Idempotent sweep: drop networking.json `interfaces[]` entries
    // that no longer correspond to any live device or virtual
    // master, and the matching `nasty-*` NM connection profiles.
    // Happens automatically on every boot — needed because the
    // kernel can rename devices across reboots (Mellanox multi-port
    // adapters: `enp6s0f0` → `enp6s0f0np0`), and because the
    // engine's apply path doesn't garbage-collect dead profiles
    // otherwise.  Runs before firewall.init so the firewall mirrors
    // the cleaned-on-disk state.
    state
        .boot_status
        .run_phase(
            "network.reconcile_orphans",
            secs(30), // a handful of NM connection deletes at most
            state.network.reconcile_orphans(),
        )
        .await;

    // Backfill project quota IDs on filesystem subvolumes that
    // predate the always-assign change (#176). Without this, those
    // subvolumes have no repquota row, so their `used_bytes` stays
    // `None` and the WebUI shows `—` forever. Idempotent: scans
    // repquota output and only writes for subvolumes that lack a
    // row. Best-effort; failures are logged and don't block startup.
    state
        .boot_status
        .run_phase(
            "subvolumes.reconcile_project_ids",
            secs(90), // repquota scan + setproject per subvolume, scales with subvol count
            state.subvolumes.reconcile_project_ids(),
        )
        .await;

    // Encrypt any backup-profile secrets still on disk in plaintext
    // (legacy state from before nasty-common::secrets landed). Walks
    // every profile, attempts to seal the password + S3/B2 cloud keys
    // via systemd-creds, persists the resulting blobs. Idempotent —
    // profiles whose secrets are already encrypted are skipped. No-op
    // on hosts where systemd-creds is unavailable (warns once per
    // profile, leaves plaintext in place so backups keep working).
    state
        .boot_status
        .run_phase(
            "backups.migrate_secrets",
            secs(30), // a handful of profiles, each two systemd-creds shellouts
            state.backups.migrate_secrets(),
        )
        .await;

    // Seal a plaintext NUT remote-server password left on disk from
    // before encrypt-at-rest. One config, one systemd-creds shellout;
    // no-op when empty / already sealed / backend unavailable.
    state
        .boot_status
        .run_phase("nut.migrate_secrets", secs(15), state.nut.migrate_secrets())
        .await;

    // Seal a plaintext OIDC client_secret left in settings.json from
    // before encrypt-at-rest. One config, one systemd-creds shellout;
    // no-op when empty / already sealed / backend unavailable.
    state
        .boot_status
        .run_phase(
            "oidc.migrate_secrets",
            secs(15),
            state.settings.migrate_secrets(),
        )
        .await;

    // Seal plaintext iSCSI CHAP passwords left in per-target state files
    // from before encrypt-at-rest. A shellout per ACL with a secret;
    // no-op when empty / already sealed / backend unavailable.
    state
        .boot_status
        .run_phase(
            "iscsi.migrate_secrets",
            secs(30),
            state.iscsi.migrate_secrets(),
        )
        .await;

    // Seal plaintext notification-channel secrets (SMTP password,
    // Telegram bot token, webhook signing secret, ntfy token) left in
    // notifications.json from before encrypt-at-rest. A shellout per
    // secret; no-op when empty / already sealed / backend unavailable.
    state
        .boot_status
        .run_phase(
            "notifications.migrate_secrets",
            secs(30),
            nasty_system::notifications::NotificationConfig::migrate_secrets(),
        )
        .await;

    // Push the engine-known set of app ingresses into Caddy's
    // admin-API config.  Caddy holds these in memory, so a fresh
    // boot or `systemctl restart caddy` would otherwise drop every
    // `/apps/<name>/` route.
    state
        .boot_status
        .run_phase(
            "apps.reconcile_app_routes",
            secs(30), // localhost HTTP to Caddy admin :2019
            state.apps.reconcile_app_routes(),
        )
        .await;

    // Recreate NASty-managed Docker networks missing from Docker (e.g.
    // after a fresh data-root); skips any whose parent interface vanished.
    {
        let ifaces = system_network_ifaces(&state).await;
        state
            .boot_status
            .run_phase(
                "apps.reconcile_networks",
                secs(30),
                state.apps.reconcile_networks(ifaces),
            )
            .await;
    }

    // TLS automation reconcile — push the policy set (main domain +
    // every app subdomain) so Caddy issues certs after a fresh boot or
    // restart, the same way ingress routes get re-pushed above.
    //
    // Spawned so a failing admin-API call (Caddy slow to start, ACME
    // server unreachable) doesn't block the engine from finishing
    // startup. The static-cert :443 block in the Caddyfile serves a
    // valid cert in the meantime; users keep working while Caddy
    // catches up.
    tokio::spawn(async {
        nasty_system::settings::reapply_tls_from_disk().await;
    });

    // Initialize firewall based on current protocol states
    state
        .boot_status
        .run_phase("firewall.init", secs(15), {
            let state = state.clone();
            async move {
                use nasty_system::protocol::Protocol;
                let mut proto_states = Vec::new();
                for p in Protocol::ALL {
                    let enabled = state.protocols.is_enabled(*p).await;
                    proto_states.push((*p, enabled));
                }
                state.firewall.init(&proto_states).await;
                // RDMA transports are a per-box opt-in orthogonal to the
                // protocol list; restore its firewall rule when enabled.
                if nasty_system::rdma::enabled().await {
                    state.firewall.open_rdma().await;
                }
                // DC role (#20): open the AD service ports once the base rules exist.
                // dc.restore (earlier) only restarts the service + DNS drop-in — opening
                // the firewall there would rebuild the table before webui/ssh rules are
                // in state, locking management out until this point.
                if nasty_system::dc::DcService::load_config().await.is_some() {
                    state.firewall.open_dc().await;
                }
                // iSCSI/NVMe-oF rules follow configured portal ports
                // (#602); replace the static defaults with the real
                // port sets from restored targets.
                router::share::sync_portal_firewall_ports(&state).await;
            }
        })
        .await;

    // Sync NVMe-oF ports with Tailscale IP (if Tailscale reconnected on boot)
    state
        .boot_status
        .run_phase("nvmeof.ensure_tailscale_ports", secs(15), {
            let state = state.clone();
            async move {
                let ts_status = state.tailscale.get().await;
                if ts_status.connected
                    && let Some(ref ip) = ts_status.ip
                {
                    state.nvmeof.ensure_tailscale_ports(ip).await;
                }
            }
        })
        .await;

    // Pre-warm caches so first page loads are fast.
    // Runs before sd_notify_ready() — Caddy won't serve until this completes.
    info!("Warming caches...");
    state
        .boot_status
        .run_phase("caches.warm", secs(30), {
            let state = state.clone();
            async move {
                state.system.info().await;
            }
        })
        .await;

    // Seed the cached ACME status from whatever cert Caddy is already
    // serving so the WebUI shows the issuer/expiry on first page load
    // instead of after the user clicks anything. Renewal itself runs
    // inside Caddy now — no daily cron from us, so no long-running
    // loop to wrap with the observer pattern used elsewhere.
    tokio::spawn(async {
        nasty_system::settings::check_acme_renewal().await;
    });

    // Build the OIDC client if SSO is configured. Failures are logged, not
    // fatal — a misconfigured IdP shouldn't block the engine from starting,
    // and admins can fix the config via the WebUI.
    //
    // Spawned (not awaited) because OIDC discovery fires an HTTP request
    // at the IdP, and a slow / unreachable IdP would otherwise block
    // startup for the full reqwest connect timeout (~75 s by default) —
    // long enough to push past systemd's TimeoutStartSec. The SSO button
    // on the login page is gated on the rebuild completing, so the
    // visible effect of spawning is "button appears a moment late" rather
    // than "engine stuck on Type=notify ready for over a minute."
    {
        let oidc_settings = state.settings.get().await.oidc;
        if oidc_settings.enabled {
            let oidc_holder = state.oidc.clone();
            tokio::spawn(async move {
                match oidc_holder.rebuild(&oidc_settings).await {
                    Ok(()) => info!(
                        "OIDC client initialized (issuer={:?})",
                        oidc_settings.issuer_url
                    ),
                    Err(e) => tracing::warn!("OIDC client init failed at startup: {e}"),
                }
            });
        }
    }

    // Start daily anonymous telemetry (if not opted out)
    telemetry::spawn_daily(state.clone());

    // Background alert evaluation + notifications
    spawn_alert_notifier(state.clone());

    // Cron-driven backup scheduler. Polls profile list every 60s;
    // when an enabled profile's cron expression elapses since its
    // last attempt, fires run_backup as a tokio::spawn so a slow
    // backup on one profile doesn't block the scheduler advancing
    // the others. Missed runs during engine downtime are NOT caught
    // up — see the module docs in nasty_backup::scheduler for why.
    {
        let backups = state.backups.clone_for_task();
        tokio::spawn(async move {
            nasty_backup::scheduler::run_scheduler_loop(backups).await;
        });
    }

    // Flip boot_status.overall from Booting → Ready / ReadyWithErrors
    // BEFORE notifying systemd — once we're READY the WebUI is going
    // to start polling /api/boot_status and we want it to immediately
    // see the post-boot state, not catch the snapshot mid-transition.
    state.boot_status.mark_ready().await;

    // Signal systemd that startup is complete
    sd_notify_ready();

    let ws_routes = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/terminal", get(terminal::terminal_handler))
        .route("/ws/apps/deploy", get(app_deploy::deploy_handler))
        .route(
            "/ws/vm/disk-import",
            get(vm_disk_import::disk_import_handler),
        )
        .route("/ws/system/logs", get(log_stream::logs_handler))
        .route("/ws/vm/{vm_id}/vnc", get(vm_console::vnc_handler))
        .route("/ws/vm/{vm_id}/serial", get(vm_console::serial_handler))
        .layer(axum::middleware::from_fn(ws_origin_check));

    let app = Router::new()
        .merge(ws_routes)
        .merge(rest_gateway::routes())
        .merge(swagger_ui::routes())
        .route("/api/openapi.json", get(openapi_handler))
        .route("/api/login", post(login_handler))
        .route("/api/logout", post(logout_handler))
        .route(
            "/api/auth/webauthn/login/start",
            post(webauthn_login_start_handler),
        )
        .route(
            "/api/auth/webauthn/login/finish",
            post(webauthn_login_finish_handler),
        )
        .route("/api/auth/oidc/available", get(oidc_available_handler))
        .route(
            "/api/auth/webauthn/available",
            get(webauthn_available_handler),
        )
        .route("/api/boot_status", get(boot_status_handler))
        // Public guest-share access (#474) — unauthenticated by design; the
        // URL token + optional unlock grant are the only credentials.
        .route("/api/public/share/{token}", get(public_share_meta_handler))
        .route(
            "/api/public/share/{token}/unlock",
            post(public_share_unlock_handler),
        )
        .route(
            "/api/public/share/{token}/download",
            get(public_share_download_handler),
        )
        .route(
            "/api/public/share/{token}/zip",
            get(public_share_zip_handler),
        )
        .route("/api/auth/oidc/start", get(oidc_start_handler))
        .route("/api/auth/oidc/callback", get(oidc_callback_handler))
        .route(
            "/api/upload/vm-image",
            post(upload_vm_image_handler).layer(DefaultBodyLimit::max(10_737_418_240)),
        )
        .route("/api/files/browse", get(files_browse_handler))
        .route("/api/files/size", get(files_size_handler))
        .route("/api/files", delete(files_delete_handler))
        .route(
            "/api/files/upload",
            post(files_upload_handler).layer(DefaultBodyLimit::max(10_737_418_240)),
        )
        .route("/api/files/mkdir", post(files_mkdir_handler))
        .route("/api/files/rename", post(files_rename_handler))
        .route("/api/files/copy", post(files_copy_handler))
        .route("/api/files/restore", post(files_restore_handler))
        .route(
            "/api/files/content",
            get(files_content_handler)
                // 10 MiB cap on edit-in-place writes. The Files page surfaces an
                // edit affordance only for textual files (conf, yml, md, …) where
                // hand-editing past a megabyte is already a smell; using upload
                // for bigger blobs keeps the small fast-path small.
                .put(files_content_put_handler)
                .layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route("/api/auth/check", get(auth_check_handler))
        .route("/health", get(health))
        .with_state(state);

    // 127.0.0.1 only — Caddy proxies https://nas:443/ → http://127.0.0.1:2137/.
    // Direct LAN access to the engine port would bypass TLS, the security
    // headers, and the reverse-proxy X-Real-IP plumbing the session-IP-binding
    // depends on (Caddy sets X-Real-IP from `{remote_host}` on every /api/* and
    // /ws handler — see services.caddy in nasty.nix).
    let addr = SocketAddr::from(([127, 0, 0, 1], 2137));
    info!("NASty Engine v{version} (built: {built})");
    info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Host interface facts for the apps Docker-network validator: each live
/// interface's name + kind, plus whether it's enslaved to a bridge (which
/// makes it an invalid macvlan/ipvlan parent — the bridge should be used).
pub(crate) async fn system_network_ifaces(state: &AppState) -> Vec<nasty_apps::IfaceInfo> {
    let st = state.network.get(None).await;
    let members: std::collections::HashSet<String> = st
        .config
        .bridges
        .iter()
        .flat_map(|b| b.members.iter().cloned())
        .collect();
    st.interfaces
        .into_iter()
        .map(|i| nasty_apps::IfaceInfo {
            bridge_member: members.contains(&i.name),
            name: i.name,
            kind: i.kind,
        })
        .collect()
}

/// Add a host-side macvlan shim for a macvlan Docker network (#448) so the
/// host can reach containers on it. Threads a `MacvlanConfig` into the
/// operator's `NetworkConfig` and applies it via the existing network rail
/// (which rolls back risky changes; a non-mgmt-parent shim is classified
/// safe so it applies without a confirm timer). Refuses when the parent
/// already carries a host IP in the container subnet (route conflict).
pub(crate) async fn add_macvlan_shim(
    state: &AppState,
    spec: &nasty_apps::ManagedNetwork,
    mgmt: Option<&str>,
) -> Result<(), String> {
    use nasty_system::network::{IpConfig, IpMethod, MacvlanConfig, UpdateRequest};
    let parent = spec
        .parent
        .as_deref()
        .ok_or("macvlan network has no parent")?;
    let subnet = spec
        .subnet
        .as_deref()
        .ok_or("host shim requires the network to have a subnet")?;
    let shim_ip = spec
        .shim_ip
        .as_deref()
        .ok_or("host shim requires a shim_ip")?;
    // The kernel parent matches Docker's (parent.<vlan> when tagged).
    let eff_parent = match spec.vlan {
        Some(v) => format!("{parent}.{v}"),
        None => parent.to_string(),
    };
    let shim_name = format!("shim-{}", spec.name);
    if shim_name.len() > 15 {
        return Err(format!(
            "network name too long for a shim interface ('{shim_name}' exceeds 15 chars)"
        ));
    }

    let st = state.network.get(mgmt.map(|s| s.to_string())).await;
    // Route-conflict guard: a host already on the container subnet via the
    // parent would get a second on-link route through the shim.
    if let Some(iface) = st.interfaces.iter().find(|i| i.name == eff_parent) {
        for a in &iface.ipv4_addresses {
            let ip = a.split('/').next().unwrap_or(a);
            if nasty_apps::cidr_contains_ip(subnet, ip) == Some(true) {
                return Err(format!(
                    "parent '{eff_parent}' already has an address ({a}) in {subnet}; a host shim \
                     would create a conflicting route — give the macvlan network a dedicated subnet"
                ));
            }
        }
    }

    let route = spec.ip_range.clone().unwrap_or_else(|| subnet.to_string());
    let mut config = st.config.clone();
    config.macvlans.retain(|m| m.name != shim_name);
    config.macvlans.push(MacvlanConfig {
        name: shim_name.clone(),
        parent: eff_parent,
        mode: "bridge".to_string(),
        ipv4: IpConfig {
            method: IpMethod::Static,
            addresses: vec![shim_ip.to_string()],
            gateway: None,
        },
        mtu: None,
        routes: vec![route],
    });
    let resp = state
        .network
        .update(
            UpdateRequest {
                config,
                confirm_within_secs: Some(0),
            },
            mgmt.map(|s| s.to_string()),
        )
        .await?;
    if !resp.apply_errors.is_empty() {
        return Err(format!(
            "network apply reported errors creating the shim: {:?}",
            resp.apply_errors
        ));
    }
    Ok(())
}

/// Remove the host macvlan shim for a Docker network (best-effort).
pub(crate) async fn remove_macvlan_shim(state: &AppState, net_name: &str) -> Result<(), String> {
    use nasty_system::network::UpdateRequest;
    let shim_name = format!("shim-{net_name}");
    let st = state.network.get(None).await;
    if !st.config.macvlans.iter().any(|m| m.name == shim_name) {
        return Ok(()); // nothing to remove
    }
    let mut config = st.config.clone();
    config.macvlans.retain(|m| m.name != shim_name);
    state
        .network
        .update(
            UpdateRequest {
                config,
                confirm_within_secs: Some(0),
            },
            None,
        )
        .await
        .map(|_| ())
}

async fn run_bootstrap_system_flake_cli(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: nasty-engine bootstrap-system-flake --dest-dir <dir> --template-file <path> --system <system>"
        );
        return Ok(());
    }

    let dest_dir = required_flag_value(args, "--dest-dir")?;
    let template_file = required_flag_value(args, "--template-file")?;
    let local_system = required_flag_value(args, "--system")?;
    let nasty_version = env!("CARGO_PKG_VERSION");

    let result = nasty_system::update::bootstrap_system_flake_from_template_path(
        &template_file,
        &dest_dir,
        nasty_version,
        &local_system,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    println!("{}", result.flake_path);
    Ok(())
}

fn required_flag_value(args: &[String], flag: &str) -> anyhow::Result<String> {
    let idx = args
        .iter()
        .position(|arg| arg == flag)
        .ok_or_else(|| anyhow::anyhow!("missing required flag: {flag}"))?;
    args.get(idx + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing value for flag: {flag}"))
}

/// Notify systemd that the service is ready (Type=notify).
fn sd_notify_ready() {
    let Some(sock_path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let sock = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = sock.send_to(b"READY=1", &sock_path);
    info!("Notified systemd: READY");
}

/// Boot-status snapshot, unauthenticated. Surfaces what the
/// `boot_status::BootStatusTracker` recorded for every restoration
/// phase + an overall state the WebUI uses to decide between
/// rendering the boot overlay or the login form.
///
/// No auth on purpose: the operator can't authenticate yet when the
/// engine is mid-boot, and we still want them to see *what's*
/// holding things up. The snapshot only exposes phase names,
/// timestamps relative to process start, and tokio-timeout error
/// strings — no secrets, no per-user data.
async fn boot_status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.boot_status.snapshot().await)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        // The git commit is the only field that distinguishes two builds
        // of the same version (every branch on 0.0.10 reports version
        // 0.0.10) — so a deploy can poll /health and confirm the running
        // engine is the exact commit it shipped. `null` on cargo builds
        // with no SHA available.
        "commit": telemetry::build_commit(),
        "built": env!("NASTY_BUILD_DATE"),
    }))
}

// ── VM Image Upload ────────────────────────────────────────────────

async fn upload_vm_image_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::RootEquivalent,
        "upload.vm_image",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };
    let session = authenticated.session;
    let client_ip = authenticated.client_ip;

    info!("VM image upload request from {}", client_ip);

    // Get or create the images subvolume. If list() fails (corrupt state,
    // permissions) we fall back to an empty set so the upload returns a
    // user-friendly "no filesystem" error instead of crashing — but we log
    // the underlying failure so it's debuggable.
    let filesystems = match state.filesystems.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("VM image upload: filesystems.list() failed: {e}");
            Vec::new()
        }
    };
    let fs_name = filesystems
        .first()
        .map(|f| f.name.clone())
        .unwrap_or_default();

    if fs_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No filesystems available" })),
        )
            .into_response();
    }

    let images_path = {
        let fs = match state.filesystems.get(&fs_name).await {
            Ok(f) => f,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        let mp = match fs.mount_point {
            Some(ref p) => p.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "Filesystem not mounted" })),
                )
                    .into_response();
            }
        };
        let path = format!("{mp}/vms/images");
        if let Err(e) = tokio::fs::create_dir_all(&path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({ "error": format!("Failed to create .nasty/images: {e}") }),
                ),
            )
                .into_response();
        }
        path
    };

    // Process the uploaded file
    let Some(mut field) = multipart.next_field().await.ok().flatten() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No file provided" })),
        )
            .into_response();
    };

    let raw_name = field.file_name().unwrap_or("").to_string();
    // Sanitize: strip any path components to prevent path traversal
    let file_name = std::path::Path::new(&raw_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    if file_name.is_empty() {
        info!(
            "VM image upload rejected: empty filename (user '{}')",
            session.username
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No file provided" })),
        )
            .into_response();
    }

    info!(
        "User '{}' uploading VM image: '{}' to {}",
        session.username, file_name, images_path
    );

    // Validate the filename through the central classifier so plain
    // and compressed shapes (e.g. .qcow2.xz, .img.bz2) are all
    // accepted via the same allowlist the importer and lister use.
    if vm_disk_import::classify_vm_image(&file_name).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "Invalid file type. Supported: {}",
                    vm_disk_import::supported_image_extensions_hint()
                )
            })),
        )
            .into_response();
    }

    let dest_path = std::path::Path::new(&images_path).join(&file_name);

    if dest_path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("Image '{}' already exists", file_name) })),
        )
            .into_response();
    }

    // Stream file content to disk
    let mut file = match tokio::fs::File::create(&dest_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Failed to create file: {}", e) })),
            )
                .into_response();
        }
    };

    use tokio::io::AsyncWriteExt;
    let cleanup = || async {
        let _ = tokio::fs::remove_file(&dest_path).await;
    };
    let start = std::time::Instant::now();
    let mut total_bytes: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                total_bytes += chunk.len() as u64;
                if let Err(e) = file.write_all(&chunk).await {
                    drop(file);
                    cleanup().await;
                    tracing::error!(
                        "VM image upload write failed after {} bytes for '{}': {}",
                        total_bytes,
                        file_name,
                        e
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            serde_json::json!({ "error": format!("Failed to write chunk: {}", e) }),
                        ),
                    )
                        .into_response();
                }
            }
            Ok(None) => break,
            Err(e) => {
                drop(file);
                cleanup().await;
                tracing::error!(
                    "VM image upload stream failed after {} bytes for '{}': {}",
                    total_bytes,
                    file_name,
                    e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("Failed to read chunk: {}", e) })),
                )
                    .into_response();
            }
        }
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        cleanup().await;
        tracing::error!("VM image upload sync failed for '{}': {}", file_name, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to sync file: {}", e) })),
        )
            .into_response();
    }

    let elapsed = start.elapsed();
    let size_mib = total_bytes as f64 / (1024.0 * 1024.0);
    let rate_mibs = if elapsed.as_secs_f64() > 0.0 {
        size_mib / elapsed.as_secs_f64()
    } else {
        0.0
    };
    info!(
        "User '{}' uploaded VM image: '{}' ({:.1} MiB in {:.1}s, {:.1} MiB/s)",
        session.username,
        file_name,
        size_mib,
        elapsed.as_secs_f64(),
        rate_mibs
    );
    auth::audit(
        "upload.vm_image",
        &session.username,
        &client_ip,
        &format!("filesystem={fs_name} name={file_name} bytes={total_bytes}"),
    );
    let _ = state.events.send("vm.images".to_string());
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": file_name,
            "path": dest_path.to_string_lossy(),
            "filesystem": fs_name,
        })),
    )
        .into_response()
}

// ── File Browser endpoints ──────────────────────────────────────

const FILES_ROOT: &str = "/fs";
const BLOCK_FILE_NAME: &str = "vol.img";

/// Check if any ancestor (or the path itself) is a block subvolume directory
/// (contains vol.img). Protects block device backing files from accidental
/// deletion or overwrites via the file browser.
fn is_inside_block_subvolume(path: &std::path::Path) -> bool {
    let mut p = path;
    loop {
        if p.join(BLOCK_FILE_NAME).exists() {
            return true;
        }
        match p.parent() {
            Some(parent)
                if parent.starts_with(FILES_ROOT) && parent != std::path::Path::new(FILES_ROOT) =>
            {
                p = parent;
            }
            _ => break,
        }
    }
    false
}

/// Validate that a path is under /fs and doesn't escape via traversal.
fn safe_path(requested: &str) -> Result<std::path::PathBuf, StatusCode> {
    let clean = requested.replace("\\", "/");
    let joined = std::path::Path::new(FILES_ROOT).join(clean.trim_start_matches('/'));
    let canonical = joined.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !canonical.starts_with(FILES_ROOT) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(canonical)
}

/// Whether a canonical path under `/fs` is visible to this session. Owner-
/// scoped API tokens are deliberately denied direct file access: ownership is
/// attached to subvolumes, while this API also exposes arbitrary directories
/// and snapshots and cannot yet prove ownership for every path safely.
fn file_path_in_scope(session: &Session, path: &std::path::Path) -> bool {
    if session.owner.is_some() {
        return false;
    }
    let Some(filesystem) = session.filesystem.as_deref() else {
        return true;
    };
    let Ok(relative) = path.strip_prefix(FILES_ROOT) else {
        return false;
    };
    relative
        .components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        == Some(filesystem)
}

/// Check the requested path before touching the filesystem so a scoped token
/// cannot use 403/404 differences to probe sibling filesystems. Parent
/// traversal is rejected here even when it would eventually resolve back into
/// the allowed filesystem.
fn requested_path_in_filesystem_scope(requested: &str, filesystem: &str) -> bool {
    let clean = requested.replace('\\', "/");
    let mut components = std::path::Path::new(clean.trim_start_matches('/')).components();
    matches!(
        components.next(),
        Some(std::path::Component::Normal(name))
            if name == std::ffi::OsStr::new(filesystem)
    ) && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// Canonicalize a file-browser path and enforce the session's scope. A request
/// for the browser root made with a filesystem-scoped token lands directly on
/// that filesystem, avoiding disclosure of sibling filesystem names.
fn safe_path_for_session(
    requested: &str,
    session: &Session,
) -> Result<std::path::PathBuf, StatusCode> {
    if session.owner.is_some() {
        return Err(StatusCode::FORBIDDEN);
    }
    let scoped_root;
    let requested = if requested.trim_matches('/').is_empty() {
        if let Some(filesystem) = session.filesystem.as_deref() {
            scoped_root = filesystem.to_string();
            scoped_root.as_str()
        } else {
            requested
        }
    } else {
        requested
    };
    if let Some(filesystem) = session.filesystem.as_deref()
        && !requested_path_in_filesystem_scope(requested, filesystem)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let path = match safe_path(requested) {
        Ok(path) => path,
        // Returning the same response for missing and out-of-scope paths keeps
        // an in-scope symlink from becoming a sibling-filesystem existence
        // oracle. Unscoped browser sessions retain the normal 404 behavior.
        Err(_) if session.filesystem.is_some() => return Err(StatusCode::FORBIDDEN),
        Err(status) => return Err(status),
    };
    if !file_path_in_scope(session, &path) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(path)
}

fn safe_path_for_request(
    requested: &str,
    authenticated: &AuthenticatedRequest,
    endpoint: &str,
) -> Result<std::path::PathBuf, StatusCode> {
    let result = safe_path_for_session(requested, &authenticated.session);
    if result == Err(StatusCode::FORBIDDEN) {
        auth::audit(
            "permission_denied",
            &authenticated.session.username,
            &authenticated.client_ip,
            &format!("endpoint={endpoint} reason=ResourceScope"),
        );
    }
    result
}

/// Total the apparent size of a directory tree. GET /api/files/size?path=first/sub
///
/// On-demand only (the browse listing never totals folders — that would make
/// every listing walk the whole tree). Bounded by a timeout so a pathological
/// directory returns "too large to total" instead of hanging the request.
async fn files_size_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let authenticated =
        match validate_bearer(&headers, &state.auth, EndpointAccess::Read, "files.size").await {
            Ok(authenticated) => authenticated,
            Err(e) => return e.into_response(),
        };

    let req_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match safe_path_for_request(req_path, &authenticated, "files.size") {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };
    match tokio::fs::metadata(&dir).await {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Not a directory"})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Not found"})),
            )
                .into_response();
        }
    }

    // Apparent (logical) recursive size via `du -sb`, so it matches the per-file
    // sizes the browser already shows. `du` doesn't follow symlinks, `safe_path`
    // confined `dir` to FILES_ROOT, and `--` guards the canonical path from being
    // read as an option. Timeout-bounded so a huge tree can't wedge the request.
    let du = tokio::process::Command::new("du")
        .arg("-sb")
        .arg("--")
        .arg(dir.as_os_str())
        .output();
    let stdout = match tokio::time::timeout(std::time::Duration::from_secs(60), du).await {
        Ok(Ok(o)) if o.status.success() => o.stdout,
        Ok(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Could not total the directory"})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({
                    "error": "Directory is too large to total quickly (timed out)"
                })),
            )
                .into_response();
        }
    };
    // `du -sb` prints "<bytes>\t<path>"; take the leading integer.
    match String::from_utf8_lossy(&stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(bytes) => (StatusCode::OK, Json(serde_json::json!({ "size": bytes }))).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Could not read the directory size"})),
        )
            .into_response(),
    }
}

/// List directory contents. GET /api/files/browse?path=/first
async fn files_browse_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let authenticated =
        match validate_bearer(&headers, &state.auth, EndpointAccess::Read, "files.browse").await {
            Ok(authenticated) => authenticated,
            Err(e) => return e.into_response(),
        };

    let req_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match safe_path_for_request(req_path, &authenticated, "files.browse") {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    let meta = match tokio::fs::metadata(&dir).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Not found"})),
            )
                .into_response();
        }
    };

    if !meta.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a directory"})),
        )
            .into_response();
    }

    let mut entries = Vec::new();
    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().await.ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(serde_json::json!({
            "name": name,
            "is_dir": is_dir,
            "size": if is_dir { 0 } else { size },
            "modified": modified,
        }));
    }

    // Sort: directories first, then by name
    entries.sort_by(|a, b| {
        let a_dir = a["is_dir"].as_bool().unwrap_or(false);
        let b_dir = b["is_dir"].as_bool().unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    let display_path = dir
        .strip_prefix(FILES_ROOT)
        .unwrap_or(&dir)
        .to_string_lossy()
        .to_string();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "path": display_path,
            "entries": entries,
        })),
    )
        .into_response()
}

/// Name of the httpOnly session cookie set by /api/login and /api/auth/oidc/callback.
const SESSION_COOKIE: &str = "nasty_session";

/// Pull the session token from (in priority order):
///   1. The httpOnly `nasty_session` cookie set by /api/login (browser flow).
///   2. The `Authorization: Bearer ...` header (CLI / kubectl / CSI clients).
///   3. A `?token=...` query parameter (only consulted by routes that accept it,
///      e.g. noVNC and the file-content fallback — handled at the call site).
pub(crate) fn token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(t) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_session_cookie)
    {
        return Some(t);
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Parse a Cookie header value (e.g. "a=1; nasty_session=xyz; b=2") and pull
/// out the session cookie if present. Quietly returns None on any malformed
/// segment rather than throwing — that matches how cookie parsers in stdlib
/// implementations behave.
fn parse_session_cookie(header: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        let (name, value) = match part.split_once('=') {
            Some(t) => t,
            None => continue,
        };
        if name == SESSION_COOKIE && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

struct AuthenticatedRequest {
    session: Session,
    client_ip: String,
}

type HttpAuthError = (StatusCode, Json<serde_json::Value>);

/// Authenticate and authorize a direct HTTP endpoint. Unlike the old helper,
/// this returns the complete session so handlers can enforce resource scope.
async fn validate_bearer(
    headers: &axum::http::HeaderMap,
    auth: &AuthService,
    access: EndpointAccess,
    endpoint: &str,
) -> Result<AuthenticatedRequest, HttpAuthError> {
    let token = match token_from_headers(headers) {
        Some(t) => t,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Missing token"})),
            ));
        }
    };
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let session = auth.validate(&token, &client_ip).await.map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid token"})),
        )
    })?;
    if let Err(reason) = auth::authorize_session(&session, access) {
        auth::audit(
            "permission_denied",
            &session.username,
            &client_ip,
            &format!("endpoint={endpoint} reason={reason:?}"),
        );
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": reason.message()})),
        ));
    }
    Ok(AuthenticatedRequest { session, client_ip })
}

/// Lightweight auth check.  GET /api/auth/check
/// Returns 200 if the bearer token is valid, 401 otherwise.
async fn auth_check_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::SelfService,
        "auth.check",
    )
    .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => e.into_response(),
    }
}

/// Delete a file or directory.  DELETE /api/files?path=first/subdir/file.txt
async fn files_delete_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.delete",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    let target = match safe_path_for_request(req_path, &authenticated, "files.delete") {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    // Refuse to delete filesystem/subvolume roots (depth 1 under /fs, e.g. /fs/mypool)
    let rel = target.strip_prefix(FILES_ROOT).unwrap_or(&target);
    if rel.components().count() <= 1 {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Cannot delete filesystem root directories — use the Subvolumes page"}))).into_response();
    }

    // Protect block subvolume backing files (vol.img and anything in the subvolume dir)
    if is_inside_block_subvolume(&target) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Cannot modify block subvolume contents — manage via the Subvolumes page"}))).into_response();
    }

    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Not found"})),
            )
                .into_response();
        }
    };

    let result = if meta.is_dir() {
        tokio::fs::remove_dir_all(&target).await
    } else {
        tokio::fs::remove_file(&target).await
    };

    match result {
        Ok(()) => {
            info!("Deleted {}", target.display());
            auth::audit(
                "files.delete",
                &authenticated.session.username,
                &authenticated.client_ip,
                &format!("path={}", target.display()),
            );
            let _ = state.events.send("files".to_string());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Upload a file to a directory.  POST /api/files/upload?path=first/subdir
async fn files_upload_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.upload",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    let req_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match safe_path_for_request(req_path, &authenticated, "files.upload") {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    if !dir.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Target is not a directory"})),
        )
            .into_response();
    }

    // Protect block subvolume directories
    if is_inside_block_subvolume(&dir) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Cannot upload into block subvolume — manage via the Subvolumes page"}))).into_response();
    }

    // Read multipart field
    let field = match multipart.next_field().await {
        Ok(Some(f)) => f,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "No file in request"})),
            )
                .into_response();
        }
    };

    let file_name = field.file_name().unwrap_or("upload").to_string();
    // Strip path components to prevent traversal via filename
    let file_name = std::path::Path::new(&file_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload")
        .to_string();

    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid filename"})),
        )
            .into_response();
    }

    let dest = dir.join(&file_name);
    let temp = dir.join(format!(
        ".{file_name}.nasty-upload-{}",
        uuid::Uuid::new_v4()
    ));

    let mut file = match tokio::fs::File::create(&temp).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let mut total: u64 = 0;
    let t0 = std::time::Instant::now();
    let mut field = field;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                total += chunk.len() as u64;
                if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    }

    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&temp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    drop(file);
    if let Err(e) = tokio::fs::rename(&temp, &dest).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to finalize upload: {e}")})),
        )
            .into_response();
    }

    let elapsed = t0.elapsed();
    let speed_mb = (total as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    info!(
        "Uploaded {} ({} bytes, {:.1} MB/s)",
        file_name, total, speed_mb
    );
    auth::audit(
        "files.upload",
        &authenticated.session.username,
        &authenticated.client_ip,
        &format!("path={} bytes={total}", dest.display()),
    );
    let _ = state.events.send("files".to_string());

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": file_name,
            "path": dest.to_string_lossy(),
            "size": total,
        })),
    )
        .into_response()
}

/// Create a directory.  POST /api/files/mkdir?path=first/subdir/newdir
async fn files_mkdir_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.mkdir",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    let clean = req_path.replace("\\", "/");
    let trimmed = clean.trim_start_matches('/');
    let Some((parent_req, leaf)) = trimmed.rsplit_once('/') else {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "Path must include a filesystem and parent directory"}),
            ),
        )
            .into_response();
    };
    if leaf.is_empty() || leaf == "." || leaf == ".." || leaf.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid directory name"})),
        )
            .into_response();
    }
    let parent = match safe_path_for_request(parent_req, &authenticated, "files.mkdir") {
        Ok(parent) => parent,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };
    let full = parent.join(leaf);

    // Protect block subvolume directories
    if is_inside_block_subvolume(&full) || is_inside_block_subvolume(full.parent().unwrap_or(&full))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot create directories inside block subvolumes"})),
        )
            .into_response();
    }

    if full.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Already exists"})),
        )
            .into_response();
    }

    match tokio::fs::create_dir(&full).await {
        Ok(()) => {
            info!("Created directory {}", full.display());

            // Make the new directory writable through the sharing layer.
            // The engine runs as root, so create_dir leaves it root:0755 —
            // but Samba forces writes through a fixed identity (`nobody` on
            // guest shares, the first valid user otherwise) and NFS through
            // squashed uids, so a root:0755 directory rejects every write
            // until it's chmod'd by hand (#519). Access control lives in the
            // protocol layer, not POSIX bits — the same reasoning as the
            // subvolume-create chmod (#482) and the SMB share-root chmod.
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    tokio::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o777)).await
                {
                    tracing::warn!(
                        "chmod 0777 {} failed: {e} — SMB/NFS writes into the new folder may be denied",
                        full.display()
                    );
                }
            }

            auth::audit(
                "files.mkdir",
                &authenticated.session.username,
                &authenticated.client_ip,
                &format!("path={}", full.display()),
            );
            let _ = state.events.send("files".to_string());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct RestoreRequest {
    /// Snapshot paths (relative to /fs) to restore, e.g.
    /// `tank/photos@daily/holiday/img.jpg`. The live destination is
    /// derived server-side by stripping `@<snap>` from the snapshot
    /// component, so a restore can only ever overwrite the snapshot's own
    /// live subvolume — never an arbitrary path.
    items: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RenameRequest {
    /// Existing path under /fs. Resolved to a canonical path; the
    /// rename is rejected if it falls outside FILES_ROOT or targets a
    /// filesystem root (the first path component under /fs).
    from: String,
    /// New path. Doesn't have to exist yet — the parent must, and the
    /// destination itself must not already be there (we never silently
    /// overwrite). Cross-directory renames are allowed as long as both
    /// ends live under the same filesystem (kernel `rename(2)` will
    /// return EXDEV otherwise; we surface that as a 409 with a hint).
    to: String,
}

/// Rename or move a file/directory.  POST /api/files/rename
/// Body: { from: "first/foo.txt", to: "first/bar.txt" }
async fn files_rename_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<RenameRequest>,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.rename",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    // Source must already exist and resolve under /fs.
    let from = match safe_path_for_request(&req.from, &authenticated, "files.rename") {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(serde_json::json!({"error": "Invalid source path"})),
            )
                .into_response();
        }
    };
    // Refuse to move filesystem/subvolume roots (depth 1 under /fs).
    let from_rel = from.strip_prefix(FILES_ROOT).unwrap_or(&from);
    if from_rel.components().count() <= 1 {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot rename filesystem root directories — use the Subvolumes page"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&from) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot rename block subvolume contents — manage via the Subvolumes page"})),
        )
            .into_response();
    }

    // Destination: parent must resolve under /fs and not already exist.
    // We don't canonicalize the destination itself (it doesn't exist
    // yet); we canonicalize the parent and join the leaf name back on
    // so traversal in the leaf is impossible.
    let clean_to = req.to.replace("\\", "/");
    let trimmed_to = clean_to.trim_start_matches('/');
    let (parent_req, leaf) = match trimmed_to.rsplit_once('/') {
        Some((p, l)) => (p, l),
        None => {
            // Renaming to a bare leaf under /fs would land at /fs/<leaf>
            // which is a filesystem root — refuse.
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Destination must include a filesystem path component"})),
            )
                .into_response();
        }
    };
    if leaf.is_empty() || leaf == "." || leaf == ".." || leaf.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid destination name"})),
        )
            .into_response();
    }
    let parent = match safe_path_for_request(parent_req, &authenticated, "files.rename") {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(serde_json::json!({"error": "Invalid destination parent"})),
            )
                .into_response();
        }
    };
    if is_inside_block_subvolume(&parent) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot move into block subvolume contents"})),
        )
            .into_response();
    }
    let to = parent.join(leaf);
    if !to.starts_with(FILES_ROOT) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Invalid destination path"})),
        )
            .into_response();
    }
    if to == from {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "noop": true})),
        )
            .into_response();
    }
    if to.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Destination already exists"})),
        )
            .into_response();
    }

    match tokio::fs::rename(&from, &to).await {
        Ok(()) => {
            info!("Renamed {} -> {}", from.display(), to.display());
            auth::audit(
                "files.rename",
                &authenticated.session.username,
                &authenticated.client_ip,
                &format!("from={} to={}", from.display(), to.display()),
            );
            let _ = state.events.send("files".to_string());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        // EXDEV (18) — cross-device rename. Reachable when source and
        // destination live on different bcachefs filesystems mounted
        // under /fs. `rename(2)` is atomic but inherently single-fs,
        // so we surface a clear message rather than swallowing it as
        // a generic 500.
        Err(e) if e.raw_os_error() == Some(18) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Cross-filesystem rename not supported — use copy + delete"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Copy a file or directory to a new location under /fs. The source
/// and destination safety rules mirror `files_rename_handler`: both
/// ends must canonicalize under /fs, neither may target a filesystem
/// root, and the destination must not exist (we never silently
/// overwrite — bulk-copying a folder of media onto an existing folder
/// of the same name has bitten us before).
///
/// Unlike rename, copy isn't kernel-atomic. For files we use
/// `tokio::fs::copy` directly; for directories we walk iteratively
/// with a stack — recursion depth on a populated photo / media tree
/// can otherwise blow the default stack.
///
/// POST /api/files/copy with body `{ from, to }` — same shape as
/// rename so the WebUI can reuse its existing request builder.
async fn files_copy_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<RenameRequest>,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.copy",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    let from = match safe_path_for_request(&req.from, &authenticated, "files.copy") {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(serde_json::json!({"error": "Invalid source path"})),
            )
                .into_response();
        }
    };
    let from_rel = from.strip_prefix(FILES_ROOT).unwrap_or(&from);
    if from_rel.components().count() <= 1 {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot copy filesystem root directories — use the Subvolumes page"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&from) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot copy block subvolume contents — manage via the Subvolumes page"})),
        )
            .into_response();
    }

    let clean_to = req.to.replace("\\", "/");
    let trimmed_to = clean_to.trim_start_matches('/');
    let (parent_req, leaf) = match trimmed_to.rsplit_once('/') {
        Some((p, l)) => (p, l),
        None => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Destination must include a filesystem path component"})),
            )
                .into_response();
        }
    };
    if leaf.is_empty() || leaf == "." || leaf == ".." || leaf.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid destination name"})),
        )
            .into_response();
    }
    let parent = match safe_path_for_request(parent_req, &authenticated, "files.copy") {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(serde_json::json!({"error": "Invalid destination parent"})),
            )
                .into_response();
        }
    };
    if is_inside_block_subvolume(&parent) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot copy into block subvolume contents"})),
        )
            .into_response();
    }
    let to = parent.join(leaf);
    if !to.starts_with(FILES_ROOT) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Invalid destination path"})),
        )
            .into_response();
    }
    if to == from {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Source and destination are the same"})),
        )
            .into_response();
    }
    // Refuse copying a directory into itself or any of its descendants
    // — that's an infinite walk and produces nonsense even when
    // bounded. `to.starts_with(&from)` catches both cases.
    if to.starts_with(&from) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Cannot copy a directory into itself"})),
        )
            .into_response();
    }
    if to.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Destination already exists"})),
        )
            .into_response();
    }

    let from_meta = match tokio::fs::symlink_metadata(&from).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("stat source: {e}")})),
            )
                .into_response();
        }
    };

    let result = if from_meta.is_dir() {
        copy_dir_recursive(&from, &to).await
    } else {
        tokio::fs::copy(&from, &to)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    };

    match result {
        Ok(()) => {
            info!("Copied {} -> {}", from.display(), to.display());
            auth::audit(
                "files.copy",
                &authenticated.session.username,
                &authenticated.client_ip,
                &format!("from={} to={}", from.display(), to.display()),
            );
            let _ = state.events.send("files".to_string());
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => {
            auth::audit(
                "files.copy_failed",
                &authenticated.session.username,
                &authenticated.client_ip,
                &format!("from={} to={} error={e}", from.display(), to.display()),
            );
            // Directory copies can fail after creating part of the destination.
            let _ = state.events.send("files".to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
                .into_response()
        }
    }
}

/// Recursively copy `src` to `dst`. Iterative (stack-based) so a deep
/// tree can't blow the default tokio task stack. Plain files use
/// `tokio::fs::copy`; symlinks are recreated with `symlink` rather
/// than dereferenced (matches `cp -a` semantics — losing a relative
/// symlink to absolute, or vice versa, would silently change what the
/// user expects to be a literal link).
///
/// Permissions and timestamps follow the kernel's default behaviour
/// for `copy`/`mkdir` — we don't preserve mtime/atime explicitly.
/// That's fine for the WebUI's "copy this folder" affordance, which
/// is treated by users as "new copy made now," not "snapshot of
/// the original at this point in time."
async fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    // (current source path, current destination path) tuples.
    let mut stack: Vec<(std::path::PathBuf, std::path::PathBuf)> =
        vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        tokio::fs::create_dir(&dst_dir)
            .await
            .map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
        let mut entries = tokio::fs::read_dir(&src_dir)
            .await
            .map_err(|e| format!("read_dir {}: {e}", src_dir.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("next_entry {}: {e}", src_dir.display()))?
        {
            let entry_src = entry.path();
            let entry_dst = dst_dir.join(entry.file_name());
            let meta = entry
                .metadata()
                .await
                .map_err(|e| format!("metadata {}: {e}", entry_src.display()))?;
            if meta.is_dir() {
                stack.push((entry_src, entry_dst));
            } else if meta.file_type().is_symlink() {
                let target = tokio::fs::read_link(&entry_src)
                    .await
                    .map_err(|e| format!("read_link {}: {e}", entry_src.display()))?;
                tokio::fs::symlink(&target, &entry_dst)
                    .await
                    .map_err(|e| format!("symlink {}: {e}", entry_dst.display()))?;
            } else {
                tokio::fs::copy(&entry_src, &entry_dst).await.map_err(|e| {
                    format!(
                        "copy {} -> {}: {e}",
                        entry_src.display(),
                        entry_dst.display()
                    )
                })?;
            }
        }
    }
    Ok(())
}

// ── Restore-from-snapshot endpoint ─────────────────────────────

/// Given a snapshot path relative to `/fs` (e.g. `tank/photos@daily/x/y`),
/// derive the *live* destination by stripping `@<snap>` from the snapshot
/// component (index 1: `<subvol>@<snap>`). Returns the live relative path
/// (`tank/photos/x/y`).
///
/// Errors when the path isn't inside a snapshot (component 1 has no `@`),
/// is too short, or contains traversal/empty components. `@` is forbidden
/// in subvolume and snapshot names (`validate_subvolume_name`), so the
/// snapshot component is unambiguous even when a deeper *file* is named
/// `a@b`. This is the safety pivot: a restore can only target the
/// snapshot's own live subvolume, never an operator-supplied path.
fn derive_restore_dest(rel: &str) -> Result<String, String> {
    let rel = rel.trim_start_matches('/');
    let comps: Vec<&str> = rel.split('/').collect();
    if comps.len() < 2 {
        return Err("not a snapshot path (expected <filesystem>/<subvol>@<snap>/…)".into());
    }
    for c in &comps {
        if c.is_empty() || *c == "." || *c == ".." {
            return Err("invalid path component".into());
        }
    }
    let Some((subvol, _snap)) = comps[1].split_once('@') else {
        return Err("source is not inside a snapshot".into());
    };
    if subvol.is_empty() {
        return Err("invalid snapshot path".into());
    }
    let mut out = comps;
    out[1] = subvol;
    Ok(out.join("/"))
}

/// Remove whatever currently exists at `p` (file, symlink, or directory)
/// so a restore can overwrite it. No-op when `p` doesn't exist.
async fn remove_existing(p: &std::path::Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(p).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(p)
            .await
            .map_err(|e| format!("rm -r {}: {e}", p.display())),
        Ok(_) => tokio::fs::remove_file(p)
            .await
            .map_err(|e| format!("rm {}: {e}", p.display())),
        Err(_) => Ok(()),
    }
}

/// Like [`copy_dir_recursive`] but with overwrite-merge semantics for
/// restore: an existing destination directory is merged into (not
/// rejected), files are overwritten, symlinks replaced. Files that exist
/// in the live tree but not the snapshot are left untouched (additive —
/// no `--delete`).
async fn restore_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    let mut stack: Vec<(std::path::PathBuf, std::path::PathBuf)> =
        vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        match tokio::fs::symlink_metadata(&dst_dir).await {
            Ok(m) if m.is_dir() => {} // merge into the existing directory
            Ok(_) => {
                // A non-directory is in the way — replace it with a dir.
                remove_existing(&dst_dir).await?;
                tokio::fs::create_dir(&dst_dir)
                    .await
                    .map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
            }
            Err(_) => tokio::fs::create_dir_all(&dst_dir)
                .await
                .map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?,
        }
        let mut entries = tokio::fs::read_dir(&src_dir)
            .await
            .map_err(|e| format!("read_dir {}: {e}", src_dir.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("next_entry {}: {e}", src_dir.display()))?
        {
            let entry_src = entry.path();
            let entry_dst = dst_dir.join(entry.file_name());
            let meta = entry
                .metadata()
                .await
                .map_err(|e| format!("metadata {}: {e}", entry_src.display()))?;
            if meta.is_dir() {
                stack.push((entry_src, entry_dst));
            } else {
                restore_leaf(&entry_src, &entry_dst, &meta).await?;
            }
        }
    }
    Ok(())
}

/// Restore a single file or symlink onto `dst`, overwriting any existing
/// entry there (including a directory or symlink in the way).
async fn restore_leaf(
    src: &std::path::Path,
    dst: &std::path::Path,
    meta: &std::fs::Metadata,
) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        remove_existing(dst).await?;
        let target = tokio::fs::read_link(src)
            .await
            .map_err(|e| format!("read_link {}: {e}", src.display()))?;
        tokio::fs::symlink(&target, dst)
            .await
            .map_err(|e| format!("symlink {}: {e}", dst.display()))
    } else {
        // `tokio::fs::copy` overwrites a regular file, but not a dir/symlink
        // sitting at the destination — clear those first.
        if let Ok(m) = tokio::fs::symlink_metadata(dst).await
            && (m.is_dir() || m.file_type().is_symlink())
        {
            remove_existing(dst).await?;
        }
        tokio::fs::copy(src, dst)
            .await
            .map(|_| ())
            .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))
    }
}

/// Restore files/folders from a read-only snapshot back over their live
/// subvolume. POST /api/files/restore with `{ items: [<snapshot path>…] }`.
///
/// The destination is derived (see [`derive_restore_dest`]) — callers
/// never supply it — so a restore can only ever overwrite the snapshot's
/// own live subvolume. Overwrites existing files (the whole point); files
/// created after the snapshot are left in place. The live destination's
/// parent must already exist (restore a parent folder first if a whole
/// directory tree was deleted).
async fn files_restore_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<RestoreRequest>,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.restore",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };
    if req.items.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "no items to restore"})),
        )
            .into_response();
    }

    let mut restored = 0u32;
    for item in &req.items {
        // Source: the snapshot path. safe_path canonicalises + jails to /fs.
        let src = match safe_path_for_request(item, &authenticated, "files.restore") {
            Ok(p) => p,
            Err(status) => {
                return (
                    status,
                    Json(serde_json::json!({"error": format!("invalid source path: {item}")})),
                )
                    .into_response();
            }
        };
        if is_inside_block_subvolume(&src) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "cannot restore block subvolume contents — manage via the Subvolumes page"})),
            )
                .into_response();
        }
        let src_rel = src.strip_prefix(FILES_ROOT).unwrap_or(&src);
        let live_rel = match derive_restore_dest(&src_rel.to_string_lossy()) {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": e})),
                )
                    .into_response();
            }
        };

        let dest = std::path::Path::new(FILES_ROOT).join(&live_rel);
        let (Some(dest_parent), Some(leaf)) = (dest.parent(), dest.file_name()) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid destination"})),
            )
                .into_response();
        };
        // The live parent must exist; canonicalising it also blocks symlink
        // escapes. If it's gone, the live subvolume/folder was deleted.
        let parent_rel = dest_parent.strip_prefix(FILES_ROOT).unwrap_or(dest_parent);
        let parent_canon = match safe_path_for_request(
            &parent_rel.to_string_lossy(),
            &authenticated,
            "files.restore",
        ) {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"error": format!(
                        "the live location for '{live_rel}' no longer exists — clone the snapshot instead"
                    )})),
                )
                    .into_response();
            }
        };
        if is_inside_block_subvolume(&parent_canon) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "cannot restore into block subvolume contents"})),
            )
                .into_response();
        }
        let final_dest = parent_canon.join(leaf);
        if !final_dest.starts_with(FILES_ROOT) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "invalid destination path"})),
            )
                .into_response();
        }

        let src_meta = match tokio::fs::symlink_metadata(&src).await {
            Ok(m) => m,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("stat source: {e}")})),
                )
                    .into_response();
            }
        };
        let result = if src_meta.is_dir() {
            restore_dir_recursive(&src, &final_dest).await
        } else {
            restore_leaf(&src, &final_dest, &src_meta).await
        };
        if let Err(e) = result {
            auth::audit(
                "files.restore_failed",
                &authenticated.session.username,
                &authenticated.client_ip,
                &format!(
                    "from={} to={} error={e}",
                    src.display(),
                    final_dest.display()
                ),
            );
            // Recursive restore can fail after changing part of an item.
            let _ = state.events.send("files".to_string());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
                .into_response();
        }
        info!("Restored {} -> {}", src.display(), final_dest.display());
        restored += 1;
        auth::audit(
            "files.restore",
            &authenticated.session.username,
            &authenticated.client_ip,
            &format!("from={} to={}", src.display(), final_dest.display()),
        );
        // A later item can fail, so publish each completed mutation rather
        // than waiting for the entire batch to succeed.
        let _ = state.events.send("files".to_string());
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "restored": restored})),
    )
        .into_response()
}

// ── File content/download endpoint ─────────────────────────────

/// Serve file content with appropriate Content-Type for browser preview.
/// GET /api/files/content?path=first/photos/image.jpg
///
/// Auth is via the session cookie (same-origin browsers — `<img>` / `<iframe>`
/// send it automatically) or `Authorization: Bearer` (CLI tools).
async fn files_content_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Read,
        "files.content.get",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    let target = match safe_path_for_request(req_path, &authenticated, "files.content.get") {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    // Don't serve directories or block subvolume contents
    if target.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Cannot serve directory"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&target) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot access block subvolume contents"})),
        )
            .into_response();
    }

    // Determine content type from extension
    let content_type = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| match ext.to_lowercase().as_str() {
            // Images
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            "bmp" => "image/bmp",
            "avif" => "image/avif",
            // Video
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "ogv" => "video/ogg",
            "mkv" => "video/x-matroska",
            "avi" => "video/x-msvideo",
            "mov" => "video/quicktime",
            // Audio
            "mp3" => "audio/mpeg",
            "ogg" | "oga" => "audio/ogg",
            "wav" => "audio/wav",
            "flac" => "audio/flac",
            "aac" | "m4a" => "audio/mp4",
            "wma" => "audio/x-ms-wma",
            "opus" => "audio/opus",
            // Documents
            "pdf" => "application/pdf",
            // Text
            "txt" | "log" | "md" | "csv" | "conf" | "cfg" | "ini" | "yml" | "yaml" | "toml"
            | "json" | "xml" | "html" | "htm" | "css" | "js" | "ts" | "rs" | "py" | "sh"
            | "bash" | "nix" | "c" | "h" | "cpp" | "go" | "java" | "rb" | "php" | "sql"
            | "dockerfile" => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        })
        .unwrap_or("application/octet-stream");

    // Stream the file
    let file = match tokio::fs::File::open(&target).await {
        Ok(f) => f,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "File not found"})),
            )
                .into_response();
        }
    };

    let metadata = file.metadata().await.ok();
    let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        content_type.parse().unwrap(),
    );
    headers.insert(
        axum::http::header::CONTENT_LENGTH,
        file_size.to_string().parse().unwrap(),
    );
    // Inline display for previewable types, attachment for downloads
    let disposition = if content_type.starts_with("image/")
        || content_type.starts_with("video/")
        || content_type.starts_with("audio/")
        || content_type == "application/pdf"
        || content_type.starts_with("text/")
    {
        format!("inline; filename=\"{file_name}\"")
    } else {
        format!("attachment; filename=\"{file_name}\"")
    };
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        disposition.parse().unwrap(),
    );

    (StatusCode::OK, headers, body).into_response()
}

/// Overwrite a file with new content. PUT /api/files/content?path=…
///
/// Used by the in-page text editor (config files, YAML, scripts).
/// Body is the raw new contents — Content-Type is ignored. The target
/// must already exist as a regular file; writing to a missing path
/// would be the upload endpoint's job, and writing into a directory
/// or block-subvolume backing file is rejected. Body size is capped
/// by the route's `DefaultBodyLimit` (10 MiB) — the in-browser editor
/// isn't where someone should be pasting a gigabyte of logs.
async fn files_content_put_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::Mutation,
        "files.content.put",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(e) => return e.into_response(),
    };

    let req_path = match params.get("path") {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "path is required"})),
            )
                .into_response();
        }
    };

    let target = match safe_path_for_request(req_path, &authenticated, "files.content.put") {
        Ok(p) => p,
        Err(status) => {
            return (status, Json(serde_json::json!({"error": "Invalid path"}))).into_response();
        }
    };

    // Refuse to edit filesystem roots (no regular-file targets there
    // anyway, but the error message is clearer than "is a directory").
    let rel = target.strip_prefix(FILES_ROOT).unwrap_or(&target);
    if rel.components().count() <= 1 {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot edit filesystem root directories"})),
        )
            .into_response();
    }
    if is_inside_block_subvolume(&target) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Cannot edit block subvolume contents — manage via the Subvolumes page"})),
        )
            .into_response();
    }

    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Not found"})),
            )
                .into_response();
        }
    };
    if !meta.is_file() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Target is not a regular file"})),
        )
            .into_response();
    }

    // Write to a sibling temp file and rename. Keeps the original
    // intact if the write fails partway, and means concurrent readers
    // never see a truncated file. The temp name uses the PID to avoid
    // colliding with anything else the engine might create.
    let tmp = target.with_extension(format!("nasty-edit.{}.tmp", std::process::id()));
    if let Err(e) = tokio::fs::write(&tmp, &body).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("write failed: {e}")})),
        )
            .into_response();
    }
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("rename failed: {e}")})),
        )
            .into_response();
    }
    info!("Edited {} ({} bytes)", target.display(), body.len());
    auth::audit(
        "files.content.put",
        &authenticated.session.username,
        &authenticated.client_ip,
        &format!("path={} bytes={}", target.display(), body.len()),
    );
    let _ = state.events.send("files".to_string());
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "bytes": body.len()})),
    )
        .into_response()
}

// ── Login endpoint ──────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct WebauthnLoginStartRequest {
    username: String,
}

#[derive(Deserialize)]
struct WebauthnLoginFinishRequest {
    username: String,
    auth_id: String,
    response: webauthn_rs::prelude::PublicKeyCredential,
}

/// Build the assertion challenge for a WebAuthn login. Pre-auth —
/// the WebUI hits this before any session exists. Echoes the
/// per-IP lockout from `AuthService::login`, so an attacker hosing
/// the box with bad assertions hits the same per-IP cap as a
/// password sprayer.
async fn webauthn_login_start_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<WebauthnLoginStartRequest>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    match state.webauthn.login_start(&state.auth, &req.username).await {
        Ok(resp) => (StatusCode::OK, Json(serde_json::to_value(&resp).unwrap())).into_response(),
        Err(e) => {
            tracing::warn!(
                "WebAuthn login.start failed for '{}' from {}: {e}",
                req.username,
                client_ip
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Verify the browser's WebAuthn assertion and mint a session. The
/// username in the body has to match the one bound to the
/// in-flight `auth_id` — `WebauthnService::login_finish` enforces
/// that. On any failure we audit + record the per-IP / per-user
/// failure so the assertion path can't be used to dodge lockouts.
async fn webauthn_login_finish_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<WebauthnLoginFinishRequest>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    let verified_username = match state
        .webauthn
        .login_finish(&state.auth, &req.auth_id, &req.response)
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(
                "WebAuthn login.finish failed for '{}' from {}: {e}",
                req.username,
                client_ip
            );
            state
                .auth
                .record_webauthn_failure(&req.username, client_ip)
                .await;
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Defense in depth: the body's claimed username must match the
    // one bound to the `auth_id` server-side. login_finish already
    // returns the verified username; we just confirm equality so a
    // browser bug or replay can't end up minting a session for a
    // different account than the client thinks.
    if verified_username != req.username {
        tracing::warn!(
            "WebAuthn login.finish: body username '{}' != session username '{}' from {}",
            req.username,
            verified_username,
            client_ip
        );
        state
            .auth
            .record_webauthn_failure(&req.username, client_ip)
            .await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "username mismatch" })),
        )
            .into_response();
    }

    match state
        .auth
        .mint_session_for_webauthn(&verified_username, client_ip)
        .await
    {
        Ok(token) => {
            info!(
                "WebAuthn login successful: user '{}' from {}",
                verified_username, client_ip
            );
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert(
                axum::http::header::SET_COOKIE,
                build_session_cookie(&token).parse().unwrap(),
            );
            (
                StatusCode::OK,
                resp_headers,
                Json(serde_json::json!({ "token": token, "username": verified_username })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(
                "WebAuthn session mint failed for '{}' from {}: {e}",
                verified_username,
                client_ip
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

async fn login_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    match state
        .auth
        .login(&req.username, &req.password, client_ip)
        .await
    {
        Ok(token) => {
            info!(
                "Login successful: user '{}' from {}",
                req.username, client_ip
            );
            // Two delivery channels for the same token:
            //   - Set-Cookie for browsers — httpOnly, so XSS can't read it.
            //   - JSON body for CLI clients (kubectl, CSI driver) that don't
            //     have a cookie jar.
            // The token in the body is the same value the cookie carries; both
            // are valid until the session TTL expires.
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert(
                axum::http::header::SET_COOKIE,
                build_session_cookie(&token).parse().unwrap(),
            );
            (
                StatusCode::OK,
                resp_headers,
                Json(serde_json::json!({ "token": token })),
            )
                .into_response()
        }
        Err(_) => {
            tracing::warn!("Login failed: user '{}' from {}", req.username, client_ip);
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid credentials" })),
            )
                .into_response()
        }
    }
}

/// Revoke the current session and clear the cookie. Browsers can't remove an
/// httpOnly cookie themselves, so logout has to round-trip to the server.
async fn logout_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut resp_headers = axum::http::HeaderMap::new();
    resp_headers.insert(
        axum::http::header::SET_COOKIE,
        build_session_clear_cookie().parse().unwrap(),
    );
    if let Some(token) = token_from_headers(&headers) {
        // Best-effort revoke; if the token is already invalid we still want
        // the browser to drop the cookie below.
        let _ = state.auth.logout(&token).await;
    }
    (
        StatusCode::OK,
        resp_headers,
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

// ── Public guest-share access surface (#474) ────────────────────────────
// Unauthenticated handlers backing `/api/public/share/*`. Auth is by the
// URL token (the link is the credential) plus, for protected shares, a
// short-lived unlock grant cookie. Every "unavailable" reason collapses to
// one generic response so a token-guesser gets no oracle.

const SHARE_GRANT_COOKIE: &str = "nasty_share_grant";

fn build_share_grant_cookie(token: &str) -> String {
    // Scoped to the public share path and short-lived to match the grant
    // TTL. SameSite=Strict is fine while shares are served same-origin
    // (path-based); the future share.* subdomain work revisits this.
    format!(
        "{SHARE_GRANT_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/api/public/share; Max-Age=3600"
    )
}

fn share_grant_from_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())?;
    let prefix = format!("{SHARE_GRANT_COOKIE}=");
    raw.split(';')
        .map(|p| p.trim())
        .find_map(|p| p.strip_prefix(&prefix))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

fn now_unix_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn share_client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// One generic "not available" for unknown / expired / revoked / exhausted /
/// unresolvable — no oracle for token or path guessing.
fn share_not_available() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "This share is not available"})),
    )
        .into_response()
}

/// `GET /api/public/share/{token}` → guest landing metadata. Counts a view.
async fn public_share_meta_handler(
    axum::extract::Path(token): axum::extract::Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let now = now_unix_i64();
    let Some(share) = state.guest_shares.lookup_active(&token, now).await else {
        return share_not_available();
    };
    state.guest_shares.record_view(&share.id).await;
    Json(guestshare::GuestShareService::meta(&share)).into_response()
}

#[derive(Deserialize)]
struct ShareUnlockRequest {
    password: String,
}

/// `POST /api/public/share/{token}/unlock` → verify the password once and
/// hand back a short-lived grant cookie. Rate-limited per (IP, token).
async fn public_share_unlock_handler(
    axum::extract::Path(token): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ShareUnlockRequest>,
) -> impl IntoResponse {
    let now = now_unix_i64();
    let client_ip = share_client_ip(&headers);

    if state.guest_shares.unlock_locked(&client_ip, &token, now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "Too many attempts; try again later"})),
        )
            .into_response();
    }

    let Some(share) = state.guest_shares.lookup_active(&token, now).await else {
        return share_not_available();
    };

    if guestshare::GuestShareService::verify_share_password(&share, &body.password) {
        state.guest_shares.clear_unlock_failures(&client_ip, &token);
        let grant = state.guest_shares.mint_grant(&share.id, now);
        crate::auth::audit(
            "guest_share_unlock",
            "guest",
            &client_ip,
            &format!("share_id={}", share.id),
        );
        let mut resp_headers = axum::http::HeaderMap::new();
        resp_headers.insert(
            axum::http::header::SET_COOKIE,
            build_share_grant_cookie(&grant).parse().unwrap(),
        );
        return (
            StatusCode::OK,
            resp_headers,
            Json(serde_json::json!({"ok": true})),
        )
            .into_response();
    }

    state
        .guest_shares
        .record_unlock_failure(&client_ip, &token, now);
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "Incorrect password"})),
    )
        .into_response()
}

/// `GET /api/public/share/{token}/download?path=…` → stream one file as an
/// attachment. Enforces token validity → password grant (if protected) →
/// path stays in a share root → `max_downloads` cap, then counts + audits.
async fn public_share_download_handler(
    axum::extract::Path(token): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let now = now_unix_i64();
    let client_ip = share_client_ip(&headers);

    let Some(share) = state.guest_shares.lookup_active(&token, now).await else {
        return share_not_available();
    };

    if guestshare::GuestShareService::needs_password(&share) {
        let granted = share_grant_from_cookie(&headers)
            .map(|g| state.guest_shares.check_grant(&g, &share.id, now))
            .unwrap_or(false);
        if !granted {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Password required"})),
            )
                .into_response();
        }
    }

    let rel = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let Some(target) = guestshare::GuestShareService::resolve_download(&share, rel) else {
        return share_not_available();
    };

    // Count + enforce the cap before opening the stream. A failure here means
    // the share went inactive (e.g. hit its cap) between lookup and now.
    if state
        .guest_shares
        .register_download(&share.id, now)
        .await
        .is_err()
    {
        return share_not_available();
    }

    let file = match tokio::fs::File::open(&target).await {
        Ok(f) => f,
        Err(_) => return share_not_available(),
    };
    let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    crate::auth::audit(
        "guest_share_download",
        "guest",
        &client_ip,
        &format!("share_id={} name={name} bytes={size}", share.id),
    );

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);
    let mut resp_headers = axum::http::HeaderMap::new();
    resp_headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    resp_headers.insert(
        axum::http::header::CONTENT_LENGTH,
        size.to_string().parse().unwrap(),
    );
    // Always an attachment — never inline. Shared HTML must not render on the
    // app origin (stored-XSS), so we never hand the browser a renderable type.
    resp_headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{name}\"").parse().unwrap(),
    );
    (StatusCode::OK, resp_headers, body).into_response()
}

/// `GET /api/public/share/{token}/zip` → stream a ZIP of the whole share
/// (folders walked recursively, symlinks skipped). Same gates as download;
/// counts as a single download against `max_downloads`.
async fn public_share_zip_handler(
    axum::extract::Path(token): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let now = now_unix_i64();
    let client_ip = share_client_ip(&headers);

    let Some(share) = state.guest_shares.lookup_active(&token, now).await else {
        return share_not_available();
    };

    if guestshare::GuestShareService::needs_password(&share) {
        let granted = share_grant_from_cookie(&headers)
            .map(|g| state.guest_shares.check_grant(&g, &share.id, now))
            .unwrap_or(false);
        if !granted {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Password required"})),
            )
                .into_response();
        }
    }

    if state
        .guest_shares
        .register_download(&share.id, now)
        .await
        .is_err()
    {
        return share_not_available();
    }

    let filename = guestshare::GuestShareService::zip_filename(&share);
    crate::auth::audit(
        "guest_share_zip",
        "guest",
        &client_ip,
        &format!("share_id={} name={filename}", share.id),
    );

    let reader = state.guest_shares.zip_stream(&share);
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(reader));
    let mut resp_headers = axum::http::HeaderMap::new();
    resp_headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/zip".parse().unwrap(),
    );
    // Streamed: length is unknown up front, so no Content-Length (chunked).
    resp_headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );
    (StatusCode::OK, resp_headers, body).into_response()
}

/// 8h, matches SESSION_TTL_SECS in auth.rs (kept in sync by hand).
const SESSION_COOKIE_MAX_AGE_SECS: u64 = 8 * 3600;

fn build_session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_COOKIE_MAX_AGE_SECS}"
    )
}

fn build_session_clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
}

// ── OIDC SSO ─────────────────────────────────────────────────────

/// Percent-encode a value for placement in a URL fragment.
fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Tells the WebUI whether to render the "Sign in with SSO" button.
/// No auth required — the response only exposes booleans / public config.
async fn oidc_available_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let oidc = state.settings.get().await.oidc;
    let configured = state.oidc.current().await.is_some();
    Json(serde_json::json!({
        "enabled": oidc.enabled && configured,
    }))
}

/// Unauthenticated probe for the login page's "Sign in with security
/// key" button visibility. Returns `{ "has_credentials": bool }` —
/// true iff at least one user has at least one registered WebAuthn
/// credential. On a fresh install the button is just visual noise
/// (clicking it fails at the engine's "no credentials for user"
/// check), so the WebUI gates the button on this AND the browser's
/// own WebAuthn API support.
///
/// Parallels `/api/auth/oidc/available`. The one-bit leak (an
/// unauthenticated caller learns whether any keys are registered)
/// is acceptable for the same reason it is there: rendering the
/// login page right requires answering this question before auth
/// has happened.
async fn webauthn_available_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let has_credentials = state.auth.any_webauthn_credentials_registered().await;
    Json(serde_json::json!({
        "has_credentials": has_credentials,
    }))
}

/// Start an OIDC authorization-code flow. 302s the browser to the IdP.
/// Returns 404 when SSO is disabled so the endpoint doesn't leak its existence.
async fn oidc_start_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(client) = state.oidc.current().await else {
        return (StatusCode::NOT_FOUND, "OIDC not enabled").into_response();
    };
    let url = client.authorize_url().await;
    axum::response::Redirect::to(url.as_str()).into_response()
}

#[derive(Deserialize)]
struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// IdP callback. Validates state + code, exchanges for tokens, mints a NASty
/// session, and 302s the browser to `/#nasty_token=…&oidc=1`. Errors land at
/// `/#oidc_error=<reason>` so the SPA can show a meaningful message.
async fn oidc_callback_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<OidcCallbackQuery>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let bounce = |fragment: String| -> axum::response::Response {
        axum::response::Redirect::to(&format!("/#{fragment}")).into_response()
    };

    if let Some(err) = q.error {
        let detail = q.error_description.unwrap_or_default();
        crate::auth::audit(
            "oidc_login_failed",
            "anonymous",
            &client_ip,
            &format!("{err}: {detail}"),
        );
        return bounce(format!(
            "oidc_error={}",
            url_encode(&format!("{err}: {detail}"))
        ));
    }

    let (Some(code), Some(state_param)) = (q.code, q.state) else {
        crate::auth::audit(
            "oidc_login_failed",
            "anonymous",
            &client_ip,
            "missing code or state",
        );
        return bounce("oidc_error=missing+code+or+state".into());
    };

    let Some(client) = state.oidc.current().await else {
        return (StatusCode::NOT_FOUND, "OIDC not enabled").into_response();
    };

    let identity = match client.exchange_code(&state_param, &code).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("OIDC token exchange failed: {e}");
            crate::auth::audit("oidc_login_failed", "anonymous", &client_ip, &e.to_string());
            return bounce(format!("oidc_error={}", url_encode(&e.to_string())));
        }
    };

    let oidc_settings = state.settings.get().await.oidc;
    let derived_role_str = auth_oidc::role_for_groups(&identity.groups, &oidc_settings);
    let derived_role = derived_role_str
        .as_deref()
        .and_then(crate::auth::parse_role_str);

    match state
        .auth
        .login_or_provision_oidc(
            &identity,
            derived_role,
            oidc_settings.auto_provision,
            &client_ip,
        )
        .await
    {
        // Token is delivered via httpOnly cookie now — never lands in the URL,
        // browser history, or referer header. The fragment is just a flag the
        // SPA reads to know "we just came back from OIDC, refresh state".
        Ok(token) => {
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert(
                axum::http::header::SET_COOKIE,
                build_session_cookie(&token).parse().unwrap(),
            );
            (resp_headers, axum::response::Redirect::to("/#oidc=1")).into_response()
        }
        Err(e) => bounce(format!("oidc_error={}", url_encode(&e.to_string()))),
    }
}

// ── WebSocket with auth ─────────────────────────────────────────

/// Reject WebSocket upgrades whose `Origin` header does not match `Host`.
/// Defends against cross-site WebSocket hijacking: a malicious page in the
/// user's browser cannot open a WS to the appliance and ride existing auth.
///
/// No Origin header → non-browser client (curl, kubectl, CSI driver) → allow.
async fn ws_origin_check(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    let headers = req.headers();
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok());

    if let Some(origin) = origin {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok());
        let origin_authority = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .map(|s| s.split('/').next().unwrap_or(s));
        let allowed = matches!(
            (origin_authority, host),
            (Some(o), Some(h)) if o.eq_ignore_ascii_case(h)
        );
        if !allowed {
            tracing::warn!(
                "WS rejected: Origin '{}' does not match Host '{}'",
                origin,
                host.unwrap_or("")
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(next.run(req).await)
}

/// Serve the OpenAPI 3.1 document describing the REST gateway. Built once on
/// first request (schemars walk is expensive) and cached for the process lifetime.
async fn openapi_handler() -> impl IntoResponse {
    static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let body = CACHED.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let (_g, groups) = registry::build_full_registry();
        let doc = registry::render_openapi(version, &groups);
        serde_json::to_string(&doc).expect("OpenAPI doc must serialize")
    });
    ([("content-type", "application/json")], body.clone())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    // Browsers send the session cookie on the upgrade request automatically;
    // resolve it here so the WS task doesn't have to wait for an auth message.
    // Non-browser clients (kubectl, CSI driver) typically don't have a cookie
    // and still send {"token": "..."} as the first message — handled in
    // handle_socket().
    let pre_auth_token = token_from_headers(&headers);
    ws.on_upgrade(move |socket| handle_socket(socket, state, client_ip, pre_auth_token))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client_ip: String,
    pre_auth_token: Option<String>,
) {
    use futures_util::{SinkExt, StreamExt};
    use nasty_common::Notification;

    info!("WebSocket client connected from {client_ip}, awaiting authentication");

    let session = match resolve_session(&mut socket, &state, &client_ip, pre_auth_token).await {
        Some(s) => s,
        None => return,
    };

    info!("WebSocket authenticated as '{}'", session.username);

    let mut event_rx = state.events.subscribe();
    let (mut writer, mut reader) = socket.split();

    // Server-initiated WebSocket-level keepalive. Without this, a client
    // that vanishes silently (laptop suspended, NAT mapping dropped,
    // upstream link cut) stays in the engine's WS-task list until the
    // OS's TCP retransmit timeouts eventually unwind — minutes later.
    // Pinging every 30 s and dropping the connection if we haven't seen
    // any traffic in IDLE_TIMEOUT closes dead clients fast, frees their
    // `event_rx` subscription, and matches what the WebUI client expects
    // from a healthy proxy (Caddy holds the upgrade open as long as both
    // ends are talking, so visible ping traffic is what keeps it alive
    // through every intermediary).
    const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_ticker.tick().await; // skip the immediate first tick
    let mut last_seen = std::time::Instant::now();
    let connected_at = std::time::Instant::now();
    // Captures *why* the WS closed so the disconnect log line below
    // says more than just "disconnected" — distinguishes client-side
    // close (with code/reason), TCP-level stream end, server-side idle
    // timeout, and engine-initiated drops after a failed write. Set on
    // every `break` path; if a future change adds a break without
    // setting this, the fallback below makes that loud rather than
    // silent.
    let disconnect_reason: String;

    loop {
        tokio::select! {
            msg = reader.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_seen = std::time::Instant::now();
                        let response = handle_rpc_request(&text, &state, &session).await;
                        if writer.send(Message::Text(response.into())).await.is_err() {
                            disconnect_reason = "write failed (socket closed)".to_string();
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Ping(_))) => {
                        // Axum's tungstenite layer auto-replies to Pings,
                        // so we just record the heartbeat. Pongs from the
                        // client confirm they're still listening.
                        last_seen = std::time::Instant::now();
                    }
                    Some(Ok(Message::Close(frame))) => {
                        match frame {
                            Some(f) => disconnect_reason = format!(
                                "client close (code={}, reason={:?})",
                                f.code, f.reason
                            ),
                            None => disconnect_reason = "client close (no frame)".to_string(),
                        }
                        break;
                    }
                    None => {
                        disconnect_reason = "stream ended (TCP close)".to_string();
                        break;
                    }
                    Some(Err(e)) => {
                        disconnect_reason = format!("read error: {e}");
                        break;
                    }
                    _ => {
                        last_seen = std::time::Instant::now();
                    }
                }
            }
            event = event_rx.recv() => {
                if let Ok(collection) = event {
                    let notification = Notification::new(
                        "event",
                        Some(serde_json::json!({ "collection": collection })),
                    );
                    let text = serde_json::to_string(&notification).unwrap();
                    if writer.send(Message::Text(text.into())).await.is_err() {
                        disconnect_reason = "event-write failed (socket closed)".to_string();
                        break;
                    }
                }
            }
            _ = ping_ticker.tick() => {
                if last_seen.elapsed() > IDLE_TIMEOUT {
                    disconnect_reason = format!(
                        "server idle timeout ({}s no traffic)",
                        IDLE_TIMEOUT.as_secs()
                    );
                    let _ = writer.send(Message::Close(None)).await;
                    break;
                }
                if writer.send(Message::Ping(Vec::new().into())).await.is_err() {
                    disconnect_reason = "ping-write failed (socket closed)".to_string();
                    break;
                }
            }
        }
    }

    info!(
        "WebSocket client '{}' disconnected after {:?}: {}",
        session.username,
        connected_at.elapsed(),
        disconnect_reason
    );
}

/// Pick the right auth path for a WebSocket connection. If the upgrade
/// request carried a session cookie or Bearer token, validate it directly
/// and acknowledge — that's the browser path now. Otherwise, fall back to
/// waiting for a `{"token": "..."}` message, which is how non-browser
/// clients (kubectl, CSI driver) authenticate.
async fn resolve_session(
    socket: &mut WebSocket,
    state: &AppState,
    client_ip: &str,
    pre_auth_token: Option<String>,
) -> Option<Session> {
    if let Some(token) = pre_auth_token {
        return match state.auth.validate(&token, client_ip).await {
            Ok(session) => {
                let _ = socket
                    .send(Message::Text(
                        serde_json::json!({
                            "authenticated": true,
                            "username": session.username,
                            "role": session.role,
                            "must_change_password": session.must_change_password,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                Some(session)
            }
            Err(_) => {
                let _ = socket
                    .send(Message::Text(r#"{"error":"invalid session"}"#.into()))
                    .await;
                let _ = socket.send(Message::Close(None)).await;
                None
            }
        };
    }
    wait_for_auth(socket, state, client_ip).await
}

/// Wait for the first message which must be: {"token": "..."}
/// Returns the session if valid, or None if auth failed (socket is closed).
async fn wait_for_auth(
    socket: &mut WebSocket,
    state: &AppState,
    client_ip: &str,
) -> Option<Session> {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv())
        .await
        .ok()??
        .ok()?;

    let text = match msg {
        Message::Text(t) => t,
        _ => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"first message must be JSON with token"}"#.into(),
                ))
                .await;
            return None;
        }
    };

    #[derive(Deserialize)]
    struct AuthMsg {
        token: String,
    }

    let auth_msg: AuthMsg = match serde_json::from_str(&text) {
        Ok(a) => a,
        Err(_) => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error":"expected {\"token\": \"...\"}"}"#.into(),
                ))
                .await;
            return None;
        }
    };

    match state.auth.validate(&auth_msg.token, client_ip).await {
        Ok(session) => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({
                        "authenticated": true,
                        "username": session.username,
                        "role": session.role,
                        "must_change_password": session.must_change_password
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            Some(session)
        }
        Err(e) => {
            tracing::warn!("Auth failed for client {client_ip}: {e}");
            let _ = socket
                .send(Message::Text(r#"{"error":"invalid token"}"#.into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            None
        }
    }
}

// ── Background Alert Notifier ──────────────────────────────────

enum AlertLifecycle {
    /// First time the rule's condition was true for this (rule, source)
    /// pair since the alert notifier started.
    Fired,
    /// The rule's condition stopped being true — the alert cleared.
    /// The payload carries the ActiveAlert as it was when it last
    /// fired (not the current state, which by definition no longer
    /// matches the rule).
    Resolved,
}

/// Build the structured webhook payload for an alert lifecycle event
/// and dispatch through every enabled notification channel. The
/// `event_id` is the same across both fired and resolved events for
/// a given alert, so receivers can correlate (and incident-tracking
/// systems can close the same incident they opened).
async fn dispatch_alert_event(
    config: &nasty_system::notifications::NotificationConfig,
    alert: &nasty_system::alerts::ActiveAlert,
    lifecycle: AlertLifecycle,
) {
    use nasty_system::notifications;
    let sev = match alert.severity {
        nasty_system::alerts::AlertSeverity::Warning => "WARNING",
        nasty_system::alerts::AlertSeverity::Critical => "CRITICAL",
    };
    let (event_type, subject_prefix, body_suffix) = match lifecycle {
        AlertLifecycle::Fired => ("alert.fired", format!("[NASty {sev}]"), String::new()),
        AlertLifecycle::Resolved => (
            "alert.resolved",
            "[NASty RESOLVED]".to_string(),
            "\n\nThe alert condition is no longer matching — incident has cleared.".to_string(),
        ),
    };
    let subject = format!("{subject_prefix} {}", alert.rule_name);
    let body = format!(
        "{}\n\nSource: {}\nValue: {:.1}\nThreshold: {:.1}{}",
        alert.message, alert.source, alert.current_value, alert.threshold, body_suffix
    );
    // Stable event id derived from rule + source + the value the
    // alert had when it fired. Crucially the resolved event reuses
    // the same id (we use the original alert's current_value, not
    // whatever the metric reads now) so receivers can pair the two.
    let event_id = format!(
        "alert-{}-{}-{}",
        alert.rule_id, alert.source, alert.current_value as i64
    );
    let mut data = serde_json::to_value(alert).unwrap_or(serde_json::Value::Null);
    // Embed lifecycle in `data.lifecycle` too — some receivers route
    // on a single nested field rather than the top-level event_type.
    if let serde_json::Value::Object(ref mut m) = data {
        m.insert(
            "lifecycle".to_string(),
            serde_json::Value::String(
                match lifecycle {
                    AlertLifecycle::Fired => "fired",
                    AlertLifecycle::Resolved => "resolved",
                }
                .to_string(),
            ),
        );
    }
    let event = notifications::Event {
        event_type,
        event_id: &event_id,
        subject: &subject,
        body: &body,
        data,
    };
    notifications::send_event(config, &event).await;
}

fn spawn_alert_notifier(state: Arc<AppState>) {
    let h = tokio::spawn(async move {
        use nasty_system::alerts::ActiveAlert;
        use nasty_system::notifications;
        use std::collections::HashMap;

        // Keyed map (rule_id, source) → the ActiveAlert as it was when
        // we first dispatched the alert.fired event. We keep the whole
        // alert (not just the key) so when the alert resolves we can
        // emit alert.resolved carrying the same payload — receivers
        // close the same incident they opened on fire.
        //
        // Note: on engine restart this map resets to empty, so any
        // alerts that resolved during downtime never get a resolved
        // event. Persisting outbox state across restarts is out of
        // scope; the same limitation already applies to fired events
        // (currently-active alerts re-fire after a restart).
        let mut previously_active: HashMap<(String, String), ActiveAlert> = HashMap::new();

        // Wait for the metrics service and the rest of the system to come up
        // before the first evaluation; first-boot stats are noisy.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            // Evaluate directly. The previous version read state.alerts_cache,
            // which was only populated by the WebUI dashboard polling — meaning
            // the notifier silently skipped every cycle when no admin had a
            // browser open. A drive failing at 3am went unalerted until someone
            // opened the dashboard the next morning.
            let active = crate::router::evaluate_active_alerts(&state).await;

            // Refresh the RPC cache as a side effect so the next WebUI poll
            // returns instantly with up-to-date data.
            if let Ok(value) = serde_json::to_value(&active) {
                *state.alerts_cache.lock().await = Some((std::time::Instant::now(), value));
            }

            let current: HashMap<(String, String), ActiveAlert> = active
                .iter()
                .map(|a| ((a.rule_id.clone(), a.source.clone()), a.clone()))
                .collect();

            // Newly-fired = in `current` but not in `previously_active`.
            let new_alerts: Vec<&ActiveAlert> = current
                .iter()
                .filter(|(k, _)| !previously_active.contains_key(k))
                .map(|(_, a)| a)
                .collect();

            // Resolved = in `previously_active` but not in `current`.
            // Emit with the original alert payload so the receiver
            // sees the same `source`, `rule_id`, etc. it opened the
            // incident with — typical pattern for monitoring systems
            // (PagerDuty / Opsgenie / Alertmanager) that key incident
            // close events on the original payload.
            let resolved_alerts: Vec<&ActiveAlert> = previously_active
                .iter()
                .filter(|(k, _)| !current.contains_key(k))
                .map(|(_, a)| a)
                .collect();

            if !new_alerts.is_empty() || !resolved_alerts.is_empty() {
                let config = notifications::NotificationConfig::load();
                if config.channels.iter().any(|ch| ch.enabled) {
                    for alert in &new_alerts {
                        dispatch_alert_event(&config, alert, AlertLifecycle::Fired).await;
                    }
                    for alert in &resolved_alerts {
                        dispatch_alert_event(&config, alert, AlertLifecycle::Resolved).await;
                    }
                }
            }

            previously_active = current;
        }
    });
    // Observer spawn — alert evaluation is supposed to run forever; if
    // the loop dies, notifications stop entirely with no log line
    // connecting "I never got a disk-full alert" to the underlying bug.
    tokio::spawn(async move {
        match h.await {
            Ok(()) => tracing::error!(
                "alert notifier loop exited unexpectedly — no further notifications \
                 will fire until engine restart"
            ),
            Err(e) => tracing::error!(
                "alert notifier loop panicked / cancelled: {e} — no further \
                 notifications will fire until engine restart"
            ),
        }
    });
}

#[cfg(test)]
mod restore_tests {
    use super::{derive_restore_dest, file_path_in_scope, requested_path_in_filesystem_scope};
    use crate::auth::{Role, Session};

    fn session(filesystem: Option<&str>, owner: Option<&str>) -> Session {
        Session {
            token: "token".to_string(),
            username: "test".to_string(),
            role: Role::Operator,
            filesystem: filesystem.map(str::to_string),
            owner: owner.map(str::to_string),
            created_at: 0,
            must_change_password: false,
            client_ip: None,
        }
    }

    #[test]
    fn file_paths_respect_filesystem_scope() {
        let unscoped = session(None, None);
        assert!(file_path_in_scope(
            &unscoped,
            std::path::Path::new("/fs/one/private/file")
        ));

        let scoped = session(Some("one"), None);
        assert!(file_path_in_scope(
            &scoped,
            std::path::Path::new("/fs/one/private/file")
        ));
        assert!(file_path_in_scope(
            &scoped,
            std::path::Path::new("/fs/one/private@snap/file")
        ));
        assert!(!file_path_in_scope(
            &scoped,
            std::path::Path::new("/fs/two/private/file")
        ));
        assert!(!file_path_in_scope(&scoped, std::path::Path::new("/fs")));
    }

    #[test]
    fn owner_scoped_tokens_cannot_use_direct_file_api() {
        let scoped = session(Some("one"), Some("automation"));
        assert!(!file_path_in_scope(
            &scoped,
            std::path::Path::new("/fs/one/private/file")
        ));
    }

    #[test]
    fn scoped_requests_reject_sibling_and_parent_traversal_before_lookup() {
        assert!(requested_path_in_filesystem_scope(
            "one/private/file",
            "one"
        ));
        assert!(requested_path_in_filesystem_scope(
            "/one/private/file",
            "one"
        ));
        assert!(!requested_path_in_filesystem_scope(
            "two/private/file",
            "one"
        ));
        assert!(!requested_path_in_filesystem_scope(
            "one/../two/file",
            "one"
        ));
        assert!(!requested_path_in_filesystem_scope(
            "two/../one/file",
            "one"
        ));
        assert!(!requested_path_in_filesystem_scope(
            "one\\..\\two\\file",
            "one"
        ));
    }

    #[test]
    fn derives_live_path_from_snapshot() {
        // <fs>/<subvol>@<snap>/rel → <fs>/<subvol>/rel
        assert_eq!(
            derive_restore_dest("tank/photos@daily/holiday/img.jpg").unwrap(),
            "tank/photos/holiday/img.jpg"
        );
        // The snapshot root restores the whole subvolume root.
        assert_eq!(
            derive_restore_dest("tank/photos@daily").unwrap(),
            "tank/photos"
        );
        // A leading slash is tolerated.
        assert_eq!(
            derive_restore_dest("/tank/photos@daily/a").unwrap(),
            "tank/photos/a"
        );
    }

    #[test]
    fn only_strips_the_snapshot_component() {
        // A deeper file literally named `a@b` must NOT be touched — only
        // component 1 (the snapshot dir) is rewritten.
        assert_eq!(
            derive_restore_dest("tank/photos@daily/a@b.txt").unwrap(),
            "tank/photos/a@b.txt"
        );
    }

    #[test]
    fn rejects_non_snapshot_and_traversal() {
        // Component 1 has no '@' — not a snapshot path.
        assert!(derive_restore_dest("tank/photos/img.jpg").is_err());
        // Too short (no subvolume component).
        assert!(derive_restore_dest("tank").is_err());
        // Traversal anywhere is rejected.
        assert!(derive_restore_dest("tank/photos@daily/../etc").is_err());
        // Empty snapshot subvolume name.
        assert!(derive_restore_dest("tank/@daily/x").is_err());
    }

    // End-to-end round-trip of the actual byte-moving logic (the heart of
    // the feature): build a fake snapshot tree + a diverged live tree, run
    // the real restore copy, and assert the live tree matches the snapshot
    // while files created after the snapshot survive (additive semantics).
    #[tokio::test]
    async fn restore_round_trip_overwrites_and_is_additive() {
        use super::{restore_dir_recursive, restore_leaf};
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("photos@daily");
        let live = tmp.path().join("photos");
        tokio::fs::create_dir_all(snap.join("trip")).await.unwrap();
        tokio::fs::create_dir_all(live.join("trip")).await.unwrap();

        // Snapshot (the "good" past state).
        tokio::fs::write(snap.join("a.txt"), b"v1").await.unwrap();
        tokio::fs::write(snap.join("trip/b.txt"), b"keep")
            .await
            .unwrap();
        // Live (diverged): a.txt modified, b.txt deleted, plus a new file.
        tokio::fs::write(live.join("a.txt"), b"v2-modified")
            .await
            .unwrap();
        tokio::fs::write(live.join("trip/after.txt"), b"new")
            .await
            .unwrap();

        // Restore the whole snapshot tree over the live subvolume.
        restore_dir_recursive(&snap, &live).await.unwrap();

        // Modified file rolled back, deleted file restored…
        assert_eq!(tokio::fs::read(live.join("a.txt")).await.unwrap(), b"v1");
        assert_eq!(
            tokio::fs::read(live.join("trip/b.txt")).await.unwrap(),
            b"keep"
        );
        // …and the file created after the snapshot is left in place (additive).
        assert!(live.join("trip/after.txt").exists());

        // A single-file restore of a now-deleted file also reappears.
        tokio::fs::remove_file(live.join("a.txt")).await.unwrap();
        let meta = tokio::fs::symlink_metadata(snap.join("a.txt"))
            .await
            .unwrap();
        restore_leaf(&snap.join("a.txt"), &live.join("a.txt"), &meta)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(live.join("a.txt")).await.unwrap(), b"v1");
    }
}
