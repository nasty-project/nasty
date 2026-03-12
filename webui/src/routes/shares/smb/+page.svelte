<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { SmbShare, Subvolume, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';

	let shares: SmbShare[] = $state([]);
	let subvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);
	let protocol: ProtocolStatus | null = $state(null);

	let newSubvolume = $state('');
	let newName = $state('');
	let newComment = $state('');
	let newReadOnly = $state(false);
	let newGuestOk = $state(false);

	const client = getClient();

	$effect(() => {
		if (showCreate) {
			loadSubvolumes();
		}
	});

	onMount(async () => {
		await refresh();
		await loadProtocol();
		loading = false;
	});

	async function loadProtocol() {
		try {
			const all = await client.call<ProtocolStatus[]>('service.protocol.list');
			protocol = all.find(p => p.name === 'smb') ?? null;
		} catch { /* ignore */ }
	}

	async function refresh() {
		await withToast(async () => {
			shares = await client.call<SmbShare[]>('share.smb.list');
		});
	}

	async function loadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			subvolumes = all.filter(s => s.subvolume_type === 'filesystem');
		});
	}

	function onSubvolumeSelect() {
		// Auto-fill share name from subvolume name if empty
		if (newSubvolume && !newName) {
			const sv = subvolumes.find(s => s.path === newSubvolume);
			if (sv) newName = sv.name;
		}
	}

	async function create() {
		if (!newName || !newSubvolume) return;
		const ok = await withToast(
			() => client.call('share.smb.create', {
				name: newName,
				path: newSubvolume,
				comment: newComment || undefined,
				read_only: newReadOnly,
				guest_ok: newGuestOk,
			}),
			'SMB share created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newSubvolume = '';
			newName = '';
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

<h1 class="mb-4 text-2xl font-bold">SMB Shares</h1>

{#if protocol}
	<Card class="mb-4">
		<CardContent class="flex items-center gap-4 py-3">
			<Badge variant={protocol.running ? 'default' : 'destructive'}>
				{protocol.running ? 'Running' : 'Stopped'}
			</Badge>
			<span class="text-sm text-muted-foreground">
				{shares.length} share{shares.length !== 1 ? 's' : ''}
				{#if shares.length > 0}
					&middot; Connect with: <code class="rounded bg-secondary px-1.5 py-0.5 text-xs">\\{window.location.hostname}\&lt;name&gt;</code>
				{/if}
			</span>
			{#if !protocol.enabled}
				<Badge variant="secondary">Disabled</Badge>
			{/if}
		</CardContent>
	</Card>
{/if}

<div class="mb-4">
	<Button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Share'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New SMB Share</h3>
			<div class="mb-4">
				<Label for="smb-subvol">Subvolume</Label>
				<select id="smb-subvol" bind:value={newSubvolume} onchange={onSubvolumeSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
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
				<Label for="smb-name">Share Name</Label>
				<Input id="smb-name" bind:value={newName} placeholder="documents" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">Name visible to network clients</span>
			</div>
			<div class="mb-4">
				<Label for="smb-comment">Comment</Label>
				<Input id="smb-comment" bind:value={newComment} placeholder="Optional description" class="mt-1" />
			</div>
			<div class="mb-4 flex gap-6">
				<label class="flex cursor-pointer items-center gap-2">
					<input type="checkbox" bind:checked={newReadOnly} class="h-4 w-4" /> Read-only
				</label>
				<label class="flex cursor-pointer items-center gap-2">
					<input type="checkbox" bind:checked={newGuestOk} class="h-4 w-4" /> Allow guests
				</label>
			</div>
			<Button onclick={create} disabled={!newName || !newSubvolume}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if shares.length === 0}
	<p class="text-muted-foreground">No SMB shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Name</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Path</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Access</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each shares as share}
				<tr class="border-b border-border">
					<td class="p-3">
						<strong>{share.name}</strong>
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3 font-mono text-sm">{share.path}</td>
					<td class="p-3">
						<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">{share.read_only ? 'RO' : 'RW'}</span>
						{#if share.guest_ok}<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">Guest</span>{/if}
						{#if share.valid_users.length > 0}
							<span class="inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">Users: {share.valid_users.join(', ')}</span>
						{/if}
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
