<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type {
		NfsShare, SmbShare, IscsiTarget, NvmeofSubsystem,
		Subvolume, ProtocolStatus, SmbGroup
	} from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import { AlertTriangle } from '@lucide/svelte';

	// ── Share creation wizard ────────────────────────────
	let shareWizardStep: 0 | 1 | 2 | 3 | 4 = $state(0);
	let shareProtocol: Tab = $state('smb');
	let shareSubvolume = $state('');
	// NFS access
	let shareNfsHost = $state('');
	let shareNfsOptions = $state('rw,sync,no_subtree_check');
	// SMB access
	let shareSmbName = $state('');
	let shareSmbGuestOk = $state(false);
	let shareSmbReadOnly = $state(false);
	let shareSmbValidUsers: string[] = $state([]);
	let smbSystemUsers: { username: string; uid: number }[] = $state([]);
	let showInlineUserCreate = $state(false);
	let inlineUsername = $state('');
	let inlinePassword = $state('');
	let inlinePasswordConfirm = $state('');
	let inlineGroups: string[] = $state([]);
	// iSCSI access
	let shareIscsiName = $state('');
	// NVMe-oF access
	let shareNvmeofName = $state('');
	let shareNvmeofAddr = $state('0.0.0.0');
	let shareNvmeofPort = $state('4420');

	let shareSubvolumes: Subvolume[] = $state([]);

	// Inline subvolume creation within share wizard
	let showInlineCreate = $state(false);
	let inlineSvName = $state('');
	let inlineSvQuota = $state('');
	let inlineSvCreating = $state(false);
	let inlineSvFilesystems: string[] = $state([]);
	let inlineSvFs = $state('');

	async function loadInlineFilesystems() {
		try {
			const fsList = await client.call<{ name: string; mounted: boolean }[]>('fs.list');
			inlineSvFilesystems = fsList.filter(f => f.mounted).map(f => f.name);
			if (inlineSvFilesystems.length > 0 && !inlineSvFs) inlineSvFs = inlineSvFilesystems[0];
		} catch { inlineSvFilesystems = []; }
	}

	async function inlineCreateSubvolume() {
		if (!inlineSvName || !inlineSvFs) return;
		inlineSvCreating = true;
		const isBlock = shareProtocol === 'iscsi' || shareProtocol === 'nvmeof';
		const params: Record<string, unknown> = {
			filesystem: inlineSvFs,
			name: inlineSvName,
			subvolume_type: isBlock ? 'block' : 'filesystem',
		};
		if (inlineSvQuota) params.volsize_bytes = parseFloat(inlineSvQuota) * 1073741824;
		try {
			await withToast(
				() => client.call('subvolume.create', params),
				'Subvolume created'
			);
			await loadShareSubvolumes();
			// Auto-select the newly created subvolume
			const created = shareSubvolumes.find(sv => sv.name === inlineSvName && sv.filesystem === inlineSvFs);
			if (created) {
				shareSubvolume = isBlock ? (created.block_device ?? '') : created.path;
			}
			showInlineCreate = false;
			inlineSvName = '';
			inlineSvQuota = '';
		} finally {
			inlineSvCreating = false;
		}
	}

	function openShareWizard() {
		shareWizardStep = 1;
		shareProtocol = activeTab;
		shareSubvolume = '';
		shareNfsHost = ''; shareNfsOptions = 'rw,sync,no_subtree_check';
		shareSmbName = ''; shareSmbGuestOk = false; shareSmbReadOnly = false; shareSmbValidUsers = [];
		shareIscsiName = ''; shareNvmeofName = '';
		shareNvmeofAddr = '0.0.0.0'; shareNvmeofPort = '4420';
		showInlineCreate = false;
		inlineSvName = '';
		inlineSvQuota = '';
	}

	async function loadShareSubvolumes() {
		try {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			shareSubvolumes = all;
		} catch { shareSubvolumes = []; }
	}

	$effect(() => { if (shareWizardStep > 0) loadShareSubvolumes(); });

	const filteredShareSubvolumes = $derived.by(() => {
		const isBlock = shareProtocol === 'iscsi' || shareProtocol === 'nvmeof';
		return shareSubvolumes.filter(sv =>
			isBlock ? (sv.subvolume_type === 'block' && sv.block_device) : sv.subvolume_type === 'filesystem'
		);
	});

	async function createShare() {
		if (!shareSubvolume) return;
		const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume);
		if (!sv) return;

		let ok;
		if (shareProtocol === 'nfs') {
			ok = await withToast(
				() => client.call('share.nfs.create', {
					path: sv.path,
					clients: [{ host: shareNfsHost || '*', options: shareNfsOptions }],
				}),
				'NFS share created'
			);
		} else if (shareProtocol === 'smb') {
			ok = await withToast(
				() => client.call('share.smb.create', {
					name: shareSmbName || sv.name,
					path: sv.path,
					guest_ok: shareSmbGuestOk,
					read_only: shareSmbReadOnly,
					valid_users: shareSmbValidUsers,
				}),
				'SMB share created'
			);
		} else if (shareProtocol === 'iscsi') {
			ok = await withToast(
				() => client.call('share.iscsi.create', {
					name: shareIscsiName || sv.name,
					device_path: sv.block_device,
				}),
				'iSCSI target created'
			);
		} else if (shareProtocol === 'nvmeof') {
			ok = await withToast(
				() => client.call('share.nvmeof.create', {
					name: shareNvmeofName || sv.name,
					device_path: sv.block_device,
					addr: shareNvmeofAddr,
					port: parseInt(shareNvmeofPort) || 4420,
				}),
				'NVMe-oF subsystem created'
			);
		}
		if (ok !== undefined) {
			shareWizardStep = 0;
			nfsRefresh(); smbRefresh(); iscsiRefresh(); nvmeRefresh();
		}
	}

	// ── Tab state ────────────────────────────────────────
	type Tab = 'nfs' | 'smb' | 'iscsi' | 'nvmeof';
	const TABS: { key: Tab; label: string; hash: string }[] = [
		{ key: 'smb',    label: 'SMB',     hash: '#smb' },
		{ key: 'nfs',    label: 'NFS',     hash: '#nfs' },
		{ key: 'iscsi',  label: 'iSCSI',   hash: '#iscsi' },
		{ key: 'nvmeof', label: 'NVMe-oF', hash: '#nvmeof' },
	];

	function tabFromHash(): Tab {
		if (typeof window === 'undefined') return 'smb';
		const h = window.location.hash.replace('#', '');
		if (TABS.some(t => t.key === h)) return h as Tab;
		return 'smb';
	}

	let activeTab: Tab = $state(tabFromHash());

	function switchTab(tab: Tab) {
		activeTab = tab;
		// Sync wizard protocol with active tab
		if (shareWizardStep > 0) {
			shareProtocol = tab;
		}
		window.location.hash = tab;
	}

	const client = getClient();

	// ── NFS state ────────────────────────────────────────
	let nfsShares: NfsShare[] = $state([]);
	let nfsLoading = $state(true);
	let nfsProtocol: ProtocolStatus | null = $state(null);
	let nfsShowCreate = $state(false);
	let nfsSubvolumes: Subvolume[] = $state([]);
	let nfsNewSubvolume = $state('');
	let nfsNewComment = $state('');
	let nfsNewHost = $state('');
	let nfsNewOptions = $state('rw,sync,no_subtree_check');
	let nfsExpanded = $state<Record<string, boolean>>({});
	let nfsAddClientShare = $state<string | null>(null);
	let nfsAddClientHost = $state('');
	let nfsAddClientOptions = $state('rw,sync,no_subtree_check');
	let nfsSearch = $state('');
	type NfsSortKey = 'path' | 'status';
	let nfsSortKey = $state<NfsSortKey | null>(null);
	let nfsSortDir = $state<'asc' | 'desc'>('asc');

	$effect(() => { if (nfsShowCreate) nfsLoadSubvolumes(); });

	function nfsToggleSort(key: NfsSortKey) {
		if (nfsSortKey === key) nfsSortDir = nfsSortDir === 'asc' ? 'desc' : 'asc';
		else { nfsSortKey = key; nfsSortDir = 'asc'; }
	}

	const nfsFiltered = $derived(
		nfsSearch.trim()
			? nfsShares.filter(s =>
				s.path.toLowerCase().includes(nfsSearch.toLowerCase()) ||
				s.comment?.toLowerCase().includes(nfsSearch.toLowerCase()) ||
				s.clients.some(c => c.host.includes(nfsSearch)))
			: nfsShares
	);

	const nfsSorted = $derived.by(() => {
		if (!nfsSortKey) return nfsFiltered;
		return [...nfsFiltered].sort((a, b) => {
			let cmp = 0;
			if (nfsSortKey === 'path') cmp = a.path.localeCompare(b.path);
			else if (nfsSortKey === 'status') cmp = Number(b.enabled) - Number(a.enabled);
			return nfsSortDir === 'asc' ? cmp : -cmp;
		});
	});

	async function nfsRefresh() {
		await withToast(async () => { nfsShares = await client.call<NfsShare[]>('share.nfs.list'); });
	}
	async function nfsLoadProtocol() {
		try {
			const all = await client.call<ProtocolStatus[]>('service.protocol.list');
			nfsProtocol = all.find(p => p.name === 'nfs') ?? null;
		} catch { /* ignore */ }
	}
	async function nfsLoadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			nfsSubvolumes = all.filter(s => s.subvolume_type === 'filesystem');
		});
	}
	async function nfsCreate() {
		if (!nfsNewSubvolume || !nfsNewHost) return;
		const ok = await withToast(
			() => client.call('share.nfs.create', {
				path: nfsNewSubvolume,
				comment: nfsNewComment || undefined,
				clients: [{ host: nfsNewHost, options: nfsNewOptions }],
			}),
			'NFS share created'
		);
		if (ok !== undefined) {
			nfsShowCreate = false;
			nfsNewSubvolume = '';
			nfsNewComment = '';
			nfsNewHost = '';
			await nfsRefresh();
		}
	}
	async function nfsToggleEnabled(share: NfsShare) {
		await withToast(
			() => client.call('share.nfs.update', { id: share.id, enabled: !share.enabled }),
			`Share ${share.enabled ? 'disabled' : 'enabled'}`
		);
		await nfsRefresh();
	}
	async function nfsRemove(id: string) {
		if (!await confirm('Delete this NFS share?')) return;
		await withToast(() => client.call('share.nfs.delete', { id }), 'NFS share deleted');
		await nfsRefresh();
	}
	async function nfsRemoveClient(share: NfsShare, host: string) {
		const clients = share.clients.filter(c => c.host !== host);
		await withToast(() => client.call('share.nfs.update', { id: share.id, clients }), 'Client removed');
		await nfsRefresh();
	}
	async function nfsAddClient(share: NfsShare) {
		if (!nfsAddClientHost) return;
		const clients = [...share.clients, { host: nfsAddClientHost, options: nfsAddClientOptions }];
		const ok = await withToast(
			() => client.call('share.nfs.update', { id: share.id, clients }),
			'Client added'
		);
		if (ok !== undefined) {
			nfsAddClientShare = null;
			nfsAddClientHost = '';
			nfsAddClientOptions = 'rw,sync,no_subtree_check';
		}
		await nfsRefresh();
	}

	// ── SMB state ────────────────────────────────────────
	let smbShares: SmbShare[] = $state([]);
	let smbLoading = $state(true);
	let smbProtocol: ProtocolStatus | null = $state(null);
	let smbShowCreate = $state(false);
	let smbSubvolumes: Subvolume[] = $state([]);
	let smbNewSubvolume = $state('');
	let smbNewName = $state('');
	let smbNewComment = $state('');
	let smbNewReadOnly = $state(false);
	let smbNewGuestOk = $state(false);
	let smbExpanded = $state<Record<string, boolean>>({});
	let smbAddUserShare = $state<string | null>(null);
	let smbAddUserName = $state('');
	let smbGroups: SmbGroup[] = $state([]);
	let smbSearch = $state('');
	type SmbSortKey = 'name' | 'path' | 'status';
	let smbSortKey = $state<SmbSortKey | null>(null);
	let smbSortDir = $state<'asc' | 'desc'>('asc');

	$effect(() => { if (smbShowCreate) smbLoadSubvolumes(); });

	function smbToggleSort(key: SmbSortKey) {
		if (smbSortKey === key) smbSortDir = smbSortDir === 'asc' ? 'desc' : 'asc';
		else { smbSortKey = key; smbSortDir = 'asc'; }
	}

	const smbFiltered = $derived(
		smbSearch.trim()
			? smbShares.filter(s =>
				s.name.toLowerCase().includes(smbSearch.toLowerCase()) ||
				s.path.toLowerCase().includes(smbSearch.toLowerCase()) ||
				s.comment?.toLowerCase().includes(smbSearch.toLowerCase()))
			: smbShares
	);

	const smbSorted = $derived.by(() => {
		if (!smbSortKey) return smbFiltered;
		return [...smbFiltered].sort((a, b) => {
			let cmp = 0;
			if (smbSortKey === 'name') cmp = a.name.localeCompare(b.name);
			else if (smbSortKey === 'path') cmp = a.path.localeCompare(b.path);
			else if (smbSortKey === 'status') cmp = Number(b.enabled) - Number(a.enabled);
			return smbSortDir === 'asc' ? cmp : -cmp;
		});
	});

	async function smbRefresh() {
		await withToast(async () => {
			[smbShares, smbGroups] = await Promise.all([
				client.call<SmbShare[]>('share.smb.list'),
				client.call<SmbGroup[]>('smb.group.list').catch(() => [] as SmbGroup[]),
			]);
		});
	}
	async function smbLoadProtocol() {
		try {
			const all = await client.call<ProtocolStatus[]>('service.protocol.list');
			smbProtocol = all.find(p => p.name === 'smb') ?? null;
		} catch { /* ignore */ }
	}
	async function smbLoadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			smbSubvolumes = all.filter(s => s.subvolume_type === 'filesystem');
		});
	}
	function smbOnSubvolumeSelect() {
		if (smbNewSubvolume && !smbNewName) {
			const sv = smbSubvolumes.find(s => s.path === smbNewSubvolume);
			if (sv) smbNewName = sv.name;
		}
	}
	async function smbCreate() {
		if (!smbNewName || !smbNewSubvolume) return;
		const ok = await withToast(
			() => client.call('share.smb.create', {
				name: smbNewName,
				path: smbNewSubvolume,
				comment: smbNewComment || undefined,
				read_only: smbNewReadOnly,
				guest_ok: smbNewGuestOk,
			}),
			'SMB share created'
		);
		if (ok !== undefined) {
			smbShowCreate = false;
			smbNewSubvolume = '';
			smbNewName = '';
			smbNewComment = '';
			await smbRefresh();
		}
	}
	async function smbToggleEnabled(share: SmbShare) {
		await withToast(
			() => client.call('share.smb.update', { id: share.id, enabled: !share.enabled }),
			`Share ${share.enabled ? 'disabled' : 'enabled'}`
		);
		await smbRefresh();
	}
	async function smbRemove(id: string) {
		if (!await confirm('Delete this SMB share?')) return;
		await withToast(() => client.call('share.smb.delete', { id }), 'SMB share deleted');
		await smbRefresh();
	}
	async function smbToggleField(share: SmbShare, field: 'read_only' | 'browseable' | 'guest_ok') {
		await withToast(
			() => client.call('share.smb.update', { id: share.id, [field]: !share[field] }),
			'Share updated'
		);
		await smbRefresh();
	}
	async function smbRemoveUser(share: SmbShare, username: string) {
		const valid_users = share.valid_users.filter(u => u !== username);
		await withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), 'User removed');
		await smbRefresh();
	}
	async function smbAddUser(share: SmbShare) {
		if (!smbAddUserName) return;
		const valid_users = [...share.valid_users, smbAddUserName];
		const ok = await withToast(
			() => client.call('share.smb.update', { id: share.id, valid_users }),
			'User added'
		);
		if (ok !== undefined) {
			smbAddUserShare = null;
			smbAddUserName = '';
		}
		await smbRefresh();
	}

	// ── iSCSI state ──────────────────────────────────────
	let iscsiTargets: IscsiTarget[] = $state([]);
	let iscsiLoading = $state(true);
	let iscsiProtocol: ProtocolStatus | null = $state(null);
	let iscsiShowCreate = $state(false);
	let iscsiBlockSubvolumes: Subvolume[] = $state([]);
	let iscsiExpanded: Record<string, boolean> = $state({});
	let iscsiNewName = $state('');
	let iscsiNewDevice = $state('');
	let iscsiAddLunTarget = $state('');
	let iscsiAddLunPath = $state('');
	let iscsiAddLunType = $state('');
	let iscsiAddAclTarget = $state('');
	let iscsiAddAclIqn = $state('');
	let iscsiAddAclUser = $state('');
	let iscsiAddAclPass = $state('');
	let iscsiSearch = $state('');
	let iscsiSortDir = $state<'asc' | 'desc'>('asc');

	$effect(() => { if (iscsiShowCreate || iscsiAddLunTarget) iscsiLoadSubvolumes(); });

	function iscsiToggleSort() { iscsiSortDir = iscsiSortDir === 'asc' ? 'desc' : 'asc'; }

	const iscsiFiltered = $derived(
		iscsiSearch.trim()
			? iscsiTargets.filter(t =>
				t.iqn.toLowerCase().includes(iscsiSearch.toLowerCase()) ||
				t.alias?.toLowerCase().includes(iscsiSearch.toLowerCase()))
			: iscsiTargets
	);

	const iscsiSorted = $derived.by(() => {
		return [...iscsiFiltered].sort((a, b) => {
			const cmp = a.iqn.localeCompare(b.iqn);
			return iscsiSortDir === 'asc' ? cmp : -cmp;
		});
	});

	async function iscsiRefresh() {
		await withToast(async () => { iscsiTargets = await client.call<IscsiTarget[]>('share.iscsi.list'); });
	}
	async function iscsiLoadProtocol() {
		try {
			const all = await client.call<ProtocolStatus[]>('service.protocol.list');
			iscsiProtocol = all.find(p => p.name === 'iscsi') ?? null;
		} catch { /* ignore */ }
	}
	async function iscsiLoadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			iscsiBlockSubvolumes = all.filter(s => s.subvolume_type === 'block' && s.block_device);
		});
	}
	function iscsiOnDeviceSelect() {
		if (iscsiNewDevice && !iscsiNewName) {
			const sv = iscsiBlockSubvolumes.find(s => s.block_device === iscsiNewDevice);
			if (sv) iscsiNewName = sv.name;
		}
	}
	async function iscsiCreate() {
		if (!iscsiNewName || !iscsiNewDevice) return;
		const ok = await withToast(
			() => client.call('share.iscsi.create', { name: iscsiNewName, device_path: iscsiNewDevice }),
			'iSCSI target created'
		);
		if (ok !== undefined) {
			iscsiShowCreate = false;
			iscsiNewName = '';
			iscsiNewDevice = '';
			await iscsiRefresh();
		}
	}
	async function iscsiRemove(id: string) {
		if (!await confirm('Delete this iSCSI target?', 'All its LUNs will also be removed.')) return;
		await withToast(() => client.call('share.iscsi.delete', { id }), 'iSCSI target deleted');
		await iscsiRefresh();
	}
	async function iscsiAddLun() {
		if (!iscsiAddLunTarget || !iscsiAddLunPath) return;
		const params: Record<string, unknown> = { target_id: iscsiAddLunTarget, backstore_path: iscsiAddLunPath };
		if (iscsiAddLunType) params.backstore_type = iscsiAddLunType;
		await withToast(() => client.call('share.iscsi.add_lun', params), 'LUN added');
		iscsiAddLunTarget = '';
		iscsiAddLunPath = '';
		iscsiAddLunType = '';
		await iscsiRefresh();
	}
	async function iscsiRemoveLun(targetId: string, lunId: number) {
		if (!await confirm(`Remove LUN ${lunId}?`)) return;
		await withToast(() => client.call('share.iscsi.remove_lun', { target_id: targetId, lun_id: lunId }), 'LUN removed');
		await iscsiRefresh();
	}
	async function iscsiAddAcl() {
		if (!iscsiAddAclTarget || !iscsiAddAclIqn) return;
		const params: Record<string, unknown> = { target_id: iscsiAddAclTarget, initiator_iqn: iscsiAddAclIqn };
		if (iscsiAddAclUser) params.userid = iscsiAddAclUser;
		if (iscsiAddAclPass) params.password = iscsiAddAclPass;
		await withToast(() => client.call('share.iscsi.add_acl', params), 'ACL added');
		iscsiAddAclTarget = '';
		iscsiAddAclIqn = '';
		iscsiAddAclUser = '';
		iscsiAddAclPass = '';
		await iscsiRefresh();
	}
	async function iscsiRemoveAcl(targetId: string, initiatorIqn: string) {
		if (!await confirm(`Remove ACL for ${initiatorIqn}?`)) return;
		await withToast(
			() => client.call('share.iscsi.remove_acl', { target_id: targetId, initiator_iqn: initiatorIqn }),
			'ACL removed'
		);
		await iscsiRefresh();
	}

	// ── NVMe-oF state ───────────────────────────────────
	let nvmeSubsystems: NvmeofSubsystem[] = $state([]);
	let nvmeLoading = $state(true);
	let nvmeProtocol: ProtocolStatus | null = $state(null);
	let nvmeShowCreate = $state(false);
	let nvmeBlockSubvolumes: Subvolume[] = $state([]);
	let nvmeExpanded: Record<string, boolean> = $state({});
	let nvmeNewName = $state('');
	let nvmeNewDevice = $state('');
	let nvmeNewAddr = $state('0.0.0.0');
	let nvmeNewPort = $state(4420);
	let nvmeAddNsSubsys = $state('');
	let nvmeAddNsDevice = $state('');
	let nvmeAddPortSubsys = $state('');
	let nvmeAddPortTransport = $state('tcp');
	let nvmeAddPortAddr = $state('0.0.0.0');
	let nvmeAddPortSvcId = $state(4420);
	let nvmeAddPortFamily = $state('ipv4');
	let nvmeAddHostSubsys = $state('');
	let nvmeAddHostNqn = $state('');
	let nvmeSearch = $state('');
	let nvmeSortDir = $state<'asc' | 'desc'>('asc');

	$effect(() => { if (nvmeShowCreate || nvmeAddNsSubsys) nvmeLoadSubvolumes(); });

	function nvmeToggleSort() { nvmeSortDir = nvmeSortDir === 'asc' ? 'desc' : 'asc'; }

	const nvmeFiltered = $derived(
		nvmeSearch.trim()
			? nvmeSubsystems.filter(s => s.nqn.toLowerCase().includes(nvmeSearch.toLowerCase()))
			: nvmeSubsystems
	);

	const nvmeSorted = $derived.by(() => {
		return [...nvmeFiltered].sort((a, b) => {
			const cmp = a.nqn.localeCompare(b.nqn);
			return nvmeSortDir === 'asc' ? cmp : -cmp;
		});
	});

	async function nvmeRefresh() {
		await withToast(async () => { nvmeSubsystems = await client.call<NvmeofSubsystem[]>('share.nvmeof.list'); });
	}
	async function nvmeLoadProtocol() {
		try {
			const all = await client.call<ProtocolStatus[]>('service.protocol.list');
			nvmeProtocol = all.find(p => p.name === 'nvmeof') ?? null;
		} catch { /* ignore */ }
	}
	async function nvmeLoadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			nvmeBlockSubvolumes = all.filter(s => s.subvolume_type === 'block' && s.block_device);
		});
	}
	function nvmeOnDeviceSelect() {
		if (nvmeNewDevice && !nvmeNewName) {
			const sv = nvmeBlockSubvolumes.find(s => s.block_device === nvmeNewDevice);
			if (sv) nvmeNewName = sv.name;
		}
	}
	async function nvmeCreate() {
		if (!nvmeNewName || !nvmeNewDevice) return;
		const ok = await withToast(
			() => client.call('share.nvmeof.create', {
				name: nvmeNewName,
				device_path: nvmeNewDevice,
				addr: nvmeNewAddr,
				port: nvmeNewPort,
			}),
			'NVMe-oF share created'
		);
		if (ok !== undefined) {
			nvmeShowCreate = false;
			nvmeNewName = '';
			nvmeNewDevice = '';
			nvmeNewAddr = '0.0.0.0';
			nvmeNewPort = 4420;
			await nvmeRefresh();
		}
	}
	async function nvmeRemove(id: string) {
		if (!await confirm('Delete this NVMe-oF share?')) return;
		await withToast(() => client.call('share.nvmeof.delete', { id }), 'NVMe-oF share deleted');
		await nvmeRefresh();
	}
	async function nvmeAddNamespace() {
		if (!nvmeAddNsSubsys || !nvmeAddNsDevice) return;
		await withToast(
			() => client.call('share.nvmeof.add_namespace', { subsystem_id: nvmeAddNsSubsys, device_path: nvmeAddNsDevice }),
			'Namespace added'
		);
		nvmeAddNsSubsys = '';
		nvmeAddNsDevice = '';
		await nvmeRefresh();
	}
	async function nvmeRemoveNamespace(subsystemId: string, nsid: number) {
		if (!await confirm(`Remove namespace ${nsid}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_namespace', { subsystem_id: subsystemId, nsid }),
			'Namespace removed'
		);
		await nvmeRefresh();
	}
	async function nvmeAddPort() {
		if (!nvmeAddPortSubsys) return;
		await withToast(
			() => client.call('share.nvmeof.add_port', {
				subsystem_id: nvmeAddPortSubsys,
				transport: nvmeAddPortTransport,
				addr: nvmeAddPortAddr,
				service_id: nvmeAddPortSvcId,
				addr_family: nvmeAddPortFamily,
			}),
			'Port added'
		);
		nvmeAddPortSubsys = '';
		nvmeAddPortTransport = 'tcp';
		nvmeAddPortAddr = '0.0.0.0';
		nvmeAddPortSvcId = 4420;
		nvmeAddPortFamily = 'ipv4';
		await nvmeRefresh();
	}
	async function nvmeRemovePort(subsystemId: string, portId: number) {
		if (!await confirm(`Remove port ${portId}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_port', { subsystem_id: subsystemId, port_id: portId }),
			'Port removed'
		);
		await nvmeRefresh();
	}
	async function nvmeAddHost() {
		if (!nvmeAddHostSubsys || !nvmeAddHostNqn) return;
		await withToast(
			() => client.call('share.nvmeof.add_host', { subsystem_id: nvmeAddHostSubsys, host_nqn: nvmeAddHostNqn }),
			'Allowed host added'
		);
		nvmeAddHostSubsys = '';
		nvmeAddHostNqn = '';
		await nvmeRefresh();
	}
	async function nvmeRemoveHost(subsystemId: string, hostNqn: string) {
		if (!await confirm(`Remove access for ${hostNqn}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_host', { subsystem_id: subsystemId, host_nqn: hostNqn }),
			'Allowed host removed'
		);
		await nvmeRefresh();
	}

	async function toggleProtocol(name: string, currentlyEnabled: boolean) {
		const action = currentlyEnabled ? 'disable' : 'enable';
		await withToast(
			() => client.call(`service.protocol.${action}`, { name }),
			`${name} ${action}d`
		);
	}

	// ── Events & lifecycle ───────────────────────────────
	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'share.nfs') nfsRefresh();
		if (p?.collection === 'share.smb') smbRefresh();
		if (p?.collection === 'share.iscsi') iscsiRefresh();
		if (p?.collection === 'share.nvmeof') nvmeRefresh();
		if (p?.collection === 'protocol') {
			nfsLoadProtocol();
			smbLoadProtocol();
			iscsiLoadProtocol();
			nvmeLoadProtocol();
		}
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await Promise.all([
			nfsRefresh().then(() => { nfsLoading = false; }),
			smbRefresh().then(() => { smbLoading = false; }),
			iscsiRefresh().then(() => { iscsiLoading = false; }),
			nvmeRefresh().then(() => { nvmeLoading = false; }),
			nfsLoadProtocol(),
			smbLoadProtocol(),
			iscsiLoadProtocol(),
			nvmeLoadProtocol(),
		]);
	});

	onDestroy(() => client.offEvent(handleEvent));
</script>

{#if shareWizardStep !== 0}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<div class="mb-6 flex items-center gap-0">
				{#each [['1', 'Protocol'], ['2', 'Source'], ['3', 'Access'], ['4', 'Review']] as [num, label], i}
					<div class="flex items-center">
						<div class="flex items-center gap-2">
							<div class="flex h-6 w-6 items-center justify-center rounded-full text-xs font-semibold
								{shareWizardStep > i + 1 ? 'bg-primary text-primary-foreground' :
								 shareWizardStep === i + 1 ? 'bg-primary text-primary-foreground' :
								 'bg-secondary text-muted-foreground'}">
								{num}
							</div>
							<span class="text-xs {shareWizardStep === i + 1 ? 'text-foreground font-medium' : 'text-muted-foreground'}">{label}</span>
						</div>
						{#if i < 3}
							<div class="mx-3 h-px w-8 bg-border"></div>
						{/if}
					</div>
				{/each}
			</div>

			<!-- Step 1: Protocol -->
			{#if shareWizardStep === 1}
			{@const selectedProto = ({ nfs: nfsProtocol, smb: smbProtocol, iscsi: iscsiProtocol, nvmeof: nvmeProtocol })[shareProtocol]}
			<div class="mb-4">
				<Label>Protocol</Label>
				<select bind:value={shareProtocol} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="smb">SMB — Windows/Samba File Sharing</option>
					<option value="nfs">NFS — Network File System</option>
					<option value="iscsi">iSCSI — Block Storage over TCP</option>
					<option value="nvmeof">NVMe-oF — NVMe over Fabrics (TCP)</option>
				</select>
			</div>
			{#if selectedProto && !selectedProto.enabled}
				<div class="mb-4 flex items-center gap-3 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2.5">
					<AlertTriangle size={16} class="shrink-0 text-amber-500" />
					<span class="flex-1 text-sm">{selectedProto.display_name} service is not enabled.</span>
					<Button size="xs" onclick={async () => { await toggleProtocol(shareProtocol === 'nvmeof' ? 'nvmeof' : shareProtocol, false); await ({ nfs: nfsLoadProtocol, smb: smbLoadProtocol, iscsi: iscsiLoadProtocol, nvmeof: nvmeLoadProtocol })[shareProtocol](); }}>
						Enable
					</Button>
				</div>
			{/if}
			<div class="flex gap-2">
				<Button size="sm" onclick={() => shareWizardStep = 2} disabled={selectedProto != null && !selectedProto.enabled}>Next: Source →</Button>
			</div>

			<!-- Step 2: Source -->
			{:else if shareWizardStep === 2}
			<div class="mb-4">
				<Label>Subvolume</Label>
				<select bind:value={shareSubvolume} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a subvolume...</option>
					{#each filteredShareSubvolumes as sv}
						{#if shareProtocol === 'iscsi' || shareProtocol === 'nvmeof'}
							<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
						{:else}
							<option value={sv.path}>{sv.filesystem}/{sv.name} ({sv.path})</option>
						{/if}
					{/each}
				</select>
				{#if filteredShareSubvolumes.length === 0 && !showInlineCreate}
					<p class="mt-1 text-xs text-muted-foreground">
						No {shareProtocol === 'iscsi' || shareProtocol === 'nvmeof' ? 'block' : 'filesystem'} subvolumes available.
					</p>
					<Button size="sm" variant="secondary" class="mt-2" onclick={() => { showInlineCreate = true; loadInlineFilesystems(); }}>Create Subvolume</Button>
				{:else if !showInlineCreate}
					<Button size="xs" variant="secondary" class="mt-2" onclick={() => { showInlineCreate = true; loadInlineFilesystems(); }}>Create Subvolume</Button>
				{/if}
				{#if showInlineCreate}
					<div class="mt-3 rounded-lg border border-border bg-secondary/20 p-3 space-y-3">
						<p class="text-xs font-medium">New {shareProtocol === 'iscsi' || shareProtocol === 'nvmeof' ? 'block' : 'filesystem'} subvolume</p>
						{#if inlineSvFilesystems.length > 1}
							<div>
								<Label class="text-xs">Filesystem</Label>
								<select bind:value={inlineSvFs} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
									{#each inlineSvFilesystems as fs}
										<option value={fs}>{fs}</option>
									{/each}
								</select>
							</div>
						{/if}
						<div>
							<Label class="text-xs">Name</Label>
							<Input bind:value={inlineSvName} placeholder="e.g. media" class="mt-1 h-8 text-sm" />
						</div>
						<div>
							<Label class="text-xs">Quota (GiB) <span class="text-muted-foreground font-normal">{shareProtocol === 'iscsi' || shareProtocol === 'nvmeof' ? '— required for block' : '— optional'}</span></Label>
							<Input bind:value={inlineSvQuota} type="number" placeholder="e.g. 100" class="mt-1 h-8 text-sm" />
						</div>
						<div class="flex gap-2">
							<Button size="sm" onclick={inlineCreateSubvolume}
								disabled={!inlineSvName || !inlineSvFs || inlineSvCreating || ((shareProtocol === 'iscsi' || shareProtocol === 'nvmeof') && !inlineSvQuota)}>
								{inlineSvCreating ? 'Creating...' : 'Create'}
							</Button>
							<Button variant="secondary" size="sm" onclick={() => showInlineCreate = false}>Cancel</Button>
						</div>
					</div>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => shareWizardStep = 1}>← Back</Button>
				<Button size="sm" onclick={() => {
					// Auto-fill names from subvolume
					const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume);
					if (sv) {
						if (shareProtocol === 'smb' && !shareSmbName) shareSmbName = sv.name;
						if (shareProtocol === 'iscsi' && !shareIscsiName) shareIscsiName = sv.name;
						if (shareProtocol === 'nvmeof' && !shareNvmeofName) shareNvmeofName = sv.name;
					}
					shareWizardStep = 3;
					if (shareProtocol === 'smb' && smbSystemUsers.length === 0) {
						client.call<{ username: string; uid: number }[]>('smb.user.list').then(u => { smbSystemUsers = u; }).catch(() => {});
					}
				}} disabled={!shareSubvolume}>Next: Access →</Button>
			</div>

			<!-- Step 3: Access (protocol-specific) -->
			{:else if shareWizardStep === 3}
			{#if shareProtocol === 'nfs'}
				<div class="mb-4">
					<Label>Allowed Network</Label>
					<Input bind:value={shareNfsHost} placeholder="192.168.1.0/24 or * for any" class="mt-1" />
				</div>
				<div class="mb-4">
					<Label>Export Options</Label>
					<Input bind:value={shareNfsOptions} class="mt-1" />
				</div>
			{:else if shareProtocol === 'smb'}
				<div class="mb-4">
					<Label>Share Name</Label>
					<Input bind:value={shareSmbName} placeholder="documents" class="mt-1" />
				</div>
				<div class="mb-4 flex gap-4">
					<label class="flex items-center gap-2 text-sm cursor-pointer">
						<input type="checkbox" bind:checked={shareSmbGuestOk} class="rounded border-input" />
						Allow guests
					</label>
					<label class="flex items-center gap-2 text-sm cursor-pointer">
						<input type="checkbox" bind:checked={shareSmbReadOnly} class="rounded border-input" />
						Read-only
					</label>
				</div>
				{#if !shareSmbGuestOk}
					<div class="mb-4">
						<Label>Allowed Users & Groups</Label>
						<p class="mt-1 mb-2 text-xs text-muted-foreground">Select who can access this share. Leave empty to allow all authenticated users.</p>
						{#if shareSmbValidUsers.length > 0}
							<div class="mb-2 flex flex-wrap gap-2">
								{#each shareSmbValidUsers as entry}
									<span class="flex items-center gap-1 rounded-md border px-2 py-1 text-xs {entry.startsWith('@') ? 'border-blue-500/40 bg-blue-500/10' : 'border-border'}">
										{entry}
										<button class="ml-1 text-muted-foreground hover:text-destructive" onclick={() => { shareSmbValidUsers = shareSmbValidUsers.filter(u => u !== entry); }}>&times;</button>
									</span>
								{/each}
							</div>
						{/if}
						<div class="flex flex-wrap gap-2">
							{#each smbSystemUsers.filter(u => !shareSmbValidUsers.includes(u.username)) as user}
								<Button size="xs" variant="secondary" onclick={() => { shareSmbValidUsers = [...shareSmbValidUsers, user.username]; }}>
									{user.username}
								</Button>
							{/each}
							{#each smbGroups.filter(g => !shareSmbValidUsers.includes(`@${g.name}`)) as group}
								<Button size="xs" variant="secondary" class="text-blue-400" onclick={() => { shareSmbValidUsers = [...shareSmbValidUsers, `@${group.name}`]; }}>
									@{group.name}
								</Button>
							{/each}
						</div>
						{#if showInlineUserCreate}
							<Card class="mt-3 max-w-md">
								<CardContent class="pt-4">
									<h3 class="mb-4 text-lg font-semibold">New System User</h3>
									<div class="mb-4">
										<Label for="inline-username">Username</Label>
										<Input id="inline-username" bind:value={inlineUsername} placeholder="johndoe" autocomplete="off" class="mt-1" />
									</div>
									<div class="mb-4">
										<Label for="inline-password">Password</Label>
										<Input id="inline-password" type="password" bind:value={inlinePassword} autocomplete="new-password" class="mt-1" />
									</div>
									<div class="mb-4">
										<Label for="inline-password-confirm">Confirm Password</Label>
										<Input id="inline-password-confirm" type="password" bind:value={inlinePasswordConfirm} autocomplete="new-password" class="mt-1" />
										{#if inlinePasswordConfirm && inlinePassword !== inlinePasswordConfirm}
											<span class="mt-1 block text-xs text-destructive">Passwords do not match</span>
										{/if}
									</div>
									{#if smbGroups.length > 0}
										<div class="mb-4">
											<Label>Add to Groups</Label>
											<div class="mt-1 flex flex-wrap gap-2">
												{#each smbGroups as group}
													<label class="flex items-center gap-1.5 text-sm cursor-pointer rounded border border-border px-2 py-1 hover:bg-muted/30">
														<input type="checkbox" class="rounded border-input"
															onchange={(e) => {
																const checked = (e.target as HTMLInputElement).checked;
																if (checked) inlineGroups = [...inlineGroups, group.name];
																else inlineGroups = inlineGroups.filter(g => g !== group.name);
															}}
															checked={inlineGroups.includes(group.name)}
														/>
														{group.name}
													</label>
												{/each}
											</div>
										</div>
									{/if}
									<div class="flex gap-2">
										<Button onclick={async () => {
											const ok = await withToast(
												() => client.call('smb.user.create', { username: inlineUsername, password: inlinePassword }),
												`User "${inlineUsername}" created`
											);
											if (ok !== undefined) {
												for (const g of inlineGroups) {
													await client.call('smb.group.add_member', { group: g, user: inlineUsername }).catch(() => {});
												}
												shareSmbValidUsers = [...shareSmbValidUsers, inlineUsername];
												smbSystemUsers = [...smbSystemUsers, { username: inlineUsername, uid: 0 }];
												showInlineUserCreate = false;
												inlineUsername = ''; inlinePassword = ''; inlinePasswordConfirm = ''; inlineGroups = [];
											}
										}} disabled={!inlineUsername || !inlinePassword || inlinePassword !== inlinePasswordConfirm}>
											Create & Add
										</Button>
										<Button variant="secondary" onclick={() => { showInlineUserCreate = false; }}>Cancel</Button>
									</div>
								</CardContent>
							</Card>
						{:else}
							<div class="mt-2 flex gap-2">
								<Button size="sm" onclick={() => showInlineUserCreate = true}>Create System User</Button>
							</div>
						{/if}
					</div>
				{/if}
			{:else if shareProtocol === 'iscsi'}
				<div class="mb-4">
					<Label>Target Name</Label>
					<Input bind:value={shareIscsiName} placeholder="dbserver" class="mt-1" />
					<p class="mt-1 text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{shareIscsiName || '...'}</p>
				</div>
			{:else if shareProtocol === 'nvmeof'}
				<div class="mb-4">
					<Label>Subsystem Name</Label>
					<Input bind:value={shareNvmeofName} placeholder="storage-vol" class="mt-1" />
				</div>
				<div class="grid grid-cols-2 gap-4 mb-4">
					<div>
						<Label>Listen Address</Label>
						<Input bind:value={shareNvmeofAddr} placeholder="0.0.0.0" class="mt-1" />
					</div>
					<div>
						<Label>Port</Label>
						<Input bind:value={shareNvmeofPort} placeholder="4420" class="mt-1" />
					</div>
				</div>
			{/if}
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => shareWizardStep = 2}>← Back</Button>
				<Button size="sm" onclick={() => shareWizardStep = 4}>Next: Review →</Button>
			</div>

			<!-- Step 4: Review -->
			{:else if shareWizardStep === 4}
			{@const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume)}
			<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
				<span class="text-muted-foreground">Protocol</span>
				<span class="uppercase">{shareProtocol}</span>
				<span class="text-muted-foreground">Source</span>
				<span class="font-mono text-xs">{sv ? `${sv.filesystem}/${sv.name}` : shareSubvolume}</span>
				{#if shareProtocol === 'nfs'}
					<span class="text-muted-foreground">Allowed</span>
					<span>{shareNfsHost || '*'}</span>
					<span class="text-muted-foreground">Options</span>
					<span class="text-xs">{shareNfsOptions}</span>
				{:else if shareProtocol === 'smb'}
					<span class="text-muted-foreground">Share Name</span>
					<span>{shareSmbName || sv?.name}</span>
					{#if shareSmbGuestOk}<span class="text-muted-foreground">Guests</span><span>Allowed</span>{/if}
					{#if shareSmbReadOnly}<span class="text-muted-foreground">Access</span><span>Read-only</span>{/if}
				{:else if shareProtocol === 'iscsi'}
					<span class="text-muted-foreground">Target</span>
					<span class="font-mono text-xs">iqn.2137-01.com.nasty:{shareIscsiName || sv?.name}</span>
				{:else if shareProtocol === 'nvmeof'}
					<span class="text-muted-foreground">Subsystem</span>
					<span>{shareNvmeofName || sv?.name}</span>
					<span class="text-muted-foreground">Listen</span>
					<span>{shareNvmeofAddr}:{shareNvmeofPort}</span>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => shareWizardStep = 3}>← Back</Button>
				<Button size="sm" onclick={createShare}>Create Share</Button>
			</div>
			{/if}
		</CardContent>
	</Card>
{/if}

<!-- Tab bar with inline status -->
<div class="mb-6 flex items-center border-b border-border">
	{#each TABS as tab}
		{@const proto = ({ nfs: nfsProtocol, smb: smbProtocol, iscsi: iscsiProtocol, nvmeof: nvmeProtocol })[tab.key]}
		{@const count = ({ nfs: nfsShares.length, smb: smbShares.length, iscsi: iscsiTargets.length, nvmeof: nvmeSubsystems.length })[tab.key]}
		<button
			onclick={() => switchTab(tab.key)}
			class="flex items-center gap-2 px-4 py-2 text-sm font-medium transition-colors {activeTab === tab.key
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>
			{tab.label}
			{#if proto}
				<span class="inline-block h-1.5 w-1.5 rounded-full {proto.running ? 'bg-green-500' : 'bg-muted-foreground/40'}"></span>
			{/if}
			{#if count > 0}
				<span class="text-[0.65rem] text-muted-foreground">{count}</span>
			{/if}
		</button>
	{/each}
</div>

<!-- Create Share button + wizard -->
<div class="mb-4">
	<Button size="sm" onclick={() => shareWizardStep === 0 ? openShareWizard() : (shareWizardStep = 0)}>
		{shareWizardStep !== 0 ? 'Cancel' : 'Create Share'}
	</Button>
</div>

<!-- ════════════════════════════════════════════════════ NFS ════════════════════════════════════════════════════ -->
{#if activeTab === 'nfs'}

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={nfsSearch} placeholder="Search..." class="h-9 w-48" />
</div>


{#if nfsLoading}
	<p class="text-muted-foreground">Loading...</p>
{:else if nfsShares.length === 0}
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Path" active={nfsSortKey === 'path'} dir={nfsSortDir} onclick={() => nfsToggleSort('path')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Clients</th>
				<SortTh label="Status" active={nfsSortKey === 'status'} dir={nfsSortDir} onclick={() => nfsToggleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each nfsSorted as share}
				<tr
					class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors"
					onclick={() => nfsExpanded[share.id] = !nfsExpanded[share.id]}
				>
					<td class="p-3">
						<span class="font-mono text-sm">{share.path}</span>
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{share.clients.length} client{share.clients.length !== 1 ? 's' : ''}
					</td>
					<td class="p-3">
						<Badge variant={share.enabled ? 'default' : 'secondary'}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => nfsExpanded[share.id] = !nfsExpanded[share.id]}>
								{nfsExpanded[share.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="secondary" size="xs" onclick={() => nfsToggleEnabled(share)}>
								{share.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => nfsRemove(share.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if nfsExpanded[share.id]}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="4" class="px-6 py-4">
							<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Allowed Clients</p>
							{#if share.clients.length === 0}
								<p class="mb-3 text-xs text-muted-foreground">No clients configured.</p>
							{:else}
								<div class="mb-3 space-y-1.5">
									{#each share.clients as c}
										<div class="flex items-center gap-3">
											<code class="text-xs">{c.host}</code>
											<span class="text-xs text-muted-foreground">({c.options})</span>
											{#if c.options.includes('no_root_squash')}
												<span class="text-xs text-yellow-500" title="no_root_squash disables quota enforcement for root clients">⚠ quota</span>
											{/if}
											<Button variant="destructive" size="xs" onclick={() => nfsRemoveClient(share, c.host)}>Remove</Button>
										</div>
									{/each}
								</div>
							{/if}
							{#if nfsAddClientShare === share.id}
								<div class="flex items-end gap-2">
									<div>
										<Label class="text-xs">Host / Network</Label>
										<Input bind:value={nfsAddClientHost} placeholder="192.168.1.0/24" class="mt-1 h-8 w-44 text-xs" />
									</div>
									<div>
										<Label class="text-xs">Options</Label>
										<Input bind:value={nfsAddClientOptions} class="mt-1 h-8 w-56 text-xs" />
									</div>
									<Button size="xs" onclick={() => nfsAddClient(share)} disabled={!nfsAddClientHost}>Add</Button>
									<Button variant="secondary" size="xs" onclick={() => { nfsAddClientShare = null; nfsAddClientHost = ''; }}>Cancel</Button>
								</div>
								{#if nfsAddClientOptions.includes('no_root_squash')}
									<p class="mt-1 text-xs text-yellow-500">Warning: <code>no_root_squash</code> disables quota enforcement for root NFS clients.</p>
								{/if}
							{:else}
								<Button variant="secondary" size="xs" onclick={() => { nfsAddClientShare = share.id; nfsAddClientHost = ''; nfsAddClientOptions = 'rw,sync,no_subtree_check'; }}>
									Add Client
								</Button>
							{/if}
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}


<!-- ════════════════════════════════════════════════════ SMB ════════════════════════════════════════════════════ -->
{:else if activeTab === 'smb'}


<div class="mb-4 flex items-center gap-3">
	<Input bind:value={smbSearch} placeholder="Search..." class="h-9 w-48" />
</div>

{#if smbShowCreate}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Share</h3>
			<div class="mb-4">
				<Label for="smb-subvol">Subvolume</Label>
				<select id="smb-subvol" bind:value={smbNewSubvolume} onchange={smbOnSubvolumeSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a subvolume...</option>
					{#each smbSubvolumes as sv}
						<option value={sv.path}>{sv.filesystem}/{sv.name} ({sv.path})</option>
					{/each}
				</select>
				{#if smbSubvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No filesystem subvolumes found.</span>
					<Button size="xs" class="mt-1" onclick={() => goto('/subvolumes')}>Subvolumes</Button>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="smb-name">Share Name</Label>
				<Input id="smb-name" bind:value={smbNewName} placeholder="documents" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">Name visible to network clients</span>
			</div>
			<div class="mb-4">
				<Label for="smb-comment">Comment</Label>
				<Input id="smb-comment" bind:value={smbNewComment} placeholder="Optional description" class="mt-1" />
			</div>
			<div class="mb-4 flex gap-6">
				<label class="flex cursor-pointer items-center gap-2">
					<input type="checkbox" bind:checked={smbNewReadOnly} class="h-4 w-4" /> Read-only
				</label>
				<label class="flex cursor-pointer items-center gap-2">
					<input type="checkbox" bind:checked={smbNewGuestOk} class="h-4 w-4" /> Allow guests
				</label>
			</div>
			<Button onclick={smbCreate} disabled={!smbNewName || !smbNewSubvolume}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if smbLoading}
	<p class="text-muted-foreground">Loading...</p>
{:else if smbShares.length === 0}
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={smbSortKey === 'name'} dir={smbSortDir} onclick={() => smbToggleSort('name')} />
				<SortTh label="Path" active={smbSortKey === 'path'} dir={smbSortDir} onclick={() => smbToggleSort('path')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Access</th>
				<SortTh label="Status" active={smbSortKey === 'status'} dir={smbSortDir} onclick={() => smbToggleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each smbSorted as share}
				<tr
					class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors"
					onclick={() => smbExpanded[share.id] = !smbExpanded[share.id]}
				>
					<td class="p-3">
						<strong>{share.name}</strong>
						{#if share.comment}<br /><span class="text-xs text-muted-foreground">{share.comment}</span>{/if}
					</td>
					<td class="p-3 font-mono text-sm">{share.path}</td>
					<td class="p-3">
						<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">{share.read_only ? 'RO' : 'RW'}</span>
						{#if share.guest_ok}<span class="mr-1 inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">Guest</span>{/if}
						{#if share.valid_users.length > 0}
							{@const userCount = share.valid_users.filter(u => !u.startsWith('@')).length}
							{@const groupCount = share.valid_users.filter(u => u.startsWith('@')).length}
							<span class="inline-block rounded bg-secondary px-1.5 py-0.5 text-xs">
								{#if userCount > 0}{userCount} user{userCount !== 1 ? 's' : ''}{/if}{#if userCount > 0 && groupCount > 0}, {/if}{#if groupCount > 0}{groupCount} group{groupCount !== 1 ? 's' : ''}{/if}
							</span>
						{/if}
					</td>
					<td class="p-3">
						<Badge variant={share.enabled ? 'default' : 'secondary'}>
							{share.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => smbExpanded[share.id] = !smbExpanded[share.id]}>
								{smbExpanded[share.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="secondary" size="xs" onclick={() => smbToggleEnabled(share)}>
								{share.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => smbRemove(share.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if smbExpanded[share.id]}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="5" class="px-6 py-4">
							<div class="flex gap-12">
								<div>
									<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Settings</p>
									<div class="space-y-2">
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.read_only} onchange={() => smbToggleField(share, 'read_only')} class="h-4 w-4" />
											Read-only
										</label>
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.browseable} onchange={() => smbToggleField(share, 'browseable')} class="h-4 w-4" />
											Browseable
										</label>
										<label class="flex cursor-pointer items-center gap-2 text-sm">
											<input type="checkbox" checked={share.guest_ok} onchange={() => smbToggleField(share, 'guest_ok')} class="h-4 w-4" />
											Allow guests
										</label>
									</div>
								</div>
								<div class="flex-1">
									<p class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Valid Users & Groups</p>
									{#if share.valid_users.length === 0}
										<p class="mb-3 text-xs text-muted-foreground">No restrictions — all authenticated users may access.</p>
									{:else}
										<div class="mb-3 flex flex-wrap gap-2">
											{#each share.valid_users as entry}
												<span class="flex items-center gap-1 rounded-md border px-2 py-1 text-xs {entry.startsWith('@') ? 'border-blue-500/40 bg-blue-500/10' : 'border-border'}">
													{#if entry.startsWith('@')}
														<span class="text-blue-400">{entry}</span>
													{:else}
														{entry}
													{/if}
													<button class="ml-1 text-muted-foreground hover:text-destructive" onclick={(e) => { e.stopPropagation(); smbRemoveUser(share, entry); }}>&times;</button>
												</span>
											{/each}
										</div>
									{/if}
									{#if smbAddUserShare === share.id}
										<div class="flex flex-wrap gap-1.5" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
											{#each smbSystemUsers.filter(u => !share.valid_users.includes(u.username)) as user}
												<Button size="xs" variant="secondary" onclick={() => {
													const valid_users = [...share.valid_users, user.username];
													withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), `${user.username} added`).then(() => smbRefresh());
												}}>{user.username}</Button>
											{/each}
											{#each smbGroups.filter(g => !share.valid_users.includes(`@${g.name}`)) as group}
												<Button size="xs" variant="secondary" class="text-blue-400" onclick={() => {
													const valid_users = [...share.valid_users, `@${group.name}`];
													withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), `@${group.name} added`).then(() => smbRefresh());
												}}>@{group.name}</Button>
											{/each}
											<Button size="xs" variant="secondary" onclick={() => goto('/users')}>Create User / Group</Button>
											<Button variant="secondary" size="xs" onclick={() => { smbAddUserShare = null; }}>Done</Button>
										</div>
									{:else}
										<Button variant="secondary" size="xs" onclick={(e) => {
											e.stopPropagation();
											smbAddUserShare = share.id;
											if (smbSystemUsers.length === 0) {
												client.call<{ username: string; uid: number }[]>('smb.user.list').then(u => { smbSystemUsers = u; }).catch(() => {});
											}
										}}>
											Add User / Group
										</Button>
									{/if}
								</div>
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}


<!-- ════════════════════════════════════════════════════ iSCSI ════════════════════════════════════════════════════ -->
{:else if activeTab === 'iscsi'}

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={iscsiSearch} placeholder="Search..." class="h-9 w-48" />
</div>

{#if iscsiShowCreate}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Target</h3>
			<div class="mb-4">
				<Label for="iscsi-device">Block Subvolume</Label>
				<select id="iscsi-device" bind:value={iscsiNewDevice} onchange={iscsiOnDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a block subvolume...</option>
					{#each iscsiBlockSubvolumes as sv}
						<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
					{/each}
				</select>
				{#if iscsiBlockSubvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No attached block subvolumes found. Create a block subvolume and attach it first.</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="iscsi-name">Target Name</Label>
				<Input id="iscsi-name" bind:value={iscsiNewName} placeholder="dbserver" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{iscsiNewName || '...'}</span>
			</div>
			<Button onclick={iscsiCreate} disabled={!iscsiNewName || !iscsiNewDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if iscsiLoading}
	<p class="text-muted-foreground">Loading...</p>
{:else if iscsiTargets.length === 0}
	<p class="text-muted-foreground">No targets configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="IQN" active={true} dir={iscsiSortDir} onclick={iscsiToggleSort} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Summary</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each iscsiSorted as target}
				<tr class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors" onclick={() => iscsiExpanded[target.id] = !iscsiExpanded[target.id]}>
					<td class="p-3">
						<span class="font-mono text-sm font-semibold">{target.iqn}</span>
						{#if target.alias}<span class="ml-2 text-xs text-muted-foreground">({target.alias})</span>{/if}
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{target.luns.length} LUN{target.luns.length !== 1 ? 's' : ''}
						&middot; {target.portals.length} portal{target.portals.length !== 1 ? 's' : ''}
						&middot; {target.acls.length === 0 ? 'open (any initiator)' : `${target.acls.length} ACL${target.acls.length !== 1 ? 's' : ''}`}
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => iscsiExpanded[target.id] = !iscsiExpanded[target.id]}>
								{iscsiExpanded[target.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => iscsiRemove(target.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if iscsiExpanded[target.id]}
					<tr class="border-b border-border bg-secondary/20">
						<td colspan="3" class="px-4 py-4">
							<div class="space-y-4">
								<!-- Portals -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Portals</h4>
									{#if target.portals.length === 0}
										<p class="text-xs text-muted-foreground">None</p>
									{:else}
										<div class="flex flex-wrap gap-2">
											{#each target.portals as p}
												<span class="rounded bg-secondary px-2 py-0.5 font-mono text-xs">{p.ip}:{p.port}</span>
											{/each}
										</div>
									{/if}
								</div>

								<!-- LUNs -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">LUNs</h4>
									{#if target.luns.length === 0}
										<p class="text-xs text-muted-foreground">No LUNs</p>
									{:else}
										<div class="space-y-1">
											{#each target.luns as lun}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs font-semibold">LUN {lun.lun_id}</span>
														<span class="ml-2 text-muted-foreground">{lun.backstore_path}</span>
														<span class="ml-1 text-xs text-muted-foreground">({lun.backstore_type})</span>
													</div>
													<Button variant="destructive" size="xs" onclick={() => iscsiRemoveLun(target.id, lun.lun_id)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if iscsiAddLunTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Block Device or Subvolume</Label>
												<select bind:value={iscsiAddLunPath} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Select...</option>
													{#each iscsiBlockSubvolumes as sv}
														<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
											</div>
											<div class="mb-2">
												<Label class="text-xs">Type</Label>
												<select bind:value={iscsiAddLunType} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Auto-detect</option>
													<option value="block">Block</option>
													<option value="fileio">File I/O</option>
												</select>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={iscsiAddLun} disabled={!iscsiAddLunPath}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { iscsiAddLunTarget = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { iscsiAddLunTarget = target.id; }}>+ Add LUN</Button>
									{/if}
								</div>

								<!-- ACLs -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Access Control (ACLs)</h4>
									{#if target.acls.length === 0}
										<p class="text-xs text-muted-foreground">Open access — any initiator can connect. Add an ACL to restrict.</p>
									{:else}
										<div class="space-y-1">
											{#each target.acls as acl}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs">{acl.initiator_iqn}</span>
														{#if acl.userid}<span class="ml-2 text-xs text-muted-foreground">CHAP: {acl.userid}</span>{/if}
													</div>
													<Button variant="destructive" size="xs" onclick={() => iscsiRemoveAcl(target.id, acl.initiator_iqn)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if iscsiAddAclTarget === target.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Initiator IQN</Label>
												<Input bind:value={iscsiAddAclIqn} placeholder="iqn.2024-01.com.client:initiator1" class="mt-1 h-8 text-xs" />
											</div>
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">CHAP User (optional)</Label>
													<Input bind:value={iscsiAddAclUser} class="mt-1 h-8 text-xs" />
												</div>
												<div>
													<Label class="text-xs">CHAP Password (optional)</Label>
													<Input bind:value={iscsiAddAclPass} type="password" class="mt-1 h-8 text-xs" />
												</div>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={iscsiAddAcl} disabled={!iscsiAddAclIqn}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { iscsiAddAclTarget = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { iscsiAddAclTarget = target.id; }}>+ Add ACL</Button>
									{/if}
								</div>
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}


<!-- ════════════════════════════════════════════════════ NVMe-oF ════════════════════════════════════════════════════ -->
{:else if activeTab === 'nvmeof'}

<div class="mb-4 flex items-center gap-3">
	<Input bind:value={nvmeSearch} placeholder="Search..." class="h-9 w-48" />
</div>

{#if nvmeShowCreate}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Share</h3>
			<div class="mb-4">
				<Label for="nvme-device">Block Subvolume</Label>
				<select id="nvme-device" bind:value={nvmeNewDevice} onchange={nvmeOnDeviceSelect} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a block subvolume...</option>
					{#each nvmeBlockSubvolumes as sv}
						<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
					{/each}
				</select>
				{#if nvmeBlockSubvolumes.length === 0}
					<span class="mt-1 block text-xs text-muted-foreground">No attached block subvolumes found. Create a block subvolume and attach it first.</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="nvme-name">Share Name</Label>
				<Input id="nvme-name" bind:value={nvmeNewName} placeholder="faststore" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">NQN: nqn.2137.com.nasty:{nvmeNewName || '...'}</span>
			</div>
			<div class="grid grid-cols-2 gap-4 mb-4">
				<div>
					<Label for="nvme-addr">Listen Address</Label>
					<Input id="nvme-addr" bind:value={nvmeNewAddr} class="mt-1" />
				</div>
				<div>
					<Label for="nvme-port">Port</Label>
					<Input id="nvme-port" type="number" bind:value={nvmeNewPort} class="mt-1" />
				</div>
			</div>
			<Button onclick={nvmeCreate} disabled={!nvmeNewName || !nvmeNewDevice}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if nvmeLoading}
	<p class="text-muted-foreground">Loading...</p>
{:else if nvmeSubsystems.length === 0}
	<p class="text-muted-foreground">No shares configured.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="NQN" active={true} dir={nvmeSortDir} onclick={nvmeToggleSort} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Summary</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each nvmeSorted as subsys}
				<tr class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors" onclick={() => nvmeExpanded[subsys.id] = !nvmeExpanded[subsys.id]}>
					<td class="p-3">
						<span class="font-mono text-sm font-semibold">{subsys.nqn}</span>
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{subsys.namespaces.length} namespace{subsys.namespaces.length !== 1 ? 's' : ''}
						&middot; {subsys.ports.length} port{subsys.ports.length !== 1 ? 's' : ''}
						&middot; {subsys.allow_any_host ? 'any host' : `${subsys.allowed_hosts.length} allowed host${subsys.allowed_hosts.length !== 1 ? 's' : ''}`}
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => nvmeExpanded[subsys.id] = !nvmeExpanded[subsys.id]}>
								{nvmeExpanded[subsys.id] ? 'Hide' : 'Details'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => nvmeRemove(subsys.id)}>Delete</Button>
						</div>
					</td>
				</tr>
				{#if nvmeExpanded[subsys.id]}
					<tr class="border-b border-border bg-secondary/20">
						<td colspan="3" class="px-4 py-4">
							<div class="space-y-4">
								<!-- Namespaces -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Namespaces</h4>
									{#if subsys.namespaces.length === 0}
										<p class="text-xs text-muted-foreground">No namespaces</p>
									{:else}
										<div class="space-y-1">
											{#each subsys.namespaces as ns}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<div class="text-sm">
														<span class="font-mono text-xs font-semibold">NSID {ns.nsid}</span>
														<span class="ml-2 text-muted-foreground">{ns.device_path}</span>
														<Badge variant={ns.enabled ? 'default' : 'secondary'} class="ml-2 text-[0.6rem]">{ns.enabled ? 'Active' : 'Off'}</Badge>
													</div>
													<Button variant="destructive" size="xs" onclick={() => nvmeRemoveNamespace(subsys.id, ns.nsid)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if nvmeAddNsSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Block Device</Label>
												<select bind:value={nvmeAddNsDevice} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
													<option value="">Select...</option>
													{#each nvmeBlockSubvolumes as sv}
														<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddNamespace} disabled={!nvmeAddNsDevice}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvmeAddNsSubsys = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { nvmeAddNsSubsys = subsys.id; }}>+ Add Namespace</Button>
									{/if}
								</div>

								<!-- Ports -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Ports</h4>
									{#if subsys.ports.length === 0}
										<p class="text-xs text-muted-foreground">Not listening (no ports configured)</p>
									{:else}
										<div class="flex flex-wrap gap-2">
											{#each subsys.ports as port}
												<div class="flex items-center gap-2 rounded bg-secondary/50 px-2 py-1">
													<span class="font-mono text-xs">{port.transport.toUpperCase()} {port.addr}:{port.service_id}</span>
													<Button variant="destructive" size="xs" class="h-5 text-xs" onclick={() => nvmeRemovePort(subsys.id, port.port_id)}>×</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if nvmeAddPortSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">Transport</Label>
													<select bind:value={nvmeAddPortTransport} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
														<option value="tcp">TCP</option>
														<option value="rdma">RDMA</option>
													</select>
												</div>
												<div>
													<Label class="text-xs">Address Family</Label>
													<select bind:value={nvmeAddPortFamily} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
														<option value="ipv4">IPv4</option>
														<option value="ipv6">IPv6</option>
													</select>
												</div>
											</div>
											<div class="grid grid-cols-2 gap-2 mb-2">
												<div>
													<Label class="text-xs">Listen Address</Label>
													<Input bind:value={nvmeAddPortAddr} class="mt-1 h-8 text-xs" />
												</div>
												<div>
													<Label class="text-xs">Port</Label>
													<Input type="number" bind:value={nvmeAddPortSvcId} class="mt-1 h-8 text-xs" />
												</div>
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddPort}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvmeAddPortSubsys = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { nvmeAddPortSubsys = subsys.id; }}>+ Add Port</Button>
									{/if}
								</div>

								<!-- Allowed Hosts -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Allowed Hosts</h4>
									{#if subsys.allow_any_host && subsys.allowed_hosts.length === 0}
										<p class="text-xs text-muted-foreground">Any host can connect. Add a host NQN to restrict access.</p>
									{:else}
										<div class="space-y-1">
											{#each subsys.allowed_hosts as hostNqn}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs">{hostNqn}</span>
													<Button variant="destructive" size="xs" onclick={() => nvmeRemoveHost(subsys.id, hostNqn)}>Remove</Button>
												</div>
											{/each}
										</div>
									{/if}
									{#if nvmeAddHostSubsys === subsys.id}
										<div class="mt-3 rounded border p-3">
											<div class="mb-2">
												<Label class="text-xs">Host NQN</Label>
												<Input bind:value={nvmeAddHostNqn} placeholder="nqn.2024-01.com.client:host1" class="mt-1 h-8 text-xs" />
											</div>
											<div class="flex gap-2">
												<Button size="xs" onclick={nvmeAddHost} disabled={!nvmeAddHostNqn}>Add</Button>
												<Button size="xs" variant="ghost" onclick={() => { nvmeAddHostSubsys = ''; }}>Cancel</Button>
											</div>
										</div>
									{:else}
										<Button size="xs" variant="outline" class="mt-2" onclick={() => { nvmeAddHostSubsys = subsys.id; }}>+ Add Host</Button>
									{/if}
								</div>
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}

{/if}
