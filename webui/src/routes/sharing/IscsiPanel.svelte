<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import {
		iscsi,
		iscsiToggleSort,
		iscsiOnDeviceSelect,
		iscsiCreate,
		iscsiRemove,
		iscsiAddLun,
		iscsiRemoveLun,
		iscsiAddAcl,
		iscsiRemoveAcl,
		iscsiLoadSubvolumes,
	} from '$lib/sharing/iscsi.svelte';

	$effect(() => { if (iscsi.showCreate || iscsi.addLunTarget) iscsiLoadSubvolumes(); });

	const iscsiFiltered = $derived(
		iscsi.search.trim()
			? iscsi.targets.filter(t =>
				t.iqn.toLowerCase().includes(iscsi.search.toLowerCase()) ||
				t.alias?.toLowerCase().includes(iscsi.search.toLowerCase()))
			: iscsi.targets
	);

	const iscsiSorted = $derived.by(() => {
		return [...iscsiFiltered].sort((a, b) => {
			const cmp = a.iqn.localeCompare(b.iqn);
			return iscsi.sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={iscsi.search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if iscsi.showCreate}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Target</h3>
			<div class="mb-4">
				<Label for="iscsi-device">Block Subvolume</Label>
				<select id="iscsi-device" bind:value={iscsi.newDevice} onchange={iscsiOnDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a block subvolume...</option>
					{#each iscsi.blockSubvolumes as sv}
						<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
					{/each}
				</select>
				{#if iscsi.blockSubvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No attached block subvolumes found. Create a block subvolume and attach it first.</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="iscsi-name">Target Name</Label>
				<Input id="iscsi-name" bind:value={iscsi.newName} placeholder="dbserver" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{iscsi.newName || '...'}</span>
			</div>
			<Button onclick={iscsiCreate} disabled={!iscsi.newName || !iscsi.newDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if iscsi.loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if iscsi.targets.length === 0}
	<p class="text-muted-foreground">No targets configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="IQN" active={true} dir={iscsi.sortDir} onclick={iscsiToggleSort} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Summary</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each iscsiSorted as target}
				<tr class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors" onclick={() => iscsi.expanded[target.id] = !iscsi.expanded[target.id]}>
					<td class="p-3">
						<span class="font-mono text-sm font-semibold">{target.iqn}</span>
						{#if target.alias}<span class="ml-2 text-xs text-muted-foreground">({target.alias})</span>{/if}
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{target.luns.length} LUN{target.luns.length !== 1 ? 's' : ''}
						&middot; {target.portals.length} portal{target.portals.length !== 1 ? 's' : ''}
						&middot; {target.acls.length === 0 ? 'open (any initiator)' : `${target.acls.length} ACL${target.acls.length !== 1 ? 's' : ''}`}
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => iscsi.expanded[target.id] = !iscsi.expanded[target.id]}>
								{iscsi.expanded[target.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => iscsiRemove(target.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if iscsi.expanded[target.id]}
					<tr class="border-b border-border bg-secondary/20">
						<td colspan="3" class="px-4 py-4">
							<div class="space-y-4">
								<!-- Portals -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Portals</h4>
									{#if target.portals.length === 0}
										<p class="text-xs text-muted-foreground">None</p>
									{:else}
										<div class="flex flex-wrap gap-2">
											{#each target.portals as p}
												<span class="rounded bg-secondary px-2 py-0.5 font-mono text-xs">{p.ip}:{p.port}</span>
											{/each}
										</div>
									{/if}
								</div>

								<!-- LUNs -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">LUNs</h4>
									{#if target.luns.length === 0}
										<p class="text-xs text-muted-foreground">No LUNs</p>
									{:else}
										<div class="space-y-1">
											{#each target.luns as lun}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs font-semibold">LUN {lun.lun_id}</span>
														<span class="ml-2 text-muted-foreground">{lun.backstore_path}</span>
														<span class="ml-1 text-xs text-muted-foreground">({lun.backstore_type})</span>
													</div>
													<Button variant="destructive" size="xs" onclick={() => iscsiRemoveLun(target.id, lun.lun_id)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if iscsi.addLunTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Block Device or Subvolume</Label>
												<select bind:value={iscsi.addLunPath} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Select...</option>
													{#each iscsi.blockSubvolumes as sv}
														<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
											</div>
											<div class="mb-2">
												<Label class="text-xs">Type</Label>
												<select bind:value={iscsi.addLunType} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Auto-detect</option>
													<option value="block">Block</option>
													<option value="fileio">File I/O</option>
												</select>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={iscsiAddLun} disabled={!iscsi.addLunPath}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { iscsi.addLunTarget = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { iscsi.addLunTarget = target.id; }}>+ Add LUN</Button>
									{/if}
								</div>

								<!-- ACLs -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Access Control (ACLs)</h4>
									{#if target.acls.length === 0}
										<p class="text-xs text-muted-foreground">Open access — any initiator can connect. Add an ACL to restrict.</p>
									{:else}
										<div class="space-y-1">
											{#each target.acls as acl}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs">{acl.initiator_iqn}</span>
														{#if acl.userid}<span class="ml-2 text-xs text-muted-foreground">CHAP: {acl.userid}</span>{/if}
													</div>
													<Button variant="destructive" size="xs" onclick={() => iscsiRemoveAcl(target.id, acl.initiator_iqn)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if iscsi.addAclTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Initiator IQN</Label>
												<Input bind:value={iscsi.addAclIqn} placeholder="iqn.2024-01.com.client:initiator1" class="mt-1 h-8 text-xs" />
											</div>
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">CHAP User (optional)</Label>
													<Input bind:value={iscsi.addAclUser} class="mt-1 h-8 text-xs" />
												</div>
												<div>
													<Label class="text-xs">CHAP Password (optional)</Label>
													<Input bind:value={iscsi.addAclPass} type="password" class="mt-1 h-8 text-xs" />
												</div>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={iscsiAddAcl} disabled={!iscsi.addAclIqn}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { iscsi.addAclTarget = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { iscsi.addAclTarget = target.id; }}>+ Add ACL</Button>
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
