<script lang="ts">
	import type { DhcpRelayConfig, NetworkState } from '$lib/types';
	interface Props {
		value?: DhcpRelayConfig | null;
		networkState: NetworkState | null;
		bridgeName: string;
		onchange?: () => void;
	}
	let { value = $bindable(null), networkState, bridgeName, onchange }: Props = $props();
	const upstreams = $derived(networkState?.interfaces.filter(i =>
		i.name !== bridgeName && i.name !== 'lo' && i.kind !== 'bridge' && i.ipv4_addresses.length > 0 &&
		!networkState?.config.bridges.some(b => b.members.includes(i.name)) &&
		!networkState?.config.bonds.some(b => b.members.includes(i.name))
	) ?? []);
</script>

<div class="space-y-2 rounded-md border border-input p-3">
	<label class="flex items-center gap-2 text-sm">
		<input type="checkbox" checked={value != null} onchange={(event) => {
			value = event.currentTarget.checked ? { server: '', upstream: upstreams[0]?.name ?? '' } : null;
			onchange?.();
		}} />
		Relay guest DHCP to an upstream server
	</label>
	{#if value}
		<label class="block text-xs text-muted-foreground">DHCP server IPv4 address
			<input bind:value={value.server} oninput={() => onchange?.()} placeholder="10.10.20.97" class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono" />
		</label>
		<label class="block text-xs text-muted-foreground">Upstream interface
			<select bind:value={value.upstream} onchange={() => onchange?.()} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm">
				<option value="">Select interface</option>
				{#each upstreams as iface}
					<option value={iface.name}>{iface.name}</option>
				{/each}
			</select>
		</label>
		<p class="text-xs text-muted-foreground">Requires a memberless bridge with one static IPv4 address and no host default gateway. Configure the guest DHCP pool, gateway, DNS, and a route back through NASty on your upstream router. NASty does not add NAT or security isolation.</p>
	{/if}
</div>
