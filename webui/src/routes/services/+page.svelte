<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';

	let protocols: ProtocolStatus[] = $state([]);
	let loading = $state(true);

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'protocol') refresh();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		await withToast(async () => {
			protocols = await client.call<ProtocolStatus[]>('service.protocol.list');
		});
	}

	async function toggle(proto: ProtocolStatus) {
		const action = proto.enabled ? 'disable' : 'enable';
		await withToast(
			() => client.call(`service.protocol.${action}`, { name: proto.name }),
			`${proto.display_name} ${proto.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}
</script>


<p class="mb-6 text-sm text-muted-foreground">
	Enable or disable sharing protocols. Disabled protocols will not start on boot.
</p>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<table class="w-full max-w-2xl text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Protocol</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Running</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each protocols as proto}
				<tr class="border-b border-border">
					<td class="p-3"><strong>{proto.display_name}</strong></td>
					<td class="p-3">
						<Badge variant={proto.enabled ? 'default' : 'secondary'}>
							{proto.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3">
						<span class="inline-block h-2 w-2 rounded-full {proto.running ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
						<span class="ml-1 text-xs text-muted-foreground">{proto.running ? 'Running' : 'Stopped'}</span>
					</td>
					<td class="p-3">
						<Button
							variant={proto.enabled ? 'secondary' : 'default'}
							size="xs"
							onclick={() => toggle(proto)}
						>
							{proto.enabled ? 'Disable' : 'Enable'}
						</Button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}
