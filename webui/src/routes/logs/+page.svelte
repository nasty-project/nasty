<script lang="ts">
	import { onMount, onDestroy, tick } from 'svelte';
	import { getClient } from '$lib/client';
	import { getToken } from '$lib/auth';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';

	const client = getClient();

	let units: string[] = $state([]);
	let selectedUnit = $state('nasty-engine');
	let content = $state('');
	let loading = $state(false);
	let lines = $state(100);

	// Server-side grep
	let grepPattern = $state('');

	// Client-side search
	let searchQuery = $state('');

	// Follow mode
	let following = $state(false);
	let followWs: WebSocket | null = null;
	let followLines: string[] = $state([]);

	let logEl: HTMLPreElement | undefined = $state();

	onMount(async () => {
		try { units = await client.call<string[]>('system.logs.units'); } catch { /* ignore */ }
	});

	onDestroy(() => {
		stopFollow();
	});

	async function load() {
		stopFollow();
		loading = true;
		try {
			const params: Record<string, unknown> = { unit: selectedUnit, lines };
			if (grepPattern.trim()) params.grep = grepPattern.trim();
			content = await client.call<string>('system.logs', params);
		} catch (e) {
			content = `Error: ${e}`;
		}
		loading = false;
	}

	function startFollow() {
		stopFollow();
		following = true;
		followLines = content ? content.split('\n') : [];

		const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
		const ws = new WebSocket(`${proto}//${location.host}/ws/system/logs`);
		followWs = ws;

		ws.onopen = () => {
			ws.send(JSON.stringify({
				token: getToken(),
				unit: selectedUnit,
				lines: content ? 0 : lines,
				grep: grepPattern.trim() || undefined,
			}));
		};

		ws.onmessage = async (e) => {
			try {
				const msg = JSON.parse(e.data);
				if (msg.type === 'line') {
					followLines = [...followLines, msg.data];
					// Keep max 5000 lines in memory
					if (followLines.length > 5000) {
						followLines = followLines.slice(-5000);
					}
					await tick();
					if (logEl) {
						logEl.scrollTop = logEl.scrollHeight;
					}
				} else if (msg.type === 'error') {
					followLines = [...followLines, `[error] ${msg.data}`];
				}
			} catch { /* ignore parse errors */ }
		};

		ws.onclose = () => {
			if (following) {
				following = false;
			}
		};
	}

	function stopFollow() {
		following = false;
		if (followWs) {
			followWs.close();
			followWs = null;
		}
		if (followLines.length > 0) {
			content = followLines.join('\n');
			followLines = [];
		}
	}

	function toggleFollow() {
		if (following) {
			stopFollow();
		} else {
			startFollow();
		}
	}

	// Displayed lines with client-side search highlighting
	const displayContent = $derived.by(() => {
		const raw = following ? followLines.join('\n') : content;
		if (!raw) return '';
		if (!searchQuery.trim()) return raw;
		return raw;
	});

	const matchCount = $derived.by(() => {
		if (!searchQuery.trim() || !displayContent) return 0;
		const q = searchQuery.toLowerCase();
		let count = 0;
		for (const line of displayContent.split('\n')) {
			if (line.toLowerCase().includes(q)) count++;
		}
		return count;
	});

	const filteredContent = $derived.by(() => {
		if (!searchQuery.trim() || !displayContent) return displayContent;
		const q = searchQuery.toLowerCase();
		return displayContent
			.split('\n')
			.filter(line => line.toLowerCase().includes(q))
			.join('\n');
	});
</script>

<div>
	<h1 class="text-2xl font-bold">Logs</h1>
	<p class="text-sm text-muted-foreground mt-0.5">View and stream systemd journal logs for NASty services.</p>
</div>

<div class="mt-4 flex flex-wrap items-center gap-2">
	<select
		bind:value={selectedUnit}
		onchange={() => { if (content || following) { stopFollow(); load(); } }}
		class="h-8 rounded-md border border-input bg-transparent px-2 text-sm"
		disabled={following}
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
		onchange={() => { if (content && !following) load(); }}
		class="h-8 rounded-md border border-input bg-transparent px-2 text-sm"
		disabled={following}
	>
		<option value={50}>50 lines</option>
		<option value={100}>100 lines</option>
		<option value={200}>200 lines</option>
		<option value={500}>500 lines</option>
		<option value={1000}>1000 lines</option>
	</select>
	<input
		type="text"
		bind:value={grepPattern}
		placeholder="grep pattern"
		class="h-8 w-36 rounded-md border border-input bg-transparent px-2 text-sm font-mono"
		disabled={following}
		onkeydown={(e) => { if (e.key === 'Enter') load(); }}
	/>
	<Button size="sm" onclick={load} disabled={loading || following}>
		{loading ? 'Loading...' : 'Load'}
	</Button>
	<Button
		size="sm"
		variant={following ? 'destructive' : 'secondary'}
		onclick={toggleFollow}
		disabled={loading}
	>
		{following ? 'Stop' : 'Follow'}
	</Button>
</div>

{#if content || following}
	<div class="mt-3 flex items-center gap-2">
		<Input
			type="text"
			bind:value={searchQuery}
			placeholder="Search / filter logs..."
			class="h-7 max-w-xs text-sm font-mono"
		/>
		{#if searchQuery.trim()}
			<span class="text-xs text-muted-foreground">
				{matchCount} matching line{matchCount !== 1 ? 's' : ''}
			</span>
		{/if}
	</div>

	<pre
		bind:this={logEl}
		class="mt-2 max-h-[calc(100vh-260px)] overflow-auto rounded-lg bg-[#0f1117] p-3 text-xs text-green-400 font-mono whitespace-pre-wrap"
	>{filteredContent}</pre>
{:else}
	<p class="mt-4 text-sm text-muted-foreground">Select a service and click Load or Follow to view logs.</p>
{/if}
