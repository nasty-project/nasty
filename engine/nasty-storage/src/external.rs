//! External drives (#1080): mount a USB stick or other drive that is not part
//! of a pool, read-only, so its files can be copied in from the Files page.
//!
//! Drives mount under [`EXTERNAL_ROOT`], never under `/fs`: shares, apps, VMs
//! and backups treat every `/fs/<name>` as a NASty filesystem. `/run` is
//! cleared at boot, so nothing is remounted without the operator asking.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cmd;
use crate::filesystem::BlockDevice;

pub const EXTERNAL_ROOT: &str = "/run/nasty/external";

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ExternalDrive {
    /// Block device, e.g. `/dev/sdb1`. Empty for a mount whose drive was
    /// unplugged without being ejected.
    pub device: String,
    /// Directory under `/run/nasty/external`, shown in the Files page as
    /// `@external/<name>`.
    pub name: String,
    pub label: Option<String>,
    pub fs_type: String,
    pub size_bytes: u64,
    /// Model of the whole disk, e.g. "SanDisk Ultra".
    pub model: Option<String>,
    /// Connection reported by lsblk, e.g. "usb".
    pub transport: Option<String>,
    pub mounted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExternalDriveRequest {
    /// Block device to mount, or the drive's `name` to eject.
    pub device: String,
}

/// Filesystems NASty mounts, and the kernel driver for each.
fn mount_type(fs_type: &str) -> Option<&'static str> {
    Some(match fs_type {
        "exfat" => "exfat",
        "vfat" => "vfat",
        "ntfs" => "ntfs3",
        "ext2" => "ext2",
        "ext3" => "ext3",
        "ext4" => "ext4",
        _ => return None,
    })
}

fn is_external_mount(mount_point: &str) -> bool {
    mount_point
        .strip_prefix(EXTERNAL_ROOT)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Drives that can be mounted or ejected. A disk is offered only when nothing
/// on it is in use except NASty's own external mounts, which keeps the system
/// disk (its ESP is vfat) and pool members out. `mounts` is `/proc/mounts`;
/// an external mount whose drive is gone is listed so it can still be ejected.
pub fn list(devices: &[BlockDevice], mounts: &str) -> Vec<ExternalDrive> {
    let disk_of = |d: &BlockDevice| d.parent_path.clone().unwrap_or_else(|| d.path.clone());
    let ours = |d: &BlockDevice| d.mount_point.as_deref().is_some_and(is_external_mount);
    let busy_disks: HashSet<String> = devices
        .iter()
        .filter(|d| d.dev_type != "free" && d.in_use && !ours(d))
        .map(disk_of)
        .collect();

    let mut drives: Vec<ExternalDrive> = devices
        .iter()
        .filter(|d| matches!(d.dev_type.as_str(), "disk" | "part"))
        .filter(|d| !busy_disks.contains(&disk_of(d)))
        .filter_map(|d| {
            let fs_type = d.fs_type.as_deref().filter(|t| mount_type(t).is_some())?;
            let disk = devices.iter().find(|p| p.path == disk_of(d));
            Some(ExternalDrive {
                device: d.path.clone(),
                name: d.path.strip_prefix("/dev/")?.to_string(),
                label: d.label.clone(),
                fs_type: fs_type.to_string(),
                size_bytes: d.size_bytes,
                model: disk.and_then(|p| p.model.clone()),
                transport: disk.and_then(|p| p.transport.clone()),
                mounted: ours(d),
            })
        })
        .filter(|d| valid_name(&d.name))
        .collect();

    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(target), Some(fs_type)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(name) = target
            .strip_prefix(EXTERNAL_ROOT)
            .and_then(|r| r.strip_prefix('/'))
        else {
            continue;
        };
        if !valid_name(name) || drives.iter().any(|d| d.name == name) {
            continue;
        }
        drives.push(ExternalDrive {
            device: String::new(),
            name: name.to_string(),
            label: None,
            fs_type: fs_type.to_string(),
            size_bytes: 0,
            model: None,
            transport: None,
            mounted: true,
        });
        tracing::debug!("external mount {target} has no drive behind it (was {source})");
    }
    drives
}

/// Kernel device names only, so a name is always one plain path component.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Mount `device` read-only. ext journals are not replayed (`noload`), so
/// mounting never writes to the drive.
pub async fn mount(
    device: &str,
    devices: &[BlockDevice],
    mounts: &str,
) -> Result<ExternalDrive, String> {
    let mut drive = list(devices, mounts)
        .into_iter()
        .find(|d| !d.device.is_empty() && d.device == device)
        .ok_or_else(|| format!("{device} is not a drive NASty can mount: it is in use, part of a pool, or has no supported filesystem"))?;
    if drive.mounted {
        return Ok(drive);
    }
    let fs_type = mount_type(&drive.fs_type).expect("listed drives have a supported filesystem");
    let options = if fs_type.starts_with("ext") {
        "ro,nosuid,nodev,noexec,noload"
    } else {
        "ro,nosuid,nodev,noexec"
    };
    let dir = format!("{EXTERNAL_ROOT}/{}", drive.name);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create {dir}: {e}"))?;
    if let Err(e) = cmd::run_ok("mount", &["-t", fs_type, "-o", options, "--", device, &dir]).await
    {
        let _ = tokio::fs::remove_dir(&dir).await;
        return Err(format!("mount {device}: {e}"));
    }
    drive.mounted = true;
    Ok(drive)
}

/// Unmount the drive mounted as `name` (or from `device`) and remove its directory.
pub async fn unmount(device: &str, devices: &[BlockDevice], mounts: &str) -> Result<(), String> {
    let drive = list(devices, mounts)
        .into_iter()
        .find(|d| d.mounted && (d.device == device || d.name == device))
        .ok_or_else(|| format!("{device} is not a mounted external drive"))?;
    let dir = format!("{EXTERNAL_ROOT}/{}", drive.name);
    cmd::run_ok("umount", &["--", &dir]).await.map_err(|e| {
        format!(
            "eject {}: {e} (a copy may still be reading from it)",
            drive.name
        )
    })?;
    let _ = tokio::fs::remove_dir(&dir).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(
        path: &str,
        dev_type: &str,
        parent: Option<&str>,
        fs: Option<&str>,
        mount: Option<&str>,
        in_use: bool,
    ) -> BlockDevice {
        serde_json::from_value(serde_json::json!({
            "path": path,
            "size_bytes": 64,
            "dev_type": dev_type,
            "mount_point": mount,
            "fs_type": fs,
            "parent_path": parent,
            "in_use": in_use,
            "rotational": false,
            "device_class": "ssd",
            "model": if parent.is_none() { Some("Stick") } else { None },
            "transport": if parent.is_none() { Some("usb") } else { None },
        }))
        .unwrap()
    }

    #[test]
    fn offers_only_supported_drives_on_disks_with_nothing_else_in_use() {
        let devices = vec![
            // System disk: its vfat ESP must never be offered.
            dev("/dev/sda", "disk", None, None, None, false),
            dev(
                "/dev/sda1",
                "part",
                Some("/dev/sda"),
                Some("vfat"),
                Some("/boot"),
                true,
            ),
            dev(
                "/dev/sda2",
                "part",
                Some("/dev/sda"),
                Some("ext4"),
                Some("/"),
                true,
            ),
            // Pool member.
            dev("/dev/sdc", "disk", None, Some("bcachefs"), None, true),
            // USB stick: one exFAT partition mounted by us, one unsupported.
            dev("/dev/sdb", "disk", None, None, None, false),
            dev(
                "/dev/sdb1",
                "part",
                Some("/dev/sdb"),
                Some("exfat"),
                Some("/run/nasty/external/sdb1"),
                true,
            ),
            dev(
                "/dev/sdb2",
                "part",
                Some("/dev/sdb"),
                Some("swap"),
                None,
                false,
            ),
            // Whole-disk NTFS, not mounted.
            dev("/dev/sdd", "disk", None, Some("ntfs"), None, false),
        ];
        let drives = list(&devices, "");
        let names: Vec<_> = drives
            .iter()
            .map(|d| (d.name.as_str(), d.mounted))
            .collect();
        assert_eq!(names, [("sdb1", true), ("sdd", false)]);
        assert_eq!(drives[0].model.as_deref(), Some("Stick"));
        assert_eq!(drives[0].transport.as_deref(), Some("usb"));
    }

    #[test]
    fn lists_a_mount_whose_drive_was_unplugged() {
        let mounts = "/dev/sda2 / ext4 rw 0 0\n/dev/sdz1 /run/nasty/external/sdz1 exfat ro 0 0\n/dev/x /run/nasty/external/../etc ext4 ro 0 0\n";
        let drives = list(&[], mounts);
        assert_eq!(drives.len(), 1);
        assert_eq!(
            (
                drives[0].name.as_str(),
                drives[0].device.as_str(),
                drives[0].mounted
            ),
            ("sdz1", "", true)
        );
    }

    #[test]
    fn mount_points_are_ours_only_below_the_root() {
        assert!(is_external_mount("/run/nasty/external/sdb1"));
        assert!(!is_external_mount("/run/nasty/external"));
        assert!(!is_external_mount("/run/nasty/externalx/sdb1"));
    }
}
