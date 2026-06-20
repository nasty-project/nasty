<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Button } from '$lib/components/ui/button';
	import { FolderOpen, File, ArrowUp, Upload, FolderPlus, Trash2, Image, Film, Music, FileText, Download, Pencil, Copy, FolderInput, Share2, Check } from '@lucide/svelte';
	import SortTh from '$lib/components/SortTh.svelte';
	import PathPicker from '$lib/components/PathPicker.svelte';
	import { getClient } from '$lib/client';
	import { withToast, error as toastError } from '$lib/toast.svelte';
	import { requiredFieldCls } from '$lib/utils';

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
		// No ?token= — browsers send the session cookie automatically with
		// same-origin <img>/<video>/<audio>/<iframe> requests.
		return `/api/files/content?path=${encodeURIComponent(path)}`;
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
			const path = currentPath ? `${currentPath}/${entry.name}` : entry.name;
			const res = await fetch(`/api/files/content?path=${encodeURIComponent(path)}`);
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
		// Switching files (or closing the modal) must drop the edit
		// buffer so the next open starts in read mode.
		editing = false;
		editBuffer = '';
	});

	const visibleEntries = $derived.by(() => {
		const filtered = showHidden ? entries : entries.filter(e => !e.name.startsWith('.'));
		const sign = sortDir === 'asc' ? 1 : -1;
		const sorted = [...filtered].sort((a, b) => {
			// Directories first regardless of column. Most users navigate
			// by folder, and inverting that within a "size" or "modified"
			// sort would make the listing harder to scan.
			if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
			let cmp = 0;
			if (sortKey === 'name') {
				cmp = a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' });
			} else if (sortKey === 'size') {
				cmp = a.size - b.size;
			} else {
				cmp = a.modified - b.modified;
			}
			// Fall back to name as a tiebreaker so equal-mtime/size rows
			// stay stably ordered between renders.
			if (cmp === 0) {
				cmp = a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' });
			}
			return sign * cmp;
		});
		return sorted;
	});

	// Upload state
	let uploading = $state(false);
	let uploadProgress = $state(0);
	let uploadName = $state('');

	// Mkdir state
	let showMkdir = $state(false);
	let newDirName = $state('');
	let mkdirTried = $state(false);

	// Delete confirmation
	let deleteTarget: FileEntry | null = $state(null);

	// Guest share creation (#474). `shareTarget` opens the dialog; once the
	// link is minted `shareUrl` holds the one-time URL (the engine never
	// returns it again).
	let shareTarget: FileEntry | null = $state(null);
	let shareExpiry = $state('7'); // days; '0' = never
	let sharePassword = $state('');
	let shareMaxDownloads = $state('');
	let shareNote = $state('');
	let shareBusy = $state(false);
	let shareUrl: string | null = $state(null);
	let shareCopied = $state(false);

	function openShare(entry: FileEntry) {
		shareTarget = entry;
		shareExpiry = '7';
		sharePassword = '';
		shareMaxDownloads = '';
		shareNote = '';
		shareUrl = null;
		shareCopied = false;
	}

	async function createShare() {
		if (!shareTarget) return;
		const rel = currentPath ? `${currentPath}/${shareTarget.name}` : shareTarget.name;
		// The engine takes absolute paths under /fs; the browser works in
		// /fs-relative paths, so re-add the prefix here.
		const abs = `/fs/${rel}`;
		const days = parseInt(shareExpiry, 10);
		const expires_at = days > 0 ? Math.floor(Date.now() / 1000) + days * 86400 : null;
		const maxDl = shareMaxDownloads.trim() ? parseInt(shareMaxDownloads, 10) : null;
		if (maxDl != null && (!Number.isFinite(maxDl) || maxDl < 1)) {
			toastError('Download limit must be a positive number');
			return;
		}
		shareBusy = true;
		const res = await withToast(
			() =>
				getClient().call<{ share: unknown; token: string }>('guestshare.create', {
					paths: [abs],
					expires_at,
					password: sharePassword ? sharePassword : null,
					max_downloads: maxDl,
					note: shareNote.trim() ? shareNote.trim() : null
				}),
			'Share link created'
		);
		shareBusy = false;
		if (res) shareUrl = `${location.origin}/share/${res.token}`;
	}

	async function copyShareUrl() {
		if (!shareUrl) return;
		try {
			await navigator.clipboard.writeText(shareUrl);
			shareCopied = true;
			setTimeout(() => (shareCopied = false), 2000);
		} catch {
			toastError('Could not copy to clipboard');
		}
	}

	// Rename inline
	let renameTarget: FileEntry | null = $state(null);
	let renameValue = $state('');
	let renameTried = $state(false);

	// Multi-select for bulk actions. Keyed by entry.name within the
	// current directory — clears automatically on browse() since
	// `selected` is reset whenever currentPath changes.
	let selected: Set<string> = $state(new Set());
	$effect(() => { void currentPath; selected = new Set(); });

	// Copy / Move picker. `pickerMode` controls which API the chosen
	// destination is fed into; `pickerTargets` is the list of entries
	// the action will iterate (either a single-row click or the
	// current multi-select). Using a list keeps the single and bulk
	// flows on one code path.
	let pickerOpen = $state(false);
	let pickerMode: 'copy' | 'move' = $state('copy');
	let pickerTargets: FileEntry[] = $state([]);
	let bulkActionRunning = $state(false);
	let bulkActionStatus = $state('');
	let bulkDeleteConfirm = $state(false);

	function isSelected(name: string): boolean { return selected.has(name); }
	function toggleSelected(name: string) {
		const next = new Set(selected);
		if (next.has(name)) next.delete(name); else next.add(name);
		selected = next;
	}
	function clearSelection() { selected = new Set(); }
	function toggleSelectAll() {
		if (selected.size === visibleEntries.length && visibleEntries.length > 0) {
			selected = new Set();
		} else {
			selected = new Set(visibleEntries.map(e => e.name));
		}
	}
	function selectedEntries(): FileEntry[] {
		return visibleEntries.filter(e => selected.has(e.name));
	}

	function openPicker(targets: FileEntry[], mode: 'copy' | 'move') {
		if (targets.length === 0) return;
		pickerTargets = targets;
		pickerMode = mode;
		pickerOpen = true;
	}

	// PathPicker returns absolute paths like "/fs/tank/photos". The
	// files API takes paths relative to /fs, so we strip that prefix
	// before issuing the per-entry request.
	function relFromHostPath(abs: string): string {
		const stripped = abs.replace(/^\/fs\/?/, '');
		return stripped;
	}

	async function runPickerAction(destAbs: string) {
		pickerOpen = false;
		const destRel = relFromHostPath(destAbs);
		const endpoint = pickerMode === 'copy' ? '/api/files/copy' : '/api/files/rename';
		const verb = pickerMode === 'copy' ? 'copy' : 'move';
		const targets = pickerTargets.slice();
		pickerTargets = [];

		bulkActionRunning = true;
		const errors: string[] = [];
		for (let i = 0; i < targets.length; i++) {
			const entry = targets[i];
			bulkActionStatus = `${verb} ${i + 1}/${targets.length}: ${entry.name}`;
			const from = currentPath ? `${currentPath}/${entry.name}` : entry.name;
			const to = destRel ? `${destRel}/${entry.name}` : entry.name;
			if (from === to) continue; // copying/moving onto self → skip silently
			try {
				const res = await fetch(endpoint, {
					method: 'POST',
					headers: { 'Content-Type': 'application/json' },
					body: JSON.stringify({ from, to }),
				});
				if (!res.ok) {
					const data = await res.json().catch(() => ({}));
					errors.push(`${entry.name}: ${data.error || res.statusText}`);
				}
			} catch (e) {
				errors.push(`${entry.name}: ${e instanceof Error ? e.message : String(e)}`);
			}
		}
		bulkActionRunning = false;
		bulkActionStatus = '';
		if (errors.length > 0) {
			alert(`${verb} finished with ${errors.length} error(s):\n\n${errors.join('\n')}`);
		}
		clearSelection();
		await browse(currentPath);
	}

	async function runBulkDelete() {
		bulkDeleteConfirm = false;
		const targets = selectedEntries();
		if (targets.length === 0) return;
		bulkActionRunning = true;
		const errors: string[] = [];
		for (let i = 0; i < targets.length; i++) {
			const entry = targets[i];
			bulkActionStatus = `delete ${i + 1}/${targets.length}: ${entry.name}`;
			const path = currentPath ? `${currentPath}/${entry.name}` : entry.name;
			try {
				const res = await fetch(`/api/files?path=${encodeURIComponent(path)}`, { method: 'DELETE' });
				if (!res.ok) {
					const data = await res.json().catch(() => ({}));
					errors.push(`${entry.name}: ${data.error || res.statusText}`);
				}
			} catch (e) {
				errors.push(`${entry.name}: ${e instanceof Error ? e.message : String(e)}`);
			}
		}
		bulkActionRunning = false;
		bulkActionStatus = '';
		if (errors.length > 0) {
			alert(`Delete finished with ${errors.length} error(s):\n\n${errors.join('\n')}`);
		}
		clearSelection();
		await browse(currentPath);
	}

	// Edit (text files in the preview modal)
	let editing = $state(false);
	let editBuffer = $state('');
	let editSaving = $state(false);

	// Sort state — directories always group above files; within each
	// group we order by the user's chosen column and direction.
	type SortKey = 'name' | 'size' | 'modified';
	let sortKey = $state<SortKey>('name');
	let sortDir = $state<'asc' | 'desc'>('asc');
	function toggleSort(key: SortKey) {
		if (sortKey === key) {
			sortDir = sortDir === 'asc' ? 'desc' : 'asc';
		} else {
			sortKey = key;
			sortDir = key === 'name' ? 'asc' : 'desc';
		}
	}

	onMount(() => browse(''));

	async function browse(path: string) {
		loading = true;
		try {
			const res = await fetch(`/api/files/browse?path=${encodeURIComponent(path)}`);
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

		const form = new FormData();
		form.append('file', file);

		try {
			await new Promise<void>((resolve, reject) => {
				const xhr = new XMLHttpRequest();
				xhr.open('POST', `/api/files/upload?path=${encodeURIComponent(currentPath)}`);
				// Cookie auth — XHR sends same-origin cookies automatically.
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
		if (!newDirName.trim()) { mkdirTried = true; return; }
		mkdirTried = false;
		const path = currentPath ? `${currentPath}/${newDirName.trim()}` : newDirName.trim();
		const res = await fetch(`/api/files/mkdir?path=${encodeURIComponent(path)}`, {
			method: 'POST',
		});
		if (!res.ok) {
			const data = await res.json();
			alert(data.error || 'Failed to create directory');
			return;
		}
		showMkdir = false;
		newDirName = '';
		mkdirTried = false;
		await browse(currentPath);
	}

	async function confirmDelete() {
		if (!deleteTarget) return;
		const path = currentPath ? `${currentPath}/${deleteTarget.name}` : deleteTarget.name;
		const res = await fetch(`/api/files?path=${encodeURIComponent(path)}`, {
			method: 'DELETE',
		});
		if (!res.ok) {
			const data = await res.json();
			alert(data.error || 'Failed to delete');
		}
		deleteTarget = null;
		await browse(currentPath);
	}

	function startRename(entry: FileEntry) {
		renameTarget = entry;
		renameValue = entry.name;
	}

	async function confirmRename() {
		if (!renameTarget) return;
		const newName = renameValue.trim();
		if (!newName || newName === renameTarget.name) {
			renameTried = true;
			return;
		}
		renameTried = false;
		if (newName.includes('/')) {
			alert('Name cannot contain slashes — use this directory only.');
			return;
		}
		const from = currentPath ? `${currentPath}/${renameTarget.name}` : renameTarget.name;
		const to = currentPath ? `${currentPath}/${newName}` : newName;
		const res = await fetch('/api/files/rename', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ from, to }),
		});
		if (!res.ok) {
			const data = await res.json().catch(() => ({}));
			alert(data.error || 'Failed to rename');
			return;
		}
		renameTarget = null;
		renameValue = '';
		renameTried = false;
		await browse(currentPath);
	}

	function startEdit() {
		if (!previewFile) return;
		editBuffer = previewText;
		editing = true;
	}

	function cancelEdit() {
		editing = false;
		editBuffer = '';
	}

	async function saveEdit() {
		if (!previewFile) return;
		editSaving = true;
		const path = currentPath ? `${currentPath}/${previewFile.name}` : previewFile.name;
		try {
			const res = await fetch(`/api/files/content?path=${encodeURIComponent(path)}`, {
				method: 'PUT',
				headers: { 'Content-Type': 'text/plain; charset=utf-8' },
				body: editBuffer,
			});
			if (!res.ok) {
				const data = await res.json().catch(() => ({}));
				alert(data.error || 'Failed to save');
				return;
			}
			previewText = editBuffer;
			editing = false;
			// Refresh the listing so the modified timestamp updates.
			await browse(currentPath);
		} finally {
			editSaving = false;
		}
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

<!-- Bulk action bar (visible only when at least one row is selected) -->
{#if selected.size > 0 && !isRoot}
	<div class="mb-3 flex items-center gap-2 rounded-md border border-border bg-muted/30 px-3 py-2">
		<span class="text-sm font-medium">{selected.size} selected</span>
		<div class="ml-auto flex items-center gap-2">
			<Button size="sm" variant="outline" onclick={() => openPicker(selectedEntries(), 'copy')} disabled={bulkActionRunning}>
				<Copy size={14} class="mr-1" /> Copy
			</Button>
			<Button size="sm" variant="outline" onclick={() => openPicker(selectedEntries(), 'move')} disabled={bulkActionRunning}>
				<FolderInput size={14} class="mr-1" /> Move
			</Button>
			<Button size="sm" variant="destructive" onclick={() => bulkDeleteConfirm = true} disabled={bulkActionRunning}>
				<Trash2 size={14} class="mr-1" /> Delete
			</Button>
			<Button size="sm" variant="ghost" onclick={clearSelection} disabled={bulkActionRunning}>
				Clear
			</Button>
		</div>
	</div>
{/if}

<!-- Bulk action progress strip -->
{#if bulkActionRunning}
	<div class="mb-3 rounded-md border border-border bg-muted/20 px-3 py-2 text-sm text-muted-foreground">
		{bulkActionStatus || 'Working…'}
	</div>
{/if}

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
			class="h-9 w-64 rounded-md border border-input bg-transparent px-3 text-sm {requiredFieldCls(!newDirName.trim(), mkdirTried)}"
			onkeydown={(e) => { if (e.key === 'Enter') createDir(); if (e.key === 'Escape') showMkdir = false; }} />
		<Button size="sm" onclick={createDir}>Create</Button>
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
			<Button size="sm" class="mt-2" onclick={() => goto('/filesystems')}>Filesystems</Button>
		{/if}
	</div>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				{#if !isRoot}
					<th class="w-10 border-b-2 border-border p-3">
						<input
							type="checkbox"
							class="h-3.5 w-3.5"
							aria-label="Select all"
							checked={selected.size > 0 && selected.size === visibleEntries.length}
							indeterminate={selected.size > 0 && selected.size < visibleEntries.length}
							onchange={toggleSelectAll} />
					</th>
				{/if}
				<SortTh label="Name" active={sortKey === 'name'} dir={sortDir} onclick={() => toggleSort('name')} />
				<SortTh label="Size" active={sortKey === 'size'} dir={sortDir} onclick={() => toggleSort('size')} align="right" />
				<SortTh label="Modified" active={sortKey === 'modified'} dir={sortDir} onclick={() => toggleSort('modified')} align="right" />
				{#if !isRoot}
					<th class="w-10 border-b-2 border-border"></th>
				{/if}
			</tr>
		</thead>
		<tbody>
			{#each visibleEntries as entry}
				<tr class="border-b border-border hover:bg-muted/30 transition-colors group {isSelected(entry.name) ? 'bg-muted/20' : ''}">
					{#if !isRoot}
						<td class="p-3">
							<input
								type="checkbox"
								class="h-3.5 w-3.5"
								aria-label={`Select ${entry.name}`}
								checked={isSelected(entry.name)}
								onchange={() => toggleSelected(entry.name)} />
						</td>
					{/if}
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
									class="text-muted-foreground/40 hover:text-foreground transition-colors"
									onclick={() => openShare(entry)}
									title="Create guest share link">
									<Share2 size={14} />
								</button>
								<button
									class="text-muted-foreground/40 hover:text-foreground transition-colors"
									onclick={() => startRename(entry)}
									title="Rename">
									<Pencil size={14} />
								</button>
								<button
									class="text-muted-foreground/40 hover:text-foreground transition-colors"
									onclick={() => openPicker([entry], 'copy')}
									title="Copy to…">
									<Copy size={14} />
								</button>
								<button
									class="text-muted-foreground/40 hover:text-foreground transition-colors"
									onclick={() => openPicker([entry], 'move')}
									title="Move to…">
									<FolderInput size={14} />
								</button>
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

<!-- Rename modal -->
{#if renameTarget}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<Card class="w-full max-w-sm">
			<CardContent class="pt-6">
				<h3 class="mb-2 text-lg font-semibold">Rename {renameTarget.is_dir ? 'folder' : 'file'}</h3>
				<p class="mb-3 text-sm text-muted-foreground">
					New name for <span class="font-mono font-medium text-foreground">{renameTarget.name}</span>:
				</p>
				<!-- svelte-ignore a11y_autofocus -->
				<input
					type="text"
					bind:value={renameValue}
					class="mb-4 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm font-mono {requiredFieldCls(!renameValue.trim() || renameValue === renameTarget.name, renameTried)}"
					onkeydown={(e) => { if (e.key === 'Enter') confirmRename(); if (e.key === 'Escape') renameTarget = null; }}
					autofocus
				/>
				<div class="flex gap-2">
					<Button onclick={confirmRename}>Rename</Button>
					<Button variant="secondary" onclick={() => { renameTarget = null; renameValue = ''; }}>Cancel</Button>
				</div>
			</CardContent>
		</Card>
	</div>
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

<!-- Guest share creation -->
{#if shareTarget}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<Card class="w-full max-w-md">
			<CardContent class="pt-6">
				{#if shareUrl}
					<h3 class="mb-2 text-lg font-semibold">Share link ready</h3>
					<p class="mb-3 text-sm text-muted-foreground">
						Copy it now — for security it can't be shown again. Manage or revoke it later under
						<a href="/shares" class="underline">Guest Shares</a>.
					</p>
					<div class="mb-4 flex items-center gap-2">
						<input
							class="h-9 w-full rounded-md border border-input bg-transparent px-3 font-mono text-xs"
							readonly
							value={shareUrl} />
						<Button variant="secondary" size="sm" onclick={copyShareUrl}>
							{#if shareCopied}<Check size={14} class="mr-1" /> Copied{:else}<Copy size={14} class="mr-1" /> Copy{/if}
						</Button>
					</div>
					<div class="flex justify-end">
						<Button onclick={() => (shareTarget = null)}>Done</Button>
					</div>
				{:else}
					<h3 class="mb-1 text-lg font-semibold">Share {shareTarget.is_dir ? 'folder' : 'file'}</h3>
					<p class="mb-4 text-sm text-muted-foreground">
						Create a public link to <span class="font-mono font-medium text-foreground">{shareTarget.name}</span>.
					</p>

					<div class="mb-4">
						<label for="share-expiry" class="text-sm font-medium">Expires</label>
						<select
							id="share-expiry"
							bind:value={shareExpiry}
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value="1">In 1 day</option>
							<option value="7">In 7 days</option>
							<option value="30">In 30 days</option>
							<option value="0">Never</option>
						</select>
					</div>

					<div class="mb-4">
						<label for="share-password" class="text-sm font-medium">Password <span class="text-muted-foreground">(optional)</span></label>
						<input
							id="share-password"
							type="password"
							autocomplete="new-password"
							bind:value={sharePassword}
							placeholder="No password"
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm" />
					</div>

					<div class="mb-4">
						<label for="share-maxdl" class="text-sm font-medium">Download limit <span class="text-muted-foreground">(optional)</span></label>
						<input
							id="share-maxdl"
							type="number"
							min="1"
							bind:value={shareMaxDownloads}
							placeholder="Unlimited"
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm" />
					</div>

					<div class="mb-4">
						<label for="share-note" class="text-sm font-medium">Note <span class="text-muted-foreground">(optional)</span></label>
						<input
							id="share-note"
							bind:value={shareNote}
							placeholder="For your reference"
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm" />
					</div>

					<div class="flex justify-end gap-2">
						<Button variant="secondary" onclick={() => (shareTarget = null)} disabled={shareBusy}>Cancel</Button>
						<Button onclick={createShare} disabled={shareBusy}>{shareBusy ? 'Creating…' : 'Create link'}</Button>
					</div>
				{/if}
			</CardContent>
		</Card>
	</div>
{/if}

<!-- File preview modal -->
{#if previewFile}
	{@const cat = fileCategory(previewFile.name)}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/80 backdrop-blur-sm" role="button" tabindex="-1" onclick={() => { if (!editing) previewFile = null; }} onkeydown={(e) => { if (e.key === 'Escape' && !editing) previewFile = null; }}>
		<div class="relative flex flex-col max-w-[90vw] max-h-[90vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<!-- Header -->
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<span class="text-sm font-semibold text-white font-mono">{previewFile.name}{editing ? ' (editing)' : ''}</span>
				<div class="flex items-center gap-2">
					{#if cat === 'text' && !isRoot}
						{#if editing}
							<Button size="xs" onclick={saveEdit} disabled={editSaving}>
								{editSaving ? 'Saving…' : 'Save'}
							</Button>
							<Button variant="ghost" size="xs" onclick={cancelEdit} disabled={editSaving} class="text-white hover:text-white/80">
								Cancel
							</Button>
						{:else}
							<button
								class="inline-flex items-center gap-1 rounded px-2 py-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
								onclick={startEdit}
								title="Edit this file in the browser">
								<Pencil size={12} /> Edit
							</button>
						{/if}
					{/if}
					<a
						href={contentUrl(previewFile)}
						download={previewFile.name}
						class="inline-flex items-center gap-1 rounded px-2 py-1 text-xs text-muted-foreground hover:text-foreground transition-colors">
						<Download size={12} /> Download
					</a>
					<Button variant="ghost" size="xs" onclick={() => { if (!editing) previewFile = null; }} disabled={editing} class="text-white hover:text-white/80">
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
					{#if editing}
						<textarea
							bind:value={editBuffer}
							spellcheck="false"
							class="w-[80vw] max-w-3xl h-[70vh] resize-none rounded-md border border-input bg-background p-3 text-xs text-foreground font-mono"
							disabled={editSaving}
						></textarea>
					{:else}
						<pre class="w-full max-h-[80vh] overflow-auto text-xs text-green-400 font-mono whitespace-pre-wrap p-4">{previewText || 'Loading...'}</pre>
					{/if}
				{/if}
			</div>
		</div>
	</div>
{/if}

<!-- Bulk delete confirm modal -->
{#if bulkDeleteConfirm}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<Card class="w-full max-w-sm">
			<CardContent class="pt-6">
				<h3 class="mb-2 text-lg font-semibold">Delete {selected.size} item{selected.size === 1 ? '' : 's'}?</h3>
				<p class="mb-4 text-sm text-muted-foreground">
					This cannot be undone. Directories are removed with their contents.
				</p>
				<div class="flex gap-2">
					<Button variant="destructive" onclick={runBulkDelete}>Delete {selected.size}</Button>
					<Button variant="secondary" onclick={() => bulkDeleteConfirm = false}>Cancel</Button>
				</div>
			</CardContent>
		</Card>
	</div>
{/if}

<!-- Copy / Move destination picker -->
<PathPicker
	open={pickerOpen}
	initialPath={currentPath}
	title={pickerMode === 'copy'
		? `Copy ${pickerTargets.length} item${pickerTargets.length === 1 ? '' : 's'} to…`
		: `Move ${pickerTargets.length} item${pickerTargets.length === 1 ? '' : 's'} to…`}
	onPick={runPickerAction}
	onClose={() => { pickerOpen = false; pickerTargets = []; }}
/>
