<script lang="ts">
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { FolderOpen, FolderPlus, ArrowUp, Database } from '@lucide/svelte';

	interface BrowseEntry {
		name: string;
		is_dir: boolean;
	}

	interface Props {
		open: boolean;
		/** Initial path to browse to. Accepts absolute /fs/... or a rel
		 * path under /fs ("first/foo"). Empty string lands on the FS
		 * root listing (all mounted filesystems). */
		initialPath?: string;
		/** Title shown in the modal header. */
		title?: string;
		/** Called with the chosen absolute host path (e.g. /fs/tank/media). */
		onPick: (path: string) => void;
		onClose: () => void;
	}

	let { open, initialPath = '', title = 'Pick host path', onPick, onClose }: Props = $props();

	let currentPath = $state('');
	let entries = $state<BrowseEntry[]>([]);
	let loading = $state(false);
	let mkdirOpen = $state(false);
	let mkdirName = $state('');
	let subvolOpen = $state(false);
	let subvolName = $state('');
	let busy = $state(false);

	const client = getClient();

	/** Strip a leading /fs prefix and any leading slashes so the path
	 * matches what `/api/files/browse` expects. */
	function normalizeRel(input: string): string {
		let p = input.replace(/^\/fs\/?/, '').replace(/^\/+/, '').replace(/\/+$/, '');
		return p;
	}

	function absolute(rel: string): string {
		return rel ? `/fs/${rel}` : '/fs';
	}

	const breadcrumbs = $derived.by(() => {
		const parts = currentPath.split('/').filter(Boolean);
		return parts.map((part, i) => ({
			label: part,
			path: parts.slice(0, i + 1).join('/'),
		}));
	});

	/** Depth-1 paths under /fs (e.g. `tank`) are bcachefs filesystem
	 * roots — that's the only place a new subvolume can be created. */
	const atFilesystemRoot = $derived(
		currentPath.split('/').filter(Boolean).length === 1
	);

	$effect(() => {
		if (open) {
			currentPath = normalizeRel(initialPath);
			mkdirOpen = false;
			subvolOpen = false;
			mkdirName = '';
			subvolName = '';
			browse(currentPath);
		}
	});

	async function browse(path: string) {
		loading = true;
		try {
			const res = await fetch(`/api/files/browse?path=${encodeURIComponent(path)}`);
			const data = await res.json();
			if (res.ok) {
				currentPath = data.path || '';
				entries = (data.entries || []).filter((e: BrowseEntry) => e.is_dir);
			} else {
				// Fall back to root if the path doesn't exist (e.g. user
				// passed a stale initialPath).
				currentPath = '';
				entries = [];
			}
		} catch {
			entries = [];
		} finally {
			loading = false;
		}
	}

	function descend(name: string) {
		const next = currentPath ? `${currentPath}/${name}` : name;
		browse(next);
	}

	function goUp() {
		const parts = currentPath.split('/').filter(Boolean);
		parts.pop();
		browse(parts.join('/'));
	}

	function pickCurrent() {
		// Reject the bare /fs root — it's not a valid bind-mount source.
		if (!currentPath) return;
		onPick(absolute(currentPath));
	}

	async function createFolder() {
		const name = mkdirName.trim();
		if (!name) return;
		if (name.includes('/')) {
			alert('Folder name cannot contain slashes.');
			return;
		}
		busy = true;
		try {
			const path = currentPath ? `${currentPath}/${name}` : name;
			const res = await fetch(`/api/files/mkdir?path=${encodeURIComponent(path)}`, {
				method: 'POST',
			});
			if (!res.ok) {
				const data = await res.json().catch(() => ({}));
				alert(data.error || 'Failed to create folder');
				return;
			}
			mkdirOpen = false;
			mkdirName = '';
			await browse(currentPath);
		} finally {
			busy = false;
		}
	}

	async function createSubvolume() {
		const name = subvolName.trim();
		if (!name || !atFilesystemRoot) return;
		if (name.includes('/')) {
			alert('Subvolume name cannot contain slashes.');
			return;
		}
		busy = true;
		try {
			const filesystem = currentPath.split('/')[0];
			await withToast(
				() => client.call('subvolume.create', {
					filesystem,
					name,
					subvolume_type: 'filesystem',
				}),
				`Subvolume ${name} created`
			);
			subvolOpen = false;
			subvolName = '';
			await browse(currentPath);
		} finally {
			busy = false;
		}
	}
</script>

{#if open}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm" role="button" tabindex="-1" onclick={onClose} onkeydown={(e) => { if (e.key === 'Escape') onClose(); }}>
		<div class="w-full max-w-xl" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<Card>
				<CardContent class="pt-6">
					<h3 class="mb-3 text-lg font-semibold">{title}</h3>

					<!-- Breadcrumb -->
					<div class="mb-2 flex items-center gap-1 text-sm font-mono">
						<button class="text-muted-foreground hover:text-foreground" onclick={() => browse('')}>/fs</button>
						{#each breadcrumbs as crumb}
							<span class="text-muted-foreground/50">/</span>
							<button class="text-muted-foreground hover:text-foreground" onclick={() => browse(crumb.path)}>{crumb.label}</button>
						{/each}
					</div>

					<!-- Up + actions -->
					<div class="mb-3 flex items-center gap-2">
						{#if currentPath}
							<Button variant="outline" size="xs" onclick={goUp} disabled={loading || busy}>
								<ArrowUp size={12} class="mr-1" /> Up
							</Button>
						{/if}
						{#if currentPath}
							<Button variant="outline" size="xs" onclick={() => { mkdirOpen = !mkdirOpen; subvolOpen = false; }} disabled={busy}>
								<FolderPlus size={12} class="mr-1" /> {mkdirOpen ? 'Cancel' : 'New folder'}
							</Button>
						{/if}
						{#if atFilesystemRoot}
							<Button variant="outline" size="xs" onclick={() => { subvolOpen = !subvolOpen; mkdirOpen = false; }} disabled={busy}>
								<Database size={12} class="mr-1" /> {subvolOpen ? 'Cancel' : 'New subvolume'}
							</Button>
						{/if}
					</div>

					{#if mkdirOpen}
						<div class="mb-3 flex gap-2">
							<Input
								bind:value={mkdirName}
								placeholder="New folder name"
								class="h-8 text-xs"
								onkeydown={(e) => { if (e.key === 'Enter') createFolder(); if (e.key === 'Escape') mkdirOpen = false; }}
							/>
							<Button size="xs" onclick={createFolder} disabled={!mkdirName.trim() || busy}>Create</Button>
						</div>
					{/if}
					{#if subvolOpen}
						<div class="mb-3 flex gap-2">
							<Input
								bind:value={subvolName}
								placeholder="New subvolume name"
								class="h-8 text-xs"
								onkeydown={(e) => { if (e.key === 'Enter') createSubvolume(); if (e.key === 'Escape') subvolOpen = false; }}
							/>
							<Button size="xs" onclick={createSubvolume} disabled={!subvolName.trim() || busy}>Create</Button>
						</div>
					{/if}

					<!-- Listing -->
					<div class="mb-4 max-h-72 overflow-y-auto rounded-md border border-border">
						{#if loading}
							<p class="p-3 text-sm text-muted-foreground">Loading...</p>
						{:else if entries.length === 0}
							<p class="p-3 text-sm text-muted-foreground">No subfolders here.</p>
						{:else}
							{#each entries as entry}
								<button
									class="flex w-full items-center gap-2 px-3 py-1.5 text-sm hover:bg-muted/40 text-left"
									onclick={() => descend(entry.name)}>
									<FolderOpen size={14} class="text-yellow-500 shrink-0" />
									<span>{entry.name}</span>
								</button>
							{/each}
						{/if}
					</div>

					<!-- Footer -->
					<div class="flex items-center justify-between gap-3">
						<span class="text-xs text-muted-foreground truncate">
							{currentPath ? absolute(currentPath) : 'Pick a folder under /fs'}
						</span>
						<div class="flex gap-2 shrink-0">
							<Button onclick={pickCurrent} disabled={!currentPath || busy}>
								Use this path
							</Button>
							<Button variant="secondary" onclick={onClose}>Cancel</Button>
						</div>
					</div>
				</CardContent>
			</Card>
		</div>
	</div>
{/if}
