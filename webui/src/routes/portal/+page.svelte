<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient, resetClient } from '$lib/client';
	import { logout } from '$lib/auth';
	import { formatBytes } from '$lib/format';
	import {
		joinPortalPath,
		parentPortalPath,
		portalBreadcrumbs,
		portalFilesUrl,
	} from '$lib/portal';
	import type {
		AuthMe,
		MyActivityEntry,
		PortalBrowseResult,
		PortalFileEntry,
		PortalFileRoot,
	} from '$lib/types';
	import logoLight from '$lib/assets/nasty.svg';
	import logoDark from '$lib/assets/nasty-white.svg';
	import { theme } from '$lib/theme.svelte';
	import {
		Activity,
		ArrowLeft,
		ChevronRight,
		Download,
		File,
		Folder,
		FolderOpen,
		KeyRound,
		LogOut,
		RefreshCw,
		UserRound,
	} from '@lucide/svelte';

	type PortalTab = 'files' | 'activity' | 'account';

	const client = getClient();
	let activeTab: PortalTab = $state('files');
	let identity: AuthMe | null = $state(null);
	let identityError = $state('');

	let roots: PortalFileRoot[] = $state([]);
	let rootsLoading = $state(true);
	let rootsError = $state('');
	let rootsRequest = 0;

	let activeShare: PortalFileRoot | null = $state(null);
	let currentPath = $state('');
	let entries: PortalFileEntry[] = $state([]);
	let browseLoading = $state(false);
	let browseError = $state('');
	let browseRequest = 0;

	let activityEntries: MyActivityEntry[] = $state([]);
	let activityLoading = $state(false);
	let activityLoaded = $state(false);
	let activityError = $state('');
	let activityRequest = 0;

	let newPassword = $state('');
	let confirmPassword = $state('');
	let passwordPending = $state(false);
	let passwordError = $state('');
	let passwordSuccess = $state('');

	const breadcrumbs = $derived(portalBreadcrumbs(currentPath));
	const sortedEntries = $derived.by(() => [...entries].sort((a, b) => {
		if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
		return a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' });
	}));

	function errorMessage(error: unknown, fallback: string): string {
		if (error instanceof Error) return error.message;
		if (error && typeof error === 'object' && 'message' in error) {
			return String((error as { message: unknown }).message);
		}
		return fallback;
	}

	async function fetchJson<T>(url: string): Promise<T> {
		const response = await fetch(url);
		if (!response.ok) {
			const body = await response.json().catch(() => ({}));
			const detail = (body as { error?: string }).error;
			throw new Error(detail ?? `Request failed (HTTP ${response.status})`);
		}
		return response.json() as Promise<T>;
	}

	async function loadIdentity() {
		identityError = '';
		try {
			identity = await client.call<AuthMe>('auth.me');
		} catch (error) {
			identityError = errorMessage(error, 'Unable to load your account.');
		}
	}

	async function loadRoots() {
		const request = ++rootsRequest;
		rootsLoading = true;
		rootsError = '';
		try {
			const result = await fetchJson<PortalFileRoot[]>('/api/user/files/roots');
			if (request !== rootsRequest) return;
			roots = result;
		} catch (error) {
			if (request !== rootsRequest) return;
			rootsError = errorMessage(error, 'Unable to load your shared folders.');
		} finally {
			if (request === rootsRequest) rootsLoading = false;
		}
	}

	async function browse(share: PortalFileRoot, path: string) {
		const request = ++browseRequest;
		activeShare = share;
		currentPath = path;
		browseLoading = true;
		browseError = '';
		try {
			const result = await fetchJson<PortalBrowseResult>(portalFilesUrl('browse', share.id, path));
			if (request !== browseRequest) return;
			currentPath = result.path;
			entries = result.entries;
		} catch (error) {
			if (request !== browseRequest) return;
			entries = [];
			browseError = errorMessage(error, 'Unable to open this folder.');
		} finally {
			if (request === browseRequest) browseLoading = false;
		}
	}

	function openFolder(entry: PortalFileEntry) {
		if (activeShare && entry.is_dir) void browse(activeShare, joinPortalPath(currentPath, entry.name));
	}

	function goBack() {
		if (!activeShare) return;
		if (currentPath) {
			void browse(activeShare, parentPortalPath(currentPath));
		} else {
			browseRequest++;
			activeShare = null;
			entries = [];
			browseError = '';
		}
	}

	async function loadActivity() {
		const request = ++activityRequest;
		activityLoading = true;
		activityError = '';
		try {
			const result = await client.call<MyActivityEntry[]>('audit.mine', { limit: 200 });
			if (request !== activityRequest) return;
			activityEntries = result;
			activityLoaded = true;
		} catch (error) {
			if (request !== activityRequest) return;
			activityError = errorMessage(error, 'Unable to load your activity.');
		} finally {
			if (request === activityRequest) activityLoading = false;
		}
	}

	function selectTab(tab: PortalTab) {
		activeTab = tab;
		if (tab === 'activity' && !activityLoaded && !activityLoading) void loadActivity();
	}

	function formatDate(unixSeconds: number): string {
		const date = new Date(unixSeconds * 1000);
		return Number.isNaN(date.getTime()) ? 'Unknown' : date.toLocaleString();
	}

	function readableEvent(event: string): string {
		return event.replace(/[._-]+/g, ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
	}

	async function changePassword() {
		passwordError = '';
		passwordSuccess = '';
		if (newPassword.length < 8) {
			passwordError = 'Password must be at least 8 characters.';
			return;
		}
		if (newPassword !== confirmPassword) {
			passwordError = 'Passwords do not match.';
			return;
		}
		if (!identity) {
			passwordError = 'Your account information is not available. Refresh and try again.';
			return;
		}
		passwordPending = true;
		try {
			await client.call('auth.change_password', {
				username: identity.username,
				new_password: newPassword,
			});
			newPassword = '';
			confirmPassword = '';
			activityLoaded = false;
			passwordSuccess = 'Your password has been changed.';
		} catch (error) {
			passwordError = errorMessage(error, 'Unable to change your password.');
		} finally {
			passwordPending = false;
		}
	}

	async function signOut() {
		await logout();
		resetClient();
		window.location.assign('/');
	}

	onMount(() => {
		void Promise.all([loadIdentity(), loadRoots()]);
	});
</script>

<svelte:head>
	<title>My Files - NASty</title>
</svelte:head>

<div class="min-h-screen bg-background">
	<header class="sticky top-0 z-30 border-b border-border bg-card/95 backdrop-blur">
		<div class="mx-auto flex h-16 max-w-6xl items-center gap-3 px-4 sm:px-6">
			<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="h-10 w-auto shrink-0" />
			<div class="hidden h-6 w-px bg-border sm:block"></div>
			<p class="hidden text-xs font-medium uppercase tracking-[0.2em] text-muted-foreground sm:block">File portal</p>
			<div class="ml-auto min-w-0 text-right">
				<p class="truncate text-sm font-medium">{identity?.username ?? 'Signed in'}</p>
				{#if identity?.file_principal}<p class="hidden truncate text-xs text-muted-foreground sm:block">{identity.file_principal}</p>{/if}
			</div>
			<button
				type="button"
				onclick={signOut}
				class="flex h-9 items-center gap-2 rounded-lg border border-border px-3 text-sm text-muted-foreground transition-colors hover:border-blue-500/50 hover:text-foreground"
				aria-label="Sign out"
			>
				<LogOut size={16} />
				<span class="hidden sm:inline">Sign out</span>
			</button>
		</div>
	</header>

	<div class="mx-auto max-w-6xl px-4 py-5 sm:px-6 sm:py-8">
		<div class="mb-6 flex gap-1 overflow-x-auto rounded-xl border border-border bg-card p-1.5 sm:w-fit" aria-label="Portal navigation">
			<button type="button" onclick={() => selectTab('files')} aria-current={activeTab === 'files' ? 'page' : undefined} class="flex min-w-fit flex-1 items-center justify-center gap-2 rounded-lg px-4 py-2 text-sm font-medium transition-colors sm:flex-none {activeTab === 'files' ? 'bg-blue-500/15 text-blue-500 dark:text-blue-400' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}">
				<FolderOpen size={16} /> Files
			</button>
			<button type="button" onclick={() => selectTab('activity')} aria-current={activeTab === 'activity' ? 'page' : undefined} class="flex min-w-fit flex-1 items-center justify-center gap-2 rounded-lg px-4 py-2 text-sm font-medium transition-colors sm:flex-none {activeTab === 'activity' ? 'bg-blue-500/15 text-blue-500 dark:text-blue-400' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}">
				<Activity size={16} /> Activity
			</button>
			<button type="button" onclick={() => selectTab('account')} aria-current={activeTab === 'account' ? 'page' : undefined} class="flex min-w-fit flex-1 items-center justify-center gap-2 rounded-lg px-4 py-2 text-sm font-medium transition-colors sm:flex-none {activeTab === 'account' ? 'bg-blue-500/15 text-blue-500 dark:text-blue-400' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}">
				<UserRound size={16} /> Account
			</button>
		</div>

		{#if activeTab === 'files'}
			<section aria-labelledby="files-title">
				<div class="mb-5">
					<p class="mb-1 font-mono text-[0.65rem] uppercase tracking-[0.24em] text-blue-500 dark:text-blue-400">Your storage</p>
					<h1 id="files-title" class="text-2xl font-semibold tracking-tight sm:text-3xl">{activeShare?.name ?? 'Shared folders'}</h1>
					<p class="mt-1 text-sm text-muted-foreground">{activeShare ? 'Browse and download files available to your account.' : 'Choose a shared folder to get started.'}</p>
				</div>

				{#if !activeShare}
					{#if rootsLoading}
						<div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3" aria-live="polite" aria-label="Loading shared folders">
							{#each [1, 2, 3] as item}
								<div class="h-28 animate-pulse rounded-xl border border-border bg-card" aria-hidden="true"></div>
							{/each}
						</div>
					{:else if rootsError}
						<div class="rounded-xl border border-destructive/30 bg-destructive/5 p-5" role="alert">
							<p class="text-sm text-destructive">{rootsError}</p>
							<button type="button" onclick={loadRoots} class="mt-3 flex items-center gap-2 rounded-lg border border-border px-3 py-2 text-sm hover:bg-accent"><RefreshCw size={15} /> Try again</button>
						</div>
					{:else if roots.length === 0}
						<div class="rounded-xl border border-dashed border-border bg-card/40 px-6 py-14 text-center">
							<FolderOpen size={32} class="mx-auto mb-3 text-muted-foreground/60" />
							<h2 class="font-medium">No shared folders</h2>
							<p class="mt-1 text-sm text-muted-foreground">No SMB shares are currently available to your account.</p>
						</div>
					{:else}
						<div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
							{#each roots as root (root.id)}
								<button type="button" onclick={() => browse(root, '')} class="group flex min-h-28 items-center gap-4 rounded-xl border border-border bg-card p-5 text-left transition-all hover:-translate-y-0.5 hover:border-blue-500/50 hover:shadow-lg">
									<span class="flex h-12 w-12 shrink-0 items-center justify-center rounded-xl bg-blue-500/10 text-blue-500 dark:text-blue-400"><Folder size={25} /></span>
									<span class="min-w-0 flex-1">
										<span class="block truncate font-semibold">{root.name}</span>
										<span class="mt-1 block text-xs text-muted-foreground">Open shared folder</span>
									</span>
									<ChevronRight size={18} class="shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5 group-hover:text-blue-500" />
								</button>
							{/each}
						</div>
					{/if}
				{:else}
					<div class="overflow-hidden rounded-xl border border-border bg-card">
						<div class="flex min-h-14 items-center gap-2 border-b border-border px-3 sm:px-4">
							<button type="button" onclick={goBack} class="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg text-muted-foreground hover:bg-accent hover:text-foreground" aria-label={currentPath ? 'Go to parent folder' : 'Back to shared folders'}><ArrowLeft size={18} /></button>
							<nav class="flex min-w-0 items-center gap-1 overflow-x-auto whitespace-nowrap text-sm" aria-label="Folder breadcrumbs">
								<button type="button" onclick={() => activeShare && browse(activeShare, '')} class="max-w-44 truncate rounded-md px-2 py-1 font-medium hover:bg-accent">{activeShare.name}</button>
								{#each breadcrumbs as crumb (crumb.path)}
									<ChevronRight size={14} class="shrink-0 text-muted-foreground" />
									<button type="button" onclick={() => activeShare && browse(activeShare, crumb.path)} class="max-w-44 truncate rounded-md px-2 py-1 text-muted-foreground hover:bg-accent hover:text-foreground" aria-current={crumb.path === currentPath ? 'location' : undefined}>{crumb.label}</button>
								{/each}
							</nav>
						</div>

						{#if browseLoading}
							<div class="space-y-1 p-3" aria-live="polite" aria-label="Loading folder">
								{#each [1, 2, 3, 4] as item}
									<div class="h-12 animate-pulse rounded-lg bg-muted/60" aria-hidden="true"></div>
								{/each}
							</div>
						{:else if browseError}
							<div class="px-5 py-12 text-center" role="alert">
								<p class="text-sm text-destructive">{browseError}</p>
								<button type="button" onclick={() => activeShare && browse(activeShare, currentPath)} class="mx-auto mt-3 flex items-center gap-2 rounded-lg border border-border px-3 py-2 text-sm hover:bg-accent"><RefreshCw size={15} /> Try again</button>
							</div>
						{:else if entries.length === 0}
							<div class="px-5 py-14 text-center">
								<FolderOpen size={30} class="mx-auto mb-3 text-muted-foreground/50" />
								<p class="font-medium">This folder is empty</p>
								<p class="mt-1 text-sm text-muted-foreground">There are no files or folders here.</p>
							</div>
						{:else}
							<div class="grid grid-cols-[minmax(0,1fr)_auto] border-b border-border bg-muted/30 px-4 py-2 text-[0.65rem] font-medium uppercase tracking-wider text-muted-foreground sm:grid-cols-[minmax(0,1fr)_7rem_12rem_3rem]">
								<span>Name</span><span class="hidden sm:block">Size</span><span class="hidden sm:block">Modified</span><span class="sr-only">Action</span>
							</div>
							<div class="divide-y divide-border/70">
								{#each sortedEntries as entry (entry.name)}
									<div class="grid min-h-14 grid-cols-[minmax(0,1fr)_auto] items-center gap-2 px-3 transition-colors hover:bg-accent/40 sm:grid-cols-[minmax(0,1fr)_7rem_12rem_3rem] sm:px-4">
										{#if entry.is_dir}
											<button type="button" onclick={() => openFolder(entry)} class="flex min-w-0 items-center gap-3 rounded-md py-2 text-left font-medium hover:text-blue-500">
												<Folder size={19} class="shrink-0 text-blue-500 dark:text-blue-400" />
												<span class="min-w-0">
													<span class="block truncate">{entry.name}</span>
													<span class="mt-0.5 block text-xs font-normal text-muted-foreground sm:hidden">{formatDate(entry.modified)}</span>
												</span>
											</button>
										{:else}
											<div class="flex min-w-0 items-center gap-3 py-2">
												<File size={19} class="shrink-0 text-muted-foreground" />
												<span class="min-w-0">
													<span class="block truncate">{entry.name}</span>
													<span class="mt-0.5 block text-xs text-muted-foreground sm:hidden">{formatBytes(entry.size)} | {formatDate(entry.modified)}</span>
												</span>
											</div>
										{/if}
										<span class="hidden text-xs tabular-nums text-muted-foreground sm:block">{entry.is_dir ? '-' : formatBytes(entry.size)}</span>
										<time class="hidden text-xs text-muted-foreground sm:block" datetime={new Date(entry.modified * 1000).toISOString()}>{formatDate(entry.modified)}</time>
										{#if entry.is_dir}
											<ChevronRight size={16} class="justify-self-center text-muted-foreground" />
										{:else}
											<a href={portalFilesUrl('content', activeShare.id, joinPortalPath(currentPath, entry.name))} download={entry.name} class="flex h-9 w-9 items-center justify-center rounded-lg text-muted-foreground hover:bg-accent hover:text-foreground" aria-label={`Download ${entry.name}`} title={`Download ${entry.name}`}><Download size={17} /></a>
										{/if}
									</div>
								{/each}
							</div>
						{/if}
					</div>
				{/if}
			</section>
		{:else if activeTab === 'activity'}
			<section aria-labelledby="activity-title">
				<div class="mb-5 flex items-end justify-between gap-4">
					<div>
						<p class="mb-1 font-mono text-[0.65rem] uppercase tracking-[0.24em] text-blue-500 dark:text-blue-400">Audit trail</p>
						<h1 id="activity-title" class="text-2xl font-semibold tracking-tight sm:text-3xl">My activity</h1>
						<p class="mt-1 text-sm text-muted-foreground">Recent actions recorded for your account.</p>
					</div>
					<button type="button" onclick={loadActivity} disabled={activityLoading} class="flex h-9 items-center gap-2 rounded-lg border border-border px-3 text-sm text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-50" aria-label="Refresh activity"><RefreshCw size={15} class={activityLoading ? 'animate-spin' : ''} /><span class="hidden sm:inline">Refresh</span></button>
				</div>
				<div class="overflow-hidden rounded-xl border border-border bg-card">
					{#if activityLoading && !activityLoaded}
						<div class="space-y-2 p-4" aria-live="polite" aria-label="Loading activity">{#each [1, 2, 3, 4] as item}<div class="h-16 animate-pulse rounded-lg bg-muted/60" aria-hidden="true"></div>{/each}</div>
					{:else if activityError}
						<div class="px-5 py-12 text-center" role="alert"><p class="text-sm text-destructive">{activityError}</p><button type="button" onclick={loadActivity} class="mt-3 rounded-lg border border-border px-3 py-2 text-sm hover:bg-accent">Try again</button></div>
					{:else if activityEntries.length === 0}
						<div class="px-5 py-14 text-center"><Activity size={30} class="mx-auto mb-3 text-muted-foreground/50" /><p class="font-medium">No activity yet</p><p class="mt-1 text-sm text-muted-foreground">Actions for your account will appear here.</p></div>
					{:else}
						<ul class="divide-y divide-border">
							{#each activityEntries as entry, index (`${entry.ts}-${entry.event}-${index}`)}
								<li class="grid gap-2 px-4 py-4 sm:grid-cols-[10rem_minmax(0,1fr)] sm:px-5">
									<time class="text-xs tabular-nums text-muted-foreground" datetime={new Date(entry.ts * 1000).toISOString()}>{formatDate(entry.ts)}</time>
									<div class="min-w-0"><p class="text-sm font-medium">{readableEvent(entry.event)}</p>{#if entry.detail}<p class="mt-1 break-words font-mono text-xs leading-relaxed text-muted-foreground">{entry.detail}</p>{/if}{#if entry.ip}<p class="mt-1 text-[0.68rem] text-muted-foreground/70">From {entry.ip}</p>{/if}</div>
								</li>
							{/each}
						</ul>
					{/if}
				</div>
			</section>
		{:else}
			<section aria-labelledby="account-title">
				<div class="mb-5">
					<p class="mb-1 font-mono text-[0.65rem] uppercase tracking-[0.24em] text-blue-500 dark:text-blue-400">Profile and security</p>
					<h1 id="account-title" class="text-2xl font-semibold tracking-tight sm:text-3xl">My account</h1>
					<p class="mt-1 text-sm text-muted-foreground">Review your identity and update your password.</p>
				</div>
				{#if identityError}<div class="mb-4 rounded-xl border border-destructive/30 bg-destructive/5 p-4 text-sm text-destructive" role="alert">{identityError}</div>{/if}
				<div class="grid gap-4 lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]">
					<div class="rounded-xl border border-border bg-card p-5 sm:p-6">
						<div class="mb-5 flex h-12 w-12 items-center justify-center rounded-xl bg-blue-500/10 text-blue-500 dark:text-blue-400"><UserRound size={24} /></div>
						<h2 class="font-semibold">Signed-in identity</h2>
						<dl class="mt-5 space-y-4 text-sm">
							<div><dt class="text-xs uppercase tracking-wider text-muted-foreground">Username</dt><dd class="mt-1 font-medium">{identity?.username ?? 'Loading...'}</dd></div>
							<div><dt class="text-xs uppercase tracking-wider text-muted-foreground">File principal</dt><dd class="mt-1 break-all font-mono text-xs">{identity?.file_principal ?? 'Not available'}</dd></div>
						</dl>
					</div>
					<div class="rounded-xl border border-border bg-card p-5 sm:p-6">
						<div class="mb-4 flex items-center gap-3"><span class="flex h-10 w-10 items-center justify-center rounded-xl bg-muted text-muted-foreground"><KeyRound size={20} /></span><div><h2 class="font-semibold">Change password</h2><p class="text-xs text-muted-foreground">Use at least 8 characters.</p></div></div>
						<form onsubmit={(event) => { event.preventDefault(); void changePassword(); }} class="space-y-4">
							<label class="block"><span class="text-sm font-medium">New password</span><input type="password" bind:value={newPassword} autocomplete="new-password" class="mt-1.5 h-10 w-full rounded-lg border border-input bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-blue-500/30" /></label>
							<label class="block"><span class="text-sm font-medium">Confirm password</span><input type="password" bind:value={confirmPassword} autocomplete="new-password" class="mt-1.5 h-10 w-full rounded-lg border border-input bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-blue-500/30" /></label>
							{#if passwordError}<p class="rounded-lg border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm text-destructive" role="alert">{passwordError}</p>{/if}
							{#if passwordSuccess}<p class="rounded-lg border border-emerald-500/30 bg-emerald-500/10 px-3 py-2 text-sm text-emerald-600 dark:text-emerald-400" role="status">{passwordSuccess}</p>{/if}
							<button type="submit" disabled={passwordPending} class="h-10 rounded-lg bg-primary px-4 text-sm font-medium text-primary-foreground transition-opacity hover:opacity-90 disabled:opacity-50">{passwordPending ? 'Changing...' : 'Change password'}</button>
						</form>
					</div>
				</div>
			</section>
		{/if}
	</div>
</div>
