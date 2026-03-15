<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { DiskHealth, Settings } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';

	let disks: DiskHealth[] = $state([]);
	let settings: Settings | null = $state(null);
	let loading = $state(true);
	let expandedDisk = $state<string | null>(null);

	const client = getClient();

	onMount(async () => {
		await loadSettings();
		await refresh();
		loading = false;
	});

	async function loadSettings() {
		await withToast(async () => {
			settings = await client.call<Settings>('system.settings.get');
		});
	}

	async function toggleSmart() {
		if (!settings) return;
		const ok = await withToast(
			() => client.call<Settings>('system.settings.update', { smart_enabled: !settings!.smart_enabled }),
			`SMART monitoring ${settings.smart_enabled ? 'disabled' : 'enabled'}`
		);
		if (ok !== undefined) {
			settings = ok;
			await refresh();
		}
	}

	async function refresh() {
		await withToast(async () => {
			disks = await client.call<DiskHealth[]>('system.disks');
		});
	}

	function formatHours(hours: number): string {
		const days = Math.floor(hours / 24);
		const years = Math.floor(days / 365);
		if (years > 0) return `${years}y ${days % 365}d`;
		if (days > 0) return `${days}d ${hours % 24}h`;
		return `${hours}h`;
	}

	const criticalIds = new Set([5, 10, 187, 188, 196, 197, 198]);
</script>


{#if settings}
	<Card class="mb-4">
		<CardContent class="flex items-center gap-4 py-3">
			<Badge variant={settings.smart_enabled ? 'default' : 'secondary'}>
				{settings.smart_enabled ? 'Enabled' : 'Disabled'}
			</Badge>
			<span class="text-sm text-muted-foreground">
				SMART disk health monitoring
			</span>
			<Button variant="secondary" size="xs" onclick={toggleSmart}>
				{settings.smart_enabled ? 'Disable' : 'Enable'}
			</Button>
		</CardContent>
	</Card>
{/if}

<div class="mb-4">
	<Button size="sm" onclick={refresh}>Refresh</Button>
</div>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if !settings?.smart_enabled}
	<p class="text-muted-foreground">SMART monitoring is disabled. Enable it above to see disk health data.</p>
{:else if disks.length === 0}
	<p class="text-muted-foreground">No disks detected or smartctl not available.</p>
{:else}
	{#each disks as disk}
		<Card class="mb-4 {!disk.health_passed ? 'border-red-900' : ''}">
			<CardContent class="pt-5">
				<div class="mb-4 flex items-center gap-4">
					<span class="rounded px-2.5 py-1 text-xs font-bold {disk.health_passed ? 'bg-green-950 text-green-400' : 'bg-red-950 text-red-400'}">
						{disk.smart_status}
					</span>
					<div class="flex flex-1 items-baseline gap-3">
						<strong class="font-mono">{disk.device}</strong>
						<span class="text-sm text-muted-foreground">{disk.model}</span>
					</div>
					<Button variant="secondary" size="xs" onclick={() => expandedDisk = expandedDisk === disk.device ? null : disk.device}>
						{expandedDisk === disk.device ? 'Hide' : 'Details'}
					</Button>
				</div>

				<div class="flex flex-wrap gap-6">
					<div class="flex flex-col">
						<span class="text-[0.7rem] uppercase text-muted-foreground">Capacity</span>
						<span class="text-sm font-semibold">{formatBytes(disk.capacity_bytes)}</span>
					</div>
					<div class="flex flex-col">
						<span class="text-[0.7rem] uppercase text-muted-foreground">Serial</span>
						<span class="font-mono text-sm font-semibold">{disk.serial}</span>
					</div>
					<div class="flex flex-col">
						<span class="text-[0.7rem] uppercase text-muted-foreground">Firmware</span>
						<span class="font-mono text-sm font-semibold">{disk.firmware}</span>
					</div>
					{#if disk.temperature_c != null}
						<div class="flex flex-col">
							<span class="text-[0.7rem] uppercase text-muted-foreground">Temperature</span>
							<span class="text-sm font-semibold {disk.temperature_c > 55 ? 'text-red-400' : disk.temperature_c > 45 ? 'text-amber-500' : ''}">
								{disk.temperature_c}°C
							</span>
						</div>
					{/if}
					{#if disk.power_on_hours != null}
						<div class="flex flex-col">
							<span class="text-[0.7rem] uppercase text-muted-foreground">Power On</span>
							<span class="text-sm font-semibold">{formatHours(disk.power_on_hours)}</span>
						</div>
					{/if}
				</div>

				{#if expandedDisk === disk.device && disk.attributes.length > 0}
					<div class="mt-5 border-t border-border pt-4">
						<h4 class="mb-3 text-xs uppercase tracking-wide text-muted-foreground">SMART Attributes</h4>
						<table class="w-full text-xs">
							<thead>
								<tr>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">ID</th>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">Attribute</th>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">Value</th>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">Worst</th>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">Thresh</th>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">Raw</th>
									<th class="p-2 text-left text-[0.7rem] uppercase text-muted-foreground">Status</th>
								</tr>
							</thead>
							<tbody>
								{#each disk.attributes as attr}
									<tr class="{criticalIds.has(attr.id) ? 'bg-amber-500/5' : ''} {attr.failing ? 'bg-red-400/10' : ''}">
										<td class="p-2 font-mono">{attr.id}</td>
										<td class="p-2">{attr.name}</td>
										<td class="p-2">{attr.value}</td>
										<td class="p-2">{attr.worst}</td>
										<td class="p-2">{attr.threshold}</td>
										<td class="p-2 font-mono">{attr.raw_value}</td>
										<td class="p-2">
											{#if attr.failing}
												<span class="rounded bg-red-950 px-1.5 py-0.5 text-[0.7rem] font-bold text-red-400">FAIL</span>
											{:else if attr.value <= attr.threshold && attr.threshold > 0}
												<span class="text-[0.7rem] font-semibold text-amber-500">WARN</span>
											{:else}
												<span class="text-[0.7rem] font-semibold text-green-400">OK</span>
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{:else if expandedDisk === disk.device}
					<p class="mt-4 text-sm text-muted-foreground">No SMART attributes available (NVMe drives use a different format).</p>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}
