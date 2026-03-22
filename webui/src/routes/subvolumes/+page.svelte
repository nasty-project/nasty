<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { Pool, Subvolume, Snapshot, SubvolumeType } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Dialog from '$lib/components/ui/dialog';
	import SortTh from '$lib/components/SortTh.svelte';
	import { X, Camera, Copy, Trash2 } from '@lucide/svelte';

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

	// Detail panel
	let detailSv = $state<Subvolume | null>(null);
	let detailSnapshots = $state<Snapshot[]>([]);
	let detailTab = $state<'info' | 'snapshots' | 'properties'>('info');

	async function openDetail(sv: Subvolume) {
		detailSv = sv;
		detailTab = 'info';
		detailSnapshots = [];
		// Load full snapshot list for this subvolume
		try {
			detailSnapshots = await client.call<Snapshot[]>('snapshot.list', { pool: selectedPool });
			detailSnapshots = detailSnapshots.filter(s => s.subvolume === sv.name);
		} catch { /* ignore */ }
	}

	function closeDetail() {
		detailSv = null;
	}

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
		if (!await confirm(`Delete "${name}"?`, 'All snapshots will also be deleted.')) return;
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
		if (!await confirm(`Delete snapshot "${snap}"?`)) return;
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
	let sortKey = $state<SortKey | null>('name');
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


{#if mountedPools.length > 0}
	<div class="mb-4 flex items-center gap-4">
		<Button size="sm" onclick={() => showCreate = !showCreate}>
			{showCreate ? 'Cancel' : 'Create Subvolume'}
		</Button>
		<select value={selectedPool} onchange={(e) => selectPool((e.target as HTMLSelectElement).value)} class="h-9 w-auto rounded-md border border-input bg-transparent px-3 text-sm">
			{#each mountedPools as p}
				<option value={p.name}>{p.name}</option>
			{/each}
		</select>
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
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as sv}
				<tr class="border-b border-border">
					<td class="p-3">
						<button class="text-left hover:text-blue-400 transition-colors" onclick={() => openDetail(sv)}>
							<strong>{sv.name}</strong>
						</button>
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
								<Button variant="secondary" size="xs" class="ml-2" onclick={() => detachSubvolume(sv.name)}>Detach</Button>
							{:else}
								<span class="text-muted-foreground">Detached</span>
								<Button variant="secondary" size="xs" class="ml-2" onclick={() => attachSubvolume(sv.name)}>Attach</Button>
							{/if}
						{:else}
							<span class="text-muted-foreground">N/A</span>
						{/if}
					</td>
					<td class="p-3">
						{#if sv.snapshots.length === 0}
							<span class="text-muted-foreground">None</span>
						{:else if sv.snapshots.length <= 2}
							{#each sv.snapshots as snap}
								<div class="my-0.5 flex items-center gap-2">
									<span class="font-mono text-xs">{snap}</span>
									<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(sv.name, snap)}>Delete</Button>
								</div>
							{/each}
						{:else}
							<button class="text-sm text-blue-400 hover:text-blue-300 transition-colors" onclick={() => { openDetail(sv); detailTab = 'snapshots'; }}>
								{sv.snapshots.length} snapshots
							</button>
						{/if}
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => { showSnap = sv.name; snapName = ''; }}>Snapshot</Button>
							<Button variant="destructive" size="xs" onclick={() => deleteSubvolume(sv.name)}>Delete</Button>
						</div>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<!-- Detail Panel -->
{#if detailSv}
	<div class="fixed inset-y-0 right-0 z-40 flex w-[480px] flex-col border-l border-border bg-background shadow-xl">
		<!-- Header -->
		<div class="flex items-center justify-between border-b border-border px-5 py-4">
			<div>
				<h2 class="text-lg font-semibold">{detailSv.name}</h2>
				<span class="text-xs text-muted-foreground font-mono">{detailSv.path}</span>
			</div>
			<button onclick={closeDetail} class="rounded-md p-1 hover:bg-accent transition-colors">
				<X class="h-5 w-5" />
			</button>
		</div>

		<!-- Tabs -->
		<div class="flex border-b border-border">
			<button
				onclick={() => detailTab = 'info'}
				class="px-4 py-2 text-sm font-medium transition-colors {detailTab === 'info'
					? 'border-b-2 border-primary text-foreground'
					: 'text-muted-foreground hover:text-foreground'}"
			>Info</button>
			<button
				onclick={() => detailTab = 'snapshots'}
				class="px-4 py-2 text-sm font-medium transition-colors {detailTab === 'snapshots'
					? 'border-b-2 border-primary text-foreground'
					: 'text-muted-foreground hover:text-foreground'}"
			>Snapshots ({detailSv.snapshots.length})</button>
			<button
				onclick={() => detailTab = 'properties'}
				class="px-4 py-2 text-sm font-medium transition-colors {detailTab === 'properties'
					? 'border-b-2 border-primary text-foreground'
					: 'text-muted-foreground hover:text-foreground'}"
			>Properties</button>
		</div>

		<!-- Tab content -->
		<div class="flex-1 overflow-y-auto p-5">
			{#if detailTab === 'info'}
				<div class="space-y-3">
					<div class="grid grid-cols-2 gap-x-4 gap-y-2 text-sm">
						<div class="text-muted-foreground">Pool</div>
						<div class="font-mono">{detailSv.pool}</div>

						<div class="text-muted-foreground">Type</div>
						<div>
							<Badge variant={detailSv.subvolume_type === 'filesystem' ? 'secondary' : 'outline'}
								class={detailSv.subvolume_type === 'filesystem' ? 'bg-blue-950 text-blue-400' : 'bg-purple-950 text-purple-400'}>
								{detailSv.subvolume_type === 'filesystem' ? 'Filesystem' : 'Block'}
							</Badge>
						</div>

						{#if detailSv.compression}
							<div class="text-muted-foreground">Compression</div>
							<div>{detailSv.compression}</div>
						{/if}

						{#if detailSv.subvolume_type === 'block' && detailSv.volsize_bytes}
							<div class="text-muted-foreground">Volume Size</div>
							<div>{formatBytes(detailSv.volsize_bytes)}</div>
						{/if}

						{#if detailSv.used_bytes !== null}
							<div class="text-muted-foreground">Used</div>
							<div>{formatBytes(detailSv.used_bytes)}</div>
						{/if}

						{#if detailSv.block_device}
							<div class="text-muted-foreground">Block Device</div>
							<div class="font-mono text-xs">{detailSv.block_device}</div>
						{/if}

						{#if detailSv.owner}
							<div class="text-muted-foreground">Owner</div>
							<div class="font-mono text-xs">{detailSv.owner}</div>
						{/if}

						{#if detailSv.comments}
							<div class="text-muted-foreground">Comments</div>
							<div class="text-xs">{detailSv.comments}</div>
						{/if}
					</div>

					<div class="mt-4 flex gap-2">
						<Button size="sm" variant="secondary" onclick={() => { showSnap = detailSv?.name ?? null; snapName = ''; }}>
							<Camera class="mr-1.5 h-3.5 w-3.5" />Snapshot
						</Button>
						<Button size="sm" variant="destructive" onclick={() => { if (detailSv) { closeDetail(); deleteSubvolume(detailSv.name); } }}>
							<Trash2 class="mr-1.5 h-3.5 w-3.5" />Delete
						</Button>
					</div>
				</div>

			{:else if detailTab === 'snapshots'}
				{#if detailSv.snapshots.length === 0}
					<p class="text-sm text-muted-foreground">No snapshots.</p>
				{:else}
					<div class="space-y-2">
						{#each detailSnapshots as snap}
							<div class="flex items-center justify-between rounded-md border border-border p-3">
								<div>
									<div class="font-mono text-sm">{snap.name}</div>
									<div class="text-xs text-muted-foreground">
										{snap.read_only ? 'Read-only' : 'Writable'}
										{#if snap.parent}
											· Parent: {snap.parent}
										{/if}
									</div>
									<div class="font-mono text-xs text-muted-foreground">{snap.path}</div>
								</div>
								<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(detailSv!.name, snap.name)}>
									<Trash2 class="h-3.5 w-3.5" />
								</Button>
							</div>
						{/each}
						{#if detailSnapshots.length === 0 && detailSv.snapshots.length > 0}
							<!-- Fallback: show names from subvolume data if snapshot list didn't load -->
							{#each detailSv.snapshots as snapName}
								<div class="flex items-center justify-between rounded-md border border-border p-3">
									<span class="font-mono text-sm">{snapName}</span>
									<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(detailSv!.name, snapName)}>
										<Trash2 class="h-3.5 w-3.5" />
									</Button>
								</div>
							{/each}
						{/if}
					</div>
				{/if}

			{:else if detailTab === 'properties'}
				{#if detailSv.properties && Object.keys(detailSv.properties).length > 0}
					<div class="space-y-1">
						{#each Object.entries(detailSv.properties).sort(([a], [b]) => a.localeCompare(b)) as [key, value]}
							<div class="flex items-start justify-between gap-2 rounded-md border border-border px-3 py-2">
								<span class="font-mono text-xs text-muted-foreground break-all">{key}</span>
								<span class="font-mono text-xs text-right break-all">{value}</span>
							</div>
						{/each}
					</div>
				{:else}
					<p class="text-sm text-muted-foreground">No properties set.</p>
				{/if}
			{/if}
		</div>
	</div>
	<!-- Backdrop -->
	<button class="fixed inset-0 z-30 bg-black/30" onclick={closeDetail}></button>
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
			<Button size="sm" onclick={createSnapshot} disabled={!snapName}>Create</Button>
			<Button variant="secondary" size="sm" onclick={() => showSnap = null}>Cancel</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
