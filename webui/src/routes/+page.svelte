<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatUptime, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { SystemInfo, SystemHealth, SystemStats, Pool, DiskHealth, DiskIoStats, NetIfStats, ActiveAlert, Settings } from '$lib/types';
	import { Card, CardContent, CardHeader, CardTitle } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';

	let info: SystemInfo | null = $state(null);
	let health: SystemHealth | null = $state(null);
	let stats: SystemStats | null = $state(null);
	let pools: Pool[] = $state([]);
	let disks: DiskHealth[] = $state([]);
	let settings: Settings | null = $state(null);
	let alerts: ActiveAlert[] = $state([]);
	let refreshTimer: ReturnType<typeof setInterval> | null = null;

	let prevDiskIo: DiskIoStats[] = $state([]);
	let prevNetIo: NetIfStats[] = $state([]);
	let prevSampleTime = $state(0);
	let diskIoRates: Map<string, { readRate: number; writeRate: number }> = $state(new Map());
	let netIoRates: Map<string, { rxRate: number; txRate: number }> = $state(new Map());

	const client = getClient();

	onMount(() => {
		loadAll();
		refreshTimer = setInterval(refreshStats, 5000);
		return () => { if (refreshTimer) clearInterval(refreshTimer); };
	});

	async function loadAll() {
		await withToast(async () => {
			[info, health, stats, pools, disks, settings, alerts] = await Promise.all([
				client.call<SystemInfo>('system.info'),
				client.call<SystemHealth>('system.health'),
				client.call<SystemStats>('system.stats'),
				client.call<Pool[]>('pool.list'),
				client.call<DiskHealth[]>('system.disks'),
				client.call<Settings>('system.settings.get'),
				client.call<ActiveAlert[]>('system.alerts'),
			]);
		});
		if (stats) {
			prevDiskIo = stats.disk_io;
			prevNetIo = stats.network;
			prevSampleTime = Date.now();
		}
	}

	async function refreshStats() {
		try {
			const newStats = await client.call<SystemStats>('system.stats');
			const now = Date.now();
			const elapsed = (now - prevSampleTime) / 1000;

			if (prevSampleTime > 0 && elapsed > 0) {
				const dRates = new Map<string, { readRate: number; writeRate: number }>();
				for (const curr of newStats.disk_io) {
					const prev = prevDiskIo.find(d => d.name === curr.name);
					if (prev) {
						dRates.set(curr.name, {
							readRate: Math.max(0, (curr.read_bytes - prev.read_bytes) / elapsed),
							writeRate: Math.max(0, (curr.write_bytes - prev.write_bytes) / elapsed),
						});
					}
				}
				diskIoRates = dRates;

				const nRates = new Map<string, { rxRate: number; txRate: number }>();
				for (const curr of newStats.network) {
					const prev = prevNetIo.find(n => n.name === curr.name);
					if (prev) {
						nRates.set(curr.name, {
							rxRate: Math.max(0, (curr.rx_bytes - prev.rx_bytes) / elapsed),
							txRate: Math.max(0, (curr.tx_bytes - prev.tx_bytes) / elapsed),
						});
					}
				}
				netIoRates = nRates;
			}

			prevDiskIo = newStats.disk_io;
			prevNetIo = newStats.network;
			prevSampleTime = now;
			stats = newStats;
			alerts = await client.call<ActiveAlert[]>('system.alerts');
		} catch {
			// Silently ignore refresh errors
		}
	}

	function cpuPercent(s: SystemStats): number {
		return Math.min(100, (s.cpu.load_1 / s.cpu.count) * 100);
	}

	function memPercent(s: SystemStats): number {
		if (s.memory.total_bytes === 0) return 0;
		return (s.memory.used_bytes / s.memory.total_bytes) * 100;
	}

	function totalStorage(p: Pool[]): { used: number; total: number } {
		let used = 0, total = 0;
		for (const pool of p) {
			if (pool.total_bytes > 0) {
				used += pool.used_bytes;
				total += pool.total_bytes;
			}
		}
		return { used, total };
	}

	function barColor(percent: number): string {
		if (percent > 90) return 'bg-red-500';
		if (percent > 75) return 'bg-amber-500';
		return 'bg-primary';
	}
</script>

<h1 class="mb-4 text-2xl font-bold">Dashboard</h1>

{#if alerts.length > 0}
	<div class="mb-4 flex flex-col gap-2">
		{#each alerts as alert}
			<div class="flex items-center gap-3 rounded-lg border px-4 py-2.5 text-sm {
				alert.severity === 'critical' ? 'border-red-800 bg-red-950 text-red-200' : 'border-amber-800 bg-amber-950 text-amber-200'
			}">
				<span class="shrink-0 text-base font-bold">{alert.severity === 'critical' ? '!' : '⚠'}</span>
				<span class="flex-1">{alert.message}</span>
				<a class="text-xs opacity-70 hover:opacity-100" href="/alerts">Configure</a>
			</div>
		{/each}
	</div>
{/if}

<div class="grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] gap-4">
	{#if info}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">System</CardTitle>
			</CardHeader>
			<CardContent>
				<dl class="grid grid-cols-[100px_1fr] gap-1 text-sm">
					<dt class="text-muted-foreground">Hostname</dt><dd>{info.hostname}</dd>
					<dt class="text-muted-foreground">Version</dt><dd>{info.version}</dd>
					<dt class="text-muted-foreground">Kernel</dt><dd>{info.kernel}</dd>
					<dt class="text-muted-foreground">Uptime</dt><dd>{formatUptime(info.uptime_seconds)}</dd>
				</dl>
			</CardContent>
		</Card>
	{/if}

	{#if health}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Health</CardTitle>
			</CardHeader>
			<CardContent>
				<p class="mb-3 text-2xl font-bold {health.status === 'ok' ? 'text-green-400' : 'text-red-400'}">
					{health.status.toUpperCase()}
				</p>
				{#each health.services as svc}
					<div class="flex items-center gap-2 py-0.5 text-sm">
						<span class="h-2 w-2 shrink-0 rounded-full {svc.running ? 'bg-green-400' : 'bg-red-400'}"></span>
						{svc.name}
					</div>
				{/each}
			</CardContent>
		</Card>
	{/if}

	{#if stats}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">CPU</CardTitle>
			</CardHeader>
			<CardContent>
				<div class="text-2xl font-bold">{stats.cpu.load_1.toFixed(2)}</div>
				<div class="mb-2 text-xs text-muted-foreground">load avg ({stats.cpu.count} cores)</div>
				<div class="h-2 overflow-hidden rounded-full bg-secondary">
					<div class="h-full rounded-full transition-all duration-500 {barColor(cpuPercent(stats))}" style="width: {cpuPercent(stats)}%"></div>
				</div>
				<div class="mt-2 flex justify-between text-sm font-semibold">
					<span>{stats.cpu.load_1.toFixed(2)}</span>
					<span>{stats.cpu.load_5.toFixed(2)}</span>
					<span>{stats.cpu.load_15.toFixed(2)}</span>
				</div>
				<div class="flex justify-between text-xs text-muted-foreground">
					<span>1m</span><span>5m</span><span>15m</span>
				</div>
			</CardContent>
		</Card>

		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Memory</CardTitle>
			</CardHeader>
			<CardContent>
				<div class="text-2xl font-bold">{formatBytes(stats.memory.used_bytes)}</div>
				<div class="mb-2 text-xs text-muted-foreground">of {formatBytes(stats.memory.total_bytes)} used</div>
				<div class="h-2 overflow-hidden rounded-full bg-secondary">
					<div class="h-full rounded-full transition-all duration-500 {barColor(memPercent(stats))}" style="width: {memPercent(stats)}%"></div>
				</div>
				<div class="mt-1 text-xs text-muted-foreground">{formatPercent(stats.memory.used_bytes, stats.memory.total_bytes)}</div>
				{#if stats.memory.swap_total_bytes > 0}
					<div class="mt-2 text-xs text-muted-foreground">
						Swap: {formatBytes(stats.memory.swap_used_bytes)} / {formatBytes(stats.memory.swap_total_bytes)}
					</div>
				{/if}
			</CardContent>
		</Card>
	{/if}

	{#if pools.length > 0}
		{@const storage = totalStorage(pools)}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Storage</CardTitle>
			</CardHeader>
			<CardContent>
				{#if storage.total > 0}
					<div class="text-2xl font-bold">{formatBytes(storage.used)}</div>
					<div class="mb-2 text-xs text-muted-foreground">of {formatBytes(storage.total)} used</div>
					<div class="h-2 overflow-hidden rounded-full bg-secondary">
						<div class="h-full rounded-full bg-primary transition-all duration-500" style="width: {(storage.used / storage.total) * 100}%"></div>
					</div>
					<div class="mt-1 text-xs text-muted-foreground">{formatPercent(storage.used, storage.total)}</div>
				{/if}
				<div class="mt-3">
					{#each pools as pool}
						<div class="flex items-center gap-2 border-t border-border py-1.5 text-sm first:border-t-0">
							<span class="font-semibold">{pool.name}</span>
							<Badge variant={pool.mounted ? 'default' : 'destructive'} class="text-[0.65rem]">
								{pool.mounted ? 'Mounted' : 'Unmounted'}
							</Badge>
							{#if pool.total_bytes > 0}
								<span class="ml-auto text-xs text-muted-foreground">{formatBytes(pool.used_bytes)} / {formatBytes(pool.total_bytes)}</span>
							{/if}
						</div>
					{/each}
				</div>
			</CardContent>
		</Card>
	{/if}

	{#if stats && stats.network.length > 0}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Network</CardTitle>
			</CardHeader>
			<CardContent>
				{#each stats.network as iface}
					{@const rates = netIoRates.get(iface.name)}
					<div class="border-t border-border py-2 first:border-t-0 first:pt-0">
						<div class="mb-1 flex items-center gap-2">
							<span class="text-sm font-semibold">{iface.name}</span>
							<span class="h-2 w-2 rounded-full {iface.up ? 'bg-green-400' : 'bg-red-400'}"></span>
							{#if iface.speed_mbps}
								<span class="ml-auto text-xs text-muted-foreground">{iface.speed_mbps >= 1000 ? `${iface.speed_mbps / 1000}G` : `${iface.speed_mbps}M`}</span>
							{/if}
						</div>
						<div class="flex gap-4 text-xs">
							<div class="flex gap-1.5">
								<span class="font-semibold text-muted-foreground">RX</span>
								<span class="tabular-nums font-semibold">{rates ? `${formatBytes(rates.rxRate)}/s` : formatBytes(iface.rx_bytes)}</span>
							</div>
							<div class="flex gap-1.5">
								<span class="font-semibold text-muted-foreground">TX</span>
								<span class="tabular-nums font-semibold">{rates ? `${formatBytes(rates.txRate)}/s` : formatBytes(iface.tx_bytes)}</span>
							</div>
							{#if rates}
								<div class="ml-auto flex gap-1.5">
									<span class="text-muted-foreground">Total</span>
									<span>{formatBytes(iface.rx_bytes + iface.tx_bytes)}</span>
								</div>
							{/if}
						</div>
					</div>
				{/each}
			</CardContent>
		</Card>
	{/if}

	{#if stats && stats.disk_io.length > 0}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Disk I/O</CardTitle>
			</CardHeader>
			<CardContent>
				{#each stats.disk_io as dio}
					{@const rates = diskIoRates.get(dio.name)}
					<div class="border-t border-border py-2 first:border-t-0 first:pt-0">
						<div class="mb-1 flex items-center gap-2">
							<span class="text-sm font-semibold">{dio.name}</span>
							{#if dio.io_in_progress > 0}
								<span class="rounded bg-amber-500/15 px-1.5 py-0.5 text-[0.7rem] text-amber-500">{dio.io_in_progress} active</span>
							{/if}
						</div>
						<div class="flex gap-4 text-xs">
							<div class="flex gap-1.5">
								<span class="font-bold text-muted-foreground">R</span>
								{#if rates}
									<span class="tabular-nums font-semibold">{formatBytes(rates.readRate)}/s</span>
								{:else}
									<span class="text-muted-foreground">{formatBytes(dio.read_bytes)}</span>
								{/if}
							</div>
							<div class="flex gap-1.5">
								<span class="font-bold text-muted-foreground">W</span>
								{#if rates}
									<span class="tabular-nums font-semibold">{formatBytes(rates.writeRate)}/s</span>
								{:else}
									<span class="text-muted-foreground">{formatBytes(dio.write_bytes)}</span>
								{/if}
							</div>
							<div class="ml-auto flex gap-1.5">
								<span class="text-muted-foreground">Total</span>
								<span>{formatBytes(dio.read_bytes + dio.write_bytes)}</span>
							</div>
						</div>
					</div>
				{/each}
			</CardContent>
		</Card>
	{/if}

	{#if settings?.smart_enabled && disks.length > 0}
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Disk Health</CardTitle>
			</CardHeader>
			<CardContent>
				{#each disks as disk}
					<div class="border-t border-border py-2 first:border-t-0 first:pt-0">
						<div class="mb-0.5 flex items-center gap-2">
							<span class="h-2 w-2 rounded-full {disk.health_passed ? 'bg-green-400' : 'bg-red-400'}"></span>
							<span class="font-mono text-sm font-semibold">{disk.device}</span>
							<span class="ml-auto rounded px-1.5 py-0.5 text-[0.7rem] font-semibold {disk.health_passed ? 'bg-green-950 text-green-400' : 'bg-red-950 text-red-400'}">
								{disk.smart_status}
							</span>
						</div>
						<div class="flex gap-3 pl-4 text-xs">
							<span>{disk.model}</span>
							<span class="text-muted-foreground">{formatBytes(disk.capacity_bytes)}</span>
							{#if disk.temperature_c != null}
								<span class="font-semibold {disk.temperature_c > 50 ? 'text-amber-500' : ''}">{disk.temperature_c}°C</span>
							{/if}
						</div>
					</div>
				{/each}
				<div class="mt-3 text-xs"><a href="/disks" class="text-primary hover:underline">View details</a></div>
			</CardContent>
		</Card>
	{/if}
</div>
