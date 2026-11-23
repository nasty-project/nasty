<script lang="ts">
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import SortTh from '$lib/components/SortTh.svelte';
	import {
		smb,
		smbToggleSort,
		smbRefresh,
		smbToggleEnabled,
		smbRemove,
		smbToggleField,
		smbRemoveUser,
		smbEnsureSystemUsers,
	} from '$lib/sharing/smb.svelte';
	import { domain, domainSearchUsers } from '$lib/domain.svelte';
	import type { SmbShare, DomainPrincipal } from '$lib/types';

	const client = getClient();

	let domainSearch = $state('');
	let domainHits: DomainPrincipal[] = $state([]);

	/** Append `entry` (a system username or `@group`) to a share's
	 * valid_users and persist. Shared by the system-user/group buttons
	 * and the domain-user search results below them. */
	function addValidUser(share: SmbShare, entry: string) {
		const valid_users = [...share.valid_users, entry];
		withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), `${entry} added`).then(() => smbRefresh());
	}

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
										<div role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
											<div class="flex flex-wrap gap-1.5">
												{#each smb.systemUsers.filter(u => !share.valid_users.includes(u.username)) as user}
													<Button size="xs" variant="secondary" onclick={() => addValidUser(share, user.username)}>{user.username}</Button>
												{/each}
												{#each smb.groups.filter(g => !share.valid_users.includes(`@${g.name}`)) as group}
													<Button size="xs" variant="secondary" class="text-blue-400" onclick={() => addValidUser(share, `@${group.name}`)}>@{group.name}</Button>
												{/each}
												<Button size="xs" variant="secondary" onclick={() => goto('/users')}>Create User / Group</Button>
												<Button variant="secondary" size="xs" onclick={() => { smb.addUserShare = null; }}>Done</Button>
											</div>
											{#if domain.status?.joined}
												<div class="mt-2">
													<Input bind:value={domainSearch} placeholder="Search domain users (2+ chars)…" class="h-8 text-xs"
														oninput={async () => { domainHits = await domainSearchUsers(domainSearch); }} />
													{#each domainHits.filter(p => !share.valid_users.includes(p.name)) as p}
														<button class="block w-full rounded px-2 py-1 text-left font-mono text-xs hover:bg-secondary/50"
															onclick={() => addValidUser(share, p.name)}>
															{p.name}
														</button>
													{/each}
												</div>
											{/if}
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
