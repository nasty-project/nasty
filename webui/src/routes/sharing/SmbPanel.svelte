<script lang="ts">
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import {
		smb,
		smbToggleSort,
		smbRefresh,
		smbCreate,
		smbToggleEnabled,
		smbRemove,
		smbToggleField,
		smbRemoveUser,
		smbOnSubvolumeSelect,
		smbLoadSubvolumes,
		smbEnsureSystemUsers,
	} from '$lib/sharing/smb.svelte';

	const client = getClient();

	/** Gates the amber required-field decoration on the SMB create form
	 * until the operator has clicked Create at least once with a missing
	 * field. Without it the form opens "alarmed" before any input. */
	let createTried = $state(false);
	async function smbCreateGuarded() {
		if (!smb.newName || !smb.newSubvolume) { createTried = true; return; }
		createTried = false;
		await smbCreate();
	}

	$effect(() => { if (smb.showCreate) smbLoadSubvolumes(); });

	const smbFiltered = $derived(
		smb.search.trim()
			? smb.shares.filter(s =>
				s.name.toLowerCase().includes(smb.search.toLowerCase()) ||
				s.path.toLowerCase().includes(smb.search.toLowerCase()) ||
				s.comment?.toLowerCase().includes(smb.search.toLowerCase()))
			: smb.shares
	);

	const smbSorted = $derived.by(() => {
		if (!smb.sortKey) return smbFiltered;
		return [...smbFiltered].sort((a, b) => {
			let cmp = 0;
			if (smb.sortKey === 'name') cmp = a.name.localeCompare(b.name);
			else if (smb.sortKey === 'path') cmp = a.path.localeCompare(b.path);
			else if (smb.sortKey === 'status') cmp = Number(b.enabled) - Number(a.enabled);
			return smb.sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={smb.search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if smb.showCreate}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Share</h3>
			<div class="mb-4">
				<Label for="smb-subvol">Subvolume {#if !smb.newSubvolume && createTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
				<select id="smb-subvol" bind:value={smb.newSubvolume} onchange={smbOnSubvolumeSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm {requiredFieldCls(!smb.newSubvolume, createTried)}">
					<option value="">Select a subvolume...</option>
					{#each smb.subvolumes as sv}
						<option value={sv.path}>{sv.filesystem}/{sv.name} ({sv.path})</option>
					{/each}
				</select>
				{#if smb.subvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No filesystem subvolumes found.</span>
					<Button size="xs" class="mt-1" onclick={() => goto('/subvolumes')}>Subvolumes</Button>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="smb-name">Share Name {#if !smb.newName && createTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
				<Input id="smb-name" bind:value={smb.newName} placeholder="documents" class="mt-1 {requiredFieldCls(!smb.newName, createTried)}" />
				<span class="mt-1 block text-xs text-muted-foreground">Name visible to network clients</span>
			</div>
			<div class="mb-4">
				<Label for="smb-comment">Comment</Label>
				<Input id="smb-comment" bind:value={smb.newComment} placeholder="Optional description" class="mt-1" />
			</div>
			<div class="mb-4">
				<label class="flex cursor-pointer items-center gap-2">
					<input
						type="checkbox"
						bind:checked={smb.newTimeMachine}
						onchange={() => { if (smb.newTimeMachine) { smb.newGuestOk = false; smb.newReadOnly = false; } }}
						class="h-4 w-4" />
					Time Machine — macOS backup destination
				</label>
				{#if smb.newTimeMachine}
					<div class="mt-2 ml-6 flex items-center gap-2 text-sm">
						<span class="text-muted-foreground">Max size (GiB)</span>
						<input
							type="number"
							min="1"
							placeholder="unlimited"
							value={smb.newTmMaxSize ?? ''}
							oninput={(e) => { const v = (e.target as HTMLInputElement).value; smb.newTmMaxSize = v === '' ? null : Number(v); }}
							class="h-8 w-32 rounded-md border border-input bg-transparent px-2 text-sm" />
					</div>
				{/if}
			</div>
			{#if !smb.newTimeMachine}
				<div class="mb-4 flex gap-6">
					<label class="flex cursor-pointer items-center gap-2">
						<input type="checkbox" bind:checked={smb.newReadOnly} class="h-4 w-4" /> Read-only
					</label>
					<label class="flex cursor-pointer items-center gap-2">
						<input type="checkbox" bind:checked={smb.newGuestOk} class="h-4 w-4" /> Allow guests
					</label>
				</div>
			{/if}
			<Button onclick={smbCreateGuarded}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if smb.loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if smb.shares.length === 0}
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={smb.sortKey === 'name'} dir={smb.sortDir} onclick={() => smbToggleSort('name')} />
				<SortTh label="Path" active={smb.sortKey === 'path'} dir={smb.sortDir} onclick={() => smbToggleSort('path')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Access</th>
				<SortTh label="Status" active={smb.sortKey === 'status'} dir={smb.sortDir} onclick={() => smbToggleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each smbSorted as share}
				<tr
					class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors"
					onclick={() => smb.expanded[share.id] = !smb.expanded[share.id]}
				>
					<td class="p-3">
						<strong>{share.name}</strong>
						{#if share.time_machine}
							<Badge variant="secondary" class="ml-2 bg-blue-950 text-blue-300">Time Machine</Badge>
						{/if}
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3 font-mono text-sm">{share.path}</td>
					<td class="p-3">
						<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">{share.read_only ? 'RO' : 'RW'}</span>
						{#if share.guest_ok}<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">Guest</span>{/if}
						{#if share.valid_users.length > 0}
							{@const userCount = share.valid_users.filter(u => !u.startsWith('@')).length}
							{@const groupCount = share.valid_users.filter(u => u.startsWith('@')).length}
							<span class="inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">
								{#if userCount > 0}{userCount} user{userCount !== 1 ? 's' : ''}{/if}{#if userCount > 0 && groupCount > 0}, {/if}{#if groupCount > 0}{groupCount} group{groupCount !== 1 ? 's' : ''}{/if}
							</span>
						{/if}
					</td>
					<td class="p-3">
						<Badge variant={share.enabled ? 'default' : 'secondary'}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => smb.expanded[share.id] = !smb.expanded[share.id]}>
								{smb.expanded[share.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="secondary" size="xs" onclick={() => smbToggleEnabled(share)}>
								{share.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => smbRemove(share.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if smb.expanded[share.id]}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="5" class="px-6 py-4">
							<div class="flex gap-12">
								<div>
									<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Settings</p>
									<div class="space-y-2">
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.read_only} onchange={() => smbToggleField(share, 'read_only')} class="h-4 w-4" />
											Read-only
										</label>
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.browseable} onchange={() => smbToggleField(share, 'browseable')} class="h-4 w-4" />
											Browseable
										</label>
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.guest_ok} onchange={() => smbToggleField(share, 'guest_ok')} class="h-4 w-4" />
											Allow guests
										</label>
									</div>
								</div>
								<div class="flex-1">
									<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Valid Users & Groups</p>
									{#if share.valid_users.length === 0}
										<p class="mb-3 text-xs text-muted-foreground">No restrictions — all authenticated users may access.</p>
									{:else}
										<div class="mb-3 flex flex-wrap gap-2">
											{#each share.valid_users as entry}
												<span class="flex items-center gap-1 rounded-md border px-2 py-1 text-xs {entry.startsWith('@') ? 'border-blue-500/40 bg-blue-500/10' : 'border-border'}">
													{#if entry.startsWith('@')}
														<span class="text-blue-400">{entry}</span>
													{:else}
														{entry}
													{/if}
													<button class="ml-1 text-muted-foreground hover:text-destructive" onclick={(e) => { e.stopPropagation(); smbRemoveUser(share, entry); }}>&times;</button>
												</span>
											{/each}
										</div>
									{/if}
									{#if smb.addUserShare === share.id}
										<div class="flex flex-wrap gap-1.5" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
											{#each smb.systemUsers.filter(u => !share.valid_users.includes(u.username)) as user}
												<Button size="xs" variant="secondary" onclick={() => {
													const valid_users = [...share.valid_users, user.username];
													withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), `${user.username} added`).then(() => smbRefresh());
												}}>{user.username}</Button>
											{/each}
											{#each smb.groups.filter(g => !share.valid_users.includes(`@${g.name}`)) as group}
												<Button size="xs" variant="secondary" class="text-blue-400" onclick={() => {
													const valid_users = [...share.valid_users, `@${group.name}`];
													withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), `@${group.name} added`).then(() => smbRefresh());
												}}>@{group.name}</Button>
											{/each}
											<Button size="xs" variant="secondary" onclick={() => goto('/users')}>Create User / Group</Button>
											<Button variant="secondary" size="xs" onclick={() => { smb.addUserShare = null; }}>Done</Button>
										</div>
									{:else}
										<Button variant="secondary" size="xs" onclick={(e) => {
											e.stopPropagation();
											smb.addUserShare = share.id;
											smbEnsureSystemUsers();
										}}>
											Add User / Group
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
