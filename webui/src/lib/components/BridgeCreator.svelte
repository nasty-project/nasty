<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { applyNetworkUpdate } from '$lib/rollbackState.svelte';
	import type { NetworkState, NetworkConfig } from '$lib/types';

	interface Props {
		networkState: NetworkState | null;
		/** Called after a successful create with the new bridge's name. The
		 * caller is responsible for re-fetching network state. */
		onCreated?: (bridgeName: string) => void | Promise<void>;
		/** Called when the user clicks Cancel. */
		onCancel?: () => void;
	}

	let { networkState, onCreated, onCancel }: Props = $props();

	let bridgeName = $state('br0');
	let bridgeMembers: string[] = $state([]);
	let bridgeMtu = $state('');
	// Inverted UI flag (checked = NM generates a random MAC). Default
	// unchecked: bridges adopt the primary member's MAC so DHCP keeps
	// handing out the same lease and the user's WebUI session survives
	// the enslave step.
	let bridgeNoInheritMac = $state(false);
	let busy = $state(false);

	function parseMtu(v: unknown): number | null {
		if (v === null || v === undefined || v === '') return null;
		const n = typeof v === 'number' ? v : parseInt(String(v), 10);
		return Number.isFinite(n) && n > 0 ? n : null;
	}

	async function create() {
		if (!bridgeName || !networkState) return;
		busy = true;
		try {
			const network = networkState.config;
			const mtu = parseMtu(bridgeMtu);
			const payload: NetworkConfig = {
				interfaces: network.interfaces || [],
				dns: network.dns || [],
				bonds: network.bonds || [],
				vlans: network.vlans || [],
				bridges: [
					...(network.bridges || []),
					{
						name: bridgeName,
						members: bridgeMembers,
						ipv4: { method: 'inherit', addresses: [], gateway: null },
						ipv6: { method: 'inherit', addresses: [], gateway: null },
						mtu,
						inherit_member_mac: !bridgeNoInheritMac,
					},
				],
			};
			const res = await applyNetworkUpdate(payload, `Bridge ${bridgeName} created`);
			if (res !== undefined) {
				const created = bridgeName;
				bridgeName = 'br0';
				bridgeMembers = [];
				bridgeMtu = '';
				bridgeNoInheritMac = false;
				await onCreated?.(created);
			}
		} finally {
			busy = false;
		}
	}
</script>

<div class="rounded-lg border border-border bg-secondary/20 p-4 space-y-3">
	<div class="text-sm font-medium">Create Bridge Interface</div>
	<p class="text-xs text-muted-foreground">A virtual switch for VMs to share the host's network. Members are optional — leave empty for a host-internal bridge that VMs attach to via veth pairs, or add a physical interface to bridge VMs onto the LAN.</p>
	<div>
		<label for="bridge-name" class="text-xs text-muted-foreground">Name</label>
		<input id="bridge-name" bind:value={bridgeName} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
	</div>
	<div>
		<div class="text-xs text-muted-foreground mb-1">Members (optional)</div>
		{#if networkState}
			<div class="flex flex-wrap gap-2">
				{#each networkState.interfaces.filter(i => i.kind === 'physical' || i.kind === 'bond') as iface}
					<label class="flex items-center gap-1.5 text-sm">
						<input type="checkbox" checked={bridgeMembers.includes(iface.name)}
							onchange={() => { bridgeMembers = bridgeMembers.includes(iface.name) ? bridgeMembers.filter(m => m !== iface.name) : [...bridgeMembers, iface.name]; }} />
						{iface.name}
					</label>
				{/each}
			</div>
		{/if}
	</div>
	<div>
		<label for="bridge-mtu" class="text-xs text-muted-foreground">MTU (optional)</label>
		<input id="bridge-mtu" type="number" min="68" max="65535" bind:value={bridgeMtu} placeholder="default (1500), 9000 for jumbo frames" class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
	</div>
	<div>
		<label class="flex items-start gap-2 text-xs">
			<input type="checkbox" bind:checked={bridgeNoInheritMac} class="mt-0.5" />
			<span>
				<span class="text-foreground">Don't inherit member MAC</span>
				<span class="block text-muted-foreground mt-0.5">By default, the bridge adopts the primary member's MAC so DHCP keeps handing out the same lease across the enslave step. Check this to let NM/the kernel pick a random MAC instead — you'll likely get a new IP.</span>
			</span>
		</label>
	</div>
	{#if networkState?.mgmt_iface && bridgeMembers.includes(networkState.mgmt_iface)}
		<div class="rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-xs text-amber-300 space-y-1">
			<div class="font-medium">Heads up — this bridges your management interface</div>
			<p>You're connected through <span class="font-mono">{networkState.mgmt_iface}</span>. The bridge will adopt its IP and route. After applying, you'll have 30 seconds to keep the change before it auto-rolls back.</p>
			{#if bridgeMembers.length > 1}
				<p>With multiple members, the bridge will use <span class="font-mono">{networkState.mgmt_iface}</span>'s MAC (the management interface, preferred when it's a member). The other members keep their own MACs as bridge slaves — they're L2-only inside the bridge.</p>
			{/if}
			{#if bridgeNoInheritMac}
				<p class="font-medium">⚠ "Don't inherit member MAC" is checked. Your DHCP server will treat the bridge as a new client and is very likely to hand out a different IP — your session will land on the new IP and you'll need to reconnect there to confirm before the 30-second rollback fires.</p>
			{/if}
		</div>
	{/if}
	<div class="flex gap-2">
		<Button size="sm" onclick={create} disabled={!bridgeName || busy}>{busy ? 'Creating…' : 'Create Bridge'}</Button>
		{#if onCancel}
			<Button size="sm" variant="ghost" onclick={onCancel} disabled={busy}>Cancel</Button>
		{/if}
	</div>
</div>
