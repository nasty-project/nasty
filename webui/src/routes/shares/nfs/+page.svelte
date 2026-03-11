<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { NfsShare } from '$lib/types';

	let shares: NfsShare[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newPath = $state('');
	let newComment = $state('');
	let newHost = $state('');
	let newOptions = $state('rw,sync,no_subtree_check');

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			shares = await client.call<NfsShare[]>('share.nfs.list');
		});
	}

	async function create() {
		if (!newPath || !newHost) return;
		const ok = await withToast(
			() => client.call('share.nfs.create', {
				path: newPath,
				comment: newComment || undefined,
				clients: [{ host: newHost, options: newOptions }],
			}),
			'NFS share created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newPath = '';
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
</script>

<h1>NFS Shares</h1>

<div class="toolbar">
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Share'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>New NFS Share</h3>
		<div class="field">
			<label for="nfs-path">Path</label>
			<input id="nfs-path" bind:value={newPath} placeholder="/mnt/nasty/tank/data" />
		</div>
		<div class="field">
			<label for="nfs-comment">Comment</label>
			<input id="nfs-comment" bind:value={newComment} placeholder="Optional description" />
		</div>
		<div class="field">
			<label for="nfs-host">Allowed Network</label>
			<input id="nfs-host" bind:value={newHost} placeholder="192.168.1.0/24" />
		</div>
		<div class="field">
			<label for="nfs-opts">Options</label>
			<input id="nfs-opts" bind:value={newOptions} />
		</div>
		<button onclick={create} disabled={!newPath || !newHost}>Create</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if shares.length === 0}
	<p class="muted">No NFS shares configured.</p>
{:else}
	<table>
		<thead>
			<tr>
				<th>Path</th>
				<th>Clients</th>
				<th>Status</th>
				<th>Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each shares as share}
				<tr>
					<td>
						<strong class="mono">{share.path}</strong>
						{#if share.comment}<br /><span class="muted">{share.comment}</span>{/if}
					</td>
					<td>
						{#each share.clients as c}
							<div class="client">{c.host} <span class="muted">({c.options})</span></div>
						{/each}
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
	.mono { font-family: monospace; font-size: 0.85rem; }
	.muted { color: #6b7280; font-size: 0.8rem; }
	.client { font-size: 0.85rem; margin: 0.15rem 0; }
	.badge { padding: 0.2rem 0.5rem; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
	.badge.enabled { background: #064e3b; color: #4ade80; }
	.badge.disabled { background: #374151; color: #9ca3af; }
	.actions { display: flex; gap: 0.5rem; }
</style>
