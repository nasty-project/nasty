<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { FirewallStatus, NetworkState } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let firewallStatus: FirewallStatus | null = $state(null);
	let networkState: NetworkState | null = $state(null);
	let fwEditService: string | null = $state(null);
	let fwEditSources = $state('');
	let fwEditIfaces: string[] = $state([]);

	onMount(async () => {
		await Promise.all([loadFirewall(), loadNetwork()]);
	});

	async function loadFirewall() {
		try { firewallStatus = await client.call<FirewallStatus>('system.firewall.status'); } catch { /* ignore */ }
	}

	async function loadNetwork() {
		try { networkState = await client.call<NetworkState>('system.network.get'); } catch { /* ignore */ }
	}

	function startEditRestriction(service: string) {
		fwEditService = service;
		fwEditSources = (firewallStatus?.restrictions[service] ?? []).join(', ');
		fwEditIfaces = [...(firewallStatus?.interface_restrictions[service] ?? [])];
	}

	async function saveRestriction() {
		if (!fwEditService) return;
		const sources = fwEditSources.split(/[,\s]+/).map(s => s.trim()).filter(Boolean);
		await withToast(
			() => client.call('system.firewall.restrict', { service: fwEditService, sources, interfaces: fwEditIfaces }),
			'Firewall restriction updated'
		);
		fwEditService = null;
		fwEditSources = '';
		fwEditIfaces = [];
		await loadFirewall();
	}
</script>

<div>
	<h1 class="text-2xl font-bold">Firewall</h1>
	<p class="text-sm text-muted-foreground mt-0.5">Dynamic nftables firewall — ports open and close automatically with services.</p>
</div>

<div class="mt-6 max-w-2xl">
	{#if !firewallStatus}
		<p class="text-muted-foreground">Loading...</p>
	{:else}
		<section class="rounded-lg border border-border p-5">
			<div class="space-y-1">
				{#each firewallStatus.rules as rule}
					<div>
						<button
							class="w-full text-left flex items-center gap-3 rounded px-3 py-2 text-sm transition-colors hover:bg-muted/30 {rule.active ? '' : 'opacity-40'}"
							onclick={() => startEditRestriction(rule.service)}
						>
							<span class="h-2 w-2 rounded-full shrink-0 {rule.active ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
							<span class="font-medium w-20">{rule.service}</span>
							<span class="font-mono text-xs text-muted-foreground">
								{#each [...new Set(rule.ports.map(p => `${p.port}/${p.transport}`))] as port}
									{port}{' '}
								{/each}
							</span>
							{#if firewallStatus.restrictions[rule.service]?.length}
								<span class="text-xs text-amber-400">
									{firewallStatus.restrictions[rule.service].length} source{firewallStatus.restrictions[rule.service].length !== 1 ? 's' : ''}
								</span>
							{/if}
							{#if firewallStatus.interface_restrictions[rule.service]?.length}
								<span class="text-xs text-blue-400">
									{firewallStatus.interface_restrictions[rule.service].join(', ')}
								</span>
							{/if}
							<span class="ml-auto text-xs {rule.active ? 'text-green-400' : 'text-muted-foreground'}">{rule.active ? 'Open' : 'Closed'}</span>
						</button>

						{#if fwEditService === rule.service}
							<div class="mx-3 mb-2 rounded-lg border border-border bg-secondary/20 p-3 space-y-3">
								<div class="text-xs font-medium">Restrict access to {rule.service}</div>

								<div>
									<div class="text-xs text-muted-foreground mb-1">Allowed source IPs (comma-separated, empty = all)</div>
									<input
										bind:value={fwEditSources}
										placeholder="e.g. 192.168.1.0/24, 10.0.0.5"
										class="w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm"
									/>
								</div>

								<div>
									<div class="text-xs text-muted-foreground mb-1">Allowed interfaces (none selected = all)</div>
									{#if networkState}
										<div class="flex flex-wrap gap-2">
											{#each networkState.interfaces as iface}
												<label class="flex items-center gap-1.5 text-xs">
													<input type="checkbox"
														checked={fwEditIfaces.includes(iface.name)}
														onchange={() => {
															fwEditIfaces = fwEditIfaces.includes(iface.name)
																? fwEditIfaces.filter(i => i !== iface.name)
																: [...fwEditIfaces, iface.name];
														}}
													/>
													<span class="font-mono">{iface.name}</span>
												</label>
											{/each}
										</div>
									{/if}
								</div>

								<div class="flex gap-2">
									<Button size="xs" onclick={saveRestriction}>Save</Button>
									<Button size="xs" variant="secondary" onclick={() => fwEditService = null}>Cancel</Button>
								</div>
							</div>
						{/if}
					</div>
				{/each}
			</div>
			<p class="mt-3 text-xs text-muted-foreground">
				Ports open/close automatically with services. Click a service to restrict access by source IP or interface.
			</p>
		</section>
	{/if}
</div>
