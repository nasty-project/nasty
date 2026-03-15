<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { Pool, BlockDevice, DeviceState, FsUsage, ScrubStatus, ReconcileStatus, TieringProfile, TieringProfileId } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	let pools: Pool[] = $state([]);
	let devices: BlockDevice[] = $state([]);
	let wizardStep: 0 | 1 | 2 | 3 = $state(0); // 0=hidden, 1=name+devices, 2=profile, 3=review
	let loading = $state(true);

	// Wizard state
	let newName = $state('tank');
	let selectedPaths: string[] = $state([]);
	let wizardProfile: TieringProfileId = $state('single');
	let replicas = $state(1);
	let compression = $state('');
	let showPartitions = $state(false);

	let expandedPool: string | null = $state(null);
	let editOptionsPool: string | null = $state(null);
	let editCompression = $state('');
	let editBgCompression = $state('');
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

	// ── Tiering profile logic ────────────────────────────────────

	function selectedDeviceObjects(): BlockDevice[] {
		return selectedPaths
			.map(p => devices.find(d => d.path === p))
			.filter(Boolean) as BlockDevice[];
	}

	function buildProfiles(): TieringProfile[] {
		const sel = selectedDeviceObjects();
		const hasNvme = sel.some(d => d.device_class === 'nvme');
		const hasSsd  = sel.some(d => d.device_class === 'ssd');
		const hasHdd  = sel.some(d => d.device_class === 'hdd');
		const hasFast = hasNvme || hasSsd;
		const hasSlow = hasHdd;
		const has3Tiers = hasNvme && (hasSsd || hasHdd);

		// Single label for single-tier: use pool name as group
		const singleLabels: Record<string, string> = {};
		sel.forEach(d => { singleLabels[d.path] = newName; });

		// Write-cache labels: fast = nvme/ssd → "fast", hdd → "slow"
		const wcLabels: Record<string, string> = {};
		sel.forEach(d => { wcLabels[d.path] = d.device_class === 'hdd' ? 'slow' : 'fast'; });

		// Full-tier labels by device class
		const ftLabels: Record<string, string> = {};
		sel.forEach(d => { ftLabels[d.path] = d.device_class; });

		// Full-tier targets
		let ftFg: string | null = null;
		let ftMeta: string | null = null;
		let ftBg: string | null = null;
		let ftPromote: string | null = null;
		if (hasNvme) {
			ftFg = 'nvme'; ftMeta = 'nvme';
			if (hasHdd) { ftBg = 'hdd'; if (hasSsd) ftPromote = 'ssd'; }
			else if (hasSsd) { ftBg = 'ssd'; }
		}

		const recommended = hasNvme && (hasSsd || hasHdd) ? 'full_tiering'
			: hasFast && hasSlow ? 'write_cache'
			: 'single';

		return [
			{
				id: 'single',
				name: 'Single Tier',
				tagline: 'Simple — all devices in one pool',
				description: 'All devices are treated as equal peers. bcachefs stripes data across them based on capacity. No performance tiers.',
				available: true,
				recommended: recommended === 'single',
				foreground_target: null,
				metadata_target: null,
				background_target: null,
				promote_target: null,
				device_labels: singleLabels,
			},
			{
				id: 'write_cache',
				name: 'Write Cache + Cold Storage',
				tagline: 'Writes land on fast devices, cold data migrates to slow',
				description: 'Writes go to the fast tier first (NVMe/SSD). Over time, background I/O migrates cold data to the slow tier (HDD), freeing fast space for new writes.',
				available: hasFast && hasSlow,
				recommended: recommended === 'write_cache',
				foreground_target: 'fast',
				metadata_target: 'fast',
				background_target: 'slow',
				promote_target: null,
				device_labels: wcLabels,
			},
			{
				id: 'full_tiering',
				name: 'Full Tiering',
				tagline: 'NVMe writes, SSD read cache, HDD cold storage',
				description: `NVMe handles all writes and metadata. Hot reads are served from the SSD read cache. Cold data moves to HDD in the background. Maximum performance with large capacity.${!has3Tiers ? ' (You can add SSD devices later to enable the read-cache tier.)' : ''}`,
				available: has3Tiers,
				recommended: recommended === 'full_tiering',
				foreground_target: ftFg,
				metadata_target: ftMeta,
				background_target: ftBg,
				promote_target: ftPromote,
				device_labels: ftLabels,
			},
		];
	}

	function activeProfile(): TieringProfile {
		return buildProfiles().find(p => p.id === wizardProfile) ?? buildProfiles()[0];
	}

	async function createPool() {
		if (!newName || selectedPaths.length === 0) return;
		const profile = activeProfile();
		const ok = await withToast(
			() => client.call('pool.create', {
				name: newName,
				devices: selectedPaths.map(path => ({
					path,
					label: profile.device_labels[path] || undefined,
				})),
				replicas,
				compression: compression || undefined,
				foreground_target: profile.foreground_target || undefined,
				metadata_target: profile.metadata_target || undefined,
				background_target: profile.background_target || undefined,
				promote_target: profile.promote_target || undefined,
			}),
			`Pool "${newName}" created`
		);
		if (ok !== undefined) {
			wizardStep = 0;
			newName = 'tank';
			selectedPaths = [];
			wizardProfile = 'single';
			await refresh();
		}
	}

	function openWizard() {
		newName = 'tank';
		selectedPaths = [];
		wizardProfile = 'single';
		replicas = 1;
		compression = '';
		showPartitions = false;
		wizardStep = 1;
	}

	function wizardNext() {
		if (wizardStep === 1) {
			// Auto-select recommended profile
			const rec = buildProfiles().find(p => p.recommended && p.available);
			if (rec) wizardProfile = rec.id;
		}
		wizardStep = (wizardStep + 1) as 1 | 2 | 3;
	}

	async function destroyPool(name: string) {
		if (!await confirm(`Destroy Pool "${name}"`, `This will unmount it.`)) return;
		await withToast(
			() => client.call('pool.destroy', { name, force: true }),
			`Pool "${name}" destroyed`
		);
		await refresh();
	}

	async function toggleMount(pool: Pool) {
		if (pool.mounted) {
			if (!await confirm(`Unmount Pool "${pool.name}"`, `Any active NFS, SMB, iSCSI, and NVMe-oF shares on this pool will be stopped first.`)) return;
		}
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
		if (!await confirm(`Remove ${devicePath}?`, `Data will be evacuated from pool "${poolName}" first.`)) return;
		await withToast(
			() => client.call('pool.device.remove', { pool: poolName, device: devicePath }),
			`Device ${devicePath} removed from "${poolName}"`
		);
		await refresh();
	}

	async function evacuateDevice(poolName: string, devicePath: string) {
		if (!await confirm(`Evacuate all data from ${devicePath}?`)) return;
		await withToast(
			() => client.call('pool.device.evacuate', { pool: poolName, device: devicePath }),
			`Evacuating ${devicePath} — this may take several minutes`
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
		if (!await confirm(`Take ${devicePath} offline?`)) return;
		await withToast(
			() => client.call('pool.device.offline', { pool: poolName, device: devicePath }),
			`Device ${devicePath} offline`
		);
		await refresh();
	}

	function openEditOptions(pool: Pool) {
		if (editOptionsPool === pool.name) {
			editOptionsPool = null;
			return;
		}
		editOptionsPool = pool.name;
		editCompression = pool.options.compression ?? '';
		editBgCompression = pool.options.background_compression ?? '';
	}

	async function saveOptions(poolName: string) {
		await withToast(
			() => client.call('pool.options.update', {
				name: poolName,
				compression: editCompression || 'none',
				background_compression: editBgCompression || 'none',
			}),
			`Options updated for "${poolName}"`
		);
		editOptionsPool = null;
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

	function toggleDevice(path: string) {
		if (selectedPaths.includes(path)) {
			selectedPaths = selectedPaths.filter(p => p !== path);
		} else {
			selectedPaths = [...selectedPaths, path];
		}
		if (selectedPaths.length <= 1) replicas = 1;
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
			case 'evacuating': return 'bg-yellow-950 text-yellow-400 animate-pulse';
			default: return 'bg-secondary text-muted-foreground';
		}
	}

	function classColor(cls: string): string {
		switch (cls) {
			case 'nvme': return 'bg-violet-950 text-violet-300';
			case 'ssd':  return 'bg-blue-950 text-blue-300';
			case 'hdd':  return 'bg-amber-950 text-amber-300';
			default:     return 'bg-secondary text-muted-foreground';
		}
	}
</script>


<div class="mb-4">
	<Button size="sm" onclick={() => wizardStep === 0 ? openWizard() : (wizardStep = 0)}>
		{wizardStep !== 0 ? 'Cancel' : 'Create Pool'}
	</Button>
</div>

{#if wizardStep !== 0}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<!-- Step indicator -->
			<div class="mb-6 flex items-center gap-0">
				{#each [['1', 'Devices'], ['2', 'Tiering'], ['3', 'Review']] as [num, label], i}
					<div class="flex items-center">
						<div class="flex items-center gap-2">
							<div class="flex h-6 w-6 items-center justify-center rounded-full text-xs font-semibold
								{wizardStep > i + 1 ? 'bg-primary text-primary-foreground' :
								 wizardStep === i + 1 ? 'bg-primary text-primary-foreground' :
								 'bg-secondary text-muted-foreground'}">
								{num}
							</div>
							<span class="text-xs {wizardStep === i + 1 ? 'text-foreground font-medium' : 'text-muted-foreground'}">{label}</span>
						</div>
						{#if i < 2}
							<div class="mx-3 h-px w-8 bg-border"></div>
						{/if}
					</div>
				{/each}
			</div>

			<!-- Step 1: Name + Devices -->
			{#if wizardStep === 1}
				<div class="mb-4">
					<Label for="pool-name">Pool Name</Label>
					<Input id="pool-name" bind:value={newName} class="mt-1 max-w-xs" />
				</div>
				<div class="mb-4">
					<div class="mb-2 flex items-center justify-between">
						<Label>Select Devices</Label>
						<label class="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
							<input type="checkbox" bind:checked={showPartitions} class="h-3.5 w-3.5" />
							Show partitions
						</label>
					</div>
					{#if availableDevices().length === 0}
						<p class="text-sm text-muted-foreground">No available devices</p>
					{:else}
						<div class="space-y-1.5">
							{#each availableDevices() as dev}
								<label class="flex cursor-pointer items-center gap-3 rounded-lg border border-border px-3 py-2 text-sm
									{selectedPaths.includes(dev.path) ? 'border-primary bg-primary/5' : 'hover:bg-secondary/50'}">
									<input type="checkbox" checked={selectedPaths.includes(dev.path)}
										onchange={() => toggleDevice(dev.path)} class="h-4 w-4 shrink-0" />
									<span class="font-mono text-xs shrink-0">{dev.path}</span>
									<span class="rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">
										{dev.device_class}
									</span>
									<span class="text-muted-foreground">{formatBytes(dev.size_bytes)}</span>
									{#if dev.fs_type}
										<span class="text-xs text-muted-foreground">[{dev.fs_type}]</span>
									{/if}
								</label>
							{/each}
						</div>
					{/if}
				</div>
				<div class="flex gap-2">
					<Button size="sm" onclick={wizardNext} disabled={!newName || selectedPaths.length === 0}>
						Next: Choose Tiering →
					</Button>
				</div>

			<!-- Step 2: Tiering Profile -->
			{:else if wizardStep === 2}
				{@const profiles = buildProfiles()}
				<div class="mb-4 space-y-3">
					{#each profiles as profile}
						<button
							disabled={!profile.available}
							onclick={() => { if (profile.available) wizardProfile = profile.id; }}
							class="w-full rounded-lg border-2 px-4 py-3 text-left transition-colors
								{!profile.available ? 'cursor-not-allowed border-border opacity-40' :
								 wizardProfile === profile.id ? 'border-primary bg-primary/5' :
								 'border-border hover:border-primary/50 hover:bg-secondary/30'}">
							<div class="flex items-center justify-between">
								<div class="flex items-center gap-2">
									<div class="h-4 w-4 rounded-full border-2 flex items-center justify-center
										{wizardProfile === profile.id && profile.available ? 'border-primary' : 'border-muted-foreground'}">
										{#if wizardProfile === profile.id && profile.available}
											<div class="h-2 w-2 rounded-full bg-primary"></div>
										{/if}
									</div>
									<span class="font-semibold text-sm">{profile.name}</span>
									{#if profile.recommended && profile.available}
										<span class="rounded bg-primary/20 px-1.5 py-0.5 text-[10px] font-semibold text-primary uppercase">recommended</span>
									{/if}
								</div>
							</div>
							<p class="mt-1 ml-6 text-xs text-muted-foreground">{profile.tagline}</p>
							{#if wizardProfile === profile.id && profile.available}
								<p class="mt-2 ml-6 text-xs text-foreground/80">{profile.description}</p>
								<!-- Tier diagram -->
								<div class="mt-3 ml-6">
									{#if profile.id === 'single'}
										<div class="flex flex-wrap gap-2">
											{#each selectedDeviceObjects() as dev}
												<div class="flex items-center gap-1.5 rounded border border-border px-2 py-1">
													<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
													<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
												</div>
											{/each}
										</div>
									{:else if profile.id === 'write_cache'}
										<div class="flex items-start gap-6">
											<div>
												<div class="mb-1 text-[10px] text-muted-foreground uppercase tracking-wide">Fast (writes + metadata)</div>
												<div class="flex flex-col gap-1">
													{#each selectedDeviceObjects().filter(d => d.device_class !== 'hdd') as dev}
														<div class="flex items-center gap-1.5 rounded border border-blue-800/50 bg-blue-950/30 px-2 py-1">
															<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
															<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
														</div>
													{/each}
												</div>
											</div>
											<div class="mt-4 text-muted-foreground">→</div>
											<div>
												<div class="mb-1 text-[10px] text-muted-foreground uppercase tracking-wide">Slow (cold data)</div>
												<div class="flex flex-col gap-1">
													{#each selectedDeviceObjects().filter(d => d.device_class === 'hdd') as dev}
														<div class="flex items-center gap-1.5 rounded border border-amber-800/50 bg-amber-950/30 px-2 py-1">
															<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
															<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
														</div>
													{/each}
												</div>
											</div>
										</div>
									{:else if profile.id === 'full_tiering'}
										<div class="flex items-start gap-4">
											{#each [['nvme', 'Writes + Metadata', 'border-violet-800/50 bg-violet-950/30'],
											         ['ssd', 'Read Cache', 'border-blue-800/50 bg-blue-950/30'],
											         ['hdd', 'Cold Storage', 'border-amber-800/50 bg-amber-950/30']] as [cls, role, colors]}
												{@const devs = selectedDeviceObjects().filter(d => d.device_class === cls)}
												{#if devs.length > 0}
													<div>
														<div class="mb-1 text-[10px] text-muted-foreground uppercase tracking-wide">{role}</div>
														<div class="flex flex-col gap-1">
															{#each devs as dev}
																<div class="flex items-center gap-1.5 rounded border {colors} px-2 py-1">
																	<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(cls)}">{cls}</span>
																	<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
																</div>
															{/each}
														</div>
													</div>
												{/if}
											{/each}
										</div>
									{/if}
								</div>
							{/if}
						</button>
					{/each}
				</div>
				<div class="flex gap-2">
					<Button variant="secondary" size="sm" onclick={() => wizardStep = 1}>← Back</Button>
					<Button size="sm" onclick={wizardNext}>Next: Review →</Button>
				</div>

			<!-- Step 3: Review + Options -->
			{:else if wizardStep === 3}
				{@const profile = activeProfile()}
				<div class="mb-5 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
					<span class="text-muted-foreground">Name</span>
					<span class="font-mono">{newName}</span>
					<span class="text-muted-foreground">Devices</span>
					<div class="flex flex-wrap gap-1.5">
						{#each selectedDeviceObjects() as dev}
							<span class="flex items-center gap-1 rounded border border-border px-1.5 py-0.5 text-xs">
								<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
								<span class="font-mono">{dev.path}</span>
								<span class="text-muted-foreground">→ {profile.device_labels[dev.path] ?? newName}</span>
							</span>
						{/each}
					</div>
					<span class="text-muted-foreground">Tiering</span>
					<span>{profile.name}</span>
					{#if profile.foreground_target}
						<span class="text-muted-foreground">FG Target</span><span>{profile.foreground_target}</span>
					{/if}
					{#if profile.metadata_target}
						<span class="text-muted-foreground">Meta Target</span><span>{profile.metadata_target}</span>
					{/if}
					{#if profile.background_target}
						<span class="text-muted-foreground">BG Target</span><span>{profile.background_target}</span>
					{/if}
					{#if profile.promote_target}
						<span class="text-muted-foreground">Promote Target</span><span>{profile.promote_target}</span>
					{/if}
				</div>

				<div class="mb-5 flex gap-4">
					<div class="flex-1">
						<Label for="replicas">Replicas</Label>
						<select id="replicas" bind:value={replicas} disabled={selectedPaths.length <= 1}
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value={1}>1 (no redundancy)</option>
							<option value={2}>2 (mirrored)</option>
							<option value={3}>3</option>
						</select>
						{#if selectedPaths.length <= 1}
							<span class="text-xs text-muted-foreground">Requires multiple devices</span>
						{/if}
					</div>
					<div class="flex-1">
						<Label for="compression">Compression</Label>
						<select id="compression" bind:value={compression}
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value="">None</option>
							<option value="lz4">LZ4</option>
							<option value="zstd">Zstd</option>
							<option value="gzip">Gzip</option>
						</select>
					</div>
				</div>

				<div class="flex gap-2">
					<Button variant="secondary" size="sm" onclick={() => wizardStep = 2}>← Back</Button>
					<Button size="sm" onclick={createPool}>Create Pool</Button>
				</div>
			{/if}
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if pools.length === 0}
	<p class="text-muted-foreground">No pools configured yet.</p>
{:else}
	{#each pools as pool}
		<Card class="mb-4 cursor-pointer" onclick={() => expandedPool = expandedPool === pool.name ? null : pool.name}>
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
					<div class="flex gap-2" onclick={(e) => e.stopPropagation()}>
						<Button variant="secondary" size="xs" onclick={() => expandedPool = expandedPool === pool.name ? null : pool.name}>
							{expandedPool === pool.name ? 'Hide Details' : 'Details'}
						</Button>
						{#if pool.mounted}
							<Button variant="secondary" size="xs" onclick={() => openEditOptions(pool)}>
								{editOptionsPool === pool.name ? 'Hide Options' : 'Options'}
							</Button>
							<Button variant="secondary" size="xs" onclick={() => toggleHealth(pool.name)}>
								{healthPool === pool.name ? 'Hide Health' : 'Health'}
							</Button>
						{/if}
						<Button variant="secondary" size="xs" onclick={() => toggleMount(pool)}>
							{pool.mounted ? 'Unmount' : 'Mount'}
						</Button>
						<Button variant="destructive" size="xs" onclick={() => destroyPool(pool.name)}>Destroy</Button>
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

				{#if editOptionsPool === pool.name}
				<div class="mt-4 border-t border-border pt-4">
					<h4 class="mb-3 text-xs uppercase tracking-wide text-muted-foreground">Edit Options</h4>
					<div class="flex flex-wrap gap-4">
						<div>
							<label for="edit-compression-{pool.name}" class="mb-1 block text-xs text-muted-foreground">Compression</label>
							<select id="edit-compression-{pool.name}" bind:value={editCompression} class="h-9 rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">None</option>
								<option value="lz4">LZ4</option>
								<option value="zstd">Zstd</option>
								<option value="gzip">Gzip</option>
							</select>
						</div>
						<div>
							<label for="edit-bg-compression-{pool.name}" class="mb-1 block text-xs text-muted-foreground">Background Compression</label>
							<select id="edit-bg-compression-{pool.name}" bind:value={editBgCompression} class="h-9 rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">None</option>
								<option value="lz4">LZ4</option>
								<option value="zstd">Zstd</option>
								<option value="gzip">Gzip</option>
							</select>
						</div>
					</div>
					<div class="mt-3 flex gap-2">
						<Button size="xs" onclick={() => saveOptions(pool.name)}>Save</Button>
						<Button variant="secondary" size="xs" onclick={() => editOptionsPool = null}>Cancel</Button>
					</div>
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
									<Button size="xs" onclick={() => startScrub(pool.name)}>Start Scrub</Button>
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
							<Button variant="secondary" size="xs" class="mt-4" onclick={() => refreshHealth(pool.name)}>Refresh</Button>
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
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Data Allowed</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Has Data</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
								</tr>
							</thead>
							<tbody>
								{#each pool.devices as dev}
									<tr class="border-b border-border">
										<td class="p-2 font-mono text-xs">
											{dev.path}
											{#if dev.durability !== null && dev.durability !== undefined && dev.durability !== 1}
												<span class="ml-1 rounded bg-secondary px-1 py-0.5 text-[10px] text-muted-foreground">durability={dev.durability}</span>
											{/if}
											{#if dev.discard}
												<span class="ml-1 rounded bg-secondary px-1 py-0.5 text-[10px] text-muted-foreground">discard</span>
											{/if}
										</td>
										<td class="p-2 text-xs">{dev.label ?? '—'}</td>
										<td class="p-2">
											<span class="rounded px-2 py-0.5 text-xs font-semibold {stateColor(dev.state)}">
												{dev.state ?? '—'}
											</span>
										</td>
										<td class="p-2 font-mono text-xs text-muted-foreground">{dev.data_allowed ?? '—'}</td>
										<td class="p-2 font-mono text-xs text-muted-foreground">{dev.has_data ?? '—'}</td>
										<td class="flex flex-wrap gap-1.5 p-2">
											{#if pool.mounted}
												{#if dev.state === 'rw'}
													<Button variant="secondary" size="xs" onclick={() => setDeviceState(pool.name, dev.path, 'ro')}>Set RO</Button>
													<Button variant="secondary" size="xs" onclick={() => offlineDevice(pool.name, dev.path)}>Offline</Button>
												{:else if dev.state === 'ro'}
													<Button variant="secondary" size="xs" onclick={() => setDeviceState(pool.name, dev.path, 'rw')}>Set RW</Button>
												{/if}
												{#if dev.state !== 'spare'}
													<Button variant="secondary" size="xs" onclick={() => evacuateDevice(pool.name, dev.path)}>Evacuate</Button>
												{/if}
												<Button variant="destructive" size="xs" onclick={() => removeDevice(pool.name, dev.path)}>Remove</Button>
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
										<Button size="xs" onclick={() => addDevice(pool.name)} disabled={!addDevicePath}>Add</Button>
										<Button variant="secondary" size="xs" onclick={() => { addDevicePool = null; addDevicePath = ''; addDeviceLabel = ''; }}>Cancel</Button>
									</div>
								</div>
							{:else}
								<Button variant="secondary" size="xs" class="mt-3" onclick={() => addDevicePool = pool.name}>+ Add Device</Button>
							{/if}
						{/if}
					</div>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}
