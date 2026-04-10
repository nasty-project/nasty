<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';
	import type { Settings, SystemInfo, NetworkConfig, TuningConfig } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Copy, Check, ChevronDown, ChevronRight } from '@lucide/svelte';

	let activeTab: 'general' | 'tls' | 'vpn' | 'metrics' | 'tuning' = $state('general');

	// ── General tab state ───────────────────────────────────
	let settings: Settings | null = $state(null);
	let info: SystemInfo | null = $state(null);
	let timezones: string[] = $state([]);
	let saving = $state(false);
	let savingHostname = $state(false);
	let hostnameInput = $state('');

	// Network
	let network: NetworkConfig | null = $state(null);
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

	// GC config
	let gcKeep = $state(20);
	let gcMaxAge = $state(0);
	let savingGc = $state(false);

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
			[settings, info, timezones, network] = await Promise.all([
				client.call<Settings>('system.settings.get'),
				client.call<SystemInfo>('system.info'),
				client.call<string[]>('system.settings.timezones'),
				client.call<NetworkConfig>('system.network.get'),
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

			// Load GC config
			try {
				const gc = await client.call<{ keep_generations: number; max_age_days: number }>('system.gc.get');
				gcKeep = gc.keep_generations;
				gcMaxAge = gc.max_age_days;
			} catch { /* ignore */ }

			// Load Tailscale status
			try {
				tsStatus = await client.call<TailscaleStatus>('system.tailscale.get');
			} catch { /* ignore — tailscale module may not be enabled */ }
		});
	});

	function syncNetworkForm() {
		if (!network) return;
		netDhcp = network.dhcp;
		netAddress = network.address ?? '';
		netPrefix = String(network.prefix_length ?? 24);
		netGateway = network.gateway ?? '';
		netNameservers = network.nameservers.join(', ');
		netChanged = false;
	}

	async function saveGc() {
		savingGc = true;
		await withToast(
			() => client.call('system.gc.set', { keep_generations: gcKeep, max_age_days: gcMaxAge }),
			'Cleanup settings saved'
		);
		savingGc = false;
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
		const payload: Partial<NetworkConfig> = { dhcp: netDhcp, nameservers };
		if (!netDhcp) {
			payload.address = netAddress.trim() || null;
			payload.prefix_length = parseInt(netPrefix) || null;
			payload.gateway = netGateway.trim() || null;
		}
		await withToast(
			() => client.call('system.network.update', payload),
			'Network configuration applied'
		);
		network = await client.call<NetworkConfig>('system.network.get');
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

	function switchTab(tab: 'general' | 'tls' | 'vpn' | 'metrics' | 'tuning') {
		activeTab = tab;
		if (tab === 'metrics' && !metricsText) {
			loadMetrics();
		}
		if (tab === 'tuning' && !tuning) {
			loadTuning();
		}
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

				<!-- System Cleanup -->
				<section class="rounded-lg border border-border p-5">
					<h2 class="mb-4 text-base font-semibold">System Cleanup</h2>
					<p class="mb-3 text-xs text-muted-foreground">
						Old NixOS generations are cleaned up before each update to free disk space.
						The booted generation is always protected — if it falls outside the keep range, the range is automatically expanded to include it.
						You can roll back to any kept generation via the bootloader.
					</p>
					<div class="flex flex-wrap gap-4 mb-3">
						<div>
							<label for="gc-keep" class="mb-1 block text-xs text-muted-foreground">Keep last N generations</label>
							<input id="gc-keep" type="number" min="1" max="50" bind:value={gcKeep}
								class="h-9 w-24 rounded-md border border-input bg-transparent px-3 text-sm" />
						</div>
						<div>
							<label for="gc-age" class="mb-1 block text-xs text-muted-foreground">Max age (days, 0 = no limit)</label>
							<input id="gc-age" type="number" min="0" max="365" bind:value={gcMaxAge}
								class="h-9 w-24 rounded-md border border-input bg-transparent px-3 text-sm" />
						</div>
					</div>
					<p class="mb-3 text-xs text-muted-foreground">
						{#if gcMaxAge > 0}
							Generations older than {gcMaxAge} days will be deleted, but at least {gcKeep} will always be kept. The booted generation is never removed.
						{:else}
							The {gcKeep} most recent generations will be kept. Older ones are deleted before each update. The booted generation is never removed even if outside this range.
						{/if}
					</p>
					<Button size="sm" onclick={saveGc} disabled={savingGc || gcKeep < 1}>
						{savingGc ? 'Saving…' : 'Save'}
					</Button>
				</section>

			</div>

			<!-- Right column: Network -->
			{#if network}
			<section class="rounded-lg border border-border p-5">
				<h2 class="mb-4 text-base font-semibold">Network</h2>

				{#if network.live_addresses.length > 0}
					<div class="mb-4 flex items-start justify-between gap-4">
						<span class="shrink-0 text-sm text-muted-foreground">Active Address</span>
						<div class="text-right">
							<div class="text-sm font-medium font-mono">
								{network.live_addresses.join(', ')}
								{#if network.live_gateway}
									<span class="ml-1 text-muted-foreground">via {network.live_gateway}</span>
								{/if}
							</div>
							{#if network.interface}<div class="text-xs text-muted-foreground">{network.interface}</div>{/if}
						</div>
					</div>
				{/if}

				<div class="mb-4">
					<div class="mb-2 text-sm text-muted-foreground">Mode</div>
					<div class="flex w-fit rounded-md border border-border text-sm">
						<button
							onclick={() => { netDhcp = true; netChanged = true; }}
							class="rounded-l-md px-4 py-1.5 font-medium transition-colors {netDhcp ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
						>DHCP</button>
						<button
							onclick={() => { netDhcp = false; netChanged = true; }}
							class="rounded-r-md px-4 py-1.5 font-medium transition-colors {!netDhcp ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
						>Static</button>
					</div>
				</div>

				<div class="mb-4 grid grid-cols-2 gap-3">
					<div>
						<label for="net-address" class="mb-1 block text-xs text-muted-foreground">IP Address</label>
						<input
							id="net-address"
							type="text"
							value={netDhcp ? (network.live_addresses[0]?.split('/')[0] ?? '') : netAddress}
							oninput={(e) => { if (!netDhcp) { netAddress = (e.target as HTMLInputElement).value; netChanged = true; } }}
							disabled={netDhcp}
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring disabled:opacity-50 disabled:cursor-not-allowed"
							placeholder="192.168.1.100"
						/>
					</div>
					<div>
						<label for="net-prefix" class="mb-1 block text-xs text-muted-foreground">Prefix Length</label>
						<input
							id="net-prefix"
							type="number"
							min="1"
							max="32"
							value={netDhcp ? (network.live_addresses[0]?.split('/')[1] ?? '') : netPrefix}
							oninput={(e) => { if (!netDhcp) { netPrefix = (e.target as HTMLInputElement).value; netChanged = true; } }}
							disabled={netDhcp}
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring disabled:opacity-50 disabled:cursor-not-allowed"
							placeholder="24"
						/>
					</div>
					<div>
						<label for="net-gateway" class="mb-1 block text-xs text-muted-foreground">Gateway</label>
						<input
							id="net-gateway"
							type="text"
							value={netDhcp ? (network.live_gateway ?? '') : netGateway}
							oninput={(e) => { if (!netDhcp) { netGateway = (e.target as HTMLInputElement).value; netChanged = true; } }}
							disabled={netDhcp}
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring disabled:opacity-50 disabled:cursor-not-allowed"
							placeholder="192.168.1.1"
						/>
					</div>
					<div>
						<label for="net-dns" class="mb-1 block text-xs text-muted-foreground">DNS Servers</label>
						<input
							id="net-dns"
							type="text"
							bind:value={netNameservers}
							oninput={() => { if (!netDhcp) netChanged = true; }}
							disabled={netDhcp}
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring disabled:opacity-50 disabled:cursor-not-allowed"
							placeholder="1.1.1.1, 8.8.8.8"
						/>
					</div>
				</div>

				{#if netChanged && !netDhcp}
					<p class="mb-3 text-xs text-amber-500">
						Changing the static IP will move your connection to the new address.
						If it differs from the current one, reconnect to continue.
					</p>
				{/if}

				<Button size="sm" onclick={saveNetwork} disabled={savingNetwork || !netChanged}>
					{savingNetwork ? 'Applying…' : 'Apply Network'}
				</Button>
			</section>
			{/if}

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
	{/if}

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
				<div>
					<label for="ts-authkey" class="block text-sm font-medium mb-1">Auth Key</label>
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
					{tsLoading ? 'Connecting...' : 'Connect'}
				</Button>
			</div>
		{/if}
	</div>

{/if}
