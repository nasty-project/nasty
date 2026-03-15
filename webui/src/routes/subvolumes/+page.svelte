<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { Pool, Subvolume, SubvolumeType } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Dialog from '$lib/components/ui/dialog';
	import SortTh from '$lib/components/SortTh.svelte';

	let pools: Pool[] = $state([]);
	let selectedPool = $state('');
	let subvolumes: Subvolume[] = $state([]);
	let loading = $state(true);

	let showCreate = $state(false);
	let newName = $state('');
	let newType: SubvolumeType = $state('filesystem');
	let newVolsize = $state('');
	let newCompression = $state('');
	let newComments = $state('');

	let showSnap = $state<string | null>(null);
	let snapName = $state('');

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'subvolume' || p?.collection === 'snapshot') refresh();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		pools = await client.call<Pool[]>('pool.list');
		const mounted = pools.filter(p => p.mounted);
		if (mounted.length > 0) {
			selectedPool = mounted[0].name;
			await refresh();
		}
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		if (!selectedPool) return;
		await withToast(async () => {
			subvolumes = await client.call<Subvolume[]>('subvolume.list', { pool: selectedPool });
		});
	}

	async function selectPool(name: string) {
		selectedPool = name;
		await refresh();
	}

	async function createSubvolume() {
		if (!newName || !selectedPool) return;
		if (newType === 'block' && !newVolsize) return;

		const params: Record<string, unknown> = {
			pool: selectedPool,
			name: newName,
			subvolume_type: newType,
		};
		if (newType === 'block' && newVolsize) {
			params.volsize_bytes = parseFloat(newVolsize) * 1073741824;
		}
		if (newCompression) params.compression = newCompression;
		if (newComments) params.comments = newComments;

		const ok = await withToast(
			() => client.call('subvolume.create', params),
			`Subvolume "${newName}" created`
		);
		if (ok !== undefined) {
			newName = '';
			newType = 'filesystem';
			newVolsize = '';
			newCompression = '';
			newComments = '';
			showCreate = false;
			await refresh();
		}
	}

	async function deleteSubvolume(name: string) {
		if (!await confirm(`Delete Subvolume`, `Delete subvolume "${name}" and all its snapshots?`)) return;
		await withToast(
			() => client.call('subvolume.delete', { pool: selectedPool, name }),
			`Subvolume "${name}" deleted`
		);
		await refresh();
	}

	async function attachSubvolume(name: string) {
		await withToast(
			() => client.call('subvolume.attach', { pool: selectedPool, name }),
			`Loop device attached for "${name}"`
		);
		await refresh();
	}

	async function detachSubvolume(name: string) {
		await withToast(
			() => client.call('subvolume.detach', { pool: selectedPool, name }),
			`Loop device detached for "${name}"`
		);
		await refresh();
	}

	async function createSnapshot() {
		if (!showSnap || !snapName) return;
		const ok = await withToast(
			() => client.call('snapshot.create', {
				pool: selectedPool,
				subvolume: showSnap,
				name: snapName,
				read_only: true,
			}),
			`Snapshot "${snapName}" created`
		);
		if (ok !== undefined) {
			showSnap = null;
			snapName = '';
		}
		await refresh();
	}

	async function deleteSnapshot(subvolume: string, snap: string) {
		if (!await confirm(`Delete Snapshot`, `Delete snapshot "${snap}"?`)) return;
		await withToast(
			() => client.call('snapshot.delete', {
				pool: selectedPool,
				subvolume,
				name: snap,
			}),
			`Snapshot "${snap}" deleted`
		);
		await refresh();
	}

	const mountedPools = $derived(pools.filter(p => p.mounted));

	let search = $state('');

	type SortKey = 'name' | 'type' | 'size';
	let sortKey = $state<SortKey | null>(null);
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort(key: SortKey) {
		if (sortKey === key) {
			sortDir = sortDir === 'asc' ? 'desc' : 'asc';
		} else {
			sortKey = key;
			sortDir = 'asc';
		}
	}

	function svSize(sv: Subvolume): number {
		return sv.subvolume_type === 'block' ? (sv.volsize_bytes ?? 0) : (sv.used_bytes ?? 0);
	}

	const filtered = $derived(
		search.trim()
			? subvolumes.filter(sv =>
				sv.name.toLowerCase().includes(search.toLowerCase()) ||
				sv.comments?.toLowerCase().includes(search.toLowerCase()))
			: subvolumes
	);

	const sorted = $derived.by(() => {
		if (!sortKey) return filtered;
		return [...filtered].sort((a, b) => {
			let cmp = 0;
			if (sortKey === 'name') cmp = a.name.localeCompare(b.name);
			else if (sortKey === 'type') cmp = a.subvolume_type.localeCompare(b.subvolume_type);
			else if (sortKey === 'size') cmp = svSize(a) - svSize(b);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

<h1 class="mb-4 text-2xl font-bold">Subvolumes</h1>

{#if mountedPools.length > 0}
	<div class="mb-4 flex items-center gap-4">
		<select value={selectedPool} onchange={(e) => selectPool((e.target as HTMLSelectElement).value)} class="h-9 w-auto rounded-md border border-input bg-transparent px-3 text-sm">
			{#each mountedPools as p}
				<option value={p.name}>{p.name}</option>
			{/each}
		</select>
		<Button onclick={() => showCreate = !showCreate}>
			{showCreate ? 'Cancel' : 'Create Subvolume'}
		</Button>
		<Input bind:value={search} placeholder="Search..." class="h-9 w-48" />
	</div>
{/if}

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">Create Subvolume in "{selectedPool}"</h3>
			<div class="mb-4">
				<Label for="sv-name">Name</Label>
				<Input id="sv-name" bind:value={newName} placeholder="documents" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="sv-type">Type</Label>
				<select id="sv-type" bind:value={newType} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="filesystem">Filesystem (NFS, SMB)</option>
					<option value="block">Block Device (iSCSI, NVMe-oF)</option>
				</select>
			</div>
			{#if newType === 'block'}
				<div class="mb-4">
					<Label for="sv-volsize">Volume Size (GiB)</Label>
					<Input id="sv-volsize" type="number" bind:value={newVolsize} placeholder="100" min="1" class="mt-1" />
				</div>
			{/if}
			<div class="mb-4">
				<Label for="sv-compression">Compression</Label>
				<select id="sv-compression" bind:value={newCompression} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">None</option>
					<option value="lz4">LZ4</option>
					<option value="zstd">Zstd</option>
					<option value="gzip">Gzip</option>
				</select>
			</div>
			<div class="mb-4">
				<Label for="sv-comments">Comments</Label>
				<Input id="sv-comments" bind:value={newComments} placeholder="Optional description" class="mt-1" />
			</div>
			<Button onclick={createSubvolume} disabled={!newName || (newType === 'block' && !newVolsize)}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if pools.length === 0}
	<p class="text-muted-foreground">No pools configured. Create a pool first.</p>
{:else if mountedPools.length === 0}
	<p class="text-muted-foreground">No mounted pools. Mount a pool first.</p>
{:else if subvolumes.length === 0}
	<p class="text-muted-foreground">No subvolumes in pool "{selectedPool}".</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={sortKey === 'name'} dir={sortDir} onclick={() => toggleSort('name')} />
				<SortTh label="Type" active={sortKey === 'type'} dir={sortDir} onclick={() => toggleSort('type')} />
				<SortTh label="Size" active={sortKey === 'size'} dir={sortDir} onclick={() => toggleSort('size')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Block Device</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Snapshots</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as sv}
				<tr class="border-b border-border">
					<td class="p-3">
						<strong>{sv.name}</strong>
						<span class="block font-mono text-xs text-muted-foreground">{sv.path}</span>
						{#if sv.comments}
							<span class="mt-0.5 block text-xs italic text-muted-foreground">{sv.comments}</span>
						{/if}
					</td>
					<td class="p-3">
						<Badge variant={sv.subvolume_type === 'filesystem' ? 'secondary' : 'outline'}
							class={sv.subvolume_type === 'filesystem' ? 'bg-blue-950 text-blue-400' : 'bg-purple-950 text-purple-400'}>
							{sv.subvolume_type === 'filesystem' ? 'Filesystem' : 'Block'}
						</Badge>
					</td>
					<td class="p-3 text-sm">
						{#if sv.subvolume_type === 'block' && sv.volsize_bytes}
							{formatBytes(sv.volsize_bytes)}
							{#if sv.used_bytes !== null}
								<span class="text-xs text-muted-foreground">({formatBytes(sv.used_bytes)} on disk)</span>
							{/if}
						{:else if sv.used_bytes !== null}
							{formatBytes(sv.used_bytes)}
						{:else}
							—
						{/if}
					</td>
					<td class="p-3">
						{#if sv.subvolume_type === 'block'}
							{#if sv.block_device}
								<span class="font-mono text-xs">{sv.block_device}</span>
								<Button variant="secondary" size="sm" class="ml-2" onclick={() => detachSubvolume(sv.name)}>Detach</Button>
							{:else}
								<span class="text-muted-foreground">Detached</span>
								<Button variant="secondary" size="sm" class="ml-2" onclick={() => attachSubvolume(sv.name)}>Attach</Button>
							{/if}
						{:else}
							<span class="text-muted-foreground">N/A</span>
						{/if}
					</td>
					<td class="p-3">
						{#if sv.snapshots.length === 0}
							<span class="text-muted-foreground">None</span>
						{:else}
							{#each sv.snapshots as snap}
								<div class="my-0.5 flex items-center gap-2">
									<span class="font-mono text-xs">{snap}</span>
									<Button variant="destructive" size="sm" onclick={() => deleteSnapshot(sv.name, snap)}>Delete</Button>
								</div>
							{/each}
						{/if}
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" onclick={() => { showSnap = sv.name; snapName = ''; }}>Snapshot</Button>
							<Button variant="destructive" onclick={() => deleteSubvolume(sv.name)}>Delete</Button>
						</div>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<Dialog.Root open={showSnap !== null} onOpenChange={(open) => { if (!open) showSnap = null; }}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Snapshot "{showSnap}"</Dialog.Title>
		</Dialog.Header>
		<div class="mb-4">
			<Label for="snap-name">Snapshot Name</Label>
			<Input id="snap-name" bind:value={snapName} placeholder="snap-2026-03-12" class="mt-1" />
		</div>
		<Dialog.Footer>
			<Button onclick={createSnapshot} disabled={!snapName}>Create</Button>
			<Button variant="secondary" onclick={() => showSnap = null}>Cancel</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
