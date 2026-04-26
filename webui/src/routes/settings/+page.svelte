<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';
	import type { Settings, SystemInfo, NetworkState, NetworkConfig, LiveInterface, TuningConfig, NutConfig, UpsStatus, NetIfStats } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Copy, Check, ChevronDown, ChevronRight } from '@lucide/svelte';

	let activeTab: 'general' | 'network' | 'notifications' | 'tls' | 'vpn' | 'metrics' | 'tuning' | 'ups' = $state('general');

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
	let nfFrom = $state(''); let nfTo = $state(''); let nfTls = $state(true);
	let nfBotToken = $state(''); let nfChatId = $state('');
	let nfUrl = $state('');
	let nfNtfyServer = $state('https://ntfy.sh'); let nfNtfyTopic = $state(''); let nfNtfyToken = $state('');
	let nfSignalUrl = $state('http://localhost:8080'); let nfSignalFrom = $state(''); let nfSignalTo = $state('');

	// Network tab
	let netInterfaces: NetIfStats[] = $state([]);
	let netIfLoaded = $state(false);
	let selectedIface: string | null = $state(null);
	// IPv6 form
	let netIpv6Method: 'slaac' | 'static' | 'dhcp' | 'disabled' = $state('slaac');
	let netIpv6Address = $state('');
	let netIpv6Gateway = $state('');
	// Bond form
	let showBondForm = $state(false);
	let bondName = $state('bond0');
	let bondMembers: string[] = $state([]);
	let bondMode: 'lacp' | 'active_backup' | 'balance_rr' | 'balance_xor' = $state('active_backup');
	// VLAN form
	let showVlanForm = $state(false);
	let vlanParent = $state('');
	let vlanId = $state(100);
	// Firewall
	import type { FirewallStatus } from '$lib/types';
	let firewallStatus: FirewallStatus | null = $state(null);
	let fwEditService: string | null = $state(null);
	let fwEditSources = $state('');

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
	let netAddress = $state('');
	let netPrefix = $state('24');
	let netGateway = $state('');
	let netNameservers = $state('');
	let netChanged = $state(false);

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

	// UPS (NUT)
	let nutConfig: NutConfig | null = $state(null);
	let savingNut = $state(false);
	let upsStatus: UpsStatus | null = $state(null);
	let upsStatusInterval: ReturnType<typeof setInterval> | null = null;
	let nutDriver = $state('');
	let nutPort = $state('');
	let nutUpsName = $state('');
	let nutDescription = $state('');
	let nutShutdownPercent = $state('');
	let nutShutdownSeconds = $state('');
	let nutShutdownCommand = $state('');

	// TLS
	let tlsDomain = $state('');
	let tlsAcmeEmail = $state('');
	let tlsAcmeEnabled = $state(false);
	let acmeStatus: { state: string; message: string; domain?: string; last_attempt?: string } | null = $state(null);
	let tlsAcmeStaging = $state(false);
	let tlsChallengeType = $state<'tls-alpn' | 'dns'>('tls-alpn');
	let tlsDnsProvider = $state('');
	let tlsDnsCredentials = $state('');
	let savingTls = $state(false);
	let tlsChanged = $state(false);

	// Telemetry
	let sendingTelemetry = $state(false);

// VPN (Tailscale)
	interface TailscaleStatus {
		enabled: boolean;
		daemon_running: boolean;
		connected: boolean;
		ip?: string;
		hostname?: string;
		version?: string;
		has_auth_key: boolean;
	}
	let tsStatus: TailscaleStatus | null = $state(null);
	let tsAuthKey = $state('');
	let tsLoading = $state(false);

	const popularDnsProviders = [
		{ code: 'cloudflare', name: 'Cloudflare' },
		{ code: 'route53', name: 'Amazon Route 53' },
		{ code: 'gcloud', name: 'Google Cloud' },
		{ code: 'azuredns', name: 'Azure DNS' },
		{ code: 'digitalocean', name: 'DigitalOcean' },
		{ code: 'hetzner', name: 'Hetzner' },
		{ code: 'godaddy', name: 'GoDaddy' },
		{ code: 'namecheap', name: 'Namecheap' },
		{ code: 'ovh', name: 'OVH' },
		{ code: 'porkbun', name: 'Porkbun' },
		{ code: 'vultr', name: 'Vultr' },
		{ code: 'linode', name: 'Linode' },
		{ code: 'duckdns', name: 'Duck DNS' },
		{ code: 'desec', name: 'deSEC.io' },
		{ code: 'oraclecloud', name: 'Oracle Cloud' },
	];

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
			tlsDomain = settings?.tls_domain ?? '';
			tlsAcmeEmail = settings?.tls_acme_email ?? '';
			tlsAcmeEnabled = settings?.tls_acme_enabled ?? false;
			tlsChallengeType = settings?.tls_challenge_type ?? 'tls-alpn';
			tlsDnsProvider = settings?.tls_dns_provider ?? '';
			tlsDnsCredentials = settings?.tls_dns_credentials ?? '';
			tlsAcmeStaging = (settings as any)?.tls_acme_staging ?? false;
			syncNetworkForm();

			// Load ACME status
			try { acmeStatus = await client.call('system.acme.status'); } catch { /* ignore */ }

// Load Tailscale status
			try {
				tsStatus = await client.call<TailscaleStatus>('system.tailscale.get');
			} catch { /* ignore — tailscale module may not be enabled */ }
		});
	});

	function syncNetworkForm() {
		if (!network || !network.interfaces.length) return;
		const iface = network.interfaces[0];
		netDhcp = iface.ipv4.method === 'dhcp';
		if (iface.ipv4.addresses.length > 0) {
			const parts = iface.ipv4.addresses[0].split('/');
			netAddress = parts[0] ?? '';
			netPrefix = parts[1] ?? '24';
		} else {
			netAddress = '';
			netPrefix = '24';
		}
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

	async function saveTls() {
		savingTls = true;
		const result = await withToast(
			() => client.call<Settings>('system.settings.update', {
				tls_domain: tlsDomain || null,
				tls_acme_email: tlsAcmeEmail || null,
				tls_acme_enabled: tlsAcmeEnabled,
				tls_challenge_type: tlsChallengeType,
				tls_dns_provider: tlsDnsProvider || null,
				tls_dns_credentials: tlsDnsCredentials || null,
				tls_acme_staging: tlsAcmeStaging,
			}),
			tlsAcmeEnabled ? 'Let\'s Encrypt certificate requested — check status below' : 'TLS settings saved'
		);
		if (result !== undefined) {
			settings = result;
			tlsChanged = false;
			// Poll ACME status for a few seconds to show progress
			if (tlsAcmeEnabled) {
				const poll = setInterval(async () => {
					try { acmeStatus = await client.call('system.acme.status'); } catch { /* ignore */ }
					if (acmeStatus && (acmeStatus.state === 'success' || acmeStatus.state === 'error')) {
						clearInterval(poll);
					}
				}, 3000);
				setTimeout(() => clearInterval(poll), 120000); // stop after 2 min
			}
		}
		savingTls = false;
	}

	async function saveNetwork() {
		savingNetwork = true;
		const nameservers = netNameservers
			.split(/[,\s]+/)
			.map((s) => s.trim())
			.filter(Boolean);

		// Build new config from current state + form values
		const ifaceName = network?.interfaces?.[0]?.name || networkState?.interfaces?.[0]?.name || 'eth0';
		const ipv4Method = netDhcp ? 'dhcp' : 'static';
		const ipv4Addresses = netDhcp ? [] : [`${netAddress.trim()}/${netPrefix}`];
		const ipv4Gateway = netDhcp ? null : (netGateway.trim() || null);

		const payload: NetworkConfig = {
			interfaces: [{
				name: ifaceName,
				enabled: true,
				ipv4: { method: ipv4Method, addresses: ipv4Addresses, gateway: ipv4Gateway },
				ipv6: network?.interfaces?.[0]?.ipv6 ?? { method: 'slaac', addresses: [], gateway: null },
				mtu: network?.interfaces?.[0]?.mtu ?? null,
			}],
			dns: nameservers,
			bonds: network?.bonds ?? [],
			vlans: network?.vlans ?? [],
		};

		await withToast(
			() => client.call('system.network.update', payload),
			'Network configuration applied'
		);
		networkState = await client.call<NetworkState>('system.network.get');
		syncNetworkForm();
		savingNetwork = false;
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
			loadFirewall();
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
		if (tab === 'ups') {
			if (!nutConfig) loadNut();
			else startUpsPolling();
		} else {
			stopUpsPolling();
		}
	}

	function selectInterface(name: string) {
		selectedIface = selectedIface === name ? null : name;
		if (selectedIface && network) {
			const cfg = network.interfaces.find((i: {name: string}) => i.name === name);
			if (cfg) {
				netDhcp = cfg.ipv4.method === 'dhcp';
				if (cfg.ipv4.addresses.length > 0) {
					const parts = cfg.ipv4.addresses[0].split('/');
					netAddress = parts[0] ?? '';
					netPrefix = parts[1] ?? '24';
				} else { netAddress = ''; netPrefix = '24'; }
				netGateway = cfg.ipv4.gateway ?? '';
				netIpv6Method = cfg.ipv6.method as typeof netIpv6Method;
				netIpv6Address = cfg.ipv6.addresses[0] ?? '';
				netIpv6Gateway = cfg.ipv6.gateway ?? '';
			} else {
				netDhcp = true; netAddress = ''; netPrefix = '24'; netGateway = '';
				netIpv6Method = 'slaac'; netIpv6Address = ''; netIpv6Gateway = '';
			}
			netChanged = false;
		}
	}

	async function saveInterfaceConfig() {
		if (!selectedIface || !network) return;
		savingNetwork = true;
		const nameservers = netNameservers.split(/[,\s]+/).map(s => s.trim()).filter(Boolean);
		const ipv4 = {
			method: netDhcp ? 'dhcp' as const : 'static' as const,
			addresses: netDhcp ? [] : [`${netAddress.trim()}/${netPrefix}`],
			gateway: netDhcp ? null : (netGateway.trim() || null),
		};
		const ipv6 = {
			method: netIpv6Method,
			addresses: netIpv6Method === 'static' && netIpv6Address ? [netIpv6Address] : [],
			gateway: netIpv6Method === 'static' ? (netIpv6Gateway || null) : null,
		};

		// Update or add the interface in config
		const ifaces = [...(network.interfaces || [])];
		const idx = ifaces.findIndex((i: {name: string}) => i.name === selectedIface);
		const entry = { name: selectedIface, enabled: true, ipv4, ipv6, mtu: null };
		if (idx >= 0) ifaces[idx] = entry; else ifaces.push(entry);

		const payload = { interfaces: ifaces, dns: nameservers, bonds: network.bonds || [], vlans: network.vlans || [] };
		await withToast(() => client.call('system.network.update', payload), 'Network configuration applied');
		networkState = await client.call<NetworkState>('system.network.get');
		netChanged = false;
		savingNetwork = false;
	}

	async function createBond() {
		if (!bondName || bondMembers.length < 2 || !network) return;
		const payload = {
			interfaces: network.interfaces || [],
			dns: network.dns || [],
			bonds: [...(network.bonds || []), { name: bondName, members: bondMembers, mode: bondMode, ipv4: { method: 'dhcp', addresses: [], gateway: null }, ipv6: { method: 'slaac', addresses: [], gateway: null } }],
			vlans: network.vlans || [],
		};
		await withToast(() => client.call('system.network.update', payload), `Bond ${bondName} created`);
		networkState = await client.call<NetworkState>('system.network.get');
		showBondForm = false; bondName = 'bond0'; bondMembers = [];
	}

	async function createVlan() {
		if (!vlanParent || vlanId < 1 || vlanId > 4094 || !network) return;
		const payload = {
			interfaces: network.interfaces || [],
			dns: network.dns || [],
			bonds: network.bonds || [],
			vlans: [...(network.vlans || []), { parent: vlanParent, vlan_id: vlanId, ipv4: { method: 'dhcp', addresses: [], gateway: null }, ipv6: { method: 'slaac', addresses: [], gateway: null } }],
		};
		await withToast(() => client.call('system.network.update', payload), `VLAN ${vlanParent}.${vlanId} created`);
		networkState = await client.call<NetworkState>('system.network.get');
		showVlanForm = false; vlanParent = ''; vlanId = 100;
	}

	async function loadFirewall() {
		try { firewallStatus = await client.call<FirewallStatus>('system.firewall.status'); } catch { /* ignore */ }
	}

	function startEditRestriction(service: string) {
		fwEditService = service;
		fwEditSources = (firewallStatus?.restrictions[service] ?? []).join(', ');
	}

	async function saveRestriction() {
		if (!fwEditService) return;
		const sources = fwEditSources.split(/[,\s]+/).map(s => s.trim()).filter(Boolean);
		await withToast(
			() => client.call('system.firewall.restrict', { service: fwEditService, sources }),
			sources.length > 0 ? `Access restricted for ${fwEditService}` : `Restriction removed for ${fwEditService}`
		);
		fwEditService = null;
		fwEditSources = '';
		await loadFirewall();
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
		if (ch.type === 'smtp') Object.assign(payload, { host: ch.host, port: ch.port, username: ch.username, password: ch.password, from: ch.from, to: ch.to, tls: ch.tls });
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

	function addChannel() {
		if (!notifAddType || !nfName) return;
		const id = crypto.randomUUID().slice(0, 8);
		const ch: NotificationChannel = { id, name: nfName, enabled: true, type: notifAddType };
		if (notifAddType === 'smtp') Object.assign(ch, { host: nfHost, port: nfPort, username: nfUser, password: nfPass, from: nfFrom, to: nfTo, tls: nfTls });
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
		nfHost = ''; nfPort = 587; nfUser = ''; nfPass = ''; nfFrom = ''; nfTo = ''; nfTls = true;
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

	async function loadNut() {
		nutConfig = await client.call<NutConfig>('system.nut.config.get');
		if (nutConfig) {
			nutDriver = nutConfig.driver;
			nutPort = nutConfig.port;
			nutUpsName = nutConfig.ups_name;
			nutDescription = nutConfig.description;
			nutShutdownPercent = nutConfig.shutdown_on_battery_percent.toString();
			nutShutdownSeconds = nutConfig.shutdown_on_battery_seconds.toString();
			nutShutdownCommand = nutConfig.shutdown_command;
		}
		await refreshUpsStatus();
		startUpsPolling();
	}

	async function saveNut() {
		savingNut = true;
		await withToast(
			() => client.call('system.nut.config.update', {
				driver: nutDriver,
				port: nutPort,
				ups_name: nutUpsName,
				description: nutDescription || undefined,
				shutdown_on_battery_percent: parseInt(nutShutdownPercent) || undefined,
				shutdown_on_battery_seconds: parseInt(nutShutdownSeconds) || undefined,
				shutdown_command: nutShutdownCommand || undefined,
			}),
			'UPS configuration saved'
		);
		savingNut = false;
		await loadNut();
	}

	async function refreshUpsStatus() {
		try {
			upsStatus = await client.call<UpsStatus>('system.nut.status');
		} catch {
			upsStatus = null;
		}
	}

	function startUpsPolling() {
		stopUpsPolling();
		upsStatusInterval = setInterval(refreshUpsStatus, 5000);
	}

	function stopUpsPolling() {
		if (upsStatusInterval) {
			clearInterval(upsStatusInterval);
			upsStatusInterval = null;
		}
	}

	function upsStatusColor(s: string): string {
		if (s === 'OL' || s.startsWith('OL ')) return 'text-green-500';
		if (s.includes('OB')) return 'text-yellow-500';
		if (s.includes('LB')) return 'text-red-500';
		return 'text-muted-foreground';
	}

	function upsStatusLabel(s: string): string {
		return s.split(' ').map(code => {
			switch (code) {
				case 'OL': return 'Online';
				case 'OB': return 'On Battery';
				case 'LB': return 'Low Battery';
				case 'HB': return 'High Battery';
				case 'RB': return 'Replace Battery';
				case 'CHRG': return 'Charging';
				case 'DISCHRG': return 'Discharging';
				case 'BYPASS': return 'Bypass';
				case 'CAL': return 'Calibrating';
				case 'OFF': return 'Offline';
				case 'OVER': return 'Overloaded';
				case 'TRIM': return 'Trimming';
				case 'BOOST': return 'Boosting';
				case 'FSD': return 'Forced Shutdown';
				default: return code;
			}
		}).join(' / ');
	}

	function formatUpsRuntime(seconds: number): string {
		if (seconds >= 3600) {
			const h = Math.floor(seconds / 3600);
			const m = Math.floor((seconds % 3600) / 60);
			return `${h}h ${m}m`;
		}
		const m = Math.floor(seconds / 60);
		const s = seconds % 60;
		return `${m}m ${s}s`;
	}

	onDestroy(() => {
		stopUpsPolling();
	});
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
		onclick={() => switchTab('tls')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'tls'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>TLS</button>
	<button
		onclick={() => switchTab('vpn')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'vpn'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>VPN</button>
	<button
		onclick={() => switchTab('tuning')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'tuning'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>Tuning</button>
	<button
		onclick={() => switchTab('ups')}
		class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'ups'
			? 'border-b-2 border-primary text-foreground'
			: 'text-muted-foreground hover:text-foreground'}"
	>UPS</button>
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


			</div>

			<!-- Right column -->
			<div class="flex flex-col gap-6">

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

	<div class="max-w-3xl space-y-6">
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
											<div class="grid grid-cols-3 gap-2">
												<div>
													<label for="net-address" class="text-xs text-muted-foreground">Address</label>
													<input id="net-address" bind:value={netAddress} placeholder="192.168.1.100" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
												</div>
												<div>
													<label for="net-prefix" class="text-xs text-muted-foreground">Prefix</label>
													<input id="net-prefix" bind:value={netPrefix} type="number" min="1" max="32" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
												</div>
												<div>
													<label for="net-gateway" class="text-xs text-muted-foreground">Gateway</label>
													<input id="net-gateway" bind:value={netGateway} placeholder="192.168.1.1" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
												</div>
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
											<div class="grid grid-cols-2 gap-2">
												<div>
													<label for="net-ipv6-addr" class="text-xs text-muted-foreground">Address (CIDR)</label>
													<input id="net-ipv6-addr" bind:value={netIpv6Address} placeholder="fd00::1/64" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
												</div>
												<div>
													<label for="net-ipv6-gw" class="text-xs text-muted-foreground">Gateway</label>
													<input id="net-ipv6-gw" bind:value={netIpv6Gateway} placeholder="fd00::1" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
												</div>
											</div>
										{/if}
									</div>

									<!-- DNS -->
									<div>
										<label for="net-dns" class="text-xs text-muted-foreground">DNS Servers</label>
										<input id="net-dns" bind:value={netNameservers} placeholder="1.1.1.1, 8.8.8.8" oninput={() => netChanged = true} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm" />
									</div>

									{#if netChanged && !netDhcp}
										<p class="text-xs text-amber-500">Changing the IP will move your connection to the new address.</p>
									{/if}

									<Button size="sm" onclick={saveInterfaceConfig} disabled={savingNetwork || !netChanged}>
										{savingNetwork ? 'Applying\u2026' : 'Apply'}
									</Button>
								</div>
							{/if}
						</div>
					{/each}
				</div>
			{/if}
		</section>

		<!-- Bond / VLAN creation -->
		<section class="rounded-lg border border-border p-5">
			<div class="flex items-center gap-3 mb-4">
				<h2 class="text-base font-semibold">Advanced</h2>
				<Button size="xs" variant="secondary" onclick={() => { showBondForm = !showBondForm; showVlanForm = false; }}>{showBondForm ? 'Cancel' : '+ Bond'}</Button>
				<Button size="xs" variant="secondary" onclick={() => { showVlanForm = !showVlanForm; showBondForm = false; }}>{showVlanForm ? 'Cancel' : '+ VLAN'}</Button>
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
					<Button size="sm" onclick={createBond} disabled={bondMembers.length < 2}>Create Bond</Button>
				</div>
			{/if}

			{#if showVlanForm}
				<div class="rounded-lg border border-border bg-secondary/20 p-4 space-y-3">
					<div class="text-sm font-medium">Create VLAN Interface</div>
					<p class="text-xs text-muted-foreground">Tag traffic on a physical interface with a VLAN ID.</p>
					<div class="grid grid-cols-2 gap-3">
						<div>
							<label for="vlan-parent" class="text-xs text-muted-foreground">Parent Interface</label>
							<select id="vlan-parent" bind:value={vlanParent} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm">
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
					<Button size="sm" onclick={createVlan} disabled={!vlanParent}>Create VLAN</Button>
				</div>
			{/if}

			{#if !showBondForm && !showVlanForm}
				{#if network && (network.bonds?.length > 0 || network.vlans?.length > 0)}
					<div class="space-y-1 text-sm">
						{#each network.bonds as bond}
							<div class="flex items-center gap-2 rounded px-2 py-1">
								<Badge variant="outline" class="text-[0.6rem]">bond</Badge>
								<span class="font-mono">{bond.name}</span>
								<span class="text-xs text-muted-foreground">{bond.mode} · {bond.members.join(', ')}</span>
							</div>
						{/each}
						{#each network.vlans as vlan}
							<div class="flex items-center gap-2 rounded px-2 py-1">
								<Badge variant="outline" class="text-[0.6rem]">vlan</Badge>
								<span class="font-mono">{vlan.parent}.{vlan.vlan_id}</span>
							</div>
						{/each}
					</div>
				{:else}
					<p class="text-xs text-muted-foreground">No bonds or VLANs configured.</p>
				{/if}
			{/if}
		</section>

		<!-- Firewall status -->
		<section class="rounded-lg border border-border p-5">
			<h2 class="mb-4 text-base font-semibold">Firewall</h2>
			{#if !firewallStatus}
				<p class="text-sm text-muted-foreground">Loading...</p>
			{:else}
				<div class="space-y-1">
					{#each firewallStatus.rules as rule}
						<div>
							<button
								class="w-full text-left flex items-center gap-3 rounded px-3 py-2 text-sm transition-colors hover:bg-muted/30 {rule.active ? '' : 'opacity-40'}"
								onclick={() => startEditRestriction(rule.service)}
							>
								<span class="h-2 w-2 rounded-full shrink-0 {rule.active ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
								<span class="font-medium w-20">{rule.service}</span>
								<span class="font-mono text-xs text-muted-foreground">
									{#each [...new Set(rule.ports.map(p => `${p.port}/${p.transport}`))] as port}
										{port}{' '}
									{/each}
								</span>
								{#if firewallStatus.restrictions[rule.service]?.length}
									<span class="text-xs text-amber-400">
										{firewallStatus.restrictions[rule.service].length} source{firewallStatus.restrictions[rule.service].length !== 1 ? 's' : ''}
									</span>
								{/if}
								<span class="ml-auto text-xs {rule.active ? 'text-green-400' : 'text-muted-foreground'}">{rule.active ? 'Open' : 'Closed'}</span>
							</button>

							{#if fwEditService === rule.service}
								<div class="mx-3 mb-2 rounded-lg border border-border bg-secondary/20 p-3 space-y-2">
									<div class="text-xs font-medium">Restrict access to {rule.service}</div>
									<p class="text-xs text-muted-foreground">
										Enter allowed source IPs or CIDRs, comma-separated. Leave empty to allow all.
									</p>
									<input
										bind:value={fwEditSources}
										placeholder="e.g. 192.168.1.0/24, 10.0.0.5"
										class="w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm"
									/>
									<div class="flex gap-2">
										<Button size="xs" onclick={saveRestriction}>Save</Button>
										<Button size="xs" variant="secondary" onclick={() => fwEditService = null}>Cancel</Button>
									</div>
								</div>
							{/if}
						</div>
					{/each}
				</div>
				<p class="mt-3 text-xs text-muted-foreground">
					Ports open/close automatically with services. Click a service to restrict access by source IP.
				</p>
			{/if}
		</section>
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
						<label class="flex items-center gap-2 text-sm">
							<input type="checkbox" bind:checked={nfTls} /> Use TLS
						</label>
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
						<Button size="sm" variant="secondary" onclick={resetNotifForm}>Cancel</Button>
					</div>
				</div>
			{/if}
		</section>
	</div>

{:else if activeTab === 'tls'}

	<div class="max-w-xl">
		<section class="rounded-lg border border-border p-5">
			<h2 class="mb-2 text-base font-semibold">TLS Certificate</h2>
			<p class="mb-5 text-sm text-muted-foreground">
				NASty uses a self-signed certificate by default. Enable Let's Encrypt for a trusted certificate
				that browsers accept without warnings.
			</p>

			<div class="mb-4">
				<label class="flex items-center gap-2 text-sm cursor-pointer">
					<input
						type="checkbox"
						bind:checked={tlsAcmeEnabled}
						onchange={() => tlsChanged = true}
						class="rounded border-input"
					/>
					<span class="font-medium">Enable Let's Encrypt</span>
				</label>
				{#if tlsAcmeEnabled}
					<label class="flex items-center gap-2 text-xs text-muted-foreground cursor-pointer mt-2 ml-6">
						<input type="checkbox" bind:checked={tlsAcmeStaging} onchange={() => tlsChanged = true} class="rounded border-input" />
						Use staging environment (for testing, certs not trusted by browsers)
					</label>
				{/if}
			</div>

			{#if acmeStatus && acmeStatus.state !== 'idle'}
				<div class="mb-4 rounded border border-border p-3 text-xs">
					<div class="flex items-center gap-2">
						{#if acmeStatus.state === 'running'}
							<span class="inline-block h-2 w-2 rounded-full bg-yellow-500 animate-pulse"></span>
							<span class="text-yellow-500 font-medium">Provisioning...</span>
						{:else if acmeStatus.state === 'success'}
							<span class="inline-block h-2 w-2 rounded-full bg-green-500"></span>
							<span class="text-green-500 font-medium">Certificate active</span>
						{:else if acmeStatus.state === 'error'}
							<span class="inline-block h-2 w-2 rounded-full bg-red-500"></span>
							<span class="text-red-500 font-medium">Error</span>
						{/if}
						{#if acmeStatus.domain}
							<span class="text-muted-foreground">({acmeStatus.domain})</span>
						{/if}
					</div>
					{#if acmeStatus.message}
						<p class="mt-1 text-muted-foreground">{acmeStatus.message}</p>
					{/if}
				</div>
			{/if}

			{#if tlsAcmeEnabled}
				<div class="mb-4">
					<label for="tls-domain" class="mb-1 block text-xs text-muted-foreground">Domain Name</label>
					<input
						id="tls-domain"
						type="text"
						bind:value={tlsDomain}
						oninput={() => tlsChanged = true}
						class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
						placeholder="nasty.example.com"
					/>
					<span class="mt-1 block text-xs text-muted-foreground">Must resolve to this machine's public IP.</span>
				</div>

				<div class="mb-4">
					<label for="tls-email" class="mb-1 block text-xs text-muted-foreground">Email</label>
					<input
						id="tls-email"
						type="email"
						bind:value={tlsAcmeEmail}
						oninput={() => tlsChanged = true}
						class="w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
						placeholder="admin@example.com"
					/>
					<span class="mt-1 block text-xs text-muted-foreground">Let's Encrypt sends expiry warnings here.</span>
				</div>

				<div class="mb-4">
					<span class="mb-1 block text-xs text-muted-foreground">Challenge Type</span>
					<div class="flex w-fit rounded-md border border-border text-sm">
						<button
							onclick={() => { tlsChallengeType = 'tls-alpn'; tlsChanged = true; }}
							class="rounded-l-md px-4 py-1.5 font-medium transition-colors {tlsChallengeType === 'tls-alpn' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
						>TLS (port 443)</button>
						<button
							onclick={() => { tlsChallengeType = 'dns'; tlsChanged = true; }}
							class="rounded-r-md px-4 py-1.5 font-medium transition-colors {tlsChallengeType === 'dns' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
						>DNS</button>
					</div>
				</div>

				{#if tlsChallengeType === 'tls-alpn'}
					<div class="mb-4 rounded-lg border border-blue-800 bg-blue-950 px-4 py-3 text-xs text-blue-200">
						The TLS-ALPN-01 challenge verifies domain ownership over port 443. No additional ports needed,
						but port 443 must be reachable from the internet.
					</div>
				{:else}
					<div class="mb-4">
						<label for="tls-dns-provider" class="mb-1 block text-xs text-muted-foreground">DNS Provider</label>
						<select
							id="tls-dns-provider"
							bind:value={tlsDnsProvider}
							onchange={() => tlsChanged = true}
							class="w-full rounded-md border border-input bg-transparent px-3 py-1.5 text-sm"
						>
							<option value="">Select provider...</option>
							{#each popularDnsProviders as p}
								<option value={p.code}>{p.name}</option>
							{/each}
							<option disabled>───────────</option>
							<option value="_custom">Other (enter code manually)</option>
						</select>
						{#if tlsDnsProvider === '_custom'}
							<input
								type="text"
								bind:value={tlsDnsProvider}
								oninput={() => tlsChanged = true}
								class="mt-2 w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
								placeholder="provider code (e.g. inwx, gandi)"
							/>
						{/if}
						<span class="mt-1 block text-xs text-muted-foreground">
							See <a href="https://go-acme.github.io/lego/dns/" target="_blank" class="text-blue-400 hover:underline">lego DNS providers</a> for the full list and required credentials.
						</span>
					</div>

					<div class="mb-4">
						<label for="tls-dns-creds" class="mb-1 block text-xs text-muted-foreground">API Credentials</label>
						<textarea
							id="tls-dns-creds"
							bind:value={tlsDnsCredentials}
							oninput={() => tlsChanged = true}
							rows={4}
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-xs focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder={"CLOUDFLARE_DNS_API_TOKEN=xxxxx\nCLOUDFLARE_ZONE_API_TOKEN=xxxxx"}
						></textarea>
						<span class="mt-1 block text-xs text-muted-foreground">
							One KEY=VALUE per line. These are passed as environment variables to the ACME client.
							No inbound ports needed — verification happens via DNS records.
						</span>
					</div>
				{/if}

				{#if !tlsDomain.trim() || !tlsAcmeEmail.trim() || (tlsChallengeType === 'dns' && !tlsDnsProvider)}
					<p class="mb-3 text-xs text-destructive">
						{#if !tlsDomain.trim()}Domain is required.
						{:else if !tlsAcmeEmail.trim()}Email is required.
						{:else}DNS provider is required.
						{/if}
					</p>
				{/if}

				{/if}

			<Button size="sm" onclick={saveTls} disabled={savingTls || !tlsChanged}>
				{savingTls ? 'Saving…' : 'Save'}
			</Button>
		</section>
	</div>

{:else if activeTab === 'tuning'}

	{#if !tuning}
		<p class="text-muted-foreground">Loading...</p>
	{:else}
		<div class="grid grid-cols-1 gap-6 xl:grid-cols-2">

			<!-- NFS -->
			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">NFS Server</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
					<div>
						<label for="nfs-threads" class="mb-1 block text-xs text-muted-foreground">Threads</label>
						<input id="nfs-threads" type="number" min="1" bind:value={tNfsThreads}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Kernel nfsd threads (default: 8). Increase under heavy concurrent load.</p>
					</div>
					<div>
						<label for="nfs-lease" class="mb-1 block text-xs text-muted-foreground">Lease time (s)</label>
						<input id="nfs-lease" type="number" min="1" bind:value={tNfsLeaseTime}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">NFSv4 lease window. Clients must renew state within this period.</p>
					</div>
					<div>
						<label for="nfs-grace" class="mb-1 block text-xs text-muted-foreground">Grace time (s)</label>
						<input id="nfs-grace" type="number" min="1" bind:value={tNfsGraceTime}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Grace period after restart for clients to reclaim locks.</p>
					</div>
				</div>
			</section>

			<!-- SMB -->
			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">SMB Server</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
					<div>
						<label for="smb-maxconn" class="mb-1 block text-xs text-muted-foreground">Max connections</label>
						<input id="smb-maxconn" type="number" min="0" bind:value={tSmbMaxConnections}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">0 = unlimited.</p>
					</div>
					<div>
						<label for="smb-deadtime" class="mb-1 block text-xs text-muted-foreground">Dead time (min)</label>
						<input id="smb-deadtime" type="number" min="0" bind:value={tSmbDeadtime}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Disconnect idle clients after N minutes. 0 = never.</p>
					</div>
					<div class="sm:col-span-3">
						<label for="smb-sockopts" class="mb-1 block text-xs text-muted-foreground">Socket options</label>
						<input id="smb-sockopts" type="text" bind:value={tSmbSocketOptions} placeholder="SO_RCVBUF=131072 SO_SNDBUF=131072"
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">TCP socket tuning. Leave empty for kernel defaults.</p>
					</div>
				</div>
			</section>

			<!-- iSCSI -->
			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">iSCSI Target</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
					<div>
						<label for="iscsi-cmdsn" class="mb-1 block text-xs text-muted-foreground">Command queue depth</label>
						<input id="iscsi-cmdsn" type="number" min="1" bind:value={tIscsiCmdsnDepth}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Default CmdSN depth per session (default: 64).</p>
					</div>
					<div>
						<label for="iscsi-timeout" class="mb-1 block text-xs text-muted-foreground">Login timeout (s)</label>
						<input id="iscsi-timeout" type="number" min="1" bind:value={tIscsiLoginTimeout}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Seconds before login attempt times out.</p>
					</div>
				</div>
			</section>

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

{:else if activeTab === 'ups'}

	{#if !nutConfig}
		<p class="text-muted-foreground">Loading...</p>
	{:else}
		<div class="flex flex-col gap-6">
			{#if upsStatus?.available}
				<section class="rounded-lg border border-border p-5">
					<h3 class="mb-4 text-sm font-semibold">UPS Status</h3>
					<div class="grid grid-cols-2 gap-4 sm:grid-cols-4">
						<div>
							<p class="text-xs text-muted-foreground">Status</p>
							<p class="text-lg font-semibold {upsStatusColor(upsStatus.status)}">
								{upsStatusLabel(upsStatus.status)}
							</p>
						</div>
						{#if upsStatus.battery_charge != null}
							<div>
								<p class="text-xs text-muted-foreground">Battery</p>
								<p class="text-lg font-semibold">{upsStatus.battery_charge.toFixed(0)}%</p>
							</div>
						{/if}
						{#if upsStatus.battery_runtime != null}
							<div>
								<p class="text-xs text-muted-foreground">Runtime</p>
								<p class="text-lg font-semibold">{formatUpsRuntime(upsStatus.battery_runtime)}</p>
							</div>
						{/if}
						{#if upsStatus.ups_load != null}
							<div>
								<p class="text-xs text-muted-foreground">Load</p>
								<p class="text-lg font-semibold">{upsStatus.ups_load.toFixed(0)}%</p>
							</div>
						{/if}
						{#if upsStatus.input_voltage != null}
							<div>
								<p class="text-xs text-muted-foreground">Input Voltage</p>
								<p class="text-sm">{upsStatus.input_voltage.toFixed(1)} V</p>
							</div>
						{/if}
						{#if upsStatus.output_voltage != null}
							<div>
								<p class="text-xs text-muted-foreground">Output Voltage</p>
								<p class="text-sm">{upsStatus.output_voltage.toFixed(1)} V</p>
							</div>
						{/if}
						{#if upsStatus.ups_model}
							<div>
								<p class="text-xs text-muted-foreground">Model</p>
								<p class="text-sm">{upsStatus.ups_model}</p>
							</div>
						{/if}
						{#if upsStatus.ups_serial}
							<div>
								<p class="text-xs text-muted-foreground">Serial</p>
								<p class="text-sm font-mono">{upsStatus.ups_serial}</p>
							</div>
						{/if}
					</div>
				</section>
			{/if}

			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">UPS Hardware</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
					<div>
						<label for="nut-driver" class="mb-1 block text-xs text-muted-foreground">Driver</label>
						<select id="nut-driver" bind:value={nutDriver}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm">
							<option value="usbhid-ups">usbhid-ups (USB HID)</option>
							<option value="blazer_usb">blazer_usb (Megatec/Q1 USB)</option>
							<option value="nutdrv_qx">nutdrv_qx (Q* protocol USB)</option>
							<option value="snmp-ups">snmp-ups (SNMP)</option>
							<option value="apcsmart">apcsmart (APC Smart serial)</option>
						</select>
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">NUT driver for your UPS hardware.</p>
					</div>
					<div>
						<label for="nut-port" class="mb-1 block text-xs text-muted-foreground">Port</label>
						<input id="nut-port" type="text" bind:value={nutPort}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">"auto" for USB, or a device path like /dev/ttyS0.</p>
					</div>
					<div>
						<label for="nut-name" class="mb-1 block text-xs text-muted-foreground">UPS Name</label>
						<input id="nut-name" type="text" bind:value={nutUpsName}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Identifier for upsc (e.g. "ups").</p>
					</div>
					<div class="sm:col-span-2 lg:col-span-3">
						<label for="nut-desc" class="mb-1 block text-xs text-muted-foreground">Description</label>
						<input id="nut-desc" type="text" bind:value={nutDescription} placeholder="My UPS"
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
					</div>
				</div>
			</section>

			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">Shutdown Policy</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
					<div>
						<label for="nut-pct" class="mb-1 block text-xs text-muted-foreground">Battery threshold (%)</label>
						<input id="nut-pct" type="number" min="0" max="100" bind:value={nutShutdownPercent}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Shutdown when battery drops below this.</p>
					</div>
					<div>
						<label for="nut-secs" class="mb-1 block text-xs text-muted-foreground">On-battery timeout (s)</label>
						<input id="nut-secs" type="number" min="0" bind:value={nutShutdownSeconds}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Shutdown after N seconds on battery. 0 = disabled.</p>
					</div>
					<div>
						<label for="nut-cmd" class="mb-1 block text-xs text-muted-foreground">Shutdown command</label>
						<input id="nut-cmd" type="text" bind:value={nutShutdownCommand}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
					</div>
				</div>
			</section>

			<div>
				<Button onclick={saveNut} disabled={savingNut}>
					{savingNut ? 'Saving...' : 'Save UPS Configuration'}
				</Button>
				<span class="ml-2 text-xs text-muted-foreground">If NUT is running, services will be restarted automatically.</span>
			</div>
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

{:else if activeTab === 'vpn'}

	<div class="space-y-6">
		<div>
			<h3 class="text-lg font-semibold mb-1">Tailscale VPN</h3>
			<p class="text-sm text-muted-foreground">Connect your NASty to a Tailscale network for secure remote access.</p>
		</div>

		{#if !tsStatus}
			<p class="text-muted-foreground">Loading...</p>
		{:else if tsStatus.connected}
			<!-- Connected state -->
			<div class="rounded-lg border border-green-500/30 bg-green-500/5 p-4 space-y-2">
				<div class="flex items-center gap-2">
					<span class="w-2 h-2 rounded-full bg-green-500"></span>
					<span class="text-sm font-medium text-green-500">Connected</span>
				</div>
				{#if tsStatus.ip}
					<div class="text-sm"><span class="text-muted-foreground">Tailscale IP:</span> <span class="font-mono">{tsStatus.ip}</span></div>
				{/if}
				{#if tsStatus.hostname}
					<div class="text-sm"><span class="text-muted-foreground">Hostname:</span> {tsStatus.hostname}</div>
				{/if}
				{#if tsStatus.version}
					<div class="text-sm"><span class="text-muted-foreground">Version:</span> {tsStatus.version}</div>
				{/if}
			</div>

			<Button
				disabled={tsLoading}
				variant="destructive"
				onclick={async () => {
					tsLoading = true;
					const result = await withToast(
						() => client.call('system.tailscale.disconnect'),
						'Tailscale disconnected'
					);
					if (result) {
						tsStatus = result as TailscaleStatus;
						tsAuthKey = '';
					}
					tsLoading = false;
				}}
			>
				{tsLoading ? 'Disconnecting...' : 'Disconnect'}
			</Button>
		{:else}
			<!-- Disconnected state -->
			<div class="rounded-lg border p-4">
				<div class="flex items-center gap-2">
					<span class="w-2 h-2 rounded-full bg-muted-foreground"></span>
					<span class="text-sm text-muted-foreground">Not connected</span>
				</div>
			</div>

			<div class="space-y-4">
				{#if tsStatus?.has_auth_key}
					<p class="text-xs text-muted-foreground">A stored auth key is available. Click Reconnect to use it, or enter a new key below.</p>
					<Button
						disabled={tsLoading}
						onclick={async () => {
							tsLoading = true;
							const result = await withToast(
								() => client.call('system.tailscale.connect', { auth_key: '' }),
								'Tailscale connected'
							);
							if (result) tsStatus = result as TailscaleStatus;
							tsLoading = false;
						}}
					>
						{tsLoading ? 'Connecting...' : 'Reconnect'}
					</Button>
				{/if}

				<div>
					<label for="ts-authkey" class="block text-sm font-medium mb-1">{tsStatus?.has_auth_key ? 'New Auth Key (optional)' : 'Auth Key'}</label>
					<input
						id="ts-authkey"
						type="password"
						bind:value={tsAuthKey}
						placeholder="tskey-auth-..."
						class="w-full max-w-md rounded-md border bg-background px-3 py-2 text-sm"
					/>
					<p class="text-xs text-muted-foreground mt-1">
						Generate at <a href="https://login.tailscale.com/admin/settings/keys" target="_blank" class="underline">Tailscale admin console</a>. Use a reusable key for persistent connections.
					</p>
				</div>

				<Button
					disabled={!tsAuthKey || tsLoading}
					onclick={async () => {
						tsLoading = true;
						const result = await withToast(
							() => client.call('system.tailscale.connect', { auth_key: tsAuthKey }),
							'Tailscale connected'
						);
						if (result) {
							tsStatus = result as TailscaleStatus;
							tsAuthKey = '';
						}
						tsLoading = false;
					}}
				>
					{tsLoading ? 'Connecting...' : 'Connect with new key'}
				</Button>
			</div>
		{/if}
	</div>

{/if}
