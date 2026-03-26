<script lang="ts">
	import { onMount, onDestroy, tick } from 'svelte';
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';
	import { getClient } from '$lib/client';
	import { getToken } from '$lib/auth';
	import { error as showError } from '$lib/toast.svelte';
	import type { Filesystem } from '$lib/types';
	import { RefreshCw, SquareX } from '@lucide/svelte';

	type Tab = 'usage' | 'top' | 'timestats';

	const TAB_META: Record<Tab, { label: string; description: string }> = {
		usage: {
			label: 'fs usage',
			description: 'Space breakdown by data type (superblock, journal, btree, data, cached, parity) and per-device usage.',
		},
		top: {
			label: 'fs top',
			description: 'Btree operations by process — reads, writes, transaction restarts. Updates live in a real PTY.',
		},
		timestats: {
			label: 'fs timestats',
			description: 'Operation latency: min/max/mean/stddev for data reads, writes, btree ops, journal flushes, and more.',
		},
	};

	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');
	let activeTab: Tab = $state('usage');

	// usage tab state
	let usageOutput = $state('');
	let usageLoading = $state(false);
	let autoRefresh = $state(false);
	let intervalId: ReturnType<typeof setInterval> | null = null;

	// timestats tab state
	let timestatsData: any = $state(null);
	let timestatsLoading = $state(false);
	let timestatsAutoRefresh = $state(false);
	let timestatsIntervalId: ReturnType<typeof setInterval> | null = null;

	// terminal tab state (fs top)
	let termEl: HTMLDivElement | undefined = $state();
	let term: Terminal | null = null;
	let fitAddon: FitAddon | null = null;
	let termWs: WebSocket | null = null;
	let termStatus = $state<'idle' | 'running' | 'done'>('idle');

	onMount(async () => {
		try {
			filesystems = await getClient().call('fs.list');
			const mounted = filesystems.filter(p => p.mounted);
			if (mounted.length > 0) {
				selectedFs = mounted[0].name;
				// Auto-load first tab
				await refreshUsage();
			}
		} catch (e) {
			showError(e instanceof Error ? e.message : 'Failed to load filesystems');
		}
	});

	onDestroy(() => {
		stopAutoRefresh();
		stopTimestatsAutoRefresh();
		killTerm();
	});

	// ── Usage tab ──────────────────────────────────────────────

	async function refreshUsage() {
		if (!selectedFs) return;
		usageLoading = true;
		try {
			const result = await getClient().call('bcachefs.usage', { name: selectedFs });
			usageOutput = typeof result === 'string' ? result : JSON.stringify(result, null, 2);
		} catch (e) {
			usageOutput = e instanceof Error ? e.message : String(e);
		} finally {
			usageLoading = false;
		}
	}

	function startAutoRefresh() {
		stopAutoRefresh();
		refreshUsage();
		intervalId = setInterval(refreshUsage, 5000);
	}

	function stopAutoRefresh() {
		if (intervalId !== null) { clearInterval(intervalId); intervalId = null; }
	}

	function toggleAutoRefresh() {
		autoRefresh = !autoRefresh;
		if (autoRefresh) startAutoRefresh(); else stopAutoRefresh();
	}

	// ── Timestats tab ──────────────────────────────────────────

	async function refreshTimestats() {
		if (!selectedFs) return;
		timestatsLoading = true;
		try {
			timestatsData = await getClient().call('bcachefs.timestats', { name: selectedFs });
		} catch (e) {
			timestatsData = null;
			showError(e instanceof Error ? e.message : String(e));
		} finally {
			timestatsLoading = false;
		}
	}

	function startTimestatsAutoRefresh() {
		stopTimestatsAutoRefresh();
		refreshTimestats();
		timestatsIntervalId = setInterval(refreshTimestats, 3000);
	}

	function stopTimestatsAutoRefresh() {
		if (timestatsIntervalId !== null) { clearInterval(timestatsIntervalId); timestatsIntervalId = null; }
	}

	function toggleTimestatsAutoRefresh() {
		timestatsAutoRefresh = !timestatsAutoRefresh;
		if (timestatsAutoRefresh) startTimestatsAutoRefresh(); else stopTimestatsAutoRefresh();
	}

	// ── Terminal tab (fs top) ────────────────────────────────

	function mountPath() {
		return filesystems.find(p => p.name === selectedFs)?.mount_point ?? `/fs/${selectedFs}`;
	}

	async function startTerm() {
		if (!selectedFs) return;
		killTerm();
		termStatus = 'running';
		await tick();
		if (!termEl) return;

		term = new Terminal({
			cursorBlink: false,
			fontSize: 13,
			fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
			theme: {
				background: '#0f1117', foreground: '#e0e0e0', cursor: '#e0e0e0',
				black: '#0f1117', red: '#dc2626', green: '#4ade80', yellow: '#f59e0b',
				blue: '#2563eb', magenta: '#a855f7', cyan: '#22d3ee', white: '#e0e0e0',
				brightBlack: '#4b5563', brightRed: '#f87171', brightGreen: '#86efac',
				brightYellow: '#fcd34d', brightBlue: '#60a5fa', brightMagenta: '#c084fc',
				brightCyan: '#67e8f9', brightWhite: '#ffffff',
			},
		});
		fitAddon = new FitAddon();
		term.loadAddon(fitAddon);
		term.open(termEl);
		fitAddon.fit();
		term.focus();

		const { cols, rows } = term;
		const mp = mountPath();
		const argv = ['bcachefs', 'fs', 'top', '-h', mp];

		const wsUrl = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws/terminal`;
		termWs = new WebSocket(wsUrl);

		termWs.onopen = () => {
			termWs!.send(JSON.stringify({ token: getToken(), cols, rows, cmd: argv }));
		};

		termWs.onmessage = (e) => {
			try {
				const msg = JSON.parse(e.data);
				if (msg.authenticated) return;
				if (msg.error) { term?.write(`\r\nError: ${msg.error}\r\n`); return; }
			} catch { /* raw PTY output */ }
			term?.write(e.data);
		};

		termWs.onclose = () => { termStatus = 'done'; };
		termWs.onerror = () => { termStatus = 'done'; };

		term.onData((data) => {
			if (termWs?.readyState === WebSocket.OPEN) {
				termWs.send(data);
			}
		});

		const resizeObserver = new ResizeObserver(() => {
			fitAddon?.fit();
			const s = fitAddon ? { cols: term!.cols, rows: term!.rows } : null;
			if (s && termWs?.readyState === WebSocket.OPEN) {
				termWs.send(JSON.stringify({ type: 'resize', ...s }));
			}
		});
		if (termEl) resizeObserver.observe(termEl);
	}

	function killTerm() {
		termWs?.close();
		termWs = null;
		term?.dispose();
		term = null;
		termStatus = 'idle';
	}

	// Reset state on filesystem or tab change — auto-load data
	$effect(() => {
		const _fs = selectedFs;
		const _tab = activeTab;
		usageOutput = '';
		timestatsData = null;
		stopAutoRefresh();
		stopTimestatsAutoRefresh();
		killTerm();

		if (_fs) {
			if (_tab === 'usage') refreshUsage();
			else if (_tab === 'timestats') refreshTimestats();
			else if (_tab === 'top') startTerm();
		}
	});

	// Helper to render timestats sections as tables
	function timestatsEntries(section: any): { name: string; count: number; dur_min: string; dur_max: string; dur_total: string; mean: string; mean_recent: string; stddev: string; stddev_recent: string }[] {
		if (!section || typeof section !== 'object') return [];
		return Object.entries(section).map(([name, v]: [string, any]) => ({
			name,
			count: v?.count ?? 0,
			dur_min: v?.duration_min ?? '—',
			dur_max: v?.duration_max ?? '—',
			dur_total: v?.duration_total ?? '—',
			mean: v?.mean ?? v?.mean_since ?? '—',
			mean_recent: v?.mean_recent ?? '—',
			stddev: v?.stddev ?? v?.stddev_since ?? '—',
			stddev_recent: v?.stddev_recent ?? '—',
		})).filter(e => e.count > 0);
	}
</script>

<div class="space-y-4">
	<div>
		<h1 class="text-2xl font-bold">bcachefs Diagnostics</h1>
		<p class="text-sm text-muted-foreground mt-0.5">Real-time filesystem health and performance</p>
	</div>

	<!-- Filesystem selector -->
	<div class="flex items-center gap-3">
		<label for="fs-select" class="text-sm font-medium shrink-0">Filesystem</label>
		{#if filesystems.length === 0}
			<span class="text-sm text-muted-foreground">No filesystems available</span>
		{:else}
			<select
				id="fs-select"
				bind:value={selectedFs}
				class="rounded-md border border-input bg-background px-3 py-1.5 text-sm"
			>
				{#each filesystems as fs}
					<option value={fs.name} disabled={!fs.mounted}>
						{fs.name}{fs.mounted ? '' : ' (unmounted)'}
					</option>
				{/each}
			</select>
		{/if}
	</div>

	<!-- Tab bar -->
	<div class="flex items-center gap-1 border-b border-border">
		{#each Object.entries(TAB_META) as [tab, meta]}
			{@const t = tab as Tab}
			<button
				onclick={() => { activeTab = t; }}
				class="px-4 py-2 text-sm font-mono transition-colors border-b-2 -mb-px
					{activeTab === t
						? 'border-primary text-foreground'
						: 'border-transparent text-muted-foreground hover:text-foreground'}"
			>
				{meta.label}
			</button>
		{/each}

		<!-- usage tab controls -->
		{#if activeTab === 'usage'}
			<div class="ml-auto flex items-center gap-2 pb-1">
				<button
					onclick={refreshUsage}
					disabled={!selectedFs || usageLoading}
					class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
				>
					<RefreshCw size={12} class={usageLoading ? 'animate-spin' : ''} />
					Refresh
				</button>
				<button
					onclick={toggleAutoRefresh}
					disabled={!selectedFs}
					class="rounded px-3 py-1 text-xs disabled:opacity-50
						{autoRefresh ? 'bg-primary text-primary-foreground' : 'bg-secondary hover:bg-secondary/80'}"
				>
					{autoRefresh ? 'Live (5s)' : 'Live'}
				</button>
			</div>

		<!-- timestats tab controls -->
		{:else if activeTab === 'timestats'}
			<div class="ml-auto flex items-center gap-2 pb-1">
				<button
					onclick={refreshTimestats}
					disabled={!selectedFs || timestatsLoading}
					class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
				>
					<RefreshCw size={12} class={timestatsLoading ? 'animate-spin' : ''} />
					Refresh
				</button>
				<button
					onclick={toggleTimestatsAutoRefresh}
					disabled={!selectedFs}
					class="rounded px-3 py-1 text-xs disabled:opacity-50
						{timestatsAutoRefresh ? 'bg-primary text-primary-foreground' : 'bg-secondary hover:bg-secondary/80'}"
				>
					{timestatsAutoRefresh ? 'Live (3s)' : 'Live'}
				</button>
			</div>

		<!-- top tab controls -->
		{:else}
			<div class="ml-auto flex items-center gap-2 pb-1">
				{#if termStatus === 'idle' || termStatus === 'done'}
					<button
						onclick={startTerm}
						disabled={!selectedFs}
						class="rounded px-3 py-1 text-xs bg-primary text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
					>
						{termStatus === 'done' ? 'Restart' : 'Start'}
					</button>
				{:else}
					<button
						onclick={killTerm}
						class="flex items-center gap-1 rounded px-3 py-1 text-xs bg-destructive text-destructive-foreground hover:bg-destructive/90"
					>
						<SquareX size={12} />
						Stop
					</button>
				{/if}
			</div>
		{/if}
	</div>

	<p class="text-xs text-muted-foreground">{TAB_META[activeTab].description}</p>

	<!-- ═══ Usage output ═══ -->
	{#if activeTab === 'usage'}
		<div class="rounded-lg border border-border bg-card overflow-hidden">
			{#if !selectedFs}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted filesystem to view diagnostics.</p>
			{:else if usageOutput === '' && usageLoading}
				<p class="p-6 text-sm text-muted-foreground">Loading...</p>
			{:else if usageOutput === ''}
				<p class="p-6 text-sm text-muted-foreground">No data available.</p>
			{:else}
				<pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre leading-relaxed">{usageOutput}</pre>
			{/if}
		</div>

	<!-- ═══ Timestats output ═══ -->
	{:else if activeTab === 'timestats'}
		<div class="space-y-4">
			{#if !selectedFs}
				<div class="rounded-lg border border-border bg-card p-6">
					<p class="text-sm text-muted-foreground">Select a mounted filesystem.</p>
				</div>
			{:else if !timestatsData && timestatsLoading}
				<div class="rounded-lg border border-border bg-card p-6">
					<p class="text-sm text-muted-foreground">Loading...</p>
				</div>
			{:else if !timestatsData}
				<div class="rounded-lg border border-border bg-card p-6">
					<p class="text-sm text-muted-foreground">No data available.</p>
				</div>
			{:else}
				{#each Object.entries(timestatsData) as [sectionName, sectionData]}
					{@const entries = timestatsEntries(sectionData)}
					{#if entries.length > 0}
						<div class="rounded-lg border border-border bg-card overflow-hidden">
							<div class="px-4 py-2 border-b border-border bg-secondary/30">
								<h3 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{sectionName.replace(/_/g, ' ')}</h3>
							</div>
							<div class="overflow-x-auto">
								<table class="w-full text-xs font-mono">
									<thead>
										<tr class="border-b border-border text-muted-foreground">
											<th class="px-3 py-2 text-left">Name</th>
											<th class="px-3 py-2 text-right">Count</th>
											<th class="px-3 py-2 text-right">Min</th>
											<th class="px-3 py-2 text-right">Max</th>
											<th class="px-3 py-2 text-right">Total</th>
											<th class="px-3 py-2 text-right">Mean</th>
											<th class="px-3 py-2 text-right">Recent</th>
											<th class="px-3 py-2 text-right">Stddev</th>
										</tr>
									</thead>
									<tbody>
										{#each entries as row}
											<tr class="border-b border-border/50 hover:bg-muted/20">
												<td class="px-3 py-1.5 text-foreground">{row.name}</td>
												<td class="px-3 py-1.5 text-right">{row.count}</td>
												<td class="px-3 py-1.5 text-right">{row.dur_min}</td>
												<td class="px-3 py-1.5 text-right">{row.dur_max}</td>
												<td class="px-3 py-1.5 text-right">{row.dur_total}</td>
												<td class="px-3 py-1.5 text-right">{row.mean}</td>
												<td class="px-3 py-1.5 text-right">{row.mean_recent}</td>
												<td class="px-3 py-1.5 text-right">{row.stddev}</td>
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
						</div>
					{/if}
				{/each}
			{/if}
		</div>

	<!-- ═══ Terminal output (fs top) ═══ -->
	{:else}
		<div class="rounded-lg border border-border bg-[#0f1117] overflow-hidden" style="min-height: 400px;">
			{#if !selectedFs}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted filesystem.</p>
			{:else if termStatus === 'idle'}
				<p class="p-6 text-sm text-muted-foreground">Starting...</p>
			{:else}
				<div bind:this={termEl} class="w-full" style="min-height: 400px;"></div>
				{#if termStatus === 'done'}
					<p class="px-4 py-2 text-xs text-muted-foreground border-t border-border">Process exited. Press Restart to run again.</p>
				{/if}
			{/if}
		</div>
	{/if}
</div>
