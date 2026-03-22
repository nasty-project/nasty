<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { SmbShare, Subvolume, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';

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

	let expanded = $state<Record<string, boolean>>({});
	let addUserShare = $state<string | null>(null);
	let addUserName = $state('');


	const client = getClient();

	$effect(() => {
		if (showCreate) {
			loadSubvolumes();
		}
	});

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'share.smb') refresh();
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
			`Share ${share.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}

	async function remove(id: string) {
		if (!await confirm('Delete this SMB share?')) return;
		await withToast(
			() => client.call('share.smb.delete', { id }),
			'SMB share deleted'
		);
		await refresh();
	}

	async function toggleField(share: SmbShare, field: 'read_only' | 'browseable' | 'guest_ok') {
		await withToast(
			() => client.call('share.smb.update', { id: share.id, [field]: !share[field] }),
			'Share updated'
		);
		await refresh();
	}

	async function removeUser(share: SmbShare, username: string) {
		const valid_users = share.valid_users.filter(u => u !== username);
		await withToast(
			() => client.call('share.smb.update', { id: share.id, valid_users }),
			'User removed'
		);
		await refresh();
	}

	async function addUser(share: SmbShare) {
		if (!addUserName) return;
		const valid_users = [...share.valid_users, addUserName];
		const ok = await withToast(
			() => client.call('share.smb.update', { id: share.id, valid_users }),
			'User added'
		);
		if (ok !== undefined) {
			addUserShare = null;
			addUserName = '';
		}
		await refresh();
	}

	let search = $state('');

	type SortKey = 'name' | 'path' | 'status';
	let sortKey = $state<SortKey | null>(null);
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort(key: SortKey) {
		if (sortKey === key) sortDir = sortDir === 'asc' ? 'desc' : 'asc';
		else { sortKey = key; sortDir = 'asc'; }
	}

	const filtered = $derived(
		search.trim()
			? shares.filter(s =>
				s.name.toLowerCase().includes(search.toLowerCase()) ||
				s.path.toLowerCase().includes(search.toLowerCase()) ||
				s.comment?.toLowerCase().includes(search.toLowerCase()))
			: shares
	);

	const sorted = $derived.by(() => {
		if (!sortKey) return filtered;
		return [...filtered].sort((a, b) => {
			let cmp = 0;
			if (sortKey === 'name') cmp = a.name.localeCompare(b.name);
			else if (sortKey === 'path') cmp = a.path.localeCompare(b.path);
			else if (sortKey === 'status') cmp = Number(b.enabled) - Number(a.enabled);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>


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

<div class="mb-4 flex items-center gap-3">
	<Button size="sm" onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Share'}
	</Button>
	<Input bind:value={search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Share</h3>
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
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={sortKey === 'name'} dir={sortDir} onclick={() => toggleSort('name')} />
				<SortTh label="Path" active={sortKey === 'path'} dir={sortDir} onclick={() => toggleSort('path')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Access</th>
				<SortTh label="Status" active={sortKey === 'status'} dir={sortDir} onclick={() => toggleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as share}
				<tr
					class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors"
					onclick={() => expanded[share.id] = !expanded[share.id]}
				>
					<td class="p-3">
						<strong>{share.name}</strong>
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3 font-mono text-sm">{share.path}</td>
					<td class="p-3">
						<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">{share.read_only ? 'RO' : 'RW'}</span>
						{#if share.guest_ok}<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">Guest</span>{/if}
						{#if share.valid_users.length > 0}
							<span class="inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">{share.valid_users.length} user{share.valid_users.length !== 1 ? 's' : ''}</span>
						{/if}
					</td>
					<td class="p-3">
						<Badge variant={share.enabled ? 'default' : 'secondary'}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => expanded[share.id] = !expanded[share.id]}>
								{expanded[share.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="secondary" size="xs" onclick={() => toggleEnabled(share)}>
								{share.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => remove(share.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if expanded[share.id]}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="5" class="px-6 py-4">
							<div class="flex gap-12">
								<div>
									<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Settings</p>
									<div class="space-y-2">
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.read_only} onchange={() => toggleField(share, 'read_only')} class="h-4 w-4" />
											Read-only
										</label>
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.browseable} onchange={() => toggleField(share, 'browseable')} class="h-4 w-4" />
											Browseable
										</label>
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.guest_ok} onchange={() => toggleField(share, 'guest_ok')} class="h-4 w-4" />
											Allow guests
										</label>
									</div>
								</div>
								<div class="flex-1">
									<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Valid Users</p>
									{#if share.valid_users.length === 0}
										<p class="mb-3 text-xs text-muted-foreground">No restrictions — all authenticated users may access.</p>
									{:else}
										<div class="mb-3 space-y-1.5">
											{#each share.valid_users as username}
												<div class="flex items-center gap-3">
													<code class="text-xs">{username}</code>
													<Button variant="destructive" size="xs" onclick={(e) => { e.stopPropagation(); removeUser(share, username); }}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if addUserShare === share.id}
										<div class="flex items-end gap-2" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
											<div>
												<Label class="text-xs">Username</Label>
												<Input bind:value={addUserName} placeholder="johndoe" class="mt-1 h-8 w-40 text-xs" />
											</div>
											<Button size="xs" onclick={() => addUser(share)} disabled={!addUserName}>Add</Button>
											<Button variant="secondary" size="xs" onclick={() => { addUserShare = null; addUserName = ''; }}>Cancel</Button>
										</div>
									{:else}
										<Button variant="secondary" size="xs" onclick={(e) => { e.stopPropagation(); addUserShare = share.id; addUserName = ''; }}>
											Add User
										</Button>
									{/if}
								</div>
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}

