<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { Settings, SystemInfo, NetworkConfig } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

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
</script>


{#if !settings}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<div class="max-w-xl space-y-8">

		<!-- System -->
		<section class="rounded-lg border border-border p-6">
			<h2 class="mb-4 text-lg font-semibold">System</h2>

			<div class="mb-4">
				<div class="mb-1 text-sm text-muted-foreground">Current Hostname</div>
				<div class="text-sm font-medium">{info?.hostname ?? '—'}</div>
			</div>

			<div class="mb-4">
				<label for="hostname" class="mb-1 block text-sm text-muted-foreground">Set Hostname</label>
				<input
					id="hostname"
					type="text"
					bind:value={hostnameInput}
					class="w-full rounded-md border border-input bg-background px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
					placeholder="nasty"
				/>
			</div>

			<Button size="sm" onclick={saveHostname} disabled={savingHostname}>
				{savingHostname ? 'Saving...' : 'Apply Hostname'}
			</Button>
		</section>

		<!-- Date & Time -->
		<section class="rounded-lg border border-border p-6">
			<h2 class="mb-4 text-lg font-semibold">Date & Time</h2>

			<div class="mb-4">
				<div class="mb-1 text-sm text-muted-foreground">NTP Synchronization</div>
				<div class="flex items-center gap-2">
					<span class="inline-block h-2 w-2 rounded-full {info?.ntp_synced ? 'bg-green-400' : 'bg-yellow-400'}"></span>
					<span class="text-sm">{info?.ntp_synced ? 'Synchronized' : 'Not synchronized'}</span>
				</div>
			</div>

			<div class="mb-4">
				<div class="mb-1 text-sm text-muted-foreground">Active Timezone</div>
				<div class="text-sm font-medium">{info?.timezone ?? '—'}</div>
			</div>

			<div class="mb-4">
				<label for="timezone" class="mb-1 block text-sm text-muted-foreground">Set Timezone</label>
				<select
					id="timezone"
					bind:value={settings.timezone}
					class="w-full rounded-md border border-input bg-background px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
				>
					{#each timezones as tz}
						<option value={tz}>{tz}</option>
					{/each}
				</select>
			</div>

			<Button size="sm" onclick={saveTimezone} disabled={saving}>
				{saving ? 'Saving...' : 'Apply Timezone'}
			</Button>
		</section>

		<!-- Network -->
		{#if network}
		<section class="rounded-lg border border-border p-6">
			<h2 class="mb-4 text-lg font-semibold">Network</h2>

			{#if network.live_addresses.length > 0}
				<div class="mb-4">
					<div class="mb-1 text-sm text-muted-foreground">Active Address</div>
					<div class="text-sm font-medium font-mono">
						{network.live_addresses.join(', ')}
						{#if network.live_gateway}
							<span class="ml-2 text-muted-foreground">via {network.live_gateway}</span>
						{/if}
					</div>
					<div class="mt-0.5 text-xs text-muted-foreground">Interface: {network.interface || '\u2014'}</div>
				</div>
			{/if}

			<div class="mb-4">
				<div class="mb-2 text-sm text-muted-foreground">Mode</div>
				<div class="flex w-fit rounded-md border border-border">
					<button
						onclick={() => { netDhcp = true; netChanged = true; }}
						class="rounded-l-md px-4 py-1.5 text-sm font-medium transition-colors {netDhcp ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
					>DHCP</button>
					<button
						onclick={() => { netDhcp = false; netChanged = true; }}
						class="rounded-r-md px-4 py-1.5 text-sm font-medium transition-colors {!netDhcp ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
					>Static</button>
				</div>
			</div>

			{#if !netDhcp}
				<div class="mb-4 grid grid-cols-1 gap-3 sm:grid-cols-2">
					<div>
						<label for="net-address" class="mb-1 block text-sm text-muted-foreground">IP Address</label>
						<input
							id="net-address"
							type="text"
							bind:value={netAddress}
							oninput={() => { netChanged = true; }}
							class="w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder="192.168.1.100"
						/>
					</div>
					<div>
						<label for="net-prefix" class="mb-1 block text-sm text-muted-foreground">Prefix Length</label>
						<input
							id="net-prefix"
							type="number"
							min="1"
							max="32"
							bind:value={netPrefix}
							oninput={() => { netChanged = true; }}
							class="w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder="24"
						/>
					</div>
					<div>
						<label for="net-gateway" class="mb-1 block text-sm text-muted-foreground">Gateway</label>
						<input
							id="net-gateway"
							type="text"
							bind:value={netGateway}
							oninput={() => { netChanged = true; }}
							class="w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder="192.168.1.1"
						/>
					</div>
					<div>
						<label for="net-dns" class="mb-1 block text-sm text-muted-foreground">DNS Servers</label>
						<input
							id="net-dns"
							type="text"
							bind:value={netNameservers}
							oninput={() => { netChanged = true; }}
							class="w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder="1.1.1.1, 8.8.8.8"
						/>
					</div>
				</div>
			{/if}

			{#if netChanged && !netDhcp}
				<p class="mb-3 text-xs text-amber-500">
					Changing the static IP will move your connection to the new address.
					If it differs from the current one, reconnect to continue.
				</p>
			{/if}

			<Button size="sm" onclick={saveNetwork} disabled={savingNetwork || !netChanged}>
				{savingNetwork ? 'Applying...' : 'Apply Network'}
			</Button>
		</section>
		{/if}

	</div>
{/if}
