<script lang="ts">
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { getClient } from '$lib/client';
	import { listenAddressOptions } from '$lib/network';
	import type { LiveInterface, NetworkState } from '$lib/types';

	let {
		address = $bindable(),
		family = $bindable(),
		allowWildcards = false,
		error = null,
		placeholderV4 = '192.168.1.10',
		placeholderV6 = 'fd00::1',
	}: {
		address: string;
		family: 'ipv4' | 'ipv6';
		allowWildcards?: boolean;
		error?: string | null;
		placeholderV4?: string;
		placeholderV6?: string;
	} = $props();

	let interfaces = $state<LiveInterface[]>([]);
	let loaded = $state(false);
	let selection = $state<string>('custom');

	$effect(() => {
		getClient()
			.call<NetworkState>('system.network.get')
			.then((s) => {
				interfaces = s.interfaces ?? [];
			})
			.catch(() => {
				// Picker degrades to Custom-only — operator can still
				// type an address. Don't surface a toast: the form
				// works without the suggestions, and the share-panel
				// already shows its own errors prominently.
			})
			.finally(() => {
				loaded = true;
			});
	});

	const options = $derived(listenAddressOptions(interfaces, allowWildcards));

	function onSelectionChange() {
		if (selection === 'custom') return;
		const opt = options.find((o) => o.key === selection);
		if (opt) {
			address = opt.addr;
			family = opt.family;
		}
	}
</script>

<div>
	<Label class="text-xs">Listen Address</Label>
	<select
		bind:value={selection}
		onchange={onSelectionChange}
		class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs"
	>
		<option value="custom">Custom address…</option>
		{#each options as opt (opt.key)}
			<option value={opt.key}>{opt.label}</option>
		{/each}
	</select>
	{#if loaded && options.length === 0 && !allowWildcards}
		<p class="mt-1 text-[0.7rem] text-muted-foreground">
			No live interface addresses detected — fall back to a custom address below.
		</p>
	{/if}

	{#if selection === 'custom'}
		<div class="mt-2 flex items-end gap-2">
			<div>
				<Label class="text-[0.7rem] text-muted-foreground">Family</Label>
				<select
					bind:value={family}
					class="mt-1 h-8 rounded-md border border-input bg-transparent px-2 text-xs"
				>
					<option value="ipv4">IPv4</option>
					<option value="ipv6">IPv6</option>
				</select>
			</div>
			<div class="flex-1">
				<Label class="text-[0.7rem] text-muted-foreground">Address</Label>
				<Input
					bind:value={address}
					placeholder={family === 'ipv6' ? placeholderV6 : placeholderV4}
					class="mt-1 h-8 text-xs {error ? 'border-red-400' : ''}"
				/>
			</div>
		</div>
		{#if error}
			<p class="mt-1 text-[0.7rem] text-red-400">{error}</p>
		{/if}
	{:else}
		<p class="mt-1 text-[0.7rem] text-muted-foreground">
			Listening on <code>{address}</code> ({family.toUpperCase()})
		</p>
	{/if}
</div>
