<script lang="ts">
	import { ChevronUp, ChevronDown, ChevronsUpDown } from '@lucide/svelte';

	let {
		label,
		active,
		dir,
		onclick,
	}: {
		label: string;
		active: boolean;
		dir: 'asc' | 'desc';
		onclick: () => void;
	} = $props();
</script>

<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">
	<button class="flex items-center gap-1 hover:text-foreground" {onclick}>
		{label}
		{#if active}
			{#if dir === 'asc'}<ChevronUp size={13} />{:else}<ChevronDown size={13} />{/if}
		{:else}
			<ChevronsUpDown size={13} class="opacity-30" />
		{/if}
	</button>
</th>
