<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { formatBytes } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import type { Filesystem, Subvolume, SubvolumeDependents, Snapshot, SubvolumeType, NfsShare, SmbShare, IscsiTarget, NvmeofSubsystem, App, AppsStatus, VmStatus } from '$lib/types';
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
	// Empty string = "All filesystems" overview (default). A non-empty value
	// narrows the list to that filesystem only. Switching is filtering, not
	// loading: every mounted FS is fetched on every refresh, so changing the
	// dropdown is instant.
	let selectedFs = $state('');
	// Filesystem picked in the create wizard. Must be a real FS name when the
	// user submits — overview mode doesn't know where to create otherwise.
	let newFilesystem = $state('');

	// Snapshots tab state
	let allSnapshots: Snapshot[] = $state([]);
	let snapshotsLoading = $state(false);
	let snapshotSearch = $state('');

	async function loadSnapshots() {
		const mounted = filesystems.filter(p => p.mounted);
		if (mounted.length === 0) {
			allSnapshots = [];
			return;
		}
		snapshotsLoading = true;
		try {
			const results = await Promise.all(
				mounted.map(fs =>
					client.call<Snapshot[]>('snapshot.list', { filesystem: fs.name }).catch(() => [])
				)
			);
			allSnapshots = results.flat();
		} catch {
			allSnapshots = [];
		}
		snapshotsLoading = false;
	}

	const filteredSnapshots = $derived.by(() => {
		let snaps = allSnapshots;
		if (selectedFs) snaps = snaps.filter(s => s.filesystem === selectedFs);
		if (snapshotSearch.trim()) {
			const q = snapshotSearch.toLowerCase();
			snaps = snaps.filter(s =>
				s.name.toLowerCase().includes(q) ||
				s.subvolume.toLowerCase().includes(q)
			);
		}
		return snaps;
	});

	function switchToSubvolumeAndExpand(filesystem: string, subvolumeName: string) {
		pageTab = 'subvolumes';
		history.replaceState(null, '', '#subvolumes');
		const sv = subvolumes.find(s => s.filesystem === filesystem && s.name === subvolumeName);
		if (sv) {
			openDetail(sv);
		}
	}

	async function deleteSnapshotFromTab(filesystem: string, subvolume: string, snap: string) {
		if (!await confirm(`Delete snapshot "${snap}"?`)) return;
		await withToast(
			() => client.call('snapshot.delete', {
				filesystem,
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
	let showAdvancedStorage = $state(false);

	const WIZARD_STEPS: [string, string][] = [
		['1', 'Basic'],
		['2', 'Storage'],
		['3', 'Review'],
	];

	/** Reset every wizard field to its declared default. One source of
	 * truth keeps the open-wizard path and the post-create cleanup
	 * from drifting — the original `openWizard` only reset 6 fields
	 * while the post-create block reset 11, so target-label fields
	 * persisted across re-opens until this consolidation. */
	function resetCreateForm() {
		newName = '';
		newType = 'filesystem';
		newVolsize = '';
		newCompression = '';
		newForegroundTarget = '';
		newBackgroundTarget = '';
		newPromoteTarget = '';
		newMetadataTarget = '';
		newDataReplicas = '';
		newComments = '';
		newDirectIo = false;
		showAdvancedStorage = false;
	}

	function openWizard() {
		wizardStep = 1;
		resetCreateForm();
		// Default the wizard's FS picker to the active filter (if any),
		// otherwise the first mounted FS so the user can hit Next
		// immediately on single-FS setups.
		const mounted = filesystems.filter(p => p.mounted);
		if (selectedFs && mounted.some(fs => fs.name === selectedFs)) {
			newFilesystem = selectedFs;
		} else if (mounted.length > 0) {
			newFilesystem = mounted[0].name;
		} else {
			newFilesystem = '';
		}
	}

	// Snapshot / clone dialogs carry the source subvolume (not just its name)
	// so the actions know which filesystem to operate on once the overview
	// view can mix subvolumes from multiple filesystems.
	interface SnapSource { filesystem: string; name: string }
	interface CloneSource { filesystem: string; name: string; snapshot?: string }
	let showSnap = $state<SnapSource | null>(null);
	let snapName = $state('');
	let showClone = $state<CloneSource | null>(null);
	let cloneName = $state('');
	let showResize = $state(false);
	let resizeValue = $state('');

	async function resizeSubvolume() {
		if (!detailSv || !resizeValue) return;
		const bytes = parseFloat(resizeValue) * 1073741824;
		const target = detailSv;
		await withToast(
			() => client.call('subvolume.resize', { filesystem: target.filesystem, name: target.name, volsize_bytes: bytes }),
			`${target.subvolume_type === 'block' ? 'Volume' : 'Quota'} updated to ${resizeValue} GiB`
		);
		showResize = false;
		resizeValue = '';
		await refresh();
		const updated = subvolumes.find(sv => sv.filesystem === target.filesystem && sv.name === target.name);
		if (updated) detailSv = updated;
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
		const target = detailSv;
		const params: Record<string, string> = {
			filesystem: target.filesystem,
			name: target.name,
		};
		params[editingField] = editValue;
		const ok = await withToast(
			() => client.call('subvolume.update', params),
			`${editingField === 'compression' ? 'Compression' : 'Comments'} updated`
		);
		if (ok !== undefined) {
			editingField = null;
			await refresh();
			const updated = subvolumes.find(sv => sv.filesystem === target.filesystem && sv.name === target.name);
			if (updated) detailSv = updated;
		}
	}

	function cancelEdit() {
		editingField = null;
	}

	// Consumers linked to the detail subvolume
	interface LinkedShares {
		nfs: NfsShare[];
		smb: SmbShare[];
		iscsi: IscsiTarget[];
		nvmeof: NvmeofSubsystem[];
		apps: App[];
		vms: VmStatus[];
	}
	let detailShares = $state<LinkedShares>({ nfs: [], smb: [], iscsi: [], nvmeof: [], apps: [], vms: [] });
	// All consumers loaded once at page level so the overview usage column
	// and each detail panel can index them without re-fetching.  Shares,
	// VMs and the apps roster + apps.storage_path all live here.
	let allNfs: NfsShare[] = $state([]);
	let allSmb: SmbShare[] = $state([]);
	let allIscsi: IscsiTarget[] = $state([]);
	let allNvmeof: NvmeofSubsystem[] = $state([]);
	let allVms: VmStatus[] = $state([]);
	let allApps: App[] = $state([]);
	let appsStoragePath: string | null = $state(null);
	/** Engine-computed per-subvolume usage. Source of truth for the
	 * Usage column — see `subvolume.list_dependents` in
	 * nasty-engine/src/subvolume_dependents.rs. The local consumer
	 * lists above (allNfs/allSmb/...) are kept for the detail pane,
	 * which still wants full entity objects for drilldown, but
	 * counts/names in the table proper come from this server-side
	 * compute. Keyed by subvolume path for O(1) lookup per row. */
	let dependentsByPath = $state(new Map<string, SubvolumeDependents>());

	async function loadShares() {
		const [nfs, smb, iscsi, nvmeof, vms, appsList, appsStat, deps] = await Promise.allSettled([
			client.call<NfsShare[]>('share.nfs.list'),
			client.call<SmbShare[]>('share.smb.list'),
			client.call<IscsiTarget[]>('share.iscsi.list'),
			client.call<NvmeofSubsystem[]>('share.nvmeof.list'),
			client.call<VmStatus[]>('vm.list'),
			client.call<App[]>('apps.list'),
			client.call<AppsStatus>('apps.status'),
			client.call<SubvolumeDependents[]>('subvolume.list_dependents'),
		]);
		allNfs = nfs.status === 'fulfilled' ? nfs.value : [];
		allSmb = smb.status === 'fulfilled' ? smb.value : [];
		allIscsi = iscsi.status === 'fulfilled' ? iscsi.value : [];
		allNvmeof = nvmeof.status === 'fulfilled' ? nvmeof.value : [];
		allVms = vms.status === 'fulfilled' ? vms.value : [];
		allApps = appsList.status === 'fulfilled' ? appsList.value : [];
		appsStoragePath = appsStat.status === 'fulfilled' ? (appsStat.value.storage_path ?? null) : null;
		const next = new Map<string, SubvolumeDependents>();
		if (deps.status === 'fulfilled') {
			for (const d of deps.value) next.set(d.path, d);
		}
		dependentsByPath = next;
	}

	// Tree: find parent chain and children for the selected subvolume
	const detailParentChain = $derived.by((): string[] => {
		if (!detailSv) return [];
		const chain: string[] = [];
		let current = detailSv.parent;
		const seen = new Set<string>();
		const fs = detailSv.filesystem;
		while (current && !seen.has(current)) {
			seen.add(current);
			chain.unshift(current);
			const parentSv = subvolumes.find(sv => sv.filesystem === fs && sv.name === current);
			current = parentSv?.parent ?? null;
		}
		return chain;
	});

	const detailChildren = $derived.by((): { name: string; type: 'clone' | 'snapshot' }[] => {
		if (!detailSv) return [];
		const result: { name: string; type: 'clone' | 'snapshot' }[] = [];
		// Writable clones: subvolumes whose parent is this subvolume.
		// Scope to the same filesystem — clones and parents always share one.
		for (const sv of subvolumes) {
			if (sv.filesystem === detailSv.filesystem && sv.parent === detailSv.name) {
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
		detailShares.iscsi.length + detailShares.nvmeof.length +
		detailShares.apps.length + detailShares.vms.length
	);

	function svKey(sv: { filesystem: string; name: string }): string {
		return `${sv.filesystem}|${sv.name}`;
	}

	async function openDetail(sv: Subvolume) {
		const key = svKey(sv);
		if (expandedName === key) {
			expandedName = null;
			detailSv = null;
			return;
		}
		expandedName = key;
		detailSv = sv;
		detailTab = 'info';
		detailSnapshots = [];
		nestedSubvolumes = [];
		detailShares = { nfs: [], smb: [], iscsi: [], nvmeof: [], apps: [], vms: [] };

		// Snapshots and children are per-FS API calls; share lists are
		// page-level state (loadShares) so we just index them here.
		const [snapResult, childrenResult] = await Promise.allSettled([
			client.call<Snapshot[]>('snapshot.list', { filesystem: sv.filesystem }),
			client.call<string[]>('subvolume.children', { filesystem: sv.filesystem, name: sv.name }),
		]);

		if (snapResult.status === 'fulfilled') {
			detailSnapshots = snapResult.value.filter(s => s.subvolume === sv.name);
		}

		const svPath = sv.path;
		const blockDev = sv.block_device;

		detailShares = {
			nfs: allNfs.filter(s => s.path === svPath),
			smb: allSmb.filter(s => s.path === svPath),
			iscsi: allIscsi.filter(t =>
				blockDev != null && t.luns.some(l => l.backstore_path === blockDev)),
			nvmeof: allNvmeof.filter(sub =>
				blockDev != null && sub.namespaces.some(ns => ns.device_path === blockDev)),
			apps: appsStoragePath && pathIsUnder(appsStoragePath, svPath) ? allApps : [],
			vms: allVms.filter(vm =>
				vm.disks.some(d =>
					(blockDev != null && d.path === blockDev) || pathIsUnder(d.path, svPath)
				)
			),
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
		// Share changes invalidate the usage column — reload so the badges stay accurate.
		if (p?.collection === 'nfs' || p?.collection === 'smb' || p?.collection === 'iscsi' || p?.collection === 'nvmeof') {
			loadShares();
		}
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		filesystems = await client.call<Filesystem[]>('fs.list');
		const mounted = filesystems.filter(p => p.mounted);
		if (mounted.length > 0) {
			await Promise.all([refresh(), loadShares()]);
			if (pageTab === 'snapshots') await loadSnapshots();
		}
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		const mounted = filesystems.filter(p => p.mounted);
		if (mounted.length === 0) {
			subvolumes = [];
			return;
		}
		await withToast(async () => {
			const results = await Promise.all(
				mounted.map(fs =>
					client.call<Subvolume[]>('subvolume.list', { filesystem: fs.name }).catch(() => [])
				)
			);
			subvolumes = results.flat();
		});
	}

	// Changing the filter is pure filtering — the underlying data is already
	// loaded for every mounted filesystem. Just keep the snapshots tab in
	// sync if it's the active tab (its derived filter does the rest).
	function selectFs(name: string) {
		selectedFs = name;
	}

	async function createSubvolume() {
		if (!newName || !newFilesystem) return;
		if (newType === 'block' && !newVolsize) return;

		const params: Record<string, unknown> = {
			filesystem: newFilesystem,
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
			resetCreateForm();
			await refresh();
		}
	}

	const SYSTEM_SUBVOLUMES: Record<string, string> = {
		'apps': 'Docker apps and container data',
		'vms': 'Virtual machine images and disk storage',
	};

	async function deleteSubvolume(sv: Subvolume) {
		const systemUse = SYSTEM_SUBVOLUMES[sv.name];
		// Check for child subvolumes
		let children: string[] = [];
		try { children = await client.call<string[]>('subvolume.children', { filesystem: sv.filesystem, name: sv.name }); } catch {}
		let warning = '';
		if (systemUse) {
			warning += `This subvolume is used by the system for: ${systemUse}. Deleting it may break functionality. `;
		}
		if (children.length > 0) {
			warning += `This will also delete ${children.length} nested subvolume${children.length > 1 ? 's' : ''}: ${children.join(', ')}. `;
		}
		warning += 'All snapshots will also be deleted.';
		if (!await confirm(`Delete "${sv.name}"?`, warning)) return;
		await withToast(
			() => client.call('subvolume.delete', { filesystem: sv.filesystem, name: sv.name }),
			`Subvolume "${sv.name}" deleted`
		);
		await refresh();
	}

	async function attachSubvolume(sv: Subvolume) {
		await withToast(
			() => client.call('subvolume.attach', { filesystem: sv.filesystem, name: sv.name }),
			`Loop device attached for "${sv.name}"`
		);
		await refresh();
	}

	async function detachSubvolume(sv: Subvolume) {
		if (!await confirm(`Detach loop device for "${sv.name}"?`, 'Any active iSCSI/NVMe-oF connections using this device will break.')) return;
		await withToast(
			() => client.call('subvolume.detach', { filesystem: sv.filesystem, name: sv.name }),
			`Loop device detached for "${sv.name}"`
		);
		await refresh();
	}

	async function createSnapshot() {
		if (!showSnap || !snapName) return;
		const src = showSnap;
		const ok = await withToast(
			() => client.call('snapshot.create', {
				filesystem: src.filesystem,
				subvolume: src.name,
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
			const updated = subvolumes.find(sv => sv.filesystem === detailSv!.filesystem && sv.name === detailSv!.name);
			if (updated) detailSv = updated;
		}
	}

	async function cloneSubvolume() {
		if (!showClone || !cloneName) return;
		const src = showClone;
		const ok = src.snapshot
			? await withToast(() =>
				client.call('snapshot.clone', {
					filesystem: src.filesystem,
					subvolume: src.name,
					snapshot: src.snapshot,
					new_name: cloneName,
				}),
				`Clone "${cloneName}" created from snapshot`)
			: await withToast(
				() => client.call('subvolume.clone', {
					filesystem: src.filesystem,
					name: src.name,
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
				const updated = subvolumes.find(sv => sv.filesystem === detailSv!.filesystem && sv.name === detailSv!.name);
				if (updated) openDetail(updated);
			}
		}
	}

	async function deleteSnapshot(sv: Subvolume, snap: string) {
		if (!await confirm(`Delete snapshot "${snap}"?`)) return;
		await withToast(
			() => client.call('snapshot.delete', {
				filesystem: sv.filesystem,
				subvolume: sv.name,
				name: snap,
			}),
			`Snapshot "${snap}" deleted`
		);
		await refresh();
		if (detailSv) {
			const updated = subvolumes.find(s => s.filesystem === detailSv!.filesystem && s.name === detailSv!.name);
			if (updated) detailSv = updated;
		}
	}

	const mountedFilesystems = $derived(filesystems.filter(p => p.mounted));

	// Per-subvolume usage tally: what shares / apps / VMs reference it.
	// Keyed by `fs|name`. Recomputed whenever subvolumes or consumers
	// change.  `system` is the hint string from SYSTEM_SUBVOLUMES — only
	// shown when there's no live consumer (otherwise the live count is
	// the more useful signal).
	interface SubvolumeUsage {
		system: string | null;     // SYSTEM_SUBVOLUMES description, or null
		nfs: number;
		smb: number;
		iscsi: number;
		nvmeof: number;
		apps: number;
		vms: number;
		backups: number;
	}
	// Counts come from `subvolume.list_dependents` (engine-side, matches
	// `fs.dependents` patterns including longest-prefix path attribution
	// for nested subvolumes). The `system` badge stays client-side because
	// it's a static SYSTEM_SUBVOLUMES table, not engine-derived state.
	const subvolumeUsage = $derived.by(() => {
		const out = new Map<string, SubvolumeUsage>();
		for (const sv of subvolumes) {
			const d = dependentsByPath.get(sv.path);
			out.set(svKey(sv), {
				system: SYSTEM_SUBVOLUMES[sv.name] ?? null,
				nfs: d?.nfs_shares.length ?? 0,
				smb: d?.smb_shares.length ?? 0,
				iscsi: d?.iscsi_targets.length ?? 0,
				nvmeof: d?.nvmeof_subsystems.length ?? 0,
				apps: d?.apps.length ?? 0,
				vms: d?.vms.length ?? 0,
				backups: d?.backup_jobs.length ?? 0,
			});
		}
		return out;
	});

	// True when `path` is either exactly `root` or sits below it
	// (`root/...`).  Used to decide whether a consumer's mount point
	// falls inside a given subvolume.
	function pathIsUnder(path: string, root: string): boolean {
		return path === root || path.startsWith(root.endsWith('/') ? root : root + '/');
	}

	function usageFor(sv: Subvolume): SubvolumeUsage {
		return subvolumeUsage.get(svKey(sv)) ?? { system: null, nfs: 0, smb: 0, iscsi: 0, nvmeof: 0, apps: 0, vms: 0, backups: 0 };
	}

	// Pick a ceiling to scale the disk-usage bar against:
	// - block subvolumes: the configured volsize is the hard cap
	// - filesystem subvolumes with a quota: that quota
	// - filesystem subvolumes without a quota: filesystem total — the bar
	//   then reads as "% of FS occupied by this subvolume", which is the
	//   only meaningful denominator we have
	function sizeDisplay(sv: Subvolume): { used: number | null; ceiling: number | null; source: 'volsize' | 'quota' | 'filesystem' | null } {
		if (sv.subvolume_type === 'block') {
			return { used: sv.used_bytes, ceiling: sv.volsize_bytes, source: sv.volsize_bytes ? 'volsize' : null };
		}
		if (sv.quota_bytes != null) {
			return { used: sv.used_bytes, ceiling: sv.quota_bytes, source: 'quota' };
		}
		const fsTotal = filesystems.find(f => f.name === sv.filesystem)?.total_bytes ?? null;
		return { used: sv.used_bytes, ceiling: fsTotal && fsTotal > 0 ? fsTotal : null, source: fsTotal && fsTotal > 0 ? 'filesystem' : null };
	}

	// Unique device labels from the filesystem chosen in the create wizard
	// (for tiering dropdowns). Tracks `newFilesystem`, not the page filter,
	// so the wizard reflects the FS the user is creating in.
	const deviceLabels = $derived(() => {
		const fs = filesystems.find(f => f.name === newFilesystem);
		if (!fs) return [];
		const labels = fs.devices.map(d => d.label).filter((l): l is string => !!l);
		return [...new Set(labels)].sort();
	});

	// Filesystem-level defaults so "Inherit" options can show what they'll
	// actually resolve to. Same target as deviceLabels — the FS the wizard
	// will create the subvolume in, not whatever the page filter shows.
	const fsDefaults = $derived(() =>
		filesystems.find(f => f.name === newFilesystem)?.options ?? null
	);
	function inheritLabel(value: string | number | null | undefined): string {
		return value == null || value === '' ? 'Inherit' : `Inherit (${value})`;
	}

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

	const filtered = $derived.by(() => {
		let items = subvolumes;
		if (selectedFs) items = items.filter(sv => sv.filesystem === selectedFs);
		if (search.trim()) {
			const q = search.toLowerCase();
			items = items.filter(sv =>
				sv.name.toLowerCase().includes(q) ||
				sv.comments?.toLowerCase().includes(q)
			);
		}
		return items;
	});

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

	// One section per filesystem in overview mode; one flat section when the
	// user has filtered to a specific FS or only one FS exists. `fs === null`
	// is the flat case so the template can decide whether to draw a header.
	const subvolumeGroups = $derived.by((): { fs: string | null; items: Subvolume[] }[] => {
		if (selectedFs || mountedFilesystems.length <= 1) {
			return [{ fs: null, items: sorted }];
		}
		const byFs = new Map<string, Subvolume[]>();
		for (const sv of sorted) {
			let bucket = byFs.get(sv.filesystem);
			if (!bucket) { bucket = []; byFs.set(sv.filesystem, bucket); }
			bucket.push(sv);
		}
		return mountedFilesystems
			.filter(fs => byFs.has(fs.name))
			.map(fs => ({ fs: fs.name, items: byFs.get(fs.name)! }));
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
		{#if mountedFilesystems.length > 1}
			<select value={selectedFs} onchange={(e) => selectFs((e.target as HTMLSelectElement).value)} class="h-9 w-auto rounded-md border border-input bg-transparent px-3 text-sm">
				<option value="">All filesystems</option>
				{#each mountedFilesystems as p}
					<option value={p.name}>{p.name}</option>
				{/each}
			</select>
		{/if}
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
			{#if mountedFilesystems.length > 1}
				<div class="mb-4">
					<Label for="sv-filesystem">Filesystem {#if !newFilesystem}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
					<select id="sv-filesystem" bind:value={newFilesystem} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm {requiredFieldCls(!newFilesystem)}">
						{#each mountedFilesystems as p}
							<option value={p.name}>{p.name}</option>
						{/each}
					</select>
				</div>
			{/if}
			<div class="mb-4">
				<Label for="sv-name">Name {#if !newName}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
				<Input id="sv-name" bind:value={newName} placeholder="documents or projects/web" class="mt-1 {requiredFieldCls(!newName)}" />
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
				<Button size="sm" onclick={() => wizardStep = 2} disabled={!newName || !newFilesystem}>Next: Storage →</Button>
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
			<div class="mb-4">
				<button
					type="button"
					onclick={() => showAdvancedStorage = !showAdvancedStorage}
					class="flex items-center gap-1 text-sm font-medium text-muted-foreground hover:text-foreground"
				>
					<span class="inline-block w-3 text-xs">{showAdvancedStorage ? '▾' : '▸'}</span>
					Advanced storage options
				</button>
				{#if showAdvancedStorage}
					<div class="mt-3 space-y-4 rounded-md border border-border bg-secondary/20 p-3">
						{#if deviceLabels().length > 0}
							<div>
								<Label>Tiering Targets</Label>
								<p class="mb-2 text-xs text-muted-foreground">Override filesystem defaults. Leave empty to inherit.</p>
								<div class="grid grid-cols-2 gap-2">
									<div>
										<label for="sv-fg-target" class="mb-1 block text-xs text-muted-foreground">Foreground Target</label>
										<select id="sv-fg-target" bind:value={newForegroundTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
											<option value="">{inheritLabel(fsDefaults()?.foreground_target)}</option>
											{#each deviceLabels() as label}
												<option value={label}>{label}</option>
											{/each}
										</select>
									</div>
									<div>
										<label for="sv-meta-target" class="mb-1 block text-xs text-muted-foreground">Metadata Target</label>
										<select id="sv-meta-target" bind:value={newMetadataTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
											<option value="">{inheritLabel(fsDefaults()?.metadata_target)}</option>
											{#each deviceLabels() as label}
												<option value={label}>{label}</option>
											{/each}
										</select>
									</div>
									<div>
										<label for="sv-bg-target" class="mb-1 block text-xs text-muted-foreground">Background Target</label>
										<select id="sv-bg-target" bind:value={newBackgroundTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
											<option value="">{inheritLabel(fsDefaults()?.background_target)}</option>
											{#each deviceLabels() as label}
												<option value={label}>{label}</option>
											{/each}
										</select>
									</div>
									<div>
										<label for="sv-promote-target" class="mb-1 block text-xs text-muted-foreground">Promote Target</label>
										<select id="sv-promote-target" bind:value={newPromoteTarget} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
											<option value="">{inheritLabel(fsDefaults()?.promote_target)}</option>
											{#each deviceLabels() as label}
												<option value={label}>{label}</option>
											{/each}
										</select>
									</div>
								</div>
							</div>
						{/if}
						<div>
							<label for="sv-data-replicas" class="mb-1 block text-xs text-muted-foreground">Data Replicas</label>
							<select id="sv-data-replicas" bind:value={newDataReplicas} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
								<option value="">{inheritLabel(fsDefaults()?.data_replicas)}</option>
								<option value="1">1</option>
								<option value="2">2</option>
								<option value="3">3</option>
								<option value="4">4</option>
							</select>
							<p class="mt-1 text-xs text-muted-foreground">Number of data copies kept on this subvolume.</p>
						</div>
						{#if newType === 'block'}
							<div>
								<label class="flex cursor-pointer items-center gap-2 text-sm font-medium">
									<input type="checkbox" bind:checked={newDirectIo} class="h-4 w-4" />
									Direct I/O (O_DIRECT)
								</label>
								<p class="mt-1 text-xs text-muted-foreground">Bypass host page cache for the backing file. Reduces double-caching when the client (iSCSI/NVMe-oF) manages its own cache.</p>
							</div>
						{/if}
					</div>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 1}>← Back</Button>
				<Button size="sm" onclick={() => wizardStep = 3} disabled={newType === 'block' && !newVolsize}>Next: Review →</Button>
			</div>

			<!-- Step 3: Review -->
			{:else if wizardStep === 3}
			<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
				<span class="text-muted-foreground">Filesystem</span>
				<span class="font-mono">{newFilesystem}</span>
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
		<Button size="sm" class="mt-2" onclick={() => goto('/filesystems')}>Filesystems</Button>
	</div>
{:else if mountedFilesystems.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">No mounted filesystems.</p>
		<Button size="sm" class="mt-2" onclick={() => goto('/filesystems')}>Filesystems</Button>
	</div>
{:else if filtered.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		{#if search.trim()}
			<p class="text-muted-foreground">No subvolumes matching "{search}".</p>
		{:else if selectedFs}
			<p class="text-muted-foreground">No subvolumes in filesystem "{selectedFs}".</p>
			<p class="mt-1 text-sm text-muted-foreground">Use the <strong>Create Subvolume</strong> button above to get started.</p>
		{:else}
			<p class="text-muted-foreground">No subvolumes yet.</p>
			<p class="mt-1 text-sm text-muted-foreground">Use the <strong>Create Subvolume</strong> button above to get started.</p>
		{/if}
	</div>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={sortKey === 'name'} dir={sortDir} onclick={() => toggleSort('name')} />
				<SortTh label="Type" active={sortKey === 'type'} dir={sortDir} onclick={() => toggleSort('type')} />
				<SortTh label="Size" active={sortKey === 'size'} dir={sortDir} onclick={() => toggleSort('size')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Used by</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Block Device</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Snapshots</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each subvolumeGroups as group (group.fs ?? '__flat__')}
				{#if group.fs}
					<tr class="bg-muted/30">
						<td colspan="7" class="border-b border-border px-3 py-2">
							<span class="font-semibold">{group.fs}</span>
							<span class="ml-2 text-xs text-muted-foreground">{group.items.length} subvolume{group.items.length === 1 ? '' : 's'}</span>
						</td>
					</tr>
				{/if}
				{#each group.items as sv (svKey(sv))}
					{@const usage = usageFor(sv)}
					{@const size = sizeDisplay(sv)}
					{@const sizePct = size.used != null && size.ceiling ? Math.min(100, Math.max(0, (size.used / size.ceiling) * 100)) : 0}
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
						<td class="p-3 text-sm w-48">
							{#if size.used == null}
								<span class="text-muted-foreground">—</span>
							{:else if size.ceiling == null}
								{formatBytes(size.used)}
							{:else}
								<div class="space-y-0.5">
									<div class="flex items-baseline justify-between gap-2 text-xs">
										<span class="font-mono">{formatBytes(size.used)}</span>
										<span class="text-muted-foreground" title={size.source === 'filesystem' ? `${sizePct.toFixed(1)}% of filesystem total` : `${sizePct.toFixed(1)}% of ${formatBytes(size.ceiling)} ${size.source === 'volsize' ? 'volume' : 'quota'}`}>
											{sizePct < 1 && size.used > 0 ? '<1' : sizePct.toFixed(0)}% {size.source === 'filesystem' ? 'of FS' : 'of ' + formatBytes(size.ceiling)}
										</span>
									</div>
									<div class="h-1 w-full overflow-hidden rounded bg-muted">
										<div
											class="h-full transition-all {sizePct >= 90 ? 'bg-red-500' : sizePct >= 75 ? 'bg-amber-500' : 'bg-emerald-500'}"
											style="width: {sizePct}%"
										></div>
									</div>
								</div>
							{/if}
						</td>
						<td class="p-3">
							<div class="flex flex-wrap gap-1">
								{#if usage.apps > 0}
									<Badge class="bg-indigo-950 text-indigo-400 text-[0.6rem]" title="{usage.apps} Docker app{usage.apps === 1 ? '' : 's'} using this subvolume's storage">Apps{usage.apps > 1 ? ` ×${usage.apps}` : ''}</Badge>
								{/if}
								{#if usage.vms > 0}
									<Badge class="bg-rose-950 text-rose-400 text-[0.6rem]" title="{usage.vms} VM{usage.vms === 1 ? '' : 's'} with a disk on this subvolume">VMs{usage.vms > 1 ? ` ×${usage.vms}` : ''}</Badge>
								{/if}
								{#if usage.nfs > 0}
									<Badge class="bg-green-950 text-green-400 text-[0.6rem]" title="{usage.nfs} NFS share{usage.nfs === 1 ? '' : 's'}">NFS{usage.nfs > 1 ? ` ×${usage.nfs}` : ''}</Badge>
								{/if}
								{#if usage.smb > 0}
									<Badge class="bg-amber-950 text-amber-400 text-[0.6rem]" title="{usage.smb} SMB share{usage.smb === 1 ? '' : 's'}">SMB{usage.smb > 1 ? ` ×${usage.smb}` : ''}</Badge>
								{/if}
								{#if usage.iscsi > 0}
									<Badge class="bg-purple-950 text-purple-400 text-[0.6rem]" title="{usage.iscsi} iSCSI target{usage.iscsi === 1 ? '' : 's'}">iSCSI{usage.iscsi > 1 ? ` ×${usage.iscsi}` : ''}</Badge>
								{/if}
								{#if usage.nvmeof > 0}
									<Badge class="bg-cyan-950 text-cyan-400 text-[0.6rem]" title="{usage.nvmeof} NVMe-oF subsystem{usage.nvmeof === 1 ? '' : 's'}">NVMe-oF{usage.nvmeof > 1 ? ` ×${usage.nvmeof}` : ''}</Badge>
								{/if}
								{#if usage.backups > 0}
									<Badge class="bg-teal-950 text-teal-400 text-[0.6rem]" title="{usage.backups} backup job{usage.backups === 1 ? '' : 's'} reading from this subvolume">Backups{usage.backups > 1 ? ` ×${usage.backups}` : ''}</Badge>
								{/if}
								{#if usage.system && usage.apps === 0 && usage.vms === 0 && usage.nfs === 0 && usage.smb === 0 && usage.iscsi === 0 && usage.nvmeof === 0 && usage.backups === 0}
									<Badge class="bg-slate-800 text-slate-300 text-[0.6rem]" title={usage.system}>System</Badge>
								{/if}
								{#if !usage.system && usage.apps === 0 && usage.vms === 0 && usage.nfs === 0 && usage.smb === 0 && usage.iscsi === 0 && usage.nvmeof === 0 && usage.backups === 0}
									<span class="text-xs text-muted-foreground">—</span>
								{/if}
							</div>
						</td>
						<td class="p-3">
							{#if sv.subvolume_type === 'block'}
								{#if sv.block_device}
									<span class="font-mono text-xs">{sv.block_device}</span>
									<Button variant="destructive" size="xs" class="ml-2" onclick={() => detachSubvolume(sv)}>Detach</Button>
								{:else}
									<span class="text-muted-foreground">Detached</span>
									<Button variant="secondary" size="xs" class="ml-2" onclick={() => attachSubvolume(sv)}>Attach</Button>
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
										<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(sv, snap)}>Delete</Button>
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
									{expandedName === svKey(sv) ? 'Hide' : 'Details'}
								</Button>
								<Button variant="destructive" size="xs" onclick={() => deleteSubvolume(sv)}>Delete</Button>
							</div>
						</td>
					</tr>
					{#if expandedName === svKey(sv) && detailSv}
					<tr>
						<td colspan="7" class="border-b border-border bg-muted/20 p-0">
							<div class="p-4">
								<!-- Tabs -->
								<div class="mb-4 flex border-b border-border">
									{#each [['info', 'Info'], ['snapshots', `Snapshots (${detailSv.snapshots.length})`], ['shares', `Used by${detailShareCount > 0 ? ` (${detailShareCount})` : ''}`], ['browse', 'Browse'], ['properties', 'Properties']] as [key, label]}
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
											<button class="font-mono text-xs text-blue-400 hover:text-blue-300 text-left" onclick={() => { const p = subvolumes.find(s => s.filesystem === detailSv!.filesystem && s.name === detailSv!.parent); if (p) openDetail(p); }}>{detailSv.parent}</button>
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
										<Button size="xs" variant="secondary" onclick={() => { showSnap = detailSv ? { filesystem: detailSv.filesystem, name: detailSv.name } : null; snapName = ''; }}>
											<Camera class="mr-1 h-3 w-3" />Snapshot
										</Button>
										<Button size="xs" variant="secondary" onclick={() => { showClone = detailSv ? { filesystem: detailSv.filesystem, name: detailSv.name } : null; cloneName = ''; }}>
											<Copy class="mr-1 h-3 w-3" />Clone
										</Button>
									</div>

								{:else if detailTab === 'snapshots'}
									{#if detailSv.snapshots.length === 0}
										<p class="text-sm text-muted-foreground">No snapshots.</p>
									{:else}
										<div class="space-y-1.5">
											{#each detailSnapshots.length > 0 ? detailSnapshots : detailSv.snapshots.map(s => ({ name: s, subvolume: detailSv!.name, filesystem: detailSv!.filesystem, path: '', read_only: true, parent: null })) as snap}
												<div class="flex items-center justify-between rounded-md border border-border px-3 py-2">
													<div>
														<span class="font-mono text-xs">{snap.name}</span>
														<span class="ml-2 text-xs text-muted-foreground">{snap.read_only ? 'read-only' : 'writable'}</span>
													</div>
													<div class="flex gap-1">
														<Button variant="secondary" size="xs" onclick={() => { showClone = { filesystem: detailSv!.filesystem, name: detailSv!.name, snapshot: snap.name }; cloneName = ''; }}>
															<Copy class="mr-1 h-3 w-3" />Clone
														</Button>
														<Button variant="destructive" size="xs" onclick={() => deleteSnapshot(detailSv!, snap.name)}>
															<Trash2 class="h-3 w-3" />
														</Button>
													</div>
												</div>
											{/each}
										</div>
									{/if}

								{:else if detailTab === 'shares'}
									{#if detailShareCount === 0}
										<p class="text-sm text-muted-foreground">Nothing is using this subvolume.</p>
									{:else}
										<div class="space-y-1.5">
											{#each detailShares.apps as app}
												<div class="flex items-center gap-2 rounded-md border border-border px-3 py-2">
													<Badge class="bg-indigo-950 text-indigo-400 text-[0.6rem]">App</Badge>
													<span class="text-sm">{app.name}</span>
													<span class="text-xs text-muted-foreground">{app.status}</span>
												</div>
											{/each}
											{#each detailShares.vms as vm}
												<div class="flex items-center gap-2 rounded-md border border-border px-3 py-2">
													<Badge class="bg-rose-950 text-rose-400 text-[0.6rem]">VM</Badge>
													<span class="text-sm">{vm.name}</span>
													<span class="text-xs text-muted-foreground">{vm.running ? 'running' : 'stopped'}</span>
												</div>
											{/each}
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
														<button class="font-mono text-xs text-blue-400 hover:text-blue-300" onclick={() => { const s = subvolumes.find(x => x.filesystem === detailSv!.filesystem && x.name === ancestor); if (s) openDetail(s); }}>{ancestor}</button>
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
															<button class="font-mono text-xs text-blue-400 hover:text-blue-300" onclick={() => { const s = subvolumes.find(x => x.filesystem === detailSv!.filesystem && x.name === child.name); if (s) openDetail(s); }}>{child.name}</button>
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
			{/each}
		</tbody>
	</table>
{/if}


{/if}

{#if pageTab === 'snapshots'}

{#if mountedFilesystems.length > 0}
	<div class="mb-4 flex items-center gap-4">
		{#if mountedFilesystems.length > 1}
			<select value={selectedFs} onchange={(e) => selectFs((e.target as HTMLSelectElement).value)} class="h-9 w-auto rounded-md border border-input bg-transparent px-3 text-sm">
				<option value="">All filesystems</option>
				{#each mountedFilesystems as p}
					<option value={p.name}>{p.name}</option>
				{/each}
			</select>
		{/if}
		<Input bind:value={snapshotSearch} placeholder="Search snapshots..." class="h-9 w-48" />
	</div>
{/if}

{#if snapshotsLoading}
	<p class="text-muted-foreground">Loading snapshots...</p>
{:else if mountedFilesystems.length === 0}
	<p class="text-muted-foreground">No mounted filesystems.</p>
{:else if filteredSnapshots.length === 0}
	<p class="text-muted-foreground">{snapshotSearch ? 'No matching snapshots.' : selectedFs ? `No snapshots in filesystem "${selectedFs}".` : 'No snapshots yet.'}</p>
{:else}
	{@const showFsHeaders = !selectedFs && mountedFilesystems.length > 1}
	{@const snapsByFs = showFsHeaders
		? mountedFilesystems
			.map(fs => ({ fs: fs.name, items: filteredSnapshots.filter(s => s.filesystem === fs.name) }))
			.filter(g => g.items.length > 0)
		: [{ fs: null as string | null, items: filteredSnapshots }]}
	{#each snapsByFs as group (group.fs ?? '__flat__')}
		{#if group.fs}
			<h2 class="mt-6 mb-2 flex items-baseline gap-2 text-sm font-semibold">
				<span>{group.fs}</span>
				<span class="text-xs font-normal text-muted-foreground">{group.items.length} snapshot{group.items.length === 1 ? '' : 's'}</span>
			</h2>
		{/if}
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
				{#each group.items as snap (`${snap.filesystem}|${snap.subvolume}|${snap.name}`)}
					<tr class="border-b border-border">
						<td class="p-3">
							<span class="font-mono text-sm">{snap.name}</span>
							{#if snap.path}
								<span class="block font-mono text-xs text-muted-foreground">{snap.path}</span>
							{/if}
						</td>
						<td class="p-3">
							{#if subvolumes.find(sv => sv.filesystem === snap.filesystem && sv.name === snap.subvolume)}
								<button
									class="font-mono text-sm text-blue-400 hover:text-blue-300 transition-colors"
									onclick={() => switchToSubvolumeAndExpand(snap.filesystem, snap.subvolume)}
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
							<Button variant="destructive" size="xs" onclick={() => deleteSnapshotFromTab(snap.filesystem, snap.subvolume, snap.name)}>
								<Trash2 class="mr-1 h-3 w-3" />Delete
							</Button>
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/each}
{/if}

{/if}

<Dialog.Root open={showSnap !== null} onOpenChange={(open) => { if (!open) showSnap = null; }}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Snapshot "{showSnap?.name ?? ''}"</Dialog.Title>
			<p class="text-sm text-muted-foreground">Create a read-only point-in-time copy.</p>
		</Dialog.Header>
		<div class="mb-4">
			<Label for="snap-name">Snapshot Name {#if !snapName}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
			<Input id="snap-name" bind:value={snapName} placeholder="snap-2026-03-12" class="mt-1 {requiredFieldCls(!snapName)}" />
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
			<Dialog.Title>Clone "{showClone?.name ?? ''}{showClone?.snapshot ? `@${showClone.snapshot}` : ''}"</Dialog.Title>
			<p class="text-sm text-muted-foreground">Create a writable copy (COW — instant, shares data until modified).</p>
		</Dialog.Header>
		<div class="mb-4">
			<Label for="clone-name">Clone Name {#if !cloneName}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
			<Input id="clone-name" bind:value={cloneName} placeholder="my-clone" class="mt-1 {requiredFieldCls(!cloneName)}" />
		</div>
		<Dialog.Footer>
			<Button size="sm" onclick={cloneSubvolume} disabled={!cloneName}>Create</Button>
			<Button variant="secondary" size="sm" onclick={() => showClone = null}>Cancel</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>

