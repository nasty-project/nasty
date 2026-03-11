<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { DiskHealth } from '$lib/types';

	let disks: DiskHealth[] = $state([]);
	let loading = $state(true);
	let expandedDisk = $state<string | null>(null);

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

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

	// Key SMART attributes to highlight
	const criticalIds = new Set([5, 10, 187, 188, 196, 197, 198]);
</script>

<h1>Disk Health</h1>

<div class="toolbar">
	<button onclick={refresh}>Refresh</button>
</div>

{#if loading}
	<p>Loading...</p>
{:else if disks.length === 0}
	<p class="muted">No disks detected or smartctl not available.</p>
{:else}
	{#each disks as disk}
		<div class="disk-card" class:failed={!disk.health_passed}>
			<div class="disk-header">
				<div class="disk-status">
					<span class="smart-badge" class:passed={disk.health_passed} class:failed={!disk.health_passed}>
						{disk.smart_status}
					</span>
				</div>
				<div class="disk-title">
					<strong class="mono">{disk.device}</strong>
					<span class="disk-model">{disk.model}</span>
				</div>
				<button class="secondary toggle" onclick={() => expandedDisk = expandedDisk === disk.device ? null : disk.device}>
					{expandedDisk === disk.device ? 'Hide' : 'Details'}
				</button>
			</div>

			<div class="disk-stats">
				<div class="disk-stat">
					<span class="stat-key">Capacity</span>
					<span class="stat-val">{formatBytes(disk.capacity_bytes)}</span>
				</div>
				<div class="disk-stat">
					<span class="stat-key">Serial</span>
					<span class="stat-val mono">{disk.serial}</span>
				</div>
				<div class="disk-stat">
					<span class="stat-key">Firmware</span>
					<span class="stat-val mono">{disk.firmware}</span>
				</div>
				{#if disk.temperature_c != null}
					<div class="disk-stat">
						<span class="stat-key">Temperature</span>
						<span class="stat-val" class:temp-warn={disk.temperature_c > 45} class:temp-crit={disk.temperature_c > 55}>
							{disk.temperature_c}°C
						</span>
					</div>
				{/if}
				{#if disk.power_on_hours != null}
					<div class="disk-stat">
						<span class="stat-key">Power On</span>
						<span class="stat-val">{formatHours(disk.power_on_hours)}</span>
					</div>
				{/if}
			</div>

			{#if expandedDisk === disk.device && disk.attributes.length > 0}
				<div class="attrs">
					<h4>SMART Attributes</h4>
					<table>
						<thead>
							<tr>
								<th>ID</th>
								<th>Attribute</th>
								<th>Value</th>
								<th>Worst</th>
								<th>Thresh</th>
								<th>Raw</th>
								<th>Status</th>
							</tr>
						</thead>
						<tbody>
							{#each disk.attributes as attr}
								<tr class:critical={criticalIds.has(attr.id)} class:failing={attr.failing}>
									<td class="mono">{attr.id}</td>
									<td>{attr.name}</td>
									<td>{attr.value}</td>
									<td>{attr.worst}</td>
									<td>{attr.threshold}</td>
									<td class="mono">{attr.raw_value}</td>
									<td>
										{#if attr.failing}
											<span class="attr-fail">FAIL</span>
										{:else if attr.value <= attr.threshold && attr.threshold > 0}
											<span class="attr-warn">WARN</span>
										{:else}
											<span class="attr-ok">OK</span>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{:else if expandedDisk === disk.device}
				<p class="muted attrs-note">No SMART attributes available (NVMe drives use a different format).</p>
			{/if}
		</div>
	{/each}
{/if}

<style>
	.toolbar { margin: 1rem 0; }
	.muted { color: #6b7280; }
	.mono { font-family: monospace; font-size: 0.85rem; }

	.disk-card {
		background: #161926;
		border: 1px solid #2d3348;
		border-radius: 8px;
		padding: 1.25rem;
		margin-bottom: 1rem;
	}
	.disk-card.failed {
		border-color: #7f1d1d;
	}

	.disk-header {
		display: flex;
		align-items: center;
		gap: 1rem;
		margin-bottom: 1rem;
	}
	.disk-title {
		flex: 1;
		display: flex;
		align-items: baseline;
		gap: 0.75rem;
	}
	.disk-model { color: #9ca3af; font-size: 0.875rem; }

	.smart-badge {
		padding: 0.2rem 0.6rem;
		border-radius: 4px;
		font-size: 0.75rem;
		font-weight: 700;
	}
	.smart-badge.passed { background: #064e3b; color: #4ade80; }
	.smart-badge.failed { background: #450a0a; color: #f87171; }

	.toggle { font-size: 0.8rem; padding: 0.3rem 0.7rem; }

	.disk-stats {
		display: flex;
		flex-wrap: wrap;
		gap: 1.5rem;
	}
	.disk-stat { display: flex; flex-direction: column; }
	.stat-key { font-size: 0.7rem; color: #6b7280; text-transform: uppercase; }
	.stat-val { font-size: 0.9rem; font-weight: 600; }
	.temp-warn { color: #f59e0b; }
	.temp-crit { color: #f87171; }

	.attrs { margin-top: 1.25rem; padding-top: 1rem; border-top: 1px solid #2d3348; }
	.attrs h4 { margin: 0 0 0.75rem; color: #9ca3af; font-size: 0.75rem; text-transform: uppercase; }
	.attrs-note { margin-top: 1rem; font-size: 0.85rem; }

	.attrs table { font-size: 0.8rem; }
	.attrs th { font-size: 0.7rem; }
	.attrs td { padding: 0.4rem 0.75rem; }

	tr.critical { background: rgba(245, 158, 11, 0.05); }
	tr.failing { background: rgba(248, 113, 113, 0.1); }

	.attr-ok { color: #4ade80; font-size: 0.7rem; font-weight: 600; }
	.attr-warn { color: #f59e0b; font-size: 0.7rem; font-weight: 600; }
	.attr-fail { color: #f87171; font-size: 0.7rem; font-weight: 700; background: #450a0a; padding: 0.1rem 0.3rem; border-radius: 3px; }
</style>
