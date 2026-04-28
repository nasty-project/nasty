<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { BlockDevice, DiskHealth, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';

	type DisksTab = 'devices' | 'smart' | 'topology';

	let activeTab: DisksTab = $state('devices');
	let blockDevices: BlockDevice[] = $state([]);
	let disks: DiskHealth[] = $state([]);
	let smartProtocol: ProtocolStatus | null = $state(null);
	let loading = $state(true);
	let expandedDisk = $state<string | null>(null);
	let isVirtual = $state(false);
	let pollInterval: ReturnType<typeof setInterval> | null = null;

	const client = getClient();

	onMount(async () => {
		await Promise.all([loadBlockDevices(), loadSmartProtocol(), loadVmStatus()]);
		loading = false;
		pollInterval = setInterval(refresh, 30000);
	});

	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});

	async function loadBlockDevices() {
		await withToast(async () => {
			blockDevices = await client.call<BlockDevice[]>('device.list');
		});
	}

	async function loadVmStatus() {
		try {
			const info = await client.call<{ is_virtual: boolean }>('system.info');
			isVirtual = info.is_virtual;
		} catch { /* ignore */ }
	}

	async function loadSmartProtocol() {
		await withToast(async () => {
			const protocols = await client.call<ProtocolStatus[]>('service.protocol.list');
			smartProtocol = protocols.find(p => p.name === 'smart') ?? null;
			if (smartProtocol?.enabled) await loadSmartDisks();
		});
	}

	async function loadSmartDisks() {
		await withToast(async () => {
			disks = await client.call<DiskHealth[]>('system.disks');
		});
	}

	async function refresh() {
		await loadBlockDevices();
		if (smartProtocol?.enabled) await loadSmartDisks();
	}

	async function toggleSmart() {
		if (!smartProtocol) return;
		const action = smartProtocol.enabled ? 'disable' : 'enable';
		const ok = await withToast(
			() => client.call(`service.protocol.${action}`, { name: 'smart' }),
			`SMART monitoring ${smartProtocol.enabled ? 'disabled' : 'enabled'}`
		);
		if (ok !== undefined) {
			await loadSmartProtocol();
			if (!smartProtocol?.enabled) disks = [];
		}
	}

	async function wipe(dev: BlockDevice) {
		if (!await confirm(`Wipe ${dev.path}?`, `This will erase all filesystem signatures on ${dev.path}. The data itself is not overwritten but the device will appear blank.`)) return;
		const ok = await withToast(
			() => client.call('device.wipe', { path: dev.path }),
			`${dev.path} wiped`
		);
		if (ok !== undefined) await loadBlockDevices();
	}

	function formatHours(hours: number): string {
		const days = Math.floor(hours / 24);
		const years = Math.floor(days / 365);
		if (years > 0) return `${years}y ${days % 365}d`;
		if (days > 0) return `${days}d ${hours % 24}h`;
		return `${hours}h`;
	}

	function deviceClassBadge(cls: string): string {
		switch (cls) {
			case 'nvme': return 'bg-purple-950 text-purple-400';
			case 'ssd': return 'bg-blue-950 text-blue-400';
			default: return 'bg-secondary text-muted-foreground';
		}
	}

	const criticalIds = new Set([5, 10, 187, 188, 196, 197, 198]);

	function smartFor(dev: BlockDevice): DiskHealth | undefined {
		return disks.find(d => d.device === dev.path || dev.path.startsWith(d.device));
	}

	// Group disks by controller for Topology tab
	interface ControllerGroup {
		pci: string;
		name: string;
		disks: DiskHealth[];
	}

	function groupByController(diskList: DiskHealth[]): ControllerGroup[] {
		const groups = new Map<string, ControllerGroup>();
		for (const disk of diskList) {
			const pci = disk.controller_pci ?? 'unknown';
			const name = disk.controller_name ?? 'Unknown Controller';
			if (!groups.has(pci)) {
				groups.set(pci, { pci, name, disks: [] });
			}
			groups.get(pci)!.disks.push(disk);
		}
		// Sort groups: known controllers first, then by PCI address
		return [...groups.values()].sort((a, b) => a.pci.localeCompare(b.pci));
	}
</script>


{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<!-- Tab bar -->
	<div class="mb-6 flex items-center border-b border-border">
		{#each [['devices', 'Devices'], ['smart', 'SMART Health'], ['topology', 'Topology']] as [tab, label]}
			<button
				onclick={() => { activeTab = tab as DisksTab; }}
				class="px-4 py-2 text-sm font-medium transition-colors border-b-2 -mb-px
					{activeTab === tab
						? 'border-primary text-foreground'
						: 'border-transparent text-muted-foreground hover:text-foreground'}"
			>
				{label}
			</button>
		{/each}
		<div class="ml-auto pb-1">
			<Button size="sm" variant="secondary" onclick={refresh}>Refresh</Button>
		</div>
	</div>

	<!-- Devices tab -->
	{#if activeTab === 'devices'}
		<table class="w-full text-sm">
			<thead>
				<tr>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Device</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Size</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Type</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Filesystem</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each blockDevices as dev}
					<tr class="border-b border-border {dev.dev_type === 'part' ? 'bg-muted/10' : ''}">
						<td class="p-3 font-mono text-sm {dev.dev_type === 'part' ? 'pl-8' : ''}">{dev.path}</td>
						<td class="p-3">{formatBytes(dev.size_bytes)}</td>
						<td class="p-3">
							<span class="rounded px-1.5 py-0.5 text-xs font-semibold {deviceClassBadge(dev.device_class)}">
								{dev.device_class.toUpperCase()}
							</span>
						</td>
						<td class="p-3 font-mono text-xs text-muted-foreground">{dev.fs_type ?? '—'}</td>
						<td class="p-3">
							{#if dev.in_use}
								<Badge variant="default">In use</Badge>
							{:else}
								<Badge variant="secondary">Free</Badge>
								{#if dev.fs_type}
									<Badge variant="outline" class="ml-1 border-amber-700 text-amber-400">Has signatures</Badge>
								{/if}
							{/if}
						</td>
						<td class="p-3 w-px whitespace-nowrap">
							{#if !dev.in_use && dev.fs_type}
								<Button variant="destructive" size="xs" onclick={() => wipe(dev)}>Wipe</Button>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>

	<!-- SMART Health tab -->
	{:else if activeTab === 'smart'}
		{#if isVirtual}
			<p class="text-sm text-muted-foreground">SMART monitoring is not available on virtual machines.</p>
		{:else}
		<div class="mb-4 flex items-center gap-4">
			{#if smartProtocol}
				<Badge variant={smartProtocol.enabled ? 'default' : 'secondary'}>
					{smartProtocol.enabled ? 'Enabled' : 'Disabled'}
				</Badge>
				<Button variant="secondary" size="xs" onclick={toggleSmart}>
					{smartProtocol.enabled ? 'Disable' : 'Enable'}
				</Button>
			{/if}
		</div>

		{#if !smartProtocol?.enabled}
			<p class="text-sm text-muted-foreground">Enable SMART monitoring above to see disk health data.</p>
		{:else if disks.length === 0}
			<p class="text-sm text-muted-foreground">No disks detected or smartctl not available.</p>
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
								{#if disk.ata_port}
									<span class="rounded bg-muted px-1.5 py-0.5 text-xs font-mono text-muted-foreground">{disk.ata_port}</span>
								{/if}
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
		{/if}

	<!-- Topology tab -->
	{:else if activeTab === 'topology'}
		{#if smartProtocol?.enabled && disks.length > 0}
			{#each groupByController(disks) as group}
				<div class="mb-6">
					<div class="mb-3 flex items-baseline gap-3">
						<h3 class="text-sm font-semibold text-foreground">{group.name}</h3>
						<span class="rounded bg-muted px-2 py-0.5 font-mono text-xs text-muted-foreground">PCI {group.pci}</span>
					</div>
					<table class="w-full text-sm">
						<thead>
							<tr>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Port</th>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Device</th>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Model</th>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Serial</th>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Capacity</th>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Temp</th>
								<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Health</th>
							</tr>
						</thead>
						<tbody>
							{#each group.disks as disk}
								<tr class="border-b border-border">
									<td class="p-3 font-mono text-sm font-semibold">{disk.ata_port ?? '—'}</td>
									<td class="p-3 font-mono text-sm">{disk.device}</td>
									<td class="p-3 text-sm">{disk.model}</td>
									<td class="p-3 font-mono text-xs text-muted-foreground">{disk.serial}</td>
									<td class="p-3 text-sm">{formatBytes(disk.capacity_bytes)}</td>
									<td class="p-3 text-sm {disk.temperature_c != null && disk.temperature_c > 55 ? 'text-red-400' : disk.temperature_c != null && disk.temperature_c > 45 ? 'text-amber-500' : ''}">
										{disk.temperature_c != null ? `${disk.temperature_c}°C` : '—'}
									</td>
									<td class="p-3">
										<span class="rounded px-2 py-0.5 text-xs font-bold {disk.health_passed ? 'bg-green-950 text-green-400' : 'bg-red-950 text-red-400'}">
											{disk.smart_status}
										</span>
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/each}
		{:else}
			<!-- Show basic topology from block devices (no SMART needed) -->
			{@const wholeDiskDevices = blockDevices.filter(d => !d.path.match(/\d+$/) || d.dev_type === 'disk')}
			{#if wholeDiskDevices.length === 0}
				<p class="text-sm text-muted-foreground">No disk devices detected.</p>
			{:else}
				<table class="w-full text-sm">
					<thead>
						<tr>
							<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Device</th>
							<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Type</th>
							<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Capacity</th>
							<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Filesystem</th>
							<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Mount</th>
						</tr>
					</thead>
					<tbody>
						{#each blockDevices as dev}
							<tr class="border-b border-border">
								<td class="p-3 font-mono text-sm">{dev.path}</td>
								<td class="p-3"><Badge variant="secondary" class={deviceClassBadge(dev.device_class)}>{dev.device_class.toUpperCase()}</Badge></td>
								<td class="p-3 text-sm">{formatBytes(dev.size_bytes)}</td>
								<td class="p-3 font-mono text-xs text-muted-foreground">{dev.fs_type ?? '—'}</td>
								<td class="p-3 font-mono text-xs text-muted-foreground">{dev.mount_point ?? '—'}</td>
							</tr>
						{/each}
					</tbody>
				</table>
				{#if !smartProtocol?.enabled && !isVirtual}
					<p class="mt-3 text-xs text-muted-foreground">Enable SMART monitoring for detailed topology with controller, model, serial, and health info.</p>
				{/if}
			{/if}
		{/if}
	{/if}
{/if}
