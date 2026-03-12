<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { IscsiTarget, Subvolume } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';

	let targets: IscsiTarget[] = $state([]);
	let blockSubvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newName = $state('');
	let newDevice = $state('');

	const client = getClient();

	$effect(() => {
		if (showCreate) {
			loadSubvolumes();
		}
	});

	onMount(async () => {
		await refresh();
		loading = false;
	});

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
		if (!confirm('Delete this iSCSI target and all its LUNs?')) return;
		await withToast(
			() => client.call('share.iscsi.delete', { id }),
			'iSCSI target deleted'
		);
		await refresh();
	}
</script>

<h1 class="mb-4 text-2xl font-bold">iSCSI Targets</h1>

<div class="mb-4">
	<Button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Target'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New iSCSI Target</h3>
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
				<span class="mt-1 block text-xs text-muted-foreground">IQN: iqn.2024-01.com.nasty:{newName || '...'}</span>
			</div>
			<Button onclick={create} disabled={!newName || !newDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if targets.length === 0}
	<p class="text-muted-foreground">No iSCSI targets configured.</p>
{:else}
	{#each targets as target}
		<Card class="mb-4">
			<CardContent class="pt-5">
				<div class="mb-3 flex items-start justify-between">
					<div>
						<strong class="font-mono text-sm">{target.iqn}</strong>
						{#if target.alias}<span class="text-muted-foreground"> ({target.alias})</span>{/if}
					</div>
					<Button variant="destructive" size="sm" onclick={() => remove(target.id)}>Delete</Button>
				</div>

				<div class="mb-2">
					<span class="text-xs uppercase tracking-wide text-muted-foreground">Portals: </span>
					{#each target.portals as p}
						<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">{p.ip}:{p.port}</span>
					{/each}
					{#if target.portals.length === 0}
						<span class="text-xs text-muted-foreground">None</span>
					{/if}
				</div>

				{#if target.luns.length > 0}
					<div class="mb-2">
						<span class="text-xs uppercase tracking-wide text-muted-foreground">LUNs: </span>
						{#each target.luns as lun}
							<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">
								LUN {lun.lun_id}: {lun.backstore_path} ({lun.backstore_type})
							</span>
						{/each}
					</div>
				{/if}

				{#if target.acls.length > 0}
					<div>
						<span class="text-xs uppercase tracking-wide text-muted-foreground">ACLs: </span>
						{#each target.acls as acl}
							<span class="mr-1 font-mono text-xs">{acl.initiator_iqn}</span>
						{/each}
					</div>
				{:else}
					<span class="text-xs text-muted-foreground">Open (any initiator)</span>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}
