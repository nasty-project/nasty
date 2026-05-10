import type { InterfaceConfig, LiveInterface, NetworkConfig } from './types';

/** A standalone interface entry with default DHCP / SLAAC L3.
 * Used when a bond/bridge member becomes orphaned by its master's
 * removal — DHCP is the right default because most homelab boxes
 * are on DHCP, and it matches what NM's auto-default would do anyway
 * if we left the iface unconfigured. The user can edit to static
 * after the apply if needed. */
function defaultStandaloneIface(name: string): InterfaceConfig {
	return {
		name,
		enabled: true,
		ipv4: { method: 'dhcp', addresses: [], gateway: null },
		ipv6: { method: 'slaac', addresses: [], gateway: null },
		mtu: null,
	};
}

/** When a bond or bridge is removed, its members lose their master and
 * would otherwise drop out of the config entirely (they were only
 * referenced via `master.members`). Promote each orphaned member to a
 * standalone `InterfaceConfig` with DHCP defaults — unless it's already
 * a standalone interface, or still a member of another master.
 *
 * `removedMaster` is the (kind, name) of the bond/bridge being deleted;
 * we ignore references to it when checking whether a member is still
 * mastered, since the caller is *about* to apply a payload that no
 * longer contains it.
 *
 * Returns the new `interfaces` array. Existing entries are preserved
 * (and not duplicated). VLANs aren't considered — they have a `parent`,
 * not `members`, so they don't orphan anything when removed. */
export function promoteOrphanedMembers(
	network: NetworkConfig,
	removedMaster: { kind: 'bond' | 'bridge'; name: string },
	members: string[],
): InterfaceConfig[] {
	const existing = new Set((network.interfaces ?? []).map((i) => i.name));
	const stillMastered = (iface: string) => {
		const inBond = (network.bonds ?? []).some(
			(b) =>
				!(removedMaster.kind === 'bond' && b.name === removedMaster.name) &&
				b.members.includes(iface),
		);
		const inBridge = (network.bridges ?? []).some(
			(b) =>
				!(removedMaster.kind === 'bridge' && b.name === removedMaster.name) &&
				b.members.includes(iface),
		);
		return inBond || inBridge;
	};

	const promoted: InterfaceConfig[] = [];
	for (const m of members) {
		if (existing.has(m)) continue;
		if (stillMastered(m)) continue;
		promoted.push(defaultStandaloneIface(m));
		existing.add(m);
	}
	return [...(network.interfaces ?? []), ...promoted];
}

/** Find `interfaces[]` entries that don't correspond to any real device
 * and aren't a placeholder for a configured-but-not-yet-applied virtual
 * interface (bond/bridge/vlan). These are stale entries from past
 * topology edits and trip the engine's "duplicate link name" validator
 * the next time the user tries to create a bond/bridge with the same
 * name — see issue #96.
 *
 * The check is conservative: we only flag a name when *all three*
 * sources fail to claim it (live device list, bonds, bridges, vlans).
 * That means a NIC that's currently disconnected (down with carrier
 * lost but still in sysfs) won't be flagged — its name is still in
 * the live list — and a freshly-added bond that hasn't been applied
 * yet stays clear because the bonds[] entry covers it.
 *
 * Returns the orphan names, in their order from `interfaces[]`. */
export function findOrphanInterfaces(
	network: NetworkConfig,
	live: LiveInterface[],
): string[] {
	const liveNames = new Set(live.map((i) => i.name));
	const bondNames = new Set((network.bonds ?? []).map((b) => b.name));
	const bridgeNames = new Set((network.bridges ?? []).map((b) => b.name));
	const vlanNames = new Set(
		(network.vlans ?? []).map((v) => `${v.parent}.${v.vlan_id}`),
	);
	return (network.interfaces ?? [])
		.filter(
			(i) =>
				!liveNames.has(i.name) &&
				!bondNames.has(i.name) &&
				!bridgeNames.has(i.name) &&
				!vlanNames.has(i.name),
		)
		.map((i) => i.name);
}

/** Strip the named entries from `network.interfaces[]`. Used to build
 * the cleanup payload for `applyNetworkUpdate` after the user clicks
 * the orphan-banner Apply button. Pure — doesn't touch live state. */
export function stripInterfaces(
	network: NetworkConfig,
	names: string[],
): InterfaceConfig[] {
	const drop = new Set(names);
	return (network.interfaces ?? []).filter((i) => !drop.has(i.name));
}
