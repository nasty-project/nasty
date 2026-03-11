<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import type { Pool, Subvolume, Snapshot } from '$lib/types';

	let pools: Pool[] = $state([]);
	let selectedPool = $state('');
	let subvolumes: Subvolume[] = $state([]);
	let snapshots: Snapshot[] = $state([]);
	let loading = $state(true);

	let showCreate = $state(false);
	let newSubvolName = $state('');
	let showSnap = $state<string | null>(null);
	let snapName = $state('');
	let snapReadOnly = $state(true);

	const client = getClient();

	onMount(async () => {
		pools = await client.call<Pool[]>('pool.list');
		if (pools.length > 0) {
			selectedPool = pools[0].name;
			await refresh();
		}
		loading = false;
	});

	async function refresh() {
		if (!selectedPool) return;
		await withToast(async () => {
			subvolumes = await client.call<Subvolume[]>('subvolume.list', { pool: selectedPool });
			snapshots = await client.call<Snapshot[]>('snapshot.list', { pool: selectedPool });
		});
	}

	async function selectPool(name: string) {
		selectedPool = name;
		await refresh();
	}

	async function createSubvolume() {
		if (!newSubvolName || !selectedPool) return;
		const ok = await withToast(
			() => client.call('subvolume.create', { pool: selectedPool, name: newSubvolName }),
			`Subvolume "${newSubvolName}" created`
		);
		if (ok !== undefined) {
			newSubvolName = '';
			showCreate = false;
			await refresh();
		}
	}

	async function deleteSubvolume(name: string) {
		if (!confirm(`Delete subvolume "${name}" and all its snapshots?`)) return;
		await withToast(
			() => client.call('subvolume.delete', { pool: selectedPool, name }),
			`Subvolume "${name}" deleted`
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
				read_only: snapReadOnly,
			}),
			`Snapshot "${snapName}" created`
		);
		if (ok !== undefined) {
			showSnap = null;
			snapName = '';
		}
		await refresh();
	}

	async function deleteSnapshot(subvolume: string, name: string) {
		if (!confirm(`Delete snapshot "${name}"?`)) return;
		await withToast(
			() => client.call('snapshot.delete', { pool: selectedPool, subvolume, name }),
			`Snapshot "${name}" deleted`
		);
		await refresh();
	}
</script>

<h1>Subvolumes</h1>

{#if pools.length > 0}
	<div class="toolbar">
		<select value={selectedPool} onchange={(e) => selectPool((e.target as HTMLSelectElement).value)}>
			{#each pools as p}
				<option value={p.name}>{p.name}</option>
			{/each}
		</select>
		<button onclick={() => showCreate = !showCreate}>
			{showCreate ? 'Cancel' : 'Create Subvolume'}
		</button>
	</div>
{/if}

{#if showCreate}
	<div class="form-card">
		<h3>Create Subvolume in "{selectedPool}"</h3>
		<div class="field">
			<label for="subvol-name">Name</label>
			<input id="subvol-name" bind:value={newSubvolName} placeholder="documents" />
		</div>
		<button onclick={createSubvolume} disabled={!newSubvolName}>Create</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if pools.length === 0}
	<p class="muted">No pools configured. Create a pool first.</p>
{:else if subvolumes.length === 0}
	<p class="muted">No subvolumes in pool "{selectedPool}".</p>
{:else}
	<table>
		<thead>
			<tr>
				<th>Name</th>
				<th>Size</th>
				<th>Snapshots</th>
				<th>Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each subvolumes as sv}
				<tr>
					<td><strong>{sv.name}</strong><br /><span class="path">{sv.path}</span></td>
					<td>{sv.size_bytes !== null ? formatBytes(sv.size_bytes) : '—'}</td>
					<td>
						{#if sv.snapshots.length === 0}
							<span class="muted">None</span>
						{:else}
							{#each sv.snapshots as snap}
								<div class="snap-row">
									<span class="mono">{snap}</span>
									<button class="danger small" onclick={() => deleteSnapshot(sv.name, snap)}>Delete</button>
								</div>
							{/each}
						{/if}
					</td>
					<td class="actions">
						<button class="secondary" onclick={() => { showSnap = sv.name; snapName = ''; }}>
							Snapshot
						</button>
						<button class="danger" onclick={() => deleteSubvolume(sv.name)}>Delete</button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

{#if showSnap}
	<div class="modal-overlay" role="presentation" onclick={() => showSnap = null} onkeydown={(e) => { if (e.key === 'Escape') showSnap = null; }}>
		<div class="modal" role="dialog" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<h3>Snapshot "{showSnap}"</h3>
			<div class="field">
				<label for="snap-name">Snapshot Name</label>
				<input id="snap-name" bind:value={snapName} placeholder="snap-2024-01-15" />
			</div>
			<label class="checkbox">
				<input type="checkbox" bind:checked={snapReadOnly} /> Read-only
			</label>
			<div class="modal-actions">
				<button onclick={createSnapshot} disabled={!snapName}>Create</button>
				<button class="secondary" onclick={() => showSnap = null}>Cancel</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.toolbar { display: flex; gap: 1rem; align-items: center; margin: 1rem 0; }
	.toolbar select { width: auto; }
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 400px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input { width: 100%; box-sizing: border-box; }
	.muted { color: #6b7280; }
	.path { font-family: monospace; font-size: 0.75rem; color: #6b7280; }
	.mono { font-family: monospace; font-size: 0.8rem; }
	.actions { display: flex; gap: 0.5rem; }
	.snap-row { display: flex; align-items: center; gap: 0.5rem; margin: 0.2rem 0; }
	:global(button.small) { padding: 0.2rem 0.5rem; font-size: 0.75rem; }
	.checkbox { display: flex; align-items: center; gap: 0.5rem; margin-bottom: 1rem; cursor: pointer; }
	.checkbox input { width: auto; }
	.modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.6); display: flex; align-items: center; justify-content: center; z-index: 100; }
	.modal { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; min-width: 350px; }
	.modal h3 { margin: 0 0 1rem; }
	.modal-actions { display: flex; gap: 0.5rem; }
</style>
