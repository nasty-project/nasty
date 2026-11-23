<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import SortTh from '$lib/components/SortTh.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import { validateAddressForFamily } from '$lib/network';
	import ListenAddressPicker from '$lib/components/ListenAddressPicker.svelte';
	import { rdma } from '$lib/sharing/rdma.svelte';
	import {
		nvme,
		nvmeToggleSort,
		nvmeRemove,
		nvmeAddNamespace,
		nvmeRemoveNamespace,
		nvmeRepairNamespace,
		nvmeAddPort,
		nvmeRemovePort,
		nvmeAddHost,
		nvmeRemoveHost,
		nvmeLoadSubvolumes,
	} from '$lib/sharing/nvmeof.svelte';

	let { isAdmin = false }: { isAdmin?: boolean } = $props();

	$effect(() => {
		if (nvme.addNsSubsys || nvme.repairNsSubsys) nvmeLoadSubvolumes();
	});

	// Per-form "tried" flags — defer amber required-field decoration
	// until each submit button is clicked at least once.
	let addNsTried = $state(false);
	let addHostTried = $state(false);

	async function nvmeAddNamespaceGuarded() {
		if (!nvme.addNsDevice) { addNsTried = true; return; }
		addNsTried = false;
		await nvmeAddNamespace();
	}
	function startRepairNamespace(subsystemId: string, nsid: number) {
		nvme.repairNsSubsys = subsystemId;
		nvme.repairNsId = nsid;
		nvme.repairNsDevice = '';
	}
	async function nvmeAddHostGuarded() {
		if (!nvme.addHostNqn) { addHostTried = true; return; }
		addHostTried = false;
		await nvmeAddHost();
	}

	// Per-port-add cross-validation: family selector + address text
	// input were independent before — operator could pick `ipv6` and
	// type a v4 address (or vice versa), the engine would reject with
	// a generic configfs EINVAL. Now we preflight with the same
	// per-family check as the rest of the network forms.
	let addPortTried = $state(false);
	const addPortAddrError = $derived(
		validateAddressForFamily(
			nvme.addPortFamily === 'ipv6' ? 'ipv6' : 'ipv4',
			nvme.addPortAddr,
		),
	);
	async function nvmeAddPortGuarded() {
		if (!nvme.addPortAddr || addPortAddrError) { addPortTried = true; return; }
		addPortTried = false;
		await nvmeAddPort();
	}

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
						{#if subsys.namespaces.some(ns => ns.backing_volume_unresolved)}<Badge variant="secondary" class="ml-2 border-amber-500/50 text-amber-500">Repair required</Badge>{/if}
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
												<div class="rounded bg-secondary/50 px-2 py-1.5">
													<div class="flex items-center gap-3">
													<div class="text-sm">
														<span class="font-mono text-xs font-semibold">NSID {ns.nsid}</span>
														<span class="ml-2 text-muted-foreground">{ns.device_path}</span>
														<Badge variant={ns.enabled ? 'default' : 'secondary'} class="ml-2 text-[0.6rem]">{ns.enabled ? 'Active' : 'Off'}</Badge>
													</div>
													{#if ns.backing_volume_unresolved}
														<Badge variant="secondary" class="border-amber-500/50 text-amber-500">Backing unresolved</Badge>
														{#if isAdmin}<Button variant="outline" size="xs" onclick={() => startRepairNamespace(subsys.id, ns.nsid)}>Reconnect</Button>{/if}
													{/if}
													<Button variant="destructive" size="xs" onclick={() => nvmeRemoveNamespace(subsys.id, ns.nsid)}>Remove</Button>
													</div>
													{#if ns.backing_volume_unresolved && !isAdmin}
														<p class="mt-2 text-xs text-amber-500">An unscoped Admin must reconnect this namespace to its original block subvolume.</p>
													{:else if nvme.repairNsSubsys === subsys.id && nvme.repairNsId === ns.nsid}
														<div class="mt-2 flex items-center gap-2 border-t border-border pt-2">
															<select bind:value={nvme.repairNsDevice} class="h-8 min-w-72 rounded-md border border-input bg-background px-2 text-xs">
																<option value="">Select the original block subvolume...</option>
																{#each nvme.blockSubvolumes as sv}<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>{/each}
															</select>
															<Button size="xs" disabled={!nvme.repairNsDevice} onclick={nvmeRepairNamespace}>Reconnect</Button>
															<Button size="xs" variant="ghost" onclick={() => { nvme.repairNsSubsys = ''; nvme.repairNsDevice = ''; }}>Cancel</Button>
														</div>
													{/if}
												</div>
											{/each}
										</div>
									{/if}
									{#if nvme.addNsSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Block Device {#if !nvme.addNsDevice && addNsTried}<span class="text-amber-500">required</span>{/if}</Label>
												<select bind:value={nvme.addNsDevice} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs {requiredFieldCls(!nvme.addNsDevice, addNsTried)}">
													<option value="">Select...</option>
													{#each nvme.blockSubvolumes as sv}
														<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddNamespaceGuarded}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvme.addNsSubsys = ''; addNsTried = false; }}>Cancel</Button>
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
											<div class="mb-2">
												<Label class="text-xs">Transport</Label>
												<select bind:value={nvme.addPortTransport} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="tcp">TCP</option>
													<option value="rdma" disabled={!rdma.status?.enabled}>RDMA{rdma.status?.enabled ? '' : rdma.status?.capable ? ' (enable RDMA in the transports card above)' : ' (requires an RDMA-capable NIC)'}</option>
												</select>
											</div>
											<ListenAddressPicker
												bind:address={nvme.addPortAddr}
												bind:family={nvme.addPortFamily}
												error={addPortTried ? addPortAddrError : null}
												placeholderV4="192.168.1.10"
												placeholderV6="fd00::1 or 2001:db8::1"
											/>
											<div class="mt-2 flex items-end gap-2">
												<div>
													<Label class="text-xs">Port</Label>
													<Input type="number" bind:value={nvme.addPortSvcId} class="mt-1 h-8 w-24 text-xs" />
												</div>
												<Button size="xs" onclick={nvmeAddPortGuarded}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvme.addPortSubsys = ''; addPortTried = false; }}>Cancel</Button>
											</div>
											{#if !nvme.addPortAddr && addPortTried}
												<p class="mt-1 text-[0.7rem] text-amber-500">Listen address is required.</p>
											{/if}
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
												<Label class="text-xs">Host NQN {#if !nvme.addHostNqn && addHostTried}<span class="text-amber-500">required</span>{/if}</Label>
												<Input bind:value={nvme.addHostNqn} placeholder="nqn.2024-01.com.client:host1" class="mt-1 h-8 text-xs {requiredFieldCls(!nvme.addHostNqn, addHostTried)}" />
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddHostGuarded}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvme.addHostSubsys = ''; addHostTried = false; }}>Cancel</Button>
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
