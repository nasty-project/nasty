<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/stores';
	import { theme } from '$lib/theme.svelte';
	import logoLight from '$lib/assets/nasty.svg';
	import logoDark from '$lib/assets/nasty-white.svg';
	import { File, Folder, Download, Lock } from '@lucide/svelte';

	// Public, unauthenticated guest landing page for a share link. It talks
	// to the engine only through /api/public/share/* — no session, no RPC.
	// The root layout renders this route bare (no sidebar) via its
	// `isPublicShare` bypass.

	interface PublicEntry {
		name: string;
		is_dir: boolean;
		size: number;
	}
	interface ShareMeta {
		entries: PublicEntry[];
		password_required: boolean;
		expires_at: number | null;
	}

	const token = $derived($page.params.token);

	let loading = $state(true);
	let meta = $state<ShareMeta | null>(null);
	let notAvailable = $state(false);

	// Password unlock state.
	let unlocked = $state(false);
	let password = $state('');
	let unlocking = $state(false);
	let unlockError = $state('');

	const needsUnlock = $derived(!!meta?.password_required && !unlocked);

	function fmtSize(n: number): string {
		if (n < 1024) return `${n} B`;
		const units = ['KB', 'MB', 'GB', 'TB'];
		let v = n / 1024;
		let i = 0;
		while (v >= 1024 && i < units.length - 1) {
			v /= 1024;
			i++;
		}
		return `${v.toFixed(1)} ${units[i]}`;
	}

	function fmtExpiry(secs: number | null): string {
		if (!secs) return '';
		return new Date(secs * 1000).toLocaleString();
	}

	async function loadMeta() {
		loading = true;
		notAvailable = false;
		try {
			const res = await fetch(`/api/public/share/${token}`);
			if (!res.ok) {
				notAvailable = true;
				return;
			}
			meta = (await res.json()) as ShareMeta;
			unlocked = !meta.password_required;
		} catch {
			notAvailable = true;
		} finally {
			loading = false;
		}
	}

	async function unlock(e: Event) {
		e.preventDefault();
		if (!password) return;
		unlocking = true;
		unlockError = '';
		try {
			const res = await fetch(`/api/public/share/${token}/unlock`, {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ password })
			});
			if (res.ok) {
				// The grant cookie is now set; downloads will carry it.
				unlocked = true;
				password = '';
			} else if (res.status === 429) {
				unlockError = 'Too many attempts. Please try again later.';
			} else if (res.status === 404) {
				notAvailable = true;
			} else {
				unlockError = 'Incorrect password.';
			}
		} catch {
			unlockError = 'Something went wrong. Please try again.';
		} finally {
			unlocking = false;
		}
	}

	// Per-file download. Shares created from the WebUI have a single root, so
	// a file entry downloads with no path (the share root *is* the file). The
	// grant cookie, if any, rides along automatically (same-origin, same-site).
	function downloadUrl(): string {
		return `/api/public/share/${token}/download`;
	}

	// Folder shares download as a streamed ZIP of the whole folder.
	function zipUrl(): string {
		return `/api/public/share/${token}/zip`;
	}

	onMount(loadMeta);
</script>

<svelte:head>
	<title>Shared files — NASty</title>
</svelte:head>

<div class="flex min-h-screen items-center justify-center bg-background p-6">
	<div class="w-full max-w-lg rounded-xl border border-border bg-card p-8">
		<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="mb-6 h-16 mx-auto" />

		{#if loading}
			<p class="text-center text-sm text-muted-foreground">Loading…</p>
		{:else if notAvailable}
			<h1 class="text-center text-lg font-semibold">This share is not available</h1>
			<p class="mt-2 text-center text-sm text-muted-foreground">
				The link may have expired, been revoked, or reached its download limit.
			</p>
		{:else if meta}
			<h1 class="text-center text-lg font-semibold">Shared with you</h1>
			{#if meta.expires_at}
				<p class="mt-1 text-center text-xs text-muted-foreground">
					Available until {fmtExpiry(meta.expires_at)}
				</p>
			{/if}

			{#if needsUnlock}
				<form onsubmit={unlock} class="mx-auto mt-6 max-w-xs">
					<div class="mb-2 flex items-center justify-center gap-2 text-sm text-muted-foreground">
						<Lock size={14} /> This share is password protected
					</div>
					<input
						type="password"
						bind:value={password}
						placeholder="Password"
						autocomplete="off"
						class="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm" />
					{#if unlockError}
						<p class="mt-2 text-sm text-destructive">{unlockError}</p>
					{/if}
					<button
						type="submit"
						disabled={unlocking || !password}
						class="mt-3 h-9 w-full rounded-md bg-primary text-sm font-medium text-primary-foreground disabled:opacity-50">
						{unlocking ? 'Unlocking…' : 'Unlock'}
					</button>
				</form>
			{:else}
				<ul class="mt-6 divide-y divide-border/60">
					{#each meta.entries as entry (entry.name)}
						<li class="flex items-center justify-between gap-3 py-3">
							<div class="flex min-w-0 items-center gap-2">
								{#if entry.is_dir}
									<Folder size={16} class="shrink-0 text-muted-foreground" />
								{:else}
									<File size={16} class="shrink-0 text-muted-foreground" />
								{/if}
								<span class="truncate text-sm">{entry.name}</span>
								{#if !entry.is_dir}
									<span class="shrink-0 text-xs text-muted-foreground">{fmtSize(entry.size)}</span>
								{/if}
							</div>
							{#if entry.is_dir}
								<a
									href={zipUrl()}
									download={`${entry.name}.zip`}
									class="inline-flex shrink-0 items-center gap-1 rounded-md border border-border px-3 py-1.5 text-sm hover:bg-accent">
									<Download size={14} /> Download as ZIP
								</a>
							{:else}
								<a
									href={downloadUrl()}
									download={entry.name}
									class="inline-flex shrink-0 items-center gap-1 rounded-md border border-border px-3 py-1.5 text-sm hover:bg-accent">
									<Download size={14} /> Download
								</a>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		{/if}

		<p class="mt-8 text-center text-xs text-muted-foreground">Powered by NASty</p>
	</div>
</div>
