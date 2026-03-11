<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { IscsiTarget } from '$lib/types';

	let targets: IscsiTarget[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newName = $state('');

	// Add LUN form
	let lunTarget = $state<string | null>(null);
	let lunPath = $state('');
	let lunType = $state('block');

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			targets = await client.call<IscsiTarget[]>('share.iscsi.list');
		});
	}

	async function create() {
		if (!newName) return;
		const ok = await withToast(
			() => client.call('share.iscsi.create', { name: newName }),
			'iSCSI target created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			await refresh();
		}
	}

	async function remove(id: string) {
		if (!confirm('Delete this iSCSI target and all its LUNs?')) return;
		await withToast(
			() => client.call('share.iscsi.delete', { id }),
			'iSCSI target deleted'
		);
		await refresh();
	}

	async function addLun() {
		if (!lunTarget || !lunPath) return;
		const ok = await withToast(
			() => client.call('share.iscsi.add_lun', {
				target_id: lunTarget,
				backstore_path: lunPath,
				backstore_type: lunType,
			}),
			'LUN added'
		);
		if (ok !== undefined) {
			lunTarget = null;
			lunPath = '';
			await refresh();
		}
	}

	async function removeLun(targetId: string, lunId: number) {
		if (!confirm(`Remove LUN ${lunId}?`)) return;
		await withToast(
			() => client.call('share.iscsi.remove_lun', { target_id: targetId, lun_id: lunId }),
			'LUN removed'
		);
		await refresh();
	}
</script>

<h1>iSCSI Targets</h1>

<div class="toolbar">
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Target'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>New iSCSI Target</h3>
		<div class="field">
			<label for="iscsi-name">Name</label>
			<input id="iscsi-name" bind:value={newName} placeholder="dbserver" />
			<span class="hint">IQN: iqn.2024-01.com.nasty:{newName || '...'}</span>
		</div>
		<button onclick={create} disabled={!newName}>Create</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if targets.length === 0}
	<p class="muted">No iSCSI targets configured.</p>
{:else}
	{#each targets as target}
		<div class="target-card">
			<div class="target-header">
				<div>
					<strong class="mono">{target.iqn}</strong>
					{#if target.alias}<span class="muted"> ({target.alias})</span>{/if}
				</div>
				<div class="actions">
					<button class="secondary" onclick={() => { lunTarget = target.id; lunPath = ''; }}>Add LUN</button>
					<button class="danger" onclick={() => remove(target.id)}>Delete</button>
				</div>
			</div>

			<div class="target-section">
				<h4>Portals</h4>
				{#if target.portals.length === 0}
					<span class="muted">None</span>
				{:else}
					{#each target.portals as p}
						<span class="tag">{p.ip}:{p.port}</span>
					{/each}
				{/if}
			</div>

			<div class="target-section">
				<h4>LUNs</h4>
				{#if target.luns.length === 0}
					<span class="muted">No LUNs</span>
				{:else}
					<table class="inner-table">
						<thead><tr><th>LUN</th><th>Type</th><th>Path</th><th></th></tr></thead>
						<tbody>
							{#each target.luns as lun}
								<tr>
									<td>{lun.lun_id}</td>
									<td><span class="tag">{lun.backstore_type}</span></td>
									<td class="mono">{lun.backstore_path}</td>
									<td><button class="danger small" onclick={() => removeLun(target.id, lun.lun_id)}>Remove</button></td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			</div>

			<div class="target-section">
				<h4>ACLs</h4>
				{#if target.acls.length === 0}
					<span class="muted">Open (any initiator)</span>
				{:else}
					{#each target.acls as acl}
						<div class="mono">{acl.initiator_iqn}</div>
					{/each}
				{/if}
			</div>
		</div>
	{/each}
{/if}

{#if lunTarget}
	<div class="modal-overlay" role="presentation" onclick={() => lunTarget = null} onkeydown={(e) => { if (e.key === 'Escape') lunTarget = null; }}>
		<div class="modal" role="dialog" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<h3>Add LUN</h3>
			<div class="field">
				<label for="lun-type">Type</label>
				<select id="lun-type" bind:value={lunType}>
					<option value="block">Block Device</option>
					<option value="fileio">File I/O</option>
				</select>
			</div>
			<div class="field">
				<label for="lun-path">Path</label>
				<input id="lun-path" bind:value={lunPath} placeholder={lunType === 'block' ? '/dev/sdb' : '/mnt/nasty/pool/disk.img'} />
			</div>
			<div class="modal-actions">
				<button onclick={addLun} disabled={!lunPath}>Add</button>
				<button class="secondary" onclick={() => lunTarget = null}>Cancel</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.toolbar { margin: 1rem 0; }
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 450px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input, .field select { width: 100%; box-sizing: border-box; }
	.hint { font-size: 0.75rem; color: #6b7280; }
	.mono { font-family: monospace; font-size: 0.85rem; }
	.muted { color: #6b7280; font-size: 0.8rem; }
	.tag { display: inline-block; background: #1e2130; padding: 0.15rem 0.4rem; border-radius: 3px; font-size: 0.75rem; margin: 0.1rem; }
	.target-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.25rem; margin-bottom: 1rem; }
	.target-header { display: flex; justify-content: space-between; align-items: start; margin-bottom: 1rem; }
	.target-section { margin-top: 0.75rem; }
	.target-section h4 { margin: 0 0 0.4rem; color: #9ca3af; font-size: 0.75rem; text-transform: uppercase; }
	.actions { display: flex; gap: 0.5rem; }
	.inner-table { margin-top: 0.25rem; }
	:global(button.small) { padding: 0.2rem 0.5rem; font-size: 0.75rem; }
	.modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.6); display: flex; align-items: center; justify-content: center; z-index: 100; }
	.modal { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; min-width: 380px; }
	.modal h3 { margin: 0 0 1rem; }
	.modal-actions { display: flex; gap: 0.5rem; }
</style>
