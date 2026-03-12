<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { NvmeofSubsystem, Subvolume } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';

	let subsystems: NvmeofSubsystem[] = $state([]);
	let blockSubvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newName = $state('');
	let newDevice = $state('');
	let newAddr = $state('0.0.0.0');
	let newPort = $state(4420);

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
			subsystems = await client.call<NvmeofSubsystem[]>('share.nvmeof.list');
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
			() => client.call('share.nvmeof.create_quick', {
				name: newName,
				device_path: newDevice,
				addr: newAddr,
				port: newPort,
			}),
			'NVMe-oF share created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			newDevice = '';
			newAddr = '0.0.0.0';
			newPort = 4420;
			await refresh();
		}
	}

	async function remove(id: string) {
		if (!confirm('Delete this NVMe-oF share?')) return;
		await withToast(
			() => client.call('share.nvmeof.delete', { id }),
			'NVMe-oF share deleted'
		);
		await refresh();
	}
</script>

<h1 class="mb-4 text-2xl font-bold">NVMe-oF Shares</h1>

<div class="mb-4">
	<Button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Share'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New NVMe-oF Share</h3>
			<div class="mb-4">
				<Label for="nvme-device">Block Subvolume</Label>
				<select id="nvme-device" bind:value={newDevice} onchange={onDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
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
				<Label for="nvme-name">Share Name</Label>
				<Input id="nvme-name" bind:value={newName} placeholder="faststore" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">NQN: nqn.2024-01.com.nasty:{newName || '...'}</span>
			</div>
			<div class="grid grid-cols-2 gap-4 mb-4">
				<div>
					<Label for="nvme-addr">Listen Address</Label>
					<Input id="nvme-addr" bind:value={newAddr} class="mt-1" />
				</div>
				<div>
					<Label for="nvme-port">Port</Label>
					<Input id="nvme-port" type="number" bind:value={newPort} class="mt-1" />
				</div>
			</div>
			<Button onclick={create} disabled={!newName || !newDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if subsystems.length === 0}
	<p class="text-muted-foreground">No NVMe-oF shares configured.</p>
{:else}
	{#each subsystems as subsys}
		<Card class="mb-4">
			<CardContent class="pt-5">
				<div class="mb-3 flex items-start justify-between">
					<div>
						<strong class="font-mono text-sm">{subsys.nqn}</strong>
						<div class="text-xs text-muted-foreground">
							{subsys.allow_any_host ? 'Any host allowed' : `${subsys.allowed_hosts.length} allowed host(s)`}
						</div>
					</div>
					<Button variant="destructive" size="sm" onclick={() => remove(subsys.id)}>Delete</Button>
				</div>

				{#if subsys.namespaces.length > 0}
					<div class="mb-2">
						<span class="text-xs uppercase tracking-wide text-muted-foreground">Devices: </span>
						{#each subsys.namespaces as ns}
							<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">
								{ns.device_path}
								<Badge variant={ns.enabled ? 'default' : 'secondary'} class="ml-1 text-[0.6rem]">
									{ns.enabled ? 'Active' : 'Off'}
								</Badge>
							</span>
						{/each}
					</div>
				{/if}

				{#if subsys.ports.length > 0}
					<div>
						<span class="text-xs uppercase tracking-wide text-muted-foreground">Listening: </span>
						{#each subsys.ports as port}
							<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">
								{port.transport.toUpperCase()} {port.addr}:{port.service_id}
							</span>
						{/each}
					</div>
				{:else}
					<span class="text-xs text-muted-foreground">Not listening (no ports)</span>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}
