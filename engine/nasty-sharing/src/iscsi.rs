use std::path::Path;

use nasty_common::secrets::{self, EncryptedBlob};
use nasty_common::{HasId, StateDir};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

const STATE_DIR: &str = "/var/lib/nasty/shares/iscsi";
const DEFAULT_IQN_PREFIX: &str = "iqn.2137-04.storage.nasty";
const ISCSI_BASE: &str = "/sys/kernel/config/target/iscsi";
const CORE_BASE: &str = "/sys/kernel/config/target/core";

#[derive(Debug, Error)]
pub enum IscsiError {
    #[error("target not found: {0}")]
    NotFound(String),
    #[error("target already exists: {0}")]
    AlreadyExists(String),
    #[error("backing device/file not found: {0}")]
    BackstoreNotFound(String),
    #[error("path is not within a NASty filesystem: {0}")]
    PathNotInPool(String),
    #[error("configfs error: {0}")]
    ConfigFs(String),
    #[error("command failed: {0}")]
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
    /// CHAP password for this initiator (optional). Legacy plaintext:
    /// encrypted into `password_encrypted` at rest when the secrets
    /// backend is healthy, and redacted to `***` in API responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// CHAP password encrypted at rest via systemd-creds. Populated by
    /// the engine when the secrets backend is available; preferred over
    /// the legacy plaintext `password`. (The live configfs auth and the
    /// kernel's saveconfig.json restore carry their own plaintext copy —
    /// this only seals NASty's state file.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_encrypted: Option<EncryptedBlob>,
}

impl Acl {
    /// Return a copy with the password redacted for API responses: `***`
    /// when a secret is set (plaintext or sealed), ciphertext omitted.
    pub fn redacted(&self) -> Self {
        let has_secret = self.password.is_some() || self.password_encrypted.is_some();
        Self {
            initiator_iqn: self.initiator_iqn.clone(),
            userid: self.userid.clone(),
            password: has_secret.then(|| "***".to_string()),
            password_encrypted: None,
        }
    }
}

/// `systemd-creds` AEAD name binding a CHAP password to this host,
/// target, and initiator.
fn chap_secret_name(target_id: &str, initiator_iqn: &str) -> String {
    format!("nasty.iscsi.{target_id}.{initiator_iqn}.password")
}

/// Seal a CHAP password for storage in NASty's state file. Returns the
/// `(password, password_encrypted)` pair to store: `(None, Some(blob))`
/// on success, `(Some(plain), None)` if the secrets backend is
/// unavailable (degraded — keep plaintext + warn), `(None, None)` when
/// no password was supplied.
async fn seal_chap_password(
    target_id: &str,
    initiator_iqn: &str,
    plaintext: Option<String>,
) -> (Option<String>, Option<EncryptedBlob>) {
    let Some(plain) = plaintext.filter(|s| !s.is_empty()) else {
        return (None, None);
    };
    match secrets::encrypt(&chap_secret_name(target_id, initiator_iqn), &plain).await {
        Ok(blob) => (None, Some(blob)),
        Err(e) => {
            warn!(
                "Failed to encrypt iSCSI CHAP password for {target_id}/{initiator_iqn} — keeping plaintext: {e}"
            );
            (Some(plain), None)
        }
    }
}

impl IscsiTarget {
    /// Return a copy with CHAP passwords redacted for API responses.
    pub fn redacted(mut self) -> Self {
        self.acls = self.acls.into_iter().map(|a| a.redacted()).collect();
        self
    }
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
    /// Block device path (e.g. /dev/loop0). When provided, a LUN is
    /// automatically created and the target is ready for connections.
    pub device_path: Option<String>,
    /// Initiator ACLs to set up. When provided, `generate_node_acls` is
    /// disabled and only these initiators are allowed.
    pub acls: Option<Vec<AclEntry>>,
}

/// ACL entry for the create request (avoids requiring target_id up front).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AclEntry {
    /// Initiator IQN to allow.
    pub initiator_iqn: String,
    /// Optional CHAP username.
    pub userid: Option<String>,
    /// Optional CHAP password.
    pub password: Option<String>,
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddPortalRequest {
    /// ID of the target to add the portal to.
    pub target_id: String,
    /// Listening IP address. `0.0.0.0` for all v4 interfaces, `::` for
    /// all v6 interfaces, or a specific host address.
    pub ip: String,
    /// TCP port to listen on. Standard iSCSI port is 3260.
    pub port: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemovePortalRequest {
    /// ID of the target from which to remove the portal.
    pub target_id: String,
    /// Listen address of the portal to remove. Must match the stored
    /// value exactly (no normalization).
    pub ip: String,
    /// TCP port of the portal to remove.
    pub port: u16,
}

fn state_dir() -> StateDir {
    StateDir::new(STATE_DIR)
}

// ── Service ─────────────────────────────────────────────────────

pub struct IscsiService;

impl Default for IscsiService {
    fn default() -> Self {
        Self::new()
    }
}

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
            let Some(new_dev) = dev_map.get(&name) else {
                continue;
            };
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
        patch_saveconfig(dev_map).await;
    }

    /// Eagerly seal plaintext CHAP passwords left in state files from
    /// before encrypt-at-rest. Called once on engine boot; idempotent,
    /// and a no-op when already sealed / empty / backend unavailable
    /// (boot does not fail). The live kernel auth is untouched.
    pub async fn migrate_secrets(&self) {
        let mut targets: Vec<IscsiTarget> = state_dir().load_all().await;
        for target in &mut targets {
            let mut changed = false;
            for acl in &mut target.acls {
                if acl.password_encrypted.is_some() {
                    continue;
                }
                let Some(plain) = acl.password.as_deref().filter(|s| !s.is_empty()) else {
                    continue;
                };
                let (password, password_encrypted) =
                    seal_chap_password(&target.id, &acl.initiator_iqn, Some(plain.to_string()))
                        .await;
                if password_encrypted.is_some() {
                    acl.password = password;
                    acl.password_encrypted = password_encrypted;
                    changed = true;
                }
            }
            if changed {
                match state_dir().save(&target.id, target).await {
                    Ok(()) => info!("Migrated iSCSI CHAP secrets for target '{}'", target.id),
                    Err(e) => {
                        warn!(
                            "Failed to persist migrated iSCSI secrets for '{}': {e}",
                            target.id
                        )
                    }
                }
            }
        }
    }

    pub async fn list(&self) -> Result<Vec<IscsiTarget>, IscsiError> {
        let targets: Vec<IscsiTarget> = state_dir().load_all().await;
        Ok(targets.into_iter().map(|t| t.redacted()).collect())
    }

    pub async fn get(&self, id: &str) -> Result<IscsiTarget, IscsiError> {
        state_dir()
            .load::<IscsiTarget>(id)
            .await
            .map(|t| t.redacted())
            .ok_or_else(|| IscsiError::NotFound(id.to_string()))
    }

    pub async fn create(&self, req: CreateTargetRequest) -> Result<IscsiTarget, IscsiError> {
        validate_target_name(&req.name)?;
        let targets: Vec<IscsiTarget> = state_dir().load_all().await;
        let iqn = format!("{DEFAULT_IQN_PREFIX}:{}", req.name);

        if let Some(existing) = targets.into_iter().find(|t| t.iqn == iqn) {
            info!("iSCSI target {iqn} already exists, returning existing (idempotent)");
            return Ok(existing);
        }

        // Split portals into "must succeed" and "best effort" buckets.
        // Operator-supplied portals are always mandatory — if they
        // typed `[::]:3260` and it can't bind, they need a loud error,
        // not silent v4-only. The default v6 portal is best-effort:
        // host_has_global_ipv6() catches the obvious "v4-only host"
        // case, but it can't catch the harder ones (LIO compiled
        // without v6, kernel iSCSI module loaded with ipv6 disabled,
        // sysctl `net.ipv6.bindv6only=0`, container runtime that
        // exposes v6 addresses but not v6 sockets). Those all surface
        // as EINVAL from configfs mkdir of `[::]:3260` and used to
        // wedge target creation entirely — observed in nasty-csi CI.
        let (mandatory_portals, opportunistic_v6) = match req.portals {
            Some(p) => (p, false),
            None => {
                let defaults = vec![Portal {
                    ip: "0.0.0.0".to_string(),
                    port: 3260,
                }];
                let try_v6 = crate::v6::host_has_global_ipv6().await;
                (defaults, try_v6)
            }
        };

        // Create target and TPG in configfs
        let tpg_path = format!("{ISCSI_BASE}/{iqn}/tpgt_1");
        configfs_mkdir(&tpg_path).await?;

        // Create mandatory portals first — any failure aborts the
        // whole create, but we've only created the TPG dir so far,
        // so cleanup is just rmdir'ing the TPG and the target dir.
        let mut created_portals = Vec::with_capacity(mandatory_portals.len() + 1);
        for portal in &mandatory_portals {
            let np_path = np_path_for(&tpg_path, &portal.ip, portal.port);
            if let Err(e) = configfs_mkdir(&np_path).await {
                // Best-effort cleanup of what we created so far so the
                // next attempt doesn't trip the idempotency check with
                // a stale half-target.
                let _ = configfs_rmdir(&tpg_path).await;
                let _ = configfs_rmdir(&format!("{ISCSI_BASE}/{iqn}")).await;
                return Err(e);
            }
            created_portals.push(portal.clone());
        }

        // Best-effort v6 portal — skipped on EINVAL/etc. so v4 stays
        // up on hosts where LIO can't bind v6. Operator can still add
        // a specific v6 portal manually via share.iscsi.add_portal,
        // which surfaces the real error (because that call is opted
        // into and must not silently no-op).
        if opportunistic_v6 {
            let v6_portal = Portal {
                ip: "::".to_string(),
                port: 3260,
            };
            let np_path = np_path_for(&tpg_path, &v6_portal.ip, v6_portal.port);
            match configfs_mkdir(&np_path).await {
                Ok(()) => created_portals.push(v6_portal),
                Err(e) => warn!(
                    "iSCSI dual-stack default [::]:3260 portal for {iqn} skipped: {e}; \
                     v4 portal is up, add a v6 portal manually if needed"
                ),
            }
        }

        // Disable authentication, allow any initiator, allow writes
        configfs_write(&format!("{tpg_path}/attrib/authentication"), "0").await?;
        configfs_write(&format!("{tpg_path}/attrib/generate_node_acls"), "1").await?;
        configfs_write(&format!("{tpg_path}/attrib/demo_mode_write_protect"), "0").await?;

        // Enable the TPG
        configfs_write(&format!("{tpg_path}/enable"), "1").await?;

        let target = IscsiTarget {
            id: Uuid::new_v4().to_string(),
            iqn: iqn.clone(),
            alias: req.alias,
            // Stored list is what actually exists in configfs, not what
            // the operator asked for — keeps state and reality in sync
            // when the opportunistic v6 portal got skipped.
            portals: created_portals,
            luns: vec![],
            acls: vec![],
            enabled: true,
        };

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        // Optional: add LUN if device_path was provided
        let mut target = target;
        if let Some(device_path) = req.device_path {
            if target.luns.is_empty() {
                target = self
                    .add_lun(AddLunRequest {
                        target_id: target.id.clone(),
                        backstore_path: device_path,
                        backstore_type: Some("block".to_string()),
                        size_bytes: None,
                    })
                    .await?;
            } else {
                info!(
                    "iSCSI target {} already has {} LUN(s), skipping",
                    target.iqn,
                    target.luns.len()
                );
            }
        }

        // Optional: add ACLs if provided
        if let Some(acls) = req.acls {
            for acl_entry in acls {
                target = self
                    .add_acl(AddAclRequest {
                        target_id: target.id.clone(),
                        initiator_iqn: acl_entry.initiator_iqn,
                        userid: acl_entry.userid,
                        password: acl_entry.password,
                    })
                    .await?;
            }
        }

        // Wait for target readiness when a LUN was attached
        if !target.luns.is_empty() {
            wait_for_target_ready(&target.iqn).await;
        }

        info!("Created iSCSI target {iqn}");
        Ok(target.redacted())
    }

    pub async fn delete(&self, req: DeleteTargetRequest) -> Result<(), IscsiError> {
        let target: IscsiTarget = state_dir()
            .load(&req.id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.id.clone()))?;

        let tpg_path = format!("{ISCSI_BASE}/{}/tpgt_1", target.iqn);

        // Disable TPG first — signals initiators to disconnect
        let _ = configfs_write(&format!("{tpg_path}/enable"), "0").await;

        // Brief settle for initiators to process disconnect
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Remove ACL dirs first (must be empty before TPG removal)
        for acl in &target.acls {
            let acl_path = format!("{tpg_path}/acls/{}", acl.initiator_iqn);
            let _ = configfs_rmdir(&acl_path).await;
        }

        // Unlink and remove LUN dirs
        for lun in &target.luns {
            let lun_path = format!("{tpg_path}/lun/lun_{}", lun.lun_id);
            // Remove the backstore symlink inside the LUN dir
            let link = format!("{lun_path}/{}", lun.backstore_name);
            let _ = configfs_unlink(&link).await;
            let _ = configfs_rmdir(&lun_path).await;
        }

        // Remove portals
        for portal in &target.portals {
            let np_path = np_path_for(&tpg_path, &portal.ip, portal.port);
            let _ = configfs_rmdir(&np_path).await;
        }

        // Remove TPG, then target
        let _ = configfs_rmdir(&tpg_path).await;
        let _ = configfs_rmdir(&format!("{ISCSI_BASE}/{}", target.iqn)).await;

        // Remove backstores
        for lun in &target.luns {
            let hba_type = backstore_hba_type(&lun.backstore_type);
            // Find which HBA index this backstore lives under
            if let Some(hba_idx) = find_backstore_hba(hba_type, &lun.backstore_name).await {
                let bs_path = format!("{CORE_BASE}/{hba_type}_{hba_idx}/{}", lun.backstore_name);
                let _ = configfs_write(&format!("{bs_path}/enable"), "0").await;
                let _ = configfs_rmdir(&bs_path).await;
                // Remove the HBA dir if empty (only has hba_info and hba_mode)
                let hba_path = format!("{CORE_BASE}/{hba_type}_{hba_idx}");
                if hba_is_empty(&hba_path).await {
                    let _ = configfs_rmdir(&hba_path).await;
                }
            }
        }

        state_dir().remove(&req.id).await?;
        save_lio_config().await;

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
                if let Some(parent) = Path::new(&req.backstore_path).parent()
                    && !parent.exists()
                {
                    return Err(IscsiError::BackstoreNotFound(
                        parent.to_string_lossy().to_string(),
                    ));
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
            target.iqn.rsplit(':').next().unwrap_or("unknown"),
            lun_id
        );

        let hba_type = backstore_hba_type(&backstore_type);
        let hba_idx = next_hba_index(hba_type).await;

        // Create backstore in configfs
        let bs_path = format!("{CORE_BASE}/{hba_type}_{hba_idx}/{backstore_name}");
        configfs_mkdir(&bs_path).await?;

        match backstore_type.as_str() {
            "block" => {
                configfs_write(
                    &format!("{bs_path}/control"),
                    &format!("udev_path={}", req.backstore_path),
                )
                .await?;
            }
            "fileio" => {
                let size = req.size_bytes.unwrap_or(1_073_741_824);
                configfs_write(
                    &format!("{bs_path}/control"),
                    &format!("fd_dev_name={},fd_dev_size={size}", req.backstore_path),
                )
                .await?;
            }
            _ => unreachable!(),
        }

        configfs_write(&format!("{bs_path}/enable"), "1").await?;

        // Create LUN in TPG and symlink to backstore
        let lun_path = format!("{ISCSI_BASE}/{}/tpgt_1/lun/lun_{lun_id}", target.iqn);
        configfs_mkdir(&lun_path).await?;
        configfs_symlink(&bs_path, &format!("{lun_path}/{backstore_name}")).await?;

        let lun = Lun {
            lun_id,
            backstore_path: req.backstore_path,
            backstore_name,
            backstore_type,
            size_bytes: req.size_bytes,
        };

        target.luns.push(lun);

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        info!(
            "Added LUN {} to target '{}'",
            target.luns.len() - 1,
            target.iqn
        );
        Ok(target.redacted())
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
            .ok_or_else(|| IscsiError::NotFound(format!("LUN {} not found", req.lun_id)))?;

        let lun = &target.luns[lun_idx];

        // Remove symlink and LUN dir
        let lun_path = format!("{ISCSI_BASE}/{}/tpgt_1/lun/lun_{}", target.iqn, lun.lun_id);
        let _ = configfs_unlink(&format!("{lun_path}/{}", lun.backstore_name)).await;
        let _ = configfs_rmdir(&lun_path).await;

        // Remove backstore
        let hba_type = backstore_hba_type(&lun.backstore_type);
        if let Some(hba_idx) = find_backstore_hba(hba_type, &lun.backstore_name).await {
            let bs_path = format!("{CORE_BASE}/{hba_type}_{hba_idx}/{}", lun.backstore_name);
            let _ = configfs_write(&format!("{bs_path}/enable"), "0").await;
            let _ = configfs_rmdir(&bs_path).await;
            let hba_path = format!("{CORE_BASE}/{hba_type}_{hba_idx}");
            if hba_is_empty(&hba_path).await {
                let _ = configfs_rmdir(&hba_path).await;
            }
        }

        target.luns.remove(lun_idx);

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        info!("Removed LUN {} from target '{}'", req.lun_id, target.iqn);
        Ok(target.redacted())
    }

    pub async fn add_acl(&self, req: AddAclRequest) -> Result<IscsiTarget, IscsiError> {
        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        let tpg_path = format!("{ISCSI_BASE}/{}/tpgt_1", target.iqn);
        let acl_path = format!("{tpg_path}/acls/{}", req.initiator_iqn);
        configfs_mkdir(&acl_path).await?;

        if let (Some(userid), Some(password)) = (&req.userid, &req.password) {
            configfs_write(&format!("{acl_path}/auth/userid"), userid).await?;
            configfs_write_secret(&format!("{acl_path}/auth/password"), password).await?;
        }

        // Disable generate_node_acls when explicit ACLs are added
        configfs_write(&format!("{tpg_path}/attrib/generate_node_acls"), "0").await?;
        configfs_write(&format!("{tpg_path}/attrib/authentication"), "0").await?;

        // configfs already has the plaintext (written above); seal the
        // copy we persist in NASty's state file.
        let (password, password_encrypted) =
            seal_chap_password(&target.id, &req.initiator_iqn, req.password).await;
        target.acls.push(Acl {
            initiator_iqn: req.initiator_iqn,
            userid: req.userid,
            password,
            password_encrypted,
        });

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        info!("Added ACL to target '{}'", target.iqn);
        Ok(target.redacted())
    }

    pub async fn remove_acl(&self, req: RemoveAclRequest) -> Result<IscsiTarget, IscsiError> {
        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        let tpg_path = format!("{ISCSI_BASE}/{}/tpgt_1", target.iqn);
        let acl_path = format!("{tpg_path}/acls/{}", req.initiator_iqn);
        let _ = configfs_rmdir(&acl_path).await;

        target.acls.retain(|a| a.initiator_iqn != req.initiator_iqn);

        // Re-enable generate_node_acls if no ACLs remain
        if target.acls.is_empty() {
            configfs_write(&format!("{tpg_path}/attrib/generate_node_acls"), "1").await?;
        }

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        info!("Removed ACL from target '{}'", target.iqn);
        Ok(target.redacted())
    }

    pub async fn add_portal(&self, req: AddPortalRequest) -> Result<IscsiTarget, IscsiError> {
        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        let ip = validate_portal_ip(&req.ip)?;

        if target
            .portals
            .iter()
            .any(|p| p.ip == ip && p.port == req.port)
        {
            info!(
                "Portal {ip}:{} already exists on target '{}', skipping",
                req.port, target.iqn
            );
            return Ok(target.redacted());
        }

        let tpg_path = format!("{ISCSI_BASE}/{}/tpgt_1", target.iqn);
        let np_path = np_path_for(&tpg_path, &ip, req.port);
        configfs_mkdir(&np_path).await?;

        target.portals.push(Portal {
            ip: ip.clone(),
            port: req.port,
        });

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        info!("Added portal {ip}:{} to target '{}'", req.port, target.iqn);
        Ok(target.redacted())
    }

    pub async fn remove_portal(&self, req: RemovePortalRequest) -> Result<IscsiTarget, IscsiError> {
        let mut target: IscsiTarget = state_dir()
            .load(&req.target_id)
            .await
            .ok_or_else(|| IscsiError::NotFound(req.target_id.clone()))?;

        let portal_idx = target
            .portals
            .iter()
            .position(|p| p.ip == req.ip && p.port == req.port)
            .ok_or_else(|| {
                IscsiError::NotFound(format!("portal {}:{} not found", req.ip, req.port))
            })?;

        // Refuse to remove the last portal — a target with zero portals
        // is unreachable, and the engine has no UI to recover from that
        // state short of re-adding via API. Operators wanting to swap
        // portals should add the replacement first, then remove the old.
        if target.portals.len() == 1 {
            return Err(IscsiError::CommandFailed(
                "Cannot remove the last portal — target would become unreachable. Add a replacement portal first.".to_string(),
            ));
        }

        let tpg_path = format!("{ISCSI_BASE}/{}/tpgt_1", target.iqn);
        let np_path = np_path_for(&tpg_path, &req.ip, req.port);
        let _ = configfs_rmdir(&np_path).await;

        target.portals.remove(portal_idx);

        state_dir().save(&target.id, &target).await?;
        save_lio_config().await;

        info!(
            "Removed portal {}:{} from target '{}'",
            req.ip, req.port, target.iqn
        );
        Ok(target.redacted())
    }
}

// ── configfs helpers ────────────────────────────────────────────

async fn configfs_mkdir(path: &str) -> Result<(), IscsiError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|e| IscsiError::ConfigFs(format!("mkdir {path}: {e}")))
}

async fn configfs_rmdir(path: &str) -> Result<(), IscsiError> {
    tokio::fs::remove_dir(path)
        .await
        .map_err(|e| IscsiError::ConfigFs(format!("rmdir {path}: {e}")))
}

async fn configfs_write(path: &str, value: &str) -> Result<(), IscsiError> {
    tokio::fs::write(path, value)
        .await
        .map_err(|e| IscsiError::ConfigFs(format!("write {path}={value}: {e}")))
}

/// Like `configfs_write` but redacts `value` in the error message so
/// secret payloads (CHAP passwords, future DH-CHAP keys) don't end up
/// in tracing output / journald. Use for any write where the value is
/// confidential — the path stays in the error so the operator can
/// still tell which target/ACL the failure was on.
async fn configfs_write_secret(path: &str, value: &str) -> Result<(), IscsiError> {
    tokio::fs::write(path, value)
        .await
        .map_err(|e| IscsiError::ConfigFs(format!("write {path}=<redacted>: {e}")))
}

async fn configfs_symlink(target: &str, link: &str) -> Result<(), IscsiError> {
    tokio::fs::symlink(target, link)
        .await
        .map_err(|e| IscsiError::ConfigFs(format!("symlink {link} -> {target}: {e}")))
}

async fn configfs_unlink(path: &str) -> Result<(), IscsiError> {
    tokio::fs::remove_file(path)
        .await
        .map_err(|e| IscsiError::ConfigFs(format!("unlink {path}: {e}")))
}

/// Map our backstore type names to LIO HBA type prefixes.
fn backstore_hba_type(bs_type: &str) -> &str {
    match bs_type {
        "block" => "iblock",
        "fileio" => "fileio",
        _ => "iblock",
    }
}

/// Find the next available HBA index by scanning /sys/kernel/config/target/core/
async fn next_hba_index(hba_type: &str) -> u32 {
    let mut max_idx: Option<u32> = None;
    if let Ok(mut entries) = tokio::fs::read_dir(CORE_BASE).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                let prefix = format!("{hba_type}_");
                if let Some(suffix) = name.strip_prefix(&prefix)
                    && let Ok(idx) = suffix.parse::<u32>()
                {
                    max_idx = Some(max_idx.map_or(idx, |m: u32| m.max(idx)));
                }
            }
        }
    }
    max_idx.map_or(0, |m| m + 1)
}

/// Find which HBA index contains a named backstore.
async fn find_backstore_hba(hba_type: &str, bs_name: &str) -> Option<u32> {
    if let Ok(mut entries) = tokio::fs::read_dir(CORE_BASE).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                let prefix = format!("{hba_type}_");
                if let Some(suffix) = name.strip_prefix(&prefix)
                    && let Ok(idx) = suffix.parse::<u32>()
                {
                    let bs_path = format!("{CORE_BASE}/{name}/{bs_name}");
                    if Path::new(&bs_path).exists() {
                        return Some(idx);
                    }
                }
            }
        }
    }
    None
}

/// Check if an HBA directory contains no backstores (only hba_info and hba_mode).
async fn hba_is_empty(hba_path: &str) -> bool {
    let mut count = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(hba_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let name = name.to_str().unwrap_or("");
            if name != "hba_info" && name != "hba_mode" {
                return false;
            }
            count += 1;
        }
    }
    count <= 2
}

/// Save the running LIO config so it persists across reboots.
/// Uses targetcli saveconfig — the only remaining targetcli dependency.
async fn save_lio_config() {
    let result = tokio::process::Command::new("targetcli")
        .args(["saveconfig"])
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("targetcli saveconfig failed: {stderr}");
        }
        Err(e) => warn!("Failed to run targetcli saveconfig: {e}"),
    }
}

/// Wait for an iSCSI target to be ready for initiator connections.
async fn wait_for_target_ready(iqn: &str) {
    let tpg_path = format!("{ISCSI_BASE}/{iqn}/tpgt_1/enable");

    for attempt in 1..=10 {
        match tokio::fs::read_to_string(&tpg_path).await {
            Ok(val) if val.trim() == "1" => {
                info!("iSCSI target {iqn} is ready (attempt {attempt})");
                return;
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    warn!("iSCSI target {iqn} readiness check timed out — proceeding anyway");
}

/// Patch /etc/target/saveconfig.json to fix stale loop device paths.
async fn patch_saveconfig(dev_map: &std::collections::HashMap<String, String>) {
    const SAVECONFIG: &str = "/etc/target/saveconfig.json";
    let text = match tokio::fs::read_to_string(SAVECONFIG).await {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse {SAVECONFIG}: {e}");
            return;
        }
    };
    let Some(objects) = json
        .get_mut("storage_objects")
        .and_then(|v| v.as_array_mut())
    else {
        return;
    };
    let mut changed = false;
    for obj in objects.iter_mut() {
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        for (subvol_name, new_dev) in dev_map {
            let expected_prefix = format!("nasty_{subvol_name}_");
            if name.starts_with(&expected_prefix)
                && let Some(dev_field) = obj.get("dev").and_then(|v| v.as_str())
                && dev_field != new_dev
            {
                info!(
                    "Patching saveconfig.json backstore '{name}' {} → {new_dev}",
                    dev_field
                );
                obj["dev"] = serde_json::Value::String(new_dev.clone());
                changed = true;
            }
        }
    }
    if changed {
        match serde_json::to_string_pretty(&json) {
            Ok(out) => {
                if let Err(e) = tokio::fs::write(SAVECONFIG, out).await {
                    // The patched config exists in memory but won't survive
                    // a restart — log so a "iSCSI config keeps reverting"
                    // bug is debuggable from a single message.
                    warn!("write patched saveconfig to {SAVECONFIG} failed: {e}");
                }
            }
            Err(e) => warn!("Failed to serialize patched saveconfig.json: {e}"),
        }
    }
}

/// Build the configfs `np/<addr>` path for a portal. LIO's configfs
/// expects IPv6 addresses bracketed (`[::]:3260`) so the `:` between
/// address and port stays unambiguous; IPv4 stays bare.
fn np_path_for(tpg_path: &str, ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("{tpg_path}/np/[{ip}]:{port}")
    } else {
        format!("{tpg_path}/np/{ip}:{port}")
    }
}

/// Validate the user-supplied portion of an iSCSI target name. The
/// engine builds the full IQN as `iqn.2137-04.storage.nasty:<name>` and
/// then uses that string as a configfs directory name and a key in
/// state files. RFC 3720 allows lowercase ASCII letters, digits, and
/// `-`, `.`, `:` in the user-suffix — we accept the same set plus
/// uppercase (LIO is case-insensitive in practice) and `_` (common in
/// existing operator naming conventions). Reject everything else,
/// notably `/` (would escape the configfs subsystem dir) and control
/// characters (would smuggle newlines into saveconfig.json).
fn validate_target_name(name: &str) -> Result<(), IscsiError> {
    if name.is_empty() {
        return Err(IscsiError::CommandFailed(
            "iSCSI target name is empty".to_string(),
        ));
    }
    if name.len() > 200 {
        return Err(IscsiError::CommandFailed(
            "iSCSI target name exceeds 200 chars".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
    {
        return Err(IscsiError::CommandFailed(format!(
            "iSCSI target name '{name}' contains invalid characters \
             (allowed: A-Z, a-z, 0-9, '-', '.', '_', ':')"
        )));
    }
    Ok(())
}

/// Validate a portal IP and return it normalized (whitespace-trimmed,
/// brackets stripped if the operator typed `[fd00::1]`). Both forms
/// reach the engine in practice — the WebUI sends bare addresses, but
/// curl users often copy the bracketed form from documentation.
fn validate_portal_ip(ip: &str) -> Result<String, IscsiError> {
    let trimmed = ip.trim();
    if trimmed.is_empty() {
        return Err(IscsiError::CommandFailed("portal IP is empty".to_string()));
    }
    let unwrapped = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if unwrapped.parse::<std::net::IpAddr>().is_err() {
        return Err(IscsiError::CommandFailed(format!(
            "portal IP '{ip}' is not a valid IPv4 or IPv6 address"
        )));
    }
    Ok(unwrapped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_blob() -> EncryptedBlob {
        serde_json::from_str("\"c2VhbGVkLWJsb2I=\"").unwrap()
    }

    fn acl_with(password: Option<&str>, encrypted: bool) -> Acl {
        Acl {
            initiator_iqn: "iqn.2020-01.com.example:init".into(),
            userid: Some("chapuser".into()),
            password: password.map(|s| s.to_string()),
            password_encrypted: encrypted.then(fake_blob),
        }
    }

    #[test]
    fn acl_redacted_masks_plaintext_password() {
        let r = acl_with(Some("chap-secret"), false).redacted();
        assert_eq!(r.password.as_deref(), Some("***"));
        assert!(r.password_encrypted.is_none());
    }

    #[test]
    fn acl_redacted_masks_encrypted_only_password() {
        let r = acl_with(None, true).redacted();
        assert_eq!(r.password.as_deref(), Some("***"));
        assert!(r.password_encrypted.is_none());
    }

    #[test]
    fn acl_redacted_leaves_passwordless_acl_clear() {
        let r = acl_with(None, false).redacted();
        assert!(r.password.is_none());
        assert!(r.password_encrypted.is_none());
    }

    #[test]
    fn acl_loads_legacy_state_without_encrypted_field() {
        let json = r#"{ "initiator_iqn": "iqn.x:y", "userid": "u", "password": "legacy" }"#;
        let acl: Acl = serde_json::from_str(json).unwrap();
        assert_eq!(acl.password.as_deref(), Some("legacy"));
        assert!(acl.password_encrypted.is_none());
    }

    #[test]
    fn np_path_brackets_v6_addresses() {
        assert_eq!(
            np_path_for("/sys/.../tpgt_1", "0.0.0.0", 3260),
            "/sys/.../tpgt_1/np/0.0.0.0:3260"
        );
        assert_eq!(
            np_path_for("/sys/.../tpgt_1", "192.168.1.10", 3260),
            "/sys/.../tpgt_1/np/192.168.1.10:3260"
        );
        assert_eq!(
            np_path_for("/sys/.../tpgt_1", "::", 3260),
            "/sys/.../tpgt_1/np/[::]:3260"
        );
        assert_eq!(
            np_path_for("/sys/.../tpgt_1", "fd00::1", 3260),
            "/sys/.../tpgt_1/np/[fd00::1]:3260"
        );
        assert_eq!(
            np_path_for("/sys/.../tpgt_1", "2001:db8::1", 8260),
            "/sys/.../tpgt_1/np/[2001:db8::1]:8260"
        );
    }

    #[test]
    fn validate_portal_ip_accepts_v4_and_v6() {
        assert_eq!(validate_portal_ip("0.0.0.0").unwrap(), "0.0.0.0");
        assert_eq!(validate_portal_ip("192.168.1.10").unwrap(), "192.168.1.10");
        assert_eq!(validate_portal_ip("::").unwrap(), "::");
        assert_eq!(validate_portal_ip("fd00::1").unwrap(), "fd00::1");
        assert_eq!(validate_portal_ip("2001:db8::1").unwrap(), "2001:db8::1");
    }

    #[test]
    fn validate_portal_ip_strips_brackets() {
        // Operators copying `[fd00::1]` from RFC examples should still
        // get accepted — strip the brackets and store the canonical form
        // so list/remove lookups stay consistent.
        assert_eq!(validate_portal_ip("[fd00::1]").unwrap(), "fd00::1");
        assert_eq!(validate_portal_ip("[::]").unwrap(), "::");
    }

    #[test]
    fn validate_portal_ip_trims_whitespace() {
        assert_eq!(validate_portal_ip("  10.0.0.1  ").unwrap(), "10.0.0.1");
    }

    #[test]
    fn validate_portal_ip_rejects_garbage() {
        assert!(validate_portal_ip("").is_err());
        assert!(validate_portal_ip("   ").is_err());
        assert!(validate_portal_ip("not.an.ip").is_err());
        assert!(validate_portal_ip("192.168.1").is_err());
        assert!(validate_portal_ip("fd00::xyz").is_err());
        // Hostname rejected — portal must be a numeric address; iSCSI
        // initiators do their own DNS but the engine writes a numeric
        // address into configfs.
        assert!(validate_portal_ip("example.com").is_err());
    }

    #[test]
    fn validate_target_name_accepts_typical_names() {
        assert!(validate_target_name("tank").is_ok());
        assert!(validate_target_name("DB-Server-01").is_ok());
        assert!(validate_target_name("vmware.cluster.prod").is_ok());
        assert!(validate_target_name("backup_2024").is_ok());
        // Colon is allowed in the user-suffix per RFC 3720; some shops
        // use it to mirror their hostname:purpose convention.
        assert!(validate_target_name("host:purpose").is_ok());
    }

    #[test]
    fn validate_target_name_rejects_configfs_escape() {
        // Slash would escape the configfs subsystem directory,
        // potentially creating /sys/.../tpgt_1/etc/...
        assert!(validate_target_name("../escape").is_err());
        assert!(validate_target_name("with/slash").is_err());
        assert!(validate_target_name("with\\backslash").is_err());
    }

    #[test]
    fn validate_target_name_rejects_control_chars_and_whitespace() {
        // Newlines would smuggle JSON entries into saveconfig.json;
        // spaces break shell quoting in legacy targetcli operations.
        assert!(validate_target_name("with newline\n").is_err());
        assert!(validate_target_name("with tab\t").is_err());
        assert!(validate_target_name("with space").is_err());
        assert!(validate_target_name("with\x00null").is_err());
    }

    #[test]
    fn validate_target_name_rejects_empty_and_oversize() {
        assert!(validate_target_name("").is_err());
        assert!(validate_target_name(&"x".repeat(201)).is_err());
        assert!(validate_target_name(&"x".repeat(200)).is_ok());
    }

    #[tokio::test]
    async fn configfs_write_secret_redacts_value_in_error() {
        // Target a path that's guaranteed to fail (no /sys outside Linux,
        // and on Linux the test process won't have configfs mounted at a
        // bogus path). The failure path is what we care about — we want
        // to confirm the resulting error message names the path but does
        // NOT contain the secret value.
        let bogus_path = "/sys/this/does/not/exist/auth/password";
        let secret = "super-secret-chap-password-do-not-leak";

        let err = configfs_write_secret(bogus_path, secret)
            .await
            .expect_err("write to nonexistent path must fail");
        let msg = format!("{err}");

        assert!(
            msg.contains(bogus_path),
            "error must surface the path so operator can identify the target: {msg}"
        );
        assert!(
            msg.contains("<redacted>"),
            "error must mark the value as redacted: {msg}"
        );
        assert!(
            !msg.contains(secret),
            "SECURITY: error message leaked the secret value: {msg}"
        );
    }

    #[tokio::test]
    async fn configfs_write_keeps_value_in_error_for_diagnostics() {
        // The non-secret variant intentionally includes the value in
        // the error — it's how operators debug "why did the write of
        // '0' to /sys/.../enable fail?". Pin that behavior so a future
        // over-eager redaction pass doesn't lose the diagnostic.
        let bogus_path = "/sys/this/does/not/exist/enable";
        let non_secret_value = "1";

        let err = configfs_write(bogus_path, non_secret_value)
            .await
            .expect_err("write to nonexistent path must fail");
        let msg = format!("{err}");

        assert!(msg.contains(bogus_path));
        assert!(
            msg.contains(non_secret_value),
            "non-secret values must surface in errors for diagnostics: {msg}"
        );
    }
}
