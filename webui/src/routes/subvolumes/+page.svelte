<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { Filesystem, Subvolume, Snapshot, SubvolumeType, NfsShare, SmbShare, IscsiTarget, NvmeofSubsystem } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Dialog from '$lib/components/ui/dialog';
	import SortTh from '$lib/components/SortTh.svelte';
	import { Camera, Copy, Trash2, Pencil, Check, X, AlertTriangle } from '@lucide/svelte';

	let pageTab = $state<'subvolumes' | 'snapshots'>(
		typeof window !== 'undefined' && window.location.hash === '#snapshots' ? 'snapshots' : 'subvolumes'
	);

	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');

	// Snapshots tab state
	let allSnapshots: Snapshot[] = $state([]);
	let snapshotsLoading = $state(false);
	let snapshotSearch = $state('');

	async function loadSnapshots() {
		if (!selectedFs) return;
		snapshotsLoading = true;
		try {
			allSnapshots = await client.call<Snapshot[]>('snapshot.list', { filesystem: selectedFs });
		} catch {
			allSnapshots = [];
		}
		snapshotsLoading = false;
	}

	const filteredSnapshots = $derived(
		snapshotSearch.trim()
			? allSnapshots.filter(s =>
				s.name.toLowerCase().includes(snapshotSearch.toLowerCase()) ||
				s.subvolume.toLowerCase().includes(snapshotSearch.toLowerCase()))
			: allSnapshots
	);

	function switchToSubvolumeAndExpand(subvolumeName: string) {
		pageTab = 'subvolumes';
		history.replaceState(null, '', '#subvolumes');
		const sv = subvolumes.find(s => s.name === subvolumeName);
		if (sv) {
			openDetail(sv);
		}
	}

	async function deleteSnapshotFromTab(subvolume: string, snap: string) {
		if (!await confirm(`Delete snapshot "${snap}"?`)) return;
		await withToast(
			() => client.call('snapshot.delete', {
				filesystem: selectedFs,
				subvolume,
				name: snap,
			}),
			`Snapshot "${snap}" deleted`
		);
		await loadSnapshots();
		await refresh();
	}
	let subvolumes: Subvolume[] = $state([]);
	let loading = $state(true);

	let wizardStep: 0 | 1 | 2 | 3 = $state(0); // 0=hidden
	let newName = $state('');
	let newType: SubvolumeType = $state('filesystem');
	let newVolsize = $state('');
	let newCompression = $state('');
	let newForegroundTarget = $state('');
	let newBackgroundTarget = $state('');
	let newPromoteTarget = $state('');
	let newMetadataTarget = $state('');
	let newDataReplicas = $state('');
	let newComments = $state('');
	let newDirectIo = $state(false);

	const WIZARD_STEPS: [string, string][] = [
		['1', 'Basic'],
		['2', 'Storage'],
		['3', 'Review'],
	];

	function openWizard() {
		wizardStep = 1;
		newName = ''; newType = 'filesystem'; newVolsize = ''; newCompression = '';
		newComments = ''; newDirectIo = false;
	}

	let showSnap = $state<string | null>(null);
	let snapName = $state('');
	let showClone = $state<string | null>(null);
	let cloneName = $state('');
	let showResize = $state(false);
	let resizeValue = $state('');

	async function resizeSubvolume() {
		if (!detailSv || !resizeValue) return;
		const bytes = parseFloat(resizeValue) * 1073741824;
		await withToast(
			() => client.call('subvolume.resize', { filesystem: selectedFs, name: detailSv!.name, volsize_bytes: bytes }),
			`${detailSv.subvolume_type === 'block' ? 'Volume' : 'Quota'} updated to ${resizeValue} GiB`
		);
		showResize = false;
		resizeValue = '';
		await refresh();
		if (detailSv) {
			const updated = subvolumes.find(sv => sv.name === detailSv!.name);
			if (updated) detailSv = updated;
		}
	}

	// Inline expanded detail
	let expandedName = $state<string | null>(null);
	let detailSv = $state<Subvolume | null>(null);
	let detailSnapshots = $state<Snapshot[]>([]);
	let nestedSubvolumes = $state<string[]>([]);
	let detailTab = $state<'info' | 'snapshots' | 'shares' | 'browse' | 'properties'>('info');

	// Inline editing
	let editingField = $state<'compression' | 'comments' | null>(null);
	let editValue = $state('');

	function startEdit(field: 'compression' | 'comments') {
		editingField = field;
		editValue = field === 'compression'
			? (detailSv?.compression ?? '')
			: (detailSv?.comments ?? '');
	}

	async function saveEdit() {
		if (!detailSv || !editingField) return;
		const params: Record<string, string> = {
			filesystem: selectedFs,
			name: detailSv.name,
		};
		params[editingField] = editValue;
		const ok = await withToast(
			() => client.call('subvolume.update', params),
			`${editingField === 'compression' ? 'Compression' : 'Comments'} updated`
		);
		if (ok !== undefined) {
			editingField = null;
			await refresh();
			const updated = subvolumes.find(sv => sv.name === detailSv!.name);
			if (updated) {
				detailSv = updated;
			}
		}
	}

	function cancelEdit() {
		editingField = null;
	}

	// Shares linked to the detail subvolume
	interface LinkedShares {
		nfs: NfsShare[];
		smb: SmbShare[];
		iscsi: IscsiTarget[];
		nvmeof: NvmeofSubsystem[];
	}
	let detailShares = $state<LinkedShares>({ nfs: [], smb: [], iscsi: [], nvmeof: [] });
	// Tree: find parent chain and children for the selected subvolume
	const detailParentChain = $derived.by((): string[] => {
		if (!detailSv) return [];
		const chain: string[] = [];
		let current = detailSv.parent;
		const seen = new Set<string>();
		while (current && !seen.has(current)) {
			seen.add(current);
			chain.unshift(current);
			const parentSv = subvolumes.find(sv => sv.name === current);
			current = parentSv?.parent ?? null;
		}
		return chain;
	});

	const detailChildren = $derived.by((): { name: string; type: 'clone' | 'snapshot' }[] => {
		if (!detailSv) return [];
		const result: { name: string; type: 'clone' | 'snapshot' }[] = [];
		// Writable clones: subvolumes whose parent is this subvolume
		for (const sv of subvolumes) {
			if (sv.parent === detailSv.name) {
				result.push({ name: sv.name, type: 'clone' });
			}
		}
		// Read-only snapshots
		for (const snap of detailSv.snapshots) {
			result.push({ name: snap, type: 'snapshot' });
		}
		return result;
	});

	const detailShareCount = $derived(
		detailShares.nfs.length + detailShares.smb.length +
		detailShares.iscsi.length + detailShares.nvmeof.length
	);

	async function openDetail(sv: Subvolume) {
		if (expandedName === sv.name) {
			expandedName = null;
			detailSv = null;
			return;
		}
		expandedName = sv.name;
		detailSv = sv;
		detailTab = 'info';
		detailSnapshots = [];
		nestedSubvolumes = [];
		detailShares = { nfs: [], smb: [], iscsi: [], nvmeof: [] };

		// Load snapshots, shares, and children in parallel
		const [snapResult, nfsResult, smbResult, iscsiResult, nvmeofResult, childrenResult] = await Promise.allSettled([
			client.call<Snapshot[]>('snapshot.list', { filesystem: selectedFs }),
			client.call<NfsShare[]>('share.nfs.list'),
			client.call<SmbShare[]>('share.smb.list'),
			client.call<IscsiTarget[]>('share.iscsi.list'),
			client.call<NvmeofSubsystem[]>('share.nvmeof.list'),
			client.call<string[]>('subvolume.children', { filesystem: selectedFs, name: sv.name }),
		]);

		if (snapResult.status === 'fulfilled') {
			detailSnapshots = snapResult.value.filter(s => s.subvolume === sv.name);
		}

		const svPath = sv.path;
		const blockDev = sv.block_device;

		detailShares = {
			nfs: nfsResult.status === 'fulfilled'
				? nfsResult.value.filter(s => s.path === svPath) : [],
			smb: smbResult.status === 'fulfilled'
				? smbResult.value.filter(s => s.path === svPath) : [],
			iscsi: iscsiResult.status === 'fulfilled'
				? iscsiResult.value.filter(t =>
					blockDev != null && t.luns.some(l => l.backstore_path === blockDev)) : [],
			nvmeof: nvmeofResult.status === 'fulfilled'
				? nvmeofResult.value.filter(sub =>
					blockDev != null && sub.namespaces.some(ns => ns.device_path === blockDev)) : [],
		};

		if (childrenResult.status === 'fulfilled') {
			nestedSubvolumes = childrenResult.value;
		}
	}

	function closeDetail() {
		expandedName = null;
		detailSv = null;
	}

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'subvolume' || p?.collection === 'snapshot') {
			refresh();
			if (pageTab === 'snapshots') loadSnapshots();
		}
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		filesystems = await client.call<Filesystem[]>('fs.list');
		const mounted = filesystems.filter(p => p.mounted);
		if (mounted.length > 0) {
			selectedFs = mounted[0].name;
			await refresh();
			if (pageTab === 'snapshots') await loadSnapshots();
		}
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		if (!selectedFs) return;
		await withToast(async () => {
			subvolumes = await client.call<Subvolume[]>('subvolume.list', { filesystem: selectedFs });
		});
	}

	async function selectFs(name: string) {
		selectedFs = name;
		await refresh();
		if (pageTab === 'snapshots') await loadSnapshots();
	}

	async function createSubvolume() {
		if (!newName || !selectedFs) return;
		if (newType === 'block' && !newVolsize) return;

		const params: Record<string, unknown> = {
			filesystem: selectedFs,
			name: newName,
			subvolume_type: newType,
		};
		if (newVolsize) {
			params.volsize_bytes = parseFloat(newVolsize) * 1073741824;
		}
		if (newCompression) params.compression = newCompression;
		if (newForegroundTarget) params.foreground_target = newForegroundTarget;
		if (newBackgroundTarget) params.background_target = newBackgroundTarget;
		if (newPromoteTarget) params.promote_target = newPromoteTarget;
		if (newMetadataTarget) params.metadata_target = newMetadataTarget;
		if (newDataReplicas) params.data_replicas = parseInt(newDataReplicas, 10);
		if (newComments) params.comments = newComments;
		if (newDirectIo) params.direct_io = true;

		const ok = await withToast(
			() => client.call('subvolume.create', params),
			`Subvolume "${newName}" created`
		);
		if (ok !== undefined) {
			wizardStep = 0;
			newName = ''; newType = 'filesystem'; newVolsize = ''; newCompression = '';
			newForegroundTarget = ''; newBackgroundTarget = ''; newPromoteTarget = '';
			newMetadataTarget = ''; newDataReplicas = '';
			newComments = ''; newDirectIo = false;
			await refresh();
		}
	}

	const SYSTEM_SUBVOLUMES: Record<string, string> = {};

	async function deleteSubvolume(name: string) {
		const systemUse = SYSTEM_SUBVOLUMES[name];
		// Check for child subvolumes
		let children: string[] = [];
		try { children = await client.call<string[]>('subvolume.children', { filesystem: selectedFs, name }); } catch {}
		let warning = '';
		if (systemUse) {
			warning += `This subvolume is used by the system for: ${systemUse}. Deleting it may break functionality. `;
		}
		if (children.length > 0) {
			warning += `This will also delete ${children.length} nested subvolume${children.length > 1 ? 's' : ''}: ${children.join(', ')}. `;
		}
		warning += 'All snapshots will also be deleted.';
		if (!await confirm(`Delete "${name}"?`, warning)) return;
		await withToast(
			() => client.call('subvolume.delete', { filesystem: selectedFs, name }),
			`Subvolume "${name}" deleted`
		);
		await refresh();
	}

	async function attachSubvolume(name: string) {
		await withToast(
			() => client.call('subvolume.attach', { filesystem: selectedFs, name }),
			`Loop device attached for "${name}"`
		);
		await refresh();
	}

	async function detachSubvolume(name: string) {
		if (!await confirm(`Detach loop device for "${name}"?`, 'Any active iSCSI/NVMe-oF connections using this device will break.')) return;
		await withToast(
			() => client.call('subvolume.detach', { filesystem: selectedFs, name }),
			`Loop device detached for "${name}"`
		);
		await refresh();
	}

	async function createSnapshot() {
		if (!showSnap || !snapName) return;
		const ok = await withToast(
			() => client.call('snapshot.create', {
				filesystem: selectedFs,
				subvolume: showSnap,
				name: snapName,
				read_only: true,
			}),
			`Snapshot "${snapName}" created`
		);
		if (ok !== undefined) {
			showSnap = null;
			snapName = '';
		}
		await refresh();
		// Update detail view so tab count reflects the new snapshot
		if (detailSv) {
			const updated = subvolumes.find(sv => sv.name === detailSv!.name);
			if (updated) detailSv = updated;
		}
	}

	async function cloneSubvolume() {
		if (!showClone || !cloneName) return;
		const isSnapshotClone = showClone.includes('@');
		const ok = isSnapshotClone
			? await withToast(() => {
				const [subvolume, snapshot] = showClone!.split('@');
				return client.call('snapshot.clone', {
					filesystem: selectedFs,
					subvolume,
					snapshot,
					new_name: cloneName,
				});
			}, `Clone "${cloneName}" created from snapshot`)
			: await withToast(
				() => client.call('subvolume.clone', {
					filesystem: selectedFs,
					name: showClone,
					new_name: cloneName,
				}),
				`Clone "${cloneName}" created`
			);
		if (ok !== undefined) {
			showClone = null;
			cloneName = '';
			await refresh();
			// Reopen detail if we cloned the detail subvolume
			if (detailSv) {
				const updated = subvolumes.find(sv => sv.name === detailSv!.name);
				if (updated) openDetail(updated);
			}
		}
	}

	async function deleteSnapshot(subvolume: string, snap: string) {
		if (!await confirm(`Delete snapshot "${snap}"?`)) return;
		await withToast(
			() => client.call('snapshot.delete', {
				filesystem: selectedFs,
				subvolume,
				name: snap,
			}),
			`Snapshot "${snap}" deleted`
		);
		await refresh();
		if (detailSv) {
			const updated = subvolumes.find(sv => sv.name === detailSv!.name);
			if (updated) detailSv = updated;
		}
	}

	const mountedFilesystems = $derived(filesystems.filter(p => p.mounted));

	// Unique device labels from the selected filesystem (for tiering dropdowns)
	const deviceLabels = $derived(() => {
		const fs = filesystems.find(f => f.name === selectedFs);
		if (!fs) return [];
		const labels = fs.devices.map(d => d.label).filter((l): l is string => !!l);
		return [...new Set(labels)].sort();
	});

	let search = $state('');

	type SortKey = 'name' | 'type' | 'size';
	let sortKey = $state<SortKey | null>('name');
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort(key: SortKey) {
		if (sortKey === key) {
			sortDir = sortDir === 'asc' ? 'desc' : 'asc';
		} else {
			sortKey = key;
			sortDir = 'asc';
		}
	}

	function svSize(sv: Subvolume): number {
		return sv.subvolume_type === 'block' ? (sv.volsize_bytes ?? 0) : (sv.used_bytes ?? 0);
	}

	const filtered = $derived(
		search.trim()
			? subvolumes.filter(sv =>
				sv.name.toLowerCase().includes(search.toLowerCase()) ||
				sv.comments?.toLowerCase().includes(search.toLowerCase()))
			: subvolumes
	);

	const sorted = $derived.by(() => {
		if (!sortKey) return filtered;
		return [...filtered].sort((a, b) => {
			let cmp = 0;
			if (sortKey === 'name') cmp = a.name.localeCompare(b.name);
			else if (sortKey === 'type') cmp = a.subvolume_type.localeCompare(b.subvolume_type);
			else if (sortKey === 'size') cmp = svSize(a) - svSize(b);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>


<!-- Page-level tabs -->
<div class="mb-4 flex items-center gap-4 border-b border-border">
	<button
		onclick={() => { pageTab = 'subvolumes'; history.replaceState(null, '', '#subvolumes'); }}
		class="px-3 py-2 text-sm font-medium transition-colors border-b-2 -mb-px
			{pageTab === 'subvolumes' ? 'border-primary text-foreground' : 'border-transparent text-muted-foreground hover:text-foreground'}"
	>Subvolumes</button>
	<button
		onclick={() => { pageTab = 'snapshots'; history.replaceState(null, '', '#snapshots'); loadSnapshots(); }}
		class="px-3 py-2 text-sm font-medium transition-colors border-b-2 -mb-px
			{pageTab === 'snapshots' ? 'border-primary text-foreground' : 'border-transparent text-muted-foreground hover:text-foreground'}"
	>Snapshots</button>
</div>

{#if pageTab === 'subvolumes'}

{#if mountedFilesystems.length > 0}
	<div class="mb-4 flex items-center gap-4">
		<Button size="sm" onclick={() => wizardStep === 0 ? openWizard() : (wizardStep = 0)}>
			{wizardStep !== 0 ? 'Cancel' : 'Create Subvolume'}
		</Button>
		<select value={selectedFs} onchange={(e) => selectFs((e.target as HTMLSelectElement).value)} class="h-9 w-auto rounded-md border border-input bg-transparent px-3 text-sm">
			{#each mountedFilesystems as p}
				<option value={p.name}>{p.name}</option>
			{/each}
		</select>
		<Input bind:value={search} placeholder="Search..." class="h-9 w-48" />
	</div>
{/if}

{#if wizardStep !== 0}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<!-- Step indicator -->
			<div class="mb-6 flex items-center gap-0">
				{#each WIZARD_STEPS as [num, label], i}
					<div class="flex items-center">
						<div class="flex items-center gap-2">
							<div class="flex h-6 w-6 items-center justify-center rounded-full text-xs font-semibold
								{wizardStep > i + 1 ? 'bg-primary text-primary-foreground' :
								 wizardStep === i + 1 ? 'bg-primary text-primary-foreground' :
								 'bg-secondary text-muted-foreground'}">
								{num}
							</div>
							<span class="text-xs {wizardStep === i + 1 ? 'text-foreground font-medium' : 'text-muted-foreground'}">{label}</span>
						</div>
						{#if i < WIZARD_STEPS.length - 1}
							<div class="mx-3 h-px w-8 bg-border"></div>
						{/if}
					</div>
				{/each}
			</div>

			<!-- Step 1: Basic -->
			{#if wizardStep === 1}
			<div class="mb-4">
				<Label for="sv-name">Name</Label>
				<Input id="sv-name" bind:value={newName} placeholder="documents or projects/web" class="mt-1" />
				<p class="mt-1 text-xs text-muted-foreground">Use <span class="font-mono">/</span> for nested subvolumes (e.g. projects/web)</p>
			</div>
			<div class="mb-4">
				<Label for="sv-type">Type</Label>
				<select id="sv-type" bind:value={newType} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="filesystem">File Share (NFS, SMB)</option>
					<option value="block">Block Device (iSCSI, NVMe-oF)</option>
				</select>
			</div>
			<div class="mb-4">
				<Label for="sv-comments">Comments</Label>
				<Input id="sv-comments" bind:value={newComments} placeholder="Optional description" class="mt-1" />
			</div>
			<div class="flex gap-2">
				<Button size="sm" onclick={() => wizardStep = 2} disabled={!newName}>Next: Storage →</Button>
			</div>

			<!-- Step 2: Storage -->
			{:else if wizardStep === 2}
			<div class="mb-4">
				<Label for="sv-volsize">{newType === 'block' ? 'Volume Size (GiB)' : 'Quota (GiB)'}</Label>
				<Input id="sv-volsize" type="number" bind:value={newVolsize} placeholder={newType === 'block' ? '100' : 'No limit'} min="1" class="mt-1" />
				{#if newType === 'filesystem'}
					<p class="mt-1 text-xs text-muted-foreground">Maximum space this share can use. Leave empty for no limit.</p>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="sv-compression">Compression</Label>
				<select id="sv-compression" bind:value={newCompression} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">None</option>
					<option value="lz4">LZ4</option>
					<option value="zstd">Zstd</option>
					<option value="gzip">Gzip</option>
				</select>
			</div>
			{#if deviceLabels().length > 0}
				<div class="mb-4">
					<Label>Tiering Targets</Label>
					<p class="mb-2 text-xs text-muted-foreground">Override filesystem defaults. Leave empty to inherit.</p>
					<div class="grid grid-cols-2 gap-2">
						<div>
							<label for="sv-fg-target" class="mb-1 block text-xs text-muted-foreground">Foreground Target</label>
							<select id="sv-fg-target" bind:value={newForegroundTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
								<option value="">Inherit</option>
								{#each deviceLabels() as label}
									<option value={label}>{label}</option>
								{/each}
							</select>
						</div>
						<div>
							<label for="sv-meta-target" class="mb-1 block text-xs text-muted-foreground">Metadata Target</label>
							<select id="sv-meta-target" bind:value={newMetadataTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
								<option value="">Inherit</option>
								{#each deviceLabels() as label}
									<option value={label}>{label}</option>
								{/each}
							</select>
						</div>
						<div>
							<label for="sv-bg-target" class="mb-1 block text-xs text-muted-foreground">Background Target</label>
							<select id="sv-bg-target" bind:value={newBackgroundTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
								<option value="">Inherit</option>
								{#each deviceLabels() as label}
									<option value={label}>{label}</option>
								{/each}
							</select>
						</div>
						<div>
							<label for="sv-promote-target" class="mb-1 block text-xs text-muted-foreground">Promote Target</label>
							<select id="sv-promote-target" bind:value={newPromoteTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
								<option value="">Inherit</option>
								{#each deviceLabels() as label}
									<option value={label}>{label}</option>
								{/each}
							</select>
						</div>
					</div>
				</div>
			{/if}
			<div class="mb-4">
				<label for="sv-data-replicas" class="mb-1 block text-sm font-medium">Data Replicas</label>
				<input id="sv-data-replicas" type="number" bind:value={newDataReplicas} placeholder="Inherit from filesystem" min="1" max="8" class="h-8 w-32 rounded-md border border-input bg-transparent px-2 text-sm" />
				<p class="mt-1 text-xs text-muted-foreground">Number of data copies. Leave empty to inherit from filesystem.</p>
			</div>
			{#if newType === 'block'}
				<div class="mb-4">
					<label class="flex cursor-pointer items-center gap-2 text-sm font-medium">
						<input type="checkbox" bind:checked={newDirectIo} class="h-4 w-4" />
						Direct I/O (O_DIRECT)
					</label>
					<p class="mt-1 text-xs text-muted-foreground">Bypass host page cache for the backing file. Reduces double-caching when the client (iSCSI/NVMe-oF) manages its own cache.</p>
				</div>
			{/if}
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 1}>← Back</Button>
				<Button size="sm" onclick={() => wizardStep = 3} disabled={newType === 'block' && !newVolsize}>Next: Review →</Button>
			</div>

			<!-- Step 3: Review -->
			{:else if wizardStep === 3}
			<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
				<span class="text-muted-foreground">Filesystem</span>
				<span class="font-mono">{selectedFs}</span>
				<span class="text-muted-foreground">Name</span>
				<span class="font-mono">{newName}</span>
				<span class="text-muted-foreground">Type</span>
				<span>{newType === 'filesystem' ? 'File Share' : 'Block Device'}</span>
				{#if newVolsize}
					<span class="text-muted-foreground">{newType === 'block' ? 'Size' : 'Quota'}</span>
					<span>{newVolsize} GiB</span>
				{/if}
				{#if newCompression}
					<span class="text-muted-foreground">Compression</span>
					<span>{newCompression}</span>
				{/if}
				{#if newForegroundTarget}
					<span class="text-muted-foreground">Foreground Target</span>
					<span>{newForegroundTarget}</span>
				{/if}
				{#if newBackgroundTarget}
					<span class="text-muted-foreground">Background Target</span>
					<span>{newBackgroundTarget}</span>
				{/if}
				{#if newPromoteTarget}
					<span class="text-muted-foreground">Promote Target</span>
					<span>{newPromoteTarget}</span>
				{/if}
				{#if newMetadataTarget}
					<span class="text-muted-foreground">Metadata Target</span>
					<span>{newMetadataTarget}</span>
				{/if}
				{#if newDataReplicas}
					<span class="text-muted-foreground">Data Replicas</span>
					<span>{newDataReplicas}</span>
				{/if}
				{#if newDirectIo}
					<span class="text-muted-foreground">Direct I/O</span>
					<span>Enabled</span>
				{/if}
				{#if newComments}
					<span class="text-muted-foreground">Comments</span>
					<span>{newComments}</span>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 2}>← Back</Button>
				<Button size="sm" onclick={createSubvolume}>Create Subvolume</Button>
			</div>
			{/if}
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if filesystems.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">No filesystems configured.</p>
		<Button size="sm" class="mt-2" onclick={() => window.location.href = '/filesystems'}>Go to Filesystems</Button>
	</div>
{:else if mountedFilesystems.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">No mounted filesystems.</p>
		<Button size="sm" class="mt-2" onclick={() => window.location.href = '/filesystems'}>Go to Filesystems</Button>
	</div>
{:else if subvolumes.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">No subvolumes in filesystem "{selectedFs}".</p>
		<p class="mt-1 text-sm text-muted-foreground">Use the <strong>Create Subvolume</strong> button above to get started.</p>
	</div>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={sortKey === 'name'} dir={sortDir} onclick={() => toggleSort('name')} />
				<SortTh label="Type" active={sortKey === 'type'} dir={sortDir} onclick={() => toggleSort('type')} />
				<SortTh label="Size" active={sortKey === 'size'} dir={sortDir} onclick={() => toggleSort('size')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Block Device</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Snapshots</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as sv}
				<tr class="border-b border-border">
					<td class="p-3">
						<button class="text-left hover:text-blue-400 transition-colors" onclick={() => openDetail(sv)}>
							<strong>{sv.name}</strong>
						</button>
						<span class="block font-mono text-xs text-muted-foreground">{sv.path}</span>
						{#if sv.comments}
							<span class="mt-0.5 block text-xs italic text-muted-foreground">{sv.comments}</span>
						{/if}
					</td>
					<td class="p-3">
						<Badge variant={sv.subvolume_type === 'filesystem' ? 'secondary' : 'outline'}
							class={sv.subvolume_type === 'filesystem' ? 'bg-blue-950 text-blue-400' : 'bg-purple-950 text-purple-400'}>
							{sv.subvolume_type === 'filesystem' ? 'File Share' : 'Block'}
						</Badge>
					</td>
					<td class="p-3 text-sm">
						{#if sv.volsize_bytes}
							{formatBytes(sv.volsize_bytes)}
							{#if sv.used_bytes !== null}
								<span class="text-xs text-muted-foreground">({formatBytes(sv.used_bytes)} used)</span>
							{/if}
						{:else if sv.used_bytes !== null}
							{formatBytes(sv.used_bytes)}
						{:else}
							—
						{/if}
					</td>
					<td class="p-3">
						{#if sv.subvolume_type === 'block'}
							{#if sv.block_device}
								<span class="font-mono text-xs">{sv.block_device}</span>
								<Button variant="destructive" size="xs" class="ml-2" onclick={() => detachSubvolume(sv.name)}>Detach</Button>
							{:else}
								<span class="text-muted-foreground">Detached</span>
								<Button variant="secondary" size="xs" class="ml-2" onclick={() => attachSubvolume(sv.name)}>Attach</Button>
							{/if}
						{:else}
							<span class="text-muted-foreground">N/A</span>
						{/if}
					</td>
					<td class="p-3">
						{#if sv.snapshots.length === 0}
							<span class="text-muted-foreground">None</span>
						{:else if sv.snapshots.length <= 2}
							{#each sv.snapshots as snap}
								<div class="my-0.5 flex items-center gap-2">
									<span class="font-mono text-xs">{snap}</span>
									<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(sv.name, snap)}>Delete</Button>
								</div>
							{/each}
						{:else}
							<button class="text-sm text-blue-400 hover:text-blue-300 transition-colors" onclick={() => { openDetail(sv); detailTab = 'snapshots'; }}>
								{sv.snapshots.length} snapshots
							</button>
						{/if}
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => openDetail(sv)}>
								{expandedName === sv.name ? 'Hide' : 'Details'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => deleteSubvolume(sv.name)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if expandedName === sv.name && detailSv}
					<tr>
						<td colspan="6" class="border-b border-border bg-muted/20 p-0">
							<div class="p-4">
								<!-- Tabs -->
								<div class="mb-4 flex border-b border-border">
									{#each [['info', 'Info'], ['snapshots', `Snapshots (${detailSv.snapshots.length})`], ['shares', `Shares${detailShareCount > 0 ? ` (${detailShareCount})` : ''}`], ['browse', 'Browse'], ['properties', 'Properties']] as [key, label]}
										<button
											onclick={() => detailTab = key as typeof detailTab}
											class="px-3 py-1.5 text-xs font-medium transition-colors {detailTab === key
												? 'border-b-2 border-primary text-foreground'
												: 'text-muted-foreground hover:text-foreground'}"
										>{label}</button>
									{/each}
								</div>

								{#if detailTab === 'info'}
									<div class="grid grid-cols-[auto_1fr_auto_1fr] gap-x-6 gap-y-1.5 text-sm">
										<span class="text-muted-foreground">Type</span>
										<span>
											<Badge variant={detailSv.subvolume_type === 'filesystem' ? 'secondary' : 'outline'}
												class={detailSv.subvolume_type === 'filesystem' ? 'bg-blue-950 text-blue-400' : 'bg-purple-950 text-purple-400'}>
												{detailSv.subvolume_type === 'filesystem' ? 'File Share' : 'Block'}
											</Badge>
										</span>
										<span class="text-muted-foreground">Path</span>
										<span class="font-mono text-xs">{detailSv.path}</span>

										<span class="text-muted-foreground">Compression</span>
										<span>
											{#if editingField === 'compression'}
												<span class="flex items-center gap-1">
													<select bind:value={editValue} class="h-7 rounded-md border border-input bg-transparent px-2 text-xs">
														<option value="">None</option>
														<option value="lz4">LZ4</option>
														<option value="zstd">Zstd</option>
														<option value="gzip">Gzip</option>
													</select>
													<button onclick={saveEdit} class="p-0.5 text-green-400 hover:text-green-300"><Check class="h-3.5 w-3.5" /></button>
													<button onclick={cancelEdit} class="p-0.5 text-muted-foreground hover:text-foreground"><X class="h-3.5 w-3.5" /></button>
												</span>
											{:else}
												<button class="group flex items-center gap-1 hover:text-blue-400 transition-colors" onclick={() => startEdit('compression')}>
													{detailSv.compression ?? 'None'}
													<Pencil class="h-3 w-3 opacity-0 group-hover:opacity-100 transition-opacity" />
												</button>
											{/if}
										</span>
										<span class="text-muted-foreground">{detailSv.subvolume_type === 'block' ? 'Size' : 'Quota'}</span>
										<span>
											{#if showResize}
												<span class="flex items-center gap-1">
													<Input type="number" bind:value={resizeValue} class="h-7 w-24 text-xs" placeholder="GiB" min="1"
														onkeydown={(e) => { if (e.key === 'Enter') resizeSubvolume(); if (e.key === 'Escape') showResize = false; }} />
													<button onclick={resizeSubvolume} class="p-0.5 text-green-400 hover:text-green-300"><Check class="h-3.5 w-3.5" /></button>
													<button onclick={() => showResize = false} class="p-0.5 text-muted-foreground hover:text-foreground"><X class="h-3.5 w-3.5" /></button>
												</span>
											{:else}
												<button class="group flex items-center gap-1 hover:text-blue-400 transition-colors" onclick={() => { showResize = true; resizeValue = detailSv?.volsize_bytes ? String(Math.round(detailSv.volsize_bytes / 1073741824)) : ''; }}>
													{detailSv.volsize_bytes ? formatBytes(detailSv.volsize_bytes) : 'No limit'}
													<Pencil class="h-3 w-3 opacity-0 group-hover:opacity-100 transition-opacity" />
												</button>
											{/if}
										</span>
										{#if detailSv.used_bytes !== null}
											<span class="text-muted-foreground">Used</span>
											<span>{formatBytes(detailSv.used_bytes)}</span>
										{/if}
										{#if detailSv.block_device}
											<span class="text-muted-foreground">Block Device</span>
											<span class="font-mono text-xs">{detailSv.block_device}</span>
										{/if}
										{#if detailSv.subvolume_type === 'block'}
											<span class="text-muted-foreground">Direct I/O</span>
											<span>{detailSv.direct_io ? 'Enabled' : 'Disabled'}</span>
										{/if}
										{#if detailSv.owner}
											<span class="text-muted-foreground">Owner</span>
											<span class="font-mono text-xs">{detailSv.owner}</span>
										{/if}
										{#if detailSv.parent}
											<span class="text-muted-foreground">Parent</span>
											<button class="font-mono text-xs text-blue-400 hover:text-blue-300 text-left" onclick={() => { const p = subvolumes.find(s => s.name === detailSv!.parent); if (p) openDetail(p); }}>{detailSv.parent}</button>
										{/if}
										{#if detailSv.bcachefs_options && Object.keys(detailSv.bcachefs_options).length > 0}
											<span class="text-muted-foreground">bcachefs Options</span>
											<div class="flex flex-wrap gap-1.5">
												{#each Object.entries(detailSv.bcachefs_options) as [key, value]}
													<span class="rounded border border-border px-1.5 py-0.5 text-[0.65rem] font-mono text-muted-foreground">{key}={value}</span>
												{/each}
											</div>
										{/if}
										{#if nestedSubvolumes.length > 0}
											<span class="text-muted-foreground">Nested Subvolumes</span>
											<div class="flex flex-col gap-0.5">
												{#each nestedSubvolumes as child}
													<span class="font-mono text-xs">{child}</span>
												{/each}
											</div>
										{/if}
										<span class="text-muted-foreground">Comments</span>
										<span>
											{#if editingField === 'comments'}
												<span class="flex items-center gap-1">
													<Input bind:value={editValue} class="h-7 text-xs" placeholder="Optional description" />
													<button onclick={saveEdit} class="p-0.5 text-green-400 hover:text-green-300"><Check class="h-3.5 w-3.5" /></button>
													<button onclick={cancelEdit} class="p-0.5 text-muted-foreground hover:text-foreground"><X class="h-3.5 w-3.5" /></button>
												</span>
											{:else}
												<button class="group flex items-center gap-1 text-xs hover:text-blue-400 transition-colors text-left" onclick={() => startEdit('comments')}>
													{detailSv.comments || '—'}
													<Pencil class="h-3 w-3 opacity-0 group-hover:opacity-100 transition-opacity shrink-0" />
												</button>
											{/if}
										</span>
									</div>
									<div class="mt-3 flex gap-2">
										<Button size="xs" variant="secondary" onclick={() => { showSnap = detailSv?.name ?? null; snapName = ''; }}>
											<Camera class="mr-1 h-3 w-3" />Snapshot
										</Button>
										<Button size="xs" variant="secondary" onclick={() => { showClone = detailSv?.name ?? null; cloneName = ''; }}>
											<Copy class="mr-1 h-3 w-3" />Clone
										</Button>
									</div>

								{:else if detailTab === 'snapshots'}
									{#if detailSv.snapshots.length === 0}
										<p class="text-sm text-muted-foreground">No snapshots.</p>
									{:else}
										<div class="space-y-1.5">
											{#each detailSnapshots.length > 0 ? detailSnapshots : detailSv.snapshots.map(s => ({ name: s, subvolume: detailSv!.name, filesystem: selectedFs, path: '', read_only: true, parent: null })) as snap}
												<div class="flex items-center justify-between rounded-md border border-border px-3 py-2">
													<div>
														<span class="font-mono text-xs">{snap.name}</span>
														<span class="ml-2 text-xs text-muted-foreground">{snap.read_only ? 'read-only' : 'writable'}</span>
													</div>
													<div class="flex gap-1">
														<Button variant="secondary" size="xs" onclick={() => { showClone = `${detailSv!.name}@${snap.name}`; cloneName = ''; }}>
															<Copy class="mr-1 h-3 w-3" />Clone
														</Button>
														<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(detailSv!.name, snap.name)}>
															<Trash2 class="h-3 w-3" />
														</Button>
													</div>
												</div>
											{/each}
										</div>
									{/if}

								{:else if detailTab === 'shares'}
									{#if detailShareCount === 0}
										<p class="text-sm text-muted-foreground">No shares linked to this subvolume.</p>
									{:else}
										<div class="space-y-1.5">
											{#each detailShares.nfs as share}
												<div class="flex items-center gap-2 rounded-md border border-border px-3 py-2">
													<Badge class="bg-green-950 text-green-400 text-[0.6rem]">NFS</Badge>
													<span class="font-mono text-xs">{share.path}</span>
													<span class="text-xs text-muted-foreground">{share.clients.length} client(s)</span>
												</div>
											{/each}
											{#each detailShares.smb as share}
												<div class="flex items-center gap-2 rounded-md border border-border px-3 py-2">
													<Badge class="bg-amber-950 text-amber-400 text-[0.6rem]">SMB</Badge>
													<span class="text-sm">{share.name}</span>
													<span class="text-xs text-muted-foreground">{share.guest_ok ? 'guest' : share.valid_users.join(', ') || 'auth'}</span>
												</div>
											{/each}
											{#each detailShares.iscsi as target}
												<div class="flex items-center gap-2 rounded-md border border-border px-3 py-2">
													<Badge class="bg-purple-950 text-purple-400 text-[0.6rem]">iSCSI</Badge>
													<span class="font-mono text-xs truncate">{target.iqn}</span>
												</div>
											{/each}
											{#each detailShares.nvmeof as sub}
												<div class="flex items-center gap-2 rounded-md border border-border px-3 py-2">
													<Badge class="bg-cyan-950 text-cyan-400 text-[0.6rem]">NVMe-oF</Badge>
													<span class="font-mono text-xs truncate">{sub.nqn}</span>
												</div>
											{/each}
										</div>
									{/if}

								{:else if detailTab === 'browse'}
									<div class="space-y-3">
										{#if detailParentChain.length > 0 || detailSv.parent}
											<div>
												<h4 class="mb-1 text-xs font-semibold uppercase text-muted-foreground">Lineage</h4>
												{#each detailParentChain as ancestor, i}
													<div class="flex items-center gap-1" style="padding-left: {i * 16}px">
														<span class="text-muted-foreground text-xs">└─</span>
														<button class="font-mono text-xs text-blue-400 hover:text-blue-300" onclick={() => { const s = subvolumes.find(x => x.name === ancestor); if (s) openDetail(s); }}>{ancestor}</button>
													</div>
												{/each}
												<div class="flex items-center gap-1" style="padding-left: {detailParentChain.length * 16}px">
													<span class="text-muted-foreground text-xs">└─</span>
													<span class="font-mono text-xs font-semibold">{detailSv.name}</span>
													<Badge variant="outline" class="text-[0.55rem]">current</Badge>
												</div>
											</div>
										{:else}
											<div class="flex items-center gap-1">
												<span class="font-mono text-xs font-semibold">{detailSv.name}</span>
												<Badge variant="outline" class="text-[0.55rem]">root</Badge>
											</div>
										{/if}
										{#if detailChildren.length > 0}
											<div>
												<h4 class="mb-1 text-xs font-semibold uppercase text-muted-foreground">Children ({detailChildren.length})</h4>
												{#each detailChildren as child}
													<div class="flex items-center gap-2 rounded-md border border-border px-3 py-1.5 mb-1">
														<Badge class="{child.type === 'snapshot' ? 'bg-amber-950 text-amber-400' : 'bg-green-950 text-green-400'} text-[0.55rem]">{child.type}</Badge>
														{#if child.type === 'clone'}
															<button class="font-mono text-xs text-blue-400 hover:text-blue-300" onclick={() => { const s = subvolumes.find(x => x.name === child.name); if (s) openDetail(s); }}>{child.name}</button>
														{:else}
															<span class="font-mono text-xs">{child.name}</span>
														{/if}
													</div>
												{/each}
											</div>
										{:else}
											<p class="text-xs text-muted-foreground">No children.</p>
										{/if}
									</div>

								{:else if detailTab === 'properties'}
									{#if detailSv.properties && Object.keys(detailSv.properties).length > 0}
										<div class="space-y-1">
											{#each Object.entries(detailSv.properties).sort(([a], [b]) => a.localeCompare(b)) as [key, value]}
												<div class="flex items-start justify-between gap-2 rounded-md border border-border px-3 py-1.5">
													<span class="font-mono text-xs text-muted-foreground break-all">{key}</span>
													<span class="font-mono text-xs text-right break-all">{value}</span>
												</div>
											{/each}
										</div>
									{:else}
										<p class="text-sm text-muted-foreground">No properties.</p>
									{/if}
								{/if}
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}


{/if}

{#if pageTab === 'snapshots'}

{#if mountedFilesystems.length > 0}
	<div class="mb-4 flex items-center gap-4">
		<select value={selectedFs} onchange={(e) => selectFs((e.target as HTMLSelectElement).value)} class="h-9 w-auto rounded-md border border-input bg-transparent px-3 text-sm">
			{#each mountedFilesystems as p}
				<option value={p.name}>{p.name}</option>
			{/each}
		</select>
		<Input bind:value={snapshotSearch} placeholder="Search snapshots..." class="h-9 w-48" />
	</div>
{/if}

{#if snapshotsLoading}
	<p class="text-muted-foreground">Loading snapshots...</p>
{:else if !selectedFs}
	<p class="text-muted-foreground">No mounted filesystems.</p>
{:else if filteredSnapshots.length === 0}
	<p class="text-muted-foreground">{snapshotSearch ? 'No matching snapshots.' : `No snapshots in filesystem "${selectedFs}".`}</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Snapshot Name</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Parent Subvolume</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Read-only</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Type</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each filteredSnapshots as snap}
				<tr class="border-b border-border">
					<td class="p-3">
						<span class="font-mono text-sm">{snap.name}</span>
						{#if snap.path}
							<span class="block font-mono text-xs text-muted-foreground">{snap.path}</span>
						{/if}
					</td>
					<td class="p-3">
						{#if subvolumes.find(sv => sv.name === snap.subvolume)}
							<button
								class="font-mono text-sm text-blue-400 hover:text-blue-300 transition-colors"
								onclick={() => switchToSubvolumeAndExpand(snap.subvolume)}
							>{snap.subvolume}</button>
						{:else}
							<span class="flex items-center gap-1.5">
								<span class="font-mono text-sm text-muted-foreground">{snap.subvolume}</span>
								<Badge variant="outline" class="bg-amber-950 text-amber-400 text-[0.6rem]">
									<AlertTriangle class="mr-0.5 h-2.5 w-2.5" />deleted
								</Badge>
							</span>
						{/if}
					</td>
					<td class="p-3">
						{#if snap.read_only}
							<Badge class="bg-green-950 text-green-400">read-only</Badge>
						{:else}
							<Badge variant="outline" class="text-muted-foreground">writable</Badge>
						{/if}
					</td>
					<td class="p-3">
						<Badge variant="secondary" class="bg-amber-950 text-amber-400">snapshot</Badge>
					</td>
					<td class="p-3">
						<Button variant="destructive" size="xs" onclick={() => deleteSnapshotFromTab(snap.subvolume, snap.name)}>
							<Trash2 class="mr-1 h-3 w-3" />Delete
						</Button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

{/if}

<Dialog.Root open={showSnap !== null} onOpenChange={(open) => { if (!open) showSnap = null; }}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Snapshot "{showSnap}"</Dialog.Title>
			<p class="text-sm text-muted-foreground">Create a read-only point-in-time copy.</p>
		</Dialog.Header>
		<div class="mb-4">
			<Label for="snap-name">Snapshot Name</Label>
			<Input id="snap-name" bind:value={snapName} placeholder="snap-2026-03-12" class="mt-1" />
		</div>
		<Dialog.Footer>
			<Button size="sm" onclick={createSnapshot} disabled={!snapName}>Create</Button>
			<Button variant="secondary" size="sm" onclick={() => showSnap = null}>Cancel</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>

<Dialog.Root open={showClone !== null} onOpenChange={(open) => { if (!open) showClone = null; }}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Clone "{showClone}"</Dialog.Title>
			<p class="text-sm text-muted-foreground">Create a writable copy (COW — instant, shares data until modified).</p>
		</Dialog.Header>
		<div class="mb-4">
			<Label for="clone-name">Clone Name</Label>
			<Input id="clone-name" bind:value={cloneName} placeholder="my-clone" class="mt-1" />
		</div>
		<Dialog.Footer>
			<Button size="sm" onclick={cloneSubvolume} disabled={!cloneName}>Create</Button>
			<Button variant="secondary" size="sm" onclick={() => showClone = null}>Cancel</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>

