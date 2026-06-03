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

// ── Address validation ────────────────────────────────────────
//
// Rejects the common mistakes (missing prefix, typoed octet, garbage)
// before the form is sent to the engine so the user gets an inline
// hint instead of a server-side error toast — discussion #159 had a
// real instance where the user forgot the `/24` and only saw the
// failure after submit. The engine still validates everything via
// NetworkManager, so these helpers are belt-not-suspenders: catch the
// obvious mistakes early, defer the obscure ones to NM.

function validIpv4Octets(addr: string): boolean {
	const parts = addr.split('.');
	if (parts.length !== 4) return false;
	for (const p of parts) {
		if (!/^\d{1,3}$/.test(p)) return false;
		// Reject leading zeros (e.g. "192.168.001.1") — both ambiguous
		// (some libs treat them as octal) and a strong typo signal.
		if (p.length > 1 && p.startsWith('0')) return false;
		const n = parseInt(p, 10);
		if (n < 0 || n > 255) return false;
	}
	return true;
}

function validIpv6Body(addr: string): boolean {
	// At most one '::' compression.
	const dcolon = (addr.match(/::/g) ?? []).length;
	if (dcolon > 1) return false;
	// Reject lone ':' or trailing ':' that isn't part of '::'.
	if (addr === '') return false;
	if (addr.startsWith(':') && !addr.startsWith('::')) return false;
	if (addr.endsWith(':') && !addr.endsWith('::')) return false;

	if (dcolon === 1) {
		// "a::b" — each side is 0..7 groups, total ≤ 7 (the '::'
		// itself stands in for at least one zero group).
		const [left, right] = addr.split('::');
		const leftGroups = left === '' ? [] : left.split(':');
		const rightGroups = right === '' ? [] : right.split(':');
		if (leftGroups.length + rightGroups.length > 7) return false;
		return [...leftGroups, ...rightGroups].every(validIpv6Group);
	}
	// No compression: must be exactly 8 groups.
	const groups = addr.split(':');
	if (groups.length !== 8) return false;
	return groups.every(validIpv6Group);
}

function validIpv6Group(g: string): boolean {
	return /^[0-9a-fA-F]{1,4}$/.test(g);
}

/** Validate an IPv4 address with a CIDR prefix (e.g. "192.168.1.10/24"). */
export function validateIpv4Cidr(s: string): string | null {
	const value = s.trim();
	if (!value) return null; // empty = caller decides whether required
	const slash = value.indexOf('/');
	if (slash < 0) {
		return 'Missing CIDR prefix — try "192.168.1.10/24"';
	}
	const addr = value.slice(0, slash);
	const prefix = value.slice(slash + 1);
	if (!validIpv4Octets(addr)) {
		return 'Not a valid IPv4 address';
	}
	if (!/^\d{1,2}$/.test(prefix)) {
		return 'CIDR prefix must be a number 0-32';
	}
	const p = parseInt(prefix, 10);
	if (p < 0 || p > 32) {
		return 'CIDR prefix out of range (0-32)';
	}
	return null;
}

/** Validate an IPv6 address with a CIDR prefix (e.g. "fd00::1/64"). */
export function validateIpv6Cidr(s: string): string | null {
	const value = s.trim();
	if (!value) return null;
	const slash = value.indexOf('/');
	if (slash < 0) {
		return 'Missing CIDR prefix — try "fd00::1/64"';
	}
	const addr = value.slice(0, slash);
	const prefix = value.slice(slash + 1);
	if (!validIpv6Body(addr)) {
		return 'Not a valid IPv6 address';
	}
	if (!/^\d{1,3}$/.test(prefix)) {
		return 'CIDR prefix must be a number 0-128';
	}
	const p = parseInt(prefix, 10);
	if (p < 0 || p > 128) {
		return 'CIDR prefix out of range (0-128)';
	}
	return null;
}

/** Validate a bare IPv4 address (no CIDR). Used for gateway + DNS. */
export function validateIpv4Address(s: string): string | null {
	const value = s.trim();
	if (!value) return null;
	if (value.includes('/')) {
		return 'Plain address only — no CIDR prefix here';
	}
	if (!validIpv4Octets(value)) {
		return 'Not a valid IPv4 address';
	}
	return null;
}

/** Validate a bare IPv6 address (no CIDR). Used for v6 gateway. */
export function validateIpv6Address(s: string): string | null {
	const value = s.trim();
	if (!value) return null;
	if (value.includes('/')) {
		return 'Plain address only — no CIDR prefix here';
	}
	if (!validIpv6Body(value)) {
		return 'Not a valid IPv6 address';
	}
	return null;
}

/** Validate a DNS server entry: bare v4 or v6 address. */
export function validateDnsServer(s: string): string | null {
	const value = s.trim();
	if (!value) return null;
	// IPv6 typically contains ':', IPv4 has dots and no colons.
	if (value.includes(':')) {
		return validateIpv6Address(value);
	}
	return validateIpv4Address(value);
}

/** Validate an NFS client host entry. Mirrors the engine's
 * `validate_nfs_host` (engine/nasty-sharing/src/nfs.rs): rejects
 * whitespace, control chars, and the shell-injection-relevant
 * punctuation (`(`, `)`, `"`, `'`, `;`, `,`, `\`) that could let a
 * value escape its position in the exports file's
 * `host(opts) host(opts) ...` grammar.
 *
 * Deliberately permissive on what *kind* of host it is — IPv4
 * addresses, IPv6 addresses (with or without CIDR), hostnames,
 * `*`, `@netgroup` and the like all pass. Anything that doesn't
 * trip the injection filter is the engine's job to interpret. */
export function validateNfsHost(s: string): string | null {
	const value = s.trim();
	if (!value) return null;
	if (/[\s\x00-\x1f()"';,\\]/.test(value)) {
		return 'Contains invalid characters (whitespace, quotes, parentheses, semicolons, commas, backslashes)';
	}
	return null;
}

/** Validate a listen address picked alongside an explicit address
 * family selector — used by the NVMe-oF port form, where the operator
 * picks `ipv4` or `ipv6` from a `<select>` and then types the
 * matching address. Today the form had no cross-check, so an
 * `ipv6` family + a v4 string sent the engine a mismatched payload
 * that errored late with a configfs EINVAL. */
export function validateAddressForFamily(
	family: 'ipv4' | 'ipv6',
	s: string,
): string | null {
	const value = s.trim();
	if (!value) return null;
	if (family === 'ipv6') {
		// Accept literal v6 with or without zone id (`fe80::1%eth0`),
		// reject CIDR — the listen address is a single host.
		if (value.includes('/')) {
			return 'Listen address must be a bare address (no CIDR prefix)';
		}
		if (!validIpv6Body(value.split('%')[0])) {
			return `'${value}' is not a valid IPv6 address`;
		}
		return null;
	}
	return validateIpv4Address(value);
}

/** A single option in the listen-address picker dropdown. `key` is a
 * stable string identifier used as the `<option value=...>`, `addr` is
 * the bare address to push into the address field on selection, and
 * `family` decides which v4/v6 selector value to mirror. */
export interface ListenAddressOption {
	key: string;
	label: string;
	addr: string;
	family: 'ipv4' | 'ipv6';
}

/** Derive picker options from the live interface list. Used by the
 * shared `<ListenAddressPicker>` to populate its dropdown.
 *
 * `allowWildcards` controls whether `0.0.0.0` / `::` are offered — iSCSI
 * accepts both (configfs creates the np entries without complaint),
 * NVMe-oF rejects them with EINVAL when the subsystem-to-port symlink
 * is created, so the picker hides them on the NVMe-oF panel.
 *
 * Filters: skip link-local v6 (`fe80::/10`) since it requires a zone
 * id the picker doesn't carry; skip loopback addresses since binding
 * a network share to `127.0.0.1` or `::1` is almost always a mistake;
 * skip interfaces with `up: false` — picking an address on a down
 * interface would succeed in configfs but accept no connections. */
export function listenAddressOptions(
	interfaces: import('./types').LiveInterface[],
	allowWildcards: boolean,
): ListenAddressOption[] {
	const opts: ListenAddressOption[] = [];

	if (allowWildcards) {
		opts.push({
			key: 'wild:0.0.0.0',
			label: 'All IPv4 interfaces (0.0.0.0)',
			addr: '0.0.0.0',
			family: 'ipv4',
		});
		opts.push({
			key: 'wild:::',
			label: 'All IPv6 interfaces (::)',
			addr: '::',
			family: 'ipv6',
		});
	}

	for (const iface of interfaces) {
		if (!iface.up) continue;
		for (const cidr of iface.ipv4_addresses ?? []) {
			const bare = cidr.split('/')[0];
			if (bare === '127.0.0.1' || bare.startsWith('127.')) continue;
			opts.push({
				key: `if:${iface.name}:v4:${bare}`,
				label: `${iface.name} — ${bare}`,
				addr: bare,
				family: 'ipv4',
			});
		}
		for (const cidr of iface.ipv6_addresses ?? []) {
			const bare = cidr.split('/')[0];
			if (bare === '::1') continue;
			if (bare.toLowerCase().startsWith('fe80')) continue;
			opts.push({
				key: `if:${iface.name}:v6:${bare}`,
				label: `${iface.name} — ${bare}`,
				addr: bare,
				family: 'ipv6',
			});
		}
	}

	return opts;
}
