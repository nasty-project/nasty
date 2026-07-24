<script lang="ts">
	import { getContext, tick } from 'svelte';
	import { ChevronLeft, ChevronRight, Search } from '@lucide/svelte';
	import {
		flattenNavigation,
		isNavGroup,
		NAVIGATION_CONTEXT,
		resolveNavigation,
		searchNavigation,
		type NavigationContext,
		type NavGroup
	} from '$lib/navigation';

	const navigationContext = getContext<NavigationContext>(NAVIGATION_CONTEXT);
	let query = $state('');
	let selectedGroupId = $state<string | null>(null);
	let launcherElement = $state<HTMLElement>();
	let backButton = $state<HTMLButtonElement>();

	let entries = $derived(resolveNavigation(navigationContext));
	let selectedGroup = $derived.by((): NavGroup | null => {
		const match = entries.find((entry) => isNavGroup(entry) && entry.id === selectedGroupId);
		return match && isNavGroup(match) ? match : null;
	});
	let matches = $derived(searchNavigation(entries, query));
	let searchItems = $derived(flattenNavigation(entries).filter((entry) => matches.has(entry.href)));
	let isSearching = $derived(query.trim().length > 0);

	$effect(() => {
		if (selectedGroupId && !entries.some((entry) => isNavGroup(entry) && entry.id === selectedGroupId)) {
			selectedGroupId = null;
		}
	});

	async function openGroup(id: string) {
		selectedGroupId = id;
		query = '';
		await tick();
		backButton?.focus();
	}

	async function closeGroup() {
		const id = selectedGroupId;
		selectedGroupId = null;
		await tick();
		if (id) launcherElement?.querySelector<HTMLButtonElement>(`[data-launcher-group="${id}"]`)?.focus();
	}
</script>

<div bind:this={launcherElement} class="mx-auto max-w-7xl">
	<section class="relative mb-6 overflow-hidden rounded-2xl border border-border bg-card px-5 py-6 sm:px-7 sm:py-8">
		<div class="pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_top_right,rgba(59,130,246,0.14),transparent_45%)]"></div>
		<div class="relative flex flex-col gap-5 lg:flex-row lg:items-end lg:justify-between">
			<div>
				<p class="mb-2 font-mono text-[0.65rem] uppercase tracking-[0.28em] text-blue-400">NASty launcher</p>
				<h1 class="text-2xl font-semibold tracking-tight sm:text-3xl">Where do you want to go?</h1>
				<p class="mt-2 max-w-2xl text-sm text-muted-foreground">Open a category to browse its tools, or search every available page.</p>
			</div>
			<label class="relative block w-full lg:max-w-sm">
				<span class="sr-only">Search navigation</span>
				<Search size={17} class="pointer-events-none absolute left-3.5 top-1/2 -translate-y-1/2 text-muted-foreground" />
				<input
					type="search"
					bind:value={query}
					placeholder="Search pages and tools"
					class="h-11 w-full rounded-xl border border-border bg-background/80 pl-10 pr-4 text-sm outline-none transition focus:border-blue-500/60 focus:ring-2 focus:ring-blue-500/20"
				/>
			</label>
		</div>
	</section>

	{#if isSearching}
		<div class="mb-4 flex items-center justify-between">
			<div>
				<h2 class="text-lg font-semibold">Search results</h2>
				<p class="text-xs text-muted-foreground">{searchItems.length} page{searchItems.length === 1 ? '' : 's'} matched</p>
			</div>
			<button onclick={() => { query = ''; }} class="rounded-md px-3 py-1.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground">Clear search</button>
		</div>
		{#if searchItems.length > 0}
			<div class="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
				{#each searchItems as entry (entry.id)}
					{@const Icon = entry.icon}
					<a href={entry.href} class="group flex min-h-32 flex-col justify-between rounded-xl border border-border bg-card p-4 no-underline transition-all hover:-translate-y-0.5 hover:border-blue-400/60 hover:shadow-[0_12px_28px_rgba(15,23,42,0.22)]">
						<span class="flex h-11 w-11 items-center justify-center rounded-xl bg-blue-500/10 text-blue-400 transition-transform group-hover:scale-105"><Icon size={23} /></span>
						<span class="mt-5 flex items-center justify-between gap-3">
							<span class="font-medium text-foreground">{entry.label}</span>
							<ChevronRight size={15} class="text-muted-foreground transition-transform group-hover:translate-x-0.5 group-hover:text-blue-400" />
						</span>
					</a>
				{/each}
			</div>
		{:else}
			<div class="rounded-xl border border-dashed border-border px-6 py-12 text-center text-sm text-muted-foreground">No navigation entries match "{query.trim()}".</div>
		{/if}
	{:else if selectedGroup}
		{@const GroupIcon = selectedGroup.icon}
		<div class="mb-5 flex items-center gap-3">
			<button
				bind:this={backButton}
				onclick={closeGroup}
				class="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border border-border bg-card text-muted-foreground transition-colors hover:border-blue-400/60 hover:text-foreground"
				aria-label="Back to launcher categories"
			>
				<ChevronLeft size={19} />
			</button>
			<span class="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-blue-500/10 text-blue-400"><GroupIcon size={21} /></span>
			<div>
				<h2 class="text-xl font-semibold">{selectedGroup.label}</h2>
				<p class="text-xs text-muted-foreground">{selectedGroup.children.length} available page{selectedGroup.children.length === 1 ? '' : 's'}</p>
			</div>
		</div>
		<div class="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
			{#each selectedGroup.children as child (child.id)}
				{@const Icon = child.icon}
				<a href={child.href} class="group flex min-h-40 flex-col justify-between rounded-xl border border-border bg-card p-5 no-underline transition-all hover:-translate-y-0.5 hover:border-blue-400/60 hover:shadow-[0_12px_28px_rgba(15,23,42,0.22)]">
					<span class="flex h-12 w-12 items-center justify-center rounded-xl bg-blue-500/10 text-blue-400 transition-transform group-hover:scale-105"><Icon size={25} /></span>
					<span class="mt-7 flex items-end justify-between gap-3">
						<span>
							<span class="block text-base font-semibold text-foreground">{child.label}</span>
							<span class="mt-1 block text-xs text-muted-foreground">Open {child.label.toLowerCase()}</span>
						</span>
						<ChevronRight size={17} class="mb-0.5 shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5 group-hover:text-blue-400" />
					</span>
				</a>
			{/each}
		</div>
	{:else}
		<div class="mb-4">
			<h2 class="text-lg font-semibold">Categories</h2>
			<p class="text-xs text-muted-foreground">The same navigation hierarchy, presented as a workspace.</p>
		</div>
		<div class="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
			{#each entries as entry (entry.id)}
				{@const Icon = entry.icon}
				{#if isNavGroup(entry)}
					<button
						onclick={() => openGroup(entry.id)}
						data-launcher-group={entry.id}
						class="group relative flex min-h-44 flex-col justify-between overflow-hidden rounded-xl border border-border bg-card p-5 text-left transition-all hover:-translate-y-0.5 hover:border-blue-400/60 hover:shadow-[0_12px_28px_rgba(15,23,42,0.22)]"
					>
						<span class="absolute inset-y-0 left-0 w-0.5 bg-blue-500/50 opacity-0 transition-opacity group-hover:opacity-100"></span>
						<span class="flex h-12 w-12 items-center justify-center rounded-xl bg-blue-500/10 text-blue-400 transition-transform group-hover:scale-105"><Icon size={25} /></span>
						<span class="mt-7 flex w-full items-end justify-between gap-3">
							<span>
								<span class="block text-base font-semibold text-foreground">{entry.label}</span>
								<span class="mt-1 block text-xs text-muted-foreground">{entry.children.length} page{entry.children.length === 1 ? '' : 's'}</span>
							</span>
							<ChevronRight size={17} class="mb-0.5 shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5 group-hover:text-blue-400" />
						</span>
					</button>
				{:else}
					<a href={entry.href} class="group relative flex min-h-44 flex-col justify-between overflow-hidden rounded-xl border border-border bg-card p-5 no-underline transition-all hover:-translate-y-0.5 hover:border-blue-400/60 hover:shadow-[0_12px_28px_rgba(15,23,42,0.22)]">
						<span class="absolute inset-y-0 left-0 w-0.5 bg-blue-500/50 opacity-0 transition-opacity group-hover:opacity-100"></span>
						<span class="flex h-12 w-12 items-center justify-center rounded-xl bg-blue-500/10 text-blue-400 transition-transform group-hover:scale-105"><Icon size={25} /></span>
						<span class="mt-7 flex items-end justify-between gap-3">
							<span>
								<span class="block text-base font-semibold text-foreground">{entry.label}</span>
								<span class="mt-1 block text-xs text-muted-foreground">Open page</span>
							</span>
							<ChevronRight size={17} class="mb-0.5 shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5 group-hover:text-blue-400" />
						</span>
					</a>
				{/if}
			{/each}
		</div>
	{/if}
</div>
