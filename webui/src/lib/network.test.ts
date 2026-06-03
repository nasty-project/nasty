import { describe, expect, it } from 'vitest';
import {
	findOrphanInterfaces,
	promoteOrphanedMembers,
	stripInterfaces,
	validateAddressForFamily,
	validateDnsServer,
	validateIpv4Address,
	validateIpv4Cidr,
	validateIpv6Address,
	validateIpv6Cidr,
	validateNfsHost,
} from './network';
import type {
	BondConfig,
	BridgeConfig,
	InterfaceConfig,
	LiveInterface,
	NetworkConfig,
} from './types';

function liveIface(name: string): LiveInterface {
	return {
		name,
		mac: '00:00:00:00:00:00',
		up: true,
		speed_mbps: null,
		carrier: true,
		ipv4_addresses: [],
		ipv6_addresses: [],
		mtu: 1500,
		kind: 'physical',
	};
}

function iface(name: string): InterfaceConfig {
	return {
		name,
		enabled: true,
		ipv4: { method: 'static', addresses: ['192.0.2.1/24'], gateway: null },
		ipv6: { method: 'disabled', addresses: [], gateway: null },
		mtu: null,
	};
}

function bridge(name: string, members: string[]): BridgeConfig {
	return {
		name,
		members,
		ipv4: { method: 'inherit', addresses: [], gateway: null },
		ipv6: { method: 'inherit', addresses: [], gateway: null },
		mtu: null,
	};
}

function bond(name: string, members: string[]): BondConfig {
	return {
		name,
		members,
		mode: 'lacp',
		ipv4: { method: 'dhcp', addresses: [], gateway: null },
		ipv6: { method: 'slaac', addresses: [], gateway: null },
		mtu: null,
	};
}

function emptyNet(over: Partial<NetworkConfig> = {}): NetworkConfig {
	return { interfaces: [], dns: [], bonds: [], vlans: [], bridges: [], ...over };
}

describe('promoteOrphanedMembers', () => {
	it('promotes a sole bridge member that has no standalone entry', () => {
		// The headline case: br0 has ens18 as a member, ens18 isn't in
		// interfaces[] (it was only known as a bridge port). When br0
		// is removed, ens18 must be promoted or it drops out of config
		// entirely and the engine has no profile to emit for it.
		const net = emptyNet({ bridges: [bridge('br0', ['ens18'])] });
		const result = promoteOrphanedMembers(net, { kind: 'bridge', name: 'br0' }, ['ens18']);
		expect(result).toHaveLength(1);
		expect(result[0]).toMatchObject({
			name: 'ens18',
			enabled: true,
			ipv4: { method: 'dhcp' },
			ipv6: { method: 'slaac' },
		});
	});

	it('does not duplicate a member that is already in interfaces', () => {
		// Some setups have the iface listed both as a standalone entry
		// (carrying its own L3 from a prior topology) and as a bridge
		// member. Removing the bridge mustn't add a second entry.
		const net = emptyNet({
			interfaces: [iface('ens18')],
			bridges: [bridge('br0', ['ens18'])],
		});
		const result = promoteOrphanedMembers(net, { kind: 'bridge', name: 'br0' }, ['ens18']);
		expect(result).toHaveLength(1);
		// The pre-existing entry is preserved untouched (incl. its
		// static IP — we don't overwrite user config).
		expect(result[0].ipv4.method).toBe('static');
	});

	it('does not promote a member still claimed by another bridge', () => {
		// Edge case: shared-port topology where eth0 is a member of
		// both br0 and br1 (unusual but representable in our schema).
		// Removing br0 leaves eth0 still under br1, so don't promote.
		const net = emptyNet({
			bridges: [bridge('br0', ['eth0']), bridge('br1', ['eth0'])],
		});
		const result = promoteOrphanedMembers(net, { kind: 'bridge', name: 'br0' }, ['eth0']);
		expect(result).toHaveLength(0);
	});

	it('does not promote a member still claimed by a bond', () => {
		// eth0 is in both bridge br0 and bond bond0. Removing the
		// bridge keeps it inside the bond — no promotion.
		const net = emptyNet({
			bridges: [bridge('br0', ['eth0'])],
			bonds: [bond('bond0', ['eth0'])],
		});
		const result = promoteOrphanedMembers(net, { kind: 'bridge', name: 'br0' }, ['eth0']);
		expect(result).toHaveLength(0);
	});

	it('ignores the master being removed when scanning for other claims', () => {
		// The function is given the master's identity so it can ignore
		// it when checking "is this iface still mastered elsewhere".
		// Without this filter, the function would see eth0 still
		// listed under br0 and skip the promotion.
		const net = emptyNet({ bridges: [bridge('br0', ['eth0'])] });
		const result = promoteOrphanedMembers(net, { kind: 'bridge', name: 'br0' }, ['eth0']);
		expect(result.map((i) => i.name)).toEqual(['eth0']);
	});

	it('promotes multiple orphans from one master', () => {
		const net = emptyNet({ bonds: [bond('bond0', ['eth0', 'eth1', 'eth2'])] });
		const result = promoteOrphanedMembers(
			net,
			{ kind: 'bond', name: 'bond0' },
			['eth0', 'eth1', 'eth2'],
		);
		expect(result.map((i) => i.name)).toEqual(['eth0', 'eth1', 'eth2']);
		// All get the same DHCP defaults — we don't pick a "primary".
		expect(result.every((i) => i.ipv4.method === 'dhcp')).toBe(true);
	});

	it('handles bond removal symmetrically to bridge removal', () => {
		const net = emptyNet({ bonds: [bond('bond0', ['eth0'])] });
		const result = promoteOrphanedMembers(net, { kind: 'bond', name: 'bond0' }, ['eth0']);
		expect(result.map((i) => i.name)).toEqual(['eth0']);
	});
});

describe('findOrphanInterfaces', () => {
	it('flags an interface entry that is neither live nor a configured master', () => {
		// The headline case from issue #96: bond0 is in interfaces[]
		// (likely from a past manual edit) but doesn't exist as a live
		// device and isn't in bonds[]. The engine's validator now
		// trips when the user tries to recreate the bond because the
		// layered model would have two Links named bond0; the WebUI
		// surfaces the orphan first so the user can clean it up.
		const net = emptyNet({
			interfaces: [iface('enp4s0'), iface('bond0'), iface('enp6s0f1')],
		});
		const live = [liveIface('enp4s0'), liveIface('enp6s0f1')];
		expect(findOrphanInterfaces(net, live)).toEqual(['bond0']);
	});

	it('does not flag a freshly-added bond before it has been applied', () => {
		// User added bond0 to bonds[] but hasn't clicked Apply yet —
		// it's not in the live list either. We must not falsely
		// surface it as orphan; the bonds[] entry covers it.
		const net = emptyNet({
			interfaces: [iface('eth0')],
			bonds: [bond('bond0', ['eth0'])],
		});
		const live = [liveIface('eth0')]; // bond0 not yet a real device
		expect(findOrphanInterfaces(net, live)).toEqual([]);
	});

	it('does not flag a bridge or vlan name held in interfaces[]', () => {
		// Symmetric coverage with bridges + vlans — same logic, no
		// regression for users who manage bridges or vlans.
		const net = emptyNet({
			interfaces: [iface('br0'), iface('eth0.100')],
			bridges: [
				{
					name: 'br0',
					members: [],
					ipv4: { method: 'inherit', addresses: [], gateway: null },
					ipv6: { method: 'inherit', addresses: [], gateway: null },
					mtu: null,
				},
			],
			vlans: [
				{
					parent: 'eth0',
					vlan_id: 100,
					ipv4: { method: 'dhcp', addresses: [], gateway: null },
					ipv6: { method: 'slaac', addresses: [], gateway: null },
					mtu: null,
				},
			],
		});
		expect(findOrphanInterfaces(net, [])).toEqual([]);
	});

	it('does not flag a NIC that is down but still present in sysfs', () => {
		// Disconnected/disabled NICs still show up in
		// `enumerate_interfaces` (sysfs is the source); their
		// `up`/`carrier` may be false but the name is still in the
		// live list. Don't strip user config for those — that would
		// destroy DHCP/static settings on temporarily-unplugged
		// devices.
		const net = emptyNet({ interfaces: [iface('eth0')] });
		const down = { ...liveIface('eth0'), up: false, carrier: false };
		expect(findOrphanInterfaces(net, [down])).toEqual([]);
	});

	it('returns multiple orphans in interfaces[] order', () => {
		// Ordering matters for the banner — show orphans in the same
		// order they appear in the user's config so the message is
		// stable across renders.
		const net = emptyNet({
			interfaces: [iface('zoot'), iface('eth0'), iface('apple')],
		});
		const live = [liveIface('eth0')];
		expect(findOrphanInterfaces(net, live)).toEqual(['zoot', 'apple']);
	});
});

describe('stripInterfaces', () => {
	it('removes only the named entries, preserving order', () => {
		const net = emptyNet({
			interfaces: [iface('eth0'), iface('bond0'), iface('eth1')],
		});
		const result = stripInterfaces(net, ['bond0']);
		expect(result.map((i) => i.name)).toEqual(['eth0', 'eth1']);
	});

	it('is a no-op when the names list is empty', () => {
		const net = emptyNet({ interfaces: [iface('eth0')] });
		expect(stripInterfaces(net, [])).toEqual(net.interfaces);
	});

	it('skips names that are not present', () => {
		// Tolerant: a stale ref shouldn't make us throw.
		const net = emptyNet({ interfaces: [iface('eth0')] });
		expect(stripInterfaces(net, ['ghost']).map((i) => i.name)).toEqual(['eth0']);
	});
});

describe('validateIpv4Cidr', () => {
	it('accepts canonical addresses with valid prefixes', () => {
		expect(validateIpv4Cidr('192.168.1.10/24')).toBeNull();
		expect(validateIpv4Cidr('10.0.0.1/8')).toBeNull();
		expect(validateIpv4Cidr('0.0.0.0/0')).toBeNull();
		expect(validateIpv4Cidr('255.255.255.255/32')).toBeNull();
	});

	it('treats empty string as null (caller decides if required)', () => {
		// The form has multiple optional address rows; an empty one is
		// fine — it just gets filtered out before submit.
		expect(validateIpv4Cidr('')).toBeNull();
		expect(validateIpv4Cidr('   ')).toBeNull();
	});

	it('flags missing CIDR prefix — the discussion #159 trigger case', () => {
		// The exact mistake HuxyUK reported: pasted an IP, forgot the
		// netmask, got a server-side error. The hint mentions the
		// suggested form so the fix is obvious.
		const err = validateIpv4Cidr('192.168.1.10');
		expect(err).toMatch(/CIDR prefix/);
		expect(err).toContain('/24');
	});

	it('rejects out-of-range octets', () => {
		expect(validateIpv4Cidr('256.0.0.1/24')).toBeTruthy();
		expect(validateIpv4Cidr('192.168.300.1/24')).toBeTruthy();
	});

	it('rejects out-of-range CIDR prefixes', () => {
		expect(validateIpv4Cidr('192.168.1.10/33')).toMatch(/0-32/);
		expect(validateIpv4Cidr('192.168.1.10/-1')).toBeTruthy();
	});

	it('rejects leading-zero octets (often-typoed, ambiguous)', () => {
		// Some libraries treat "001" as octal; rejecting it both
		// avoids the ambiguity and catches a common typo.
		expect(validateIpv4Cidr('192.168.001.1/24')).toBeTruthy();
	});

	it('rejects garbage and partial addresses', () => {
		expect(validateIpv4Cidr('not.an.ip.address/24')).toBeTruthy();
		expect(validateIpv4Cidr('192.168.1/24')).toBeTruthy();
		expect(validateIpv4Cidr('192.168.1.1.1/24')).toBeTruthy();
		expect(validateIpv4Cidr('/24')).toBeTruthy();
	});
});

describe('validateIpv6Cidr', () => {
	it('accepts canonical addresses with valid prefixes', () => {
		expect(validateIpv6Cidr('fd00::1/64')).toBeNull();
		expect(validateIpv6Cidr('2001:db8::/32')).toBeNull();
		expect(validateIpv6Cidr('::1/128')).toBeNull();
		expect(validateIpv6Cidr('::/0')).toBeNull();
		expect(validateIpv6Cidr(
			'2001:0db8:85a3:0000:0000:8a2e:0370:7334/64',
		)).toBeNull();
	});

	it('flags missing CIDR prefix', () => {
		const err = validateIpv6Cidr('fd00::1');
		expect(err).toMatch(/CIDR prefix/);
		expect(err).toContain('/64');
	});

	it('rejects multiple :: compression markers', () => {
		expect(validateIpv6Cidr('fd00::1::2/64')).toBeTruthy();
	});

	it('rejects out-of-range CIDR prefixes', () => {
		expect(validateIpv6Cidr('fd00::1/129')).toMatch(/0-128/);
	});

	it('rejects garbage groups', () => {
		expect(validateIpv6Cidr('xyzz::1/64')).toBeTruthy();
		expect(validateIpv6Cidr('fd00:::1/64')).toBeTruthy();
		expect(validateIpv6Cidr('fd00:1:2:3:4:5:6:7:8/64')).toBeTruthy();
	});
});

describe('validateIpv4Address (no CIDR — gateway + DNS)', () => {
	it('accepts canonical addresses', () => {
		expect(validateIpv4Address('192.168.1.1')).toBeNull();
		expect(validateIpv4Address('1.1.1.1')).toBeNull();
	});

	it('rejects a CIDR suffix — gateways do not take prefixes', () => {
		// Easy mistake: copy/paste the address row into the gateway
		// field. We tell the user exactly what's wrong.
		expect(validateIpv4Address('192.168.1.1/24')).toMatch(/no CIDR/);
	});

	it('rejects bad addresses', () => {
		expect(validateIpv4Address('not-an-ip')).toBeTruthy();
		expect(validateIpv4Address('192.168.1')).toBeTruthy();
		expect(validateIpv4Address('256.1.1.1')).toBeTruthy();
	});
});

describe('validateIpv6Address (no CIDR — v6 gateway)', () => {
	it('accepts canonical addresses', () => {
		expect(validateIpv6Address('fe80::1')).toBeNull();
		expect(validateIpv6Address('::1')).toBeNull();
		expect(validateIpv6Address('2001:db8::1')).toBeNull();
	});

	it('rejects a CIDR suffix', () => {
		expect(validateIpv6Address('fe80::1/64')).toMatch(/no CIDR/);
	});

	it('rejects garbage', () => {
		expect(validateIpv6Address('zzzz::1')).toBeTruthy();
	});
});

describe('validateDnsServer', () => {
	it('accepts both v4 and v6 (no CIDR)', () => {
		expect(validateDnsServer('1.1.1.1')).toBeNull();
		expect(validateDnsServer('2606:4700:4700::1111')).toBeNull();
	});

	it('routes by colon presence — v6 if present, v4 otherwise', () => {
		// Strings with a colon are always treated as v6 attempts so the
		// error message matches what the user wrote.
		expect(validateDnsServer('not:a:v6')).toMatch(/IPv6/);
		expect(validateDnsServer('not.a.v4')).toMatch(/IPv4/);
	});
});

describe('validateNfsHost', () => {
	it('accepts the host forms the engine accepts', () => {
		// Same set the Rust validate_nfs_host unit test covers, ported
		// here so a future divergence between client and server
		// surfaces in CI rather than as a confusing runtime error.
		expect(validateNfsHost('192.168.1.5')).toBeNull();
		expect(validateNfsHost('192.168.1.0/24')).toBeNull();
		expect(validateNfsHost('fd00::1/64')).toBeNull();
		expect(validateNfsHost('client.example.com')).toBeNull();
		expect(validateNfsHost('*')).toBeNull();
		expect(validateNfsHost('@netgroup')).toBeNull();
	});

	it('passes empty input as null (let the required-field check handle it)', () => {
		expect(validateNfsHost('')).toBeNull();
		expect(validateNfsHost('   ')).toBeNull();
	});

	it('rejects shell-injection-relevant punctuation', () => {
		// Match the engine: whitespace, control chars, parens, quotes,
		// semicolons, commas, backslashes all forbidden. The exports
		// file grammar is positional and these characters would let a
		// value escape into a fresh export entry.
		expect(validateNfsHost('host with space')).toBeTruthy();
		expect(validateNfsHost('host\nnewline')).toBeTruthy();
		expect(validateNfsHost('host(opts)')).toBeTruthy();
		expect(validateNfsHost('host;evil')).toBeTruthy();
		expect(validateNfsHost('host"quoted')).toBeTruthy();
		expect(validateNfsHost('host,other')).toBeTruthy();
	});
});

describe('validateAddressForFamily', () => {
	it('accepts v4 strings under ipv4 family', () => {
		expect(validateAddressForFamily('ipv4', '192.168.1.10')).toBeNull();
		expect(validateAddressForFamily('ipv4', '10.0.0.1')).toBeNull();
	});

	it('accepts v6 strings under ipv6 family', () => {
		expect(validateAddressForFamily('ipv6', 'fd00::1')).toBeNull();
		expect(validateAddressForFamily('ipv6', '2001:db8::1')).toBeNull();
		expect(validateAddressForFamily('ipv6', '::1')).toBeNull();
	});

	it('catches v6 string typed under ipv4 family', () => {
		// Operator picked IPv4 in the dropdown but typed an IPv6
		// address. Before this preflight, the engine would reject
		// it late with a configfs EINVAL after submission.
		expect(validateAddressForFamily('ipv4', 'fd00::1')).toBeTruthy();
	});

	it('catches v4 string typed under ipv6 family', () => {
		expect(validateAddressForFamily('ipv6', '192.168.1.10')).toBeTruthy();
	});

	it('rejects a CIDR suffix on a listen address', () => {
		// portal listen address is a single host — CIDR doesn't make
		// sense here even though it does for client ACLs.
		expect(validateAddressForFamily('ipv6', 'fd00::/64')).toMatch(/CIDR/);
	});

	it('accepts a v6 zone id (link-local interface scope)', () => {
		expect(validateAddressForFamily('ipv6', 'fe80::1%eth0')).toBeNull();
	});

	it('passes empty input as null', () => {
		expect(validateAddressForFamily('ipv4', '')).toBeNull();
		expect(validateAddressForFamily('ipv6', '')).toBeNull();
	});
});
