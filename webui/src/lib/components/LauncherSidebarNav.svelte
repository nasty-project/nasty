<script lang="ts">
	import { LAUNCHER_NAV_ITEM, type NavGroup } from '$lib/navigation';

	interface Props {
		collapsed: boolean;
		active: boolean;
		group: NavGroup | null;
	}

	let { collapsed, active, group }: Props = $props();
	const Icon = LAUNCHER_NAV_ITEM.icon;
</script>

<nav class="flex-1 space-y-2 px-2 py-3" aria-label="Primary navigation">
	<a
		href={LAUNCHER_NAV_ITEM.href}
		aria-current={active ? 'page' : undefined}
		title={collapsed ? 'Start' : undefined}
		aria-label={collapsed ? 'Start' : undefined}
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
				<span class="block text-sm font-semibold">Start</span>
				<span class="mt-0.5 block text-[0.65rem] leading-tight text-muted-foreground">Browse categories and pages</span>
			</span>
		{/if}
	</a>

	{#if group}
		{@const GroupIcon = group.icon}
		<a
			href={`/menu?group=${encodeURIComponent(group.id)}`}
			title={collapsed ? `Open ${group.label} category` : undefined}
			aria-label={collapsed ? `Open ${group.label} category` : undefined}
			class="flex items-center rounded-lg border border-blue-500/35 bg-blue-500/5 text-foreground no-underline transition-colors hover:border-blue-400/60 hover:bg-blue-500/10
				{collapsed ? 'justify-center py-2' : 'gap-3 px-3 py-2.5'}"
		>
			<span class="flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-blue-500/10 text-blue-400">
				<GroupIcon size={18} />
			</span>
			{#if !collapsed}
				<span class="min-w-0">
					<span class="block truncate text-sm font-semibold">{group.label}</span>
					<span class="mt-0.5 block text-[0.65rem] leading-tight text-muted-foreground">Return to category</span>
				</span>
			{/if}
		</a>
	{/if}
</nav>
