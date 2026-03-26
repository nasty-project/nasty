<script lang="ts">
	import { onMount } from 'svelte';
	import { getToken } from '$lib/auth';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Button } from '$lib/components/ui/button';
	import { FolderOpen, File, ArrowUp } from '@lucide/svelte';

	interface FileEntry {
		name: string;
		is_dir: boolean;
		size: number;
		modified: number;
	}

	let currentPath = $state('');
	let entries: FileEntry[] = $state([]);
	let loading = $state(true);

	onMount(() => browse(''));

	async function browse(path: string) {
		loading = true;
		try {
			const token = getToken();
			const res = await fetch(`/api/files/browse?path=${encodeURIComponent(path)}`, {
				headers: { 'Authorization': `Bearer ${token}` },
			});
			const data = await res.json();
			if (res.ok) {
				currentPath = data.path || '';
				entries = data.entries || [];
			}
		} catch { /* ignore */ }
		loading = false;
	}

	function navigateTo(entry: FileEntry) {
		if (entry.is_dir) {
			browse(currentPath ? `${currentPath}/${entry.name}` : entry.name);
		}
	}

	function goUp() {
		const parts = currentPath.split('/').filter(Boolean);
		parts.pop();
		browse(parts.join('/'));
	}

	function formatSize(bytes: number): string {
		if (bytes === 0) return '—';
		if (bytes < 1024) return `${bytes} B`;
		if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
		if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
		return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
	}

	function formatDate(epoch: number): string {
		if (!epoch) return '—';
		return new Date(epoch * 1000).toLocaleString();
	}

	const breadcrumbs = $derived.by(() => {
		const parts = currentPath.split('/').filter(Boolean);
		return parts.map((part, i) => ({
			label: part,
			path: parts.slice(0, i + 1).join('/'),
		}));
	});
</script>

<!-- Breadcrumbs -->
<div class="mb-4 flex items-center justify-between gap-4">
	<div class="flex items-center gap-1 text-sm font-mono">
		<button class="text-muted-foreground hover:text-foreground transition-colors" onclick={() => browse('')}>
			/fs
		</button>
		{#each breadcrumbs as crumb}
			<span class="text-muted-foreground/50">/</span>
			<button class="text-muted-foreground hover:text-foreground transition-colors" onclick={() => browse(crumb.path)}>
				{crumb.label}
			</button>
		{/each}
	</div>
	{#if currentPath}
		<Button variant="outline" size="sm" onclick={goUp}>
			<ArrowUp size={14} class="mr-1" /> Up
		</Button>
	{/if}
</div>

<!-- File listing -->
{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if entries.length === 0}
	<Card>
		<CardContent class="py-8 text-center text-muted-foreground">
			{currentPath ? 'Empty directory' : 'No filesystems mounted'}
		</CardContent>
	</Card>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr class="border-b-2 border-border">
				<th class="p-3 text-left text-xs uppercase text-muted-foreground">Name</th>
				<th class="p-3 text-right text-xs uppercase text-muted-foreground">Size</th>
				<th class="p-3 text-right text-xs uppercase text-muted-foreground">Modified</th>
			</tr>
		</thead>
		<tbody>
			{#each entries as entry}
				<tr class="border-b border-border hover:bg-muted/30 transition-colors">
					<td class="p-3">
						{#if entry.is_dir}
							<button class="flex items-center gap-2 hover:text-primary transition-colors" onclick={() => navigateTo(entry)}>
								<FolderOpen size={16} class="text-yellow-500 shrink-0" />
								<span class="font-medium">{entry.name}</span>
							</button>
						{:else}
							<div class="flex items-center gap-2">
								<File size={16} class="text-muted-foreground shrink-0" />
								<span>{entry.name}</span>
							</div>
						{/if}
					</td>
					<td class="p-3 text-right text-muted-foreground tabular-nums">{entry.is_dir ? '—' : formatSize(entry.size)}</td>
					<td class="p-3 text-right text-muted-foreground text-xs tabular-nums">{formatDate(entry.modified)}</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}
