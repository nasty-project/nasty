<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let units: string[] = $state([]);
	let selectedUnit = $state('nasty-engine');
	let content = $state('');
	let loading = $state(false);
	let lines = $state(100);

	onMount(async () => {
		try { units = await client.call<string[]>('system.logs.units'); } catch { /* ignore */ }
	});

	async function load() {
		loading = true;
		try {
			content = await client.call<string>('system.logs', { unit: selectedUnit, lines });
		} catch (e) {
			content = `Error: ${e}`;
		}
		loading = false;
	}
</script>

<div>
	<h1 class="text-2xl font-bold">Logs</h1>
	<p class="text-sm text-muted-foreground mt-0.5">View systemd journal logs for NASty services.</p>
</div>

<div class="mt-4 flex items-center gap-2">
	<select
		bind:value={selectedUnit}
		onchange={() => load()}
		class="h-8 rounded-md border border-input bg-transparent px-2 text-sm"
	>
		{#if units.length === 0}
			<option value="nasty-engine">nasty-engine</option>
		{/if}
		{#each units as unit}
			<option value={unit}>{unit}</option>
		{/each}
	</select>
	<select
		bind:value={lines}
		onchange={() => { if (content) load(); }}
		class="h-8 rounded-md border border-input bg-transparent px-2 text-sm"
	>
		<option value={50}>50 lines</option>
		<option value={100}>100 lines</option>
		<option value={200}>200 lines</option>
		<option value={500}>500 lines</option>
		<option value={1000}>1000 lines</option>
	</select>
	<Button size="sm" onclick={load} disabled={loading}>
		{loading ? 'Loading...' : 'Load'}
	</Button>
</div>

{#if content}
	<pre class="mt-4 max-h-[calc(100vh-220px)] overflow-auto rounded-lg bg-[#0f1117] p-3 text-xs text-green-400 font-mono whitespace-pre-wrap">{content}</pre>
{:else}
	<p class="mt-4 text-sm text-muted-foreground">Select a service and click Load to view logs.</p>
{/if}
