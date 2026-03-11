<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { SmbShare } from '$lib/types';

	let shares: SmbShare[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newName = $state('');
	let newPath = $state('');
	let newComment = $state('');
	let newReadOnly = $state(false);
	let newGuestOk = $state(false);

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			shares = await client.call<SmbShare[]>('share.smb.list');
		});
	}

	async function create() {
		if (!newName || !newPath) return;
		const ok = await withToast(
			() => client.call('share.smb.create', {
				name: newName,
				path: newPath,
				comment: newComment || undefined,
				read_only: newReadOnly,
				guest_ok: newGuestOk,
			}),
			'SMB share created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			newPath = '';
			newComment = '';
			await refresh();
		}
	}

	async function toggleEnabled(share: SmbShare) {
		await withToast(
			() => client.call('share.smb.update', { id: share.id, enabled: !share.enabled }),
			`SMB share ${share.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}

	async function remove(id: string) {
		if (!confirm('Delete this SMB share?')) return;
		await withToast(
			() => client.call('share.smb.delete', { id }),
			'SMB share deleted'
		);
		await refresh();
	}
</script>

<h1>SMB Shares</h1>

<div class="toolbar">
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Share'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>New SMB Share</h3>
		<div class="field">
			<label for="smb-name">Share Name</label>
			<input id="smb-name" bind:value={newName} placeholder="documents" />
		</div>
		<div class="field">
			<label for="smb-path">Path</label>
			<input id="smb-path" bind:value={newPath} placeholder="/mnt/nasty/tank/data" />
		</div>
		<div class="field">
			<label for="smb-comment">Comment</label>
			<input id="smb-comment" bind:value={newComment} placeholder="Optional description" />
		</div>
		<div class="checkboxes">
			<label><input type="checkbox" bind:checked={newReadOnly} /> Read-only</label>
			<label><input type="checkbox" bind:checked={newGuestOk} /> Allow guests</label>
		</div>
		<button onclick={create} disabled={!newName || !newPath}>Create</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if shares.length === 0}
	<p class="muted">No SMB shares configured.</p>
{:else}
	<table>
		<thead>
			<tr>
				<th>Name</th>
				<th>Path</th>
				<th>Access</th>
				<th>Status</th>
				<th>Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each shares as share}
				<tr>
					<td>
						<strong>{share.name}</strong>
						{#if share.comment}<br /><span class="muted">{share.comment}</span>{/if}
					</td>
					<td class="mono">{share.path}</td>
					<td>
						<span class="tag">{share.read_only ? 'RO' : 'RW'}</span>
						{#if share.guest_ok}<span class="tag">Guest</span>{/if}
						{#if share.valid_users.length > 0}
							<span class="tag">Users: {share.valid_users.join(', ')}</span>
						{/if}
					</td>
					<td>
						<span class="badge" class:enabled={share.enabled} class:disabled={!share.enabled}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</span>
					</td>
					<td class="actions">
						<button class="secondary" onclick={() => toggleEnabled(share)}>
							{share.enabled ? 'Disable' : 'Enable'}
						</button>
						<button class="danger" onclick={() => remove(share.id)}>Delete</button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<style>
	.toolbar { margin: 1rem 0; }
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 450px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input { width: 100%; box-sizing: border-box; }
	.checkboxes { display: flex; gap: 1.5rem; margin-bottom: 1rem; }
	.checkboxes label { display: flex; align-items: center; gap: 0.5rem; cursor: pointer; }
	.checkboxes input { width: auto; }
	.mono { font-family: monospace; font-size: 0.85rem; }
	.muted { color: #6b7280; font-size: 0.8rem; }
	.tag { display: inline-block; background: #1e2130; padding: 0.15rem 0.4rem; border-radius: 3px; font-size: 0.75rem; margin: 0.1rem; }
	.badge { padding: 0.2rem 0.5rem; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
	.badge.enabled { background: #064e3b; color: #4ade80; }
	.badge.disabled { background: #374151; color: #9ca3af; }
	.actions { display: flex; gap: 0.5rem; }
</style>
