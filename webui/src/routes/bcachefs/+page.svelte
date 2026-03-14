<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { error as showError } from '$lib/toast.svelte';
	import type { Pool } from '$lib/types';
	import { RefreshCw } from '@lucide/svelte';

	type Tab = 'usage' | 'top' | 'timestats';

	const TAB_META: Record<Tab, { label: string; method: string; description: string; interval: number }> = {
		usage: {
			label: 'fs usage',
			method: 'bcachefs.usage',
			description: 'Space breakdown by data type (superblock, journal, btree, data, cached, parity) and per-device fragmentation.',
			interval: 5000,
		},
		top: {
			label: 'fs top',
			method: 'bcachefs.top',
			description: 'Btree operations by process — reads, writes, transaction restarts. Use to diagnose metadata performance.',
			interval: 3000,
		},
		timestats: {
			label: 'fs timestats',
			method: 'bcachefs.timestats',
			description: 'Operation latency stats: min/max/mean/stddev/EWMA for data reads, writes, btree ops, journal flushes, copygc, and more.',
			interval: 3000,
		},
	};

	let pools: Pool[] = $state([]);
	let selectedPool = $state('');
	let activeTab: Tab = $state('usage');
	let output = $state('');
	let loading = $state(false);
	let autoRefresh = $state(false);
	let intervalId: ReturnType<typeof setInterval> | null = null;

	onMount(async () => {
		const client = getClient();
		try {
			pools = await client.call('pool.list');
			const mounted = pools.filter(p => p.mounted);
			if (mounted.length > 0) selectedPool = mounted[0].name;
		} catch (e) {
			showError(e instanceof Error ? e.message : 'Failed to load pools');
		}
	});

	onDestroy(() => stopAutoRefresh());

	async function refresh() {
		if (!selectedPool) return;
		loading = true;
		output = '';
		try {
			const result = await getClient().call(TAB_META[activeTab].method, { name: selectedPool });
			output = typeof result === 'string' ? result : JSON.stringify(result, null, 2);
		} catch (e) {
			output = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	function startAutoRefresh() {
		stopAutoRefresh();
		refresh();
		intervalId = setInterval(refresh, TAB_META[activeTab].interval);
	}

	function stopAutoRefresh() {
		if (intervalId !== null) {
			clearInterval(intervalId);
			intervalId = null;
		}
	}

	function toggleAutoRefresh() {
		autoRefresh = !autoRefresh;
		if (autoRefresh) startAutoRefresh();
		else stopAutoRefresh();
	}

	function switchTab(tab: Tab) {
		activeTab = tab;
		output = '';
		if (autoRefresh) startAutoRefresh();
	}

	$effect(() => {
		// Clear output when pool changes
		selectedPool;
		output = '';
		if (autoRefresh) startAutoRefresh();
	});
</script>

<div class="space-y-4">
	<div class="flex items-center justify-between">
		<div>
			<h1 class="text-2xl font-bold">bcachefs Diagnostics</h1>
			<p class="text-sm text-muted-foreground mt-0.5">Real-time filesystem health and performance</p>
		</div>
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
		{#each (Object.entries(TAB_META) as [tab, meta])}
			{@const t = tab as Tab}
			<button
				onclick={() => switchTab(t)}
				class="px-4 py-2 text-sm font-mono transition-colors border-b-2 -mb-px
					{activeTab === t
						? 'border-primary text-foreground'
						: 'border-transparent text-muted-foreground hover:text-foreground'}"
			>
				{meta.label}
			</button>
		{/each}
		<div class="ml-auto flex items-center gap-2 pb-1">
			<button
				onclick={refresh}
				disabled={!selectedPool || loading}
				class="flex items-center gap-1.5 rounded px-3 py-1 text-xs bg-secondary hover:bg-secondary/80 disabled:opacity-50"
			>
				<RefreshCw size={12} class={loading ? 'animate-spin' : ''} />
				Refresh
			</button>
			<button
				onclick={toggleAutoRefresh}
				disabled={!selectedPool}
				class="rounded px-3 py-1 text-xs disabled:opacity-50
					{autoRefresh ? 'bg-primary text-primary-foreground' : 'bg-secondary hover:bg-secondary/80'}"
			>
				{autoRefresh ? `Live (${TAB_META[activeTab].interval / 1000}s)` : 'Live'}
			</button>
		</div>
	</div>

	<!-- Description -->
	<p class="text-xs text-muted-foreground">{TAB_META[activeTab].description}</p>

	<!-- Output -->
	<div class="rounded-lg border border-border bg-card overflow-hidden">
		{#if !selectedPool}
			<p class="p-6 text-sm text-muted-foreground">Select a mounted pool to view diagnostics.</p>
		{:else if output === ''}
			<p class="p-6 text-sm text-muted-foreground">
				{loading ? 'Running command…' : 'Press Refresh or enable Live to fetch data.'}
			</p>
		{:else}
			<pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre leading-relaxed">{output}</pre>
		{/if}
	</div>
</div>
