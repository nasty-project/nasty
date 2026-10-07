<script lang="ts">
	import type { DashboardDensity } from '$lib/dashboard.svelte';
	import type { DiskHealth, Filesystem, FilesystemDevice, FsDeviceUsage, FsUsage } from '$lib/types';
	import { formatBytes, formatPercent } from '$lib/format';
	import { formatTemp } from '$lib/temperature.svelte';
	import { memberUsage, memberHealthEntries, memberRate } from '$lib/storage-member-statistics';
	import { Card, CardContent, CardHeader, CardTitle } from '$lib/components/ui/card';
	import { Database, TriangleAlert } from '@lucide/svelte';

	type DiskRate = { readRate: number; writeRate: number };
	let { filesystems, usages, health, rates, density }: {
		filesystems: Filesystem[];
		usages: Record<string, FsUsage>;
		health: DiskHealth[];
		rates: Map<string, DiskRate>;
		density: DashboardDensity;
	} = $props();

	function deviceName(path: string): string {
		return path.split('/').filter(Boolean).at(-1) ?? path;
	}

	function usageFor(filesystem: Filesystem, device: FilesystemDevice): FsDeviceUsage | undefined {
		return memberUsage(device, usages[filesystem.name]?.devices ?? []);
	}

	function healthEntriesFor(device: FilesystemDevice): DiskHealth[] {
		return memberHealthEntries(device, health);
	}

	function healthFor(device: FilesystemDevice): DiskHealth | undefined {
		const matches = healthEntriesFor(device);
		return matches.length === 1 ? matches[0] : undefined;
	}

	function smartFailed(device: FilesystemDevice): boolean {
		return healthEntriesFor(device).some((disk) => disk.smart_status !== 'UNAVAILABLE' && !disk.health_passed);
	}

	function smartUnavailable(device: FilesystemDevice): boolean {
		const matches = healthEntriesFor(device);
		return matches.length > 0 && matches.every((disk) => disk.smart_status === 'UNAVAILABLE');
	}

	function rateFor(device: FilesystemDevice): DiskRate | undefined {
		return memberRate(device, rates);
	}

	function filesystemState(filesystem: Filesystem): { label: string; className: string } {
		if (filesystem.devices.some((device) => device.missing)) return { label: 'Missing member', className: 'text-red-400' };
		if (!filesystem.mounted) return { label: 'Unmounted', className: 'text-amber-400' };
		if (filesystem.devices.some((device) => device.state === 'failed' || smartFailed(device))) return { label: 'Failed member', className: 'text-red-400' };
		if (filesystem.devices.some((device) => device.state === 'ro')) return { label: 'Read-only member', className: 'text-amber-400' };
		if (filesystem.devices.some((device) => device.state === 'evacuating')) return { label: 'Evacuating', className: 'text-amber-400' };
		if (filesystem.devices.some((device) => (device.read_errors ?? 0) + (device.write_errors ?? 0) + (device.checksum_errors ?? 0) > 0)) return { label: 'Errors recorded', className: 'text-amber-400' };
		if (filesystem.used_bytes + filesystem.available_bytes <= 0) return { label: 'Capacity unavailable', className: 'text-amber-400' };
		return { label: 'Healthy', className: 'text-emerald-400' };
	}

	function deviceState(device: FilesystemDevice): { label: string; className: string } {
		if (device.missing) return { label: 'missing', className: 'border-red-800 bg-red-950 text-red-300' };
		if (smartFailed(device)) return { label: 'SMART fail', className: 'border-red-800 bg-red-950 text-red-300' };
		if (device.state === 'failed') return { label: 'failed', className: 'border-red-800 bg-red-950 text-red-300' };
		if (device.state === 'evacuating') return { label: 'evacuating', className: 'border-amber-700 bg-amber-950 text-amber-300' };
		if (device.state === 'ro') return { label: 'ro', className: 'border-amber-700 bg-amber-950 text-amber-300' };
		if (smartUnavailable(device)) return { label: 'SMART unavailable', className: 'border-border bg-muted text-muted-foreground' };
		return { label: device.state ?? 'online', className: 'border-emerald-800 bg-emerald-950 text-emerald-300' };
	}

	function policy(filesystem: Filesystem): string {
		const data = filesystem.options.erasure_code ? 'EC data' : `Data x${filesystem.options.data_replicas ?? 1}`;
		return `${data} - Metadata x${filesystem.options.metadata_replicas ?? 1}`;
	}

	function barColor(percent: number): string {
		if (percent > 90) return 'bg-red-500';
		if (percent > 75) return 'bg-amber-500';
		return 'bg-emerald-500';
	}
</script>

<Card class="overflow-hidden">
	<CardHeader class={density === 'compact' ? 'px-4 py-2.5' : 'pb-2'}>
		<CardTitle class="flex items-center justify-between gap-3 text-xs uppercase tracking-wide text-muted-foreground">
			<span class="flex items-center gap-2"><Database class="h-4 w-4" /> Compact storage</span>
			<a href="/filesystems" class="normal-case tracking-normal text-primary hover:underline">Manage filesystems</a>
		</CardTitle>
	</CardHeader>
	<CardContent class="p-0">
		{#if filesystems.length === 0}
			<div class="px-4 pb-4 text-sm text-muted-foreground">No filesystems configured.</div>
		{:else}
			<div class="divide-y divide-border">
				{#each filesystems as filesystem (filesystem.uuid)}
					{@const state = filesystemState(filesystem)}
					{@const userCapacity = filesystem.used_bytes + filesystem.available_bytes}
					{@const usedPercent = userCapacity > 0 ? filesystem.used_bytes / userCapacity * 100 : 0}
					<section>
						<div class="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-border bg-muted/35 px-4 py-2.5">
							<div class="min-w-36"><div class="flex items-center gap-2"><a href="/filesystems" class="font-semibold hover:text-primary">{filesystem.name}</a>{#if state.label !== 'Healthy'}<TriangleAlert class="h-3.5 w-3.5 text-amber-400" />{/if}</div><div class="text-[0.68rem] text-muted-foreground">{policy(filesystem)}</div></div>
							<div class="min-w-48 flex-1">
								{#if userCapacity > 0}
									<div class="mb-1 flex justify-between text-xs tabular-nums"><span>{formatBytes(filesystem.used_bytes)} used</span><span class="text-muted-foreground">{formatBytes(filesystem.available_bytes)} available - {formatPercent(filesystem.used_bytes, userCapacity)}</span></div><div class="h-1.5 overflow-hidden rounded-full bg-secondary"><div class="h-full rounded-full {barColor(usedPercent)}" style="width: {Math.min(100, usedPercent)}%"></div></div>
								{:else}
									<div class="text-xs text-muted-foreground">Capacity data unavailable</div>
								{/if}
							</div>
							<span class="text-xs font-medium {state.className}">{state.label}</span>
						</div>
						<div class="overflow-x-auto">
							<table class="w-full min-w-[760px] text-left text-xs">
								<caption class="sr-only">Storage members for {filesystem.name}</caption>
								<thead class="text-[0.65rem] uppercase tracking-wide text-muted-foreground"><tr><th class="px-4 py-1.5 font-medium">Device</th><th class="px-3 py-1.5 font-medium">State</th><th class="px-3 py-1.5 text-right font-medium">Raw size</th><th class="px-3 py-1.5 text-right font-medium">Physical allocation</th><th class="px-3 py-1.5 text-right font-medium">Unallocated</th><th class="px-3 py-1.5 text-right font-medium">Read / Write</th><th class="px-4 py-1.5 text-right font-medium">Temp</th></tr></thead>
								<tbody class="divide-y divide-border/70">
									{#each filesystem.devices as device (device.uuid ?? device.path)}
										{@const usage = usageFor(filesystem, device)}
										{@const disk = healthFor(device)}
										{@const rate = rateFor(device)}
										{@const memberState = deviceState(device)}
										{@const allocationPercent = usage && usage.total_bytes > 0 ? usage.used_bytes / usage.total_bytes * 100 : 0}
										<tr class="hover:bg-muted/20">
											<td class={density === 'compact' ? 'px-4 py-1.5' : 'px-4 py-2.5'}><div class="flex items-center gap-2"><span class="font-mono font-semibold">{device.path}</span>{#if device.label}<span class="rounded bg-primary/10 px-1.5 py-0.5 text-[0.65rem] text-primary">{device.label}</span>{/if}</div><div class="mt-0.5 text-[0.65rem] text-muted-foreground">member {device.member_index ?? '-'}{device.durability === 0 ? ' - cache durability' : ''}</div></td>
											<td class="px-3"><span class="rounded border px-1.5 py-0.5 text-[0.65rem] {memberState.className}">{memberState.label}</span></td>
											<td class="px-3 text-right tabular-nums">{usage ? formatBytes(usage.total_bytes) : disk ? formatBytes(disk.capacity_bytes) : '-'}</td>
											<td class="px-3 text-right"><div class="tabular-nums">{usage ? formatBytes(usage.used_bytes) : '-'}</div>{#if usage}<div class="ml-auto mt-1 h-1 w-20 overflow-hidden rounded-full bg-secondary"><div class="h-full rounded-full {barColor(allocationPercent)}" style="width: {Math.min(100, allocationPercent)}%"></div></div>{/if}</td>
											<td class="px-3 text-right tabular-nums text-muted-foreground">{usage ? formatBytes(usage.free_bytes) : '-'}</td>
											<td class="px-3 text-right tabular-nums"><span class="text-cyan-400">{rate ? formatBytes(rate.readRate) + '/s' : '-'}</span><span class="mx-1 text-muted-foreground">/</span><span class="text-violet-400">{rate ? formatBytes(rate.writeRate) + '/s' : '-'}</span></td>
											<td class="px-4 text-right font-medium {smartFailed(device) ? 'text-red-400' : disk?.temperature_c != null && disk.temperature_c >= 50 ? 'text-amber-400' : disk?.temperature_c != null ? 'text-emerald-400' : 'text-muted-foreground'}">{formatTemp(disk?.temperature_c ?? null) ?? '-'}</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					</section>
				{/each}
			</div>
		{/if}
	</CardContent>
</Card>
