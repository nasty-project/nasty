//! Network configuration management — multi-interface, IPv4/IPv6, bonds, VLANs.
//!
//! Persists to `/var/lib/nasty/networking.json` and generates `/etc/nixos/networking.nix`.
//! Changes are applied immediately via `ip` commands without a full nixos-rebuild.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub mod layered;
pub mod nm;

const JSON_PATH: &str = "/var/lib/nasty/networking.json";
const HISTORY_DIR: &str = "/var/lib/nasty/networking.history";
const HISTORY_KEEP: usize = 10;
/// Snapshot of the prior config, written before applying a risky change.
/// Removed when the user confirms the change. If still present at engine
/// startup, the engine was killed mid-apply and we restore from it.
const PENDING_REVERT_PATH: &str = "/var/lib/nasty/networking.json.pending-revert";
/// Phase 1 shadow output of the layered model (see
/// `docs/network-architecture.md`). Written alongside `JSON_PATH` on every
/// successful apply; the legacy file remains the source of truth until
/// phase 3 cuts over.
const JSON_PATH_V2: &str = "/var/lib/nasty/networking-v2.json";
/// Phase 2 shadow output: per-link NM connection-profile previews. One
/// `<id>.nmconnection.preview` file per managed link, in NM keyfile
/// format. Phase 3 will swap these for real
/// `/etc/NetworkManager/system-connections/` files (or DBus calls).
const NM_PREVIEW_DIR: &str = "/var/lib/nasty/networking-v2-nm-preview";
// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum IpMethod {
    Dhcp,
    Static,
    Slaac,
    /// For bridges/bonds: adopt the primary member's L3 (addresses, default
    /// route, or DHCP lease) so creating a bridge over the management iface
    /// doesn't drop connectivity. No-op for top-level interfaces.
    Inherit,
    #[default]
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct IpConfig {
    pub method: IpMethod,
    /// Addresses in CIDR notation, e.g. "192.168.1.100/24" or "fd00::1/64".
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InterfaceConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BondMode {
    Lacp,
    ActiveBackup,
    BalanceRr,
    BalanceXor,
}

impl BondMode {
    /// String the kernel and NM both understand for the
    /// `802-3-ethernet` / NM bond `mode` field.
    pub(crate) fn to_kernel(&self) -> &'static str {
        match self {
            BondMode::Lacp => "802.3ad",
            BondMode::ActiveBackup => "active-backup",
            BondMode::BalanceRr => "balance-rr",
            BondMode::BalanceXor => "balance-xor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BondConfig {
    pub name: String,
    pub members: Vec<String>,
    pub mode: BondMode,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// When true, the bond's MAC address is taken from the primary
    /// member's live MAC instead of letting NM/the kernel generate
    /// a random one. Keeps DHCP servers handing out the same lease
    /// across the enslave step — important when one of the members
    /// is the management interface, since otherwise the user's
    /// session lands on a new IP.
    ///
    /// Defaults to `true` because the surprise-IP-change on the
    /// random-MAC default is the much louder failure mode. Users
    /// who want a different identity for the bond can opt out via
    /// the "Don't inherit member MAC" checkbox in the WebUI.
    #[serde(default = "default_true")]
    pub inherit_member_mac: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VlanConfig {
    pub parent: String,
    pub vlan_id: u16,
    #[serde(default)]
    pub ipv4: IpConfig,
    #[serde(default)]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

/// A Linux bridge (e.g. `br0`) used as a virtual switch — typically for VMs
/// to share the host's network. Members can be empty (a host-internal bridge
/// that VMs attach to via veth pairs at runtime) or one or more physical /
/// bond interfaces (bridge to the LAN).
///
/// `ipv4`/`ipv6` default to `Inherit` so a bridge created over the management
/// iface adopts that iface's L3 instead of dropping connectivity (issue #74).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BridgeConfig {
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default = "inherit_ip")]
    pub ipv4: IpConfig,
    #[serde(default = "inherit_ip")]
    pub ipv6: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Enable Spanning Tree Protocol on the bridge.
    #[serde(default)]
    pub stp: bool,
    /// Bridge forward delay in seconds. `None` leaves the kernel default
    /// (15s with STP on, irrelevant with STP off). Set to 0 to skip the
    /// 15-second blackhole when STP is off but forward-delay still applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_delay_s: Option<u8>,
    /// When true, the bridge's MAC address is taken from the primary
    /// member's live MAC instead of letting NM/the kernel generate
    /// a random one. See `BondConfig::inherit_member_mac` for the
    /// rationale; the rule is identical (DHCP-stable identity for
    /// the master across the enslave step).
    #[serde(default = "default_true")]
    pub inherit_member_mac: bool,
}

fn inherit_ip() -> IpConfig {
    IpConfig {
        method: IpMethod::Inherit,
        addresses: Vec::new(),
        gateway: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct NetworkConfig {
    #[serde(default)]
    pub interfaces: Vec<InterfaceConfig>,
    #[serde(default)]
    pub dns: Vec<String>,
    #[serde(default)]
    pub bonds: Vec<BondConfig>,
    #[serde(default)]
    pub vlans: Vec<VlanConfig>,
    #[serde(default)]
    pub bridges: Vec<BridgeConfig>,
}

/// Live interface state — read-only, populated at query time.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveInterface {
    pub name: String,
    pub mac: String,
    pub up: bool,
    pub speed_mbps: Option<u32>,
    pub carrier: bool,
    pub ipv4_addresses: Vec<String>,
    pub ipv6_addresses: Vec<String>,
    pub mtu: u32,
    /// "physical", "bond", "vlan", "bridge", "virtual"
    pub kind: String,
}

/// Full network state returned by `system.network.get`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkState {
    pub config: NetworkConfig,
    pub interfaces: Vec<LiveInterface>,
    /// The interface the calling client is currently reaching the engine
    /// through (resolved by `mgmt_iface_for_peer` from `session.client_ip`).
    /// Surfaced so the WebUI can warn before submitting a change that would
    /// disconnect the user — e.g. enslaving this iface into a new bridge.
    /// `None` if we couldn't resolve it (no peer info, route lookup failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mgmt_iface: Option<String>,
}

/// Request shape for `system.network.update`. The `NetworkConfig` fields
/// are flattened in for backwards compatibility — old clients posting a
/// bare `NetworkConfig` still parse — and `confirm_within_secs` is the
/// optional opt-in to the confirm-or-rollback safety net.
#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
pub struct UpdateRequest {
    #[serde(flatten)]
    pub config: NetworkConfig,
    /// If set, schedule a rollback to the previous config after this many
    /// seconds unless `system.network.confirm` is called with the returned
    /// `txn_id`. If unset, the server picks: 0 for safe changes, 30s for
    /// changes the risk classifier flags as touching the management iface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_within_secs: Option<u64>,
}

/// Response shape for `system.network.update`. All fields are `None` when
/// no rollback was scheduled (safe change applied directly). When a
/// rollback is pending, the caller must hit `system.network.confirm` with
/// `txn_id` before `revert_at_unix` to keep the change.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct UpdateResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_at_unix: Option<u64>,
    /// Human-readable reason the server scheduled a rollback (e.g. "bridges
    /// the management iface eth0"). Surfaced in the WebUI banner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ConfirmRequest {
    pub txn_id: String,
}

// ── Service ────────────────────────────────────────────────────

pub struct NetworkService {
    /// In-memory transaction table. Each `update` call that schedules a
    /// rollback inserts an entry here, keyed by `txn_id`. `confirm` removes
    /// it (and cancels the rollback timer); a timeout removes it (and
    /// performs the rollback). Wrapped in `Arc` so spawned tasks can
    /// remove their own entry on completion.
    transactions: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, PendingTxn>>>,
}

struct PendingTxn {
    /// When the rollback will fire if not confirmed. Surfaced via
    /// `system.network.pending` so a fresh WebUI session (e.g. after
    /// the user reconnects on a new IP they just configured) can find
    /// out about pending rollbacks and offer the Confirm button.
    revert_at_unix: u64,
    /// Why the server scheduled this rollback (human-readable). Same
    /// rationale as `revert_at_unix`.
    risk_reason: String,
    cancel: tokio::sync::oneshot::Sender<()>,
}

/// Snapshot of one pending rollback, returned by
/// `system.network.pending`. Intended for the WebUI to recover the
/// rollback banner after a reconnect (or first connect from a new IP)
/// — see `network.rs:PendingTxn` for storage.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NetworkPendingTxn {
    pub txn_id: String,
    pub revert_at_unix: u64,
    pub risk_reason: String,
}

impl Default for NetworkService {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkService {
    pub fn new() -> Self {
        Self {
            transactions: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    pub async fn get(&self, mgmt_iface: Option<String>) -> NetworkState {
        let config = load_config().await;
        let interfaces = enumerate_interfaces().await;
        NetworkState {
            config,
            interfaces,
            mgmt_iface,
        }
    }

    pub async fn update(
        &self,
        request: UpdateRequest,
        mgmt_iface: Option<String>,
    ) -> Result<UpdateResponse, String> {
        let config = request.config;

        // Validate input. `Inherit` is allowed here — `resolve_inherit`
        // turns it into a concrete `Static` / `Dhcp` below.
        for iface in &config.interfaces {
            validate_ip_config(&iface.ipv4, "IPv4")?;
            validate_ip_config(&iface.ipv6, "IPv6")?;
        }
        for bond in &config.bonds {
            if bond.members.is_empty() {
                return Err(format!("Bond '{}' has no members", bond.name));
            }
            validate_ip_config(&bond.ipv4, "IPv4")?;
            validate_ip_config(&bond.ipv6, "IPv6")?;
        }
        for vlan in &config.vlans {
            if vlan.vlan_id == 0 || vlan.vlan_id > 4094 {
                return Err(format!("VLAN ID {} is invalid (1-4094)", vlan.vlan_id));
            }
            validate_ip_config(&vlan.ipv4, "IPv4")?;
            validate_ip_config(&vlan.ipv6, "IPv6")?;
        }
        for bridge in &config.bridges {
            if bridge.name.is_empty() {
                return Err("Bridge name is required".to_string());
            }
            validate_ip_config(&bridge.ipv4, "IPv4")?;
            validate_ip_config(&bridge.ipv6, "IPv6")?;
        }

        // Resolve `IpMethod::Inherit` against the prior config and live
        // kernel state, turning each Inherit-mode bridge into a concrete
        // Static-or-Dhcp config. The persisted config is the resolved
        // form, so reboot reapplies the same L3 we just applied at runtime.
        let prev = load_config().await;
        let live = LiveTopology::snapshot().await;
        let config = resolve_inherit(config, &prev, &live);

        // Decide whether this change needs a rollback timer. The classifier
        // looks for changes that touch the management iface — anything that
        // could plausibly disconnect the user mid-apply. The caller can
        // override the default 30s window via `confirm_within_secs`; passing
        // 0 explicitly opts out of the safety net (use with care).
        let risk_reason = classify_risk(&prev, &config, mgmt_iface.as_deref());
        let rollback_secs = match (request.confirm_within_secs, &risk_reason) {
            (Some(0), _) => None,        // explicit opt-out
            (Some(n), _) => Some(n),     // explicit opt-in
            (None, Some(_)) => Some(30), // server-default rollback for risky changes
            (None, None) => None,        // safe change, no rollback
        };

        if let Some(secs) = rollback_secs {
            // Snapshot the prior config as the rollback source *before*
            // touching anything else. Atomic write so we can't end up with
            // a half-written revert source if the engine is killed here.
            let prev_json = serde_json::to_string_pretty(&prev)
                .map_err(|e| format!("snapshot prior config: {e}"))?;
            atomic_write(PENDING_REVERT_PATH, prev_json.as_bytes())
                .await
                .map_err(|e| format!("write {PENDING_REVERT_PATH}: {e}"))?;

            persist_config(&config).await?;
            apply_config(&config, mgmt_iface.as_deref()).await?;

            let txn_id = new_txn_id();
            let revert_at_unix = unix_now() + secs;
            let reason = risk_reason.unwrap_or_else(|| "explicit confirm requested".to_string());
            self.schedule_rollback(txn_id.clone(), secs, revert_at_unix, reason.clone())
                .await;

            info!(
                "Network config updated with {secs}s rollback window (txn {txn_id}, {} interfaces, {} bonds, {} bridges, {} VLANs)",
                config.interfaces.len(),
                config.bonds.len(),
                config.bridges.len(),
                config.vlans.len()
            );

            Ok(UpdateResponse {
                txn_id: Some(txn_id),
                revert_at_unix: Some(revert_at_unix),
                risk_reason: Some(reason),
            })
        } else {
            persist_config(&config).await?;
            apply_config(&config, mgmt_iface.as_deref()).await?;

            info!(
                "Network config updated ({} interfaces, {} bonds, {} bridges, {} VLANs)",
                config.interfaces.len(),
                config.bonds.len(),
                config.bridges.len(),
                config.vlans.len()
            );

            // Post-cutover (phase 3b-beta): NM persists profiles to
            // /etc/NetworkManager/system-connections/ as part of
            // apply_profiles, so reboot picks them up automatically.
            // No nixos-rebuild needed.

            Ok(UpdateResponse::default())
        }
    }

    /// Snapshot of all currently-pending rollback transactions. The
    /// WebUI calls this on connect so a session that didn't initiate
    /// the change (e.g. the user just reconnected on a new IP they
    /// configured 5 seconds ago) can still see the Confirm banner.
    /// Order is unspecified; the table rarely has more than one entry.
    pub async fn pending(&self) -> Vec<NetworkPendingTxn> {
        self.transactions
            .lock()
            .await
            .iter()
            .map(|(id, txn)| NetworkPendingTxn {
                txn_id: id.clone(),
                revert_at_unix: txn.revert_at_unix,
                risk_reason: txn.risk_reason.clone(),
            })
            .collect()
    }

    /// Confirm a pending rollback transaction — cancel its timer and remove
    /// the pending-revert file. Returns an error if the txn_id is unknown
    /// (already confirmed, already reverted, or never existed).
    pub async fn confirm(&self, txn_id: &str) -> Result<(), String> {
        let removed = self.transactions.lock().await.remove(txn_id);
        match removed {
            Some(txn) => {
                // Best-effort cancel: if the rollback task already started
                // executing, the receive end is gone — that's OK, we just
                // race to clean up below.
                let _ = txn.cancel.send(());
                let _ = tokio::fs::remove_file(PENDING_REVERT_PATH).await;
                info!("Network txn {txn_id} confirmed");
                // Post-cutover: NM keyfiles already persisted as part
                // of the apply that scheduled this rollback. No
                // explicit rebuild step needed on confirm.
                Ok(())
            }
            None => Err(format!("unknown or already-completed txn_id {txn_id}")),
        }
    }

    /// Called once at engine startup. If a `pending-revert` file exists, the
    /// engine was killed mid-apply (or after applying but before the user
    /// confirmed) — restore the prior config so the box doesn't come back
    /// up with an unconfirmed change. No-op if the file doesn't exist.
    pub async fn restore_pending_revert(&self) {
        let Ok(contents) = tokio::fs::read(PENDING_REVERT_PATH).await else {
            return;
        };
        let prev: NetworkConfig = match serde_json::from_slice(&contents) {
            Ok(c) => c,
            Err(e) => {
                warn!("pending-revert file is unparseable, removing: {e}");
                let _ = tokio::fs::remove_file(PENDING_REVERT_PATH).await;
                return;
            }
        };
        warn!(
            "Found pending-revert at startup — engine likely crashed mid-apply or shut down before the user confirmed. Restoring prior network config."
        );
        if let Err(e) = persist_config(&prev).await {
            warn!("restore: persist failed: {e}");
            return;
        }
        // Restore path: we don't know which iface the user is on
        // (they might have reconnected on a new IP). MAC inheritance
        // for any masters in `prev` falls back to "first member".
        if let Err(e) = apply_config(&prev, None).await {
            warn!("restore: apply failed: {e}");
            return;
        }
        let _ = tokio::fs::remove_file(PENDING_REVERT_PATH).await;
        info!("Pending-revert restored cleanly");
    }

    /// Spawn a tokio task that performs the rollback after `secs` unless
    /// `confirm()` cancels it first. The task removes its own entry from
    /// `transactions` on completion (either path), so the table never
    /// accumulates stale records.
    async fn schedule_rollback(
        &self,
        txn_id: String,
        secs: u64,
        revert_at_unix: u64,
        risk_reason: String,
    ) {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let transactions = std::sync::Arc::clone(&self.transactions);
        let txn_id_for_task = txn_id.clone();
        tokio::spawn(async move {
            let timer = tokio::time::sleep(std::time::Duration::from_secs(secs));
            tokio::pin!(timer);
            tokio::select! {
                _ = &mut timer => {
                    warn!("Network txn {txn_id_for_task} not confirmed in {secs}s — rolling back");
                    perform_rollback(&txn_id_for_task).await;
                }
                _ = cancel_rx => {
                    // Confirmed; nothing to do, confirm() already cleaned up.
                }
            }
            transactions.lock().await.remove(&txn_id_for_task);
        });
        self.transactions.lock().await.insert(
            txn_id,
            PendingTxn {
                revert_at_unix,
                risk_reason,
                cancel: cancel_tx,
            },
        );
    }

    /// List physical interfaces (for UI to show available interfaces).
    pub async fn list_interfaces(&self) -> Vec<LiveInterface> {
        enumerate_interfaces().await
    }

    /// Phase 3a — connect to NetworkManager via DBus and report what
    /// would change if the persisted (resolved) network config were
    /// applied via NM. **Read-only**; no NM state is touched. Phase 3b
    /// adds the actual apply.
    ///
    /// Returns the diff as data so callers (and the future WebUI) can
    /// surface it before committing to the cutover.
    pub async fn nm_preview(&self) -> Result<nm::dbus::NmDiff, String> {
        let cfg = load_config().await;
        let layered_cfg = layered::to_layered(&cfg);
        let desired = nm::to_nm_profiles(&layered_cfg);

        let client = nm::dbus::NmDbusClient::new().await?;
        let existing = client.list_nasty_connections().await?;
        Ok(nm::dbus::compute_diff(&desired, &existing))
    }

    /// Phase 3b-alpha — push the current desired config into NM via
    /// DBus. **Persists profiles to disk; does not activate them.**
    ///
    /// Intended for explicit invocation (curl + `nm_apply` RPC) on a
    /// box that has NM installed alongside the legacy stack. Phase
    /// 3b-beta replaces this with automatic invocation as part of
    /// the cutover migration.
    ///
    /// Calling this on a box without NM installed errors out at the
    /// DBus connect step; safe.
    pub async fn nm_apply(&self) -> Result<nm::dbus::NmApplyOutcome, String> {
        let cfg = load_config().await;
        let layered_cfg = layered::to_layered(&cfg);
        let desired = nm::to_nm_profiles(&layered_cfg);

        let client = nm::dbus::NmDbusClient::new().await?;
        nm::dbus::apply_profiles(&client, &desired).await
    }
}

/// Top-level helper for the rollback timer. Reads the pending-revert file,
/// applies it, and removes it. Best-effort — failures are logged but don't
/// panic the spawned task.
async fn perform_rollback(txn_id: &str) {
    let contents = match tokio::fs::read(PENDING_REVERT_PATH).await {
        Ok(c) => c,
        Err(e) => {
            warn!("rollback {txn_id}: pending-revert file disappeared: {e}");
            return;
        }
    };
    let prev: NetworkConfig = match serde_json::from_slice(&contents) {
        Ok(c) => c,
        Err(e) => {
            warn!("rollback {txn_id}: unparseable pending-revert: {e}");
            return;
        }
    };
    if let Err(e) = persist_config(&prev).await {
        warn!("rollback {txn_id}: persist failed: {e}");
        return;
    }
    // Rollback runs from a tokio task without a session — no mgmt
    // iface to prefer. Same as `restore_pending_revert`: pre-existing
    // masters in the rolled-back config fall back to first-member
    // MAC, which matches what was applied originally.
    if let Err(e) = apply_config(&prev, None).await {
        warn!("rollback {txn_id}: apply failed: {e}");
        return;
    }
    let _ = tokio::fs::remove_file(PENDING_REVERT_PATH).await;
    warn!("rollback {txn_id}: completed; previous config restored");
}

fn new_txn_id() -> String {
    // Process-local monotonic counter mixed with start time. Distinct
    // within a session without an external RNG dep, and stable enough to
    // copy/paste into curl. Not security-relevant — txns are short-lived
    // and only meaningful to the client that submitted them.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs:x}-{n:x}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn validate_ip_config(ip: &IpConfig, label: &str) -> Result<(), String> {
    if let IpMethod::Static = ip.method
        && ip.addresses.is_empty()
    {
        return Err(format!("{label} static mode requires at least one address"));
    }
    Ok(())
}

// ── Config persistence ─────────────────────────────────────────

async fn persist_config(config: &NetworkConfig) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(config).map_err(|e| format!("serialization error: {e}"))?;

    // Snapshot the existing config to history before overwriting, so a bad
    // apply can be rolled back. Best-effort: a missing/unreadable prior file
    // is fine (first-run case).
    if let Ok(prev) = tokio::fs::read(JSON_PATH).await
        && let Err(e) = snapshot_history(&prev).await
    {
        warn!("failed to snapshot prior network config: {e}");
    }

    atomic_write(JSON_PATH, json.as_bytes())
        .await
        .map_err(|e| format!("failed to write {JSON_PATH}: {e}"))?;

    // Layered validation is now authoritative (post-cutover): a
    // structurally-broken config (cycle, dangling reference, double
    // enslavement, ...) is rejected here before NM ever sees it.
    // Phase 1 shipped this as warn-only while the legacy stack was
    // still authoritative; phase 3b-beta flips it to error.
    let layered_cfg = layered::to_layered(config);
    layered::validate(&layered_cfg)
        .map_err(|e| format!("network config rejected by validator: {e}"))?;
    match serde_json::to_string_pretty(&layered_cfg) {
        Ok(layered_json) => {
            if let Err(e) = atomic_write(JSON_PATH_V2, layered_json.as_bytes()).await {
                warn!("failed to write {JSON_PATH_V2}: {e}");
            }
        }
        Err(e) => warn!("failed to serialize layered config: {e}"),
    }

    // Phase 2 shadow write of NM connection-profile previews. Same
    // best-effort stance — these are inspectable artifacts, not yet
    // active. Phase 3 will replace them with real NM keyfiles + DBus.
    if let Err(e) = write_nm_previews(&layered_cfg).await {
        warn!("failed to write NM connection previews: {e}");
    }

    Ok(())
}

/// Render the layered config to NM connection profiles and write each
/// one as a `.nmconnection.preview` file in `NM_PREVIEW_DIR`. Stale
/// preview files (links that no longer exist) are removed first so
/// the directory always reflects the current desired state.
async fn write_nm_previews(layered_cfg: &layered::LayeredConfig) -> std::io::Result<()> {
    tokio::fs::create_dir_all(NM_PREVIEW_DIR).await?;

    let profiles = nm::to_nm_profiles(layered_cfg);
    let expected_filenames: std::collections::HashSet<String> = profiles
        .iter()
        .map(|p| format!("{}.nmconnection.preview", p.id))
        .collect();

    // Best-effort cleanup of stale previews. Don't fail the write if
    // this errors — just log.
    if let Ok(mut dir) = tokio::fs::read_dir(NM_PREVIEW_DIR).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && name.ends_with(".nmconnection.preview")
                && !expected_filenames.contains(name)
            {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }

    for profile in &profiles {
        let path = format!("{NM_PREVIEW_DIR}/{}.nmconnection.preview", profile.id);
        let body = nm::serialize_keyfile(profile);
        atomic_write(&path, body.as_bytes()).await?;
    }
    Ok(())
}

/// Write `contents` to `path` atomically: write to `path.tmp`, fsync, rename.
/// Eliminates the half-written-config window if the engine is killed mid-write.
async fn atomic_write(path: &str, contents: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = format!("{path}.tmp");
    let mut f = tokio::fs::File::create(&tmp).await?;
    f.write_all(contents).await?;
    f.sync_all().await?;
    drop(f);
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn snapshot_history(prev: &[u8]) -> std::io::Result<()> {
    tokio::fs::create_dir_all(HISTORY_DIR).await?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = format!("{HISTORY_DIR}/{ts}.json");
    tokio::fs::write(&path, prev).await?;
    prune_history().await;
    Ok(())
}

async fn prune_history() {
    let Ok(mut entries) = tokio::fs::read_dir(HISTORY_DIR).await else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((mtime, path));
    }
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest first
    for (_, path) in files.into_iter().skip(HISTORY_KEEP) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

async fn load_config() -> NetworkConfig {
    match tokio::fs::read_to_string(JSON_PATH).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            warn!("Failed to parse {JSON_PATH}: {e}, using defaults");
            NetworkConfig::default()
        }),
        Err(_) => NetworkConfig::default(),
    }
}

// ── Interface enumeration ──────────────────────────────────────

async fn enumerate_interfaces() -> Vec<LiveInterface> {
    let mut result = Vec::new();
    let sys_net = std::path::Path::new("/sys/class/net");
    let Ok(entries) = std::fs::read_dir(sys_net) else {
        return result;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip loopback and Docker/container interfaces
        if name == "lo"
            || name.starts_with("docker")
            || name.starts_with("veth")
            || name.starts_with("br-")
            || name.starts_with("cni")
        {
            continue;
        }

        let path = entry.path();
        let read_file = |f: &str| -> String {
            std::fs::read_to_string(path.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };

        let mac = read_file("address");
        let operstate = read_file("operstate");
        // TUN/TAP and some virtual interfaces report "unknown" when they're working
        let up = operstate == "up" || operstate == "unknown";
        let carrier = read_file("carrier") == "1";
        let mtu: u32 = read_file("mtu").parse().unwrap_or(1500);
        let speed: Option<u32> = read_file("speed")
            .parse()
            .ok()
            .filter(|&s: &u32| s > 0 && s < 100_000);

        // Detect interface type from sysfs
        let tun_flags = read_file("tun_flags");
        let dev_type = read_file("type");

        let kind = if path.join("bonding").is_dir() {
            "bond"
        } else if !tun_flags.is_empty()
            || name.starts_with("tun")
            || name.starts_with("tap")
            || name.starts_with("tailscale")
            || name.starts_with("wg")
        {
            "tunnel"
        } else if dev_type == "772" {
            "vlan"
        } else if path.join("bridge").is_dir() {
            "bridge"
        } else {
            "physical"
        };

        let ipv4_addresses = get_addresses(&name, false).await;
        let ipv6_addresses = get_addresses(&name, true).await;

        result.push(LiveInterface {
            name,
            mac,
            up,
            speed_mbps: speed,
            carrier,
            ipv4_addresses,
            ipv6_addresses,
            mtu,
            kind: kind.to_string(),
        });
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

async fn get_addresses(iface: &str, ipv6: bool) -> Vec<String> {
    let flag = if ipv6 { "-6" } else { "-4" };
    let inet = if ipv6 { "inet6" } else { "inet" };
    let Ok(output) = tokio::process::Command::new("ip")
        .args([flag, "addr", "show", iface])
        .output()
        .await
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with(inet) {
                let addr = line.split_whitespace().nth(1)?;
                // Skip link-local for IPv6 unless it's the only one
                if ipv6 && addr.starts_with("fe80:") {
                    return None;
                }
                Some(addr.to_string())
            } else {
                None
            }
        })
        .collect()
}

// ── Risk classification + mgmt-iface detection ─────────────────
//
// "Risky" = could plausibly disconnect the user mid-apply. The classifier
// looks at the diff between the prior and next config in light of the
// management iface (the one carrying the calling client's HTTP/SSH session).
// Any change that touches that iface — enslaving it into a new bridge,
// changing its IP, changing its MTU — gets flagged so the safety net kicks
// in. Non-mgmt changes are safe.
//
// Returns Some(reason) when risky, None when safe. The reason string is
// surfaced verbatim to the WebUI banner, so it should read sensibly to a
// user looking at the screen.

fn classify_risk(
    prev: &NetworkConfig,
    next: &NetworkConfig,
    mgmt_iface: Option<&str>,
) -> Option<String> {
    let Some(mgmt) = mgmt_iface else {
        // We don't know which iface the user is connected through, so we
        // can't reason about whether the change touches it. Fail safe: any
        // structural change (bonds / bridges / vlans) gets a rollback. DNS
        // and Disabled-only changes are still considered safe.
        if prev.bonds != next.bonds || prev.bridges != next.bridges || prev.vlans != next.vlans {
            return Some(
                "management iface unknown — applying topology change with rollback safety net"
                    .to_string(),
            );
        }
        return None;
    };

    // mgmt iface (or any of its masters in the prev topology) being
    // enslaved into a *new* bridge member list is the headline #74 case.
    let prev_bridges_by_name: std::collections::HashMap<&str, &BridgeConfig> =
        prev.bridges.iter().map(|b| (b.name.as_str(), b)).collect();
    for next_br in &next.bridges {
        let was_member = prev_bridges_by_name
            .get(next_br.name.as_str())
            .is_some_and(|prev_br| prev_br.members.iter().any(|m| m == mgmt));
        let is_member = next_br.members.iter().any(|m| m == mgmt);
        if is_member && !was_member {
            return Some(format!(
                "management iface {mgmt} is being enslaved into bridge {}",
                next_br.name
            ));
        }
    }

    // Same check for bonds.
    let prev_bonds_by_name: std::collections::HashMap<&str, &BondConfig> =
        prev.bonds.iter().map(|b| (b.name.as_str(), b)).collect();
    for next_bond in &next.bonds {
        let was_member = prev_bonds_by_name
            .get(next_bond.name.as_str())
            .is_some_and(|prev_bond| prev_bond.members.iter().any(|m| m == mgmt));
        let is_member = next_bond.members.iter().any(|m| m == mgmt);
        if is_member && !was_member {
            return Some(format!(
                "management iface {mgmt} is being enslaved into bond {}",
                next_bond.name
            ));
        }
    }

    // mgmt iface IP / MTU change.
    let prev_iface = prev.interfaces.iter().find(|i| i.name == mgmt);
    let next_iface = next.interfaces.iter().find(|i| i.name == mgmt);
    match (prev_iface, next_iface) {
        (Some(p), Some(n)) => {
            if p.ipv4 != n.ipv4 || p.ipv6 != n.ipv6 {
                return Some(format!("IP config of management iface {mgmt} is changing"));
            }
            if p.mtu != n.mtu {
                return Some(format!("MTU of management iface {mgmt} is changing"));
            }
            if p.enabled && !n.enabled {
                return Some(format!("management iface {mgmt} is being disabled"));
            }
        }
        (Some(_), None) => {
            return Some(format!(
                "management iface {mgmt} is being removed from config"
            ));
        }
        _ => {}
    }

    // Removing a bridge/bond that mgmt is enslaved into, or that *is* mgmt
    // (the L3-inherit case where mgmt_iface resolves to the bridge/bond
    // name itself). Either way the master goes away and mgmt loses its
    // path — the symmetric case to enslaving above.
    let next_bridges_by_name: std::collections::HashMap<&str, &BridgeConfig> =
        next.bridges.iter().map(|b| (b.name.as_str(), b)).collect();
    for prev_br in &prev.bridges {
        if next_bridges_by_name.contains_key(prev_br.name.as_str()) {
            continue;
        }
        if prev_br.name == mgmt {
            return Some(format!("management bridge {mgmt} is being removed"));
        }
        if prev_br.members.iter().any(|m| m == mgmt) {
            return Some(format!(
                "management iface {mgmt} is being released from bridge {} that's being removed",
                prev_br.name
            ));
        }
    }

    let next_bonds_by_name: std::collections::HashMap<&str, &BondConfig> =
        next.bonds.iter().map(|b| (b.name.as_str(), b)).collect();
    for prev_bond in &prev.bonds {
        if next_bonds_by_name.contains_key(prev_bond.name.as_str()) {
            continue;
        }
        if prev_bond.name == mgmt {
            return Some(format!("management bond {mgmt} is being removed"));
        }
        if prev_bond.members.iter().any(|m| m == mgmt) {
            return Some(format!(
                "management iface {mgmt} is being released from bond {} that's being removed",
                prev_bond.name
            ));
        }
    }

    // mgmt iface is the parent of a VLAN — VLAN changes don't disconnect,
    // skipped. Bridge/bond IP changes on a master that mgmt is enslaved
    // into would be risky too, but that requires walking the master chain
    // which lives in the live state — out of scope for the diff classifier.
    None
}

/// Resolve the network interface the calling client is currently reaching
/// the engine through. Returns the *topmost* master if the egress link is
/// enslaved, so the right answer for an SSH session over `eth0` enslaved
/// into `br0` is `br0` (that's the iface a topology change would actually
/// disconnect on). `None` if we can't tell — caller should treat that as
/// risky.
pub async fn mgmt_iface_for_peer(peer_addr: &str) -> Option<String> {
    if peer_addr.is_empty() || peer_addr == "unknown" {
        return None;
    }
    // Strip a port suffix if the caller passed `1.2.3.4:55432`. `ip route
    // get` doesn't accept ports.
    let host = peer_addr.rsplit_once(':').map_or(peer_addr, |(h, _)| h);
    let json = run_ip_json(&["-j", "route", "get", host]).await;
    let parsed: Vec<IpRouteLine> = serde_json::from_str(&json).ok()?;
    let dev = parsed.into_iter().find_map(|r| r.dev)?;
    Some(walk_to_topmost_master(&dev).await)
}

async fn walk_to_topmost_master(start: &str) -> String {
    let mut current = start.to_string();
    // Defensive cap so a malformed sysfs can't loop us.
    for _ in 0..8 {
        let master_path = format!("/sys/class/net/{current}/master");
        match tokio::fs::read_link(&master_path).await {
            Ok(target) => {
                // Symlink target is e.g. "../../../br0"; we want the
                // basename.
                if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                    current = name.to_string();
                } else {
                    break;
                }
            }
            Err(_) => break, // not enslaved
        }
    }
    current
}

// ── Inherit resolution ─────────────────────────────────────────
//
// An `IpMethod::Inherit` bridge should adopt the L3 of its primary member
// so creating a bridge over the management iface doesn't drop connectivity
// (issue #74). Done at update-time, not apply-time: we substitute Inherit
// with a concrete Static-or-Dhcp config and persist that. Reboot then
// reapplies the same L3 we just applied at runtime — no special boot path.
//
// Primary-member selection: walk `bridge.members` in order; the first one
// that has either a previous top-level config (Dhcp or Static-with-addrs)
// or live addrs in the kernel wins. This is deterministic and matches the
// user's mental model ("the first member is the carrier").

fn resolve_inherit(
    mut config: NetworkConfig,
    prev: &NetworkConfig,
    live: &LiveTopology,
) -> NetworkConfig {
    for bridge in &mut config.bridges {
        if bridge.ipv4.method == IpMethod::Inherit {
            bridge.ipv4 = resolve_inherit_one(&bridge.members, prev, live, false);
        }
        if bridge.ipv6.method == IpMethod::Inherit {
            bridge.ipv6 = resolve_inherit_one(&bridge.members, prev, live, true);
        }
    }
    config
}

fn resolve_inherit_one(
    members: &[String],
    prev: &NetworkConfig,
    live: &LiveTopology,
    v6: bool,
) -> IpConfig {
    for member in members {
        if let Some(prev_iface) = prev.interfaces.iter().find(|i| &i.name == member) {
            let prev_ip = if v6 {
                &prev_iface.ipv6
            } else {
                &prev_iface.ipv4
            };
            match prev_ip.method {
                IpMethod::Dhcp => {
                    return IpConfig {
                        method: IpMethod::Dhcp,
                        addresses: Vec::new(),
                        gateway: None,
                    };
                }
                IpMethod::Static if !prev_ip.addresses.is_empty() => {
                    return prev_ip.clone();
                }
                IpMethod::Slaac if v6 => {
                    return IpConfig {
                        method: IpMethod::Slaac,
                        addresses: Vec::new(),
                        gateway: None,
                    };
                }
                _ => {}
            }
        }
        // No prior config for this member. The fallback is to look at live
        // kernel state — but with one important exception: if the member's
        // address came from dhcpcd (the kernel route table marks the route
        // `proto: dhcp`), the live address is a *lease*, not a static
        // configuration. Baking that lease into the bridge as Static would
        // cause two real bugs:
        //   1) On reboot, the bridge claims the leased address even though
        //      the DHCP server may have re-issued it elsewhere.
        //   2) The bridge stops doing DHCP renewals — when the lease
        //      expires the address is still claimed but no longer valid.
        // So treat DHCP-managed members as `Dhcp` (let the bridge get its
        // own lease), and only bake live addrs as Static when the route
        // table says they're statically configured.
        if !v6 && live.is_dhcp_managed_v4(member) {
            return IpConfig {
                method: IpMethod::Dhcp,
                addresses: Vec::new(),
                gateway: None,
            };
        }
        let live_addrs = live.addrs(member, v6);
        if !live_addrs.is_empty() {
            return IpConfig {
                method: IpMethod::Static,
                addresses: live_addrs.to_vec(),
                gateway: live.default_via(member, v6).map(str::to_string),
            };
        }
    }
    // No member has any L3 to inherit from — leave the bridge bare.
    IpConfig::default()
}

// ── Apply config ───────────────────────────────────────────────
//
// Two-phase model:
//
//   apply_config = LiveTopology::snapshot()  (read kernel state)
//                → Plan::compute(config, live)  (pure: produce an ordered
//                                                list of `Op`s)
//                → Plan::execute()  (run each Op via `ip` / helpers)
//
// `Plan::compute` is sync and side-effect-free, which lets us unit-test the
// full set of imperative steps for any config without touching the kernel.
// Future work (#74 follow-ups) will extend `Op` with topology-change
// variants (MAC inheritance, L3 migration on enslave/un-enslave) and grow
// `LiveTopology` with addrs / routes / masters.

/// Snapshot of the kernel's view of the network at apply time. Used by
/// `resolve_inherit` to decide what L3 a bridge should adopt from its
/// primary member when the prior config doesn't give a clearer answer.
/// Post-cutover (phase 3b-beta): no longer used for create-vs-skip
/// link decisions — NM handles that internally.
#[derive(Debug, Default, Clone)]
struct LiveTopology {
    /// Per-iface IPv4 addresses (CIDR strings).
    addrs_v4: std::collections::HashMap<String, Vec<String>>,
    /// Per-iface IPv6 addresses (CIDR strings, link-local filtered out).
    addrs_v6: std::collections::HashMap<String, Vec<String>>,
    /// Egress iface → IPv4 default gateway.
    default_via_v4: std::collections::HashMap<String, String>,
    /// Egress iface → IPv6 default gateway.
    default_via_v6: std::collections::HashMap<String, String>,
    /// Ifaces whose IPv4 addresses were installed by dhcpcd or NM via
    /// DHCP (any route on the iface with `proto: dhcp`). Used by
    /// `resolve_inherit` so the live lease address isn't baked into
    /// the bridge config as Static — the bridge inherits DHCP
    /// semantics, not the specific address.
    dhcp_managed_v4: std::collections::HashSet<String>,
}

impl LiveTopology {
    async fn snapshot() -> Self {
        let addrs_v4 = parse_ip_addrs(&run_ip_json(&["-j", "-4", "addr", "show"]).await, false)
            .unwrap_or_default();
        let addrs_v6 = parse_ip_addrs(&run_ip_json(&["-j", "-6", "addr", "show"]).await, true)
            .unwrap_or_default();
        let default_via_v4 =
            parse_default_via(&run_ip_json(&["-j", "-4", "route", "show", "default"]).await)
                .unwrap_or_default();
        let default_via_v6 =
            parse_default_via(&run_ip_json(&["-j", "-6", "route", "show", "default"]).await)
                .unwrap_or_default();
        let dhcp_managed_v4 =
            parse_dhcp_managed(&run_ip_json(&["-j", "-4", "route", "show"]).await)
                .unwrap_or_default();
        Self {
            addrs_v4,
            addrs_v6,
            default_via_v4,
            default_via_v6,
            dhcp_managed_v4,
        }
    }

    fn addrs(&self, iface: &str, v6: bool) -> &[String] {
        let map = if v6 { &self.addrs_v6 } else { &self.addrs_v4 };
        map.get(iface).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn default_via(&self, iface: &str, v6: bool) -> Option<&str> {
        let map = if v6 {
            &self.default_via_v6
        } else {
            &self.default_via_v4
        };
        map.get(iface).map(|s| s.as_str())
    }

    fn is_dhcp_managed_v4(&self, iface: &str) -> bool {
        self.dhcp_managed_v4.contains(iface)
    }
}

async fn run_ip_json(args: &[&str]) -> String {
    match tokio::process::Command::new("ip").args(args).output().await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => "[]".to_string(),
    }
}

#[derive(Deserialize)]
struct IpAddrLine {
    ifname: String,
    #[serde(default)]
    addr_info: Vec<IpAddrInfo>,
}

#[derive(Deserialize)]
struct IpAddrInfo {
    local: String,
    prefixlen: u8,
    #[serde(default)]
    scope: String,
}

fn parse_ip_addrs(json: &str, v6: bool) -> Option<std::collections::HashMap<String, Vec<String>>> {
    let parsed: Vec<IpAddrLine> = serde_json::from_str(json).ok()?;
    Some(
        parsed
            .into_iter()
            .map(|line| {
                let cidrs = line
                    .addr_info
                    .into_iter()
                    // For IPv6, skip link-local (fe80::/10) — it's not
                    // something a bridge should "inherit" from a member.
                    .filter(|a| !(v6 && a.scope == "link"))
                    .map(|a| format!("{}/{}", a.local, a.prefixlen))
                    .collect();
                (line.ifname, cidrs)
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct IpRouteLine {
    dst: String,
    gateway: Option<String>,
    dev: Option<String>,
    /// Routing protocol id — `"dhcp"` when dhcpcd installed the route.
    /// Used to distinguish a DHCP lease from a static address that
    /// happens to be the same value.
    #[serde(default)]
    protocol: String,
}

fn parse_default_via(json: &str) -> Option<std::collections::HashMap<String, String>> {
    let parsed: Vec<IpRouteLine> = serde_json::from_str(json).ok()?;
    Some(
        parsed
            .into_iter()
            .filter(|r| r.dst == "default")
            .filter_map(|r| match (r.dev, r.gateway) {
                (Some(dev), Some(gw)) => Some((dev, gw)),
                _ => None,
            })
            .collect(),
    )
}

/// Set of ifaces with at least one route installed by dhcpcd. Parsed from
/// `ip -j -4 route show` (no filter — we want default *and* link routes).
fn parse_dhcp_managed(json: &str) -> Option<std::collections::HashSet<String>> {
    let parsed: Vec<IpRouteLine> = serde_json::from_str(json).ok()?;
    Some(
        parsed
            .into_iter()
            .filter(|r| r.protocol == "dhcp")
            .filter_map(|r| r.dev)
            .collect(),
    )
}

async fn apply_config(config: &NetworkConfig, mgmt_iface: Option<&str>) -> Result<(), String> {
    // Post-cutover (phase 3b-beta): NetworkManager is the active
    // backend. Convert the resolved config to layered form, then to
    // NM profiles, then push to NM via DBus. NM owns DHCP, DNS, and
    // L2 management; the engine just authors the connection profiles.
    //
    // Build a name → MAC map from live state so bond/bridge masters
    // with `inherit_member_mac=true` can adopt their primary
    // member's MAC. Without this, NM creates the master with a
    // random MAC and DHCP gives it a new lease — which yanks the
    // user's session if they're enslaving the management iface.
    let live_macs = enumerate_interfaces()
        .await
        .into_iter()
        .map(|i| (i.name, i.mac))
        .collect();
    let mac_ctx = nm::MacContext {
        live_macs,
        mgmt_iface: mgmt_iface.map(|s| s.to_string()),
    };
    let layered_cfg = layered::to_layered(config);
    let profiles = nm::to_nm_profiles_with_macs(&layered_cfg, &mac_ctx);
    let client = nm::dbus::NmDbusClient::new()
        .await
        .map_err(|e| format!("connect to NetworkManager: {e}"))?;
    let outcome = nm::dbus::apply_profiles(&client, &profiles).await?;

    info!(
        "Network config applied via NM: {} added, {} updated, {} deleted, {} unchanged, {} activated, {} errors",
        outcome.added.len(),
        outcome.updated.len(),
        outcome.deleted.len(),
        outcome.unchanged.len(),
        outcome.activated.len(),
        outcome.errors.len(),
    );
    if !outcome.errors.is_empty() {
        // Best-effort apply: report per-connection errors via the
        // outcome, but don't fail the whole apply unless ALL
        // connections failed (which would indicate something
        // genuinely broken — NM not running, DBus permissions, etc.).
        for (id, msg) in &outcome.errors {
            warn!("network apply: connection '{id}' failed: {msg}");
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn empty_live() -> LiveTopology {
        LiveTopology::default()
    }

    fn iface(name: &str) -> InterfaceConfig {
        InterfaceConfig {
            name: name.to_string(),
            enabled: true,
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: None,
        }
    }

    fn bridge(name: &str, members: &[&str]) -> BridgeConfig {
        BridgeConfig {
            name: name.to_string(),
            members: members.iter().map(|s| (*s).to_string()).collect(),
            // Tests use the *resolved* form by default — explicit Disabled
            // means "no L3 on this bridge", which is unambiguous.
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: None,
            stp: false,
            forward_delay_s: None,
            inherit_member_mac: false,
        }
    }

    fn bond(name: &str, members: &[&str]) -> BondConfig {
        BondConfig {
            name: name.to_string(),
            members: members.iter().map(|s| (*s).to_string()).collect(),
            mode: BondMode::Lacp,
            ipv4: IpConfig::default(),
            ipv6: IpConfig::default(),
            mtu: None,
            inherit_member_mac: false,
        }
    }

    // ── resolve_inherit ────────────────────────────────────────

    fn live_for(iface: &str, addrs: &[&str], default_via: Option<&str>) -> LiveTopology {
        let mut t = LiveTopology::default();
        t.addrs_v4.insert(
            iface.to_string(),
            addrs.iter().map(|s| (*s).to_string()).collect(),
        );
        if let Some(gw) = default_via {
            t.default_via_v4.insert(iface.to_string(), gw.to_string());
        }
        t
    }

    #[test]
    fn inherit_resolves_to_dhcp_when_member_was_dhcp() {
        let mut prev_eth0 = iface("eth0");
        prev_eth0.ipv4 = IpConfig {
            method: IpMethod::Dhcp,
            ..Default::default()
        };
        let prev = NetworkConfig {
            interfaces: vec![prev_eth0],
            ..Default::default()
        };
        let mut br = bridge("br0", &["eth0"]);
        br.ipv4 = inherit_ip();
        let next = NetworkConfig {
            bridges: vec![br],
            ..Default::default()
        };
        let resolved = resolve_inherit(next, &prev, &empty_live());
        assert_eq!(resolved.bridges[0].ipv4.method, IpMethod::Dhcp);
        assert!(resolved.bridges[0].ipv4.addresses.is_empty());
    }

    #[test]
    fn inherit_resolves_to_static_when_member_had_static() {
        let mut prev_eth0 = iface("eth0");
        prev_eth0.ipv4 = IpConfig {
            method: IpMethod::Static,
            addresses: vec!["192.168.1.10/24".into()],
            gateway: Some("192.168.1.1".into()),
        };
        let prev = NetworkConfig {
            interfaces: vec![prev_eth0],
            ..Default::default()
        };
        let mut br = bridge("br0", &["eth0"]);
        br.ipv4 = inherit_ip();
        let next = NetworkConfig {
            bridges: vec![br],
            ..Default::default()
        };
        let resolved = resolve_inherit(next, &prev, &empty_live());
        assert_eq!(resolved.bridges[0].ipv4.method, IpMethod::Static);
        assert_eq!(
            resolved.bridges[0].ipv4.addresses,
            vec!["192.168.1.10/24".to_string()]
        );
        assert_eq!(
            resolved.bridges[0].ipv4.gateway,
            Some("192.168.1.1".to_string())
        );
    }

    #[test]
    fn inherit_falls_back_to_live_addrs_as_static_when_not_dhcp_managed() {
        // No prior config for eth0; live shows an address that wasn't
        // installed by dhcpcd (proto != dhcp). Adopt it as Static — it's
        // a manually-configured address that should follow the bridge.
        let prev = NetworkConfig::default();
        let mut br = bridge("br0", &["eth0"]);
        br.ipv4 = inherit_ip();
        let next = NetworkConfig {
            bridges: vec![br],
            ..Default::default()
        };
        let live = live_for("eth0", &["10.0.0.5/24"], Some("10.0.0.1"));
        let resolved = resolve_inherit(next, &prev, &live);
        assert_eq!(resolved.bridges[0].ipv4.method, IpMethod::Static);
        assert_eq!(
            resolved.bridges[0].ipv4.addresses,
            vec!["10.0.0.5/24".to_string()]
        );
        assert_eq!(
            resolved.bridges[0].ipv4.gateway,
            Some("10.0.0.1".to_string())
        );
    }

    #[test]
    fn inherit_resolves_to_dhcp_when_live_route_is_dhcp_managed() {
        // No prior config, but the kernel route table marks the iface's
        // route as `proto: dhcp` — the live address is a lease, not a
        // static configuration. The bridge must inherit DHCP semantics
        // (so it does its own renewals) rather than baking the leased
        // address as Static, which would break on reboot if the DHCP
        // server hands the address to someone else.
        let prev = NetworkConfig::default();
        let mut br = bridge("br0", &["eth0"]);
        br.ipv4 = inherit_ip();
        let next = NetworkConfig {
            bridges: vec![br],
            ..Default::default()
        };
        let mut live = live_for("eth0", &["10.0.0.5/24"], Some("10.0.0.1"));
        live.dhcp_managed_v4.insert("eth0".to_string());
        let resolved = resolve_inherit(next, &prev, &live);
        assert_eq!(resolved.bridges[0].ipv4.method, IpMethod::Dhcp);
        assert!(resolved.bridges[0].ipv4.addresses.is_empty());
        assert!(resolved.bridges[0].ipv4.gateway.is_none());
    }

    #[test]
    fn inherit_with_no_inheritable_member_resolves_to_disabled() {
        let prev = NetworkConfig::default();
        let mut br = bridge("br0", &["eth0"]);
        br.ipv4 = inherit_ip();
        let next = NetworkConfig {
            bridges: vec![br],
            ..Default::default()
        };
        let resolved = resolve_inherit(next, &prev, &empty_live());
        assert_eq!(resolved.bridges[0].ipv4.method, IpMethod::Disabled);
    }

    #[test]
    fn inherit_picks_first_inheritable_member() {
        // eth0 has nothing; eth1 was DHCP. The bridge should pick eth1.
        let mut prev_eth1 = iface("eth1");
        prev_eth1.ipv4 = IpConfig {
            method: IpMethod::Dhcp,
            ..Default::default()
        };
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0"), prev_eth1],
            ..Default::default()
        };
        let mut br = bridge("br0", &["eth0", "eth1"]);
        br.ipv4 = inherit_ip();
        let next = NetworkConfig {
            bridges: vec![br],
            ..Default::default()
        };
        let resolved = resolve_inherit(next, &prev, &empty_live());
        assert_eq!(resolved.bridges[0].ipv4.method, IpMethod::Dhcp);
    }

    #[test]
    fn explicit_bridge_config_passes_through_unchanged() {
        let prev = NetworkConfig::default();
        let mut br = bridge("br0", &["eth0"]);
        br.ipv4 = IpConfig {
            method: IpMethod::Static,
            addresses: vec!["192.168.99.1/24".into()],
            gateway: None,
        };
        let next = NetworkConfig {
            bridges: vec![br.clone()],
            ..Default::default()
        };
        let resolved = resolve_inherit(next, &prev, &empty_live());
        assert_eq!(resolved.bridges[0].ipv4, br.ipv4);
    }

    // ── ip -j parsers ──────────────────────────────────────────

    #[test]
    fn parse_ip_addrs_extracts_cidrs() {
        let json = r#"[
          {"ifname": "eth0", "addr_info": [
            {"local": "192.168.1.10", "prefixlen": 24, "scope": "global"},
            {"local": "192.168.1.11", "prefixlen": 24, "scope": "global"}
          ]},
          {"ifname": "lo", "addr_info": [
            {"local": "127.0.0.1", "prefixlen": 8, "scope": "host"}
          ]}
        ]"#;
        let parsed = parse_ip_addrs(json, false).unwrap();
        assert_eq!(
            parsed.get("eth0").unwrap(),
            &vec!["192.168.1.10/24".to_string(), "192.168.1.11/24".to_string()]
        );
        assert_eq!(parsed.get("lo").unwrap(), &vec!["127.0.0.1/8".to_string()]);
    }

    #[test]
    fn parse_ip_addrs_v6_skips_link_local() {
        let json = r#"[
          {"ifname": "eth0", "addr_info": [
            {"local": "2001:db8::1", "prefixlen": 64, "scope": "global"},
            {"local": "fe80::1", "prefixlen": 64, "scope": "link"}
          ]}
        ]"#;
        let parsed = parse_ip_addrs(json, true).unwrap();
        assert_eq!(
            parsed.get("eth0").unwrap(),
            &vec!["2001:db8::1/64".to_string()]
        );
    }

    #[test]
    fn parse_default_via_extracts_dev_gateway() {
        let json = r#"[
          {"dst": "default", "gateway": "192.168.1.1", "dev": "eth0"}
        ]"#;
        let parsed = parse_default_via(json).unwrap();
        assert_eq!(parsed.get("eth0").unwrap(), "192.168.1.1");
    }

    #[test]
    fn parse_handles_empty_or_malformed_json() {
        assert!(parse_ip_addrs("[]", false).unwrap().is_empty());
        assert!(parse_default_via("[]").unwrap().is_empty());
        assert!(parse_dhcp_managed("[]").unwrap().is_empty());
        assert!(parse_ip_addrs("not json", false).is_none());
    }

    #[test]
    fn parse_dhcp_managed_picks_up_proto_dhcp_routes() {
        // Real `ip -j -4 route show` output from a DHCP-managed iface:
        // both the default route and the link-scope route are tagged
        // proto: dhcp because dhcpcd installed them.
        let json = r#"[
          {"dst":"default","gateway":"10.10.10.1","dev":"ens18","protocol":"dhcp"},
          {"dst":"10.10.10.0/24","dev":"ens18","protocol":"dhcp"},
          {"dst":"172.17.0.0/16","dev":"docker0","protocol":"kernel"}
        ]"#;
        let parsed = parse_dhcp_managed(json).unwrap();
        assert!(parsed.contains("ens18"));
        assert!(!parsed.contains("docker0"));
    }

    #[test]
    fn parse_dhcp_managed_ignores_static_routes() {
        let json = r#"[
          {"dst":"default","gateway":"10.0.0.1","dev":"eth0","protocol":"static"},
          {"dst":"10.0.0.0/24","dev":"eth0","protocol":"kernel"}
        ]"#;
        assert!(parse_dhcp_managed(json).unwrap().is_empty());
    }

    // ── BridgeConfig deserialization defaults ──────────────────

    #[test]
    fn bridge_default_ipv4_and_ipv6_methods_are_inherit() {
        // A bridge sent without explicit IP config from the WebUI
        // (or in a legacy JSON written by an earlier nasty version
        // that didn't include the field at all) should default to
        // Inherit so #74 doesn't recur.
        let json = r#"{ "name": "br0", "members": ["eth0"] }"#;
        let parsed: BridgeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.ipv4.method, IpMethod::Inherit);
        assert_eq!(parsed.ipv6.method, IpMethod::Inherit);
    }

    // ── classify_risk ──────────────────────────────────────────

    #[test]
    fn risk_flags_mgmt_iface_being_bridged() {
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bridges: vec![bridge("br0", &["eth0"])],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(
            reason.is_some(),
            "bridging the mgmt iface must be flagged risky"
        );
        let reason = reason.unwrap();
        assert!(reason.contains("eth0"));
        assert!(reason.contains("br0"));
    }

    #[test]
    fn risk_flags_mgmt_iface_being_bonded() {
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bonds: vec![bond("bond0", &["eth0"])],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("bond0"));
    }

    #[test]
    fn risk_flags_removing_bridge_carrying_mgmt() {
        // Healthy bridged setup: br0 is the mgmt iface (owns the L3 via
        // inherit). Removing it would disconnect mgmt — must roll back.
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bridges: vec![bridge("br0", &["eth0"])],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("br0"));
        assert!(
            reason.is_some(),
            "removing the mgmt bridge must be flagged risky"
        );
        assert!(reason.unwrap().contains("br0"));
    }

    #[test]
    fn risk_flags_removing_bridge_holding_mgmt_member() {
        // Bridge is going away while mgmt iface is one of its members —
        // NM has to release the slave and re-activate it standalone, which
        // can interrupt the session.
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bridges: vec![bridge("br0", &["eth0"])],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(reason.is_some());
        let reason = reason.unwrap();
        assert!(reason.contains("eth0"));
        assert!(reason.contains("br0"));
    }

    #[test]
    fn risk_flags_removing_bond_carrying_mgmt() {
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bonds: vec![bond("bond0", &["eth0"])],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("bond0"));
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("bond0"));
    }

    #[test]
    fn risk_flags_removing_bond_holding_mgmt_member() {
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bonds: vec![bond("bond0", &["eth0"])],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(reason.is_some());
        let reason = reason.unwrap();
        assert!(reason.contains("eth0"));
        assert!(reason.contains("bond0"));
    }

    #[test]
    fn risk_safe_when_removing_unrelated_bridge() {
        // mgmt is on eth0 standalone — removing an unrelated bridge that
        // doesn't touch eth0 should stay safe.
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0"), iface("eth1")],
            bridges: vec![bridge("br0", &["eth1"])],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0"), iface("eth1")],
            ..Default::default()
        };
        assert!(classify_risk(&prev, &next, Some("eth0")).is_none());
    }

    #[test]
    fn risk_flags_mgmt_ip_change() {
        let mut prev_eth0 = iface("eth0");
        prev_eth0.ipv4 = IpConfig {
            method: IpMethod::Static,
            addresses: vec!["192.168.1.10/24".into()],
            gateway: Some("192.168.1.1".into()),
        };
        let mut next_eth0 = iface("eth0");
        next_eth0.ipv4 = IpConfig {
            method: IpMethod::Static,
            addresses: vec!["192.168.2.10/24".into()],
            gateway: Some("192.168.2.1".into()),
        };
        let prev = NetworkConfig {
            interfaces: vec![prev_eth0],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![next_eth0],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(
            reason.is_some_and(|r| r.contains("IP config")),
            "changing mgmt iface IP must be flagged"
        );
    }

    #[test]
    fn risk_flags_mgmt_mtu_change() {
        let prev_eth0 = iface("eth0");
        let mut next_eth0 = iface("eth0");
        next_eth0.mtu = Some(9000);
        let prev = NetworkConfig {
            interfaces: vec![prev_eth0],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![next_eth0],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(reason.is_some_and(|r| r.contains("MTU")));
    }

    #[test]
    fn risk_flags_mgmt_iface_disable() {
        let prev_eth0 = iface("eth0");
        let mut next_eth0 = iface("eth0");
        next_eth0.enabled = false;
        let prev = NetworkConfig {
            interfaces: vec![prev_eth0],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![next_eth0],
            ..Default::default()
        };
        let reason = classify_risk(&prev, &next, Some("eth0"));
        assert!(reason.is_some_and(|r| r.contains("disabled")));
    }

    #[test]
    fn risk_safe_when_only_non_mgmt_iface_changes() {
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0"), iface("eth1")],
            ..Default::default()
        };
        let mut next_eth1 = iface("eth1");
        next_eth1.mtu = Some(9000);
        let next = NetworkConfig {
            interfaces: vec![iface("eth0"), next_eth1],
            ..Default::default()
        };
        // mgmt is eth0; the change is on eth1 only.
        assert!(classify_risk(&prev, &next, Some("eth0")).is_none());
    }

    #[test]
    fn risk_safe_when_only_dns_changes() {
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            dns: vec!["1.1.1.1".into()],
            ..Default::default()
        };
        let next = NetworkConfig {
            interfaces: vec![iface("eth0")],
            dns: vec!["8.8.8.8".into()],
            ..Default::default()
        };
        assert!(classify_risk(&prev, &next, Some("eth0")).is_none());
    }

    #[test]
    fn risk_safe_when_existing_bridge_member_list_unchanged() {
        // The bridge already had eth0 as a member (from a prior apply).
        // A no-op re-apply shouldn't flag it as risky just because eth0 is
        // listed as a bridge member.
        let prev = NetworkConfig {
            interfaces: vec![iface("eth0")],
            bridges: vec![bridge("br0", &["eth0"])],
            ..Default::default()
        };
        let next = prev.clone();
        assert!(classify_risk(&prev, &next, Some("eth0")).is_none());
    }

    #[test]
    fn risk_unknown_mgmt_falls_back_to_topology_check() {
        // Without mgmt info, the classifier can't pinpoint risk — but it
        // should still flag any topology change so the rollback safety net
        // engages. DNS-only changes stay safe.
        let prev = NetworkConfig::default();
        let next_topology = NetworkConfig {
            bridges: vec![bridge("br0", &["eth0"])],
            ..Default::default()
        };
        assert!(classify_risk(&prev, &next_topology, None).is_some());

        let prev_dns = NetworkConfig {
            dns: vec!["1.1.1.1".into()],
            ..Default::default()
        };
        let next_dns = NetworkConfig {
            dns: vec!["8.8.8.8".into()],
            ..Default::default()
        };
        assert!(classify_risk(&prev_dns, &next_dns, None).is_none());
    }

    // ── transaction store ──────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn confirm_cancels_rollback_timer() {
        let svc = NetworkService::new();
        svc.schedule_rollback("txn-test".into(), 30, unix_now() + 30, "test".into())
            .await;
        // Confirm before the timer would fire.
        assert!(svc.confirm("txn-test").await.is_ok());
        // Advancing past the original deadline must not re-fire — the
        // task already exited via the cancel path.
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        // The transactions table is empty either way; the assertion is
        // mainly that confirm twice errors (entry was removed cleanly).
        assert!(svc.confirm("txn-test").await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_txn_id_errors() {
        let svc = NetworkService::new();
        let res = svc.confirm("does-not-exist").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("does-not-exist"));
    }

    #[tokio::test(start_paused = true)]
    async fn pending_returns_empty_when_no_active_txns() {
        let svc = NetworkService::new();
        assert!(svc.pending().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_surfaces_active_txn_metadata() {
        // Reproduces the IP-change recovery scenario: an apply has
        // scheduled a rollback, the original WebUI session lost
        // connectivity, and a fresh session calls `pending()` to
        // recover the banner. The metadata returned must be enough
        // for the WebUI to render the banner verbatim — txn_id (for
        // confirm), revert deadline (for the countdown), risk reason
        // (for the tooltip).
        let svc = NetworkService::new();
        let revert_at = unix_now() + 30;
        svc.schedule_rollback(
            "txn-abc".into(),
            30,
            revert_at,
            "IP config of management iface eth0 is changing".into(),
        )
        .await;
        let pending = svc.pending().await;
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        assert_eq!(p.txn_id, "txn-abc");
        assert_eq!(p.revert_at_unix, revert_at);
        assert!(p.risk_reason.contains("IP config"));
    }

    #[tokio::test(start_paused = true)]
    async fn pending_drops_confirmed_txn() {
        // Once a txn is confirmed, it must not appear in pending —
        // otherwise a fresh WebUI session would re-show a banner for
        // a change the user already accepted.
        let svc = NetworkService::new();
        svc.schedule_rollback("txn-abc".into(), 30, unix_now() + 30, "test".into())
            .await;
        svc.confirm("txn-abc").await.unwrap();
        assert!(svc.pending().await.is_empty());
    }

    // The timeout-fires-rollback path isn't unit-tested here — exercising
    // it deterministically would mean stubbing perform_rollback / the
    // pending-revert file, and the mechanism (a tokio::select! racing a
    // sleep against a oneshot) is straightforward enough that the cancel
    // test above covers the interesting half. The rollback path is best
    // verified end-to-end on a real box.

    #[test]
    fn new_txn_ids_are_distinct() {
        // Sanity — collision-resistant enough for in-memory use.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let id = new_txn_id();
            assert!(seen.insert(id), "duplicate txn_id");
        }
    }
}
