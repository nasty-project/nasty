<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { applyNetworkUpdate } from '$lib/rollbackState.svelte';
	import { tempUnit } from '$lib/temperature.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import {
		promoteOrphanedMembers,
		validateDnsServer,
		validateIpv4Address,
		validateIpv4Cidr,
		validateIpv6Address,
		validateIpv6Cidr,
	} from '$lib/network';
	import { confirm } from '$lib/confirm.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';
	import type { Settings, SystemInfo, NetworkState, NetworkConfig, LiveInterface, TuningConfig, NetIfStats, IpConfig, InterfaceConfig } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import BridgeCreator from '$lib/components/BridgeCreator.svelte';
	import { Copy, Check, ChevronDown, ChevronRight } from '@lucide/svelte';

	let activeTab: 'general' | 'network' | 'notifications' | 'metrics' | 'tuning' = $state('general');

	// Notifications tab
	import type { NotificationConfig, NotificationChannel } from '$lib/types';
	let notifConfig: NotificationConfig = $state({ channels: [] });
	let notifLoaded = $state(false);
	let notifSaving = $state(false);
	let notifTesting = $state<string | null>(null);
	let notifAddType: 'smtp' | 'telegram' | 'webhook' | 'ntfy' | 'signal' | null = $state(null);
	let notifEditId: string | null = $state(null);
	// Form fields
	let nfName = $state('');
	let nfHost = $state(''); let nfPort = $state(587); let nfUser = $state(''); let nfPass = $state('');
	let nfFrom = $state(''); let nfTo = $state('');
	let nfBotToken = $state(''); let nfChatId = $state('');
	let nfUrl = $state('');
	let nfNtfyServer = $state('https://ntfy.sh'); let nfNtfyTopic = $state(''); let nfNtfyToken = $state('');
	let nfSignalUrl = $state('http://localhost:8080'); let nfSignalFrom = $state(''); let nfSignalTo = $state('');

	// Network tab
	let netInterfaces: NetIfStats[] = $state([]);
	let netIfLoaded = $state(false);
	let selectedIface: string | null = $state(null);
	// Multiple IPv4/IPv6 addresses
	let netIpv4Addrs: string[] = $state(['']);
	let netIpv6Addrs: string[] = $state(['']);
	// IPv6 form
	let netIpv6Method = $state<'slaac' | 'static' | 'dhcp' | 'disabled'>('slaac');
	let netIpv6Gateway = $state('');
	// MTU on inline interface form (string so an empty value means "unset")
	let netMtu = $state('');
	// Bond form
	let showBondForm = $state(false);
	let bondName = $state('bond0');
	let bondMembers: string[] = $state([]);
	let bondMode: 'lacp' | 'active_backup' | 'balance_rr' | 'balance_xor' = $state('active_backup');
	let bondMtu = $state('');
	// Inverted in the UI ("Don't inherit member MAC"). Default OFF
	// (i.e. inherit_member_mac=true) so bonds get DHCP-stable
	// identity by default — same logic as bridges below.
	let bondNoInheritMac = $state(false);
	// VLAN form
	let showVlanForm = $state(false);
	let vlanParent = $state('');
	let vlanTried = $state(false);
	let vlanId = $state(100);
	let vlanMtu = $state('');
	// Bridge form
	let showBridgeForm = $state(false);
	// ── General tab state ───────────────────────────────────
	let settings: Settings | null = $state(null);
	let info: SystemInfo | null = $state(null);
	let timezones: string[] = $state([]);
	let saving = $state(false);
	let savingHostname = $state(false);
	let hostnameInput = $state('');

	// Network
	let networkState: NetworkState | null = $state(null);
	const network = $derived.by((): NetworkConfig | null => {
		return networkState?.config ?? null;
	});
	let savingNetwork = $state(false);
	let netDhcp = $state(true);
	let netGateway = $state('');
	let netNameservers = $state('');
	let netChanged = $state(false);

	// Per-input validation. Each entry is null when the field is OK
	// (empty or correctly formed) or a short, fixable error message
	// when not. Computed reactively so the inline hints update as the
	// user types. The Save button stays disabled while any field is in
	// error — mirrors the engine-side validation NM does, but locally
	// so the user gets a hint *before* submit (discussion #159).
	const netIpv4AddrErrors = $derived(netIpv4Addrs.map(validateIpv4Cidr));
	const netGatewayError = $derived(netDhcp ? null : validateIpv4Address(netGateway));
	const netIpv6AddrErrors = $derived(netIpv6Addrs.map(validateIpv6Cidr));
	const netIpv6GatewayError = $derived(
		netIpv6Method === 'static' ? validateIpv6Address(netIpv6Gateway) : null,
	);
	const netNameserversErrors = $derived(
		netNameservers
			.split(/[,\s]+/)
			.map((s) => s.trim())
			.filter(Boolean)
			.map((s) => ({ value: s, err: validateDnsServer(s) }))
			.filter((entry) => entry.err !== null),
	);
	const netHasErrors = $derived(
		netIpv4AddrErrors.some((e) => e !== null) ||
			netGatewayError !== null ||
			netIpv6AddrErrors.some((e) => e !== null) ||
			netIpv6GatewayError !== null ||
			netNameserversErrors.length > 0,
	);

	// Log level
	let logFilter = $state('');
	let savingLog = $state(false);
	const logPresets = [
		{ label: 'Normal', value: 'nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info' },
		{ label: 'Debug', value: 'nasty_engine=debug,nasty_storage=debug,nasty_sharing=debug,nasty_snapshot=debug,nasty_system=debug,tower_http=debug' },
		{ label: 'Trace', value: 'nasty_engine=trace,nasty_storage=trace,nasty_sharing=trace,nasty_snapshot=trace,nasty_system=trace,tower_http=trace' },
	];

	// Tuning
	let tuning: TuningConfig | null = $state(null);
	let savingTuning = $state(false);
	let tNfsThreads = $state('');
	let tNfsLeaseTime = $state('');
	let tNfsGraceTime = $state('');
	let tSmbMaxConnections = $state('');
	let tSmbDeadtime = $state('');
	let tSmbSocketOptions = $state('');
	let tIscsiCmdsnDepth = $state('');
	let tIscsiLoginTimeout = $state('');
	let tVmDirtyRatio = $state('');
	let tVmDirtyBgRatio = $state('');
	let tVmDirtyExpire = $state('');
	let tVmDirtyWriteback = $state('');


	// Telemetry
	let sendingTelemetry = $state(false);

	// ── Metrics tab state ───────────────────────────────────
	let metricsText = $state('');
	let metricsLoading = $state(false);
	let metricsCopied = $state(false);
	let collapsedSections: Record<string, boolean> = $state({});

	interface MetricsSection {
		title: string;
		lines: string[];
	}

	const metricsSections = $derived.by((): MetricsSection[] => {
		if (!metricsText) return [];

		const sections: MetricsSection[] = [];
		let currentTitle = 'General';
		let currentLines: string[] = [];

		for (const line of metricsText.split('\n')) {
			if (line.startsWith('# HELP ')) {
				const metricName = line.slice(7).split(' ')[0];
				let title: string;
				if (metricName.startsWith('nasty_bcachefs_device_')) {
					title = 'bcachefs — Devices';
				} else if (metricName.startsWith('nasty_bcachefs_time_stat_')) {
					title = 'bcachefs — Time Stats';
				} else if (metricName.startsWith('nasty_bcachefs_counter')) {
					title = 'bcachefs — Counters';
				} else if (metricName.startsWith('nasty_bcachefs_')) {
					title = 'bcachefs — Filesystem';
				} else if (metricName.startsWith('nasty_disk_smart_') || metricName.startsWith('nasty_disk_temperature') || metricName.startsWith('nasty_disk_power_on')) {
					title = 'Disk Health (SMART)';
				} else if (metricName.startsWith('nasty_disk_')) {
					title = 'Disk I/O';
				} else if (metricName.startsWith('nasty_net_')) {
					title = 'Network';
				} else if (metricName.startsWith('nasty_cpu_') || metricName.startsWith('nasty_memory_') || metricName.startsWith('nasty_swap_')) {
					title = 'System';
				} else {
					title = 'Other';
				}

				if (title !== currentTitle && currentLines.length > 0) {
					sections.push({ title: currentTitle, lines: currentLines });
					currentLines = [];
				}
				currentTitle = title;
			}
			if (line.trim()) {
				currentLines.push(line);
			}
		}
		if (currentLines.length > 0) {
			sections.push({ title: currentTitle, lines: currentLines });
		}

		return sections;
	});

	const client = getClient();

	onMount(async () => {
		await withToast(async () => {
			[settings, info, timezones, networkState] = await Promise.all([
				client.call<Settings>('system.settings.get'),
				client.call<SystemInfo>('system.info'),
				client.call<string[]>('system.settings.timezones'),
				client.call<NetworkState>('system.network.get'),
			]);
			hostnameInput = settings.hostname ?? info.hostname;
			syncNetworkForm();
		});
	});

	function syncNetworkForm() {
		if (!network || !network.interfaces.length) return;
		const iface = network.interfaces[0];
		netDhcp = iface.ipv4.method === 'dhcp';
		netGateway = iface.ipv4.gateway ?? '';
		netNameservers = network.dns.join(', ');
		netChanged = false;
	}

	async function saveHostname() {
		savingHostname = true;
		await withToast(
			() => client.call('system.settings.update', { hostname: hostnameInput }),
			'Hostname updated'
		);
		info = await client.call<SystemInfo>('system.info');
		sysInfoRefresh.trigger();
		savingHostname = false;
	}

	async function saveTimezone() {
		if (!settings) return;
		saving = true;
		await withToast(
			() => client.call('system.settings.update', { timezone: settings!.timezone }),
			'Timezone updated'
		);
		info = await client.call<SystemInfo>('system.info');
		saving = false;
	}

	async function saveClock24h(val: boolean) {
		if (!settings) return;
		settings.clock_24h = val;
		await withToast(
			() => client.call('system.settings.update', { clock_24h: val }),
			val ? '24-hour clock enabled' : '12-hour clock enabled'
		);
	}

	async function saveTempUnit(val: 'celsius' | 'fahrenheit') {
		if (!settings) return;
		settings.temp_unit = val;
		tempUnit.set(val);
		await withToast(
			() => client.call('system.settings.update', { temp_unit: val }),
			val === 'fahrenheit' ? 'Temperature unit: Fahrenheit' : 'Temperature unit: Celsius'
		);
	}

	async function saveTelemetry(enabled: boolean) {
		if (!settings) return;
		settings.telemetry_enabled = enabled;
		await withToast(
			() => client.call('system.settings.update', { telemetry_enabled: enabled }),
			enabled ? 'Telemetry enabled' : 'Telemetry disabled'
		);
	}

	async function sendTelemetry() {
		sendingTelemetry = true;
		await withToast(
			() => client.call<{ sent: boolean }>('telemetry.send'),
			'Telemetry report sent'
		);
		sendingTelemetry = false;
	}

	async function applyLogLevel() {
		if (!logFilter.trim()) return;
		savingLog = true;
		await withToast(
			() => client.call('system.log.set_level', { filter: logFilter }),
			'Log level updated'
		);
		savingLog = false;
	}

	async function loadMetrics() {
		metricsLoading = true;
		try {
			metricsText = await client.call<string>('system.metrics.prometheus');
		} catch {
			metricsText = '';
		}
		metricsLoading = false;
	}

	async function copyMetrics() {
		await navigator.clipboard.writeText(metricsText);
		metricsCopied = true;
		setTimeout(() => { metricsCopied = false; }, 2000);
	}

	function toggleSection(title: string) {
		collapsedSections[title] = !collapsedSections[title];
	}

	async function loadNetInterfaces() {
		try {
			const stats = await client.call<{ network: NetIfStats[] }>('system.stats');
			netInterfaces = stats.network.filter(iface => iface.name !== 'lo');
			netIfLoaded = true;
		} catch { /* ignore */ }
	}

	function switchTab(tab: typeof activeTab) {
		activeTab = tab;
		if (tab === 'network') {
			if (!netIfLoaded) loadNetInterfaces();
		}
		if (tab === 'notifications' && !notifLoaded) {
			loadNotifications();
		}
		if (tab === 'metrics' && !metricsText) {
			loadMetrics();
		}
		if (tab === 'tuning' && !tuning) {
			loadTuning();
		}
	}

	function liveKind(name: string): string {
		return networkState?.interfaces.find(i => i.name === name)?.kind ?? 'physical';
	}

	function selectInterface(name: string) {
		selectedIface = selectedIface === name ? null : name;
		if (!selectedIface || !network) return;

		const kind = liveKind(name);
		let cfg: { ipv4: IpConfig; ipv6: IpConfig; mtu: number | null } | undefined;
		if (kind === 'bond') {
			cfg = network.bonds?.find(b => b.name === name);
		} else if (kind === 'bridge') {
			cfg = network.bridges?.find(b => b.name === name);
		} else if (kind === 'vlan') {
			cfg = network.vlans?.find(v => `${v.parent}.${v.vlan_id}` === name);
		} else {
			cfg = network.interfaces?.find(i => i.name === name);
		}

		if (cfg) {
			netDhcp = cfg.ipv4.method === 'dhcp';
			netIpv4Addrs = cfg.ipv4.addresses.length > 0 ? [...cfg.ipv4.addresses] : [''];
			netGateway = cfg.ipv4.gateway ?? '';
			netIpv6Method = cfg.ipv6.method as typeof netIpv6Method;
			netIpv6Addrs = cfg.ipv6.addresses.length > 0 ? [...cfg.ipv6.addresses] : [''];
			netIpv6Gateway = cfg.ipv6.gateway ?? '';
			netMtu = cfg.mtu != null ? String(cfg.mtu) : '';
		} else {
			netDhcp = true; netIpv4Addrs = ['']; netGateway = '';
			netIpv6Method = 'slaac'; netIpv6Addrs = ['']; netIpv6Gateway = '';
			netMtu = '';
		}
		netChanged = false;
	}

	// Svelte coerces bind:value on <input type="number"> to a number (or '' / null
	// when the field is empty), so accept anything and normalise.
	function parseMtu(v: unknown): number | null {
		if (v === null || v === undefined || v === '') return null;
		const n = typeof v === 'number' ? v : parseInt(String(v), 10);
		return Number.isFinite(n) && n > 0 ? n : null;
	}

	async function saveInterfaceConfig() {
		if (!selectedIface || !network) return;
		savingNetwork = true;
		const nameservers = netNameservers.split(/[,\s]+/).map(s => s.trim()).filter(Boolean);
		const ipv4: IpConfig = {
			method: netDhcp ? 'dhcp' : 'static',
			addresses: netDhcp ? [] : netIpv4Addrs.filter(a => a.trim()),
			gateway: netDhcp ? null : (netGateway.trim() || null),
		};
		const ipv6: IpConfig = {
			method: netIpv6Method,
			addresses: netIpv6Method === 'static' ? netIpv6Addrs.filter(a => a.trim()) : [],
			gateway: netIpv6Method === 'static' ? (netIpv6Gateway || null) : null,
		};
		const mtu = parseMtu(netMtu);
		const kind = liveKind(selectedIface);

		const payload: NetworkConfig = {
			interfaces: [...(network.interfaces || [])],
			dns: nameservers,
			bonds: [...(network.bonds || [])],
			vlans: [...(network.vlans || [])],
			bridges: [...(network.bridges || [])],
		};

		if (kind === 'bond') {
			const idx = payload.bonds.findIndex(b => b.name === selectedIface);
			if (idx >= 0) payload.bonds[idx] = { ...payload.bonds[idx], ipv4, ipv6, mtu };
		} else if (kind === 'bridge') {
			const idx = payload.bridges.findIndex(b => b.name === selectedIface);
			if (idx >= 0) payload.bridges[idx] = { ...payload.bridges[idx], ipv4, ipv6, mtu };
		} else if (kind === 'vlan') {
			const idx = payload.vlans.findIndex(v => `${v.parent}.${v.vlan_id}` === selectedIface);
			if (idx >= 0) payload.vlans[idx] = { ...payload.vlans[idx], ipv4, ipv6, mtu };
		} else {
			const idx = payload.interfaces.findIndex(i => i.name === selectedIface);
			const entry: InterfaceConfig = { name: selectedIface, enabled: true, ipv4, ipv6, mtu };
			if (idx >= 0) payload.interfaces[idx] = entry; else payload.interfaces.push(entry);
		}

		await applyNetworkUpdate(payload, 'Network configuration applied');
		networkState = await client.call<NetworkState>('system.network.get');
		netChanged = false;
		savingNetwork = false;
	}

	async function createBond() {
		if (!bondName || bondMembers.length < 2 || !network) return;
		const mtu = parseMtu(bondMtu);
		const payload: NetworkConfig = {
			interfaces: network.interfaces || [],
			dns: network.dns || [],
			bonds: [...(network.bonds || []), {
				name: bondName,
				members: bondMembers,
				mode: bondMode,
				ipv4: { method: 'dhcp', addresses: [], gateway: null },
				ipv6: { method: 'slaac', addresses: [], gateway: null },
				mtu,
				// Checkbox is "Don't inherit member MAC" → invert.
				inherit_member_mac: !bondNoInheritMac,
			}],
			vlans: network.vlans || [],
			bridges: network.bridges || [],
		};
		await applyNetworkUpdate(payload, `Bond ${bondName} created`);
		networkState = await client.call<NetworkState>('system.network.get');
		showBondForm = false; bondName = 'bond0'; bondMembers = []; bondMtu = ''; bondNoInheritMac = false;
	}

	async function createVlan() {
		if (!vlanParent) { vlanTried = true; return; }
		if (vlanId < 1 || vlanId > 4094 || !network) return;
		vlanTried = false;
		const mtu = parseMtu(vlanMtu);
		const payload: NetworkConfig = {
			interfaces: network.interfaces || [],
			dns: network.dns || [],
			bonds: network.bonds || [],
			vlans: [...(network.vlans || []), { parent: vlanParent, vlan_id: vlanId, ipv4: { method: 'dhcp', addresses: [], gateway: null }, ipv6: { method: 'slaac', addresses: [], gateway: null }, mtu }],
			bridges: network.bridges || [],
		};
		await applyNetworkUpdate(payload, `VLAN ${vlanParent}.${vlanId} created`);
		networkState = await client.call<NetworkState>('system.network.get');
		showVlanForm = false; vlanParent = ''; vlanId = 100; vlanMtu = ''; vlanTried = false;
	}

	async function refreshNetworkState() {
		networkState = await client.call<NetworkState>('system.network.get');
	}

	async function deleteBond(name: string) {
		if (!network) return;
		if (!await confirm(`Remove bond ${name}?`, 'Members will return to standalone interfaces.', { confirmLabel: 'Remove' })) return;
		const removed = (network.bonds || []).find(b => b.name === name);
		const payload: NetworkConfig = {
			interfaces: removed
				? promoteOrphanedMembers(network, { kind: 'bond', name }, removed.members)
				: network.interfaces || [],
			dns: network.dns || [],
			bonds: (network.bonds || []).filter(b => b.name !== name),
			vlans: network.vlans || [],
			bridges: network.bridges || [],
		};
		await applyNetworkUpdate(payload, `Bond ${name} removed`);
		networkState = await client.call<NetworkState>('system.network.get');
	}

	async function deleteBridge(name: string) {
		if (!network) return;
		if (!await confirm(`Remove bridge ${name}?`, 'Members will return to standalone interfaces.', { confirmLabel: 'Remove' })) return;
		const removed = (network.bridges || []).find(b => b.name === name);
		const payload: NetworkConfig = {
			interfaces: removed
				? promoteOrphanedMembers(network, { kind: 'bridge', name }, removed.members)
				: network.interfaces || [],
			dns: network.dns || [],
			bonds: network.bonds || [],
			vlans: network.vlans || [],
			bridges: (network.bridges || []).filter(b => b.name !== name),
		};
		await applyNetworkUpdate(payload, `Bridge ${name} removed`);
		networkState = await client.call<NetworkState>('system.network.get');
	}

	async function deleteVlan(parent: string, vlan_id: number) {
		if (!network) return;
		if (!await confirm(`Remove VLAN ${parent}.${vlan_id}?`, undefined, { confirmLabel: 'Remove' })) return;
		const payload: NetworkConfig = {
			interfaces: network.interfaces || [],
			dns: network.dns || [],
			bonds: network.bonds || [],
			vlans: (network.vlans || []).filter(v => !(v.parent === parent && v.vlan_id === vlan_id)),
			bridges: network.bridges || [],
		};
		await applyNetworkUpdate(payload, `VLAN ${parent}.${vlan_id} removed`);
		networkState = await client.call<NetworkState>('system.network.get');
	}

	async function loadNotifications() {
		try {
			notifConfig = await client.call<NotificationConfig>('notifications.config.get');
			notifLoaded = true;
		} catch { /* ignore */ }
	}

	async function saveNotifications() {
		notifSaving = true;
		await withToast(
			() => client.call('notifications.config.update', notifConfig),
			'Notification settings saved'
		);
		notifSaving = false;
	}

	async function testChannel(ch: NotificationChannel) {
		notifTesting = ch.id;
		const payload: Record<string, unknown> = { type: ch.type };
		if (ch.type === 'smtp') Object.assign(payload, { host: ch.host, port: ch.port, username: ch.username, password: ch.password, from: ch.from, to: ch.to });
		else if (ch.type === 'telegram') Object.assign(payload, { bot_token: ch.bot_token, chat_id: ch.chat_id });
		else if (ch.type === 'webhook') Object.assign(payload, { url: ch.url, headers: ch.headers || {} });
		else if (ch.type === 'ntfy') Object.assign(payload, { server_url: ch.server_url, topic: ch.topic, token: ch.token });
		else if (ch.type === 'signal') Object.assign(payload, { api_url: ch.api_url, from_number: ch.from_number, to_number: ch.to_number });
		await withToast(
			() => client.call('notifications.test', payload),
			'Test notification sent'
		);
		notifTesting = null;
	}

	async function testNewChannel() {
		if (!notifAddType) return;
		notifTesting = '_new';
		const payload: Record<string, unknown> = { type: notifAddType };
		if (notifAddType === 'smtp') Object.assign(payload, { host: nfHost, port: nfPort, username: nfUser, password: nfPass, from: nfFrom, to: nfTo });
		else if (notifAddType === 'telegram') Object.assign(payload, { bot_token: nfBotToken, chat_id: nfChatId });
		else if (notifAddType === 'webhook') Object.assign(payload, { url: nfUrl, headers: {} });
		else if (notifAddType === 'ntfy') Object.assign(payload, { server_url: nfNtfyServer, topic: nfNtfyTopic, token: nfNtfyToken || undefined });
		else if (notifAddType === 'signal') Object.assign(payload, { api_url: nfSignalUrl, from_number: nfSignalFrom, to_number: nfSignalTo });
		await withToast(
			() => client.call('notifications.test', payload),
			'Test notification sent'
		);
		notifTesting = null;
	}

	function addChannel() {
		if (!notifAddType || !nfName) return;
		const id = crypto.randomUUID().slice(0, 8);
		const ch: NotificationChannel = { id, name: nfName, enabled: true, type: notifAddType };
		if (notifAddType === 'smtp') Object.assign(ch, { host: nfHost, port: nfPort, username: nfUser, password: nfPass, from: nfFrom, to: nfTo });
		else if (notifAddType === 'telegram') Object.assign(ch, { bot_token: nfBotToken, chat_id: nfChatId });
		else if (notifAddType === 'webhook') Object.assign(ch, { url: nfUrl, headers: {} });
		else if (notifAddType === 'ntfy') Object.assign(ch, { server_url: nfNtfyServer, topic: nfNtfyTopic, token: nfNtfyToken || undefined });
		else if (notifAddType === 'signal') Object.assign(ch, { api_url: nfSignalUrl, from_number: nfSignalFrom, to_number: nfSignalTo });
		notifConfig.channels = [...notifConfig.channels, ch];
		resetNotifForm();
		saveNotifications();
	}

	function removeChannel(id: string) {
		notifConfig.channels = notifConfig.channels.filter(c => c.id !== id);
		saveNotifications();
	}

	function toggleChannel(id: string) {
		notifConfig.channels = notifConfig.channels.map(c => c.id === id ? { ...c, enabled: !c.enabled } : c);
		saveNotifications();
	}

	function resetNotifForm() {
		notifAddType = null; nfName = '';
		nfHost = ''; nfPort = 587; nfUser = ''; nfPass = ''; nfFrom = ''; nfTo = '';
		nfBotToken = ''; nfChatId = '';
		nfUrl = '';
		nfNtfyServer = 'https://ntfy.sh'; nfNtfyTopic = ''; nfNtfyToken = '';
		nfSignalUrl = 'http://localhost:8080'; nfSignalFrom = ''; nfSignalTo = '';
	}

	async function loadTuning() {
		tuning = await client.call<TuningConfig>('system.tuning.get');
		if (tuning) {
			tNfsThreads = tuning.nfs_threads.toString();
			tNfsLeaseTime = tuning.nfs_lease_time.toString();
			tNfsGraceTime = tuning.nfs_grace_time.toString();
			tSmbMaxConnections = tuning.smb_max_connections.toString();
			tSmbDeadtime = tuning.smb_deadtime.toString();
			tSmbSocketOptions = tuning.smb_socket_options;
			tIscsiCmdsnDepth = tuning.iscsi_default_cmdsn_depth.toString();
			tIscsiLoginTimeout = tuning.iscsi_login_timeout.toString();
			tVmDirtyRatio = tuning.vm_dirty_ratio.toString();
			tVmDirtyBgRatio = tuning.vm_dirty_background_ratio.toString();
			tVmDirtyExpire = tuning.vm_dirty_expire_centisecs.toString();
			tVmDirtyWriteback = tuning.vm_dirty_writeback_centisecs.toString();
		}
	}

	async function saveTuning() {
		savingTuning = true;
		await withToast(
			() => client.call('system.tuning.update', {
				nfs_threads: parseInt(tNfsThreads) || undefined,
				nfs_lease_time: parseInt(tNfsLeaseTime) || undefined,
				nfs_grace_time: parseInt(tNfsGraceTime) || undefined,
				smb_max_connections: parseInt(tSmbMaxConnections) ?? undefined,
				smb_deadtime: parseInt(tSmbDeadtime) ?? undefined,
				smb_socket_options: tSmbSocketOptions || undefined,
				iscsi_default_cmdsn_depth: parseInt(tIscsiCmdsnDepth) || undefined,
				iscsi_login_timeout: parseInt(tIscsiLoginTimeout) || undefined,
				vm_dirty_ratio: parseInt(tVmDirtyRatio) ?? undefined,
				vm_dirty_background_ratio: parseInt(tVmDirtyBgRatio) ?? undefined,
				vm_dirty_expire_centisecs: parseInt(tVmDirtyExpire) || undefined,
				vm_dirty_writeback_centisecs: parseInt(tVmDirtyWriteback) || undefined,
			}),
			'Tuning settings applied'
		);
		savingTuning = false;
		await loadTuning();
	}

</script>


<!-- Tab bar -->
<div class="mb-6 flex border-b border-border">
	<button
		onclick={() => switchTab('general')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'general'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>General</button>
	<button
		onclick={() => switchTab('network')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'network'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>Network</button>
	<button
		onclick={() => switchTab('notifications')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'notifications'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>Notifications</button>
	<button
		onclick={() => switchTab('tuning')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'tuning'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>System Tuning</button>
	<button
		onclick={() => switchTab('metrics')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'metrics'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>Prometheus Metrics</button>
</div>

{#if activeTab === 'general'}

	{#if !settings}
		<p class="text-muted-foreground">Loading...</p>
	{:else}
		<div class="grid grid-cols-1 gap-6 xl:grid-cols-2">

			<!-- Left column -->
			<div class="flex flex-col gap-6">

				<!-- System -->
				<section class="rounded-lg border border-border p-5">
					<h2 class="mb-4 text-base font-semibold">System</h2>

					<div class="mb-4 flex items-center justify-between">
						<span class="text-sm text-muted-foreground">Hostname</span>
						<span class="text-sm font-medium font-mono">{info?.hostname ?? '—'}</span>
					</div>

					<div class="flex gap-2">
						<input
							id="hostname"
							type="text"
							bind:value={hostnameInput}
							class="min-w-0 flex-1 rounded-md border border-input bg-background px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder="nasty"
						/>
						<Button size="sm" onclick={saveHostname} disabled={savingHostname}>
							{savingHostname ? 'Saving…' : 'Apply'}
						</Button>
					</div>
				</section>

				<!-- Date & Time -->
				<section class="rounded-lg border border-border p-5">
					<h2 class="mb-4 text-base font-semibold">Date & Time</h2>

					<div class="mb-3 flex items-center justify-between">
						<span class="text-sm text-muted-foreground">NTP Synchronization</span>
						<div class="flex items-center gap-1.5">
							<span class="inline-block h-2 w-2 rounded-full {info?.ntp_synced ? 'bg-green-400' : 'bg-yellow-400'}"></span>
							<span class="text-sm">{info?.ntp_synced ? 'Synchronized' : 'Not synchronized'}</span>
						</div>
					</div>

					<div class="mb-3 flex items-center justify-between">
						<span class="text-sm text-muted-foreground">Active Timezone</span>
						<span class="text-sm font-medium font-mono">{info?.timezone ?? '—'}</span>
					</div>

					<div class="mb-4 flex items-center justify-between">
						<span class="text-sm text-muted-foreground">Clock Format</span>
						<div class="flex rounded-md border border-border text-xs">
							<button
								onclick={() => saveClock24h(true)}
								class="rounded-l-md px-3 py-1 font-medium transition-colors {settings.clock_24h ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
							>24h</button>
							<button
								onclick={() => saveClock24h(false)}
								class="rounded-r-md px-3 py-1 font-medium transition-colors {!settings.clock_24h ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
							>AM/PM</button>
						</div>
					</div>

					<div class="mb-4 flex items-center justify-between">
						<span class="text-sm text-muted-foreground">Temperature Unit</span>
						<div class="flex rounded-md border border-border text-xs">
							<button
								onclick={() => saveTempUnit('celsius')}
								class="rounded-l-md px-3 py-1 font-medium transition-colors {settings.temp_unit !== 'fahrenheit' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
							>°C</button>
							<button
								onclick={() => saveTempUnit('fahrenheit')}
								class="rounded-r-md px-3 py-1 font-medium transition-colors {settings.temp_unit === 'fahrenheit' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
							>°F</button>
						</div>
					</div>

					<div class="flex gap-2">
						<select
							id="timezone"
							bind:value={settings.timezone}
							class="min-w-0 flex-1 rounded-md border border-input bg-background px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
						>
							{#each timezones as tz}
								<option value={tz}>{tz}</option>
							{/each}
						</select>
						<Button size="sm" onclick={saveTimezone} disabled={saving}>
							{saving ? 'Saving…' : 'Apply'}
						</Button>
					</div>
				</section>


			</div>

			<!-- Right column -->
			<div class="flex flex-col gap-6">

			<!-- Log Level -->
			<section class="rounded-lg border border-border p-5">
				<h2 class="mb-4 text-base font-semibold">Log Level</h2>

				<div class="mb-3 flex flex-wrap gap-2">
					{#each logPresets as preset}
						<button
							onclick={() => logFilter = preset.value}
							class="rounded-md border px-3 py-1 text-xs transition-colors
								{logFilter === preset.value
									? 'border-primary bg-primary text-primary-foreground'
									: 'border-border text-muted-foreground hover:bg-accent'}"
						>{preset.label}</button>
					{/each}
				</div>

				<div class="mb-3">
					<input
						type="text"
						bind:value={logFilter}
						class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-xs focus:outline-none focus:ring-2 focus:ring-ring"
						placeholder="nasty_engine=debug,nasty_system=trace"
					/>
					<span class="mt-1 block text-xs text-muted-foreground">
						Uses <a href="https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html" target="_blank" class="text-blue-400 hover:underline">tracing EnvFilter</a> syntax. Applied immediately, resets on engine restart.
					</span>
				</div>

				<Button size="sm" onclick={applyLogLevel} disabled={savingLog || !logFilter.trim()}>
					{savingLog ? 'Applying…' : 'Apply'}
				</Button>
			</section>

			<!-- Telemetry -->
			<section class="rounded-lg border border-border p-5">
				<h2 class="mb-2 text-base font-semibold">Anonymous Telemetry</h2>
				<p class="mb-4 text-sm text-muted-foreground">
					Help improve NASty by sharing anonymous usage data: number of drives and storage capacity.
					No personal information is collected.
				</p>

				<div class="mb-4">
					<label class="flex items-center gap-2 text-sm cursor-pointer">
						<input
							type="checkbox"
							checked={settings.telemetry_enabled}
							onchange={(e) => saveTelemetry(e.currentTarget.checked)}
							class="rounded border-input"
						/>
						<span class="font-medium">Enable telemetry</span>
					</label>
				</div>

				<Button size="sm" onclick={sendTelemetry} disabled={sendingTelemetry || !settings.telemetry_enabled}>
					{sendingTelemetry ? 'Sending…' : 'Send Now'}
				</Button>
			</section>


			</div>
		</div>
	{/if}

{:else if activeTab === 'network'}

	<div class="grid grid-cols-1 gap-6 xl:grid-cols-2">
		<!-- Left column: Interfaces + Advanced -->
		<div class="space-y-6">
		<!-- Interface list — click to configure -->
		<section class="rounded-lg border border-border p-5">
			<h2 class="mb-4 text-base font-semibold">Interfaces</h2>
			{#if !networkState}
				<p class="text-sm text-muted-foreground">Loading...</p>
			{:else if networkState.interfaces.length === 0}
				<p class="text-sm text-muted-foreground">No network interfaces detected.</p>
			{:else}
				<div class="space-y-2">
					{#each networkState.interfaces as iface}
						{@const isConfigured = network?.interfaces?.some((i: {name: string}) => i.name === iface.name)}
						{@const isSelected = selectedIface === iface.name}
						<div>
							<button
								class="w-full text-left flex items-center gap-4 rounded-lg border px-4 py-3 transition-colors
									{isSelected ? 'border-primary bg-primary/10' : isConfigured ? 'border-primary/50 bg-primary/5' : 'border-border hover:bg-muted/30'}"
								onclick={() => selectInterface(iface.name)}
							>
								<div class="flex-1 min-w-0">
									<div class="flex items-center gap-2">
										<span class="font-mono text-sm font-medium">{iface.name}</span>
										<Badge variant={iface.up ? 'default' : 'secondary'} class="text-[0.6rem]">{iface.up ? 'Up' : 'Down'}</Badge>
										<Badge variant="outline" class="text-[0.6rem]">{iface.kind}</Badge>
										{#if isConfigured}<Badge variant="outline" class="text-[0.6rem]">Configured</Badge>{/if}
									</div>
									{#if iface.ipv4_addresses.length > 0 || iface.ipv6_addresses.length > 0}
										<div class="mt-0.5 font-mono text-xs text-muted-foreground">{[...iface.ipv4_addresses, ...iface.ipv6_addresses].join(', ')}</div>
									{/if}
									<div class="mt-0.5 text-xs text-muted-foreground">{iface.mac} · MTU {iface.mtu}</div>
								</div>
								{#if iface.speed_mbps}
									<span class="text-xs text-muted-foreground">{iface.speed_mbps >= 1000 ? `${iface.speed_mbps / 1000}G` : `${iface.speed_mbps}M`}</span>
								{/if}
							</button>

							<!-- Inline config when selected -->
							{#if isSelected}
								<div class="mt-2 rounded-lg border border-border bg-secondary/20 p-4 space-y-4">
									<!-- IPv4 -->
									<div>
										<div class="mb-2 text-sm font-medium">IPv4</div>
										<div class="flex w-fit rounded-md border border-border text-sm mb-3">
											<button onclick={() => { netDhcp = true; netChanged = true; }} class="rounded-l-md px-3 py-1 font-medium transition-colors {netDhcp ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}">DHCP</button>
											<button onclick={() => { netDhcp = false; netChanged = true; }} class="rounded-r-md px-3 py-1 font-medium transition-colors {!netDhcp ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}">Static</button>
										</div>
										{#if !netDhcp}
											<div class="space-y-1">
												{#each netIpv4Addrs as addr, i}
													<div>
														<div class="flex items-center gap-2">
															<input
																bind:value={netIpv4Addrs[i]}
																placeholder="192.168.1.100/24"
																oninput={() => netChanged = true}
																class="flex-1 rounded-md border bg-background px-2 py-1 font-mono text-sm {netIpv4AddrErrors[i] ? 'border-destructive' : 'border-input'}"
															/>
															{#if netIpv4Addrs.length > 1}
																<button onclick={() => { netIpv4Addrs = netIpv4Addrs.filter((_, j) => j !== i); netChanged = true; }} class="text-xs text-muted-foreground hover:text-foreground">x</button>
															{/if}
														</div>
														{#if netIpv4AddrErrors[i]}
															<p class="mt-0.5 text-xs text-destructive">{netIpv4AddrErrors[i]}</p>
														{/if}
													</div>
												{/each}
												<button onclick={() => { netIpv4Addrs = [...netIpv4Addrs, '']; }} class="text-xs text-primary hover:underline">+ Add address</button>
											</div>
											<div class="mt-2">
												<label for="net-gateway" class="text-xs text-muted-foreground">Gateway</label>
												<input
													id="net-gateway"
													bind:value={netGateway}
													placeholder="192.168.1.1"
													oninput={() => netChanged = true}
													class="mt-1 w-full rounded-md border bg-background px-2 py-1 font-mono text-sm {netGatewayError ? 'border-destructive' : 'border-input'}"
												/>
												{#if netGatewayError}
													<p class="mt-0.5 text-xs text-destructive">{netGatewayError}</p>
												{/if}
											</div>
										{/if}
									</div>

									<!-- IPv6 -->
									<div>
										<div class="mb-2 text-sm font-medium">IPv6</div>
										<div class="flex w-fit rounded-md border border-border text-xs mb-3">
											{#each ['slaac', 'dhcp', 'static', 'disabled'] as m}
												<button onclick={() => { netIpv6Method = m as typeof netIpv6Method; netChanged = true; }}
													class="px-3 py-1 font-medium transition-colors first:rounded-l-md last:rounded-r-md {netIpv6Method === m ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
												>{m === 'slaac' ? 'SLAAC' : m === 'dhcp' ? 'DHCPv6' : m === 'static' ? 'Static' : 'Off'}</button>
											{/each}
										</div>
										{#if netIpv6Method === 'static'}
											<div class="space-y-1">
												{#each netIpv6Addrs as addr, i}
													<div>
														<div class="flex items-center gap-2">
															<input
																bind:value={netIpv6Addrs[i]}
																placeholder="fd00::1/64"
																oninput={() => netChanged = true}
																class="flex-1 rounded-md border bg-background px-2 py-1 font-mono text-sm {netIpv6AddrErrors[i] ? 'border-destructive' : 'border-input'}"
															/>
															{#if netIpv6Addrs.length > 1}
																<button onclick={() => { netIpv6Addrs = netIpv6Addrs.filter((_, j) => j !== i); netChanged = true; }} class="text-xs text-muted-foreground hover:text-foreground">x</button>
															{/if}
														</div>
														{#if netIpv6AddrErrors[i]}
															<p class="mt-0.5 text-xs text-destructive">{netIpv6AddrErrors[i]}</p>
														{/if}
													</div>
												{/each}
												<button onclick={() => { netIpv6Addrs = [...netIpv6Addrs, '']; }} class="text-xs text-primary hover:underline">+ Add address</button>
											</div>
											<div class="mt-2">
												<label for="net-ipv6-gw" class="text-xs text-muted-foreground">Gateway</label>
												<input
													id="net-ipv6-gw"
													bind:value={netIpv6Gateway}
													placeholder="fd00::1"
													oninput={() => netChanged = true}
													class="mt-1 w-full rounded-md border bg-background px-2 py-1 font-mono text-sm {netIpv6GatewayError ? 'border-destructive' : 'border-input'}"
												/>
												{#if netIpv6GatewayError}
													<p class="mt-0.5 text-xs text-destructive">{netIpv6GatewayError}</p>
												{/if}
											</div>
										{/if}
									</div>

									<!-- MTU -->
									<div>
										<label for="net-mtu" class="text-xs text-muted-foreground">MTU</label>
										<input id="net-mtu" type="number" min="68" max="65535" bind:value={netMtu} placeholder="default (1500)" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
										<p class="mt-1 text-[0.65rem] text-muted-foreground">Leave empty for default. 9000 enables jumbo frames (requires switch support end-to-end).</p>
									</div>

									<!-- DNS -->
									<div>
										<label for="net-dns" class="text-xs text-muted-foreground">DNS Servers</label>
										<input
											id="net-dns"
											bind:value={netNameservers}
											placeholder="1.1.1.1, 8.8.8.8"
											oninput={() => netChanged = true}
											class="mt-1 w-full rounded-md border bg-background px-2 py-1 font-mono text-sm {netNameserversErrors.length > 0 ? 'border-destructive' : 'border-input'}"
										/>
										{#if netNameserversErrors.length > 0}
											<p class="mt-0.5 text-xs text-destructive">
												{netNameserversErrors[0].err} ({netNameserversErrors[0].value})
											</p>
										{/if}
									</div>

									{#if netChanged && !netDhcp}
										<p class="text-xs text-amber-500">Changing the IP will move your connection to the new address.</p>
									{/if}

									<Button size="sm" onclick={saveInterfaceConfig} disabled={savingNetwork || !netChanged || netHasErrors}>
										{savingNetwork ? 'Applying\u2026' : 'Apply'}
									</Button>
									{#if netChanged && netHasErrors}
										<p class="text-xs text-destructive">Fix the highlighted fields before applying.</p>
									{/if}
								</div>
							{/if}
						</div>
					{/each}
				</div>
			{/if}
		</section>

		<!-- Bond / Bridge / VLAN creation -->
		<section class="rounded-lg border border-border p-5">
			<div class="flex items-center gap-3 mb-4">
				<h2 class="text-base font-semibold">Advanced</h2>
				<Button size="xs" variant="secondary" onclick={() => { showBondForm = !showBondForm; showVlanForm = false; showBridgeForm = false; }}>{showBondForm ? 'Cancel' : '+ Bond'}</Button>
				<Button size="xs" variant="secondary" onclick={() => { showBridgeForm = !showBridgeForm; showBondForm = false; showVlanForm = false; }}>{showBridgeForm ? 'Cancel' : '+ Bridge'}</Button>
				<Button size="xs" variant="secondary" onclick={() => { showVlanForm = !showVlanForm; showBondForm = false; showBridgeForm = false; }}>{showVlanForm ? 'Cancel' : '+ VLAN'}</Button>
			</div>

			{#if showBondForm}
				<div class="rounded-lg border border-border bg-secondary/20 p-4 space-y-3">
					<div class="text-sm font-medium">Create Bond Interface</div>
					<p class="text-xs text-muted-foreground">Combine multiple interfaces for redundancy or throughput.</p>
					<div class="grid grid-cols-2 gap-3">
						<div>
							<label for="bond-name" class="text-xs text-muted-foreground">Name</label>
							<input id="bond-name" bind:value={bondName} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
						</div>
						<div>
							<label for="bond-mode" class="text-xs text-muted-foreground">Mode</label>
							<select id="bond-mode" bind:value={bondMode} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm">
								<option value="active_backup">Active-Backup (failover)</option>
								<option value="lacp">LACP (802.3ad)</option>
								<option value="balance_rr">Balance Round-Robin</option>
								<option value="balance_xor">Balance XOR</option>
							</select>
						</div>
					</div>
					<div>
						<div class="text-xs text-muted-foreground mb-1">Members (select 2+)</div>
						{#if networkState}
							<div class="flex flex-wrap gap-2">
								{#each networkState.interfaces.filter(i => i.kind === 'physical') as iface}
									<label class="flex items-center gap-1.5 text-sm">
										<input type="checkbox" checked={bondMembers.includes(iface.name)}
											onchange={() => { bondMembers = bondMembers.includes(iface.name) ? bondMembers.filter(m => m !== iface.name) : [...bondMembers, iface.name]; }} />
										{iface.name}
									</label>
								{/each}
							</div>
						{/if}
					</div>
					<div>
						<label for="bond-mtu" class="text-xs text-muted-foreground">MTU (optional)</label>
						<input id="bond-mtu" type="number" min="68" max="65535" bind:value={bondMtu} placeholder="default (1500), 9000 for jumbo frames" class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
					</div>
					<div>
						<label class="flex items-start gap-2 text-xs">
							<input type="checkbox" bind:checked={bondNoInheritMac} class="mt-0.5" />
							<span>
								<span class="text-foreground">Don't inherit member MAC</span>
								<span class="block text-muted-foreground mt-0.5">By default, the bond adopts the primary member's MAC so DHCP keeps handing out the same lease across the enslave step. Check this to let the kernel pick from active slaves instead.</span>
							</span>
						</label>
					</div>
					{#if networkState?.mgmt_iface && bondMembers.includes(networkState.mgmt_iface)}
						<div class="rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-xs text-amber-300 space-y-1">
							<div class="font-medium">Heads up — this bonds your management interface</div>
							<p>You're connected through <span class="font-mono">{networkState.mgmt_iface}</span>. After applying, you'll have 30 seconds to keep the change before it auto-rolls back.</p>
							{#if bondMembers.length > 1}
								<p>The bond will use <span class="font-mono">{networkState.mgmt_iface}</span>'s MAC (the management interface, preferred when it's a member among multiple).</p>
							{/if}
							{#if bondNoInheritMac}
								<p class="font-medium">⚠ "Don't inherit member MAC" is checked. Your DHCP server will treat the bond as a new client and is very likely to hand out a different IP — your session will land on the new IP and you'll need to reconnect there to confirm.</p>
							{/if}
						</div>
					{/if}
					<Button size="sm" onclick={createBond} disabled={bondMembers.length < 2}>Create Bond</Button>
				</div>
			{/if}

			{#if showBridgeForm}
				<BridgeCreator
					{networkState}
					onCreated={async () => { await refreshNetworkState(); showBridgeForm = false; }}
					onCancel={() => { showBridgeForm = false; }}
				/>
			{/if}

			{#if showVlanForm}
				<div class="rounded-lg border border-border bg-secondary/20 p-4 space-y-3">
					<div class="text-sm font-medium">Create VLAN Interface</div>
					<p class="text-xs text-muted-foreground">Tag traffic on a physical interface with a VLAN ID.</p>
					<div class="grid grid-cols-2 gap-3">
						<div>
							<label for="vlan-parent" class="text-xs text-muted-foreground">Parent Interface {#if !vlanParent && vlanTried}<span class="text-amber-500">required</span>{/if}</label>
							<select id="vlan-parent" bind:value={vlanParent} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm {requiredFieldCls(!vlanParent, vlanTried)}">
								<option value="">Select...</option>
								{#if networkState}
									{#each networkState.interfaces.filter(i => i.kind === 'physical' || i.kind === 'bond') as iface}
										<option value={iface.name}>{iface.name}</option>
									{/each}
								{/if}
							</select>
						</div>
						<div>
							<label for="vlan-id" class="text-xs text-muted-foreground">VLAN ID (1-4094)</label>
							<input id="vlan-id" type="number" min="1" max="4094" bind:value={vlanId} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
						</div>
					</div>
					<div>
						<label for="vlan-mtu" class="text-xs text-muted-foreground">MTU (optional)</label>
						<input id="vlan-mtu" type="number" min="68" max="65535" bind:value={vlanMtu} placeholder="default (1500), 9000 for jumbo frames" class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
					</div>
					<Button size="sm" onclick={createVlan}>Create VLAN</Button>
				</div>
			{/if}

			{#if !showBondForm && !showBridgeForm && !showVlanForm}
				{#if network && (network.bonds?.length > 0 || network.bridges?.length > 0 || network.vlans?.length > 0)}
					<div class="space-y-1 text-sm">
						{#each network.bonds as bond}
							<div class="flex items-center gap-2 rounded px-2 py-1">
								<Badge variant="outline" class="text-[0.6rem]">bond</Badge>
								<span class="font-mono">{bond.name}</span>
								<span class="text-xs text-muted-foreground">{bond.mode} · {bond.members.join(', ')}{bond.mtu ? ` · MTU ${bond.mtu}` : ''}</span>
								<Button size="xs" variant="ghost" class="ml-auto text-destructive hover:text-destructive" onclick={() => deleteBond(bond.name)}>Remove</Button>
							</div>
						{/each}
						{#each network.bridges ?? [] as bridge}
							<div class="flex items-center gap-2 rounded px-2 py-1">
								<Badge variant="outline" class="text-[0.6rem]">bridge</Badge>
								<span class="font-mono">{bridge.name}</span>
								<span class="text-xs text-muted-foreground">{bridge.members.length === 0 ? 'no members' : bridge.members.join(', ')}{bridge.mtu ? ` · MTU ${bridge.mtu}` : ''}</span>
								<Button size="xs" variant="ghost" class="ml-auto text-destructive hover:text-destructive" onclick={() => deleteBridge(bridge.name)}>Remove</Button>
							</div>
						{/each}
						{#each network.vlans as vlan}
							<div class="flex items-center gap-2 rounded px-2 py-1">
								<Badge variant="outline" class="text-[0.6rem]">vlan</Badge>
								<span class="font-mono">{vlan.parent}.{vlan.vlan_id}</span>
								{#if vlan.mtu}<span class="text-xs text-muted-foreground">MTU {vlan.mtu}</span>{/if}
								<Button size="xs" variant="ghost" class="ml-auto text-destructive hover:text-destructive" onclick={() => deleteVlan(vlan.parent, vlan.vlan_id)}>Remove</Button>
							</div>
						{/each}
					</div>
				{:else}
					<p class="text-xs text-muted-foreground">No bonds, bridges, or VLANs configured.</p>
				{/if}
			{/if}
		</section>

		</div>
	</div>

{:else if activeTab === 'notifications'}

	<div class="max-w-2xl space-y-4">
		<section class="rounded-lg border border-border p-5">
			<h2 class="mb-1 text-base font-semibold">Notification Channels</h2>
			<p class="mb-4 text-sm text-muted-foreground">
				Get notified when alerts fire — disk failures, space issues, scrub errors, and more.
			</p>

			{#if notifConfig.channels.length === 0}
				<p class="text-sm text-muted-foreground">No channels configured.</p>
			{:else}
				<div class="space-y-2 mb-4">
					{#each notifConfig.channels as ch}
						<div class="flex items-center gap-3 rounded-lg border border-border px-4 py-3">
							<button onclick={() => toggleChannel(ch.id)} class="shrink-0" title="{ch.enabled ? 'Disable' : 'Enable'} {ch.name}">
								<span class="h-2 w-2 rounded-full inline-block {ch.enabled ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
							</button>
							<div class="flex-1 min-w-0">
								<div class="text-sm font-medium">{ch.name}</div>
								<div class="text-xs text-muted-foreground">
									{ch.type === 'smtp' ? `${ch.to} via ${ch.host}` :
									 ch.type === 'telegram' ? `Chat ${ch.chat_id}` :
									 ch.type === 'webhook' ? ch.url :
									 ch.type === 'ntfy' ? `${ch.server_url}/${ch.topic}` :
								 ch.type === 'signal' ? `${ch.to_number} via ${ch.api_url}` : ch.type}
								</div>
							</div>
							<div class="flex gap-2">
								<Button size="xs" variant="secondary" onclick={() => testChannel(ch)} disabled={notifTesting === ch.id}>
									{notifTesting === ch.id ? 'Sending...' : 'Test'}
								</Button>
								<Button size="xs" variant="destructive" onclick={() => removeChannel(ch.id)}>Remove</Button>
							</div>
						</div>
					{/each}
				</div>
			{/if}

			{#if notifAddType === null}
				<div class="flex gap-2">
					<Button size="sm" variant="secondary" onclick={() => { notifAddType = 'smtp'; nfName = 'Email'; }}>+ Email</Button>
					<Button size="sm" variant="secondary" onclick={() => { notifAddType = 'telegram'; nfName = 'Telegram'; }}>+ Telegram</Button>
					<Button size="sm" variant="secondary" onclick={() => { notifAddType = 'webhook'; nfName = 'Webhook'; }}>+ Webhook</Button>
					<Button size="sm" variant="secondary" onclick={() => { notifAddType = 'ntfy'; nfName = 'ntfy'; }}>+ ntfy</Button>
					<Button size="sm" variant="secondary" onclick={() => { notifAddType = 'signal'; nfName = 'Signal'; }}>+ Signal</Button>
				</div>
			{:else}
				<div class="rounded-lg border border-border bg-secondary/20 p-4 space-y-3">
					<div class="text-sm font-medium">Add {notifAddType.toUpperCase()} channel</div>
					{#if notifAddType === 'smtp'}
						<p class="text-xs text-muted-foreground">Send alerts via email. Use your email provider's SMTP settings (e.g. Gmail: smtp.gmail.com, port 587, TLS on). For Gmail, use an <a href="https://myaccount.google.com/apppasswords" target="_blank" rel="noopener" class="text-primary hover:underline">App Password</a>.</p>
					{:else if notifAddType === 'telegram'}
						<p class="text-xs text-muted-foreground">Send alerts to a Telegram chat. Create a bot via <a href="https://t.me/BotFather" target="_blank" rel="noopener" class="text-primary hover:underline">@BotFather</a>, copy the token. Then send a message to the bot and get your Chat ID from <code class="font-mono">https://api.telegram.org/bot&lt;TOKEN&gt;/getUpdates</code>.</p>
					{:else if notifAddType === 'webhook'}
						<p class="text-xs text-muted-foreground">Send a JSON POST to any URL when alerts fire. The payload includes <code class="font-mono">subject</code>, <code class="font-mono">body</code>, <code class="font-mono">source</code>, and <code class="font-mono">timestamp</code> fields. Works with Discord webhooks, Slack incoming webhooks, Home Assistant, or any custom endpoint.</p>
					{:else if notifAddType === 'ntfy'}
						<p class="text-xs text-muted-foreground">Push notifications via <a href="https://ntfy.sh" target="_blank" rel="noopener" class="text-primary hover:underline">ntfy</a> — install the ntfy app on your phone, subscribe to your topic, and alerts arrive as push notifications. The free ntfy.sh server works without a token. Self-hosted servers may require one.</p>
					{:else if notifAddType === 'signal'}
						<p class="text-xs text-muted-foreground">Send alerts via Signal. Requires a <a href="https://github.com/bbernhard/signal-cli-rest-api" target="_blank" rel="noopener" class="text-primary hover:underline">signal-cli-rest-api</a> container running — deploy it as a NASty app, then point the API URL here. You'll need a registered phone number as the sender.</p>
					{/if}
					<div>
						<label for="nf-name" class="text-xs text-muted-foreground">Name</label>
						<input id="nf-name" bind:value={nfName} class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
					</div>

					{#if notifAddType === 'smtp'}
						<div class="grid grid-cols-2 gap-3">
							<div>
								<label for="nf-host" class="text-xs text-muted-foreground">SMTP Host</label>
								<input id="nf-host" bind:value={nfHost} placeholder="smtp.gmail.com" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
							</div>
							<div>
								<label for="nf-port" class="text-xs text-muted-foreground">Port</label>
								<input id="nf-port" type="number" bind:value={nfPort} class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
							</div>
							<div>
								<label for="nf-user" class="text-xs text-muted-foreground">Username</label>
								<input id="nf-user" bind:value={nfUser} class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
							</div>
							<div>
								<label for="nf-pass" class="text-xs text-muted-foreground">Password</label>
								<input id="nf-pass" type="password" bind:value={nfPass} class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
							</div>
							<div>
								<label for="nf-from" class="text-xs text-muted-foreground">From</label>
								<input id="nf-from" bind:value={nfFrom} placeholder="nasty@example.com" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
							</div>
							<div>
								<label for="nf-to" class="text-xs text-muted-foreground">To</label>
								<input id="nf-to" bind:value={nfTo} placeholder="admin@example.com" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm" />
							</div>
						</div>
					{:else if notifAddType === 'telegram'}
						<div>
							<label for="nf-bot-token" class="text-xs text-muted-foreground">Bot Token</label>
							<input id="nf-bot-token" bind:value={nfBotToken} placeholder="123456:ABC-DEF..." class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
						<div>
							<label for="nf-chat-id" class="text-xs text-muted-foreground">Chat ID</label>
							<input id="nf-chat-id" bind:value={nfChatId} placeholder="-1001234567890" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
					{:else if notifAddType === 'webhook'}
						<div>
							<label for="nf-url" class="text-xs text-muted-foreground">URL</label>
							<input id="nf-url" bind:value={nfUrl} placeholder="https://discord.com/api/webhooks/..." class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
							<p class="mt-1 text-xs text-muted-foreground">Example: Discord webhook URL, Slack incoming webhook, or any endpoint that accepts JSON POST.</p>
						</div>
					{:else if notifAddType === 'ntfy'}
						<div>
							<label for="nf-ntfy-server" class="text-xs text-muted-foreground">Server URL</label>
							<input id="nf-ntfy-server" bind:value={nfNtfyServer} class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
						<div>
							<label for="nf-ntfy-topic" class="text-xs text-muted-foreground">Topic</label>
							<input id="nf-ntfy-topic" bind:value={nfNtfyTopic} placeholder="my-nasty-alerts" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
						<div>
							<label for="nf-ntfy-token" class="text-xs text-muted-foreground">Token (optional)</label>
							<input id="nf-ntfy-token" bind:value={nfNtfyToken} class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
					{:else if notifAddType === 'signal'}
						<div>
							<label for="nf-signal-url" class="text-xs text-muted-foreground">API URL</label>
							<input id="nf-signal-url" bind:value={nfSignalUrl} placeholder="http://localhost:8080" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
						<div>
							<label for="nf-signal-from" class="text-xs text-muted-foreground">From Number</label>
							<input id="nf-signal-from" bind:value={nfSignalFrom} placeholder="+1234567890" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
						<div>
							<label for="nf-signal-to" class="text-xs text-muted-foreground">To Number</label>
							<input id="nf-signal-to" bind:value={nfSignalTo} placeholder="+0987654321" class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm font-mono" />
						</div>
					{/if}

					<div class="flex gap-2">
						<Button size="sm" onclick={addChannel}>Add</Button>
						<Button size="sm" variant="secondary" onclick={testNewChannel} disabled={notifTesting === '_new'}>
							{notifTesting === '_new' ? 'Sending...' : 'Test'}
						</Button>
						<Button size="sm" variant="secondary" onclick={resetNotifForm}>Cancel</Button>
					</div>
				</div>
			{/if}
		</section>
	</div>

{:else if activeTab === 'tuning'}

	{#if !tuning}
		<p class="text-muted-foreground">Loading...</p>
	{:else}
		<div class="grid grid-cols-1 gap-6 xl:grid-cols-2">
			<p class="text-sm text-muted-foreground col-span-full">
				NFS, SMB, and iSCSI tuning is now in <a href="/services" class="text-blue-400 hover:underline">Services</a> → Configure.
			</p>

			<!-- VM Writeback -->
			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">VM Writeback (sysctl)</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
					<div>
						<label for="vm-dirty" class="mb-1 block text-xs text-muted-foreground">dirty_ratio (%)</label>
						<input id="vm-dirty" type="number" min="0" max="100" bind:value={tVmDirtyRatio}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Max dirty memory before synchronous writeback. Default: 20.</p>
					</div>
					<div>
						<label for="vm-dirty-bg" class="mb-1 block text-xs text-muted-foreground">dirty_background_ratio (%)</label>
						<input id="vm-dirty-bg" type="number" min="0" max="100" bind:value={tVmDirtyBgRatio}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Threshold for background writeback to start. Default: 10.</p>
					</div>
					<div>
						<label for="vm-expire" class="mb-1 block text-xs text-muted-foreground">dirty_expire (cs)</label>
						<input id="vm-expire" type="number" min="0" bind:value={tVmDirtyExpire}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Centiseconds before dirty pages are eligible for flush. Default: 3000.</p>
					</div>
					<div>
						<label for="vm-writeback" class="mb-1 block text-xs text-muted-foreground">dirty_writeback (cs)</label>
						<input id="vm-writeback" type="number" min="0" bind:value={tVmDirtyWriteback}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Centiseconds between writeback daemon wakeups. Default: 500.</p>
					</div>
				</div>
			</section>

		</div>

		<div class="mt-6">
			<Button onclick={saveTuning} disabled={savingTuning}>
				{savingTuning ? 'Applying...' : 'Apply Tuning'}
			</Button>
			<p class="mt-1 text-xs text-muted-foreground">All changes take effect immediately without restart.</p>
		</div>
	{/if}

{:else if activeTab === 'metrics'}

	<!-- Metrics tab -->
	<div class="rounded-lg border border-border p-5">
		<div class="mb-4 flex items-center justify-between">
			<div>
				<h2 class="text-base font-semibold">Prometheus Metrics</h2>
				<p class="text-xs text-muted-foreground">Raw metrics from nasty-metrics in Prometheus text exposition format</p>
			</div>
			<div class="flex gap-2">
				<Button size="sm" variant="outline" onclick={loadMetrics} disabled={metricsLoading}>
					{metricsLoading ? 'Loading…' : 'Refresh'}
				</Button>
				{#if metricsText}
					<Button size="sm" variant="outline" onclick={copyMetrics}>
						{#if metricsCopied}
							<Check class="mr-1.5 h-3.5 w-3.5" />Copied
						{:else}
							<Copy class="mr-1.5 h-3.5 w-3.5" />Copy All
						{/if}
					</Button>
				{/if}
			</div>
		</div>

		{#if metricsLoading && !metricsText}
			<p class="text-sm text-muted-foreground">Loading metrics...</p>
		{:else if !metricsText}
			<p class="text-sm text-muted-foreground">No metrics available. Is nasty-metrics running?</p>
		{:else}
			<div class="space-y-2">
				{#each metricsSections as section}
					<div class="rounded-md border border-border">
						<button
							onclick={() => toggleSection(section.title)}
							class="flex w-full items-center gap-2 px-3 py-2 text-left text-sm font-medium hover:bg-accent/50 transition-colors"
						>
							{#if collapsedSections[section.title]}
								<ChevronRight class="h-4 w-4 shrink-0 text-muted-foreground" />
							{:else}
								<ChevronDown class="h-4 w-4 shrink-0 text-muted-foreground" />
							{/if}
							{section.title}
							<span class="ml-auto text-xs text-muted-foreground">
								{section.lines.filter(l => !l.startsWith('#')).length} metrics
							</span>
						</button>
						{#if !collapsedSections[section.title]}
							<pre class="max-h-[400px] overflow-auto border-t border-border bg-muted/30 px-3 py-2 text-xs leading-relaxed font-mono">{section.lines.join('\n')}</pre>
						{/if}
					</div>
				{/each}
			</div>
		{/if}
	</div>

{/if}
