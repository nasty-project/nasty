<script lang="ts">
	import { tick } from 'svelte';
	import { page } from '$app/stores';
	import { theme } from '$lib/theme.svelte';
	import { formatBytes } from '$lib/format';
	import {
		joinSharePath,
		parentSharePath,
		shareBreadcrumbs,
		shareBrowseUrl,
		shareDownloadUrl,
		shareZipUrl,
		type PublicDirectoryEntry,
		type PublicDirectoryListing,
		type PublicShareMeta,
		type PublicShareRoot
	} from '$lib/public-share';
	import logoLight from '$lib/assets/nasty.svg';
	import logoDark from '$lib/assets/nasty-white.svg';
	import { ArrowLeft, ChevronRight, Download, File, FolderOpen, Home, Lock } from '@lucide/svelte';

	const token = $derived($page.params.token ?? '');

	let loading = $state(true);
	let meta = $state<PublicShareMeta | null>(null);
	let notAvailable = $state(false);
	let selectedRoot = $state<PublicShareRoot | null>(null);
	let listing = $state<PublicDirectoryListing | null>(null);
	let browseLoading = $state(false);
	let browseError = $state('');
	let browseRequest = 0;
	let shareGeneration = 0;
	let shareAbortController: AbortController | null = null;
	let downloadError = $state('');
	let downloadPending = $state(false);
	let breadcrumbNav = $state<HTMLElement | null>(null);

	let unlocked = $state(false);
	let password = $state('');
	let unlocking = $state(false);
	let unlockError = $state('');

	const needsUnlock = $derived(!!meta?.password_required && !unlocked);
	const breadcrumbs = $derived(
		selectedRoot && listing ? shareBreadcrumbs(selectedRoot.name, listing.path) : []
	);

	function fmtExpiry(secs: number | null): string {
		if (!secs) return '';
		return new Date(secs * 1000).toLocaleString();
	}

	function resetShareState() {
		browseRequest++;
		meta = null;
		notAvailable = false;
		selectedRoot = null;
		listing = null;
		browseLoading = false;
		browseError = '';
		downloadError = '';
		downloadPending = false;
		unlocked = false;
		password = '';
		unlocking = false;
		unlockError = '';
	}

	async function loadMeta(requestToken: string, generation: number, signal: AbortSignal) {
		loading = true;
		notAvailable = false;
		try {
			const response = await fetch(`/api/public/share/${encodeURIComponent(requestToken)}`, { signal });
			if (generation !== shareGeneration) return;
			if (!response.ok) {
				notAvailable = true;
				return;
			}
			const nextMeta = (await response.json()) as PublicShareMeta;
			if (generation !== shareGeneration) return;
			meta = nextMeta;
			unlocked = nextMeta.unlocked;
		} catch {
			if (generation === shareGeneration) notAvailable = true;
		} finally {
			if (generation === shareGeneration) loading = false;
		}
	}

	async function unlock(event: Event) {
		event.preventDefault();
		if (!password) return;
		unlocking = true;
		unlockError = '';
		const requestToken = token;
		const generation = shareGeneration;
		const signal = shareAbortController?.signal;
		try {
			const response = await fetch(`/api/public/share/${encodeURIComponent(requestToken)}/unlock`, {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ password }),
				signal
			});
			if (generation !== shareGeneration) return;
			if (response.ok) {
				const nextMeta = (await response.json()) as PublicShareMeta;
				if (generation !== shareGeneration) return;
				password = '';
				meta = nextMeta;
				unlocked = nextMeta.unlocked;
			} else if (response.status === 429) {
				unlockError = 'Too many attempts. Please try again later.';
			} else if (response.status === 404) {
				notAvailable = true;
			} else {
				unlockError = 'Incorrect password.';
			}
		} catch {
			if (generation === shareGeneration) unlockError = 'Something went wrong. Please try again.';
		} finally {
			if (generation === shareGeneration) unlocking = false;
		}
	}

	async function browse(root: PublicShareRoot, path: string) {
		const request = ++browseRequest;
		browseLoading = true;
		browseError = '';
		try {
			const response = await fetch(shareBrowseUrl(token, root.root, path));
			if (response.status === 401) {
				if (request !== browseRequest) return;
				showRoots();
				unlocked = false;
				unlockError = 'Your unlock expired. Enter the password again.';
				return;
			}
			if (!response.ok) {
				if (request !== browseRequest) return;
				browseError = 'This folder is no longer available.';
				return;
			}
			const nextListing = (await response.json()) as PublicDirectoryListing;
			if (request !== browseRequest) return;
			selectedRoot = root;
			listing = nextListing;
			await tick();
			if (request === browseRequest) {
				breadcrumbNav?.scrollTo({ left: breadcrumbNav.scrollWidth, behavior: 'smooth' });
			}
		} catch {
			if (request === browseRequest) browseError = 'Could not load this folder. Please try again.';
		} finally {
			if (request === browseRequest) browseLoading = false;
		}
	}

	function showRoots() {
		browseRequest++;
		browseLoading = false;
		browseError = '';
		downloadError = '';
		selectedRoot = null;
		listing = null;
	}

	function openDirectory(entry: PublicDirectoryEntry) {
		if (!selectedRoot || !listing) return;
		void browse(selectedRoot, joinSharePath(listing.path, entry.name));
	}

	function goUp() {
		if (!selectedRoot || !listing) return;
		if (!listing.path) {
			showRoots();
			return;
		}
		void browse(selectedRoot, parentSharePath(listing.path));
	}

	async function startDownload(event: MouseEvent, url: string, name: string) {
		event.preventDefault();
		if (downloadPending) return;
		downloadPending = true;
		downloadError = '';
		const generation = shareGeneration;
		let started = false;
		try {
			const response = await fetch(url, { method: 'HEAD' });
			if (generation !== shareGeneration) return;
			if (response.status === 401) {
				showRoots();
				unlocked = false;
				unlockError = 'Your unlock expired. Enter the password again.';
				return;
			}
			if (!response.ok) {
				downloadError = 'This download is no longer available.';
				return;
			}
			const link = document.createElement('a');
			link.href = url;
			link.download = name;
			link.click();
			started = true;
		} catch {
			if (generation === shareGeneration) {
				downloadError = 'Could not start the download. Please try again.';
			}
		} finally {
			if (generation !== shareGeneration) return;
			if (started) {
				window.setTimeout(() => {
					if (generation === shareGeneration) downloadPending = false;
				}, 1000);
			} else {
				downloadPending = false;
			}
		}
	}

	$effect(() => {
		const requestToken = token;
		shareAbortController?.abort();
		const controller = new AbortController();
		shareAbortController = controller;
		const generation = ++shareGeneration;
		resetShareState();
		if (requestToken) void loadMeta(requestToken, generation, controller.signal);
		return () => controller.abort();
	});
</script>

<svelte:head>
	<title>Shared files - NASty</title>
</svelte:head>

<div class="flex min-h-screen items-center justify-center bg-background p-3 sm:p-6">
	<div class="w-full max-w-3xl rounded-xl border border-border bg-card p-4 shadow-sm sm:p-8">
		<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="mx-auto mb-6 h-16" />

		{#if loading}
			<p class="text-center text-sm text-muted-foreground">Loading...</p>
		{:else if notAvailable}
			<h1 class="text-center text-lg font-semibold">This share is not available</h1>
			<p class="mt-2 text-center text-sm text-muted-foreground">
				The link may have expired, been revoked, or reached its download limit.
			</p>
		{:else if meta}
			<div class="text-center">
				<h1 class="text-xl font-semibold">Shared with you</h1>
				{#if meta.expires_at}
					<p class="mt-1 text-xs text-muted-foreground">
						Available until {fmtExpiry(meta.expires_at)}
					</p>
				{/if}
			</div>

			{#if needsUnlock}
				<form onsubmit={unlock} class="mx-auto mt-6 max-w-xs">
					<div class="mb-2 flex items-center justify-center gap-2 text-sm text-muted-foreground">
						<Lock size={14} /> This share is password protected
					</div>
					<label for="share-password" class="sr-only">Share password</label>
					<input
						id="share-password"
						type="password"
						bind:value={password}
						placeholder="Password"
						autocomplete="current-password"
						aria-invalid={unlockError ? 'true' : undefined}
						aria-describedby={unlockError ? 'share-password-error' : undefined}
						class="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm" />
					{#if unlockError}
						<p id="share-password-error" role="alert" class="mt-2 text-sm text-destructive">{unlockError}</p>
					{/if}
					<button
						type="submit"
						disabled={unlocking || !password}
						class="mt-3 h-9 w-full rounded-md bg-primary text-sm font-medium text-primary-foreground disabled:opacity-50">
						{unlocking ? 'Unlocking...' : 'Unlock'}
					</button>
				</form>
			{:else}
				<div class="mt-6 flex flex-col gap-3 border-b border-border pb-4 sm:flex-row sm:items-center sm:justify-between">
					<p class="text-sm text-muted-foreground">
						{selectedRoot ? 'Browse and download individual files.' : `${meta.entries.length} shared item${meta.entries.length === 1 ? '' : 's'}`}
					</p>
					<a
						href={shareZipUrl(token)}
						download="shared-files.zip"
						aria-disabled={downloadPending}
						onclick={(event) => void startDownload(event, shareZipUrl(token), 'shared-files.zip')}
						class="inline-flex h-9 shrink-0 items-center justify-center gap-2 rounded-md border border-border px-3 text-sm font-medium hover:bg-accent {downloadPending ? 'pointer-events-none opacity-50' : ''}">
						<Download size={15} /> Download all as ZIP
					</a>
				</div>

				{#if selectedRoot && listing}
					<div class="mt-4 flex items-center gap-2">
						<button
							type="button"
							onclick={goUp}
							class="inline-flex size-9 shrink-0 items-center justify-center rounded-md border border-border hover:bg-accent"
							aria-label="Go up">
							<ArrowLeft size={16} />
						</button>
						<nav bind:this={breadcrumbNav} class="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto text-sm" aria-label="Folder path">
							<button
								type="button"
								onclick={showRoots}
								class="inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-1 text-muted-foreground hover:bg-accent hover:text-foreground">
								<Home size={14} /> Shared items
							</button>
							{#each breadcrumbs as crumb, index (crumb.path)}
								<ChevronRight size={14} class="shrink-0 text-muted-foreground" />
								<button
									type="button"
									onclick={() => selectedRoot && void browse(selectedRoot, crumb.path)}
									disabled={index === breadcrumbs.length - 1}
									aria-current={index === breadcrumbs.length - 1 ? 'page' : undefined}
									class="max-w-48 shrink-0 truncate rounded px-1.5 py-1 hover:bg-accent disabled:font-medium disabled:text-foreground">
									{crumb.label}
								</button>
							{/each}
						</nav>
					</div>
				{/if}

				{#if browseError}
					<p role="alert" class="mt-4 rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
						{browseError}
					</p>
				{/if}
				{#if downloadError}
					<p role="alert" class="mt-4 rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
						{downloadError}
					</p>
				{/if}

				{#if browseLoading}
					<p role="status" class="py-10 text-center text-sm text-muted-foreground">Loading folder...</p>
				{:else if listing && selectedRoot}
					{#if listing.entries.length === 0}
						<p class="py-10 text-center text-sm text-muted-foreground">This folder is empty.</p>
					{:else}
						<ul class="mt-2 divide-y divide-border/60">
							{#each listing.entries as entry (entry.name)}
								<li class="flex min-w-0 items-center gap-3 py-3">
									{#if entry.is_dir}
										<button
											type="button"
											onclick={() => openDirectory(entry)}
											class="flex min-w-0 flex-1 items-center gap-3 rounded-md text-left hover:text-primary">
											<FolderOpen size={19} class="shrink-0 text-amber-500" />
											<span class="truncate text-sm font-medium">{entry.name}</span>
										</button>
										<ChevronRight size={16} class="shrink-0 text-muted-foreground" />
									{:else}
										<div class="flex min-w-0 flex-1 items-center gap-3">
											<File size={18} class="shrink-0 text-muted-foreground" />
											<div class="min-w-0 flex-1">
												<p class="truncate text-sm">{entry.name}</p>
												<p class="text-xs text-muted-foreground">{formatBytes(entry.size)}</p>
											</div>
										</div>
										<a
											href={shareDownloadUrl(token, selectedRoot.root, joinSharePath(listing.path, entry.name))}
											download={entry.name}
											aria-label={`Download ${entry.name}`}
											aria-disabled={downloadPending}
											onclick={(event) => void startDownload(event, shareDownloadUrl(token, selectedRoot!.root, joinSharePath(listing!.path, entry.name)), entry.name)}
											class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm hover:bg-accent {downloadPending ? 'pointer-events-none opacity-50' : ''}">
											<Download size={14} /> <span class="hidden sm:inline">Download</span>
										</a>
									{/if}
								</li>
							{/each}
						</ul>
					{/if}
				{:else}
					<ul class="mt-2 divide-y divide-border/60">
						{#each meta.entries as entry (entry.root)}
							<li class="flex min-w-0 items-center gap-3 py-3">
								{#if entry.is_dir}
									<button
										type="button"
										onclick={() => void browse(entry, '')}
										class="flex min-w-0 flex-1 items-center gap-3 rounded-md text-left hover:text-primary">
										<FolderOpen size={20} class="shrink-0 text-amber-500" />
										<span class="truncate text-sm font-medium">{entry.name}</span>
									</button>
									<ChevronRight size={16} class="shrink-0 text-muted-foreground" />
								{:else}
									<div class="flex min-w-0 flex-1 items-center gap-3">
										<File size={18} class="shrink-0 text-muted-foreground" />
										<div class="min-w-0 flex-1">
											<p class="truncate text-sm">{entry.name}</p>
											<p class="text-xs text-muted-foreground">{formatBytes(entry.size)}</p>
										</div>
									</div>
									<a
										href={shareDownloadUrl(token, entry.root, '')}
										download={entry.name}
										aria-label={`Download ${entry.name}`}
										aria-disabled={downloadPending}
										onclick={(event) => void startDownload(event, shareDownloadUrl(token, entry.root, ''), entry.name)}
										class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm hover:bg-accent {downloadPending ? 'pointer-events-none opacity-50' : ''}">
										<Download size={14} /> <span class="hidden sm:inline">Download</span>
									</a>
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			{/if}
		{/if}

		<p class="mt-8 text-center text-xs text-muted-foreground">Powered by NASty</p>
	</div>
</div>
