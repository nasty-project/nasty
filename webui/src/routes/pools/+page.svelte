<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { Pool, BlockDevice } from '$lib/types';

	let pools: Pool[] = $state([]);
	let devices: BlockDevice[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	// Create form
	let newName = $state('');
	let selectedDevices: string[] = $state([]);
	let replicas = $state(1);
	let compression = $state('');

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			pools = await client.call<Pool[]>('pool.list');
			devices = await client.call<BlockDevice[]>('device.list');
		});
	}

	async function createPool() {
		if (!newName || selectedDevices.length === 0) return;
		const ok = await withToast(
			() => client.call('pool.create', {
				name: newName,
				devices: selectedDevices,
				replicas,
				compression: compression || undefined,
			}),
			`Pool "${newName}" created`
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			selectedDevices = [];
			await refresh();
		}
	}

	async function destroyPool(name: string) {
		if (!confirm(`Destroy pool "${name}"? This will unmount it.`)) return;
		await withToast(
			() => client.call('pool.destroy', { name, force: true }),
			`Pool "${name}" destroyed`
		);
		await refresh();
	}

	async function toggleMount(pool: Pool) {
		const action = pool.mounted ? 'unmount' : 'mount';
		await withToast(
			() => pool.mounted
				? client.call('pool.unmount', { name: pool.name })
				: client.call('pool.mount', { name: pool.name }),
			`Pool "${pool.name}" ${action}ed`
		);
		await refresh();
	}

	function toggleDevice(path: string) {
		if (selectedDevices.includes(path)) {
			selectedDevices = selectedDevices.filter(d => d !== path);
		} else {
			selectedDevices = [...selectedDevices, path];
		}
	}

	$effect(() => {
		// Filter to unused devices
		devices = devices;
	});

	function availableDevices(): BlockDevice[] {
		return devices.filter(d => !d.in_use);
	}
</script>

<h1>Storage Pools</h1>

<div class="toolbar">
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Pool'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>Create Pool</h3>
		<div class="field">
			<label for="pool-name">Name</label>
			<input id="pool-name" bind:value={newName} placeholder="tank" />
		</div>
		<div class="field">
			<span class="field-label">Devices</span>
			{#if availableDevices().length === 0}
				<p class="muted">No available devices</p>
			{:else}
				{#each availableDevices() as dev}
					<label class="device-option">
						<input type="checkbox" checked={selectedDevices.includes(dev.path)} onchange={() => toggleDevice(dev.path)} />
						{dev.path} ({formatBytes(dev.size_bytes)}) {dev.fs_type ? `[${dev.fs_type}]` : ''}
					</label>
				{/each}
			{/if}
		</div>
		<div class="field-row">
			<div class="field">
				<label for="replicas">Replicas</label>
				<select id="replicas" bind:value={replicas}>
					<option value={1}>1 (no redundancy)</option>
					<option value={2}>2 (mirrored)</option>
					<option value={3}>3</option>
				</select>
			</div>
			<div class="field">
				<label for="compression">Compression</label>
				<select id="compression" bind:value={compression}>
					<option value="">None</option>
					<option value="lz4">LZ4</option>
					<option value="zstd">Zstd</option>
					<option value="gzip">Gzip</option>
				</select>
			</div>
		</div>
		<button onclick={createPool} disabled={!newName || selectedDevices.length === 0}>
			Create
		</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if pools.length === 0}
	<p class="muted">No pools configured yet.</p>
{:else}
	<table>
		<thead>
			<tr>
				<th>Name</th>
				<th>Devices</th>
				<th>Usage</th>
				<th>Replicas</th>
				<th>Status</th>
				<th>Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each pools as pool}
				<tr>
					<td><strong>{pool.name}</strong></td>
					<td class="mono">{pool.devices.join(', ')}</td>
					<td>
						{#if pool.total_bytes > 0}
							<div class="usage">
								<div class="usage-bar">
									<div class="usage-fill" style="width: {(pool.used_bytes / pool.total_bytes) * 100}%"></div>
								</div>
								<span>{formatBytes(pool.used_bytes)} / {formatBytes(pool.total_bytes)} ({formatPercent(pool.used_bytes, pool.total_bytes)})</span>
							</div>
						{:else}
							—
						{/if}
					</td>
					<td>{pool.replicas}</td>
					<td>
						<span class="badge" class:mounted={pool.mounted} class:unmounted={!pool.mounted}>
							{pool.mounted ? 'Mounted' : 'Unmounted'}
						</span>
					</td>
					<td class="actions">
						<button class="secondary" onclick={() => toggleMount(pool)}>
							{pool.mounted ? 'Unmount' : 'Mount'}
						</button>
						<button class="danger" onclick={() => destroyPool(pool.name)}>Destroy</button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<style>
	.toolbar { margin: 1rem 0; }
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 500px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label, .field-label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input, .field select { width: 100%; box-sizing: border-box; }
	.field-row { display: flex; gap: 1rem; }
	.field-row .field { flex: 1; }
	.device-option { display: flex; align-items: center; gap: 0.5rem; padding: 0.25rem 0; cursor: pointer; font-size: 0.875rem; }
	.device-option input[type="checkbox"] { width: auto; }
	.mono { font-family: monospace; font-size: 0.8rem; }
	.muted { color: #6b7280; }
	.usage { font-size: 0.8rem; }
	.usage-bar { height: 6px; background: #1e2130; border-radius: 3px; margin-bottom: 0.25rem; }
	.usage-fill { height: 100%; background: #2563eb; border-radius: 3px; }
	.badge { padding: 0.2rem 0.5rem; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
	.badge.mounted { background: #064e3b; color: #4ade80; }
	.badge.unmounted { background: #3b0e0e; color: #f87171; }
	.actions { display: flex; gap: 0.5rem; }
</style>
