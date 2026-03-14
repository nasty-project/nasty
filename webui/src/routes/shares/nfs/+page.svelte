<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { NfsShare, Subvolume, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';

	let shares: NfsShare[] = $state([]);
	let subvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);
	let protocol: ProtocolStatus | null = $state(null);

	let newSubvolume = $state('');
	let newComment = $state('');
	let newHost = $state('');
	let newOptions = $state('rw,sync,no_subtree_check');

	const client = getClient();

	$effect(() => {
		if (showCreate) {
			loadSubvolumes();
		}
	});

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'share.nfs') refresh();
		if (p?.collection === 'protocol') loadProtocol();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		await loadProtocol();
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function loadProtocol() {
		try {
			const all = await client.call<ProtocolStatus[]>('service.protocol.list');
			protocol = all.find(p => p.name === 'nfs') ?? null;
		} catch { /* ignore */ }
	}

	async function refresh() {
		await withToast(async () => {
			shares = await client.call<NfsShare[]>('share.nfs.list');
		});
	}

	async function loadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			subvolumes = all.filter(s => s.subvolume_type === 'filesystem');
		});
	}

	async function create() {
		if (!newSubvolume || !newHost) return;
		const ok = await withToast(
			() => client.call('share.nfs.create', {
				path: newSubvolume,
				comment: newComment || undefined,
				clients: [{ host: newHost, options: newOptions }],
			}),
			'NFS share created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newSubvolume = '';
			newComment = '';
			newHost = '';
			await refresh();
		}
	}

	async function toggleEnabled(share: NfsShare) {
		await withToast(
			() => client.call('share.nfs.update', { id: share.id, enabled: !share.enabled }),
			`NFS share ${share.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}

	async function remove(id: string) {
		if (!confirm('Delete this NFS share?')) return;
		await withToast(
			() => client.call('share.nfs.delete', { id }),
			'NFS share deleted'
		);
		await refresh();
	}

	function pathLabel(path: string): string {
		// /mnt/nasty/tank/data -> tank/data
		return path.replace(/^\/mnt\/nasty\//, '');
	}

	let search = $state('');

	type SortKey = 'path' | 'status';
	let sortKey = $state<SortKey | null>(null);
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort(key: SortKey) {
		if (sortKey === key) sortDir = sortDir === 'asc' ? 'desc' : 'asc';
		else { sortKey = key; sortDir = 'asc'; }
	}

	const filtered = $derived(
		search.trim()
			? shares.filter(s =>
				s.path.toLowerCase().includes(search.toLowerCase()) ||
				s.comment?.toLowerCase().includes(search.toLowerCase()) ||
				s.clients.some(c => c.host.includes(search)))
			: shares
	);

	const sorted = $derived.by(() => {
		if (!sortKey) return filtered;
		return [...filtered].sort((a, b) => {
			let cmp = 0;
			if (sortKey === 'path') cmp = a.path.localeCompare(b.path);
			else if (sortKey === 'status') cmp = Number(b.enabled) - Number(a.enabled);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

<h1 class="mb-4 text-2xl font-bold">NFS Shares</h1>

{#if protocol}
	<Card class="mb-4">
		<CardContent class="flex items-center gap-4 py-3">
			<Badge variant={protocol.running ? 'default' : 'destructive'}>
				{protocol.running ? 'Running' : 'Stopped'}
			</Badge>
			<span class="text-sm text-muted-foreground">
				{shares.length} share{shares.length !== 1 ? 's' : ''}
				{#if shares.length > 0}
					&middot; Mount with: <code class="rounded bg-secondary px-1.5 py-0.5 text-xs">mount -t nfs {window.location.hostname}:&lt;path&gt; /mnt</code>
				{/if}
			</span>
			{#if !protocol.enabled}
				<Badge variant="secondary">Disabled</Badge>
			{/if}
		</CardContent>
	</Card>
{/if}

<div class="mb-4 flex items-center gap-3">
	<Button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Share'}
	</Button>
	<Input bind:value={search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New NFS Share</h3>
			<div class="mb-4">
				<Label for="nfs-path">Subvolume</Label>
				<select id="nfs-path" bind:value={newSubvolume} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a subvolume...</option>
					{#each subvolumes as sv}
						<option value={sv.path}>{sv.pool}/{sv.name}</option>
					{/each}
				</select>
				{#if subvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No filesystem subvolumes found. Create one first.</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="nfs-comment">Comment</Label>
				<Input id="nfs-comment" bind:value={newComment} placeholder="Optional description" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="nfs-host">Allowed Network</Label>
				<Input id="nfs-host" bind:value={newHost} placeholder="192.168.1.0/24" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="nfs-opts">Options</Label>
				<Input id="nfs-opts" bind:value={newOptions} class="mt-1" />
			</div>
			<Button onclick={create} disabled={!newSubvolume || !newHost}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if shares.length === 0}
	<p class="text-muted-foreground">No NFS shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Path" active={sortKey === 'path'} dir={sortDir} onclick={() => toggleSort('path')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Clients</th>
				<SortTh label="Status" active={sortKey === 'status'} dir={sortDir} onclick={() => toggleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as share}
				<tr class="border-b border-border">
					<td class="p-3">
						<strong class="text-sm">{pathLabel(share.path)}</strong>
						<br /><span class="font-mono text-xs text-muted-foreground">{share.path}</span>
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3">
						{#each share.clients as c}
							<div class="text-sm">{c.host} <span class="text-xs text-muted-foreground">({c.options})</span></div>
						{/each}
					</td>
					<td class="p-3">
						<Badge variant={share.enabled ? 'default' : 'secondary'}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" size="sm" onclick={() => toggleEnabled(share)}>
								{share.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="sm" onclick={() => remove(share.id)}>Delete</Button>
						</div>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}
