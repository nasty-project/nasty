<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import { validateAddressForFamily } from '$lib/network';
	import ListenAddressPicker from '$lib/components/ListenAddressPicker.svelte';
	import { rdma } from '$lib/sharing/rdma.svelte';
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
		iscsiAddPortal,
		iscsiReplacePortal,
		iscsiRemovePortal,
		iscsiLoadSubvolumes,
	} from '$lib/sharing/iscsi.svelte';

	$effect(() => { if (iscsi.showCreate || iscsi.addLunTarget) iscsiLoadSubvolumes(); });

	// Per-form "tried" flags — defer amber required-field decoration
	// until each submit button is clicked at least once.
	let createTried = $state(false);
	let addLunTried = $state(false);
	let addAclTried = $state(false);
	let addPortalTried = $state(false);

	async function iscsiCreateGuarded() {
		if (!iscsi.newName || !iscsi.newDevice) { createTried = true; return; }
		createTried = false;
		await iscsiCreate();
	}
	async function iscsiAddLunGuarded() {
		if (!iscsi.addLunPath) { addLunTried = true; return; }
		addLunTried = false;
		await iscsiAddLun();
	}
	async function iscsiAddAclGuarded() {
		if (!iscsi.addAclIqn) { addAclTried = true; return; }
		addAclTried = false;
		await iscsiAddAcl();
	}

	const addPortalIpError = $derived(
		validateAddressForFamily(iscsi.addPortalFamily, iscsi.addPortalIp),
	);
	async function iscsiAddPortalGuarded() {
		if (!iscsi.addPortalIp || addPortalIpError) { addPortalTried = true; return; }
		addPortalTried = false;
		await iscsiAddPortal();
	}
	async function iscsiReplacePortalGuarded() {
		if (!iscsi.addPortalIp || addPortalIpError) { addPortalTried = true; return; }
		addPortalTried = false;
		await iscsiReplacePortal();
	}
	function startEditPortal(targetId: string, p: { ip: string; port: number; iser?: boolean }) {
		iscsi.addPortalTarget = '';
		iscsi.editPortalTarget = targetId;
		iscsi.editPortalOrigIp = p.ip;
		iscsi.editPortalOrigPort = p.port;
		iscsi.addPortalIp = p.ip;
		iscsi.addPortalPort = p.port;
		iscsi.addPortalIser = p.iser ?? false;
		iscsi.addPortalFamily = p.ip.includes(':') ? 'ipv6' : 'ipv4';
		addPortalTried = false;
	}
	function cancelPortalForm() {
		iscsi.addPortalTarget = '';
		iscsi.editPortalTarget = '';
		addPortalTried = false;
	}

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
				<Label for="iscsi-device">Block Subvolume {#if !iscsi.newDevice && createTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
				<select id="iscsi-device" bind:value={iscsi.newDevice} onchange={iscsiOnDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm {requiredFieldCls(!iscsi.newDevice, createTried)}">
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
				<Label for="iscsi-name">Target Name {#if !iscsi.newName && createTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
				<Input id="iscsi-name" bind:value={iscsi.newName} placeholder="dbserver" class="mt-1 {requiredFieldCls(!iscsi.newName, createTried)}" />
				<span class="mt-1 block text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{iscsi.newName || '...'}</span>
			</div>
			<Button onclick={iscsiCreateGuarded}>Create</Button>
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
										<div class="space-y-1">
											{#each target.portals as p}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs">{p.ip.includes(':') && p.ip !== '0.0.0.0' ? `[${p.ip}]` : p.ip}:{p.port}</span>
													{#if p.iser}<Badge variant="secondary" class="text-[0.6rem]">iSER</Badge>{/if}
													<Button variant="outline" size="xs" onclick={() => startEditPortal(target.id, p)}>Edit</Button>
													{#if target.portals.length > 1}
														<Button variant="destructive" size="xs" onclick={() => iscsiRemovePortal(target.id, p.ip, p.port)}>Remove</Button>
													{/if}
												</div>
											{/each}
										</div>
									{/if}
									{#if iscsi.addPortalTarget === target.id || iscsi.editPortalTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<ListenAddressPicker
												bind:address={iscsi.addPortalIp}
												bind:family={iscsi.addPortalFamily}
												allowWildcards
												error={addPortalTried ? addPortalIpError : null}
												placeholderV4="0.0.0.0 or 192.168.1.10"
												placeholderV6=":: or fd00::1"
											/>
											<label class="mt-3 flex items-center gap-2 text-xs {rdma.status?.enabled ? '' : 'opacity-50'}">
												<input type="checkbox" bind:checked={iscsi.addPortalIser} disabled={!rdma.status?.enabled} class="h-3.5 w-3.5" />
												iSER (iSCSI over RDMA)
												{#if !rdma.status?.enabled}
													<span class="text-muted-foreground">— {rdma.status?.capable ? 'enable RDMA in the transports card above' : 'requires an RDMA-capable NIC'}</span>
												{/if}
											</label>
											<div class="mt-3 flex items-end gap-2">
												<div>
													<Label class="text-xs">Port</Label>
													<Input type="number" bind:value={iscsi.addPortalPort} class="mt-1 h-8 w-24 text-xs" />
												</div>
												{#if iscsi.editPortalTarget === target.id}
													<Button size="xs" onclick={iscsiReplacePortalGuarded}>Save</Button>
												{:else}
													<Button size="xs" onclick={iscsiAddPortalGuarded}>Add</Button>
												{/if}
												<Button size="xs" variant="ghost" onclick={cancelPortalForm}>Cancel</Button>
											</div>
											{#if !iscsi.addPortalIp && addPortalTried}
												<p class="mt-1 text-[0.7rem] text-amber-500">Listen address is required.</p>
											{/if}
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { iscsi.editPortalTarget = ''; iscsi.addPortalTarget = target.id; iscsi.addPortalIp = ''; iscsi.addPortalPort = 3260; iscsi.addPortalFamily = 'ipv4'; iscsi.addPortalIser = false; }}>+ Add Portal</Button>
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
												<Label class="text-xs">Block Device or Subvolume {#if !iscsi.addLunPath && addLunTried}<span class="text-amber-500">required</span>{/if}</Label>
												<select bind:value={iscsi.addLunPath} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs {requiredFieldCls(!iscsi.addLunPath, addLunTried)}">
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
												<Button size="xs" onclick={iscsiAddLunGuarded}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { iscsi.addLunTarget = ''; addLunTried = false; }}>Cancel</Button>
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
												<Label class="text-xs">Initiator IQN {#if !iscsi.addAclIqn && addAclTried}<span class="text-amber-500">required</span>{/if}</Label>
												<Input bind:value={iscsi.addAclIqn} placeholder="iqn.2024-01.com.client:initiator1" class="mt-1 h-8 text-xs {requiredFieldCls(!iscsi.addAclIqn, addAclTried)}" />
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
												<Button size="xs" onclick={iscsiAddAclGuarded}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { iscsi.addAclTarget = ''; addAclTried = false; }}>Cancel</Button>
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
