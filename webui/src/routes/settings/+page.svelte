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
	let savingHostname = $state(false);
	let hostnameInput = $state('');

	const client = getClient();

	onMount(async () => {
		await withToast(async () => {
			[settings, info, timezones] = await Promise.all([
				client.call<Settings>('system.settings.get'),
				client.call<SystemInfo>('system.info'),
				client.call<string[]>('system.settings.timezones'),
			]);
			hostnameInput = settings.hostname ?? info.hostname;
		});
	});

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

	</div>
{/if}
