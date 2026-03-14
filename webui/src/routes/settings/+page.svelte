<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { Settings, SystemInfo } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	let settings: Settings | null = $state(null);
	let info: SystemInfo | null = $state(null);
	let timezones: string[] = $state([]);
	let saving = $state(false);

	const client = getClient();

	onMount(async () => {
		await withToast(async () => {
			[settings, info, timezones] = await Promise.all([
				client.call<Settings>('system.settings.get'),
				client.call<SystemInfo>('system.info'),
				client.call<string[]>('system.settings.timezones'),
			]);
		});
	});

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

	async function toggleSmart() {
		if (!settings) return;
		await withToast(
			() => client.call('system.settings.update', { smart_enabled: !settings!.smart_enabled }),
			`S.M.A.R.T. monitoring ${settings!.smart_enabled ? 'disabled' : 'enabled'}`
		);
		settings = await client.call<Settings>('system.settings.get');
	}
</script>

<h1 class="mb-6 text-2xl font-bold">Settings</h1>

{#if !settings}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<div class="max-w-xl space-y-8">

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

			<Button onclick={saveTimezone} disabled={saving}>
				{saving ? 'Saving...' : 'Apply Timezone'}
			</Button>
		</section>

		<!-- Monitoring -->
		<section class="rounded-lg border border-border p-6">
			<h2 class="mb-4 text-lg font-semibold">Monitoring</h2>

			<div class="flex items-center justify-between">
				<div>
					<div class="text-sm font-medium">S.M.A.R.T. Disk Monitoring</div>
					<div class="text-xs text-muted-foreground">Enables disk health checks and temperature reporting</div>
				</div>
				<Button
					variant={settings.smart_enabled ? 'secondary' : 'default'}
					size="sm"
					onclick={toggleSmart}
				>
					{settings.smart_enabled ? 'Disable' : 'Enable'}
				</Button>
			</div>
		</section>

	</div>
{/if}
