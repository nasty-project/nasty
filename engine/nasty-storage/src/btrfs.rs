//! btrfs filesystem management (nastty extension).
//!
//! Additive module: mirrors the shape of `filesystem.rs` (bcachefs) so a
//! server can dispatch `fs.*` calls per backend. Lives in its own file —
//! the only upstream touch is the `mod` line in `lib.rs` — to keep
//! rebases onto upstream trivial.

use std::collections::BTreeMap;
use std::path::Path;

use nasty_common::cmd::run_ok;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const MOUNT_BASE: &str = "/fs";
const STATE_PATH: &str = "/var/lib/nasty/btrfs-state.json";
/// Read-only snapshots live under this directory at the filesystem root.
const SNAPSHOT_DIR: &str = ".snapshots";

static STATE_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug)]
pub enum BtrfsError {
    CommandFailed(String),
    NotFound(String),
    InvalidInput(String),
}

impl std::fmt::Display for BtrfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtrfsError::CommandFailed(e) => write!(f, "btrfs command failed: {e}"),
            BtrfsError::NotFound(e) => write!(f, "not found: {e}"),
            BtrfsError::InvalidInput(e) => write!(f, "invalid input: {e}"),
        }
    }
}

impl std::error::Error for BtrfsError {}

type Result<T> = std::result::Result<T, BtrfsError>;

// ── wire types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BtrfsFilesystem {
    pub name: String,
    pub uuid: String,
    /// Member device paths.
    pub devices: Vec<String>,
    pub mount_point: Option<String>,
    pub mounted: bool,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    /// Data/metadata profile (e.g. `single`, `raid1`).
    pub raid: Option<String>,
    /// Mount-time compression (e.g. `zstd`).
    pub compression: Option<String>,
    /// Constant `"btrfs"`, so mixed `fs.list` results are tellable apart.
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateBtrfsRequest {
    /// Name for the new filesystem; becomes the mount point under `/fs/`.
    pub name: String,
    /// Devices to include.
    pub devices: Vec<String>,
    /// Data+metadata profile: `single` (default), `raid0`, `raid1`, `raid10`.
    pub raid: Option<String>,
    /// Compression mount option (e.g. `zstd`, `lzo`); omit for none.
    pub compression: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BtrfsSubvolume {
    pub id: u64,
    /// Path relative to the filesystem root (e.g. `data` or `media/tv`).
    pub path: String,
    /// Same as `path` — kept so mixed listings render uniformly with
    /// bcachefs subvolumes (which are keyed by `name`).
    pub name: String,
    pub filesystem: String,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BtrfsSnapshot {
    pub id: u64,
    /// Snapshot name (`<subvolume>@<label>`).
    pub name: String,
    /// The subvolume this snapshot was taken from, when derivable.
    pub subvolume: Option<String>,
    pub filesystem: String,
    pub backend: String,
}


// ── persisted state ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredFs {
    uuid: String,
    raid: Option<String>,
    compression: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BtrfsState {
    /// Keyed by filesystem name.
    filesystems: BTreeMap<String, StoredFs>,
}

async fn load_state() -> BtrfsState {
    nasty_common::load_singleton_or_recover(STATE_PATH).await
}

async fn save_state(state: &BtrfsState) -> Result<()> {
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| BtrfsError::CommandFailed(format!("serialize state: {e}")))?;
    if let Some(dir) = Path::new(STATE_PATH).parent()
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        // The write below fails with full context; this is just a breadcrumb.
        tracing::trace!("create {}: {e}", dir.display());
    }
    let tmp = format!("{STATE_PATH}.tmp");
    tokio::fs::write(&tmp, text)
        .await
        .map_err(|e| BtrfsError::CommandFailed(format!("write {tmp}: {e}")))?;
    tokio::fs::rename(&tmp, STATE_PATH)
        .await
        .map_err(|e| BtrfsError::CommandFailed(format!("persist {STATE_PATH}: {e}")))
}

// ── service ─────────────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct BtrfsService;

impl BtrfsService {
    pub fn new() -> Self {
        Self
    }

    /// All known btrfs filesystems (from persisted state), with live
    /// mount/usage/device info where obtainable.
    pub async fn list(&self) -> Result<Vec<BtrfsFilesystem>> {
        let state = load_state().await;
        let mounts = proc_mounts().await;
        let mut out = Vec::new();
        for (name, stored) in &state.filesystems {
            out.push(self.assemble(name, stored, &mounts).await);
        }
        Ok(out)
    }

    pub async fn get(&self, name: &str) -> Result<BtrfsFilesystem> {
        let state = load_state().await;
        let stored = state
            .filesystems
            .get(name)
            .ok_or_else(|| BtrfsError::NotFound(format!("btrfs filesystem '{name}'")))?;
        let mounts = proc_mounts().await;
        Ok(self.assemble(name, stored, &mounts).await)
    }

    /// Whether `name` is a btrfs filesystem this service manages.
    pub async fn manages(&self, name: &str) -> bool {
        load_state().await.filesystems.contains_key(name)
    }

    pub async fn create(&self, req: CreateBtrfsRequest) -> Result<BtrfsFilesystem> {
        validate_name(&req.name)?;
        if req.devices.is_empty() {
            return Err(BtrfsError::InvalidInput("no devices given".into()));
        }
        for d in &req.devices {
            if !Path::new(d).exists() {
                return Err(BtrfsError::InvalidInput(format!(
                    "device {d} does not exist"
                )));
            }
        }
        if load_state().await.filesystems.contains_key(&req.name) {
            return Err(BtrfsError::InvalidInput(format!(
                "filesystem '{}' already exists",
                req.name
            )));
        }
        let raid = req.raid.clone().unwrap_or_else(|| "single".to_string());
        if !matches!(raid.as_str(), "single" | "raid0" | "raid1" | "raid10") {
            return Err(BtrfsError::InvalidInput(format!(
                "unsupported raid profile '{raid}'"
            )));
        }

        let mut args: Vec<&str> = vec!["-f", "-L", &req.name];
        if raid != "single" {
            args.extend(["-d", &raid, "-m", &raid]);
        }
        let device_refs: Vec<&str> = req.devices.iter().map(String::as_str).collect();
        args.extend(device_refs);
        run_ok("mkfs.btrfs", &args)
            .await
            .map_err(BtrfsError::CommandFailed)?;

        let uuid = run_ok("blkid", &["-s", "UUID", "-o", "value", &req.devices[0]])
            .await
            .map_err(BtrfsError::CommandFailed)?
            .trim()
            .to_string();

        let stored = StoredFs {
            uuid: uuid.clone(),
            raid: Some(raid),
            compression: req.compression.clone(),
        };
        let _guard = STATE_LOCK.lock().await;
        let mut state = load_state().await;
        state.filesystems.insert(req.name.clone(), stored.clone());
        save_state(&state).await?;
        drop(_guard);

        self.mount(&req.name).await?;
        self.get(&req.name).await
    }

    pub async fn mount(&self, name: &str) -> Result<BtrfsFilesystem> {
        let state = load_state().await;
        let stored = state
            .filesystems
            .get(name)
            .ok_or_else(|| BtrfsError::NotFound(format!("btrfs filesystem '{name}'")))?;
        let target = format!("{MOUNT_BASE}/{name}");
        tokio::fs::create_dir_all(&target)
            .await
            .map_err(|e| BtrfsError::CommandFailed(format!("mkdir {target}: {e}")))?;
        let source = format!("UUID={}", stored.uuid);
        let opts = stored
            .compression
            .as_ref()
            .map(|c| format!("compress={c}"))
            .unwrap_or_else(|| "defaults".to_string());
        run_ok("mount", &["-t", "btrfs", "-o", &opts, &source, &target])
            .await
            .map_err(BtrfsError::CommandFailed)?;
        self.get(name).await
    }

    pub async fn unmount(&self, name: &str) -> Result<()> {
        if !self.manages(name).await {
            return Err(BtrfsError::NotFound(format!("btrfs filesystem '{name}'")));
        }
        let target = format!("{MOUNT_BASE}/{name}");
        run_ok("umount", &[target.as_str()])
            .await
            .map_err(BtrfsError::CommandFailed)?;
        Ok(())
    }

    /// Forget the filesystem and wipe signatures from its devices.
    pub async fn destroy(&self, name: &str) -> Result<()> {
        let fs = self.get(name).await?;
        if fs.mounted {
            self.unmount(name).await?;
        }
        for dev in &fs.devices {
            run_ok("wipefs", &["-a", dev])
                .await
                .map_err(BtrfsError::CommandFailed)?;
        }
        let _guard = STATE_LOCK.lock().await;
        let mut state = load_state().await;
        state.filesystems.remove(name);
        save_state(&state).await
    }

    /// Remount every known filesystem that isn't mounted. Returns
    /// human-readable failures (non-fatal, same contract as bcachefs's
    /// `restore_mounts`).
    pub async fn restore_mounts(&self) -> Vec<String> {
        let mut failures = Vec::new();
        let list = match self.list().await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("btrfs restore: listing filesystems failed: {e}");
                return failures;
            }
        };
        for fs in list.iter().filter(|f| !f.mounted) {
            if let Err(e) = self.mount(&fs.name).await {
                failures.push(format!("btrfs {}: {e}", fs.name));
            }
        }
        failures
    }

    // ── subvolumes ──────────────────────────────────────────────

    pub async fn subvolume_list(&self, name: &str) -> Result<Vec<BtrfsSubvolume>> {
        let mnt = self.mounted_path(name).await?;
        let raw = run_ok("btrfs", &["subvolume", "list", &mnt])
            .await
            .map_err(BtrfsError::CommandFailed)?;
        Ok(parse_subvolume_list(&raw)
            .into_iter()
            .filter(|(_, p)| !p.starts_with(SNAPSHOT_DIR))
            .map(|(id, path)| BtrfsSubvolume {
                id,
                name: path.clone(),
                path,
                filesystem: name.to_string(),
                backend: "btrfs".to_string(),
            })
            .collect())
    }

    pub async fn subvolume_create(&self, name: &str, subvol: &str) -> Result<BtrfsSubvolume> {
        validate_subpath(subvol)?;
        let mnt = self.mounted_path(name).await?;
        run_ok(
            "btrfs",
            &["subvolume", "create", &format!("{mnt}/{subvol}")],
        )
        .await
        .map_err(BtrfsError::CommandFailed)?;
        self.subvolume_list(name)
            .await?
            .into_iter()
            .find(|s| s.path == subvol)
            .ok_or_else(|| BtrfsError::CommandFailed("created subvolume not listed".into()))
    }

    pub async fn subvolume_delete(&self, name: &str, subvol: &str) -> Result<()> {
        validate_subpath(subvol)?;
        let mnt = self.mounted_path(name).await?;
        run_ok(
            "btrfs",
            &["subvolume", "delete", &format!("{mnt}/{subvol}")],
        )
        .await
        .map_err(BtrfsError::CommandFailed)?;
        Ok(())
    }

    pub async fn subvolume_get(&self, name: &str, subvol: &str) -> Result<BtrfsSubvolume> {
        self.subvolume_list(name)
            .await?
            .into_iter()
            .find(|s| s.path == subvol)
            .ok_or_else(|| {
                BtrfsError::NotFound(format!("subvolume '{subvol}' on filesystem '{name}'"))
            })
    }

    /// Subvolumes nested directly or transitively under `subvol`.
    pub async fn subvolume_children(
        &self,
        name: &str,
        subvol: &str,
    ) -> Result<Vec<BtrfsSubvolume>> {
        let prefix = format!("{subvol}/");
        Ok(self
            .subvolume_list(name)
            .await?
            .into_iter()
            .filter(|s| s.path.starts_with(&prefix))
            .collect())
    }

    /// Writable copy of a subvolume (btrfs snapshot without `-r`).
    pub async fn subvolume_clone(
        &self,
        name: &str,
        subvol: &str,
        new_name: &str,
    ) -> Result<BtrfsSubvolume> {
        validate_subpath(subvol)?;
        validate_subpath(new_name)?;
        let mnt = self.mounted_path(name).await?;
        run_ok(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                &format!("{mnt}/{subvol}"),
                &format!("{mnt}/{new_name}"),
            ],
        )
        .await
        .map_err(BtrfsError::CommandFailed)?;
        self.subvolume_get(name, new_name).await
    }

    // ── snapshots ───────────────────────────────────────────────

    pub async fn snapshot_list(&self, name: &str) -> Result<Vec<BtrfsSnapshot>> {
        let mnt = self.mounted_path(name).await?;
        let raw = run_ok("btrfs", &["subvolume", "list", &mnt])
            .await
            .map_err(BtrfsError::CommandFailed)?;
        Ok(parse_subvolume_list(&raw)
            .into_iter()
            .filter_map(|(id, path)| {
                let snap = path.strip_prefix(&format!("{SNAPSHOT_DIR}/"))?.to_string();
                let subvolume = snap.split_once('@').map(|(s, _)| s.to_string());
                Some(BtrfsSnapshot {
                    id,
                    name: snap,
                    subvolume,
                    filesystem: name.to_string(),
                    backend: "btrfs".to_string(),
                })
            })
            .collect())
    }

    /// Read-only snapshot of `subvol` as `<subvol>@<label>`.
    pub async fn snapshot_create(
        &self,
        name: &str,
        subvol: &str,
        label: &str,
    ) -> Result<BtrfsSnapshot> {
        validate_subpath(subvol)?;
        validate_name(label)?;
        let mnt = self.mounted_path(name).await?;
        let snapdir = format!("{mnt}/{SNAPSHOT_DIR}");
        tokio::fs::create_dir_all(&snapdir)
            .await
            .map_err(|e| BtrfsError::CommandFailed(format!("mkdir {snapdir}: {e}")))?;
        let snap_name = format!("{}@{label}", subvol.replace('/', "_"));
        run_ok(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                "-r",
                &format!("{mnt}/{subvol}"),
                &format!("{snapdir}/{snap_name}"),
            ],
        )
        .await
        .map_err(BtrfsError::CommandFailed)?;
        self.snapshot_list(name)
            .await?
            .into_iter()
            .find(|s| s.name == snap_name)
            .ok_or_else(|| BtrfsError::CommandFailed("created snapshot not listed".into()))
    }

    /// Writable subvolume from an existing (read-only) snapshot.
    pub async fn snapshot_clone(
        &self,
        name: &str,
        snap_name: &str,
        new_name: &str,
    ) -> Result<BtrfsSubvolume> {
        validate_subpath(snap_name)?;
        validate_subpath(new_name)?;
        let mnt = self.mounted_path(name).await?;
        run_ok(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                &format!("{mnt}/{SNAPSHOT_DIR}/{snap_name}"),
                &format!("{mnt}/{new_name}"),
            ],
        )
        .await
        .map_err(BtrfsError::CommandFailed)?;
        self.subvolume_get(name, new_name).await
    }

    pub async fn snapshot_delete(&self, name: &str, snap_name: &str) -> Result<()> {
        validate_subpath(snap_name)?;
        let mnt = self.mounted_path(name).await?;
        run_ok(
            "btrfs",
            &[
                "subvolume",
                "delete",
                &format!("{mnt}/{SNAPSHOT_DIR}/{snap_name}"),
            ],
        )
        .await
        .map_err(BtrfsError::CommandFailed)?;
        Ok(())
    }

    // ── internals ───────────────────────────────────────────────

    async fn assemble(
        &self,
        name: &str,
        stored: &StoredFs,
        mounts: &BTreeMap<String, String>,
    ) -> BtrfsFilesystem {
        let mount_point = format!("{MOUNT_BASE}/{name}");
        let mounted = mounts
            .get(&mount_point)
            .is_some_and(|fstype| fstype == "btrfs");

        let devices = run_ok("btrfs", &["filesystem", "show", &stored.uuid])
            .await
            .map(|raw| parse_show_devices(&raw))
            .unwrap_or_default();

        let (total, used, available) = if mounted {
            run_ok("btrfs", &["filesystem", "usage", "-b", &mount_point])
                .await
                .map(|raw| parse_usage(&raw))
                .unwrap_or((0, 0, 0))
        } else {
            (0, 0, 0)
        };

        BtrfsFilesystem {
            name: name.to_string(),
            uuid: stored.uuid.clone(),
            devices,
            mount_point: mounted.then_some(mount_point),
            mounted,
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            raid: stored.raid.clone(),
            compression: stored.compression.clone(),
            backend: "btrfs".to_string(),
        }
    }

    async fn mounted_path(&self, name: &str) -> Result<String> {
        let fs = self.get(name).await?;
        fs.mount_point
            .ok_or_else(|| BtrfsError::InvalidInput(format!("filesystem '{name}' is not mounted")))
    }
}

/// Mount point → filesystem type, from /proc/mounts.
async fn proc_mounts() -> BTreeMap<String, String> {
    let raw = match tokio::fs::read_to_string("/proc/mounts").await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("read /proc/mounts: {e}");
            return BTreeMap::new();
        }
    };
    raw.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let _dev = it.next()?;
            let mnt = it.next()?;
            let fstype = it.next()?;
            Some((mnt.replace("\\040", " "), fstype.to_string()))
        })
        .collect()
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(BtrfsError::InvalidInput(format!(
            "invalid name '{name}' (alphanumeric, '-', '_' only)"
        )));
    }
    Ok(())
}

/// Subvolume paths may nest but must stay inside the filesystem.
fn validate_subpath(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|seg| {
            seg.is_empty()
                || seg == "."
                || seg == ".."
                || !seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
        })
    {
        return Err(BtrfsError::InvalidInput(format!(
            "invalid subvolume path '{path}'"
        )));
    }
    Ok(())
}

// ── output parsers (pure, unit-tested) ──────────────────────────

/// Device paths from `btrfs filesystem show <uuid>` output.
fn parse_show_devices(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|l| {
            let l = l.trim();
            if !l.starts_with("devid") {
                return None;
            }
            l.split_whitespace().last().map(|p| p.to_string())
        })
        .collect()
}

/// (total, used, free) bytes from `btrfs filesystem usage -b <mnt>`.
fn parse_usage(raw: &str) -> (u64, u64, u64) {
    let grab = |key: &str| -> u64 {
        raw.lines()
            .find_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix(key)?;
                // Lines look like `Free (estimated):  123  (min: 456)` —
                // take the first numeric token after the colon.
                rest.trim_start_matches(':')
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0)
    };
    (grab("Device size"), grab("Used"), grab("Free (estimated)"))
}

/// `ID 256 gen 12 top level 5 path data`.
fn parse_subvolume_list(raw: &str) -> Vec<(u64, String)> {
    raw.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            if it.next()? != "ID" {
                return None;
            }
            let id: u64 = it.next()?.parse().ok()?;
            let path_idx = l.find(" path ")?;
            Some((id, l[path_idx + 6..].to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show_devices() {
        let raw = "Label: 'tank'  uuid: 0f0e-1\n\tTotal devices 2 FS bytes used 1.00GiB\n\tdevid    1 size 3.64TiB used 2.01GiB path /dev/sda\n\tdevid    2 size 3.64TiB used 2.01GiB path /dev/sdb\n";
        assert_eq!(parse_show_devices(raw), vec!["/dev/sda", "/dev/sdb"]);
    }

    #[test]
    fn parses_usage() {
        let raw = "Overall:\n    Device size:\t\t4000787030016\n    Device allocated:\t\t2155872256\n    Used:\t\t\t1310720\n    Free (estimated):\t\t3999544131584\t(min: 1999772065792)\n";
        assert_eq!(parse_usage(raw), (4000787030016, 1310720, 3999544131584));
    }

    #[test]
    fn parses_subvolume_list() {
        let raw = "ID 256 gen 12 top level 5 path data\nID 257 gen 14 top level 5 path media/tv\nID 258 gen 15 top level 5 path .snapshots/data@daily\n";
        assert_eq!(
            parse_subvolume_list(raw),
            vec![
                (256, "data".to_string()),
                (257, "media/tv".to_string()),
                (258, ".snapshots/data@daily".to_string()),
            ]
        );
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("tank-1_a").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a b").is_err());
    }

    #[test]
    fn subpath_validation() {
        assert!(validate_subpath("data").is_ok());
        assert!(validate_subpath("media/tv").is_ok());
        assert!(validate_subpath("data@daily").is_ok());
        assert!(validate_subpath("/abs").is_err());
        assert!(validate_subpath("a/../b").is_err());
        assert!(validate_subpath("").is_err());
    }
}
