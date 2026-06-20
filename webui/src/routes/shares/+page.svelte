<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Link2, Lock, Ban, Trash2 } from '@lucide/svelte';

	// Mirrors the engine's GuestShare (engine/nasty-engine/src/guestshare.rs).
	// Note: only `token_hash` is stored, never the plaintext token — so a
	// share's link cannot be reconstructed here. Links are shown once, at
	// creation time, in the Files-page Share dialog.
	interface GuestShare {
		id: string;
		token_hash: string;
		paths: string[];
		created_by: string;
		created_at: number;
		expires_at: number | null;
		password_hash: string | null;
		max_downloads: number | null;
		downloads: number;
		views: number;
		revoked: boolean;
		hidden: boolean;
		note: string | null;
	}

	let shares = $state<GuestShare[] | null>(null);
	// Removed shares are kept (for audit/history) but hidden by default; the
	// toggle reveals them without a refetch since list() returns them all.
	let showRemoved = $state(false);

	const visibleShares = $derived(
		shares === null ? null : showRemoved ? shares : shares.filter((s) => !s.hidden)
	);
	const removedCount = $derived(shares?.filter((s) => s.hidden).length ?? 0);

	const nowSecs = () => Math.floor(Date.now() / 1000);

	function basename(p: string): string {
		const parts = p.split('/').filter(Boolean);
		return parts.length ? parts[parts.length - 1] : p;
	}

	function fmtDate(secs: number | null): string {
		if (!secs) return '—';
		return new Date(secs * 1000).toLocaleString();
	}

	function isExpired(s: GuestShare): boolean {
		return s.expires_at != null && s.expires_at <= nowSecs();
	}

	function isExhausted(s: GuestShare): boolean {
		return s.max_downloads != null && s.downloads >= s.max_downloads;
	}

	async function load() {
		shares = (await getClient().call<GuestShare[]>('guestshare.list')) ?? [];
	}

	async function revoke(s: GuestShare) {
		const names = s.paths.map(basename).join(', ');
		if (!(await confirm(`Revoke share of "${names}"?`, 'The link will stop working immediately. This cannot be undone.'))) {
			return;
		}
		await withToast(() => getClient().call('guestshare.revoke', { id: s.id }), 'Share revoked');
		await load();
	}

	// Remove only applies to already-revoked shares; the record is kept on
	// disk (and in the audit log) but hidden from the default list.
	async function remove(s: GuestShare) {
		const names = s.paths.map(basename).join(', ');
		if (!(await confirm(`Remove "${names}" from the list?`, 'It stays in the audit log; the row is hidden until you toggle "Show removed".'))) {
			return;
		}
		await withToast(() => getClient().call('guestshare.remove', { id: s.id }), 'Share removed');
		await load();
	}

	onMount(load);
</script>

<div class="mx-auto max-w-6xl p-6">
	<h1 class="text-2xl font-semibold">Guest Shares</h1>
	<p class="mt-1 mb-6 text-sm text-muted-foreground">
		Public links to files and folders under <code>/fs</code>. Create one from the
		<a href="/files" class="underline">Files</a> page. For security the link itself is shown only
		once, when it's created, and can't be retrieved here.
	</p>

	{#if removedCount > 0}
		<label class="mb-4 flex items-center gap-2 text-sm text-muted-foreground">
			<input type="checkbox" bind:checked={showRemoved} class="h-4 w-4" />
			Show removed ({removedCount})
		</label>
	{/if}

	{#if shares === null}
		<p class="text-muted-foreground">Loading…</p>
	{:else if visibleShares && visibleShares.length === 0}
		<p class="text-muted-foreground">
			{shares.length === 0 ? 'No guest shares yet.' : 'No shares to show.'}
		</p>
	{:else}
		<table class="w-full text-sm">
			<thead>
				<tr>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Shared</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Created by</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Created</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Expires</th>
					<th class="border-b-2 border-border p-3 text-right text-xs uppercase text-muted-foreground">Downloads</th>
					<th class="border-b-2 border-border p-3 text-right text-xs uppercase text-muted-foreground">Views</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
					<th class="border-b-2 border-border p-3 text-right text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each visibleShares ?? [] as s (s.id)}
					<tr class="border-b border-border {s.hidden ? 'opacity-50' : ''}">
						<td class="p-3">
							<div class="flex items-center gap-2">
								<Link2 size={14} class="text-muted-foreground shrink-0" />
								<span class="font-medium">{s.paths.map(basename).join(', ')}</span>
								{#if s.password_hash}
									<Lock size={13} class="text-amber-500 shrink-0" />
								{/if}
							</div>
							{#if s.note}
								<div class="text-xs text-muted-foreground">{s.note}</div>
							{/if}
						</td>
						<td class="p-3">{s.created_by}</td>
						<td class="p-3 text-muted-foreground text-xs tabular-nums">{fmtDate(s.created_at)}</td>
						<td class="p-3 text-xs tabular-nums {isExpired(s) ? 'text-destructive' : 'text-muted-foreground'}">
							{s.expires_at ? fmtDate(s.expires_at) : 'Never'}
						</td>
						<td class="p-3 text-right tabular-nums">{s.downloads}{s.max_downloads != null ? ` / ${s.max_downloads}` : ''}</td>
						<td class="p-3 text-right tabular-nums text-muted-foreground">{s.views}</td>
						<td class="p-3">
							{#if s.hidden}
								<Badge variant="secondary">Removed</Badge>
							{:else if s.revoked}
								<Badge variant="secondary">Revoked</Badge>
							{:else if isExpired(s)}
								<Badge variant="secondary">Expired</Badge>
							{:else if isExhausted(s)}
								<Badge variant="secondary">Exhausted</Badge>
							{:else}
								<Badge class="bg-emerald-500/15 text-emerald-600 hover:bg-emerald-500/15">Active</Badge>
							{/if}
						</td>
						<td class="p-3 text-right">
							{#if !s.revoked}
								<Button variant="ghost" size="sm" onclick={() => revoke(s)} title="Revoke">
									<Ban size={14} class="mr-1" /> Revoke
								</Button>
							{:else if !s.hidden}
								<Button variant="ghost" size="sm" onclick={() => remove(s)} title="Remove from list (kept for audit)">
									<Trash2 size={14} class="mr-1" /> Remove
								</Button>
							{:else}
								<span class="text-xs text-muted-foreground">—</span>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
</div>
