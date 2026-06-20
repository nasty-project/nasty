<script lang="ts">
	import { ChevronUp, ChevronDown, ChevronsUpDown } from '@lucide/svelte';

	let {
		label,
		active,
		dir,
		onclick,
		align = 'left',
		thClass = 'border-b-2 border-border p-3',
	}: {
		label: string;
		active: boolean;
		dir: 'asc' | 'desc';
		onclick: () => void;
		/** Cell alignment. Defaults to "left"; pass "right" for numeric
		 * columns so the label hugs the right edge of the cell. */
		align?: 'left' | 'right';
		/** Border + padding classes for the <th>. Defaults to the standard
		 * heavy header (border-b-2 + p-3); override to match a table whose
		 * cells use different padding (e.g. a denser "p-2" device table or a
		 * borderless "pb-2 font-medium" header). */
		thClass?: string;
	} = $props();
</script>

<th class="{thClass} text-xs uppercase text-muted-foreground {align === 'right' ? 'text-right' : 'text-left'}">
	<button class="flex items-center gap-1 hover:text-foreground {align === 'right' ? 'ml-auto' : ''}" {onclick}>
		{label}
		{#if active}
			{#if dir === 'asc'}<ChevronUp size={13} />{:else}<ChevronDown size={13} />{/if}
		{:else}
			<ChevronsUpDown size={13} class="opacity-30" />
		{/if}
	</button>
</th>
