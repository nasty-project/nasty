<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import SortTh from '$lib/components/SortTh.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import {
		nfs,
		nfsToggleSort,
		nfsToggleEnabled,
		nfsRemove,
		nfsAddClient,
		nfsRemoveClient,
		nfsLoadSubvolumes,
	} from '$lib/sharing/nfs.svelte';

	$effect(() => { if (nfs.showCreate) nfsLoadSubvolumes(); });

	/** Per-panel "tried Add" flag — defers the amber required-field
	 * decoration until the operator has clicked Add at least once
	 * with the host field empty. Reset when the Add Client panel
	 * collapses (showAddClientShare is cleared elsewhere). */
	let addClientTried = $state(false);

	async function nfsAddClickHost(share: Parameters<typeof nfsAddClient>[0]) {
		if (!nfs.addClientHost) { addClientTried = true; return; }
		addClientTried = false;
		await nfsAddClient(share);
	}

	const nfsFiltered = $derived(
		nfs.search.trim()
			? nfs.shares.filter(s =>
				s.path.toLowerCase().includes(nfs.search.toLowerCase()) ||
				s.comment?.toLowerCase().includes(nfs.search.toLowerCase()) ||
				s.clients.some(c => c.host.includes(nfs.search)))
			: nfs.shares
	);

	const nfsSorted = $derived.by(() => {
		if (!nfs.sortKey) return nfsFiltered;
		return [...nfsFiltered].sort((a, b) => {
			let cmp = 0;
			if (nfs.sortKey === 'path') cmp = a.path.localeCompare(b.path);
			else if (nfs.sortKey === 'status') cmp = Number(b.enabled) - Number(a.enabled);
			return nfs.sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={nfs.search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if nfs.loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if nfs.shares.length === 0}
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Path" active={nfs.sortKey === 'path'} dir={nfs.sortDir} onclick={() => nfsToggleSort('path')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Clients</th>
				<SortTh label="Status" active={nfs.sortKey === 'status'} dir={nfs.sortDir} onclick={() => nfsToggleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each nfsSorted as share}
				<tr
					class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors"
					onclick={() => nfs.expanded[share.id] = !nfs.expanded[share.id]}
				>
					<td class="p-3">
						<span class="font-mono text-sm">{share.path}</span>
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{share.clients.length} client{share.clients.length !== 1 ? 's' : ''}
					</td>
					<td class="p-3">
						<Badge variant={share.enabled ? 'default' : 'secondary'}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => nfs.expanded[share.id] = !nfs.expanded[share.id]}>
								{nfs.expanded[share.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="secondary" size="xs" onclick={() => nfsToggleEnabled(share)}>
								{share.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => nfsRemove(share.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if nfs.expanded[share.id]}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="4" class="px-6 py-4">
							<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Allowed Clients</p>
							{#if share.clients.length === 0}
								<p class="mb-3 text-xs text-muted-foreground">No clients configured.</p>
							{:else}
								<div class="mb-3 space-y-1.5">
									{#each share.clients as c}
										<div class="flex items-center gap-3">
											<code class="text-xs">{c.host}</code>
											<span class="text-xs text-muted-foreground">({c.options})</span>
											{#if c.options.includes('no_root_squash')}
												<span class="text-xs text-yellow-500" title="no_root_squash disables quota enforcement for root clients">⚠ quota</span>
											{/if}
											<Button variant="destructive" size="xs" onclick={() => nfsRemoveClient(share, c.host)}>Remove</Button>
										</div>
									{/each}
								</div>
							{/if}
							{#if nfs.addClientShare === share.id}
								<div class="flex items-end gap-2">
									<div>
										<Label class="text-xs">Host / Network {#if !nfs.addClientHost && addClientTried}<span class="text-amber-500">required</span>{/if}</Label>
										<Input bind:value={nfs.addClientHost} placeholder="192.168.1.0/24" class="mt-1 h-8 w-44 text-xs {requiredFieldCls(!nfs.addClientHost, addClientTried)}" />
									</div>
									<div>
										<Label class="text-xs">Options</Label>
										<Input bind:value={nfs.addClientOptions} class="mt-1 h-8 w-56 text-xs" />
									</div>
									<Button size="xs" onclick={() => nfsAddClickHost(share)}>Add</Button>
									<Button variant="secondary" size="xs" onclick={() => { nfs.addClientShare = null; nfs.addClientHost = ''; addClientTried = false; }}>Cancel</Button>
								</div>
								{#if nfs.addClientOptions.includes('no_root_squash')}
									<p class="mt-1 text-xs text-yellow-500">Warning: <code>no_root_squash</code> disables quota enforcement for root NFS clients.</p>
								{/if}
							{:else}
								<Button variant="secondary" size="xs" onclick={() => { nfs.addClientShare = share.id; nfs.addClientHost = ''; nfs.addClientOptions = 'rw,sync,no_subtree_check'; }}>
									Add Client
								</Button>
							{/if}
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}
