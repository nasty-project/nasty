<script lang="ts">
	import { LAUNCHER_NAV_ITEM } from '$lib/navigation';

	interface Props {
		collapsed: boolean;
		active: boolean;
	}

	let { collapsed, active }: Props = $props();
	const Icon = LAUNCHER_NAV_ITEM.icon;
</script>

<nav class="flex-1 px-2 py-3" aria-label="Primary navigation">
	<a
		href={LAUNCHER_NAV_ITEM.href}
		aria-current={active ? 'page' : undefined}
		title={collapsed ? 'Open launcher' : undefined}
		aria-label={collapsed ? 'Open launcher' : undefined}
		class="flex items-center rounded-lg border-2 no-underline transition-all
			{collapsed ? 'justify-center py-2' : 'gap-3 px-3 py-3'}
			{active
				? 'border-blue-500/60 bg-blue-500/10 text-foreground shadow-[0_0_12px_rgba(96,165,250,0.18)]'
				: 'border-border/70 text-muted-foreground hover:border-blue-400/50 hover:bg-accent/50 hover:text-foreground'}"
	>
		<span class="flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-blue-500/10 text-blue-400">
			<Icon size={18} />
		</span>
		{#if !collapsed}
			<span class="min-w-0">
				<span class="block text-sm font-semibold">Open Launcher</span>
				<span class="mt-0.5 block text-[0.65rem] leading-tight text-muted-foreground">Browse categories and pages</span>
			</span>
		{/if}
	</a>
</nav>
