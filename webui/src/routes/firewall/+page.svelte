<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { FirewallStatus, NetworkState, PublishedAppPort } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let firewallStatus: FirewallStatus | null = $state(null);
	let networkState: NetworkState | null = $state(null);
	let fwEditService: string | null = $state(null);
	let fwEditSources = $state('');
	let fwEditIfaces: string[] = $state([]);

	/** A published app port, or a collapsed contiguous 1:1 range of them. */
	type AppPortRow = PublishedAppPort & { host_port_end?: number };

	/** Collapse contiguous 1:1 (host == container) published ports of the same
	 * app + transport into a single range row, so a game server publishing
	 * 2300-2399 shows as one line instead of 100. Non-contiguous or
	 * remapped (host != container) ports stay individual. */
	const collapsedAppPorts = $derived.by<AppPortRow[]>(() => {
		const ports = [...(firewallStatus?.published_app_ports ?? [])].sort(
			(a, b) => a.app.localeCompare(b.app) || a.transport.localeCompare(b.transport) || a.host_port - b.host_port
		);
		const out: AppPortRow[] = [];
		for (const p of ports) {
			const last = out[out.length - 1];
			const lastEnd = last ? (last.host_port_end ?? last.host_port) : -1;
			const oneToOne = p.host_port === p.container_port;
			const lastOneToOne = last ? (last.host_port_end ?? last.host_port) === last.container_port : false;
			if (last && oneToOne && lastOneToOne && last.app === p.app && last.transport === p.transport && lastEnd + 1 === p.host_port) {
				last.host_port_end = p.host_port;
				last.container_port = p.container_port;
			} else {
				out.push({ ...p });
			}
		}
		return out;
	});

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

								{#if rule.service === 'webui'}
									<div class="rounded border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
										Restricting the WebUI also restricts Terminal, VM console, app deployment, and log streaming — they all use the same port.
									</div>
								{/if}

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

		{#if firewallStatus.published_app_ports?.length}
			<section class="mt-6 rounded-lg border border-border p-5">
				<h2 class="text-sm font-semibold">App ports (published by Docker)</h2>
				<p class="mt-1 text-xs text-muted-foreground">
					These host ports are opened directly by Docker for your apps and bypass this firewall —
					Docker forwards them straight to the container, so the rules above don't apply.
					Their only gate is your upstream/cloud firewall. Shown here so you can see everything
					that's reachable on this box in one place.
				</p>
				<div class="mt-3 space-y-1">
					{#each collapsedAppPorts as p}
						<div class="flex items-center gap-3 rounded px-3 py-2 text-sm">
							<span class="h-2 w-2 rounded-full shrink-0 bg-sky-400"></span>
							<span class="font-mono text-xs w-28">
								{#if p.host_port_end}{p.host_port}–{p.host_port_end}{:else}{p.host_port}{/if}/{p.transport}
							</span>
							<a href="/apps" class="font-medium text-primary hover:underline">{p.app}</a>
							{#if !p.host_port_end && p.container_port !== p.host_port}
								<span class="text-xs text-muted-foreground">→ container {p.container_port}</span>
							{/if}
							<span class="ml-auto text-xs text-sky-400">Open (Docker)</span>
						</div>
					{/each}
				</div>
			</section>
		{/if}
	{/if}
</div>
