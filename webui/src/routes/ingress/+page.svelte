<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { goto } from '$app/navigation';
	import type { CaddyRouteSummary } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let routes: CaddyRouteSummary[] = $state([]);
	let loading = $state(true);
	let error: string | null = $state(null);
	let poll: ReturnType<typeof setInterval> | null = null;

	async function refresh() {
		try {
			routes = await client.call<CaddyRouteSummary[]>('apps.caddy.routes');
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message :
				(typeof e === 'object' && e !== null && 'message' in e) ?
				String((e as { message: unknown }).message) : String(e);
		}
	}

	onMount(async () => {
		await refresh();
		loading = false;
		// 5s mirrors the Apps page's idle poll cadence. Caddy config
		// changes whenever someone installs/removes an app or flips a
		// subdomain; a static cached view would silently rot.
		poll = setInterval(() => { if (!document.hidden) void refresh(); }, 5000);
	});

	onDestroy(() => {
		if (poll) { clearInterval(poll); poll = null; }
	});

	/** Group rows by Caddy server name so the HTTPS server's routes
	 * stay separate from the HTTP→HTTPS redirect on srv1. Sorted by
	 * server name (srv0 first) and then by match-kind so host routes
	 * cluster together. */
	const grouped = $derived.by(() => {
		const by_server = new Map<string, CaddyRouteSummary[]>();
		for (const r of routes) {
			if (!by_server.has(r.server)) by_server.set(r.server, []);
			by_server.get(r.server)!.push(r);
		}
		// Stable order within a group: host before path before catch_all,
		// then alphabetical by match_value within each kind. Keeps the
		// table predictable as Caddy reorders things underneath.
		const kindOrder: Record<string, number> = { host: 0, path: 1, catch_all: 2, other: 3 };
		for (const list of by_server.values()) {
			list.sort((a, b) => {
				const ko = (kindOrder[a.match_kind] ?? 9) - (kindOrder[b.match_kind] ?? 9);
				if (ko !== 0) return ko;
				return a.match_value.localeCompare(b.match_value);
			});
		}
		return [...by_server.entries()].sort(([a], [b]) => a.localeCompare(b));
	});

	function sourceBadge(source: string): { class: string; label: string } {
		if (source === 'engine-app') return { class: 'bg-blue-500/15 text-blue-400 border-blue-500/30', label: 'engine' };
		return { class: 'bg-muted text-muted-foreground border-border', label: 'static' };
	}

	function matchKindBadge(kind: string): { class: string; label: string } {
		switch (kind) {
			case 'host':      return { class: 'bg-emerald-500/15 text-emerald-400 border-emerald-500/30', label: 'host' };
			case 'path':      return { class: 'bg-amber-500/15 text-amber-400 border-amber-500/30', label: 'path' };
			case 'catch_all': return { class: 'bg-slate-800 text-slate-300 border-slate-600', label: 'catch-all' };
			default:          return { class: 'bg-muted text-muted-foreground border-border', label: kind };
		}
	}
</script>

<svelte:head><title>Ingress · NASty</title></svelte:head>

<div class="mb-4">
	<h1 class="text-2xl font-semibold">Ingress</h1>
	<p class="mt-1 text-sm text-muted-foreground">
		Every route Caddy is currently serving — both engine-managed app ingresses and the
		Caddyfile-baked WebUI / API / WebSocket routes. Read-only here; edit engine-app rows
		on the <a href="/apps" class="text-blue-400 hover:text-blue-300">Apps</a> page.
	</p>
</div>

{#if loading}
	<p class="text-sm text-muted-foreground">Loading…</p>
{:else if error}
	<div class="rounded-md border border-destructive/40 bg-destructive/10 p-4 text-sm">
		<div class="font-medium text-destructive">Could not load Caddy config</div>
		<div class="mt-1 text-xs text-muted-foreground font-mono">{error}</div>
		<Button class="mt-3" size="sm" onclick={refresh}>Retry</Button>
	</div>
{:else if routes.length === 0}
	<p class="text-sm text-muted-foreground">No routes — is Caddy running?</p>
{:else}
	{#each grouped as [server, list]}
		<div class="mb-6">
			<h2 class="mb-2 text-xs uppercase tracking-wide text-muted-foreground">
				{server === 'srv0' ? 'HTTPS server (srv0)' : server === 'srv1' ? 'HTTP redirect (srv1)' : server}
				<span class="ml-2 text-muted-foreground/60">{list.length} route{list.length === 1 ? '' : 's'}</span>
			</h2>
			<div class="overflow-x-auto rounded-md border border-border">
				<table class="w-full text-sm">
					<thead class="bg-muted/40">
						<tr>
							<th class="p-2 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Match</th>
							<th class="p-2 text-left text-xs uppercase text-muted-foreground">Value</th>
							<th class="p-2 text-left text-xs uppercase text-muted-foreground">Handler</th>
							<th class="p-2 text-left text-xs uppercase text-muted-foreground">Upstream</th>
							<th class="p-2 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Source</th>
							<th class="p-2 text-left text-xs uppercase text-muted-foreground">App</th>
						</tr>
					</thead>
					<tbody>
						{#each list as r}
							{@const sb = sourceBadge(r.source)}
							{@const mb = matchKindBadge(r.match_kind)}
							<tr class="border-t border-border">
								<td class="p-2">
									<span class="inline-flex items-center whitespace-nowrap rounded-md border px-2 py-0.5 text-[0.65rem] {mb.class}">{mb.label}</span>
								</td>
								<td class="p-2 font-mono text-xs break-all">{r.match_value}</td>
								<td class="p-2 text-xs text-muted-foreground">{r.handler_kind}</td>
								<td class="p-2 font-mono text-xs">
									{#if r.upstream}{r.upstream}{:else}<span class="text-muted-foreground">—</span>{/if}
								</td>
								<td class="p-2">
									<span class="inline-flex items-center whitespace-nowrap rounded-md border px-2 py-0.5 text-[0.65rem] {sb.class}">{sb.label}</span>
								</td>
								<td class="p-2 text-xs">
									{#if r.app_name}
										<!-- Engine-app rows link to the Apps page so the operator
										     can jump straight to editing the ingress (subdomain,
										     port, etc.) — static rows have no editor target. -->
										<button class="text-blue-400 hover:text-blue-300" onclick={() => goto('/apps')}>{r.app_name}</button>
									{:else}
										<span class="text-muted-foreground">—</span>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</div>
	{/each}
{/if}
