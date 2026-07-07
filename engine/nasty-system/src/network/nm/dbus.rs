//! NetworkManager D-Bus client.
//!
//! Talks to NM via zbus.  Read side: `list_connections`,
//! `get_settings`, `compute_diff`.  Write side: `AddConnection`,
//! `Connection.Update`, `Connection.Delete`, all wrapped by
//! `apply_profiles` which also activates the resulting profiles.
//! This is the active backend — `apply_config` in
//! `super::super::network` calls `apply_profiles` directly.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Serialize;
use zbus::Connection as DbusConnection;
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::{NmConnection, to_settings_dict};

/// NM's settings dict shape: `a{sa{sv}}` — section name → setting
/// name → variant. We use `OwnedValue` so the dict can hold any
/// type NM expects (strings, integers, arrays of arrays of bytes,
/// etc.). The `to_settings_dict` converter in `super` produces this.
pub type SettingsDict = HashMap<String, HashMap<String, OwnedValue>>;

// ── Proxies ────────────────────────────────────────────────────

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait Settings {
    /// Returns a list of object paths, one per persisted connection.
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    /// Persist a new connection profile. NM writes the keyfile to
    /// `/etc/NetworkManager/system-connections/` and returns the
    /// object path of the new `Connection`. Does not activate.
    fn add_connection(&self, connection: SettingsDict) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ConnectionSettings {
    /// Read the connection's settings dict.  Used to identify
    /// NASty-managed connections (id starting with `nasty-`) and to
    /// diff against the desired profile set.
    fn get_settings(&self) -> zbus::Result<SettingsDict>;
    /// Replace the connection's settings dict in place. Profile
    /// identity (UUID, object path) is preserved. Does not re-activate;
    /// running connections need an explicit `nmcli connection up` to
    /// reload the new settings.
    fn update(&self, settings: SettingsDict) -> zbus::Result<()>;
    /// Remove the connection from NM. Deletes the on-disk keyfile.
    /// If the connection was active, NM deactivates it first.
    fn delete(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    /// Reload NM configuration in place. `flags = 0x1` (CONF) rereads
    /// `NetworkManager.conf` and its conf.d drop-ins — how our
    /// `unmanaged-devices` changes take effect without restarting NM
    /// (a restart would bounce every managed connection).
    fn reload(&self, flags: u32) -> zbus::Result<()>;
    /// Activate a connection on a device. Pass `/` for `specific_object`
    /// to let NM pick (the right call for our case — we don't bind to
    /// a specific access point or VPN secret). Returns the object path
    /// of the new ActiveConnection.
    fn activate_connection(
        &self,
        connection: &zbus::zvariant::ObjectPath<'_>,
        device: &zbus::zvariant::ObjectPath<'_>,
        specific_object: &zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    /// Kernel interface name (`eth0`, `br0`, `bond0`, ...).  Used to
    /// resolve a profile's `interface_name` to the device path that
    /// `ActivateConnection` needs.
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
}

// ── Client ─────────────────────────────────────────────────────

/// NM DBus client. Holds a system-bus connection and the top-level
/// proxies. Cheap to construct — proxies don't open new connections.
pub struct NmDbusClient {
    conn: DbusConnection,
}

impl NmDbusClient {
    /// Connect to the system bus.  Fails if NM isn't running, the bus
    /// isn't reachable, or the user lacks access.  Surface the error
    /// verbatim — the `nm_preview` RPC reports it to the caller so a
    /// misconfigured box is immediately diagnosable.
    pub async fn new() -> Result<Self, String> {
        let conn = DbusConnection::system()
            .await
            .map_err(|e| format!("connect to system DBus: {e}"))?;
        Ok(Self { conn })
    }

    /// Ask NM to re-read NetworkManager.conf + conf.d drop-ins
    /// (Reload flag CONF = 0x1). Used after (re)writing the
    /// InfiniBand `unmanaged-devices` drop-in so ownership changes
    /// apply without restarting NM.
    pub async fn reload_conf(&self) -> Result<(), String> {
        let proxy = NetworkManagerProxy::new(&self.conn)
            .await
            .map_err(|e| format!("NM proxy: {e}"))?;
        proxy
            .reload(0x1)
            .await
            .map_err(|e| format!("NM Reload(CONF): {e}"))
    }

    /// Object paths for every persisted connection NM knows about.
    /// Includes external (Docker, libvirt, etc.) connections plus our
    /// `nasty-*` ones; the caller filters.
    pub async fn list_connections(&self) -> Result<Vec<OwnedObjectPath>, String> {
        let proxy = SettingsProxy::new(&self.conn)
            .await
            .map_err(|e| format!("settings proxy: {e}"))?;
        proxy
            .list_connections()
            .await
            .map_err(|e| format!("list_connections: {e}"))
    }

    /// Read a single connection's settings dict.
    pub async fn get_settings(&self, path: &OwnedObjectPath) -> Result<SettingsDict, String> {
        let proxy = ConnectionSettingsProxy::builder(&self.conn)
            .path(path.clone())
            .map_err(|e| format!("connection path {path}: {e}"))?
            .build()
            .await
            .map_err(|e| format!("connection proxy {path}: {e}"))?;
        proxy
            .get_settings()
            .await
            .map_err(|e| format!("get_settings {path}: {e}"))
    }

    /// All NASty-managed connections.  Discriminator: the
    /// `[connection].id` field starts with `nasty-`.  Returns
    /// (object path, settings) so callers can both compare settings
    /// and identify which path to update/delete.
    pub async fn list_nasty_connections(&self) -> Result<Vec<NmExisting>, String> {
        let mut out = Vec::new();
        for path in self.list_connections().await? {
            let settings = match self.get_settings(&path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("skip unreadable connection {path}: {e}");
                    continue;
                }
            };
            let Some(id) = read_string(&settings, "connection", "id") else {
                continue;
            };
            if id.starts_with("nasty-") {
                out.push(NmExisting { path, id, settings });
            }
        }
        Ok(out)
    }

    /// Persist a new NASty-managed connection. Returns the object path
    /// of the new `Connection` for follow-up calls (Update, Delete).
    /// Does not activate.
    pub async fn add_connection(&self, settings: SettingsDict) -> Result<OwnedObjectPath, String> {
        let proxy = SettingsProxy::new(&self.conn)
            .await
            .map_err(|e| format!("settings proxy: {e}"))?;
        proxy
            .add_connection(settings)
            .await
            .map_err(|e| format!("add_connection: {e}"))
    }

    /// Replace an existing connection's settings dict in place.
    pub async fn update_connection(
        &self,
        path: &OwnedObjectPath,
        settings: SettingsDict,
    ) -> Result<(), String> {
        let proxy = ConnectionSettingsProxy::builder(&self.conn)
            .path(path.clone())
            .map_err(|e| format!("connection path {path}: {e}"))?
            .build()
            .await
            .map_err(|e| format!("connection proxy {path}: {e}"))?;
        proxy
            .update(settings)
            .await
            .map_err(|e| format!("update {path}: {e}"))
    }

    /// Delete a connection. NM removes the on-disk keyfile and (if
    /// active) deactivates it.
    pub async fn delete_connection(&self, path: &OwnedObjectPath) -> Result<(), String> {
        let proxy = ConnectionSettingsProxy::builder(&self.conn)
            .path(path.clone())
            .map_err(|e| format!("connection path {path}: {e}"))?
            .build()
            .await
            .map_err(|e| format!("connection proxy {path}: {e}"))?;
        proxy
            .delete()
            .await
            .map_err(|e| format!("delete {path}: {e}"))
    }

    /// Find the NM Device object path matching a kernel interface name
    /// (`eth0`, `br0`, `bond0`, ...). Returns `None` if no NM-managed
    /// device with that name exists — typical for an iface that's
    /// excluded by `unmanaged-devices`.
    pub async fn find_device_by_name(
        &self,
        iface: &str,
    ) -> Result<Option<OwnedObjectPath>, String> {
        let nm = NetworkManagerProxy::new(&self.conn)
            .await
            .map_err(|e| format!("nm proxy: {e}"))?;
        let devices = nm
            .get_devices()
            .await
            .map_err(|e| format!("get_devices: {e}"))?;
        for path in devices {
            let dev = DeviceProxy::builder(&self.conn)
                .path(path.clone())
                .map_err(|e| format!("device path {path}: {e}"))?
                .build()
                .await
                .map_err(|e| format!("device proxy {path}: {e}"))?;
            match dev.interface().await {
                Ok(name) if name == iface => return Ok(Some(path)),
                Ok(_) => {}
                Err(e) => tracing::debug!("device {path} interface read failed: {e}"),
            }
        }
        Ok(None)
    }

    /// Activate a connection on a device. Idempotent for NM:
    /// activating an already-active connection is a no-op (NM returns
    /// the existing ActiveConnection). Use `find_device_by_name` to
    /// resolve the device path first.
    pub async fn activate_connection(
        &self,
        connection: &OwnedObjectPath,
        device: &OwnedObjectPath,
    ) -> Result<OwnedObjectPath, String> {
        let nm = NetworkManagerProxy::new(&self.conn)
            .await
            .map_err(|e| format!("nm proxy: {e}"))?;
        let unspecified = zbus::zvariant::ObjectPath::try_from("/")
            .map_err(|e| format!("unspecified path: {e}"))?;
        let conn_ref = connection.as_ref();
        let dev_ref = device.as_ref();
        nm.activate_connection(&conn_ref, &dev_ref, &unspecified)
            .await
            .map_err(|e| format!("activate_connection: {e}"))
    }
}

/// Read a string field out of a settings dict. Returns None when the
/// section, key, or expected variant type is missing.
fn read_string(settings: &SettingsDict, section: &str, key: &str) -> Option<String> {
    let value = settings.get(section)?.get(key)?;
    let s: &str = value.downcast_ref().ok()?;
    Some(s.to_string())
}

/// One existing NASty-managed NM connection.
#[derive(Debug, Clone)]
pub struct NmExisting {
    pub path: OwnedObjectPath,
    pub id: String,
    pub settings: SettingsDict,
}

// ── Diff ───────────────────────────────────────────────────────

/// Per-id breakdown of what `apply_profiles` would change.  The same
/// data drives both `nm_preview` (read-only) and the actual apply.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct NmDiff {
    /// Connection IDs present in the desired set but not currently in
    /// NM — `apply_profiles` calls `Settings.AddConnection` for these.
    pub to_add: Vec<String>,
    /// IDs present in both, but with different settings — `apply_profiles`
    /// calls `Connection.Update`.
    pub to_update: Vec<NmDiffUpdate>,
    /// NASty-managed IDs in NM but not in the desired set — user must
    /// have removed the link.  `apply_profiles` calls `Connection.Delete`.
    pub to_delete: Vec<String>,
    /// Counts for at-a-glance display in the WebUI.
    pub summary: NmDiffSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NmDiffUpdate {
    pub id: String,
    /// One line per differing top-level section (e.g. `"ipv4"`,
    /// `"bridge"`).  Cheap signal for the UI; the WebUI can render a
    /// richer diff if it wants by re-fetching settings.
    pub changed_sections: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct NmDiffSummary {
    pub add: usize,
    pub update: usize,
    pub delete: usize,
    pub unchanged: usize,
}

/// Compute what would change if the desired profiles were applied.
/// Pure-data once `existing` is fetched — easy to unit-test.
pub fn compute_diff(desired: &[NmConnection], existing: &[NmExisting]) -> NmDiff {
    let mut out = NmDiff::default();
    let existing_by_id: HashMap<&str, &NmExisting> =
        existing.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut seen = std::collections::HashSet::new();
    for desired_conn in desired {
        seen.insert(desired_conn.id.clone());
        match existing_by_id.get(desired_conn.id.as_str()) {
            None => {
                out.to_add.push(desired_conn.id.clone());
                out.summary.add += 1;
            }
            Some(existing_conn) => {
                let desired_dict = to_settings_dict(desired_conn);
                let changed = diff_sections(&desired_dict, &existing_conn.settings);
                if changed.is_empty() {
                    out.summary.unchanged += 1;
                } else {
                    out.to_update.push(NmDiffUpdate {
                        id: desired_conn.id.clone(),
                        changed_sections: changed,
                    });
                    out.summary.update += 1;
                }
            }
        }
    }
    for existing_conn in existing {
        if !seen.contains(&existing_conn.id) {
            out.to_delete.push(existing_conn.id.clone());
            out.summary.delete += 1;
        }
    }
    out
}

/// Names of top-level sections that differ between two settings dicts.
/// We compare section-by-section by serialized representation —
/// sidesteps the `OwnedValue` equality awkwardness and is enough
/// signal for change detection.
fn diff_sections(a: &SettingsDict, b: &SettingsDict) -> Vec<String> {
    let mut sections: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    sections.extend(a.keys().map(String::as_str));
    sections.extend(b.keys().map(String::as_str));
    sections
        .into_iter()
        .filter(|s| section_changed(a.get(*s), b.get(*s)))
        .map(String::from)
        .collect()
}

fn section_changed(
    a: Option<&HashMap<String, OwnedValue>>,
    b: Option<&HashMap<String, OwnedValue>>,
) -> bool {
    match (a, b) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(a), Some(b)) => {
            // Compare keys-and-values via Debug repr — `OwnedValue`
            // doesn't impl `Eq`, but its Debug is stable and includes
            // the variant + payload.  A real variant-walker would be
            // tighter; this is good enough for change detection.
            if a.len() != b.len() {
                return true;
            }
            for (k, va) in a {
                let Some(vb) = b.get(k) else {
                    return true;
                };
                if format!("{va:?}") != format!("{vb:?}") {
                    return true;
                }
            }
            false
        }
    }
}

// ── Apply ──────────────────────────────────────────────────────
//
// `apply_profiles` computes the desired-vs-existing diff and issues
// `AddConnection` / `Connection.Update` / `Connection.Delete` for it,
// then activates each enabled connection on its matching device.
// NM is the authoritative apply backend — there's no parallel
// ip-command path to coordinate with.

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct NmApplyOutcome {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub unchanged: Vec<String>,
    /// Connection IDs successfully activated this apply. Subset of
    /// `added ∪ updated ∪ unchanged` — we only activate enabled
    /// connections that have a matching NM-managed device.
    pub activated: Vec<String>,
    /// Per-id error map. Empty on full success. The apply is best-
    /// effort — one failed connection doesn't abort the rest.
    pub errors: HashMap<String, String>,
}

/// Apply a desired profile set to NM.  Computes the diff against the
/// current `nasty-*` connections, then issues `AddConnection` /
/// `Update` / `Delete` calls.  After Add/Update, activates each
/// enabled connection on its matching device (idempotent — NM no-ops
/// when the connection is already the active one for the device).
pub async fn apply_profiles(
    client: &NmDbusClient,
    desired: &[NmConnection],
) -> Result<NmApplyOutcome, String> {
    let existing = client.list_nasty_connections().await?;
    let existing_by_id: HashMap<&str, &NmExisting> =
        existing.iter().map(|e| (e.id.as_str(), e)).collect();
    let desired_ids: std::collections::HashSet<&str> =
        desired.iter().map(|d| d.id.as_str()).collect();

    let mut outcome = NmApplyOutcome::default();
    // Connection paths for activation step. Tracked in iteration
    // order so masters get activated before their members would
    // — though NM tolerates either order via the `controller` field
    // on the port profile.
    let mut activate_targets: Vec<(String, OwnedObjectPath, String)> = Vec::new();

    for d in desired {
        match existing_by_id.get(d.id.as_str()) {
            None => {
                let dict = super::to_settings_dict(d);
                match client.add_connection(dict).await {
                    Ok(path) => {
                        outcome.added.push(d.id.clone());
                        if d.autoconnect {
                            activate_targets.push((d.id.clone(), path, d.interface_name.clone()));
                        }
                    }
                    Err(e) => {
                        outcome.errors.insert(d.id.clone(), e);
                    }
                }
            }
            Some(existing_conn) => {
                let dict = super::to_settings_dict(d);
                if section_changed_any(&dict, &existing_conn.settings) {
                    match client.update_connection(&existing_conn.path, dict).await {
                        Ok(()) => {
                            outcome.updated.push(d.id.clone());
                            if d.autoconnect {
                                activate_targets.push((
                                    d.id.clone(),
                                    existing_conn.path.clone(),
                                    d.interface_name.clone(),
                                ));
                            }
                        }
                        Err(e) => {
                            outcome.errors.insert(d.id.clone(), e);
                        }
                    }
                } else {
                    outcome.unchanged.push(d.id.clone());
                    if d.autoconnect {
                        activate_targets.push((
                            d.id.clone(),
                            existing_conn.path.clone(),
                            d.interface_name.clone(),
                        ));
                    }
                }
            }
        }
    }

    // Deletes: any existing nasty-* not in desired.
    for e in &existing {
        if !desired_ids.contains(e.id.as_str()) {
            match client.delete_connection(&e.path).await {
                Ok(()) => outcome.deleted.push(e.id.clone()),
                Err(err) => {
                    outcome.errors.insert(e.id.clone(), err);
                }
            }
        }
    }

    // Activate phase. Each activation is independent — failure to
    // activate one connection doesn't abort the others. A common
    // benign failure is "no NM-managed device matches this iface
    // name" (e.g., the iface is in `unmanaged-devices` or hasn't
    // come up yet); we surface that as an error in the outcome map
    // so the caller can decide whether to retry.
    for (id, conn_path, iface_name) in &activate_targets {
        let device = match client.find_device_by_name(iface_name).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                outcome.errors.insert(
                    id.clone(),
                    format!("no NM-managed device matches interface '{iface_name}'"),
                );
                continue;
            }
            Err(e) => {
                outcome.errors.insert(id.clone(), e);
                continue;
            }
        };
        match client.activate_connection(conn_path, &device).await {
            Ok(_active) => outcome.activated.push(id.clone()),
            Err(e) => {
                outcome.errors.insert(id.clone(), format!("activate: {e}"));
            }
        }
    }

    Ok(outcome)
}

/// Quick "any section differs" check, mirroring `diff_sections`. Returns
/// true if either dict has a section the other lacks, or any shared
/// section disagrees on contents (compared via Debug repr — same
/// caveat as `compute_diff`).
fn section_changed_any(a: &SettingsDict, b: &SettingsDict) -> bool {
    !diff_sections(a, b).is_empty()
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::nm::{
        NmConnection, NmConnectionType, NmIpMethod, NmIpSettings, NmTypeSpecific,
    };

    fn ethernet_profile(id: &str) -> NmConnection {
        NmConnection {
            sriov_num_vfs: None,
            vfs: Vec::new(),
            id: id.into(),
            uuid: format!("uuid-for-{id}"),
            conn_type: NmConnectionType::Ethernet,
            interface_name: id.trim_start_matches("nasty-").into(),
            controller: None,
            port_type: None,
            mtu: None,
            mac: None,
            autoconnect: true,
            ipv4: NmIpSettings {
                method: NmIpMethod::Auto,
                ..Default::default()
            },
            ipv6: NmIpSettings::default(),
            type_specific: NmTypeSpecific::None,
        }
    }

    #[test]
    fn diff_empty_when_nothing_exists_and_nothing_desired() {
        let d = compute_diff(&[], &[]);
        assert_eq!(d.summary.add, 0);
        assert_eq!(d.summary.update, 0);
        assert_eq!(d.summary.delete, 0);
        assert_eq!(d.summary.unchanged, 0);
    }

    #[test]
    fn diff_lists_new_profiles_as_add() {
        let d = compute_diff(&[ethernet_profile("nasty-eth0")], &[]);
        assert_eq!(d.to_add, vec!["nasty-eth0".to_string()]);
        assert_eq!(d.summary.add, 1);
    }

    #[test]
    fn diff_lists_orphaned_existing_as_delete() {
        // A nasty-* connection in NM that's no longer in our desired
        // set — the user removed the link.
        let existing = NmExisting {
            path: OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/1").unwrap(),
            id: "nasty-old".into(),
            settings: HashMap::new(),
        };
        let d = compute_diff(&[], &[existing]);
        assert_eq!(d.to_delete, vec!["nasty-old".to_string()]);
        assert_eq!(d.summary.delete, 1);
    }

    #[test]
    fn diff_reports_unchanged_when_settings_match() {
        // Existing connection with identical settings to the desired
        // profile should be classified as unchanged.
        let desired = ethernet_profile("nasty-eth0");
        let existing = NmExisting {
            path: OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/1").unwrap(),
            id: "nasty-eth0".into(),
            settings: to_settings_dict(&desired),
        };
        let d = compute_diff(&[desired], &[existing]);
        assert_eq!(d.summary.unchanged, 1);
        assert_eq!(d.summary.update, 0);
        assert!(d.to_update.is_empty());
    }

    #[test]
    fn diff_reports_changed_section_when_settings_differ() {
        // Existing has IPv4 manual; desired is auto. The diff should
        // flag `ipv4` as the changed section.
        let mut desired = ethernet_profile("nasty-eth0");
        desired.ipv4.method = NmIpMethod::Auto;

        let mut existing_profile = ethernet_profile("nasty-eth0");
        existing_profile.ipv4.method = NmIpMethod::Manual;
        existing_profile.ipv4.addresses = vec!["10.0.0.5/24".into()];

        let existing = NmExisting {
            path: OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/1").unwrap(),
            id: "nasty-eth0".into(),
            settings: to_settings_dict(&existing_profile),
        };
        let d = compute_diff(&[desired], &[existing]);
        assert_eq!(d.summary.update, 1);
        assert_eq!(d.to_update[0].id, "nasty-eth0");
        assert!(
            d.to_update[0]
                .changed_sections
                .contains(&"ipv4".to_string()),
            "ipv4 should be flagged as changed; got {:?}",
            d.to_update[0].changed_sections
        );
    }
}
