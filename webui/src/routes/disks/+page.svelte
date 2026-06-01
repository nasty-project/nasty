<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { formatTemp } from '$lib/temperature.svelte';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { BlockDevice, DiskHealth, ProtocolStatus, SmartAttribute } from '$lib/types';
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

	// NVMe "data units" are 1000 × 512-byte LBAs per spec (512,000 bytes
	// per unit). Most consumer drives still report in this fixed unit
	// regardless of formatted_lba_size, so the multiplication is safe.
	const NVME_DATA_UNIT_BYTES = 512_000;
	function formatNvmeDataUnits(units: number): string {
		return formatBytes(units * NVME_DATA_UNIT_BYTES);
	}

	function formatMinutes(minutes: number): string {
		if (minutes === 0) return '0';
		const days = Math.floor(minutes / 1440);
		const hours = Math.floor((minutes % 1440) / 60);
		if (days > 0) return `${days}d ${hours}h`;
		if (hours > 0) return `${hours}h ${minutes % 60}m`;
		return `${minutes}m`;
	}

	function deviceClassBadge(cls: string): string {
		switch (cls) {
			case 'nvme': return 'bg-purple-950 text-purple-400';
			case 'ssd': return 'bg-blue-950 text-blue-400';
			case 'mmc': return 'bg-amber-950 text-amber-400';
			case 'hdd': return 'bg-emerald-950 text-emerald-400';
			// SAS = enterprise transport, distinct colour so it doesn't get
			// visually conflated with consumer SATA hdd/ssd (#365).
			case 'sas': return 'bg-rose-950 text-rose-400';
			default: return 'bg-secondary text-muted-foreground';
		}
	}

	// SMART attribute IDs that warrant highlighting in the attribute
	// table. 22 = Helium_Level (HGST/WD helium-filled spinners) — when
	// it crosses its threshold the drive fails; treating it as critical
	// matches how operators of He10/He12 drives think about it.
	const criticalIds = new Set([5, 10, 22, 187, 188, 196, 197, 198]);

	// A small subset of SMART attribute IDs we surface as ATA-panel
	// tiles. The numbers are vendor-stable enough that picking by ID
	// rather than scraping by name avoids vendor naming drift
	// ("Reallocated_Sector_Ct" vs "Reallocated Sector Count" vs ...).
	const ATA_TILE_IDS = {
		reallocated: 5,
		spinRetry: 10,
		helium: 22,
		pendingSector: 197,
		offlineUncorrectable: 198,
		crcErrors: 199,
	} as const;

	function findAttr(disk: DiskHealth, id: number): SmartAttribute | undefined {
		return disk.attributes.find(a => a.id === id);
	}

	function smartFor(dev: BlockDevice): DiskHealth | undefined {
		return disks.find(d => d.device === dev.path || dev.path.startsWith(d.device));
	}

	// Identity for expand/collapse and Svelte keying. A block-device path
	// is no longer unique because RAID-tunneled drives share /dev/sda;
	// the (device, transport) pair is the physical-drive key.
	function diskKey(d: DiskHealth): string {
		return d.transport ? `${d.device}@${d.transport}` : d.device;
	}

	// Display label matching smartctl's own `info_name` convention so
	// operators see the same string they'd see running smartctl by hand.
	function diskLabel(d: DiskHealth): string {
		return d.transport ? `${d.device} [${d.transport}]` : d.device;
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
			{#each disks as disk (diskKey(disk))}
				{@const unavailable = disk.smart_status === 'UNAVAILABLE'}
				{@const key = diskKey(disk)}
				<Card class="mb-4 {!disk.health_passed && !unavailable ? 'border-red-900' : ''}">
					<CardContent class="pt-5">
						<div class="mb-4 flex items-center gap-4">
							<span class="rounded px-2.5 py-1 text-xs font-bold
								{unavailable
									? 'bg-muted text-muted-foreground'
									: disk.health_passed
										? 'bg-green-950 text-green-400'
										: 'bg-red-950 text-red-400'}">
								{disk.smart_status}
							</span>
							<div class="flex flex-1 items-baseline gap-3">
								<strong class="font-mono">{disk.device}</strong>
								{#if disk.transport}
									<span class="rounded bg-muted px-1.5 py-0.5 text-xs font-mono text-muted-foreground" title="smartctl transport flag — physical drive behind a RAID controller">{disk.transport}</span>
								{/if}
								{#if disk.ata_port}
									<span class="rounded bg-muted px-1.5 py-0.5 text-xs font-mono text-muted-foreground">{disk.ata_port}</span>
								{/if}
								<span class="text-sm text-muted-foreground">{disk.model}</span>
							</div>
							<Button variant="secondary" size="xs" onclick={() => expandedDisk = expandedDisk === key ? null : key}>
								{expandedDisk === key ? 'Hide' : 'Details'}
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
										{formatTemp(disk.temperature_c)}
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

						{#if expandedDisk === key && disk.attributes.length > 0}
							{@const reallocated = findAttr(disk, ATA_TILE_IDS.reallocated)}
							{@const pending = findAttr(disk, ATA_TILE_IDS.pendingSector)}
							{@const offlineUnc = findAttr(disk, ATA_TILE_IDS.offlineUncorrectable)}
							{@const crc = findAttr(disk, ATA_TILE_IDS.crcErrors)}
							{@const spinRetry = findAttr(disk, ATA_TILE_IDS.spinRetry)}
							{@const helium = findAttr(disk, ATA_TILE_IDS.helium)}
							{@const speedDowngraded = disk.ata?.interface_speed_current
								&& disk.ata?.interface_speed_max
								&& disk.ata.interface_speed_current !== disk.ata.interface_speed_max}
							<div class="mt-5 border-t border-border pt-4">
								<h4 class="mb-3 text-xs uppercase tracking-wide text-muted-foreground">ATA / SATA Health</h4>
								<div class="grid grid-cols-2 gap-x-6 gap-y-4 md:grid-cols-3 lg:grid-cols-4">
									{#if disk.ata?.interface_speed_current}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Interface Speed</span>
											<span class="text-sm font-semibold {speedDowngraded ? 'text-amber-500' : ''}" title={speedDowngraded ? 'Link trained below max — often a cable, backplane, or controller-port issue' : ''}>
												{disk.ata.interface_speed_current}{#if disk.ata.interface_speed_max && speedDowngraded}<span class="text-xs text-muted-foreground"> / max {disk.ata.interface_speed_max}</span>{/if}
											</span>
										</div>
									{/if}
									{#if reallocated}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Reallocated Sectors</span>
											<span class="text-sm font-semibold {reallocated.raw_value > 0 ? 'text-red-400' : ''}">{reallocated.raw_value.toLocaleString()}</span>
										</div>
									{/if}
									{#if pending}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Pending Sectors</span>
											<span class="text-sm font-semibold {pending.raw_value > 0 ? 'text-amber-500' : ''}">{pending.raw_value.toLocaleString()}</span>
										</div>
									{/if}
									{#if offlineUnc}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Offline Uncorrectable</span>
											<span class="text-sm font-semibold {offlineUnc.raw_value > 0 ? 'text-red-400' : ''}">{offlineUnc.raw_value.toLocaleString()}</span>
										</div>
									{/if}
									{#if crc}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">CRC Errors</span>
											<span class="text-sm font-semibold {crc.raw_value > 0 ? 'text-amber-500' : ''}" title="Transport-level errors on the SATA link — often cable / port issue">{crc.raw_value.toLocaleString()}</span>
										</div>
									{/if}
									{#if spinRetry && spinRetry.raw_value > 0}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Spin Retry Count</span>
											<span class="text-sm font-semibold text-amber-500">{spinRetry.raw_value.toLocaleString()}</span>
										</div>
									{/if}
									{#if helium}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Helium Level</span>
											<span class="text-sm font-semibold {helium.value < helium.threshold ? 'text-red-400' : helium.value < helium.threshold * 1.5 ? 'text-amber-500' : ''}" title="Internal helium pressure as a normalized percentage. Drive fails when this drops below its threshold.">
												{helium.value}% <span class="text-xs text-muted-foreground">(threshold {helium.threshold}%)</span>
											</span>
										</div>
									{/if}
								</div>
							</div>

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
						{:else if expandedDisk === key && disk.nvme}
							{@const n = disk.nvme}
							{@const spareLow = n.available_spare_percent <= n.available_spare_threshold_percent}
							<div class="mt-5 border-t border-border pt-4">
								<h4 class="mb-3 text-xs uppercase tracking-wide text-muted-foreground">NVMe Health</h4>
								<div class="grid grid-cols-2 gap-x-6 gap-y-4 md:grid-cols-3 lg:grid-cols-4">
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Endurance Used</span>
										<span class="text-sm font-semibold {n.percentage_used >= 100 ? 'text-red-400' : n.percentage_used >= 80 ? 'text-amber-500' : ''}">
											{n.percentage_used}%
										</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Available Spare</span>
										<span class="text-sm font-semibold {spareLow ? 'text-red-400' : ''}">
											{n.available_spare_percent}% <span class="text-xs text-muted-foreground">(threshold {n.available_spare_threshold_percent}%)</span>
										</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Critical Warning</span>
										<span class="text-sm font-semibold {n.critical_warning !== 0 ? 'text-red-400' : 'text-green-400'}">
											{n.critical_warning === 0 ? 'None' : `0x${n.critical_warning.toString(16).padStart(2, '0')}`}
										</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Media Errors</span>
										<span class="text-sm font-semibold {n.media_errors > 0 ? 'text-red-400' : ''}">{n.media_errors}</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Data Written</span>
										<span class="text-sm font-semibold">{formatNvmeDataUnits(n.data_units_written)}</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Data Read</span>
										<span class="text-sm font-semibold">{formatNvmeDataUnits(n.data_units_read)}</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Power Cycles</span>
										<span class="text-sm font-semibold">{n.power_cycles.toLocaleString()}</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Unsafe Shutdowns</span>
										<span class="text-sm font-semibold">{n.unsafe_shutdowns.toLocaleString()}</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Controller Busy</span>
										<span class="text-sm font-semibold">{formatMinutes(n.controller_busy_minutes)}</span>
									</div>
									<div class="flex flex-col">
										<span class="text-[0.7rem] uppercase text-muted-foreground">Error Log Entries</span>
										<span class="text-sm font-semibold {n.num_err_log_entries > 0 ? 'text-amber-500' : ''}">{n.num_err_log_entries.toLocaleString()}</span>
										{#if n.most_recent_error}
											<span class="text-xs text-muted-foreground" title="Status of the most recent entry in the NVMe error information log">{n.most_recent_error}</span>
										{/if}
									</div>
									{#if n.warning_temp_minutes > 0}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Above Warning Temp</span>
											<span class="text-sm font-semibold text-amber-500">{formatMinutes(n.warning_temp_minutes)}</span>
										</div>
									{/if}
									{#if n.critical_comp_minutes > 0}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Above Critical Temp</span>
											<span class="text-sm font-semibold text-red-400">{formatMinutes(n.critical_comp_minutes)}</span>
										</div>
									{/if}
									{#if n.temperature_sensors_c.length > 1}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Sensors</span>
											<span class="text-sm font-semibold">
												{n.temperature_sensors_c.map((t, i) => t == null ? `S${i + 1}: —` : `S${i + 1}: ${formatTemp(t)}`).join('  ')}
											</span>
										</div>
									{/if}
								</div>
							</div>
						{:else if expandedDisk === key && disk.scsi}
							{@const s = disk.scsi}
							{@const anyUncorrected = s.read_errors.uncorrected_total + s.write_errors.uncorrected_total + s.verify_errors.uncorrected_total > 0}
							<div class="mt-5 border-t border-border pt-4">
								<h4 class="mb-3 text-xs uppercase tracking-wide text-muted-foreground">SAS / SCSI Health</h4>
								<div class="grid grid-cols-2 gap-x-6 gap-y-4 md:grid-cols-3 lg:grid-cols-4">
									{#if s.transport_protocol}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Transport</span>
											<span class="text-sm font-semibold">{s.transport_protocol}</span>
										</div>
									{/if}
									{#if s.scsi_version}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">SCSI Version</span>
											<span class="text-sm font-semibold">{s.scsi_version}</span>
										</div>
									{/if}
									{#if s.rotation_rate != null}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Rotation</span>
											<span class="text-sm font-semibold">{s.rotation_rate === 0 ? 'SSD' : `${s.rotation_rate.toLocaleString()} RPM`}</span>
										</div>
									{/if}
									{#if s.form_factor}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Form Factor</span>
											<span class="text-sm font-semibold">{s.form_factor}</span>
										</div>
									{/if}
									{#if s.drive_trip_temp_c != null}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Drive Trip Temp</span>
											<span class="text-sm font-semibold">{formatTemp(s.drive_trip_temp_c)}</span>
										</div>
									{/if}
									{#if s.year_of_manufacture}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Manufactured</span>
											<span class="text-sm font-semibold">{s.year_of_manufacture}{s.week_of_manufacture ? ` w${s.week_of_manufacture}` : ''}</span>
										</div>
									{/if}
									{#if s.grown_defect_list != null}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Grown Defects</span>
											<span class="text-sm font-semibold {s.grown_defect_list > 10 ? 'text-red-400' : s.grown_defect_list > 0 ? 'text-amber-500' : ''}" title="Sectors moved to spare blocks since manufacture. Non-zero is normal on aging drives; sudden growth indicates wear.">{s.grown_defect_list.toLocaleString()}</span>
										</div>
									{/if}
									{#if s.power_on_minutes_since_format != null}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Hours Since Format</span>
											<span class="text-sm font-semibold">{Math.floor(s.power_on_minutes_since_format / 60).toLocaleString()}h</span>
										</div>
									{/if}
									{#if s.start_stop_cycles != null}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Start/Stop Cycles</span>
											<span class="text-sm font-semibold">
												{s.start_stop_cycles.toLocaleString()}{#if s.start_stop_cycles_designed}<span class="text-xs text-muted-foreground"> / {s.start_stop_cycles_designed.toLocaleString()}</span>{/if}
											</span>
										</div>
									{/if}
									{#if s.load_unload_cycles != null}
										<div class="flex flex-col">
											<span class="text-[0.7rem] uppercase text-muted-foreground">Load/Unload Cycles</span>
											<span class="text-sm font-semibold">
												{s.load_unload_cycles.toLocaleString()}{#if s.load_unload_cycles_designed}<span class="text-xs text-muted-foreground"> / {s.load_unload_cycles_designed.toLocaleString()}</span>{/if}
											</span>
										</div>
									{/if}
								</div>

								<h5 class="mt-5 mb-2 text-[0.7rem] uppercase tracking-wide text-muted-foreground">I/O Error Counters {#if anyUncorrected}<span class="ml-2 rounded bg-red-950 px-1.5 py-0.5 text-[0.65rem] font-bold text-red-400">UNCORRECTED ERRORS</span>{/if}</h5>
								<table class="w-full text-xs">
									<thead>
										<tr>
											<th class="p-2 text-left text-[0.65rem] uppercase text-muted-foreground"></th>
											<th class="p-2 text-right text-[0.65rem] uppercase text-muted-foreground">Uncorrected</th>
											<th class="p-2 text-right text-[0.65rem] uppercase text-muted-foreground">Corrected</th>
											<th class="p-2 text-right text-[0.65rem] uppercase text-muted-foreground">GB Processed</th>
										</tr>
									</thead>
									<tbody>
										{#each [{ label: 'Read', e: s.read_errors }, { label: 'Write', e: s.write_errors }, { label: 'Verify', e: s.verify_errors }] as row}
											<tr class="border-t border-border/30">
												<td class="p-2 font-semibold">{row.label}</td>
												<td class="p-2 text-right font-mono {row.e.uncorrected_total > 0 ? 'font-bold text-red-400' : ''}">{row.e.uncorrected_total.toLocaleString()}</td>
												<td class="p-2 text-right font-mono text-muted-foreground">{row.e.corrected_total.toLocaleString()}</td>
												<td class="p-2 text-right font-mono text-muted-foreground">{row.e.gigabytes_processed.toLocaleString(undefined, { maximumFractionDigits: 1 })}</td>
											</tr>
										{/each}
									</tbody>
								</table>

								{#if s.last_self_test}
									{@const t = s.last_self_test}
									<h5 class="mt-5 mb-2 text-[0.7rem] uppercase tracking-wide text-muted-foreground">Self-Test History ({s.self_test_count} recorded)</h5>
									<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-sm">
										<span class="font-semibold">{t.code}</span>
										<span class="rounded px-1.5 py-0.5 text-[0.7rem] font-bold {t.in_progress ? 'bg-blue-950 text-blue-400' : t.passed ? 'bg-green-950 text-green-400' : 'bg-amber-950 text-amber-400'}">
											{t.in_progress ? 'IN PROGRESS' : t.passed ? 'PASSED' : 'ABORTED'}
										</span>
										<span class="text-xs text-muted-foreground">{t.result}</span>
										{#if t.power_on_hours != null && disk.power_on_hours != null}
											<span class="text-xs text-muted-foreground">
												— {(disk.power_on_hours - t.power_on_hours).toLocaleString()} hours ago
											</span>
										{/if}
									</div>
								{/if}
							</div>
						{:else if expandedDisk === key}
							<p class="mt-4 text-sm text-muted-foreground">No detailed SMART data available for this drive.</p>
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
							{#each group.disks as disk (diskKey(disk))}
								<tr class="border-b border-border">
									<td class="p-3 font-mono text-sm font-semibold">{disk.ata_port ?? '—'}</td>
									<td class="p-3 font-mono text-sm">{diskLabel(disk)}</td>
									<td class="p-3 text-sm">{disk.model}</td>
									<td class="p-3 font-mono text-xs text-muted-foreground">{disk.serial}</td>
									<td class="p-3 text-sm">{formatBytes(disk.capacity_bytes)}</td>
									<td class="p-3 text-sm {disk.temperature_c != null && disk.temperature_c > 55 ? 'text-red-400' : disk.temperature_c != null && disk.temperature_c > 45 ? 'text-amber-500' : ''}">
										{formatTemp(disk.temperature_c) ?? '—'}
									</td>
									<td class="p-3">
										<span class="rounded px-2 py-0.5 text-xs font-bold
											{disk.smart_status === 'UNAVAILABLE'
												? 'bg-muted text-muted-foreground'
												: disk.health_passed
													? 'bg-green-950 text-green-400'
													: 'bg-red-950 text-red-400'}">
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
