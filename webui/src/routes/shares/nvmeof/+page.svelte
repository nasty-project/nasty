<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { NvmeofSubsystem, Subvolume, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { ChevronsUpDown, ChevronUp, ChevronDown } from '@lucide/svelte';

	let subsystems: NvmeofSubsystem[] = $state([]);
	let blockSubvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);
	let expanded: Record<string, boolean> = $state({});
	let protocol: ProtocolStatus | null = $state(null);

	// Create form
	let newName = $state('');
	let newDevice = $state('');
	let newAddr = $state('0.0.0.0');
	let newPort = $state(4420);

	// Add Namespace form
	let addNsSubsys = $state('');
	let addNsDevice = $state('');

	// Add Port form
	let addPortSubsys = $state('');
	let addPortTransport = $state('tcp');
	let addPortAddr = $state('0.0.0.0');
	let addPortSvcId = $state(4420);
	let addPortFamily = $state('ipv4');

	// Add Host form
	let addHostSubsys = $state('');
	let addHostNqn = $state('');

	const client = getClient();

	$effect(() => {
		if (showCreate || addNsSubsys) {
			loadSubvolumes();
		}
	});

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'share.nvmeof') refresh();
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
			protocol = all.find(p => p.name === 'nvmeof') ?? null;
		} catch { /* ignore */ }
	}

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
		if (!await confirm('Delete Share', 'Delete this share?')) return;
		await withToast(
			() => client.call('share.nvmeof.delete', { id }),
			'NVMe-oF share deleted'
		);
		await refresh();
	}

	// Namespace management
	async function addNamespace() {
		if (!addNsSubsys || !addNsDevice) return;
		await withToast(
			() => client.call('share.nvmeof.add_namespace', {
				subsystem_id: addNsSubsys,
				device_path: addNsDevice,
			}),
			'Namespace added'
		);
		addNsSubsys = '';
		addNsDevice = '';
		await refresh();
	}

	async function removeNamespace(subsystemId: string, nsid: number) {
		if (!await confirm(`Remove Namespace`, `Remove namespace ${nsid}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_namespace', { subsystem_id: subsystemId, nsid }),
			'Namespace removed'
		);
		await refresh();
	}

	// Port management
	async function addPort() {
		if (!addPortSubsys) return;
		await withToast(
			() => client.call('share.nvmeof.add_port', {
				subsystem_id: addPortSubsys,
				transport: addPortTransport,
				addr: addPortAddr,
				service_id: addPortSvcId,
				addr_family: addPortFamily,
			}),
			'Port added'
		);
		addPortSubsys = '';
		addPortTransport = 'tcp';
		addPortAddr = '0.0.0.0';
		addPortSvcId = 4420;
		addPortFamily = 'ipv4';
		await refresh();
	}

	async function removePort(subsystemId: string, portId: number) {
		if (!await confirm(`Remove Port`, `Remove port ${portId}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_port', { subsystem_id: subsystemId, port_id: portId }),
			'Port removed'
		);
		await refresh();
	}

	// Host ACL management
	async function addHost() {
		if (!addHostSubsys || !addHostNqn) return;
		await withToast(
			() => client.call('share.nvmeof.add_host', {
				subsystem_id: addHostSubsys,
				host_nqn: addHostNqn,
			}),
			'Allowed host added'
		);
		addHostSubsys = '';
		addHostNqn = '';
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
			? subsystems.filter(s => s.nqn.toLowerCase().includes(search.toLowerCase()))
			: subsystems
	);

	const sorted = $derived.by(() => {
		if (!sortDir) return filtered;
		return [...filtered].sort((a, b) => {
			const cmp = a.nqn.localeCompare(b.nqn);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});

	async function removeHost(subsystemId: string, hostNqn: string) {
		if (!await confirm(`Remove Host`, `Remove access for ${hostNqn}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_host', { subsystem_id: subsystemId, host_nqn: hostNqn }),
			'Allowed host removed'
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
				{subsystems.length} subsystem{subsystems.length !== 1 ? 's' : ''}
				&middot; Connect with: <code class="rounded bg-secondary px-1.5 py-0.5 text-xs">nvme connect -t tcp -a {window.location.hostname} -s 4420 -n &lt;nqn&gt;</code>
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
	{#if subsystems.length > 1}
		<button class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground" onclick={toggleSort}>
			Sort by NQN
			{#if sortDir === 'asc'}<ChevronUp size={13} />{:else if sortDir === 'desc'}<ChevronDown size={13} />{:else}<ChevronsUpDown size={13} class="opacity-30" />{/if}
		</button>
	{/if}
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Share</h3>
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
				<span class="mt-1 block text-xs text-muted-foreground">NQN: nqn.2137.com.nasty:{newName || '...'}</span>
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
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	{#each sorted as subsys}
		<Card class="mb-4">
			<CardContent class="pt-5">
				<!-- Header row (always visible) -->
				<button class="mb-1 flex w-full items-start justify-between text-left" onclick={() => toggle(subsys.id)}>
					<div>
						<strong class="font-mono text-sm">{subsys.nqn}</strong>
						<div class="mt-1 text-xs text-muted-foreground">
							{subsys.namespaces.length} namespace{subsys.namespaces.length !== 1 ? 's' : ''}
							&middot; {subsys.ports.length} port{subsys.ports.length !== 1 ? 's' : ''}
							&middot; {subsys.allow_any_host ? 'any host' : `${subsys.allowed_hosts.length} allowed host${subsys.allowed_hosts.length !== 1 ? 's' : ''}`}
						</div>
					</div>
					<span class="ml-4 mt-1 text-muted-foreground">{expanded[subsys.id] ? '▲' : '▼'}</span>
				</button>

				<!-- Expanded details -->
				{#if expanded[subsys.id]}
					<div class="mt-4 space-y-4 border-t pt-4">
						<!-- Namespaces -->
						<div>
							<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Namespaces</h4>
							{#if subsys.namespaces.length === 0}
								<p class="text-xs text-muted-foreground">No namespaces</p>
							{:else}
								<div class="space-y-1">
									{#each subsys.namespaces as ns}
										<div class="flex items-center justify-between rounded bg-secondary/50 px-2 py-1.5">
											<div class="text-sm">
												<span class="font-mono text-xs font-semibold">NSID {ns.nsid}</span>
												<span class="ml-2 text-muted-foreground">{ns.device_path}</span>
												<Badge variant={ns.enabled ? 'default' : 'secondary'} class="ml-2 text-[0.6rem]">
													{ns.enabled ? 'Active' : 'Off'}
												</Badge>
											</div>
											<Button variant="ghost" size="xs" class="h-7 text-xs text-destructive hover:text-destructive" onclick={() => removeNamespace(subsys.id, ns.nsid)}>Remove</Button>
										</div>
									{/each}
								</div>
							{/if}

							{#if addNsSubsys === subsys.id}
								<div class="mt-3 rounded border p-3">
									<div class="mb-2">
										<Label class="text-xs">Block Device</Label>
										<select bind:value={addNsDevice} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
											<option value="">Select...</option>
											{#each blockSubvolumes as sv}
												<option value={sv.block_device}>{sv.pool}/{sv.name} ({sv.block_device})</option>
											{/each}
										</select>
									</div>
									<div class="flex gap-2">
										<Button size="xs" class="h-7 text-xs" onclick={addNamespace} disabled={!addNsDevice}>Add</Button>
										<Button size="xs" variant="ghost" class="h-7 text-xs" onclick={() => { addNsSubsys = ''; }}>Cancel</Button>
									</div>
								</div>
							{:else}
								<Button size="xs" variant="outline" class="mt-2 h-7 text-xs" onclick={() => { addNsSubsys = subsys.id; }}>+ Add Namespace</Button>
							{/if}
						</div>

						<!-- Ports -->
						<div>
							<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Ports</h4>
							{#if subsys.ports.length === 0}
								<p class="text-xs text-muted-foreground">Not listening (no ports configured)</p>
							{:else}
								<div class="space-y-1">
									{#each subsys.ports as port}
										<div class="flex items-center justify-between rounded bg-secondary/50 px-2 py-1.5">
											<div class="text-sm">
												<span class="font-mono text-xs font-semibold">Port {port.port_id}</span>
												<span class="ml-2">{port.transport.toUpperCase()} {port.addr}:{port.service_id}</span>
												<span class="ml-1 text-xs text-muted-foreground">({port.addr_family})</span>
											</div>
											<Button variant="ghost" size="xs" class="h-7 text-xs text-destructive hover:text-destructive" onclick={() => removePort(subsys.id, port.port_id)}>Remove</Button>
										</div>
									{/each}
								</div>
							{/if}

							{#if addPortSubsys === subsys.id}
								<div class="mt-3 rounded border p-3">
									<div class="grid grid-cols-2 gap-2 mb-2">
										<div>
											<Label class="text-xs">Transport</Label>
											<select bind:value={addPortTransport} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
												<option value="tcp">TCP</option>
												<option value="rdma">RDMA</option>
											</select>
										</div>
										<div>
											<Label class="text-xs">Address Family</Label>
											<select bind:value={addPortFamily} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
												<option value="ipv4">IPv4</option>
												<option value="ipv6">IPv6</option>
											</select>
										</div>
									</div>
									<div class="grid grid-cols-2 gap-2 mb-2">
										<div>
											<Label class="text-xs">Listen Address</Label>
											<Input bind:value={addPortAddr} class="mt-1 h-8 text-xs" />
										</div>
										<div>
											<Label class="text-xs">Port</Label>
											<Input type="number" bind:value={addPortSvcId} class="mt-1 h-8 text-xs" />
										</div>
									</div>
									<div class="flex gap-2">
										<Button size="xs" class="h-7 text-xs" onclick={addPort}>Add</Button>
										<Button size="xs" variant="ghost" class="h-7 text-xs" onclick={() => { addPortSubsys = ''; }}>Cancel</Button>
									</div>
								</div>
							{:else}
								<Button size="xs" variant="outline" class="mt-2 h-7 text-xs" onclick={() => { addPortSubsys = subsys.id; }}>+ Add Port</Button>
							{/if}
						</div>

						<!-- Allowed Hosts -->
						<div>
							<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Allowed Hosts</h4>
							{#if subsys.allow_any_host && subsys.allowed_hosts.length === 0}
								<p class="text-xs text-muted-foreground">Any host can connect. Add a host NQN to restrict access.</p>
							{:else}
								<div class="space-y-1">
									{#each subsys.allowed_hosts as hostNqn}
										<div class="flex items-center justify-between rounded bg-secondary/50 px-2 py-1.5">
											<span class="font-mono text-xs">{hostNqn}</span>
											<Button variant="ghost" size="xs" class="h-7 text-xs text-destructive hover:text-destructive" onclick={() => removeHost(subsys.id, hostNqn)}>Remove</Button>
										</div>
									{/each}
								</div>
							{/if}

							{#if addHostSubsys === subsys.id}
								<div class="mt-3 rounded border p-3">
									<div class="mb-2">
										<Label class="text-xs">Host NQN</Label>
										<Input bind:value={addHostNqn} placeholder="nqn.2024-01.com.client:host1" class="mt-1 h-8 text-xs" />
									</div>
									<div class="flex gap-2">
										<Button size="xs" class="h-7 text-xs" onclick={addHost} disabled={!addHostNqn}>Add</Button>
										<Button size="xs" variant="ghost" class="h-7 text-xs" onclick={() => { addHostSubsys = ''; }}>Cancel</Button>
									</div>
								</div>
							{:else}
								<Button size="xs" variant="outline" class="mt-2 h-7 text-xs" onclick={() => { addHostSubsys = subsys.id; }}>+ Add Host</Button>
							{/if}
						</div>

						<!-- Delete -->
						<div class="border-t pt-3">
							<Button variant="destructive" size="xs" onclick={() => remove(subsys.id)}>Delete Share</Button>
						</div>
					</div>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}
