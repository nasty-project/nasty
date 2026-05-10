<script lang="ts">
	import { ChevronUp, ChevronDown, ChevronsUpDown } from '@lucide/svelte';

	let {
		label,
		active,
		dir,
		onclick,
		align = 'left',
	}: {
		label: string;
		active: boolean;
		dir: 'asc' | 'desc';
		onclick: () => void;
		/** Cell alignment. Defaults to "left"; pass "right" for numeric
		 * columns so the label hugs the right edge of the cell. */
		align?: 'left' | 'right';
	} = $props();
</script>

<th class="border-b-2 border-border p-3 text-xs uppercase text-muted-foreground {align === 'right' ? 'text-right' : 'text-left'}">
	<button class="flex items-center gap-1 hover:text-foreground {align === 'right' ? 'ml-auto' : ''}" {onclick}>
		{label}
		{#if active}
			{#if dir === 'asc'}<ChevronUp size={13} />{:else}<ChevronDown size={13} />{/if}
		{:else}
			<ChevronsUpDown size={13} class="opacity-30" />
		{/if}
	</button>
</th>
