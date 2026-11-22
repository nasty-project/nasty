//! Opt-in provisioning journal and no-replace namespace publication.
//! Failed/interrupted reservations are deliberately never automatically reused.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::subvolume::{SubvolumeError, validate_subvolume_name};

pub(crate) const PUBLICATION_XATTR: &str = "trusted.nasty.publication_version";

fn reservation_path(mount_point: &str, name: &str) -> PathBuf {
    let key: String = Sha256::digest(name.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Path::new(mount_point).join(".nasty/provisioning").join(key)
}

pub(crate) fn require_no_reservation(mount_point: &str, name: &str) -> Result<(), SubvolumeError> {
    let path = reservation_path(mount_point, name);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(SubvolumeError::ExistingIncompatible(format!(
            "an interrupted operation for {name} requires explicit recovery at {}",
            path.display()
        ))),
    }
}

pub(crate) fn validate_properties(
    properties: &HashMap<String, String>,
) -> Result<(), SubvolumeError> {
    if properties.len() > 64
        || properties
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > 65_536
    {
        return Err(SubvolumeError::CommandFailed(
            "initial properties exceed provisioning limits".into(),
        ));
    }
    for (key, value) in properties {
        if key.starts_with("nasty.") {
            return Err(SubvolumeError::ReservedProperty(key.clone()));
        }
        if key.is_empty() || key.len() > 240 || key.contains('\0') || value.len() > 4096 {
            return Err(SubvolumeError::CommandFailed(
                "invalid initial property key or value".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_destination(name: &str) -> Result<(), SubvolumeError> {
    validate_subvolume_name(name)?;
    if name.contains('/') {
        return Err(SubvolumeError::InvalidName(
            "atomic provisioning requires a top-level destination".into(),
        ));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), SubvolumeError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?
        .sync_all()?;
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), SubvolumeError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_directory(path.parent().unwrap())?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    // Never follow or chmod an untrusted existing internal namespace.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(SubvolumeError::CommandFailed(format!(
            "unsafe provisioning directory {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) struct Publication {
    reservation: PathBuf,
    pub(crate) volume: String,
    destination: PathBuf,
}

impl Publication {
    pub(crate) async fn reserve_async(
        mount_point: &str,
        name: &str,
        owner: Option<&str>,
        properties: &HashMap<String, String>,
        operation: &str,
    ) -> Result<Self, SubvolumeError> {
        let mount_point = mount_point.to_string();
        let name = name.to_string();
        let owner = owner.map(str::to_string);
        let properties = properties.clone();
        let operation = operation.to_string();
        tokio::task::spawn_blocking(move || {
            Self::reserve(
                &mount_point,
                &name,
                owner.as_deref(),
                &properties,
                &operation,
            )
        })
        .await
        .map_err(|error| SubvolumeError::CommandFailed(error.to_string()))?
    }

    pub(crate) async fn publish_async(self) -> Result<(), SubvolumeError> {
        tokio::task::spawn_blocking(move || self.publish())
            .await
            .map_err(|error| SubvolumeError::CommandFailed(error.to_string()))?
    }

    pub(crate) fn reserve(
        mount_point: &str,
        name: &str,
        owner: Option<&str>,
        properties: &HashMap<String, String>,
        operation: &str,
    ) -> Result<Self, SubvolumeError> {
        validate_destination(name)?;
        validate_properties(properties)?;
        let mount = fs::symlink_metadata(mount_point)?;
        // Private child permissions alone do not stop a share user renaming
        // .nasty through a writable parent. Require a protected mount root;
        // never silently change permissions on a live share to obtain one.
        if !mount.is_dir()
            || mount.uid() != unsafe { libc::geteuid() }
            || (mount.mode() & 0o022 != 0 && mount.mode() & 0o1000 == 0)
        {
            return Err(SubvolumeError::CommandFailed("atomic provisioning requires an engine-owned mount root without untrusted rename access (non-writable or sticky)".into()));
        }
        let internal = Path::new(mount_point).join(".nasty");
        private_directory(&internal)?;
        let root = internal.join("provisioning");
        private_directory(&root)?;
        let reservation = reservation_path(mount_point, name);
        match fs::DirBuilder::new().mode(0o700).create(&reservation) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(SubvolumeError::ExistingIncompatible(format!(
                    "an interrupted operation for {name} requires explicit recovery at {}",
                    reservation.display()
                )));
            }
            Err(error) => return Err(error.into()),
        }
        // Persist the reservation before invoking any storage command. Even a
        // crash before the journal write blocks automatic mutation replay.
        sync_directory(&root)?;
        let record = serde_json::to_vec(&serde_json::json!({
            "version": 1, "destination": name, "owner": owner,
            "operation": operation, "initial_properties": properties,
        }))
        .map_err(|error| SubvolumeError::CommandFailed(error.to_string()))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(reservation.join("request.json"))?;
        file.write_all(&record)?;
        file.sync_all()?;
        sync_directory(&reservation)?;
        Ok(Self {
            volume: reservation.join("volume").to_string_lossy().into_owned(),
            destination: Path::new(mount_point).join(name),
            reservation,
        })
    }

    pub(crate) fn publish(&self) -> Result<(), SubvolumeError> {
        let volume = Path::new(&self.volume);
        if xattr::get(volume, PUBLICATION_XATTR)?.as_deref() != Some(b"1")
            || xattr::get(volume, "trusted.nasty.provisioning_state")?.as_deref() != Some(b"ready")
        {
            return Err(SubvolumeError::ExistingIncompatible(
                "staged metadata is not ready for publication".into(),
            ));
        }
        let image = volume.join("vol.img");
        if image.try_exists()? {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(image)?
                .sync_all()?;
        }
        sync_directory(volume)?;
        sync_directory(&self.reservation)?;
        rename_no_replace(volume, &self.destination)?;
        // Completion may be uncertain if either durability barrier fails, but
        // the final path can only contain fully initialized metadata.
        sync_directory(self.destination.parent().unwrap())?;
        sync_directory(&self.reservation)?;
        // Only successful publication removes our own empty journal. Failed
        // cleanup leaves a reservation for explicit review, never data deletion.
        fs::remove_file(self.reservation.join("request.json"))?;
        fs::remove_dir(&self.reservation)?;
        sync_directory(self.reservation.parent().unwrap())?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), SubvolumeError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|error| SubvolumeError::CommandFailed(error.to_string()))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|error| SubvolumeError::CommandFailed(error.to_string()))?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> Result<(), SubvolumeError> {
    Err(SubvolumeError::CommandFailed(
        "atomic publication requires Linux renameat2".into(),
    ))
}

pub(crate) fn write_properties(
    path: &str,
    properties: &HashMap<String, String>,
    reset: bool,
) -> Result<(), SubvolumeError> {
    use std::os::unix::ffi::OsStrExt;
    validate_properties(properties)?;
    if reset {
        // Atomic clones must not inherit a source volume's CSI identity or
        // presentation IDs. Preserve only the appliance's internal metadata.
        for key in xattr::list(path)? {
            let bytes = key.as_bytes();
            if bytes.starts_with(b"user.") && !bytes.starts_with(b"user.nasty.") {
                xattr::remove(path, &key)?;
            }
        }
    }
    for (key, value) in properties {
        xattr::set(path, format!("user.{key}"), value.as_bytes())?;
    }
    xattr::set(path, PUBLICATION_XATTR, b"1")?;
    Ok(())
}

pub(crate) fn require_properties(
    path: &str,
    actual: &HashMap<String, String>,
    expected: &HashMap<String, String>,
) -> Result<(), SubvolumeError> {
    if xattr::get(path, PUBLICATION_XATTR)?.as_deref() != Some(b"1")
        || expected
            .iter()
            .any(|(key, value)| actual.get(key) != Some(value))
    {
        return Err(SubvolumeError::ExistingIncompatible("existing destination lacks matching atomic publication metadata; explicit recovery required".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_properties_before_reserving_storage() {
        for key in ["", "nasty.owner", "bad\0key"] {
            assert!(validate_properties(&HashMap::from([(key.into(), "value".into())])).is_err());
        }
        assert!(validate_properties(&HashMap::from([("valid".into(), "x".repeat(4097))])).is_err());
        let too_many = (0..65)
            .map(|index| (format!("key{index}"), "value".into()))
            .collect();
        assert!(validate_properties(&too_many).is_err());
        assert!(
            validate_properties(&HashMap::from([(
                "nasty-csi:managed_by".into(),
                "nasty-csi".into()
            )]))
            .is_ok()
        );
    }

    #[test]
    fn destinations_cannot_escape_or_nest() {
        for name in ["", "../escape", ".nasty", "nested/volume", "/absolute"] {
            assert!(validate_destination(name).is_err());
        }
        assert!(validate_destination("pvc-123").is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interrupted_reservation_is_never_replayed() {
        let root = tempfile::tempdir().unwrap();
        let mount = root.path().to_str().unwrap();
        let properties = HashMap::from([("nasty-csi:delete_strategy".into(), "retain".into())]);
        let stage =
            Publication::reserve(mount, "pvc", Some("token"), &properties, "create").unwrap();
        assert!(!root.path().join("pvc").exists());
        let journal: serde_json::Value =
            serde_json::from_slice(&fs::read(stage.reservation.join("request.json")).unwrap())
                .unwrap();
        assert_eq!(
            journal["initial_properties"]["nasty-csi:delete_strategy"],
            "retain"
        );
        drop(stage); // Simulate process loss; there is intentionally no cleanup Drop.
        assert!(matches!(
            Publication::reserve(mount, "pvc", Some("token"), &properties, "create"),
            Err(SubvolumeError::ExistingIncompatible(_))
        ));
        assert!(require_no_reservation(mount, "pvc").is_err());
        assert!(!root.path().join("pvc").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn incomplete_stage_cannot_be_published() {
        let root = tempfile::tempdir().unwrap();
        let stage = Publication::reserve(
            root.path().to_str().unwrap(),
            "pvc",
            None,
            &HashMap::new(),
            "create",
        )
        .unwrap();
        fs::create_dir(&stage.volume).unwrap();
        assert!(stage.publish().is_err());
        assert!(!root.path().join("pvc").exists());
        assert!(Path::new(&stage.volume).exists());
        assert!(stage.reservation.join("request.json").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn publication_refuses_raced_destinations_and_preserves_both() {
        let root = tempfile::tempdir().unwrap();
        let staged = root.path().join("staged");
        let destination = root.path().join("pvc");
        fs::create_dir(&staged).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("must-survive"), b"foreign data").unwrap();
        assert!(rename_no_replace(&staged, &destination).is_err());
        assert!(staged.exists());
        assert_eq!(
            fs::read(destination.join("must-survive")).unwrap(),
            b"foreign data"
        );
        fs::remove_file(destination.join("must-survive")).unwrap();
        fs::remove_dir(&destination).unwrap();
        rename_no_replace(&staged, &destination).unwrap();
        assert!(!staged.exists());
        assert!(destination.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlinked_namespace_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(foreign.path(), root.path().join(".nasty")).unwrap();
        assert!(
            Publication::reserve(
                root.path().to_str().unwrap(),
                "pvc",
                None,
                &HashMap::new(),
                "create"
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(foreign.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_mount_root_is_not_silently_hardened() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            Publication::reserve(
                root.path().to_str().unwrap(),
                "pvc",
                None,
                &HashMap::new(),
                "create"
            )
            .is_err()
        );
        assert!(!root.path().join(".nasty").exists());
        assert_eq!(fs::metadata(root.path()).unwrap().mode() & 0o777, 0o777);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an explicitly isolated Linux fixture with trusted-xattr capability"]
    fn initialized_properties_survive_publication_and_source_identity_is_reset() {
        let root = tempfile::tempdir().unwrap();
        let properties = HashMap::from([
            ("nasty-csi:volume_id".into(), "destination".into()),
            ("nasty-csi:delete_strategy".into(), "retain".into()),
        ]);
        let stage = Publication::reserve(
            root.path().to_str().unwrap(),
            "pvc",
            None,
            &properties,
            "clone",
        )
        .unwrap();
        fs::create_dir(&stage.volume).unwrap();
        xattr::set(&stage.volume, "user.nasty.owner", b"fixture-owner").unwrap();
        xattr::set(&stage.volume, "user.nasty-csi:volume_id", b"source").unwrap();
        xattr::set(&stage.volume, "user.nasty-csi:share_id", b"source-export").unwrap();
        write_properties(&stage.volume, &properties, true).unwrap();
        xattr::set(&stage.volume, "trusted.nasty.provisioning_state", b"ready").unwrap();
        stage.publish().unwrap();
        let destination = root.path().join("pvc");
        assert_eq!(
            xattr::get(&destination, "user.nasty-csi:volume_id")
                .unwrap()
                .as_deref(),
            Some(b"destination".as_slice())
        );
        assert_eq!(
            xattr::get(&destination, "user.nasty-csi:delete_strategy")
                .unwrap()
                .as_deref(),
            Some(b"retain".as_slice())
        );
        assert_eq!(
            xattr::get(&destination, "user.nasty.owner")
                .unwrap()
                .as_deref(),
            Some(b"fixture-owner".as_slice())
        );
        assert!(
            xattr::get(&destination, "user.nasty-csi:share_id")
                .unwrap()
                .is_none()
        );
        require_properties(destination.to_str().unwrap(), &properties, &properties).unwrap();
        assert!(
            require_properties(
                destination.to_str().unwrap(),
                &properties,
                &HashMap::from([("nasty-csi:volume_id".into(), "other".into())])
            )
            .is_err()
        );
        assert!(!stage.reservation.exists());
        assert!(require_no_reservation(root.path().to_str().unwrap(), "pvc").is_ok());
    }
}
