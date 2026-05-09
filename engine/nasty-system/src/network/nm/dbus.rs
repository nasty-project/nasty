//! NetworkManager D-Bus client.
//!
//! Talks to NM via zbus. Read methods (`list_connections`,
//! `get_settings`) plus diff computation arrived in phase 3a; write
//! methods (`AddConnection` / `Update` / `Delete`) and `apply_profiles`
//! arrived in phase 3b-alpha. **No activation yet** — `apply_profiles`
//! persists keyfiles into NM but doesn't bring connections up. That
//! lets the writes ship before the migration cutover, since persisted
//! keyfiles don't affect the running system until something explicitly
//! activates them.
//!
//! Phase 3b-beta — which adds activation, runs the
//! `nixos-rebuild switch` cutover, and switches the active apply path
//! away from the legacy ip-command flow — ships separately, once the
//! migration story (idempotent runner with snapshot+rollback) is
//! built. By then the write code here is already in the field via
//! the explicit `system.network.nm_apply` RPC; bugs surface as failed
//! preview/apply on inspection rather than as broken connectivity.

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
    /// Read the connection's settings dict. Phase 3a uses this to
    /// identify NASty-managed connections (id starting with `nasty-`)
    /// and to diff against the desired profile set.
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
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    /// Kernel interface name (`eth0`, `br0`, `bond0`, ...). Phase 3b
    /// uses this to resolve a profile's `interface_name` to the device
    /// path that `ActivateConnection` needs.
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
}

// ── Client ─────────────────────────────────────────────────────

/// Read-only NM DBus client. Holds a system-bus connection and the
/// top-level proxies. Cheap to construct — proxies don't open new
/// connections.
pub struct NmDbusClient {
    conn: DbusConnection,
}

impl NmDbusClient {
    /// Connect to the system bus. Fails if NM isn't running, the bus
    /// isn't reachable, or the user lacks access. Surface the error
    /// verbatim — phase 3a's `nm_preview` RPC reports it to the
    /// caller so a misconfigured box is immediately diagnosable.
    pub async fn new() -> Result<Self, String> {
        let conn = DbusConnection::system()
            .await
            .map_err(|e| format!("connect to system DBus: {e}"))?;
        Ok(Self { conn })
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

    /// All NASty-managed connections. Discriminator: the
    /// `[connection].id` field starts with `nasty-`. Returns
    /// (object path, settings) so callers can both compare settings
    /// and identify which path to update/delete in phase 3b.
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

/// What `nm_preview` would do if it were an apply: the per-id breakdown
/// of additions, updates, and deletions. JSON-serialized for the RPC.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct NmDiff {
    /// Connection IDs present in the desired set but not currently in
    /// NM. Phase 3b would call `Settings.AddConnection`.
    pub to_add: Vec<String>,
    /// IDs present in both, but with different settings. Phase 3b would
    /// call `Connection.Update`.
    pub to_update: Vec<NmDiffUpdate>,
    /// NASty-managed IDs in NM but not in the desired set — user must
    /// have removed the link. Phase 3b would call `Connection.Delete`.
    pub to_delete: Vec<String>,
    /// Counts for at-a-glance display in the WebUI.
    pub summary: NmDiffSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NmDiffUpdate {
    pub id: String,
    /// One line per differing top-level section (e.g. `"ipv4"`,
    /// `"bridge"`). Cheap signal for the UI; phase 3b can show a
    /// richer diff.
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
/// We compare section-by-section by serialized representation — good
/// enough signal for phase 3a's preview, sidesteps the OwnedValue
/// equality awkwardness.
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
            // Compare keys-and-values via Debug repr — OwnedValue
            // doesn't impl Eq, but its Debug is stable and includes
            // the variant + payload. Phase 3b gets a real comparator
            // that walks the variant tree.
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
// Phase 3b-alpha: writes the diff to NM. **Does not activate**
// connections — profiles are persisted to
// `/etc/NetworkManager/system-connections/` but not brought up. A
// caller wanting to test a profile end-to-end can do
// `nmcli connection up nasty-<iface>` manually after `apply_profiles`
// returns. Phase 3b-beta will add automatic activation as part of the
// cutover.
//
// Why no activation here: activation deactivates whatever was holding
// the iface previously. On a box still running the legacy ip-command
// apply path, that's the live network config — auto-activating an NM
// profile would drop connectivity. Keeping `apply_profiles` to
// "persist only" makes phase 3b-alpha safe to invoke at any time on
// any box that has NM installed.

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct NmApplyOutcome {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub unchanged: Vec<String>,
    /// Per-id error map. Empty on full success. The apply is best-
    /// effort — one failed connection doesn't abort the rest.
    pub errors: HashMap<String, String>,
}

/// Apply a desired profile set to NM. Computes the diff against the
/// current `nasty-*` connections, then issues `AddConnection` /
/// `Update` / `Delete` calls. Persists everything to disk via NM;
/// does not activate.
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

    // Adds first: a new bridge profile may be a controller for a port
    // we're about to add; NM resolves controller-by-name lazily, so
    // the order doesn't matter for correctness, but adding masters
    // first matches operator intuition when watching journalctl.
    for d in desired {
        match existing_by_id.get(d.id.as_str()) {
            None => {
                let dict = super::to_settings_dict(d);
                match client.add_connection(dict).await {
                    Ok(_path) => outcome.added.push(d.id.clone()),
                    Err(e) => {
                        outcome.errors.insert(d.id.clone(), e);
                    }
                }
            }
            Some(existing_conn) => {
                let dict = super::to_settings_dict(d);
                if section_changed_any(&dict, &existing_conn.settings) {
                    match client.update_connection(&existing_conn.path, dict).await {
                        Ok(()) => outcome.updated.push(d.id.clone()),
                        Err(e) => {
                            outcome.errors.insert(d.id.clone(), e);
                        }
                    }
                } else {
                    outcome.unchanged.push(d.id.clone());
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
                addresses: vec![],
                gateway: None,
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
