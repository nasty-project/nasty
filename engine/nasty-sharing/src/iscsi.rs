use std::path::Path;

use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

const STATE_DIR: &str = "/var/lib/nasty/shares/iscsi";
const DEFAULT_IQN_PREFIX: &str = "iqn.2137-04.storage.nasty";

#[derive(Debug, Error)]
pub enum IscsiError {
    #[error("target not found: {0}")]
    NotFound(String),
    #[error("target already exists: {0}")]
    AlreadyExists(String),
    #[error("backing device/file not found: {0}")]
    BackstoreNotFound(String),
    #[error("path is not within a NASty pool: {0}")]
    PathNotInPool(String),
    #[error("targetcli command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IscsiTarget {
    /// Unique target identifier (UUID).
    pub id: String,
    /// iSCSI Qualified Name (e.g. `iqn.2137-04.storage.nasty:tank-vol`).
    pub iqn: String,
    /// Optional human-readable alias for the target.
    pub alias: Option<String>,
    /// Network portals (IP:port) the target listens on.
    pub portals: Vec<Portal>,
    /// Logical units exposed by this target.
    pub luns: Vec<Lun>,
    /// Initiator ACL entries controlling which hosts may connect.
    pub acls: Vec<Acl>,
    /// Whether the target is currently active in LIO.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Portal {
    /// IP address the portal listens on (use `0.0.0.0` for all interfaces).
    pub ip: String,
    /// TCP port number (default iSCSI port is 3260).
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Lun {
    pub lun_id: u32,
    /// Path to block device or file used as backstore
    pub backstore_path: String,
    /// LIO backstore name (auto-generated)
    pub backstore_name: String,
    /// "block" or "fileio"
    pub backstore_type: String,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Acl {
    /// Initiator IQN allowed to connect
    pub initiator_iqn: String,
    /// CHAP username for this initiator (optional).
    pub userid: Option<String>,
    /// CHAP password for this initiator (optional).
    pub password: Option<String>,
}

impl HasId for IscsiTarget {
    fn id(&self) -> &str {
        &self.id
    }
}

// ── Requests ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTargetRequest {
    /// Short name used to generate the IQN: iqn.2137-01.com.nasty:<name>
    pub name: String,
    /// Optional human-readable alias for the target.
    pub alias: Option<String>,
    /// Defaults to 0.0.0.0:3260
    pub portals: Option<Vec<Portal>>,
}

/// Simplified request: creates target + LUN in one shot
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuickCreateRequest {
    /// Short name for the IQN
    pub name: String,
    /// Block device path (e.g. /dev/loop0)
    pub device_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteTargetRequest {
    /// ID of the iSCSI target to delete.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddLunRequest {
    pub target_id: String,
    /// Block device path (/dev/sdb) or file path (/mnt/nasty/pool/disk.img)
    pub backstore_path: String,
    /// "block" or "fileio" — auto-detected if omitted
    pub backstore_type: Option<String>,
    /// Required for fileio if file doesn't exist yet
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveLunRequest {
    /// ID of the target from which to remove the LUN.
    pub target_id: String,
    /// LUN ID to remove.
    pub lun_id: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddAclRequest {
    /// ID of the target to add the ACL to.
    pub target_id: String,
    /// Initiator IQN to allow.
    pub initiator_iqn: String,
    /// Optional CHAP username for this initiator.
    pub userid: Option<String>,
    /// Optional CHAP password for this initiator.
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveAclRequest {
    /// ID of the target from which to remove the ACL.
    pub target_id: String,
    /// Initiator IQN to disallow.
    pub initiator_iqn: String,
}

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

// ── Service ─────────────────────────────────────────────────────

pub struct IscsiService;

impl IscsiService {
    pub fn new() -> Self {
        Self
    }

    /// Update persisted device paths after a reboot where loop device numbers changed.
    /// `dev_map` maps subvolume_name → current loop device (e.g. "vol1" → "/dev/loop0").
    /// The subvolume name is extracted from the IQN suffix after the last ':'.
    /// Also patches /etc/target/saveconfig.json so target.service loads with correct paths.
    pub async fn remap_device_paths(&self, dev_map: &std::collections::HashMap<String, String>) {
        let mut targets: Vec<IscsiTarget> = state_dir().load_all().await;
        for target in &mut targets {
            let name = target.iqn.rsplit(':').next().unwrap_or("").to_string();
            let Some(new_dev) = dev_map.get(&name) else { continue };
            let mut changed = false;
            for lun in &mut target.luns {
                if &lun.backstore_path != new_dev {
                    info!(
                        "Remapping iSCSI '{}' lun{} {} → {}",
                        target.iqn, lun.lun_id, lun.backstore_path, new_dev
                    );
                    lun.backstore_path = new_dev.clone();
                    changed = true;
                }
            }
            if changed {
                let _ = state_dir().save(&target.id, target).await;
            }
        }
        // Patch saveconfig.json so target.service picks up the corrected device on boot.
        patch_saveconfig(dev_map).await;
    }

    pub async fn list(&self) -> Result<Vec<IscsiTarget>, IscsiError> {

        Ok(state_dir().load_all().await)
    }

    pub async fn get(&self, id: &str) -> Result<IscsiTarget, IscsiError> {

        state_dir()
            .load::<IscsiTarget>(id)
            .await
            .ok_or_else(|| IscsiError::NotFound(id.to_string()))
    }

    pub async fn create(&self, req: CreateTargetRequest) -> Result<IscsiTarget, IscsiError> {

        let targets: Vec<IscsiTarget> = state_dir().load_all().await;
        let iqn = format!("{DEFAULT_IQN_PREFIX}:{}", req.name);

        if let Some(existing) = targets.into_iter().find(|t| t.iqn == iqn) {
            info!("iSCSI target {iqn} already exists, returning existing (idempotent)");
            return Ok(existing);
        }

        let portals = req.portals.unwrap_or_else(|| {
            vec![Portal {
                ip: "0.0.0.0".to_string(),
                port: 3260,
            }]
        });

        // Create the iSCSI target in LIO
        targetcli(&format!("/iscsi create {iqn}")).await?;

        // Create portals (tpg1 is created automatically)
        for portal in &portals {
            // Default portal 0.0.0.0:3260 is auto-created, skip it
            if portal.ip == "0.0.0.0" && portal.port == 3260 {
                continue;
            }
            targetcli(&format!(
                "/iscsi/{iqn}/tpg1/portals create {} {}",
                portal.ip, portal.port
            ))
            .await?;
        }

        // Disable authentication by default (can be enabled via ACLs)
        targetcli(&format!(
            "/iscsi/{iqn}/tpg1 set attribute authentication=0 demo_mode_write_protect=0 generate_node_acls=1"
        ))
        .await?;

        let target = IscsiTarget {
            id: Uuid::new_v4().to_string(),
            iqn: iqn.clone(),
            alias: req.alias,
            portals,
            luns: vec![],
            acls: vec![],
            enabled: true,
        };

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await?;

        info!("Created iSCSI target {iqn}");
        Ok(target)
    }

    /// Create a complete iSCSI target with a LUN in one step
    pub async fn create_quick(&self, req: QuickCreateRequest) -> Result<IscsiTarget, IscsiError> {
        // Create the target (idempotent — returns existing if name matches)
        let target = self.create(CreateTargetRequest {
            name: req.name,
            alias: None,
            portals: None,
        }).await?;

        // Skip adding LUN if one already exists (idempotent retry)
        let target = if target.luns.is_empty() {
            self.add_lun(AddLunRequest {
                target_id: target.id.clone(),
                backstore_path: req.device_path,
                backstore_type: Some("block".to_string()),
                size_bytes: None,
            }).await?
        } else {
            info!("iSCSI target {} already has {} LUN(s), skipping", target.iqn, target.luns.len());
            target
        };

        Ok(target)
    }

    pub async fn delete(&self, req: DeleteTargetRequest) -> Result<(), IscsiError> {

        let target: IscsiTarget = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.id.clone()))?;

        // Remove the target first — this releases all LUN mappings.
        // Backstores cannot be deleted while a target still references them.
        if let Err(e) = targetcli(&format!("/iscsi delete {}", target.iqn)).await {
            tracing::warn!("targetcli delete failed (may already be gone): {e}");
        }

        // Now remove backstores (loop devices are released here)
        for lun in &target.luns {
            let _ = targetcli(&format!(
                "/backstores/{} delete {}",
                lun.backstore_type, lun.backstore_name
            )).await;
        }

        state_dir().remove(&req.id).await?;
        let _ = save_lio_config().await;

        info!("Deleted iSCSI target '{}'", req.id);
        Ok(())
    }

    pub async fn add_lun(&self, req: AddLunRequest) -> Result<IscsiTarget, IscsiError> {

        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        let backstore_type = req.backstore_type.unwrap_or_else(|| {
            if Path::new(&req.backstore_path)
                .metadata()
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                "fileio".to_string()
            } else {
                "block".to_string()
            }
        });

        // Validate backstore path
        match backstore_type.as_str() {
            "block" => {
                if !Path::new(&req.backstore_path).exists() {
                    return Err(IscsiError::BackstoreNotFound(req.backstore_path));
                }
            }
            "fileio" => {
                // For fileio, the parent directory must exist
                if let Some(parent) = Path::new(&req.backstore_path).parent() {
                    if !parent.exists() {
                        return Err(IscsiError::BackstoreNotFound(
                            parent.to_string_lossy().to_string(),
                        ));
                    }
                }
            }
            _ => {
                return Err(IscsiError::CommandFailed(format!(
                    "Unknown backstore type: {backstore_type}"
                )));
            }
        }

        let lun_id = target
            .luns
            .iter()
            .map(|l| l.lun_id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        let backstore_name = format!(
            "nasty_{}_lun{}",
            target
                .iqn
                .rsplit(':')
                .next()
                .unwrap_or("unknown"),
            lun_id
        );

        // Create backstore
        match backstore_type.as_str() {
            "block" => {
                targetcli(&format!(
                    "/backstores/block create name={backstore_name} dev={}",
                    req.backstore_path
                ))
                .await?;
            }
            "fileio" => {
                let size = req.size_bytes.unwrap_or(1_073_741_824); // 1GB default
                targetcli(&format!(
                    "/backstores/fileio create name={backstore_name} file_or_dev={} size={size}",
                    req.backstore_path
                ))
                .await?;
            }
            _ => unreachable!(),
        }

        // Map LUN to target
        targetcli(&format!(
            "/iscsi/{}/tpg1/luns create /backstores/{backstore_type}/{backstore_name}",
            target.iqn
        ))
        .await?;

        let lun = Lun {
            lun_id,
            backstore_path: req.backstore_path,
            backstore_name,
            backstore_type,
            size_bytes: req.size_bytes,
        };

        target.luns.push(lun);

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await?;

        info!("Added LUN {} to target '{}'", target.luns.len() - 1, target.iqn);
        Ok(target)
    }

    pub async fn remove_lun(&self, req: RemoveLunRequest) -> Result<IscsiTarget, IscsiError> {

        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        let lun_idx = target
            .luns
            .iter()
            .position(|l| l.lun_id == req.lun_id)
            .ok_or_else(|| {
                IscsiError::NotFound(format!("LUN {} not found", req.lun_id))
            })?;

        let lun = &target.luns[lun_idx];

        // Remove LUN mapping
        let _ = targetcli(&format!(
            "/iscsi/{}/tpg1/luns delete lun{}",
            target.iqn, lun.lun_id
        ))
        .await;

        // Remove backstore
        let _ = targetcli(&format!(
            "/backstores/{} delete {}",
            lun.backstore_type, lun.backstore_name
        ))
        .await;

        target.luns.remove(lun_idx);

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await?;

        info!("Removed LUN {} from target '{}'", req.lun_id, target.iqn);
        Ok(target)
    }

    pub async fn add_acl(&self, req: AddAclRequest) -> Result<IscsiTarget, IscsiError> {

        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        targetcli(&format!(
            "/iscsi/{}/tpg1/acls create {}",
            target.iqn, req.initiator_iqn
        ))
        .await?;

        if let (Some(userid), Some(password)) = (&req.userid, &req.password) {
            targetcli(&format!(
                "/iscsi/{}/tpg1/acls/{} set auth userid={userid} password={password}",
                target.iqn, req.initiator_iqn
            ))
            .await?;
        }

        target.acls.push(Acl {
            initiator_iqn: req.initiator_iqn,
            userid: req.userid,
            password: req.password,
        });

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await?;

        info!("Added ACL to target '{}'", target.iqn);
        Ok(target)
    }

    pub async fn remove_acl(&self, req: RemoveAclRequest) -> Result<IscsiTarget, IscsiError> {

        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        targetcli(&format!(
            "/iscsi/{}/tpg1/acls delete {}",
            target.iqn, req.initiator_iqn
        ))
        .await?;

        target.acls.retain(|a| a.initiator_iqn != req.initiator_iqn);

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await?;

        info!("Removed ACL from target '{}'", target.iqn);
        Ok(target)
    }
}

// ── targetcli helpers ───────────────────────────────────────────

async fn targetcli(cmd: &str) -> Result<String, IscsiError> {
    let output = tokio::process::Command::new("targetcli")
        .args([cmd])
        .env("TARGETCLI_HOME", "/var/lib/nasty/.targetcli")
        .output()
        .await
        .map_err(|e| IscsiError::CommandFailed(format!("failed to execute targetcli: {e}")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(IscsiError::CommandFailed(format!(
            "targetcli `{cmd}` failed: {stderr} {stdout}"
        )))
    }
}

/// Save the running LIO config so it persists across reboots
async fn save_lio_config() -> Result<(), IscsiError> {
    targetcli("saveconfig").await?;
    Ok(())
}

/// Patch /etc/target/saveconfig.json to fix stale loop device paths.
/// storage_objects entries look like: {"dev": "/dev/loop2", "name": "nasty_<subvol>_lun0", ...}
/// We match by name containing the subvolume name and update the "dev" field.
async fn patch_saveconfig(dev_map: &std::collections::HashMap<String, String>) {
    const SAVECONFIG: &str = "/etc/target/saveconfig.json";
    let text = match tokio::fs::read_to_string(SAVECONFIG).await {
        Ok(t) => t,
        Err(_) => return, // file doesn't exist yet (iSCSI never used)
    };
    let mut json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => { warn!("Failed to parse {SAVECONFIG}: {e}"); return; }
    };
    let Some(objects) = json.get_mut("storage_objects").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let mut changed = false;
    for obj in objects.iter_mut() {
        let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // backstore names follow "nasty_<subvol_name>_lun<n>" convention
        for (subvol_name, new_dev) in dev_map {
            let expected_prefix = format!("nasty_{subvol_name}_");
            if name.starts_with(&expected_prefix) {
                if let Some(dev_field) = obj.get("dev").and_then(|v| v.as_str()) {
                    if dev_field != new_dev {
                        info!("Patching saveconfig.json backstore '{name}' {} → {new_dev}", dev_field);
                        obj["dev"] = serde_json::Value::String(new_dev.clone());
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        match serde_json::to_string_pretty(&json) {
            Ok(out) => { let _ = tokio::fs::write(SAVECONFIG, out).await; }
            Err(e) => warn!("Failed to serialize patched saveconfig.json: {e}"),
        }
    }
}

