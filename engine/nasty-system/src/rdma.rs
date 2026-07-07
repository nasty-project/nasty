//! Per-box RDMA opt-in for storage share transports (#602).
//!
//! RDMA is a transport *modifier* on existing share protocols (iSER on
//! iSCSI portals, NFS-over-RDMA listeners, NVMe-oF `trtype=rdma`
//! ports), not a protocol of its own — so it lives beside
//! `protocol.rs` rather than inside it. The per-box toggle follows the
//! house pattern: capability detection informs the WebUI, the explicit
//! toggle enables the feature (the toggle IS the validation).
//!
//! Capability signal: `/sys/class/infiniband` non-empty — one probe
//! covers native InfiniBand, RoCE NICs, and `rdma_rxe` soft-RoCE
//! identically (all register an RDMA device there; the directory name
//! is historical).
//!
//! State: `/var/lib/nasty/rdma.json`, default disabled — upgraded
//! boxes behave exactly as before until the operator opts in, and a
//! rolled-back engine simply ignores the file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::protocol::{modprobe, systemctl, systemctl_is_active};

const STATE_PATH: &str = "/var/lib/nasty/rdma.json";
const IB_CLASS_DIR: &str = "/sys/class/infiniband";
const NFSD_PORTLIST: &str = "/proc/fs/nfsd/portlist";

/// nfsd's RDMA listener service id. Like NVMe-oF's 4420, this is an
/// RDMA-CM service id, not an IP port — the only wire-level port RDMA
/// traffic occupies is RoCEv2's UDP 4791 (native IB bypasses the IP
/// stack entirely).
pub const NFS_RDMA_PORT: u16 = 20049;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RdmaConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// One RDMA device from `/sys/class/infiniband`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RdmaDevice {
    /// Device name, e.g. `mlx5_0`, `rxe0`.
    pub name: String,
    /// `InfiniBand` or `Ethernet` (RoCE / soft-RoCE), from the first
    /// port's `link_layer`.
    pub link_layer: String,
    /// Associated network interfaces, when resolvable.
    pub netdevs: Vec<String>,
}

/// Live RDMA state for the WebUI checklist.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RdmaStatus {
    pub enabled: bool,
    /// An RDMA device exists — the gate for flipping the toggle on.
    pub capable: bool,
    pub devices: Vec<RdmaDevice>,
    /// `modprobe -n` dry-run results — informational (the checklist
    /// shows them); real errors surface at activation time.
    pub ib_isert_available: bool,
    pub nvmet_rdma_available: bool,
    pub nfs_rdma_available: bool,
    /// nfsd currently has an `rdma` listener in its portlist.
    pub nfs_rdma_active: bool,
    /// One-line reason the toggle is disabled, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RdmaSetRequest {
    pub enabled: bool,
}

static STATE_LOCK: Mutex<()> = Mutex::const_new(());

pub async fn load() -> RdmaConfig {
    let _guard = STATE_LOCK.lock().await;
    nasty_common::load_singleton_or_recover(STATE_PATH).await
}

/// Quick boolean for gate checks in the routers.
pub async fn enabled() -> bool {
    load().await.enabled
}

pub async fn status() -> RdmaStatus {
    let devices = scan_devices().await;
    let capable = !devices.is_empty();
    let enabled = load().await.enabled;
    let blocker = (!capable).then(|| {
        "no RDMA-capable device found (/sys/class/infiniband is empty) — install an \
         RDMA NIC (InfiniBand or RoCE) or load a soft-RoCE device (rdma_rxe) first"
            .to_string()
    });
    RdmaStatus {
        enabled,
        capable,
        devices,
        ib_isert_available: modprobe_dry_run("ib_isert").await,
        nvmet_rdma_available: modprobe_dry_run("nvmet-rdma").await,
        nfs_rdma_available: modprobe_dry_run("rpcrdma").await,
        nfs_rdma_active: portlist_has_rdma(
            &tokio::fs::read_to_string(NFSD_PORTLIST)
                .await
                .unwrap_or_default(),
        ),
        blocker,
    }
}

/// Flip the toggle. Enabling requires a capable box and live-activates
/// the transports for already-running protocols; disabling restarts
/// nfs-server if needed (the portlist has no listener-removal API).
///
/// Precondition checks that need nasty-sharing state (no RDMA NVMe-oF
/// ports / iSER portals left when disabling) live in the router arm —
/// this crate can't see the sharing crate.
pub async fn set_enabled(enable: bool) -> Result<RdmaStatus, String> {
    if enable {
        let devices = scan_devices().await;
        if devices.is_empty() {
            return Err(
                "cannot enable RDMA: no RDMA-capable device found (/sys/class/infiniband \
                 is empty). Install an RDMA NIC (InfiniBand or RoCE) or load a soft-RoCE \
                 device (rdma_rxe) first."
                    .to_string(),
            );
        }
    }

    {
        let _guard = STATE_LOCK.lock().await;
        let cfg = RdmaConfig { enabled: enable };
        let json = serde_json::to_string_pretty(&cfg).map_err(|e| format!("serialize: {e}"))?;
        tokio::fs::write(STATE_PATH, json)
            .await
            .map_err(|e| format!("write {STATE_PATH}: {e}"))?;
    }

    if enable {
        // Live-activate for protocols that are already running; the
        // protocol enable/restore paths handle the rest from now on.
        if systemctl_is_active("nfs-server.service").await
            && let Err(e) = activate_nfs_rdma().await
        {
            warn!("RDMA enabled but NFS-RDMA activation failed: {e}");
        }
        if std::path::Path::new("/sys/kernel/config/nvmet").exists()
            && let Err(e) = ensure_module("nvmet-rdma").await
        {
            warn!("RDMA enabled but nvmet-rdma load failed: {e}");
        }
        if std::path::Path::new("/sys/module/iscsi_target_mod").exists()
            && let Err(e) = ensure_module("ib_isert").await
        {
            warn!("RDMA enabled but ib_isert load failed: {e}");
        }
        info!("RDMA transports enabled");
    } else {
        if systemctl_is_active("nfs-server.service").await {
            // Only a restart drops the rdma listener; the start path
            // sees enabled=false and won't re-add it.
            if let Err(e) = systemctl("restart", "nfs-server.service").await {
                warn!("RDMA disable: nfs-server restart failed: {e}");
            }
        }
        info!("RDMA transports disabled");
    }

    Ok(status().await)
}

/// Load a kernel module for an RDMA transport on demand — used by the
/// share routers right before creating an RDMA port/portal, so the
/// module is present even if it was unloaded since enable.
pub async fn ensure_module(module: &str) -> Result<(), String> {
    modprobe(module).await
}

/// Add nfsd's RDMA listener (idempotent). Called after every
/// engine-initiated nfs-server start while the toggle is on; the
/// engine is the only thing that starts nfs-server on NASty, so this
/// covers every start path. (A manual `systemctl restart nfs-server`
/// over SSH drops the listener until the next engine-driven start —
/// documented operator caveat, not a supported flow.)
pub async fn activate_nfs_rdma() -> Result<(), String> {
    let portlist = tokio::fs::read_to_string(NFSD_PORTLIST)
        .await
        .map_err(|e| format!("read {NFSD_PORTLIST}: {e} (is nfsd running?)"))?;
    if portlist_has_rdma(&portlist) {
        return Ok(());
    }
    // The kernel would auto-load svcrdma via the write below, but the
    // explicit modprobe turns a missing module into a readable error
    // instead of a bare EPROTONOSUPPORT.
    modprobe("rpcrdma").await?;
    tokio::fs::write(NFSD_PORTLIST, format!("rdma {NFS_RDMA_PORT}\n"))
        .await
        .map_err(|e| format!("add rdma listener to {NFSD_PORTLIST}: {e}"))?;
    info!("NFS-over-RDMA listener active (service id {NFS_RDMA_PORT})");
    Ok(())
}

/// `rdma <port>` line present in an nfsd portlist dump. Pure for
/// testing.
pub fn portlist_has_rdma(portlist: &str) -> bool {
    portlist.lines().any(|l| l.trim_start().starts_with("rdma"))
}

async fn modprobe_dry_run(module: &str) -> bool {
    nasty_common::cmd::run("modprobe", &["-n", "-q", module])
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn scan_devices() -> Vec<RdmaDevice> {
    let mut out = Vec::new();
    let Ok(mut dir) = tokio::fs::read_dir(IB_CLASS_DIR).await else {
        return out;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let base = entry.path();
        // First port's link layer identifies the fabric. Ports are
        // numbered from 1.
        let link_layer = tokio::fs::read_to_string(base.join("ports/1/link_layer"))
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        // Associated netdevs: real hardware exposes device/net/<if>;
        // rxe devices expose a `parent` attribute naming the netdev.
        let mut netdevs = Vec::new();
        if let Ok(mut net) = tokio::fs::read_dir(base.join("device/net")).await {
            while let Ok(Some(e)) = net.next_entry().await {
                netdevs.push(e.file_name().to_string_lossy().to_string());
            }
        } else if let Ok(parent) = tokio::fs::read_to_string(base.join("parent")).await {
            let parent = parent.trim().to_string();
            if !parent.is_empty() {
                netdevs.push(parent);
            }
        }
        netdevs.sort();
        out.push(RdmaDevice {
            name,
            link_layer,
            netdevs,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portlist_rdma_detection() {
        assert!(portlist_has_rdma("tcp 2049\nrdma 20049\n"));
        assert!(portlist_has_rdma("rdma 20049"));
        // Indented variants and missing entries.
        assert!(portlist_has_rdma("  rdma 20049\n"));
        assert!(!portlist_has_rdma("tcp 2049\nudp 2049\n"));
        assert!(!portlist_has_rdma(""));
    }
}
