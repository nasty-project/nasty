<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatUptime, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { SystemInfo, SystemHealth, SystemStats, Pool, DiskHealth, DiskIoStats, NetIfStats, ActiveAlert } from '$lib/types';

	let info: SystemInfo | null = $state(null);
	let health: SystemHealth | null = $state(null);
	let stats: SystemStats | null = $state(null);
	let pools: Pool[] = $state([]);
	let disks: DiskHealth[] = $state([]);
	let alerts: ActiveAlert[] = $state([]);
	let refreshTimer: ReturnType<typeof setInterval> | null = null;

	// For computing rates between samples
	let prevDiskIo: DiskIoStats[] = $state([]);
	let prevNetIo: NetIfStats[] = $state([]);
	let prevSampleTime = $state(0);
	let diskIoRates: Map<string, { readRate: number; writeRate: number }> = $state(new Map());
	let netIoRates: Map<string, { rxRate: number; txRate: number }> = $state(new Map());

	const client = getClient();

	onMount(() => {
		loadAll();
		// Refresh stats every 5 seconds
		refreshTimer = setInterval(refreshStats, 5000);
		return () => { if (refreshTimer) clearInterval(refreshTimer); };
	});

	async function loadAll() {
		await withToast(async () => {
			[info, health, stats, pools, disks, alerts] = await Promise.all([
				client.call<SystemInfo>('system.info'),
				client.call<SystemHealth>('system.health'),
				client.call<SystemStats>('system.stats'),
				client.call<Pool[]>('pool.list'),
				client.call<DiskHealth[]>('system.disks'),
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
				// Disk I/O rates
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

				// Network I/O rates
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

			// Refresh alerts too
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
		if (percent > 90) return '#dc2626';
		if (percent > 75) return '#f59e0b';
		return '#2563eb';
	}
</script>

<h1>Dashboard</h1>

{#if alerts.length > 0}
	<div class="alerts-banner">
		{#each alerts as alert}
			<div class="alert" class:alert-warning={alert.severity === 'warning'} class:alert-critical={alert.severity === 'critical'}>
				<span class="alert-icon">{alert.severity === 'critical' ? '!' : '⚠'}</span>
				<span class="alert-msg">{alert.message}</span>
				<a class="alert-link" href="/alerts">Configure</a>
			</div>
		{/each}
	</div>
{/if}

<div class="cards">
	<!-- System Info -->
	{#if info}
		<div class="card">
			<h3>System</h3>
			<dl>
				<dt>Hostname</dt><dd>{info.hostname}</dd>
				<dt>Version</dt><dd>{info.version}</dd>
				<dt>Kernel</dt><dd>{info.kernel}</dd>
				<dt>Uptime</dt><dd>{formatUptime(info.uptime_seconds)}</dd>
			</dl>
		</div>
	{/if}

	<!-- Health -->
	{#if health}
		<div class="card">
			<h3>Health</h3>
			<p class="health-status" class:ok={health.status === 'ok'}>
				{health.status.toUpperCase()}
			</p>
			{#each health.services as svc}
				<div class="svc">
					<span class="dot" class:running={svc.running}></span>
					{svc.name}
				</div>
			{/each}
		</div>
	{/if}

	<!-- CPU -->
	{#if stats}
		<div class="card">
			<h3>CPU</h3>
			<div class="stat-value">{stats.cpu.load_1.toFixed(2)}</div>
			<div class="stat-label">load avg ({stats.cpu.count} cores)</div>
			<div class="bar-container">
				<div class="bar-fill" style="width: {cpuPercent(stats)}%; background: {barColor(cpuPercent(stats))}"></div>
			</div>
			<div class="stat-detail">
				<span>{stats.cpu.load_1.toFixed(2)}</span>
				<span>{stats.cpu.load_5.toFixed(2)}</span>
				<span>{stats.cpu.load_15.toFixed(2)}</span>
			</div>
			<div class="stat-detail labels">
				<span>1m</span>
				<span>5m</span>
				<span>15m</span>
			</div>
		</div>

		<!-- Memory -->
		<div class="card">
			<h3>Memory</h3>
			<div class="stat-value">{formatBytes(stats.memory.used_bytes)}</div>
			<div class="stat-label">of {formatBytes(stats.memory.total_bytes)} used</div>
			<div class="bar-container">
				<div class="bar-fill" style="width: {memPercent(stats)}%; background: {barColor(memPercent(stats))}"></div>
			</div>
			<div class="stat-sub">{formatPercent(stats.memory.used_bytes, stats.memory.total_bytes)}</div>
			{#if stats.memory.swap_total_bytes > 0}
				<div class="swap-info">
					Swap: {formatBytes(stats.memory.swap_used_bytes)} / {formatBytes(stats.memory.swap_total_bytes)}
				</div>
			{/if}
		</div>
	{/if}

	<!-- Storage Pools -->
	{#if pools.length > 0}
		{@const storage = totalStorage(pools)}
		<div class="card">
			<h3>Storage</h3>
			{#if storage.total > 0}
				<div class="stat-value">{formatBytes(storage.used)}</div>
				<div class="stat-label">of {formatBytes(storage.total)} used</div>
				<div class="bar-container">
					<div class="bar-fill" style="width: {(storage.used / storage.total) * 100}%; background: {barColor((storage.used / storage.total) * 100)}"></div>
				</div>
				<div class="stat-sub">{formatPercent(storage.used, storage.total)}</div>
			{/if}
			<div class="pool-list">
				{#each pools as pool}
					<div class="pool-row">
						<span class="pool-name">{pool.name}</span>
						<span class="pool-badge" class:mounted={pool.mounted} class:unmounted={!pool.mounted}>
							{pool.mounted ? 'Mounted' : 'Unmounted'}
						</span>
						{#if pool.total_bytes > 0}
							<span class="pool-usage">{formatBytes(pool.used_bytes)} / {formatBytes(pool.total_bytes)}</span>
						{/if}
					</div>
				{/each}
			</div>
		</div>
	{/if}

	<!-- Network -->
	{#if stats && stats.network.length > 0}
		<div class="card">
			<h3>Network</h3>
			{#each stats.network as iface}
				{@const rates = netIoRates.get(iface.name)}
				<div class="net-iface">
					<div class="net-header">
						<span class="net-name">{iface.name}</span>
						<span class="dot" class:running={iface.up}></span>
						{#if iface.speed_mbps}
							<span class="net-speed">{iface.speed_mbps >= 1000 ? `${iface.speed_mbps / 1000}G` : `${iface.speed_mbps}M`}</span>
						{/if}
					</div>
					{#if rates}
						<div class="net-stats">
							<div class="net-stat">
								<span class="net-dir">RX</span>
								<span class="net-rate">{formatBytes(rates.rxRate)}/s</span>
							</div>
							<div class="net-stat">
								<span class="net-dir">TX</span>
								<span class="net-rate">{formatBytes(rates.txRate)}/s</span>
							</div>
							<div class="net-stat net-total">
								<span class="net-total-label">Total</span>
								<span>{formatBytes(iface.rx_bytes + iface.tx_bytes)}</span>
							</div>
						</div>
					{:else}
						<div class="net-stats">
							<div class="net-stat">
								<span class="net-dir">RX</span>
								<span>{formatBytes(iface.rx_bytes)}</span>
							</div>
							<div class="net-stat">
								<span class="net-dir">TX</span>
								<span>{formatBytes(iface.tx_bytes)}</span>
							</div>
						</div>
					{/if}
				</div>
			{/each}
		</div>
	{/if}

	<!-- Disk I/O -->
	{#if stats && stats.disk_io.length > 0}
		<div class="card">
			<h3>Disk I/O</h3>
			{#each stats.disk_io as dio}
				{@const rates = diskIoRates.get(dio.name)}
				<div class="dio-row">
					<div class="dio-header">
						<span class="dio-name">{dio.name}</span>
						{#if dio.io_in_progress > 0}
							<span class="dio-busy">{dio.io_in_progress} active</span>
						{/if}
					</div>
					<div class="dio-stats">
						<div class="dio-stat">
							<span class="dio-dir">R</span>
							{#if rates}
								<span class="dio-rate">{formatBytes(rates.readRate)}/s</span>
							{:else}
								<span class="dio-total">{formatBytes(dio.read_bytes)}</span>
							{/if}
						</div>
						<div class="dio-stat">
							<span class="dio-dir">W</span>
							{#if rates}
								<span class="dio-rate">{formatBytes(rates.writeRate)}/s</span>
							{:else}
								<span class="dio-total">{formatBytes(dio.write_bytes)}</span>
							{/if}
						</div>
						<div class="dio-stat total">
							<span class="dio-label">Total</span>
							<span>{formatBytes(dio.read_bytes + dio.write_bytes)}</span>
						</div>
					</div>
				</div>
			{/each}
		</div>
	{/if}

	<!-- Disk Health -->
	{#if disks.length > 0}
		<div class="card">
			<h3>Disk Health</h3>
			{#each disks as disk}
				<div class="disk-row">
					<div class="disk-header">
						<span class="dot" class:running={disk.health_passed} class:failed={!disk.health_passed}></span>
						<span class="disk-name">{disk.device}</span>
						<span class="disk-smart" class:passed={disk.health_passed} class:failed={!disk.health_passed}>
							{disk.smart_status}
						</span>
					</div>
					<div class="disk-info">
						<span>{disk.model}</span>
						<span class="muted">{formatBytes(disk.capacity_bytes)}</span>
						{#if disk.temperature_c != null}
							<span class="disk-temp" class:hot={disk.temperature_c > 50}>{disk.temperature_c}°C</span>
						{/if}
					</div>
				</div>
			{/each}
			<div class="disk-link"><a href="/disks">View details</a></div>
		</div>
	{/if}
</div>

<style>
	/* Alerts */
	.alerts-banner { display: flex; flex-direction: column; gap: 0.5rem; margin: 1rem 0; }
	.alert { display: flex; align-items: center; gap: 0.75rem; padding: 0.6rem 1rem; border-radius: 6px; font-size: 0.875rem; }
	.alert-warning { background: #422006; border: 1px solid #854d0e; color: #fde68a; }
	.alert-critical { background: #450a0a; border: 1px solid #991b1b; color: #fca5a5; }
	.alert-icon { font-weight: 700; font-size: 1rem; flex-shrink: 0; }
	.alert-msg { flex: 1; }
	.alert-link { font-size: 0.75rem; color: inherit; opacity: 0.7; }
	.alert-link:hover { opacity: 1; }

	.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); gap: 1.5rem; margin-top: 1.5rem; }
	.card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.25rem; }
	.card h3 { margin: 0 0 1rem; color: #9ca3af; font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; }
	dl { display: grid; grid-template-columns: 100px 1fr; gap: 0.4rem; margin: 0; }
	dt { color: #9ca3af; }
	.health-status { font-size: 1.5rem; font-weight: 700; margin: 0 0 1rem; }
	.health-status.ok { color: #4ade80; }
	.svc { display: flex; align-items: center; gap: 0.5rem; margin: 0.25rem 0; }
	.dot { width: 8px; height: 8px; border-radius: 50%; background: #f87171; flex-shrink: 0; }
	.dot.running { background: #4ade80; }

	/* Stats */
	.stat-value { font-size: 1.5rem; font-weight: 700; line-height: 1.2; }
	.stat-label { color: #6b7280; font-size: 0.8rem; margin-bottom: 0.75rem; }
	.stat-sub { color: #6b7280; font-size: 0.8rem; margin-top: 0.35rem; }
	.bar-container { height: 8px; background: #1e2130; border-radius: 4px; overflow: hidden; }
	.bar-fill { height: 100%; border-radius: 4px; transition: width 0.5s ease; }
	.stat-detail { display: flex; justify-content: space-between; margin-top: 0.5rem; font-size: 0.9rem; font-weight: 600; }
	.stat-detail.labels { font-size: 0.7rem; color: #6b7280; font-weight: 400; margin-top: 0; }

	/* Swap */
	.swap-info { margin-top: 0.75rem; font-size: 0.8rem; color: #6b7280; }

	/* Pools */
	.pool-list { margin-top: 0.75rem; }
	.pool-row { display: flex; align-items: center; gap: 0.5rem; padding: 0.35rem 0; font-size: 0.85rem; border-top: 1px solid #1e2130; }
	.pool-row:first-child { border-top: none; }
	.pool-name { font-weight: 600; }
	.pool-badge { padding: 0.1rem 0.35rem; border-radius: 3px; font-size: 0.65rem; font-weight: 600; }
	.pool-badge.mounted { background: #064e3b; color: #4ade80; }
	.pool-badge.unmounted { background: #3b0e0e; color: #f87171; }
	.pool-usage { color: #6b7280; font-size: 0.8rem; margin-left: auto; }

	/* Network */
	.net-iface { padding: 0.5rem 0; border-top: 1px solid #1e2130; }
	.net-iface:first-child { border-top: none; padding-top: 0; }
	.net-header { display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.35rem; }
	.net-name { font-weight: 600; font-size: 0.9rem; }
	.net-speed { color: #6b7280; font-size: 0.75rem; margin-left: auto; }
	.net-stats { display: flex; gap: 1.5rem; }
	.net-stat { font-size: 0.8rem; display: flex; gap: 0.4rem; }
	.net-dir { color: #6b7280; font-weight: 600; font-size: 0.7rem; }
	.net-rate { font-weight: 600; font-variant-numeric: tabular-nums; }
	.net-total { margin-left: auto; }
	.net-total-label { color: #6b7280; font-size: 0.7rem; }

	/* Disk I/O */
	.dio-row { padding: 0.5rem 0; border-top: 1px solid #1e2130; }
	.dio-row:first-child { border-top: none; padding-top: 0; }
	.dio-header { display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.3rem; }
	.dio-name { font-weight: 600; font-size: 0.9rem; }
	.dio-busy { font-size: 0.7rem; color: #f59e0b; background: rgba(245, 158, 11, 0.15); padding: 0.1rem 0.35rem; border-radius: 3px; }
	.dio-stats { display: flex; gap: 1.25rem; }
	.dio-stat { font-size: 0.8rem; display: flex; gap: 0.35rem; align-items: baseline; }
	.dio-stat.total { margin-left: auto; }
	.dio-dir { color: #6b7280; font-weight: 700; font-size: 0.7rem; }
	.dio-rate { font-weight: 600; font-variant-numeric: tabular-nums; }
	.dio-total { color: #6b7280; }
	.dio-label { color: #6b7280; font-size: 0.7rem; }

	/* Disk Health */
	.disk-row { padding: 0.5rem 0; border-top: 1px solid #1e2130; }
	.disk-row:first-child { border-top: none; padding-top: 0; }
	.disk-header { display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.2rem; }
	.disk-name { font-weight: 600; font-size: 0.85rem; font-family: monospace; }
	.disk-smart { font-size: 0.7rem; font-weight: 600; padding: 0.1rem 0.35rem; border-radius: 3px; margin-left: auto; }
	.disk-smart.passed { background: #064e3b; color: #4ade80; }
	.disk-smart.failed { background: #3b0e0e; color: #f87171; }
	.dot.failed { background: #f87171; }
	.disk-info { display: flex; gap: 0.75rem; font-size: 0.8rem; padding-left: 1rem; }
	.disk-temp { font-weight: 600; }
	.disk-temp.hot { color: #f59e0b; }
	.muted { color: #6b7280; }
	.disk-link { margin-top: 0.75rem; font-size: 0.8rem; }
</style>
