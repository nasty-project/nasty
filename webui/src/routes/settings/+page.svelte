<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { Settings, SystemInfo, NetworkConfig } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Copy, Check, ChevronDown, ChevronRight } from '@lucide/svelte';

	let activeTab: 'general' | 'metrics' = $state('general');

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
					title = 'bcachefs — Pool';
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
			syncNetworkForm();
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

	async function saveHostname() {
		savingHostname = true;
		await withToast(
			() => client.call('system.settings.update', { hostname: hostnameInput }),
			'Hostname updated'
		);
		info = await client.call<SystemInfo>('system.info');
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

	function switchTab(tab: 'general' | 'metrics') {
		activeTab = tab;
		if (tab === 'metrics' && !metricsText) {
			loadMetrics();
		}
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
							<div class="text-xs text-muted-foreground">{network.interface || '—'}</div>
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

		</div>
	{/if}

{:else}

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
