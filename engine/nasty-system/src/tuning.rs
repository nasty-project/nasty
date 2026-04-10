use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

const STATE_PATH: &str = "/var/lib/nasty/tuning.json";
const STATE_DIR: &str = "/var/lib/nasty";
const SMB_TUNING_CONF: &str = "/etc/samba/nasty-tuning.conf";

// ── Structs ──────────────────────────────────────────────────

/// System-wide NAS performance tuning configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TuningConfig {
    // ── NFS ──────────────────────────────────────────────
    /// Number of NFS server (nfsd) kernel threads.
    #[serde(default = "default_nfs_threads")]
    pub nfs_threads: u32,
    /// NFSv4 lease time in seconds. Clients must renew state within this window.
    #[serde(default = "default_nfs_lease_time")]
    pub nfs_lease_time: u32,
    /// NFSv4 grace period in seconds after server restart. Clients can reclaim locks.
    #[serde(default = "default_nfs_grace_time")]
    pub nfs_grace_time: u32,

    // ── SMB ──────────────────────────────────────────────
    /// Maximum simultaneous SMB connections (0 = unlimited).
    #[serde(default)]
    pub smb_max_connections: u32,
    /// Minutes before idle SMB clients are disconnected (0 = never).
    #[serde(default)]
    pub smb_deadtime: u32,
    /// Samba socket options for TCP tuning (e.g. `SO_RCVBUF=131072 SO_SNDBUF=131072`).
    #[serde(default)]
    pub smb_socket_options: String,

    // ── iSCSI ────────────────────────────────────────────
    /// Default command queue depth per iSCSI session.
    #[serde(default = "default_iscsi_cmdsn_depth")]
    pub iscsi_default_cmdsn_depth: u32,
    /// iSCSI login timeout in seconds.
    #[serde(default = "default_iscsi_login_timeout")]
    pub iscsi_login_timeout: u32,

    // ── VM writeback ─────────────────────────────────────
    /// Maximum percentage of memory that can be dirty before synchronous writeback kicks in.
    #[serde(default = "default_vm_dirty_ratio")]
    pub vm_dirty_ratio: u32,
    /// Dirty page percentage at which background writeback starts.
    #[serde(default = "default_vm_dirty_background_ratio")]
    pub vm_dirty_background_ratio: u32,
    /// Centiseconds before dirty pages are old enough to be written out.
    #[serde(default = "default_vm_dirty_expire_centisecs")]
    pub vm_dirty_expire_centisecs: u32,
    /// Centiseconds between writeback daemon wakeups.
    #[serde(default = "default_vm_dirty_writeback_centisecs")]
    pub vm_dirty_writeback_centisecs: u32,
}

fn default_nfs_threads() -> u32 { 8 }
fn default_nfs_lease_time() -> u32 { 90 }
fn default_nfs_grace_time() -> u32 { 90 }
fn default_iscsi_cmdsn_depth() -> u32 { 64 }
fn default_iscsi_login_timeout() -> u32 { 15 }
fn default_vm_dirty_ratio() -> u32 { 20 }
fn default_vm_dirty_background_ratio() -> u32 { 10 }
fn default_vm_dirty_expire_centisecs() -> u32 { 3000 }
fn default_vm_dirty_writeback_centisecs() -> u32 { 500 }

impl Default for TuningConfig {
    fn default() -> Self {
        Self {
            nfs_threads: default_nfs_threads(),
            nfs_lease_time: default_nfs_lease_time(),
            nfs_grace_time: default_nfs_grace_time(),
            smb_max_connections: 0,
            smb_deadtime: 0,
            smb_socket_options: String::new(),
            iscsi_default_cmdsn_depth: default_iscsi_cmdsn_depth(),
            iscsi_login_timeout: default_iscsi_login_timeout(),
            vm_dirty_ratio: default_vm_dirty_ratio(),
            vm_dirty_background_ratio: default_vm_dirty_background_ratio(),
            vm_dirty_expire_centisecs: default_vm_dirty_expire_centisecs(),
            vm_dirty_writeback_centisecs: default_vm_dirty_writeback_centisecs(),
        }
    }
}

/// Partial update for tuning configuration. Only provided fields are changed.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TuningUpdate {
    pub nfs_threads: Option<u32>,
    pub nfs_lease_time: Option<u32>,
    pub nfs_grace_time: Option<u32>,
    pub smb_max_connections: Option<u32>,
    pub smb_deadtime: Option<u32>,
    pub smb_socket_options: Option<String>,
    pub iscsi_default_cmdsn_depth: Option<u32>,
    pub iscsi_login_timeout: Option<u32>,
    pub vm_dirty_ratio: Option<u32>,
    pub vm_dirty_background_ratio: Option<u32>,
    pub vm_dirty_expire_centisecs: Option<u32>,
    pub vm_dirty_writeback_centisecs: Option<u32>,
}

// ── Service ──────────────────────────────────────────────────

pub struct TuningService {
    state: Arc<RwLock<TuningConfig>>,
}

impl TuningService {
    pub async fn new() -> Self {
        let config = load().await;
        let svc = Self {
            state: Arc::new(RwLock::new(config)),
        };
        svc.apply_all().await;
        svc
    }

    pub async fn get(&self) -> TuningConfig {
        self.state.read().await.clone()
    }

    pub async fn update(&self, update: TuningUpdate) -> Result<TuningConfig, String> {
        let mut config = self.state.write().await;

        // ── NFS ──
        if let Some(v) = update.nfs_threads {
            if v == 0 { return Err("nfs_threads must be > 0".into()); }
            apply_nfs_threads(v).await?;
            config.nfs_threads = v;
        }
        if let Some(v) = update.nfs_lease_time {
            if v == 0 { return Err("nfs_lease_time must be > 0".into()); }
            apply_proc_value("/proc/fs/nfsd/nfsv4leasetime", v).await?;
            config.nfs_lease_time = v;
        }
        if let Some(v) = update.nfs_grace_time {
            if v == 0 { return Err("nfs_grace_time must be > 0".into()); }
            apply_proc_value("/proc/fs/nfsd/nfsv4gracetime", v).await?;
            config.nfs_grace_time = v;
        }

        // ── SMB ──
        let mut smb_changed = false;
        if let Some(v) = update.smb_max_connections {
            config.smb_max_connections = v;
            smb_changed = true;
        }
        if let Some(v) = update.smb_deadtime {
            config.smb_deadtime = v;
            smb_changed = true;
        }
        if let Some(v) = update.smb_socket_options {
            config.smb_socket_options = v;
            smb_changed = true;
        }
        if smb_changed {
            apply_smb_tuning(&config).await?;
        }

        // ── iSCSI ──
        if let Some(v) = update.iscsi_default_cmdsn_depth {
            apply_iscsi_cmdsn_depth(v).await?;
            config.iscsi_default_cmdsn_depth = v;
        }
        if let Some(v) = update.iscsi_login_timeout {
            apply_iscsi_login_timeout(v).await?;
            config.iscsi_login_timeout = v;
        }

        // ── VM writeback ──
        if let Some(v) = update.vm_dirty_ratio {
            if v > 100 { return Err("vm_dirty_ratio must be 0-100".into()); }
            apply_sysctl("vm.dirty_ratio", v).await?;
            config.vm_dirty_ratio = v;
        }
        if let Some(v) = update.vm_dirty_background_ratio {
            if v > 100 { return Err("vm_dirty_background_ratio must be 0-100".into()); }
            apply_sysctl("vm.dirty_background_ratio", v).await?;
            config.vm_dirty_background_ratio = v;
        }
        if let Some(v) = update.vm_dirty_expire_centisecs {
            apply_sysctl("vm.dirty_expire_centisecs", v).await?;
            config.vm_dirty_expire_centisecs = v;
        }
        if let Some(v) = update.vm_dirty_writeback_centisecs {
            apply_sysctl("vm.dirty_writeback_centisecs", v).await?;
            config.vm_dirty_writeback_centisecs = v;
        }

        save(&config).await.map_err(|e| e.to_string())?;
        Ok(config.clone())
    }

    /// Apply all persisted tuning values to the running system.
    /// Called once at startup.
    async fn apply_all(&self) {
        let config = self.state.read().await.clone();

        if let Err(e) = apply_nfs_threads(config.nfs_threads).await {
            warn!("Failed to apply nfs_threads: {e}");
        }
        if let Err(e) = apply_proc_value("/proc/fs/nfsd/nfsv4leasetime", config.nfs_lease_time).await {
            warn!("Failed to apply nfs_lease_time: {e}");
        }
        if let Err(e) = apply_proc_value("/proc/fs/nfsd/nfsv4gracetime", config.nfs_grace_time).await {
            warn!("Failed to apply nfs_grace_time: {e}");
        }
        if let Err(e) = apply_smb_tuning(&config).await {
            warn!("Failed to apply SMB tuning: {e}");
        }
        if let Err(e) = apply_iscsi_cmdsn_depth(config.iscsi_default_cmdsn_depth).await {
            warn!("Failed to apply iscsi_default_cmdsn_depth: {e}");
        }
        if let Err(e) = apply_iscsi_login_timeout(config.iscsi_login_timeout).await {
            warn!("Failed to apply iscsi_login_timeout: {e}");
        }
        if let Err(e) = apply_sysctl("vm.dirty_ratio", config.vm_dirty_ratio).await {
            warn!("Failed to apply vm.dirty_ratio: {e}");
        }
        if let Err(e) = apply_sysctl("vm.dirty_background_ratio", config.vm_dirty_background_ratio).await {
            warn!("Failed to apply vm.dirty_background_ratio: {e}");
        }
        if let Err(e) = apply_sysctl("vm.dirty_expire_centisecs", config.vm_dirty_expire_centisecs).await {
            warn!("Failed to apply vm.dirty_expire_centisecs: {e}");
        }
        if let Err(e) = apply_sysctl("vm.dirty_writeback_centisecs", config.vm_dirty_writeback_centisecs).await {
            warn!("Failed to apply vm.dirty_writeback_centisecs: {e}");
        }
        info!("Tuning configuration applied");
    }
}

// ── Apply helpers ────────────────────────────────────────────

async fn apply_nfs_threads(count: u32) -> Result<(), String> {
    tokio::fs::write("/proc/fs/nfsd/threads", count.to_string())
        .await
        .map_err(|e| format!("failed to set nfsd threads: {e}"))?;
    info!("Set nfsd threads to {count}");
    Ok(())
}

async fn apply_proc_value(path: &str, value: u32) -> Result<(), String> {
    tokio::fs::write(path, value.to_string())
        .await
        .map_err(|e| format!("failed to write {path}: {e}"))?;
    Ok(())
}

async fn apply_sysctl(key: &str, value: u32) -> Result<(), String> {
    let output = tokio::process::Command::new("sysctl")
        .args(["-w", &format!("{key}={value}")])
        .output()
        .await
        .map_err(|e| format!("sysctl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("sysctl {key}={value} failed: {stderr}"));
    }
    Ok(())
}

async fn apply_smb_tuning(config: &TuningConfig) -> Result<(), String> {
    // Build a Samba config fragment with tuning parameters
    let mut lines = vec!["[global]".to_string()];
    if config.smb_max_connections > 0 {
        lines.push(format!("   max connections = {}", config.smb_max_connections));
    }
    if config.smb_deadtime > 0 {
        lines.push(format!("   deadtime = {}", config.smb_deadtime));
    }
    if !config.smb_socket_options.is_empty() {
        lines.push(format!("   socket options = {}", config.smb_socket_options));
    }
    lines.push(String::new()); // trailing newline

    tokio::fs::write(SMB_TUNING_CONF, lines.join("\n"))
        .await
        .map_err(|e| format!("failed to write {SMB_TUNING_CONF}: {e}"))?;

    // Reload Samba config (non-fatal if smbd isn't running)
    let _ = tokio::process::Command::new("smbcontrol")
        .args(["smbd", "reload-config"])
        .output()
        .await;

    info!("SMB tuning config written and reload requested");
    Ok(())
}

async fn apply_iscsi_cmdsn_depth(depth: u32) -> Result<(), String> {
    // LIO default_cmdsn_depth is per-TPG; set on all existing TPGs
    let tpg_base = "/sys/kernel/config/target/iscsi";
    let entries = match tokio::fs::read_dir(tpg_base).await {
        Ok(e) => e,
        Err(_) => return Ok(()), // iSCSI not loaded or no targets
    };

    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let iqn_path = entry.path();
        if !iqn_path.is_dir() { continue; }
        let iqn_name = entry.file_name().to_string_lossy().to_string();
        if !iqn_name.starts_with("iqn.") { continue; }

        // Iterate TPGs within this IQN
        if let Ok(mut tpg_entries) = tokio::fs::read_dir(&iqn_path).await {
            while let Ok(Some(tpg)) = tpg_entries.next_entry().await {
                let tpg_name = tpg.file_name().to_string_lossy().to_string();
                if !tpg_name.starts_with("tpgt_") { continue; }
                let attr_path = tpg.path().join("attrib/default_cmdsn_depth");
                if attr_path.exists() {
                    let _ = tokio::fs::write(&attr_path, depth.to_string()).await;
                }
            }
        }
    }
    Ok(())
}

async fn apply_iscsi_login_timeout(timeout: u32) -> Result<(), String> {
    let tpg_base = "/sys/kernel/config/target/iscsi";
    let entries = match tokio::fs::read_dir(tpg_base).await {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let iqn_path = entry.path();
        if !iqn_path.is_dir() { continue; }
        let iqn_name = entry.file_name().to_string_lossy().to_string();
        if !iqn_name.starts_with("iqn.") { continue; }

        if let Ok(mut tpg_entries) = tokio::fs::read_dir(&iqn_path).await {
            while let Ok(Some(tpg)) = tpg_entries.next_entry().await {
                let tpg_name = tpg.file_name().to_string_lossy().to_string();
                if !tpg_name.starts_with("tpgt_") { continue; }
                let attr_path = tpg.path().join("param/login_timeout");
                if attr_path.exists() {
                    let _ = tokio::fs::write(&attr_path, timeout.to_string()).await;
                }
            }
        }
    }
    Ok(())
}

// ── Persistence ──────────────────────────────────────────────

async fn load() -> TuningConfig {
    match tokio::fs::read_to_string(STATE_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => TuningConfig::default(),
    }
}

async fn save(config: &TuningConfig) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(STATE_DIR).await?;
    let json = serde_json::to_string_pretty(config).unwrap();
    tokio::fs::write(STATE_PATH, json).await?;
    Ok(())
}
