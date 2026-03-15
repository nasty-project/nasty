<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { IscsiTarget, Subvolume, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { ChevronsUpDown, ChevronUp, ChevronDown } from '@lucide/svelte';

	let targets: IscsiTarget[] = $state([]);
	let blockSubvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);
	let expanded: Record<string, boolean> = $state({});
	let protocol: ProtocolStatus | null = $state(null);

	// Create form
	let newName = $state('');
	let newDevice = $state('');

	// Add LUN form
	let addLunTarget = $state('');
	let addLunPath = $state('');
	let addLunType = $state('');

	// Add ACL form
	let addAclTarget = $state('');
	let addAclIqn = $state('');
	let addAclUser = $state('');
	let addAclPass = $state('');

	const client = getClient();

	$effect(() => {
		if (showCreate || addLunTarget) {
			loadSubvolumes();
		}
	});

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'share.iscsi') refresh();
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
			protocol = all.find(p => p.name === 'iscsi') ?? null;
		} catch { /* ignore */ }
	}

	async function refresh() {
		await withToast(async () => {
			targets = await client.call<IscsiTarget[]>('share.iscsi.list');
		});
	}

	async function loadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			blockSubvolumes = all.filter(s => s.subvolume_type === 'block' && s.block_device);
		});
	}

	function toggle(id: string) {
		expanded[id] = !expanded[id];
	}

	function onDeviceSelect() {
		if (newDevice && !newName) {
			const sv = blockSubvolumes.find(s => s.block_device === newDevice);
			if (sv) newName = sv.name;
		}
	}

	async function create() {
		if (!newName || !newDevice) return;
		const ok = await withToast(
			() => client.call('share.iscsi.create_quick', {
				name: newName,
				device_path: newDevice,
			}),
			'iSCSI target created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			newDevice = '';
			await refresh();
		}
	}

	async function remove(id: string) {
		if (!await confirm('Delete Target', 'Delete this target and all its LUNs?')) return;
		await withToast(
			() => client.call('share.iscsi.delete', { id }),
			'iSCSI target deleted'
		);
		await refresh();
	}

	async function addLun() {
		if (!addLunTarget || !addLunPath) return;
		const params: Record<string, unknown> = {
			target_id: addLunTarget,
			backstore_path: addLunPath,
		};
		if (addLunType) params.backstore_type = addLunType;
		await withToast(
			() => client.call('share.iscsi.add_lun', params),
			'LUN added'
		);
		addLunTarget = '';
		addLunPath = '';
		addLunType = '';
		await refresh();
	}

	async function removeLun(targetId: string, lunId: number) {
		if (!await confirm(`Remove LUN`, `Remove LUN ${lunId}?`)) return;
		await withToast(
			() => client.call('share.iscsi.remove_lun', { target_id: targetId, lun_id: lunId }),
			'LUN removed'
		);
		await refresh();
	}

	async function addAcl() {
		if (!addAclTarget || !addAclIqn) return;
		const params: Record<string, unknown> = {
			target_id: addAclTarget,
			initiator_iqn: addAclIqn,
		};
		if (addAclUser) params.userid = addAclUser;
		if (addAclPass) params.password = addAclPass;
		await withToast(
			() => client.call('share.iscsi.add_acl', params),
			'ACL added'
		);
		addAclTarget = '';
		addAclIqn = '';
		addAclUser = '';
		addAclPass = '';
		await refresh();
	}

	let search = $state('');
	let sortDir = $state<'asc' | 'desc' | null>(null);

	function toggleSort() {
		if (sortDir === null) sortDir = 'asc';
		else if (sortDir === 'asc') sortDir = 'desc';
		else sortDir = null;
	}

	const filtered = $derived(
		search.trim()
			? targets.filter(t =>
				t.iqn.toLowerCase().includes(search.toLowerCase()) ||
				t.alias?.toLowerCase().includes(search.toLowerCase()))
			: targets
	);

	const sorted = $derived.by(() => {
		if (!sortDir) return filtered;
		return [...filtered].sort((a, b) => {
			const cmp = a.iqn.localeCompare(b.iqn);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});

	async function removeAcl(targetId: string, initiatorIqn: string) {
		if (!await confirm(`Remove ACL`, `Remove ACL for ${initiatorIqn}?`)) return;
		await withToast(
			() => client.call('share.iscsi.remove_acl', { target_id: targetId, initiator_iqn: initiatorIqn }),
			'ACL removed'
		);
		await refresh();
	}
</script>


{#if protocol}
	<Card class="mb-4">
		<CardContent class="flex items-center gap-4 py-3">
			<Badge variant={protocol.running ? 'default' : 'destructive'}>
				{protocol.running ? 'Running' : 'Stopped'}
			</Badge>
			<span class="text-sm text-muted-foreground">
				{targets.length} target{targets.length !== 1 ? 's' : ''}
				&middot; Portal: <code class="rounded bg-secondary px-1.5 py-0.5 text-xs">{window.location.hostname}:3260</code>
			</span>
			{#if !protocol.enabled}
				<Badge variant="secondary">Disabled</Badge>
			{/if}
		</CardContent>
	</Card>
{/if}

<div class="mb-4 flex items-center gap-3">
	<Button size="sm" onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Target'}
	</Button>
	<Input bind:value={search} placeholder="Search..." class="h-9 w-48" />
	{#if targets.length > 1}
		<button class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground" onclick={toggleSort}>
			Sort by IQN
			{#if sortDir === 'asc'}<ChevronUp size={13} />{:else if sortDir === 'desc'}<ChevronDown size={13} />{:else}<ChevronsUpDown size={13} class="opacity-30" />{/if}
		</button>
	{/if}
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Target</h3>
			<div class="mb-4">
				<Label for="iscsi-device">Block Subvolume</Label>
				<select id="iscsi-device" bind:value={newDevice} onchange={onDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a block subvolume...</option>
					{#each blockSubvolumes as sv}
						<option value={sv.block_device}>{sv.pool}/{sv.name} ({sv.block_device})</option>
					{/each}
				</select>
				{#if blockSubvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No attached block subvolumes found. Create a block subvolume and attach it first.</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="iscsi-name">Target Name</Label>
				<Input id="iscsi-name" bind:value={newName} placeholder="dbserver" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{newName || '...'}</span>
			</div>
			<Button onclick={create} disabled={!newName || !newDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if targets.length === 0}
	<p class="text-muted-foreground">No targets configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">IQN</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Summary</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as target}
				<tr class="border-b border-border">
					<td class="p-3">
						<span class="font-mono text-sm font-semibold">{target.iqn}</span>
						{#if target.alias}<span class="ml-2 text-xs text-muted-foreground">({target.alias})</span>{/if}
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{target.luns.length} LUN{target.luns.length !== 1 ? 's' : ''}
						&middot; {target.portals.length} portal{target.portals.length !== 1 ? 's' : ''}
						&middot; {target.acls.length === 0 ? 'open (any initiator)' : `${target.acls.length} ACL${target.acls.length !== 1 ? 's' : ''}`}
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => toggle(target.id)}>
								{expanded[target.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => remove(target.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if expanded[target.id]}
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
												<div class="flex items-center justify-between rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs font-semibold">LUN {lun.lun_id}</span>
														<span class="ml-2 text-muted-foreground">{lun.backstore_path}</span>
														<span class="ml-1 text-xs text-muted-foreground">({lun.backstore_type})</span>
													</div>
													<Button variant="ghost" size="xs" class="text-destructive hover:text-destructive" onclick={() => removeLun(target.id, lun.lun_id)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if addLunTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Block Device or Subvolume</Label>
												<select bind:value={addLunPath} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Select...</option>
													{#each blockSubvolumes as sv}
														<option value={sv.block_device}>{sv.pool}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
											</div>
											<div class="mb-2">
												<Label class="text-xs">Type</Label>
												<select bind:value={addLunType} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Auto-detect</option>
													<option value="block">Block</option>
													<option value="fileio">File I/O</option>
												</select>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={addLun} disabled={!addLunPath}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { addLunTarget = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { addLunTarget = target.id; }}>+ Add LUN</Button>
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
												<div class="flex items-center justify-between rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs">{acl.initiator_iqn}</span>
														{#if acl.userid}<span class="ml-2 text-xs text-muted-foreground">CHAP: {acl.userid}</span>{/if}
													</div>
													<Button variant="ghost" size="xs" class="text-destructive hover:text-destructive" onclick={() => removeAcl(target.id, acl.initiator_iqn)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if addAclTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Initiator IQN</Label>
												<Input bind:value={addAclIqn} placeholder="iqn.2024-01.com.client:initiator1" class="mt-1 h-8 text-xs" />
											</div>
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">CHAP User (optional)</Label>
													<Input bind:value={addAclUser} class="mt-1 h-8 text-xs" />
												</div>
												<div>
													<Label class="text-xs">CHAP Password (optional)</Label>
													<Input bind:value={addAclPass} type="password" class="mt-1 h-8 text-xs" />
												</div>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={addAcl} disabled={!addAclIqn}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { addAclTarget = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { addAclTarget = target.id; }}>+ Add ACL</Button>
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
