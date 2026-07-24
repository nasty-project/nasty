<script lang="ts">
	import { tick } from 'svelte';
	import { ChevronLeft, ChevronRight } from '@lucide/svelte';
	import { flattenNavigation, isNavGroup, type NavEntry, type NavGroup, type NavMode } from '$lib/navigation';
	import { uiPrefs } from '$lib/uiPrefs.svelte';

	interface Props {
		entries: NavEntry[];
		fullEntries: NavEntry[];
		mode: NavMode;
		currentHref: string;
		activeGroupId: string | null;
		collapsed: boolean;
		isSearching: boolean;
		searchMatches: Set<string>;
		onNavigate: () => void;
	}

	let {
		entries,
		fullEntries,
		mode,
		currentHref,
		activeGroupId,
		collapsed,
		isSearching,
		searchMatches,
		onNavigate
	}: Props = $props();

	let selectedGroupId = $state<string | null>(null);
	let previousCurrentHref = $state('');
	let previousMode = $state<NavMode | null>(null);
	let initialized = $state(false);
	let navElement = $state<HTMLElement>();
	let backButton = $state<HTMLButtonElement>();
	let selectedGroup = $derived.by((): NavGroup | null => {
		const match = fullEntries.find((entry) => isNavGroup(entry) && entry.id === selectedGroupId);
		return match && isNavGroup(match) ? match : null;
	});
	let searchItems = $derived(flattenNavigation(entries).filter((item) => searchMatches.has(item.href)));

	$effect(() => {
		if (!initialized) {
			const persisted = uiPrefs.iconGroupId;
			selectedGroupId = activeGroupId ?? (persisted && fullEntries.some((entry) => isNavGroup(entry) && entry.id === persisted) ? persisted : null);
			previousCurrentHref = currentHref;
			previousMode = mode;
			initialized = true;
		} else if (currentHref !== previousCurrentHref) {
			selectedGroupId = activeGroupId;
			previousCurrentHref = currentHref;
		}
		if (mode !== previousMode) {
			if (mode === 'full') selectedGroupId = activeGroupId ?? uiPrefs.iconGroupId;
			previousMode = mode;
		}
		if (selectedGroupId && !fullEntries.some((entry) => isNavGroup(entry) && entry.id === selectedGroupId)) {
			selectedGroupId = null;
			uiPrefs.setIconGroupId(null);
		}
	});

	function selectGroup(id: string | null) {
		selectedGroupId = id;
		uiPrefs.setIconGroupId(id);
	}

	async function openGroup(id: string) {
		selectGroup(id);
		await tick();
		backButton?.focus();
	}

	async function closeGroup() {
		const id = selectedGroupId;
		selectGroup(null);
		await tick();
		if (id) navElement?.querySelector<HTMLButtonElement>(`[data-nav-group="${id}"]`)?.focus();
	}

	function navigateDirect() {
		selectGroup(null);
		onNavigate();
	}

	function navigateSearch(href: string) {
		onNavigate();
		if (href === currentHref) selectGroup(activeGroupId);
	}
</script>

<nav bind:this={navElement} class="flex-1 overflow-y-auto px-2 py-3" aria-label="Primary navigation">
	{#if collapsed && isSearching}
		<div class="space-y-1" aria-label="Navigation search results">
			{#each searchItems as entry (entry.id)}
				{@const Icon = entry.icon}
				{@const active = currentHref === entry.href}
				<a
					href={entry.href}
					onclick={() => navigateSearch(entry.href)}
					title={entry.label}
					aria-label={entry.label}
					aria-current={active ? 'page' : undefined}
					class="flex min-h-12 flex-col items-center justify-center gap-1 rounded-md border-2 px-0.5 py-1.5 no-underline transition-all
						{active ? 'border-blue-500/50 text-foreground shadow-[0_0_8px_rgba(96,165,250,0.25)]' : 'border-transparent text-muted-foreground hover:border-blue-400/50 hover:text-foreground'}"
				>
					<Icon size={16} />
					<span class="w-full text-center text-[0.625rem] font-medium leading-tight">{entry.label}</span>
				</a>
			{/each}
		</div>
	{:else if collapsed && mode === 'full' && selectedGroup}
		<div class="space-y-1">
			<button
				bind:this={backButton}
				onclick={closeGroup}
				class="mb-2 flex min-h-12 w-full flex-col items-center justify-center gap-1 rounded-md border border-border py-1.5 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
				aria-label="Back to navigation categories"
				title="Back to categories"
			>
				<ChevronLeft size={16} />
				<span class="text-[0.625rem] font-medium">Back</span>
			</button>
			{#each selectedGroup.children as child (child.id)}
				{@const Icon = child.icon}
				{@const active = currentHref === child.href}
				<a
					href={child.href}
					onclick={onNavigate}
					title={child.label}
					aria-label={child.label}
					aria-current={active ? 'page' : undefined}
					class="flex min-h-12 flex-col items-center justify-center gap-1 rounded-md border-2 px-0.5 py-1.5 no-underline transition-all
						{active ? 'border-blue-500/50 text-foreground shadow-[0_0_8px_rgba(96,165,250,0.25)]' : 'border-transparent text-muted-foreground hover:border-blue-400/50 hover:text-foreground'}"
				>
					<Icon size={16} />
					<span class="w-full text-center text-[0.625rem] font-medium leading-tight">{child.label}</span>
				</a>
			{/each}
		</div>
	{:else if collapsed}
		<div class="space-y-1">
			{#each entries as entry (entry.id)}
				{@const Icon = entry.icon}
				{@const active = isNavGroup(entry) ? activeGroupId === entry.id : currentHref === entry.href}
				{#if isNavGroup(entry)}
					<button
						onclick={() => openGroup(entry.id)}
						data-nav-group={entry.id}
						title={entry.label}
						aria-label={`Open ${entry.label}`}
						class="flex min-h-12 w-full flex-col items-center justify-center gap-1 rounded-md border-2 px-0.5 py-1.5 transition-all
							{active ? 'border-blue-500/50 text-foreground shadow-[0_0_8px_rgba(96,165,250,0.25)]' : 'border-transparent text-muted-foreground hover:border-blue-400/50 hover:text-foreground'}"
					>
						<Icon size={16} />
						<span class="w-full text-center text-[0.625rem] font-medium leading-tight">{entry.label}</span>
					</button>
				{:else}
					<a
						href={entry.href}
						onclick={navigateDirect}
						title={entry.label}
						aria-label={entry.label}
						aria-current={active ? 'page' : undefined}
						class="flex min-h-12 flex-col items-center justify-center gap-1 rounded-md border-2 px-0.5 py-1.5 no-underline transition-all
							{active ? 'border-blue-500/50 text-foreground shadow-[0_0_8px_rgba(96,165,250,0.25)]' : 'border-transparent text-muted-foreground hover:border-blue-400/50 hover:text-foreground'}"
					>
						<Icon size={16} />
						<span class="w-full text-center text-[0.625rem] font-medium leading-tight">{entry.label}</span>
					</a>
				{/if}
			{/each}
		</div>
	{:else if isSearching}
		<div class="grid grid-cols-2 gap-2" aria-label="Navigation search results">
			{#each searchItems as entry (entry.id)}
				{@const Icon = entry.icon}
				{@const active = currentHref === entry.href}
				<a
					href={entry.href}
					onclick={() => navigateSearch(entry.href)}
					aria-current={active ? 'page' : undefined}
					class="group flex min-h-20 flex-col items-center justify-center gap-2 rounded-lg border px-2 py-3 text-center no-underline transition-all
						{active ? 'border-blue-500/60 bg-blue-500/10 text-foreground shadow-[0_0_12px_rgba(96,165,250,0.18)]' : 'border-border/70 text-muted-foreground hover:border-blue-400/50 hover:bg-accent/50 hover:text-foreground'}"
				>
					<Icon size={20} class="transition-transform group-hover:scale-110" />
					<span class="text-[0.7rem] font-medium leading-tight">{entry.label}</span>
				</a>
			{/each}
		</div>
	{:else if mode === 'full' && selectedGroup}
		{@const GroupIcon = selectedGroup.icon}
		<div class="mb-3 flex items-center gap-2 border-b border-border/70 pb-2">
			<button
				bind:this={backButton}
				onclick={closeGroup}
				class="flex h-7 w-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
				aria-label="Back to navigation categories"
				title="Back to categories"
			>
				<ChevronLeft size={16} />
			</button>
			<div class="flex min-w-0 items-center gap-2">
				<GroupIcon size={17} class="shrink-0 text-blue-400" />
				<span class="truncate text-sm font-semibold">{selectedGroup.label}</span>
			</div>
		</div>
		<div class="grid grid-cols-2 gap-2">
			{#each selectedGroup.children as child (child.id)}
				{@const Icon = child.icon}
				{@const active = currentHref === child.href}
				<a
					href={child.href}
					onclick={onNavigate}
					aria-current={active ? 'page' : undefined}
					class="group flex min-h-20 flex-col items-center justify-center gap-2 rounded-lg border px-2 py-3 text-center no-underline transition-all
						{active ? 'border-blue-500/60 bg-blue-500/10 text-foreground shadow-[0_0_12px_rgba(96,165,250,0.18)]' : 'border-border/70 text-muted-foreground hover:border-blue-400/50 hover:bg-accent/50 hover:text-foreground'}"
				>
					<Icon size={21} class="transition-transform group-hover:scale-110" />
					<span class="text-[0.7rem] font-medium leading-tight">{child.label}</span>
				</a>
			{/each}
		</div>
	{:else}
		<div class="grid grid-cols-2 gap-2">
			{#each entries as entry (entry.id)}
				{@const Icon = entry.icon}
				{@const active = isNavGroup(entry) ? activeGroupId === entry.id : currentHref === entry.href}
				{#if isNavGroup(entry)}
					<button
						onclick={() => openGroup(entry.id)}
						data-nav-group={entry.id}
						class="group relative flex min-h-20 flex-col items-center justify-center gap-2 rounded-lg border px-2 py-3 text-center transition-all
							{active ? 'border-blue-500/60 bg-blue-500/10 text-foreground shadow-[0_0_12px_rgba(96,165,250,0.18)]' : 'border-border/70 text-muted-foreground hover:border-blue-400/50 hover:bg-accent/50 hover:text-foreground'}"
						aria-label={`Open ${entry.label}`}
					>
						<Icon size={22} class="transition-transform group-hover:scale-110" />
						<span class="text-[0.7rem] font-medium leading-tight">{entry.label}</span>
						<ChevronRight size={12} class="absolute right-1.5 top-1.5 text-muted-foreground/50" />
					</button>
				{:else}
					<a
						href={entry.href}
						onclick={navigateDirect}
						aria-current={active ? 'page' : undefined}
						class="group flex min-h-20 flex-col items-center justify-center gap-2 rounded-lg border px-2 py-3 text-center no-underline transition-all
							{active ? 'border-blue-500/60 bg-blue-500/10 text-foreground shadow-[0_0_12px_rgba(96,165,250,0.18)]' : 'border-border/70 text-muted-foreground hover:border-blue-400/50 hover:bg-accent/50 hover:text-foreground'}"
					>
						<Icon size={22} class="transition-transform group-hover:scale-110" />
						<span class="text-[0.7rem] font-medium leading-tight">{entry.label}</span>
					</a>
				{/if}
			{/each}
		</div>
	{/if}
</nav>
