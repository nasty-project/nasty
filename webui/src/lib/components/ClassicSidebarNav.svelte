<script lang="ts">
	import { ChevronRight } from '@lucide/svelte';
	import { isNavGroup, type NavEntry } from '$lib/navigation';

	interface Props {
		entries: NavEntry[];
		currentHref: string;
		activeGroupId: string | null;
		expandedGroups: Record<string, boolean>;
		collapsed: boolean;
		isSearching: boolean;
		searchMatches: Set<string>;
		onToggleGroup: (id: string) => void;
		onNavigate: () => void;
	}

	let {
		entries,
		currentHref,
		activeGroupId,
		expandedGroups,
		collapsed,
		isSearching,
		searchMatches,
		onToggleGroup,
		onNavigate
	}: Props = $props();

	function isExpanded(id: string): boolean {
		return expandedGroups[id] || activeGroupId === id;
	}
</script>

<nav class="flex-1 overflow-y-auto py-2" aria-label="Primary navigation">
	{#each entries as entry (entry.id)}
		{#if isNavGroup(entry)}
			{@const GroupIcon = entry.icon}
			{@const expanded = isExpanded(entry.id)}
			{@const groupActive = activeGroupId === entry.id}
			{@const matchingChildren = isSearching ? entry.children.filter((child) => searchMatches.has(child.href)) : entry.children}
			{#if matchingChildren.length === 0 && isSearching}
				<!-- Group has no matching children during search. -->
			{:else if collapsed}
				{#each matchingChildren as child (child.id)}
					{@const ChildIcon = child.icon}
					{@const active = currentHref === child.href}
					<a
						href={child.href}
						title={child.label}
						aria-label={child.label}
						aria-current={active ? 'page' : undefined}
						class="relative mx-2 flex items-center justify-center rounded-md py-2 text-sm no-underline transition-all border-2
							{active
								? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
								: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
					>
						<ChildIcon size={15} class="shrink-0" />
					</a>
				{/each}
			{:else if isSearching}
				{#each matchingChildren as child (child.id)}
					{@const ChildIcon = child.icon}
					{@const active = currentHref === child.href}
					<a
						href={child.href}
						onclick={onNavigate}
						aria-current={active ? 'page' : undefined}
						class="relative mx-2 flex items-center gap-2.5 rounded-md py-2 pl-4 pr-4 text-sm no-underline transition-all border-2
							{active
								? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
								: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
					>
						<ChildIcon size={14} class="shrink-0" />
						{child.label}
					</a>
				{/each}
			{:else}
				<button
					onclick={() => onToggleGroup(entry.id)}
					aria-expanded={expanded}
					class="mx-2 flex w-[calc(100%-1rem)] items-center gap-2.5 rounded-md py-2 pl-4 pr-3 text-sm transition-colors
						{groupActive ? 'text-foreground font-medium' : 'text-muted-foreground hover:text-foreground'}"
				>
					<GroupIcon size={15} class="shrink-0" />
					{entry.label}
					<ChevronRight size={13} class="ml-auto shrink-0 transition-transform duration-200 {expanded ? 'rotate-90' : ''}" />
				</button>
				{#if expanded}
					{#each entry.children as child (child.id)}
						{@const ChildIcon = child.icon}
						{@const active = currentHref === child.href}
						<a
							href={child.href}
							aria-current={active ? 'page' : undefined}
							class="relative mx-2 flex items-center gap-2.5 rounded-md py-1.5 pl-8 pr-4 text-sm no-underline transition-all border-2
								{active
									? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
									: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
						>
							<ChildIcon size={14} class="shrink-0" />
							{child.label}
						</a>
					{/each}
				{/if}
			{/if}
		{:else if !isSearching || searchMatches.has(entry.href)}
			{@const Icon = entry.icon}
			{@const active = currentHref === entry.href}
			<a
				href={entry.href}
				onclick={() => { if (isSearching) onNavigate(); }}
				title={collapsed ? entry.label : undefined}
				aria-label={collapsed ? entry.label : undefined}
				aria-current={active ? 'page' : undefined}
				class="relative mx-2 flex items-center rounded-md py-2 text-sm no-underline transition-all border-2
					{collapsed ? 'justify-center px-0' : 'gap-2.5 pl-4 pr-4'}
					{active
						? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
						: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
			>
				<Icon size={15} class="shrink-0" />
				{#if !collapsed}{entry.label}{/if}
			</a>
		{/if}
	{/each}
</nav>
