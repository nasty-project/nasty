<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { Pool, BlockDevice, DeviceState, FsUsage, ScrubStatus, ReconcileStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	interface SelectedDevice {
		path: string;
		label: string;
	}

	let pools: Pool[] = $state([]);
	let devices: BlockDevice[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newName = $state('tank');
	let selectedDevices: SelectedDevice[] = $state([]);
	let replicas = $state(1);
	let compression = $state('');
	let showPartitions = $state(false);

	let expandedPool: string | null = $state(null);
	let addDevicePool: string | null = $state(null);
	let addDevicePath = $state('');
	let addDeviceLabel = $state('');
	let showAddPartitions = $state(false);

	let healthPool: string | null = $state(null);
	let fsUsage: FsUsage | null = $state(null);
	let scrubStatus: ScrubStatus | null = $state(null);
	let reconcileStatus: ReconcileStatus | null = $state(null);
	let healthLoading = $state(false);

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'pool') refresh();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

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
				devices: selectedDevices.map(d => ({
					path: d.path,
					label: d.label || undefined,
				})),
				replicas,
				compression: compression || undefined,
			}),
			`Pool "${newName}" created`
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = 'tank';
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

	async function addDevice(poolName: string) {
		if (!addDevicePath) return;
		const ok = await withToast(
			() => client.call('pool.device.add', {
				pool: poolName,
				device: {
					path: addDevicePath,
					label: addDeviceLabel || undefined,
				},
			}),
			`Device ${addDevicePath} added to "${poolName}"`
		);
		if (ok !== undefined) {
			addDevicePool = null;
			addDevicePath = '';
			addDeviceLabel = '';
			await refresh();
		}
	}

	async function removeDevice(poolName: string, devicePath: string) {
		if (!confirm(`Remove ${devicePath} from pool "${poolName}"? Data will be evacuated first.`)) return;
		await withToast(
			() => client.call('pool.device.remove', { pool: poolName, device: devicePath }),
			`Device ${devicePath} removed from "${poolName}"`
		);
		await refresh();
	}

	async function evacuateDevice(poolName: string, devicePath: string) {
		if (!confirm(`Evacuate all data from ${devicePath}?`)) return;
		await withToast(
			() => client.call('pool.device.evacuate', { pool: poolName, device: devicePath }),
			`Device ${devicePath} evacuated`
		);
		await refresh();
	}

	async function setDeviceState(poolName: string, devicePath: string, state: DeviceState) {
		await withToast(
			() => client.call('pool.device.set_state', { pool: poolName, device: devicePath, state }),
			`Device ${devicePath} set to ${state}`
		);
		await refresh();
	}

	async function onlineDevice(poolName: string, devicePath: string) {
		await withToast(
			() => client.call('pool.device.online', { pool: poolName, device: devicePath }),
			`Device ${devicePath} online`
		);
		await refresh();
	}

	async function offlineDevice(poolName: string, devicePath: string) {
		if (!confirm(`Take ${devicePath} offline?`)) return;
		await withToast(
			() => client.call('pool.device.offline', { pool: poolName, device: devicePath }),
			`Device ${devicePath} offline`
		);
		await refresh();
	}

	async function toggleHealth(poolName: string) {
		if (healthPool === poolName) {
			healthPool = null;
			fsUsage = null;
			scrubStatus = null;
			reconcileStatus = null;
			return;
		}
		healthPool = poolName;
		await refreshHealth(poolName);
	}

	async function refreshHealth(poolName: string) {
		healthLoading = true;
		try {
			[fsUsage, scrubStatus, reconcileStatus] = await Promise.all([
				client.call<FsUsage>('pool.usage', { name: poolName }),
				client.call<ScrubStatus>('pool.scrub.status', { name: poolName }),
				client.call<ReconcileStatus>('pool.reconcile.status', { name: poolName }),
			]);
		} catch {
			// Individual calls may fail
		}
		healthLoading = false;
	}

	async function startScrub(poolName: string) {
		await withToast(
			() => client.call('pool.scrub.start', { name: poolName }),
			`Scrub started on "${poolName}"`
		);
		await refreshHealth(poolName);
	}

	function isSelected(path: string): boolean {
		return selectedDevices.some(d => d.path === path);
	}

	function toggleDevice(path: string) {
		if (isSelected(path)) {
			selectedDevices = selectedDevices.filter(d => d.path !== path);
		} else {
			selectedDevices = [...selectedDevices, { path, label: '' }];
		}
		if (selectedDevices.length <= 1) {
			replicas = 1;
		}
	}

	function setDeviceLabel(path: string, label: string) {
		selectedDevices = selectedDevices.map(d =>
			d.path === path ? { ...d, label } : d
		);
	}

	function availableDevices(): BlockDevice[] {
		return devices.filter(d => !d.in_use && (showPartitions || d.dev_type !== 'part'));
	}

	function availableDevicesForAdd(): BlockDevice[] {
		return devices.filter(d => !d.in_use && (showAddPartitions || d.dev_type !== 'part'));
	}

	function stateColor(state: string | null): string {
		switch (state) {
			case 'rw': return 'bg-green-950 text-green-400';
			case 'ro': return 'bg-blue-950 text-blue-400';
			case 'failed': return 'bg-red-950 text-red-400';
			case 'spare': return 'bg-amber-950 text-amber-400';
			default: return 'bg-secondary text-muted-foreground';
		}
	}
</script>

<h1 class="mb-4 text-2xl font-bold">Storage Pools</h1>

<div class="mb-4">
	<Button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Pool'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">Create Pool</h3>
			<div class="mb-4">
				<Label for="pool-name">Name</Label>
				<Input id="pool-name" bind:value={newName} class="mt-1" />
			</div>
			<div class="mb-4">
				<div class="mb-1 flex items-center justify-between">
					<Label>Devices</Label>
					<label class="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
						<input type="checkbox" bind:checked={showPartitions} class="h-3.5 w-3.5" />
						Show partitions
					</label>
				</div>
				{#if availableDevices().length === 0}
					<p class="text-sm text-muted-foreground">No available devices</p>
				{:else}
					{#each availableDevices() as dev}
						<div class="mb-1">
							<label class="flex cursor-pointer items-center gap-2 py-1 text-sm">
								<input type="checkbox" checked={isSelected(dev.path)} onchange={() => toggleDevice(dev.path)} class="h-4 w-4" />
								{dev.path} ({formatBytes(dev.size_bytes)}) {dev.dev_type === 'part' ? '[part]' : ''} {dev.fs_type ? `[${dev.fs_type}]` : ''}
							</label>
							{#if isSelected(dev.path)}
								<Input
									class="ml-6 text-xs"
									placeholder="label (e.g. ssd.fast)"
									value={selectedDevices.find(d => d.path === dev.path)?.label ?? ''}
									oninput={(e) => setDeviceLabel(dev.path, (e.target as HTMLInputElement).value)}
								/>
							{/if}
						</div>
					{/each}
				{/if}
				{#if selectedDevices.length > 1}
					<span class="mt-1 block text-xs text-muted-foreground">Labels enable tiered storage (e.g. "ssd" and "hdd" groups)</span>
				{/if}
			</div>
			<div class="mb-4 flex gap-4">
				<div class="flex-1">
					<Label for="replicas">Replicas</Label>
					<select id="replicas" bind:value={replicas} disabled={selectedDevices.length <= 1} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						<option value={1}>1 (no redundancy)</option>
						<option value={2}>2 (mirrored)</option>
						<option value={3}>3</option>
					</select>
					{#if selectedDevices.length <= 1}
						<span class="text-xs text-muted-foreground">Requires multiple devices</span>
					{/if}
				</div>
				<div class="flex-1">
					<Label for="compression">Compression</Label>
					<select id="compression" bind:value={compression} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						<option value="">None</option>
						<option value="lz4">LZ4</option>
						<option value="zstd">Zstd</option>
						<option value="gzip">Gzip</option>
					</select>
				</div>
			</div>
			<Button onclick={createPool} disabled={!newName || selectedDevices.length === 0}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if pools.length === 0}
	<p class="text-muted-foreground">No pools configured yet.</p>
{:else}
	{#each pools as pool}
		<Card class="mb-4">
			<CardContent class="pt-4">
				<div class="flex flex-wrap items-center justify-between gap-4">
					<div class="flex items-center gap-3">
						<strong class="text-lg">{pool.name}</strong>
						<Badge variant={pool.mounted ? 'default' : 'destructive'}>
							{pool.mounted ? 'Mounted' : 'Unmounted'}
						</Badge>
						{#if pool.mounted && pool.mount_point}
							<span class="font-mono text-xs text-muted-foreground">{pool.mount_point}</span>
						{/if}
					</div>
					<div class="flex gap-2">
						<Button variant="secondary" size="sm" onclick={() => expandedPool = expandedPool === pool.name ? null : pool.name}>
							{expandedPool === pool.name ? 'Hide Devices' : `Devices (${pool.devices.length})`}
						</Button>
						{#if pool.mounted}
							<Button variant="secondary" size="sm" onclick={() => toggleHealth(pool.name)}>
								{healthPool === pool.name ? 'Hide Health' : 'Health'}
							</Button>
						{/if}
						<Button variant="secondary" size="sm" onclick={() => toggleMount(pool)}>
							{pool.mounted ? 'Unmount' : 'Mount'}
						</Button>
						<Button variant="destructive" size="sm" onclick={() => destroyPool(pool.name)}>Destroy</Button>
					</div>
				</div>

				{#if pool.total_bytes > 0}
					<div class="mt-3">
						<div class="mb-1 h-1.5 overflow-hidden rounded-full bg-secondary">
							<div class="h-full rounded-full bg-primary" style="width: {(pool.used_bytes / pool.total_bytes) * 100}%"></div>
						</div>
						<span class="text-xs text-muted-foreground">
							{formatBytes(pool.used_bytes)} / {formatBytes(pool.total_bytes)} ({formatPercent(pool.used_bytes, pool.total_bytes)})
							{#if pool.options.data_replicas && pool.options.data_replicas > 1} · {pool.options.data_replicas} replicas{/if}
							{#if pool.options.compression} · {pool.options.compression}{/if}
						</span>
					</div>
				{/if}

				{#if healthPool === pool.name}
					<div class="mt-4 border-t border-border pt-4">
						{#if healthLoading}
							<p class="text-sm text-muted-foreground">Loading health data...</p>
						{:else}
							{#if fsUsage}
								<div class="mb-4">
									<h4 class="mb-2 text-xs uppercase tracking-wide text-muted-foreground">Filesystem Usage</h4>
									{#if fsUsage.devices.length > 0}
										<table class="w-full text-sm">
											<thead>
												<tr>
													<th class="p-1.5 text-left text-xs uppercase text-muted-foreground">Device</th>
													<th class="p-1.5 text-left text-xs uppercase text-muted-foreground">Used</th>
													<th class="p-1.5 text-left text-xs uppercase text-muted-foreground">Free</th>
													<th class="p-1.5 text-left text-xs uppercase text-muted-foreground">Total</th>
												</tr>
											</thead>
											<tbody>
												{#each fsUsage.devices as dev}
													<tr>
														<td class="p-1.5 font-mono text-xs">{dev.path}</td>
														<td class="p-1.5 text-xs">{formatBytes(dev.used_bytes)}</td>
														<td class="p-1.5 text-xs">{formatBytes(dev.free_bytes)}</td>
														<td class="p-1.5 text-xs">{formatBytes(dev.total_bytes)}</td>
													</tr>
												{/each}
											</tbody>
										</table>
									{/if}
									<div class="mt-2 grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5 text-xs">
										<span class="text-muted-foreground">Data</span>
										<span>{formatBytes(fsUsage.data_bytes)}</span>
										<span class="text-muted-foreground">Metadata</span>
										<span>{formatBytes(fsUsage.metadata_bytes)}</span>
										<span class="text-muted-foreground">Reserved</span>
										<span>{formatBytes(fsUsage.reserved_bytes)}</span>
									</div>
								</div>
							{/if}
							<div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
								<div class="rounded-lg border border-border p-4">
									<div class="mb-2 flex items-center justify-between">
										<h4 class="text-xs uppercase tracking-wide text-muted-foreground">Scrub</h4>
										{#if scrubStatus}
											<Badge variant={scrubStatus.running ? 'destructive' : 'default'}>
												{scrubStatus.running ? 'Running' : 'Idle'}
											</Badge>
										{/if}
									</div>
									{#if scrubStatus?.raw}
										<pre class="mb-3 max-h-[200px] overflow-auto whitespace-pre-wrap rounded bg-secondary p-2 font-mono text-xs text-muted-foreground">{scrubStatus.raw}</pre>
									{/if}
									<Button size="sm" onclick={() => startScrub(pool.name)}>Start Scrub</Button>
								</div>
								<div class="rounded-lg border border-border p-4">
									<h4 class="mb-2 text-xs uppercase tracking-wide text-muted-foreground">Reconcile</h4>
									{#if reconcileStatus?.raw}
										<pre class="max-h-[200px] overflow-auto whitespace-pre-wrap rounded bg-secondary p-2 font-mono text-xs text-muted-foreground">{reconcileStatus.raw}</pre>
									{:else}
										<p class="text-xs text-muted-foreground">No reconcile data available</p>
									{/if}
								</div>
							</div>
							<Button variant="secondary" size="sm" class="mt-4" onclick={() => refreshHealth(pool.name)}>Refresh</Button>
						{/if}
					</div>
				{/if}

				{#if expandedPool === pool.name}
					<div class="mt-4 border-t border-border pt-4">
						<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5 text-xs">
							<span class="text-muted-foreground">Replicas</span>
							<span>{pool.options.data_replicas ?? 1}</span>
							<span class="text-muted-foreground">Checksum</span>
							<span>{pool.options.data_checksum ?? '—'}</span>
							<span class="text-muted-foreground">Compression</span>
							<span>{pool.options.compression ?? 'none'}{#if pool.options.background_compression} / bg: {pool.options.background_compression}{/if}</span>
							<span class="text-muted-foreground">Encrypted</span>
							<span>{pool.options.encrypted ? 'Yes' : 'No'}</span>
							{#if pool.options.foreground_target}
								<span class="text-muted-foreground">FG Target</span>
								<span>{pool.options.foreground_target}</span>
							{/if}
							{#if pool.options.background_target}
								<span class="text-muted-foreground">BG Target</span>
								<span>{pool.options.background_target}</span>
							{/if}
							{#if pool.options.promote_target}
								<span class="text-muted-foreground">Promote Target</span>
								<span>{pool.options.promote_target}</span>
							{/if}
							{#if pool.options.metadata_target}
								<span class="text-muted-foreground">Meta Target</span>
								<span>{pool.options.metadata_target}</span>
							{/if}
							{#if pool.options.erasure_code}
								<span class="text-muted-foreground">Erasure Code</span>
								<span>Enabled</span>
							{/if}
							{#if pool.options.error_action}
								<span class="text-muted-foreground">Error Action</span>
								<span>{pool.options.error_action}</span>
							{/if}
						</div>

						<table class="w-full text-sm">
							<thead>
								<tr>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Device</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Label</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">State</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Actions</th>
								</tr>
							</thead>
							<tbody>
								{#each pool.devices as dev}
									<tr class="border-b border-border">
										<td class="p-2 font-mono text-xs">{dev.path}</td>
										<td class="p-2 text-xs">{dev.label ?? '—'}</td>
										<td class="p-2">
											<span class="rounded px-2 py-0.5 text-xs font-semibold {stateColor(dev.state)}">
												{dev.state ?? '—'}
											</span>
										</td>
										<td class="flex flex-wrap gap-1.5 p-2">
											{#if pool.mounted}
												{#if dev.state === 'rw'}
													<Button variant="secondary" size="sm" onclick={() => setDeviceState(pool.name, dev.path, 'ro')}>Set RO</Button>
													<Button variant="secondary" size="sm" onclick={() => offlineDevice(pool.name, dev.path)}>Offline</Button>
												{:else if dev.state === 'ro'}
													<Button variant="secondary" size="sm" onclick={() => setDeviceState(pool.name, dev.path, 'rw')}>Set RW</Button>
												{/if}
												{#if dev.state !== 'spare'}
													<Button variant="secondary" size="sm" onclick={() => evacuateDevice(pool.name, dev.path)}>Evacuate</Button>
												{/if}
												<Button variant="destructive" size="sm" onclick={() => removeDevice(pool.name, dev.path)}>Remove</Button>
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>

						{#if pool.mounted}
							{#if addDevicePool === pool.name}
								<div class="mt-3 rounded-lg bg-secondary p-3">
									<div class="mb-2 flex items-center justify-between">
										<Label>Add Device</Label>
										<label class="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
											<input type="checkbox" bind:checked={showAddPartitions} class="h-3.5 w-3.5" />
											Show partitions
										</label>
									</div>
									{#if availableDevicesForAdd().length === 0}
										<p class="text-sm text-muted-foreground">No available devices</p>
									{:else}
										{#each availableDevicesForAdd() as dev}
											<label class="flex cursor-pointer items-center gap-2 py-1 text-sm">
												<input type="radio" name="add-device" value={dev.path} bind:group={addDevicePath} class="h-4 w-4" />
												{dev.path} ({formatBytes(dev.size_bytes)}) {dev.dev_type === 'part' ? '[part]' : ''} {dev.fs_type ? `[${dev.fs_type}]` : ''}
											</label>
										{/each}
									{/if}
									{#if addDevicePath}
										<div class="mt-2">
											<Label for="add-dev-label">Label (optional)</Label>
											<Input id="add-dev-label" bind:value={addDeviceLabel} placeholder="e.g. ssd.fast" class="mt-1" />
										</div>
									{/if}
									<div class="mt-2 flex gap-2">
										<Button size="sm" onclick={() => addDevice(pool.name)} disabled={!addDevicePath}>Add</Button>
										<Button variant="secondary" size="sm" onclick={() => { addDevicePool = null; addDevicePath = ''; addDeviceLabel = ''; }}>Cancel</Button>
									</div>
								</div>
							{:else}
								<Button variant="secondary" size="sm" class="mt-3" onclick={() => addDevicePool = pool.name}>+ Add Device</Button>
							{/if}
						{/if}
					</div>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}
