<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatUptime, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { SystemInfo, SystemHealth, SystemStats, Filesystem, DiskHealth, DiskIoStats, NetIfStats, ActiveAlert, Settings, ResourceHistory } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent, CardHeader, CardTitle } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { createIoHistory } from '$lib/history.svelte';
	import IoChart from '$lib/components/io-chart.svelte';
	import { goto } from '$app/navigation';
	import { ChevronDown, ChevronRight } from '@lucide/svelte';

	let info: SystemInfo | null = $state(null);
	let healthExpanded = $state(false);
	let health: SystemHealth | null = $state(null);
	let stats: SystemStats | null = $state(null);
	let filesystems: Filesystem[] = $state([]);
	let disks: DiskHealth[] = $state([]);
	let settings: Settings | null = $state(null);
	let alerts: ActiveAlert[] = $state([]);
	let refreshTimer: ReturnType<typeof setInterval> | null = null;
	let metricsRange = $state<'5m' | '1h' | '1d' | '7d' | '30d'>('5m');
	let metricsOffset = $state(0); // ms into the past, 0 = live

	const rangeDurations: Record<string, number> = {
		'5m': 300_000, '1h': 3_600_000, '1d': 86_400_000, '7d': 604_800_000, '30d': 2_592_000_000,
	};

	let prevDiskIo: DiskIoStats[] = $state([]);
	let prevNetIo: NetIfStats[] = $state([]);
	let prevSampleTime = $state(0);
	let diskIoRates: Map<string, { readRate: number; writeRate: number }> = $state(new Map());
	let netIoRates: Map<string, { rxRate: number; txRate: number }> = $state(new Map());
	let netSamples: Map<string, { time: Date; in: number; out: number }[]> = $state(new Map());
	let diskSamples: Map<string, { time: Date; in: number; out: number }[]> = $state(new Map());
	let cpuChartSamples: { time: Date; in: number; out: number }[] = $state([]);
	let memChartSamples: { time: Date; in: number; out: number }[] = $state([]);

	const netHistory = createIoHistory();
	const diskHistory = createIoHistory();
	const cpuHistory = createIoHistory();
	const memHistory = createIoHistory();

	const client = getClient();

	onMount(() => {
		loadAll();
		refreshTimer = setInterval(refreshStats, 15000);
		return () => { if (refreshTimer) clearInterval(refreshTimer); };
	});

	async function loadAll() {
		await withToast(async () => {
			[info, health, stats, filesystems, disks, settings, alerts] = await Promise.all([
				client.call<SystemInfo>('system.info'),
				client.call<SystemHealth>('system.health'),
				client.call<SystemStats>('system.stats'),
				client.call<Filesystem[]>('fs.list'),
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
		await loadMetrics();
	}

	async function loadMetrics() {
		try {
			const params = { range: metricsRange, ...(metricsOffset > 0 ? { offset: metricsOffset } : {}) };
			const [netHist, diskHist, cpuHist, memHist] = await Promise.all([
				client.call<ResourceHistory[]>('system.metrics.history', { kind: 'net', ...params }),
				client.call<ResourceHistory[]>('system.metrics.history', { kind: 'disk', ...params }),
				client.call<ResourceHistory[]>('system.metrics.history', { kind: 'cpu', ...params }),
				client.call<ResourceHistory[]>('system.metrics.history', { kind: 'mem', ...params }),
			]);

			netHistory.clear();
			diskHistory.clear();
			cpuHistory.clear();
			memHistory.clear();

			for (const rh of netHist) {
				for (const s of rh.samples) {
					netHistory.push(rh.name, new Date(s.ts), s.in_rate, s.out_rate);
				}
			}
			for (const rh of diskHist) {
				for (const s of rh.samples) {
					diskHistory.push(rh.name, new Date(s.ts), s.in_rate, s.out_rate);
				}
			}
			for (const rh of cpuHist) {
				for (const s of rh.samples) {
					cpuHistory.push('cpu', new Date(s.ts), s.in_rate, 0);
				}
			}
			for (const rh of memHist) {
				for (const s of rh.samples) {
					memHistory.push('mem', new Date(s.ts), s.in_rate, 0);
				}
			}
			if (stats) {
				netSamples = new Map(
					stats.network.map(n => [n.name, [...netHistory.getSamples(n.name)]])
				);
				diskSamples = new Map(
					stats.disk_io.map(d => [d.name, [...diskHistory.getSamples(d.name)]])
				);
			}
			cpuChartSamples = [...cpuHistory.getSamples('cpu')];
			memChartSamples = [...memHistory.getSamples('mem')];
		} catch {
			// Metrics history not available yet, charts will populate over time
		}
	}

	async function changeRange(r: typeof metricsRange) {
		metricsRange = r;
		metricsOffset = 0;
		await loadMetrics();
	}

	async function navigateBack() {
		metricsOffset += rangeDurations[metricsRange];
		await loadMetrics();
	}

	async function navigateForward() {
		metricsOffset = Math.max(0, metricsOffset - rangeDurations[metricsRange]);
		await loadMetrics();
	}

	async function navigateLive() {
		metricsOffset = 0;
		await loadMetrics();
	}

	async function refreshStats() {
		try {
			const newStats = await client.call<SystemStats>('system.stats');
			const now = Date.now();
			const elapsed = (now - prevSampleTime) / 1000;
			const sampleTime = new Date(now);

			if (prevSampleTime > 0 && elapsed > 0) {
				const dRates = new Map<string, { readRate: number; writeRate: number }>();
				for (const curr of newStats.disk_io) {
					const prev = prevDiskIo.find(d => d.name === curr.name);
					if (prev) {
						const readRate = Math.max(0, (curr.read_bytes - prev.read_bytes) / elapsed);
						const writeRate = Math.max(0, (curr.write_bytes - prev.write_bytes) / elapsed);
						dRates.set(curr.name, { readRate, writeRate });
						if (metricsRange === '5m' && metricsOffset === 0) diskHistory.push(curr.name, sampleTime, readRate, writeRate);
					}
				}
				diskIoRates = dRates;
				if (metricsRange === '5m' && metricsOffset === 0) diskSamples = new Map(
					newStats.disk_io.map(d => [d.name, [...diskHistory.getSamples(d.name)]])
				);

				const nRates = new Map<string, { rxRate: number; txRate: number }>();
				for (const curr of newStats.network) {
					const prev = prevNetIo.find(n => n.name === curr.name);
					if (prev) {
						const rxRate = Math.max(0, (curr.rx_bytes - prev.rx_bytes) / elapsed);
						const txRate = Math.max(0, (curr.tx_bytes - prev.tx_bytes) / elapsed);
						nRates.set(curr.name, { rxRate, txRate });
						if (metricsRange === '5m' && metricsOffset === 0) netHistory.push(curr.name, sampleTime, rxRate, txRate);
					}
				}
				netIoRates = nRates;
				if (metricsRange === '5m' && metricsOffset === 0) netSamples = new Map(
					newStats.network.map(n => [n.name, [...netHistory.getSamples(n.name)]])
				);

				// CPU and memory
				const cpuPct = Math.min(100, (newStats.cpu.load_1 / newStats.cpu.count) * 100);
				if (metricsRange === '5m' && metricsOffset === 0) {
					cpuHistory.push('cpu', sampleTime, cpuPct, 0);
					cpuChartSamples = [...cpuHistory.getSamples('cpu')];
				}

				const memPct = newStats.memory.total_bytes > 0
					? (newStats.memory.used_bytes / newStats.memory.total_bytes) * 100
					: 0;
				if (metricsRange === '5m' && metricsOffset === 0) {
					memHistory.push('mem', sampleTime, memPct, 0);
					memChartSamples = [...memHistory.getSamples('mem')];
				}
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

	function totalStorage(p: Filesystem[]): { used: number; total: number } {
		let used = 0, total = 0;
		for (const fs of p) {
			if (fs.total_bytes > 0) {
				used += fs.used_bytes;
				total += fs.total_bytes;
			}
		}
		return { used, total };
	}

	function storagePercent(p: Filesystem[]): number {
		const s = totalStorage(p);
		if (s.total === 0) return 0;
		return (s.used / s.total) * 100;
	}

	function barColor(percent: number): string {
		if (percent > 90) return 'bg-red-500';
		if (percent > 75) return 'bg-amber-500';
		return 'bg-primary';
	}

	function ipv4Only(addresses: string[]): string[] {
		return addresses.filter(a => !a.includes(':'));
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

<!-- System info bar -->
{#if info || health}
	<Card class="mb-4">
		<CardContent class="py-4">
			<div class="flex flex-wrap items-center gap-x-8 gap-y-2">
				{#if info}
					<div class="flex items-center gap-2">
						<span class="text-lg font-bold">{info.hostname}</span>
						<span class="text-xs text-muted-foreground">v{info.version}</span>
					</div>
					<div class="flex gap-4 text-sm text-muted-foreground">
						<span>Kernel {info.kernel}</span>
						<span>Up {formatUptime(info.uptime_seconds)}</span>
					</div>
				{/if}
				{#if health}
					<button
						onclick={() => healthExpanded = !healthExpanded}
						class="ml-auto flex items-center gap-3 hover:opacity-80 transition-opacity"
					>
						<span class="text-sm font-semibold {health.status === 'ok' ? 'text-green-400' : 'text-red-400'}">
							{health.status.toUpperCase()}
						</span>
						{#each health.services as svc}
							<div class="flex items-center gap-1.5 text-xs text-muted-foreground">
								<span class="h-1.5 w-1.5 rounded-full {svc.running ? 'bg-green-400' : 'bg-red-400'}"></span>
								{svc.name}
							</div>
						{/each}
						{#if healthExpanded}
							<ChevronDown class="h-4 w-4 text-muted-foreground" />
						{:else}
							<ChevronRight class="h-4 w-4 text-muted-foreground" />
						{/if}
					</button>
				{/if}
			</div>

			{#if healthExpanded && health}
				<div class="mt-4 border-t border-border pt-4">
					<div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
						{#each health.services as svc}
							<div class="rounded-md border border-border p-3">
								<div class="mb-2 flex items-center justify-between">
									<div class="flex items-center gap-2">
										<span class="h-2 w-2 rounded-full {svc.running ? 'bg-green-400' : 'bg-red-400'}"></span>
										<span class="text-sm font-medium">{svc.name}</span>
									</div>
									<span class="rounded-md px-2 py-0.5 text-xs font-medium {svc.running
										? 'border border-green-700 bg-green-950 text-green-400'
										: 'border border-red-700 bg-red-950 text-red-400'}">{svc.running ? 'Running' : 'Down'}</span>
								</div>
								{#if svc.running && svc.pid}
									<div class="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
										<div class="text-muted-foreground">PID</div>
										<div class="font-mono text-right">{svc.pid}</div>
										<div class="text-muted-foreground">Memory</div>
										<div class="font-mono text-right">{svc.memory_bytes != null ? formatBytes(svc.memory_bytes) : '—'}</div>
										<div class="text-muted-foreground">CPU Time</div>
										<div class="font-mono text-right">{svc.cpu_seconds != null ? svc.cpu_seconds.toFixed(1) + 's' : '—'}</div>
										<div class="text-muted-foreground">Uptime</div>
										<div class="font-mono text-right">{svc.uptime_seconds != null ? formatUptime(svc.uptime_seconds) : '—'}</div>
									</div>
								{/if}
							</div>
						{/each}
					</div>
				</div>
			{/if}
		</CardContent>
	</Card>
{/if}

<!-- Onboarding — shown only when no filesystems exist -->
{#if stats && filesystems.length === 0}
	<Card class="mb-4 border-primary/30 bg-primary/5">
		<CardContent class="flex items-center gap-6 py-6">
			<div class="flex-1">
				<h2 class="text-lg font-bold">Get started with NASty</h2>
				<p class="mt-1 text-sm text-muted-foreground">Create your first filesystem to start storing and sharing data.</p>
			</div>
			<Button onclick={() => goto('/filesystems')}>Create Filesystem</Button>
		</CardContent>
	</Card>
{/if}

<!-- Resource gauges -->
{#if stats}
	<div class="mb-3 flex items-center justify-between">
		<div class="flex items-center gap-2">
			<span class="text-sm font-semibold">History</span>
			{#if metricsOffset > 0}
				<span class="text-xs text-muted-foreground">
					{new Date(Date.now() - metricsOffset - rangeDurations[metricsRange]).toLocaleTimeString()} — {new Date(Date.now() - metricsOffset).toLocaleTimeString()}
				</span>
			{/if}
		</div>
		<div class="flex items-center gap-1">
			<button
				onclick={navigateBack}
				class="rounded-md border border-border px-2 py-1 text-xs text-muted-foreground hover:bg-accent hover:text-foreground transition-colors"
				title="Back"
			>&larr;</button>
			<div class="flex rounded-md border border-border">
				{#each (['5m', '1h', '1d', '7d', '30d'] as const) as r}
					<button
						onclick={() => changeRange(r)}
						class="px-3 py-1 text-xs font-medium transition-colors first:rounded-l-md last:rounded-r-md {metricsRange === r ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
					>{r}</button>
				{/each}
			</div>
			<button
				onclick={navigateForward}
				disabled={metricsOffset === 0}
				class="rounded-md border border-border px-2 py-1 text-xs text-muted-foreground hover:bg-accent hover:text-foreground transition-colors disabled:opacity-30 disabled:cursor-default"
				title="Forward"
			>&rarr;</button>
			{#if metricsOffset > 0}
				<button
					onclick={navigateLive}
					class="rounded-md border border-border px-2 py-1 text-xs font-medium text-primary hover:bg-accent transition-colors"
				>Live</button>
			{/if}
		</div>
	</div>
	<!-- Stats summary row -->
	<div class="mb-4 grid grid-cols-2 gap-4 sm:grid-cols-4">
		<Card>
			<CardContent class="pt-4 pb-3">
				<div class="text-xs uppercase tracking-wide text-muted-foreground">CPU Load</div>
				<div class="mt-1 flex items-baseline gap-2">
					<span class="text-2xl font-bold">{stats.cpu.load_1.toFixed(2)}</span>
					<span class="text-xs text-muted-foreground">/ {stats.cpu.count} cores</span>
				</div>
				<div class="mt-2 h-1.5 overflow-hidden rounded-full bg-secondary">
					<div class="h-full rounded-full transition-all duration-500 {barColor(cpuPercent(stats))}" style="width: {cpuPercent(stats)}%"></div>
				</div>
				<div class="mt-1.5 flex justify-between text-xs tabular-nums text-muted-foreground">
					<span>1m {stats.cpu.load_1.toFixed(2)}</span>
					<span>5m {stats.cpu.load_5.toFixed(2)}</span>
					<span>15m {stats.cpu.load_15.toFixed(2)}</span>
				</div>
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-4 pb-3">
				<div class="text-xs uppercase tracking-wide text-muted-foreground">Memory</div>
				<div class="mt-1 flex items-baseline gap-2">
					<span class="text-2xl font-bold">{formatPercent(stats.memory.used_bytes, stats.memory.total_bytes)}</span>
					<span class="text-xs text-muted-foreground">{formatBytes(stats.memory.used_bytes)} / {formatBytes(stats.memory.total_bytes)}</span>
				</div>
				<div class="mt-2 h-1.5 overflow-hidden rounded-full bg-secondary">
					<div class="h-full rounded-full transition-all duration-500 {barColor(memPercent(stats))}" style="width: {memPercent(stats)}%"></div>
				</div>
				{#if stats.memory.swap_total_bytes > 0}
					<div class="mt-1.5 text-xs text-muted-foreground">
						Swap: {formatBytes(stats.memory.swap_used_bytes)} / {formatBytes(stats.memory.swap_total_bytes)}
					</div>
				{/if}
			</CardContent>
		</Card>

		{#if stats.cpu.temp_c != null || stats.cpu.freq_mhz != null}
		<Card>
			<CardContent class="pt-4 pb-3">
				<div class="text-xs uppercase tracking-wide text-muted-foreground">CPU Temp</div>
				<div class="mt-1 flex items-baseline gap-2">
					<span class="text-2xl font-bold">{stats.cpu.temp_c != null ? stats.cpu.temp_c + '°C' : '—'}</span>
				</div>
				<div class="mt-2 text-xs text-muted-foreground">
					{#if stats.cpu.freq_mhz != null}
						{stats.cpu.freq_mhz >= 1000 ? (stats.cpu.freq_mhz / 1000).toFixed(1) + ' GHz' : stats.cpu.freq_mhz + ' MHz'}
					{/if}
					{#if stats.cpu.governor}
						<span class="ml-2">{stats.cpu.governor}</span>
					{/if}
				</div>
			</CardContent>
		</Card>
		{/if}

		{#if filesystems.length > 0}
			{@const storage = totalStorage(filesystems)}
			<Card>
				<CardContent class="pt-4 pb-3">
					<div class="text-xs uppercase tracking-wide text-muted-foreground">Storage</div>
					<div class="mt-1 flex items-baseline gap-2">
						<span class="text-2xl font-bold">{formatPercent(storage.used, storage.total)}</span>
						<span class="text-xs text-muted-foreground">{formatBytes(storage.used)} / {formatBytes(storage.total)}</span>
					</div>
					<div class="mt-2 h-1.5 overflow-hidden rounded-full bg-secondary">
						<div class="h-full rounded-full transition-all duration-500 {barColor(storagePercent(filesystems))}" style="width: {storagePercent(filesystems)}%"></div>
					</div>
					<div class="mt-1.5 text-xs text-muted-foreground">
						{filesystems.length} filesystem{filesystems.length !== 1 ? 's' : ''}
					</div>
				</CardContent>
			</Card>
		{/if}
	</div>

	<!-- Charts row — equal height, aligned -->
	<div class="mb-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">CPU Usage</CardTitle>
			</CardHeader>
			<CardContent>
				<IoChart
					samples={cpuChartSamples}
					inLabel="Usage"
					inColor="var(--chart-3)"
					yFormat={(v) => v.toFixed(0) + '%'}
					tooltipFormat={(v) => v.toFixed(1) + '%'}
				/>
			</CardContent>
		</Card>
		<Card>
			<CardHeader class="pb-2">
				<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Memory Usage</CardTitle>
			</CardHeader>
			<CardContent>
				<IoChart
					samples={memChartSamples}
					inLabel="Used"
					inColor="var(--chart-5)"
					yFormat={(v) => v.toFixed(0) + '%'}
					tooltipFormat={(v) => v.toFixed(1) + '%'}
				/>
			</CardContent>
		</Card>
	</div>

{/if}

<!-- Network & Disk I/O -->
{#if stats}
	<div class="mb-4 grid grid-cols-1 gap-4 lg:grid-cols-2">
		{#if stats.network.length > 0}
			<Card>
				<CardHeader class="pb-2">
					<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Network</CardTitle>
				</CardHeader>
				<CardContent>
					<div class="divide-y divide-border">
						{#each stats.network as iface}
							{@const rates = netIoRates.get(iface.name)}
							{@const ips = ipv4Only(iface.addresses)}
							{@const samples = netSamples.get(iface.name) ?? []}
							<div class="py-2.5 first:pt-0 last:pb-0">
								<div class="mb-1.5 flex items-center gap-2">
									<span class="text-sm font-semibold">{iface.name}</span>
									<span class="h-2 w-2 rounded-full {iface.up ? 'bg-green-400' : 'bg-red-400'}"></span>
									{#if iface.speed_mbps}
										<span class="text-xs text-muted-foreground">{iface.speed_mbps >= 1000 ? `${iface.speed_mbps / 1000}G` : `${iface.speed_mbps}M`}</span>
									{/if}
									{#if ips.length > 0}
										<span class="ml-auto font-mono text-xs">{ips.join(', ')}</span>
									{/if}
								</div>
								<div class="mb-2 flex gap-6 text-xs">
									<div class="flex items-center gap-1.5">
										<span class="w-5 text-right font-semibold text-muted-foreground">RX</span>
										<span class="tabular-nums font-semibold">{rates ? `${formatBytes(rates.rxRate)}/s` : formatBytes(iface.rx_bytes)}</span>
									</div>
									<div class="flex items-center gap-1.5">
										<span class="w-5 text-right font-semibold text-muted-foreground">TX</span>
										<span class="tabular-nums font-semibold">{rates ? `${formatBytes(rates.txRate)}/s` : formatBytes(iface.tx_bytes)}</span>
									</div>
									<div class="ml-auto flex items-center gap-1.5 text-muted-foreground">
										<span>Total</span>
										<span class="tabular-nums">{formatBytes(iface.rx_bytes + iface.tx_bytes)}</span>
									</div>
								</div>
								<IoChart
									{samples}
									inLabel="RX"
									outLabel="TX"
									inColor="var(--chart-2)"
									outColor="var(--chart-1)"
								/>
							</div>
						{/each}
					</div>
				</CardContent>
			</Card>
		{/if}

		{#if stats.disk_io.length > 0}
			<Card>
				<CardHeader class="pb-2">
					<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">Disk I/O</CardTitle>
				</CardHeader>
				<CardContent>
					<div class="divide-y divide-border">
						{#each stats.disk_io as dio}
							{@const rates = diskIoRates.get(dio.name)}
							{@const samples = diskSamples.get(dio.name) ?? []}
							<div class="py-2.5 first:pt-0 last:pb-0">
								<div class="mb-1.5 flex items-center gap-2">
									<span class="text-sm font-semibold">{dio.name}</span>
									{#if dio.io_in_progress > 0}
										<span class="rounded bg-amber-500/15 px-1.5 py-0.5 text-[0.65rem] font-medium text-amber-500">{dio.io_in_progress} active</span>
									{/if}
									<span class="ml-auto text-xs tabular-nums text-muted-foreground">{formatBytes(dio.read_bytes + dio.write_bytes)}</span>
								</div>
								<div class="mb-2 flex gap-6 text-xs">
									<div class="flex items-center gap-1.5">
										<span class="w-5 text-right font-bold text-muted-foreground">R</span>
										{#if rates}
											<span class="tabular-nums font-semibold">{formatBytes(rates.readRate)}/s</span>
										{:else}
											<span class="tabular-nums text-muted-foreground">{formatBytes(dio.read_bytes)}</span>
										{/if}
									</div>
									<div class="flex items-center gap-1.5">
										<span class="w-5 text-right font-bold text-muted-foreground">W</span>
										{#if rates}
											<span class="tabular-nums font-semibold">{formatBytes(rates.writeRate)}/s</span>
										{:else}
											<span class="tabular-nums text-muted-foreground">{formatBytes(dio.write_bytes)}</span>
										{/if}
									</div>
								</div>
								<IoChart
									{samples}
									inLabel="Read"
									outLabel="Write"
									inColor="var(--chart-2)"
									outColor="var(--chart-4)"
								/>
							</div>
						{/each}
					</div>
				</CardContent>
			</Card>
		{/if}
	</div>
{/if}

<!-- Disk Health -->
{#if disks.length > 0}
	<Card>
		<CardHeader class="pb-2">
			<CardTitle class="text-xs uppercase tracking-wide text-muted-foreground">S.M.A.R.T.</CardTitle>
		</CardHeader>
		<CardContent>
			<div class="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-3">
				{#each disks as disk}
					<div class="flex items-center gap-3 rounded-lg border border-border px-3 py-2">
						<span class="h-2.5 w-2.5 shrink-0 rounded-full {disk.health_passed ? 'bg-green-400' : 'bg-red-400'}"></span>
						<div class="min-w-0 flex-1">
							<div class="flex items-center gap-2">
								<span class="font-mono text-sm font-semibold">{disk.device}</span>
								<span class="rounded px-1.5 py-0.5 text-[0.6rem] font-semibold {disk.health_passed ? 'bg-green-950 text-green-400' : 'bg-red-950 text-red-400'}">
									{disk.smart_status}
								</span>
							</div>
							<div class="flex gap-2 text-xs text-muted-foreground">
								<span class="truncate">{disk.model}</span>
								<span class="shrink-0">{formatBytes(disk.capacity_bytes)}</span>
								{#if disk.temperature_c != null}
									<span class="shrink-0 font-semibold {disk.temperature_c > 50 ? 'text-amber-500' : ''}">{disk.temperature_c}°C</span>
								{/if}
							</div>
						</div>
					</div>
				{/each}
			</div>
			<div class="mt-3 text-xs"><a href="/disks" class="text-primary hover:underline">View details</a></div>
		</CardContent>
	</Card>
{/if}
