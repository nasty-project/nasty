<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import {
		nvme,
		nvmeToggleSort,
		nvmeOnDeviceSelect,
		nvmeCreate,
		nvmeRemove,
		nvmeAddNamespace,
		nvmeRemoveNamespace,
		nvmeAddPort,
		nvmeRemovePort,
		nvmeAddHost,
		nvmeRemoveHost,
		nvmeLoadSubvolumes,
	} from '$lib/sharing/nvmeof.svelte';

	$effect(() => { if (nvme.showCreate || nvme.addNsSubsys) nvmeLoadSubvolumes(); });

	const nvmeFiltered = $derived(
		nvme.search.trim()
			? nvme.subsystems.filter(s => s.nqn.toLowerCase().includes(nvme.search.toLowerCase()))
			: nvme.subsystems
	);

	const nvmeSorted = $derived.by(() => {
		return [...nvmeFiltered].sort((a, b) => {
			const cmp = a.nqn.localeCompare(b.nqn);
			return nvme.sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={nvme.search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if nvme.showCreate}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Share</h3>
			<div class="mb-4">
				<Label for="nvme-device">Block Subvolume</Label>
				<select id="nvme-device" bind:value={nvme.newDevice} onchange={nvmeOnDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a block subvolume...</option>
					{#each nvme.blockSubvolumes as sv}
						<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
					{/each}
				</select>
				{#if nvme.blockSubvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No attached block subvolumes found. Create a block subvolume and attach it first.</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="nvme-name">Share Name</Label>
				<Input id="nvme-name" bind:value={nvme.newName} placeholder="faststore" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">NQN: nqn.2137.com.nasty:{nvme.newName || '...'}</span>
			</div>
			<div class="grid grid-cols-2 gap-4 mb-4">
				<div>
					<Label for="nvme-addr">Listen Address</Label>
					<Input id="nvme-addr" bind:value={nvme.newAddr} class="mt-1" />
				</div>
				<div>
					<Label for="nvme-port">Port</Label>
					<Input id="nvme-port" type="number" bind:value={nvme.newPort} class="mt-1" />
				</div>
			</div>
			<Button onclick={nvmeCreate} disabled={!nvme.newName || !nvme.newDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if nvme.loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if nvme.subsystems.length === 0}
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="NQN" active={true} dir={nvme.sortDir} onclick={nvmeToggleSort} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Summary</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each nvmeSorted as subsys}
				<tr class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors" onclick={() => nvme.expanded[subsys.id] = !nvme.expanded[subsys.id]}>
					<td class="p-3">
						<span class="font-mono text-sm font-semibold">{subsys.nqn}</span>
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{subsys.namespaces.length} namespace{subsys.namespaces.length !== 1 ? 's' : ''}
						&middot; {subsys.ports.length} port{subsys.ports.length !== 1 ? 's' : ''}
						&middot; {subsys.allow_any_host ? 'any host' : `${subsys.allowed_hosts.length} allowed host${subsys.allowed_hosts.length !== 1 ? 's' : ''}`}
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => nvme.expanded[subsys.id] = !nvme.expanded[subsys.id]}>
								{nvme.expanded[subsys.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => nvmeRemove(subsys.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if nvme.expanded[subsys.id]}
					<tr class="border-b border-border bg-secondary/20">
						<td colspan="3" class="px-4 py-4">
							<div class="space-y-4">
								<!-- Namespaces -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Namespaces</h4>
									{#if subsys.namespaces.length === 0}
										<p class="text-xs text-muted-foreground">No namespaces</p>
									{:else}
										<div class="space-y-1">
											{#each subsys.namespaces as ns}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs font-semibold">NSID {ns.nsid}</span>
														<span class="ml-2 text-muted-foreground">{ns.device_path}</span>
														<Badge variant={ns.enabled ? 'default' : 'secondary'} class="ml-2 text-[0.6rem]">{ns.enabled ? 'Active' : 'Off'}</Badge>
													</div>
													<Button variant="destructive" size="xs" onclick={() => nvmeRemoveNamespace(subsys.id, ns.nsid)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if nvme.addNsSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Block Device</Label>
												<select bind:value={nvme.addNsDevice} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Select...</option>
													{#each nvme.blockSubvolumes as sv}
														<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddNamespace} disabled={!nvme.addNsDevice}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvme.addNsSubsys = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { nvme.addNsSubsys = subsys.id; }}>+ Add Namespace</Button>
									{/if}
								</div>

								<!-- Ports -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Ports</h4>
									{#if subsys.ports.length === 0}
										<p class="text-xs text-muted-foreground">Not listening (no ports configured)</p>
									{:else}
										<div class="flex flex-wrap gap-2">
											{#each subsys.ports as port}
												<div class="flex items-center gap-2 rounded bg-secondary/50 px-2 py-1">
													<span class="font-mono text-xs">{port.transport.toUpperCase()} {port.addr}:{port.service_id}</span>
													<Button variant="destructive" size="xs" class="h-5 text-xs" onclick={() => nvmeRemovePort(subsys.id, port.port_id)}>×</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if nvme.addPortSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">Transport</Label>
													<select bind:value={nvme.addPortTransport} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
														<option value="tcp">TCP</option>
														<option value="rdma">RDMA</option>
													</select>
												</div>
												<div>
													<Label class="text-xs">Address Family</Label>
													<select bind:value={nvme.addPortFamily} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
														<option value="ipv4">IPv4</option>
														<option value="ipv6">IPv6</option>
													</select>
												</div>
											</div>
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">Listen Address</Label>
													<Input bind:value={nvme.addPortAddr} class="mt-1 h-8 text-xs" />
												</div>
												<div>
													<Label class="text-xs">Port</Label>
													<Input type="number" bind:value={nvme.addPortSvcId} class="mt-1 h-8 text-xs" />
												</div>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddPort}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvme.addPortSubsys = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { nvme.addPortSubsys = subsys.id; }}>+ Add Port</Button>
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
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs">{hostNqn}</span>
													<Button variant="destructive" size="xs" onclick={() => nvmeRemoveHost(subsys.id, hostNqn)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if nvme.addHostSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Host NQN</Label>
												<Input bind:value={nvme.addHostNqn} placeholder="nqn.2024-01.com.client:host1" class="mt-1 h-8 text-xs" />
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddHost} disabled={!nvme.addHostNqn}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvme.addHostSubsys = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { nvme.addHostSubsys = subsys.id; }}>+ Add Host</Button>
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
