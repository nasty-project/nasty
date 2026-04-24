<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { getToken } from '$lib/auth';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Button } from '$lib/components/ui/button';
	import { FolderOpen, File, ArrowUp, Upload, FolderPlus, Trash2, Image, Film, Music, FileText, Download } from '@lucide/svelte';

	interface FileEntry {
		name: string;
		is_dir: boolean;
		size: number;
		modified: number;
	}

	let currentPath = $state('');
	let entries: FileEntry[] = $state([]);
	let loading = $state(true);
	let showHidden = $state(false);

	// Preview state
	let previewFile: FileEntry | null = $state(null);

	const IMAGE_EXT = new Set(['jpg', 'jpeg', 'png', 'gif', 'webp', 'svg', 'bmp', 'avif', 'ico']);
	const VIDEO_EXT = new Set(['mp4', 'm4v', 'webm', 'ogv', 'mkv', 'avi', 'mov']);
	const AUDIO_EXT = new Set(['mp3', 'ogg', 'oga', 'wav', 'flac', 'aac', 'm4a', 'wma', 'opus']);
	const TEXT_EXT = new Set(['txt', 'log', 'md', 'csv', 'conf', 'cfg', 'ini', 'yml', 'yaml', 'toml', 'json', 'xml', 'html', 'htm', 'css', 'js', 'ts', 'rs', 'py', 'sh', 'bash', 'nix', 'c', 'h', 'cpp', 'go', 'java', 'rb', 'php', 'sql', 'dockerfile']);
	const PDF_EXT = new Set(['pdf']);

	function fileExt(name: string): string {
		const dot = name.lastIndexOf('.');
		return dot >= 0 ? name.slice(dot + 1).toLowerCase() : '';
	}

	function fileCategory(name: string): 'image' | 'video' | 'audio' | 'pdf' | 'text' | 'other' {
		const ext = fileExt(name);
		if (IMAGE_EXT.has(ext)) return 'image';
		if (VIDEO_EXT.has(ext)) return 'video';
		if (AUDIO_EXT.has(ext)) return 'audio';
		if (PDF_EXT.has(ext)) return 'pdf';
		if (TEXT_EXT.has(ext)) return 'text';
		return 'other';
	}

	function isPreviewable(entry: FileEntry): boolean {
		return !entry.is_dir && fileCategory(entry.name) !== 'other';
	}

	function contentUrl(entry: FileEntry): string {
		const path = currentPath ? `${currentPath}/${entry.name}` : entry.name;
		const token = getToken();
		return `/api/files/content?path=${encodeURIComponent(path)}&token=${encodeURIComponent(token ?? '')}`;
	}

	function openPreview(entry: FileEntry) {
		if (isPreviewable(entry)) {
			previewFile = entry;
		} else {
			// Non-previewable files — trigger download
			const a = document.createElement('a');
			a.href = contentUrl(entry);
			a.download = entry.name;
			a.click();
		}
	}

	let previewText = $state('');
	async function loadTextPreview(entry: FileEntry) {
		try {
			const token = getToken();
			const path = currentPath ? `${currentPath}/${entry.name}` : entry.name;
			const res = await fetch(`/api/files/content?path=${encodeURIComponent(path)}`, {
				headers: { 'Authorization': `Bearer ${token}` },
			});
			previewText = await res.text();
		} catch {
			previewText = 'Failed to load file content';
		}
	}

	$effect(() => {
		if (previewFile && fileCategory(previewFile.name) === 'text') {
			loadTextPreview(previewFile);
		} else {
			previewText = '';
		}
	});

	const visibleEntries = $derived(
		showHidden ? entries : entries.filter(e => !e.name.startsWith('.'))
	);

	// Upload state
	let uploading = $state(false);
	let uploadProgress = $state(0);
	let uploadName = $state('');

	// Mkdir state
	let showMkdir = $state(false);
	let newDirName = $state('');

	// Delete confirmation
	let deleteTarget: FileEntry | null = $state(null);

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

	// True for top-level dirs (filesystems) — don't allow deleting them here
	const isRoot = $derived(currentPath.split('/').filter(Boolean).length === 0);

	function triggerUpload() {
		const input = document.createElement('input');
		input.type = 'file';
		input.multiple = true;
		input.onchange = async () => {
			if (!input.files?.length) return;
			for (const file of input.files) {
				await uploadFile(file);
			}
		};
		input.click();
	}

	async function uploadFile(file: globalThis.File) {
		uploading = true;
		uploadProgress = 0;
		uploadName = file.name;

		const token = getToken();
		const form = new FormData();
		form.append('file', file);

		try {
			await new Promise<void>((resolve, reject) => {
				const xhr = new XMLHttpRequest();
				xhr.open('POST', `/api/files/upload?path=${encodeURIComponent(currentPath)}`);
				xhr.setRequestHeader('Authorization', `Bearer ${token}`);
				xhr.upload.onprogress = (e) => {
					if (e.lengthComputable) uploadProgress = Math.round((e.loaded / e.total) * 100);
				};
				xhr.onload = () => {
					if (xhr.status === 200) resolve();
					else reject(new Error(JSON.parse(xhr.responseText)?.error || 'Upload failed'));
				};
				xhr.onerror = () => reject(new Error('Network error'));
				xhr.send(form);
			});
		} catch (e: unknown) {
			alert(e instanceof Error ? e.message : 'Upload failed');
		}

		uploading = false;
		uploadProgress = 0;
		uploadName = '';
		await browse(currentPath);
	}

	async function createDir() {
		if (!newDirName.trim()) return;
		const path = currentPath ? `${currentPath}/${newDirName.trim()}` : newDirName.trim();
		const token = getToken();
		const res = await fetch(`/api/files/mkdir?path=${encodeURIComponent(path)}`, {
			method: 'POST',
			headers: { 'Authorization': `Bearer ${token}` },
		});
		if (!res.ok) {
			const data = await res.json();
			alert(data.error || 'Failed to create directory');
			return;
		}
		showMkdir = false;
		newDirName = '';
		await browse(currentPath);
	}

	async function confirmDelete() {
		if (!deleteTarget) return;
		const path = currentPath ? `${currentPath}/${deleteTarget.name}` : deleteTarget.name;
		const token = getToken();
		const res = await fetch(`/api/files?path=${encodeURIComponent(path)}`, {
			method: 'DELETE',
			headers: { 'Authorization': `Bearer ${token}` },
		});
		if (!res.ok) {
			const data = await res.json();
			alert(data.error || 'Failed to delete');
		}
		deleteTarget = null;
		await browse(currentPath);
	}
</script>

<!-- Toolbar -->
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
	<div class="flex items-center gap-2">
		{#if currentPath}
			<Button variant="outline" size="sm" onclick={goUp}>
				<ArrowUp size={14} class="mr-1" /> Up
			</Button>
		{/if}
		{#if !isRoot}
			<Button variant="outline" size="sm" onclick={triggerUpload} disabled={uploading}>
				<Upload size={14} class="mr-1" /> Upload
			</Button>
		{/if}
		{#if !isRoot}
			<Button variant="outline" size="sm" onclick={() => { showMkdir = true; newDirName = ''; }}>
				<FolderPlus size={14} class="mr-1" /> New Folder
			</Button>
		{/if}
		<label class="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
			<input type="checkbox" bind:checked={showHidden} class="h-3.5 w-3.5" />
			Show hidden
		</label>
	</div>
</div>

<!-- Upload progress -->
{#if uploading}
	<div class="mb-4 rounded-md border border-border p-3">
		<div class="mb-1 flex items-center justify-between text-sm">
			<span class="text-muted-foreground">Uploading <span class="font-mono">{uploadName}</span></span>
			<span class="tabular-nums">{uploadProgress}%</span>
		</div>
		<div class="h-2 w-full overflow-hidden rounded-full bg-muted">
			<div class="h-full rounded-full bg-primary transition-all" style="width: {uploadProgress}%"></div>
		</div>
	</div>
{/if}

<!-- New folder inline -->
{#if showMkdir}
	<div class="mb-4 flex items-center gap-2">
		<input type="text" bind:value={newDirName} placeholder="Folder name"
			class="h-9 w-64 rounded-md border border-input bg-transparent px-3 text-sm"
			onkeydown={(e) => { if (e.key === 'Enter') createDir(); if (e.key === 'Escape') showMkdir = false; }} />
		<Button size="sm" onclick={createDir} disabled={!newDirName.trim()}>Create</Button>
		<Button variant="secondary" size="sm" onclick={() => showMkdir = false}>Cancel</Button>
	</div>
{/if}

<!-- File listing -->
{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if visibleEntries.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">{currentPath ? 'Empty directory' : 'No filesystems mounted.'}</p>
		{#if !currentPath}
			<Button size="sm" class="mt-2" onclick={() => goto('/filesystems')}>Go to Filesystems</Button>
		{/if}
	</div>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr class="border-b-2 border-border">
				<th class="p-3 text-left text-xs uppercase text-muted-foreground">Name</th>
				<th class="p-3 text-right text-xs uppercase text-muted-foreground">Size</th>
				<th class="p-3 text-right text-xs uppercase text-muted-foreground">Modified</th>
				{#if !isRoot}
					<th class="w-10"></th>
				{/if}
			</tr>
		</thead>
		<tbody>
			{#each visibleEntries as entry}
				<tr class="border-b border-border hover:bg-muted/30 transition-colors group">
					<td class="p-3">
						{#if entry.is_dir}
							<button class="flex items-center gap-2 hover:text-primary transition-colors" onclick={() => navigateTo(entry)}>
								<FolderOpen size={16} class="text-yellow-500 shrink-0" />
								<span class="font-medium">{entry.name}</span>
							</button>
						{:else}
							{@const cat = fileCategory(entry.name)}
							<button class="flex items-center gap-2 hover:text-primary transition-colors text-left" onclick={() => openPreview(entry)}>
								{#if cat === 'image'}
									<Image size={16} class="text-blue-400 shrink-0" />
								{:else if cat === 'video'}
									<Film size={16} class="text-purple-400 shrink-0" />
								{:else if cat === 'audio'}
									<Music size={16} class="text-green-400 shrink-0" />
								{:else if cat === 'pdf' || cat === 'text'}
									<FileText size={16} class="text-orange-400 shrink-0" />
								{:else}
									<File size={16} class="text-muted-foreground shrink-0" />
								{/if}
								<span class={isPreviewable(entry) ? '' : 'text-foreground'}>{entry.name}</span>
							</button>
						{/if}
					</td>
					<td class="p-3 text-right text-muted-foreground tabular-nums">{entry.is_dir ? '—' : formatSize(entry.size)}</td>
					<td class="p-3 text-right text-muted-foreground text-xs tabular-nums">{formatDate(entry.modified)}</td>
					{#if !isRoot}
						<td class="p-3 text-right">
							<div class="flex items-center justify-end gap-2 opacity-0 group-hover:opacity-100">
								{#if !entry.is_dir}
									<a
										href={contentUrl(entry)}
										download={entry.name}
										class="text-muted-foreground/40 hover:text-foreground transition-colors"
										title="Download">
										<Download size={14} />
									</a>
								{/if}
								<button
									class="text-muted-foreground/40 hover:text-destructive transition-colors"
									onclick={() => deleteTarget = entry}
									title="Delete">
									<Trash2 size={14} />
								</button>
							</div>
						</td>
					{/if}
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<!-- Delete confirmation -->
{#if deleteTarget}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<Card class="w-full max-w-sm">
			<CardContent class="pt-6">
				<h3 class="mb-2 text-lg font-semibold">Delete {deleteTarget.is_dir ? 'folder' : 'file'}</h3>
				<p class="mb-4 text-sm text-muted-foreground">
					{#if deleteTarget.is_dir}
						Delete <span class="font-mono font-medium text-foreground">{deleteTarget.name}</span> and all its contents? This cannot be undone.
					{:else}
						Delete <span class="font-mono font-medium text-foreground">{deleteTarget.name}</span>? This cannot be undone.
					{/if}
				</p>
				<div class="flex gap-2">
					<Button variant="destructive" onclick={confirmDelete}>Delete</Button>
					<Button variant="secondary" onclick={() => deleteTarget = null}>Cancel</Button>
				</div>
			</CardContent>
		</Card>
	</div>
{/if}

<!-- File preview modal -->
{#if previewFile}
	{@const cat = fileCategory(previewFile.name)}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/80 backdrop-blur-sm" role="button" tabindex="-1" onclick={() => previewFile = null} onkeydown={(e) => { if (e.key === 'Escape') previewFile = null; }}>
		<div class="relative flex flex-col max-w-[90vw] max-h-[90vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<!-- Header -->
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<span class="text-sm font-semibold text-white font-mono">{previewFile.name}</span>
				<div class="flex items-center gap-2">
					<a
						href={contentUrl(previewFile)}
						download={previewFile.name}
						class="inline-flex items-center gap-1 rounded px-2 py-1 text-xs text-muted-foreground hover:text-foreground transition-colors">
						<Download size={12} /> Download
					</a>
					<Button variant="ghost" size="xs" onclick={() => previewFile = null} class="text-white hover:text-white/80">
						Close
					</Button>
				</div>
			</div>

			<!-- Content -->
			<div class="flex-1 overflow-auto p-4 flex items-center justify-center min-h-[200px]">
				{#if cat === 'image'}
					<img src={contentUrl(previewFile)} alt={previewFile.name} class="max-w-full max-h-[80vh] object-contain" />
				{:else if cat === 'video'}
					<video controls autoplay class="max-w-full max-h-[80vh]">
						<source src={contentUrl(previewFile)} />
						<track kind="captions" />
					</video>
				{:else if cat === 'audio'}
					<div class="flex flex-col items-center gap-4 py-8">
						<Music size={48} class="text-green-400" />
						<span class="text-sm text-muted-foreground">{previewFile.name}</span>
						<audio controls autoplay src={contentUrl(previewFile)} class="w-full max-w-md"></audio>
					</div>
				{:else if cat === 'pdf'}
					<iframe src={contentUrl(previewFile)} class="w-full h-[80vh]" title={previewFile.name}></iframe>
				{:else if cat === 'text'}
					<pre class="w-full max-h-[80vh] overflow-auto text-xs text-green-400 font-mono whitespace-pre-wrap p-4">{previewText || 'Loading...'}</pre>
				{/if}
			</div>
		</div>
	</div>
{/if}
