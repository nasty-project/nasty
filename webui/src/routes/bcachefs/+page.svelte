<script lang="ts">
	import { onMount, onDestroy, tick } from 'svelte';
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';
	import { getClient } from '$lib/client';
	import { getToken } from '$lib/auth';
	import { error as showError } from '$lib/toast.svelte';
	import type { Pool } from '$lib/types';
	import { RefreshCw, SquareX } from '@lucide/svelte';

	type Tab = 'usage' | 'top' | 'timestats';

	const TAB_META: Record<Tab, { label: string; description: string }> = {
		usage: {
			label: 'fs usage',
			description: 'Space breakdown by data type (superblock, journal, btree, data, cached, parity) and per-device fragmentation.',
		},
		top: {
			label: 'fs top',
			description: 'Btree operations by process — reads, writes, transaction restarts. Updates live in a real PTY.',
		},
		timestats: {
			label: 'fs timestats',
			description: 'Operation latency: min/max/mean/stddev/EWMA for data reads, writes, btree ops, journal flushes, copygc, and more. Updates live in a real PTY.',
		},
	};

	let pools: Pool[] = $state([]);
	let selectedPool = $state('');
	let activeTab: Tab = $state('usage');

	// usage tab state
	let usageOutput = $state('');
	let usageLoading = $state(false);
	let autoRefresh = $state(false);
	let intervalId: ReturnType<typeof setInterval> | null = null;

	// terminal tab state
	let termEl: HTMLDivElement | undefined = $state();
	let term: Terminal | null = null;
	let fitAddon: FitAddon | null = null;
	let termWs: WebSocket | null = null;
	let termStatus = $state<'idle' | 'running' | 'done'>('idle');

	onMount(async () => {
		try {
			pools = await getClient().call('pool.list');
			const mounted = pools.filter(p => p.mounted);
			if (mounted.length > 0) selectedPool = mounted[0].name;
		} catch (e) {
			showError(e instanceof Error ? e.message : 'Failed to load pools');
		}
	});

	onDestroy(() => {
		stopAutoRefresh();
		killTerm();
	});

	// ── Usage tab ──────────────────────────────────────────────

	async function refreshUsage() {
		if (!selectedPool) return;
		usageLoading = true;
		try {
			const result = await getClient().call('bcachefs.usage', { name: selectedPool });
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

	// ── Terminal tab (fs top / fs timestats) ──────────────────

	function mountPool() {
		return pools.find(p => p.name === selectedPool)?.mount_point ?? `/storage/${selectedPool}`;
	}

	async function startTerm() {
		if (!selectedPool) return;
		killTerm();
		termStatus = 'running';
		await tick(); // wait for termEl div to appear in the DOM
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
		const mp = mountPool();
		const argv = ['bcachefs', 'fs', activeTab === 'top' ? 'top' : 'timestats', mp];

		const wsUrl = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws/terminal`;
		termWs = new WebSocket(wsUrl);
		termStatus = 'running';

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

		// Forward keystrokes to PTY
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

	// Reset terminal state when pool or tab changes
	$effect(() => {
		selectedPool; activeTab;
		usageOutput = '';
		if (autoRefresh && activeTab === 'usage') startAutoRefresh();
		else stopAutoRefresh();
		killTerm();
	});
</script>

<div class="space-y-4">
	<div>
		<h1 class="text-2xl font-bold">bcachefs Diagnostics</h1>
		<p class="text-sm text-muted-foreground mt-0.5">Real-time filesystem health and performance</p>
	</div>

	<!-- Pool selector -->
	<div class="flex items-center gap-3">
		<label for="pool-select" class="text-sm font-medium shrink-0">Pool</label>
		{#if pools.length === 0}
			<span class="text-sm text-muted-foreground">No pools available</span>
		{:else}
			<select
				id="pool-select"
				bind:value={selectedPool}
				class="rounded-md border border-input bg-background px-3 py-1.5 text-sm"
			>
				{#each pools as pool}
					<option value={pool.name} disabled={!pool.mounted}>
						{pool.name}{pool.mounted ? '' : ' (unmounted)'}
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
					disabled={!selectedPool || usageLoading}
					class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
				>
					<RefreshCw size={12} class={usageLoading ? 'animate-spin' : ''} />
					Refresh
				</button>
				<button
					onclick={toggleAutoRefresh}
					disabled={!selectedPool}
					class="rounded px-3 py-1 text-xs disabled:opacity-50
						{autoRefresh ? 'bg-primary text-primary-foreground' : 'bg-secondary hover:bg-secondary/80'}"
				>
					{autoRefresh ? 'Live (5s)' : 'Live'}
				</button>
			</div>

		<!-- top/timestats tab controls -->
		{:else}
			<div class="ml-auto flex items-center gap-2 pb-1">
				{#if termStatus === 'idle' || termStatus === 'done'}
					<button
						onclick={startTerm}
						disabled={!selectedPool}
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

	<!-- usage output -->
	{#if activeTab === 'usage'}
		<div class="rounded-lg border border-border bg-card overflow-hidden">
			{#if !selectedPool}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted pool to view diagnostics.</p>
			{:else if usageOutput === ''}
				<p class="p-6 text-sm text-muted-foreground">
					{usageLoading ? 'Running…' : 'Press Refresh or enable Live to fetch data.'}
				</p>
			{:else}
				<pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre leading-relaxed">{usageOutput}</pre>
			{/if}
		</div>

	<!-- terminal output (top / timestats) -->
	{:else}
		<div class="rounded-lg border border-border bg-[#0f1117] overflow-hidden" style="min-height: 400px;">
			{#if !selectedPool}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted pool.</p>
			{:else if termStatus === 'idle'}
				<p class="p-6 text-sm text-muted-foreground">Press Start to launch <code class="font-mono">bcachefs fs {activeTab === 'top' ? 'top' : 'timestats'}</code> in a live terminal.</p>
			{:else}
				<div bind:this={termEl} class="w-full" style="min-height: 400px;"></div>
				{#if termStatus === 'done'}
					<p class="px-4 py-2 text-xs text-muted-foreground border-t border-border">Process exited. Press Restart to run again.</p>
				{/if}
			{/if}
		</div>
	{/if}
</div>
