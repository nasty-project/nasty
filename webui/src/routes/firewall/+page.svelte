<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast, info } from '$lib/toast.svelte';
	import type { FirewallStatus, NetworkState, PublishedAppPort, CustomRule } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let firewallStatus: FirewallStatus | null = $state(null);
	let networkState: NetworkState | null = $state(null);
	let fwEditService: string | null = $state(null);
	let fwEditSources = $state('');
	let fwEditIfaces: string[] = $state([]);

	// Custom port rules (#620)
	let showAddCustom = $state(false);
	let editCustomId: string | null = $state(null);
	let cLabel = $state('');
	let cTransport: 'tcp' | 'udp' = $state('tcp');
	let cFrom = $state('');
	let cTo = $state('');
	let cSource = $state('');
	let cIface = $state('');
	let cEnabled = $state(true);

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

	function resetCustomForm() {
		showAddCustom = false;
		editCustomId = null;
		cLabel = '';
		cTransport = 'tcp';
		cFrom = '';
		cTo = '';
		cSource = '';
		cIface = '';
		cEnabled = true;
	}

	function startAddCustom() {
		resetCustomForm();
		showAddCustom = true;
	}

	function startEditCustom(r: CustomRule) {
		editCustomId = r.id;
		showAddCustom = true;
		cLabel = r.label;
		cTransport = r.transport;
		cFrom = String(r.from);
		cTo = r.to === r.from ? '' : String(r.to);
		cSource = r.source ?? '';
		cIface = r.iface ?? '';
		cEnabled = r.enabled;
	}

	async function saveCustom() {
		const from = parseInt(cFrom, 10);
		const to = cTo.trim() ? parseInt(cTo, 10) : from;
		const params: Record<string, unknown> = {
			label: cLabel.trim(),
			transport: cTransport,
			from,
			to,
			enabled: cEnabled,
		};
		if (cSource.trim()) params.source = cSource.trim();
		if (cIface.trim()) params.iface = cIface.trim();
		if (editCustomId) params.id = editCustomId;

		const method = editCustomId ? 'system.firewall.custom.update' : 'system.firewall.custom.add';
		const res = await withToast(
			() => client.call<{ rule: CustomRule; warnings: string[] }>(method, params),
			editCustomId ? 'Custom rule updated' : 'Custom rule added'
		);
		if (!res) return;
		for (const w of res.warnings ?? []) {
			info(w);
		}
		resetCustomForm();
		await loadFirewall();
	}

	async function toggleCustom(r: CustomRule) {
		await withToast(
			() =>
				client.call('system.firewall.custom.update', {
					id: r.id,
					label: r.label,
					transport: r.transport,
					from: r.from,
					to: r.to,
					...(r.source ? { source: r.source } : {}),
					...(r.iface ? { iface: r.iface } : {}),
					enabled: !r.enabled,
				}),
			r.enabled ? 'Rule disabled' : 'Rule enabled'
		);
		await loadFirewall();
	}

	async function removeCustom(r: CustomRule) {
		await withToast(() => client.call('system.firewall.custom.remove', { id: r.id }), 'Custom rule removed');
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
								{#each [...new Set(rule.ports.map(p => `${p.to != null ? `${p.port}-${p.to}` : p.port}/${p.transport}`))] as port}
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

		<section class="mt-6 rounded-lg border border-border p-5">
			<div class="flex items-center justify-between gap-4">
				<div>
					<h2 class="text-sm font-semibold">Custom port rules</h2>
					<p class="mt-1 text-xs text-muted-foreground">
						Open a port for something running directly on the host (e.g. a <code>network_mode: host</code> app).
						Bridge-networked apps don't need a rule — their published ports (listed above, if any) already bypass
						this firewall.
					</p>
				</div>
				<Button size="xs" onclick={startAddCustom}>Add rule</Button>
			</div>

			{#if firewallStatus.custom_rules?.length}
				<div class="mt-3 space-y-1">
					{#each firewallStatus.custom_rules as r}
						<div class="flex items-center gap-3 rounded px-3 py-2 text-sm {r.enabled ? '' : 'opacity-40'}">
							<span class="h-2 w-2 rounded-full shrink-0 {r.enabled ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
							<span class="font-medium w-32 truncate">{r.label}</span>
							<span class="font-mono text-xs text-muted-foreground">
								{r.from === r.to ? r.from : `${r.from}-${r.to}`}/{r.transport}
							</span>
							{#if r.source}
								<span class="text-xs text-amber-400">{r.source}</span>
							{/if}
							{#if r.iface}
								<span class="text-xs text-blue-400">{r.iface}</span>
							{/if}
							<div class="ml-auto flex items-center gap-2">
								<Button size="xs" variant="secondary" onclick={() => toggleCustom(r)}>{r.enabled ? 'Disable' : 'Enable'}</Button>
								<Button size="xs" variant="secondary" onclick={() => startEditCustom(r)}>Edit</Button>
								<Button size="xs" variant="destructive" onclick={() => removeCustom(r)}>Delete</Button>
							</div>
						</div>
					{/each}
				</div>
			{:else}
				<p class="mt-3 text-xs text-muted-foreground">No custom rules.</p>
			{/if}

			{#if showAddCustom}
				<div class="mt-3 rounded-lg border border-border bg-secondary/20 p-3 space-y-3">
					<div class="text-xs font-medium">{editCustomId ? 'Edit rule' : 'New rule'}</div>

					<div>
						<div class="text-xs text-muted-foreground mb-1">Label</div>
						<input
							bind:value={cLabel}
							placeholder="e.g. Plex host mode"
							class="w-full rounded-md border border-input bg-background px-2 py-1 text-sm"
						/>
					</div>

					<div class="flex gap-2">
						<div>
							<div class="text-xs text-muted-foreground mb-1">Transport</div>
							<select bind:value={cTransport} class="rounded-md border border-input bg-background px-2 py-1 text-sm h-[30px]">
								<option value="tcp">TCP</option>
								<option value="udp">UDP</option>
							</select>
						</div>
						<div>
							<div class="text-xs text-muted-foreground mb-1">Port</div>
							<input
								bind:value={cFrom}
								placeholder="e.g. 32400"
								class="w-24 rounded-md border border-input bg-background px-2 py-1 font-mono text-sm"
							/>
						</div>
						<div>
							<div class="text-xs text-muted-foreground mb-1">to (optional, range)</div>
							<input
								bind:value={cTo}
								placeholder="e.g. 32410"
								class="w-24 rounded-md border border-input bg-background px-2 py-1 font-mono text-sm"
							/>
						</div>
					</div>

					<div>
						<div class="text-xs text-muted-foreground mb-1">Allowed source IP/CIDR (optional, empty = all)</div>
						<input
							bind:value={cSource}
							placeholder="e.g. 192.168.1.0/24"
							class="w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm"
						/>
					</div>

					<div>
						<div class="text-xs text-muted-foreground mb-1">Interface (optional, empty = any)</div>
						{#if networkState}
							<select bind:value={cIface} class="w-full rounded-md border border-input bg-background px-2 py-1 text-sm h-[30px]">
								<option value="">Any interface</option>
								{#each networkState.interfaces as iface}
									<option value={iface.name}>{iface.name}</option>
								{/each}
							</select>
						{/if}
					</div>

					<label class="flex items-center gap-1.5 text-xs">
						<input type="checkbox" bind:checked={cEnabled} />
						Enabled
					</label>

					<div class="flex gap-2">
						<Button size="xs" onclick={saveCustom}>{editCustomId ? 'Save' : 'Add'}</Button>
						<Button size="xs" variant="secondary" onclick={resetCustomForm}>Cancel</Button>
					</div>
				</div>
			{/if}
		</section>
	{/if}
</div>
