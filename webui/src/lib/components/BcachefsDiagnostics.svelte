<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { error as showError } from '$lib/toast.svelte';
	import type { Filesystem } from '$lib/types';
	import { RefreshCw, SquareArrowOutUpRight } from '@lucide/svelte';

	type Tab = 'usage' | 'top' | 'timestats' | 'scrub' | 'reconcile';

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
		scrub: {
			label: 'scrub',
			description: 'Verify checksums and correct errors. Run periodically to detect silent data corruption.',
		},
		reconcile: {
			label: 'reconcile',
			description: 'Background data rebalancing status — moves data between tiers and restripes after device changes.',
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

	// top tab state
	let topOutput = $state('');
	let topLoading = $state(false);
	let topAutoRefresh = $state(false);
	let topIntervalId: ReturnType<typeof setInterval> | null = null;

	// scrub/reconcile tab state
	let scrubOutput = $state('');
	let scrubRunning = $state(false);
	let scrubLoading = $state(false);
	let reconcileOutput = $state('');
	let reconcileLoading = $state(false);
	let reconcileEnabled = $state(true);
	let reconcileToggling = $state(false);

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

	// scrub polling
	let scrubPollId: ReturnType<typeof setInterval> | null = null;

	function startScrubPolling() {
		if (scrubPollId) return;
		scrubPollId = setInterval(async () => {
			await refreshScrub();
			if (!scrubRunning && scrubPollId) {
				clearInterval(scrubPollId);
				scrubPollId = null;
			}
		}, 5000);
	}

	function stopScrubPolling() {
		if (scrubPollId) { clearInterval(scrubPollId); scrubPollId = null; }
	}

	onDestroy(() => {
		stopAutoRefresh();
		stopTopAutoRefresh();
		stopTimestatsAutoRefresh();
		stopScrubPolling();
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

	// ── Scrub tab ──────────────────────────────────────────────

	async function refreshScrub() {
		if (!selectedFs) return;
		scrubLoading = true;
		try {
			const result = await getClient().call<{ raw: string; running: boolean }>('fs.scrub.status', { name: selectedFs });
			scrubOutput = result.raw || 'No scrub data available.';
			scrubRunning = result.running ?? false;
			if (scrubRunning) startScrubPolling();
			else stopScrubPolling();
		} catch (e) {
			scrubOutput = e instanceof Error ? e.message : String(e);
		} finally {
			scrubLoading = false;
		}
	}

	async function startScrub() {
		if (!selectedFs) return;
		try {
			await getClient().call('fs.scrub.start', { name: selectedFs });
			scrubRunning = true;
			await refreshScrub();
			startScrubPolling();
		} catch (e) {
			showError(e instanceof Error ? e.message : String(e));
		}
	}

	// ── Reconcile tab ──────────────────────────────────────────

	async function refreshReconcile() {
		if (!selectedFs) return;
		reconcileLoading = true;
		try {
			const result = await getClient().call<{ raw: string; enabled: boolean }>('fs.reconcile.status', { name: selectedFs });
			reconcileOutput = result.raw || 'No reconcile data available.';
			reconcileEnabled = result.enabled;
		} catch (e) {
			reconcileOutput = e instanceof Error ? e.message : String(e);
		} finally {
			reconcileLoading = false;
		}
	}

	async function toggleReconcile() {
		if (!selectedFs) return;
		reconcileToggling = true;
		try {
			const method = reconcileEnabled ? 'fs.reconcile.disable' : 'fs.reconcile.enable';
			await getClient().call(method, { name: selectedFs });
			reconcileEnabled = !reconcileEnabled;
			await refreshReconcile();
		} catch (e) {
			reconcileOutput = e instanceof Error ? e.message : String(e);
		} finally {
			reconcileToggling = false;
		}
	}

	// ── Top tab ──────────────────────────────────────────────

	async function refreshTop() {
		if (!selectedFs) return;
		topLoading = true;
		try {
			const result = await getClient().call('bcachefs.top', { name: selectedFs });
			topOutput = typeof result === 'string' ? result : JSON.stringify(result, null, 2);
		} catch (e) {
			topOutput = e instanceof Error ? e.message : String(e);
		} finally {
			topLoading = false;
		}
	}

	function startTopAutoRefresh() {
		stopTopAutoRefresh();
		refreshTop();
		topIntervalId = setInterval(refreshTop, 3000);
	}

	function stopTopAutoRefresh() {
		if (topIntervalId !== null) { clearInterval(topIntervalId); topIntervalId = null; }
	}

	function toggleTopAutoRefresh() {
		topAutoRefresh = !topAutoRefresh;
		if (topAutoRefresh) startTopAutoRefresh(); else stopTopAutoRefresh();
	}

	// Reset state on filesystem or tab change — auto-load data
	$effect(() => {
		const _fs = selectedFs;
		const _tab = activeTab;
		usageOutput = '';
		topOutput = '';
		timestatsData = null;
		scrubOutput = '';
		reconcileOutput = '';
		stopAutoRefresh();
		stopTopAutoRefresh();
		stopTimestatsAutoRefresh();

		if (_fs) {
			if (_tab === 'usage') refreshUsage();
			else if (_tab === 'top') refreshTop();
			else if (_tab === 'timestats') refreshTimestats();
			else if (_tab === 'scrub') refreshScrub();
			else if (_tab === 'reconcile') refreshReconcile();
		}
	});

	// Format nanoseconds to human-readable duration
	function fmtNs(ns: number): string {
		if (ns === 0) return '0';
		if (ns < 1000) return `${ns}ns`;
		if (ns < 1_000_000) return `${(ns / 1000).toFixed(1)}us`;
		if (ns < 1_000_000_000) return `${(ns / 1_000_000).toFixed(1)}ms`;
		return `${(ns / 1_000_000_000).toFixed(2)}s`;
	}

	// Parse timestats JSON: { device: { operation: { count, duration_ns: {min,max,total,mean,stddev}, duration_ewma_ns: {mean,stddev} } } }
	function timestatsEntries(section: any): { name: string; count: number; dur_min: string; dur_max: string; dur_total: string; mean: string; ewma_mean: string; stddev: string }[] {
		if (!section || typeof section !== 'object') return [];
		return Object.entries(section).map(([name, v]: [string, any]) => {
			const dur = v?.duration_ns ?? {};
			const ewma = v?.duration_ewma_ns ?? {};
			return {
				name,
				count: v?.count ?? 0,
				dur_min: fmtNs(dur.min ?? 0),
				dur_max: fmtNs(dur.max ?? 0),
				dur_total: fmtNs(dur.total ?? 0),
				mean: fmtNs(dur.mean ?? 0),
				ewma_mean: fmtNs(ewma.mean ?? 0),
				stddev: fmtNs(dur.stddev ?? 0),
			};
		}).filter(e => e.count > 0);
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

	<!-- nasty-top suggestion -->
	<div class="flex items-center gap-2 rounded-md border border-border bg-secondary/30 px-4 py-2 text-sm">
		<span class="text-muted-foreground">For live IO monitoring, latency graphs, and tuning use</span>
		<a
			href="/terminal?cmd=nasty-top"
			class="inline-flex items-center gap-1 font-mono font-medium text-primary hover:underline"
		>
			nasty-top
			<SquareArrowOutUpRight size={12} />
		</a>
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

		<!-- scrub tab controls -->
		{:else if activeTab === 'scrub'}
			<div class="ml-auto flex items-center gap-2 pb-1">
				<button
					onclick={refreshScrub}
					disabled={!selectedFs || scrubLoading}
					class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
				>
					<RefreshCw size={12} class={scrubLoading ? 'animate-spin' : ''} />
					Refresh
				</button>
				<button
					onclick={startScrub}
					disabled={!selectedFs || scrubRunning}
					class="rounded px-3 py-1 text-xs bg-primary text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
				>
					{scrubRunning ? 'Running...' : 'Start Scrub'}
				</button>
			</div>

		<!-- reconcile tab controls -->
		{:else if activeTab === 'reconcile'}
			<div class="ml-auto flex items-center gap-2 pb-1">
				<button
					onclick={toggleReconcile}
					disabled={!selectedFs || reconcileToggling}
					class="rounded px-3 py-1 text-xs {reconcileEnabled ? 'bg-primary text-primary-foreground hover:bg-primary/90' : 'bg-destructive text-destructive-foreground hover:bg-destructive/90'} disabled:opacity-50"
				>
					{reconcileToggling ? '...' : reconcileEnabled ? 'Enabled' : 'Disabled'}
				</button>
				<button
					onclick={refreshReconcile}
					disabled={!selectedFs || reconcileLoading}
					class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
				>
					<RefreshCw size={12} class={reconcileLoading ? 'animate-spin' : ''} />
					Refresh
				</button>
			</div>

		<!-- top tab controls -->
		{:else if activeTab === 'top'}
			<div class="ml-auto flex items-center gap-2 pb-1">
				<button
					onclick={refreshTop}
					disabled={!selectedFs || topLoading}
					class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
				>
					<RefreshCw size={12} class={topLoading ? 'animate-spin' : ''} />
					Refresh
				</button>
				<button
					onclick={toggleTopAutoRefresh}
					disabled={!selectedFs}
					class="rounded px-3 py-1 text-xs disabled:opacity-50
						{topAutoRefresh ? 'bg-primary text-primary-foreground' : 'bg-secondary hover:bg-secondary/80'}"
				>
					{topAutoRefresh ? 'Live (3s)' : 'Live'}
				</button>
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
								<h3 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{sectionName}</h3>
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
											<th class="px-3 py-2 text-right">EWMA</th>
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
												<td class="px-3 py-1.5 text-right">{row.ewma_mean}</td>
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

	<!-- ═══ Scrub output ═══ -->
	{:else if activeTab === 'scrub'}
		<div class="rounded-lg border border-border bg-card overflow-hidden">
			{#if !selectedFs}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted filesystem.</p>
			{:else if scrubOutput === '' && scrubLoading}
				<p class="p-6 text-sm text-muted-foreground">Loading...</p>
			{:else if scrubOutput === ''}
				<p class="p-6 text-sm text-muted-foreground">No scrub data available. Start a scrub to verify checksums.</p>
			{:else}
				<div class="p-4">
					{#if scrubRunning}
						<div class="mb-3 flex items-center gap-2">
							<span class="inline-block h-2 w-2 rounded-full bg-yellow-500 animate-pulse"></span>
							<span class="text-sm font-medium text-yellow-500">Scrub in progress</span>
						</div>
					{/if}
					<pre class="text-xs font-mono overflow-x-auto whitespace-pre leading-relaxed">{scrubOutput}</pre>
				</div>
			{/if}
		</div>

	<!-- ═══ Reconcile output ═══ -->
	{:else if activeTab === 'reconcile'}
		<div class="rounded-lg border border-border bg-card overflow-hidden">
			{#if !selectedFs}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted filesystem.</p>
			{:else if reconcileOutput === '' && reconcileLoading}
				<p class="p-6 text-sm text-muted-foreground">Loading...</p>
			{:else if reconcileOutput === ''}
				<p class="p-6 text-sm text-muted-foreground">No reconcile data available.</p>
			{:else}
				<pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre leading-relaxed">{reconcileOutput}</pre>
			{/if}
		</div>

	<!-- ═══ Top output ═══ -->
	{:else if activeTab === 'top'}
		<div class="rounded-lg border border-border bg-card overflow-hidden">
			{#if !selectedFs}
				<p class="p-6 text-sm text-muted-foreground">Select a mounted filesystem.</p>
			{:else if topOutput === '' && topLoading}
				<p class="p-6 text-sm text-muted-foreground">Loading...</p>
			{:else if topOutput === ''}
				<p class="p-6 text-sm text-muted-foreground">No data available.</p>
			{:else}
				<pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre leading-relaxed">{topOutput}</pre>
			{/if}
		</div>
	{/if}
</div>
