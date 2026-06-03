/** iSCSI target state + handlers.
 *
 * Same shape as the NFS / SMB modules: a single `$state` object so
 * the page and the `<IscsiPanel>` view share one source of truth.
 * iSCSI shares are block-only (`subvolume_type === 'block'`) and
 * carry LUNs + ACLs (CHAP credentials), so the surface is bigger
 * than NFS but smaller than SMB. */

import { getClient } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import { confirm } from '$lib/confirm.svelte';
import type { IscsiTarget, Subvolume, ProtocolStatus } from '$lib/types';

const client = getClient();

export const iscsi = $state({
	targets: [] as IscsiTarget[],
	loading: true,
	protocol: null as ProtocolStatus | null,
	showCreate: false,
	blockSubvolumes: [] as Subvolume[],
	expanded: {} as Record<string, boolean>,
	newName: '',
	newDevice: '',
	addLunTarget: '',
	addLunPath: '',
	addLunType: '',
	addAclTarget: '',
	addAclIqn: '',
	addAclUser: '',
	addAclPass: '',
	addPortalTarget: '',
	addPortalIp: '',
	addPortalPort: 3260,
	addPortalFamily: 'ipv4' as 'ipv4' | 'ipv6',
	search: '',
	sortDir: 'asc' as 'asc' | 'desc',
});

export function iscsiToggleSort() {
	iscsi.sortDir = iscsi.sortDir === 'asc' ? 'desc' : 'asc';
}

export async function iscsiRefresh() {
	await withToast(async () => { iscsi.targets = await client.call<IscsiTarget[]>('share.iscsi.list'); });
}

export async function iscsiLoadProtocol() {
	try {
		const all = await client.call<ProtocolStatus[]>('service.protocol.list');
		iscsi.protocol = all.find(p => p.name === 'iscsi') ?? null;
	} catch { /* ignore */ }
}

export async function iscsiLoadSubvolumes() {
	await withToast(async () => {
		const all = await client.call<Subvolume[]>('subvolume.list_all');
		iscsi.blockSubvolumes = all.filter(s => s.subvolume_type === 'block' && s.block_device);
	});
}

export function iscsiOnDeviceSelect() {
	if (iscsi.newDevice && !iscsi.newName) {
		const sv = iscsi.blockSubvolumes.find(s => s.block_device === iscsi.newDevice);
		if (sv) iscsi.newName = sv.name;
	}
}

export async function iscsiCreate() {
	if (!iscsi.newName || !iscsi.newDevice) return;
	const ok = await withToast(
		() => client.call('share.iscsi.create', { name: iscsi.newName, device_path: iscsi.newDevice }),
		'iSCSI target created'
	);
	if (ok !== undefined) {
		iscsi.showCreate = false;
		iscsi.newName = '';
		iscsi.newDevice = '';
		await iscsiRefresh();
	}
}

export async function iscsiRemove(id: string) {
	if (!await confirm('Delete this iSCSI target?', 'All its LUNs will also be removed.')) return;
	await withToast(() => client.call('share.iscsi.delete', { id }), 'iSCSI target deleted');
	await iscsiRefresh();
}

export async function iscsiAddLun() {
	if (!iscsi.addLunTarget || !iscsi.addLunPath) return;
	const params: Record<string, unknown> = { target_id: iscsi.addLunTarget, backstore_path: iscsi.addLunPath };
	if (iscsi.addLunType) params.backstore_type = iscsi.addLunType;
	await withToast(() => client.call('share.iscsi.add_lun', params), 'LUN added');
	iscsi.addLunTarget = '';
	iscsi.addLunPath = '';
	iscsi.addLunType = '';
	await iscsiRefresh();
}

export async function iscsiRemoveLun(targetId: string, lunId: number) {
	if (!await confirm(`Remove LUN ${lunId}?`)) return;
	await withToast(() => client.call('share.iscsi.remove_lun', { target_id: targetId, lun_id: lunId }), 'LUN removed');
	await iscsiRefresh();
}

export async function iscsiAddAcl() {
	if (!iscsi.addAclTarget || !iscsi.addAclIqn) return;
	const params: Record<string, unknown> = { target_id: iscsi.addAclTarget, initiator_iqn: iscsi.addAclIqn };
	if (iscsi.addAclUser) params.userid = iscsi.addAclUser;
	if (iscsi.addAclPass) params.password = iscsi.addAclPass;
	await withToast(() => client.call('share.iscsi.add_acl', params), 'ACL added');
	iscsi.addAclTarget = '';
	iscsi.addAclIqn = '';
	iscsi.addAclUser = '';
	iscsi.addAclPass = '';
	await iscsiRefresh();
}

export async function iscsiRemoveAcl(targetId: string, initiatorIqn: string) {
	if (!await confirm(`Remove ACL for ${initiatorIqn}?`)) return;
	await withToast(
		() => client.call('share.iscsi.remove_acl', { target_id: targetId, initiator_iqn: initiatorIqn }),
		'ACL removed'
	);
	await iscsiRefresh();
}

export async function iscsiAddPortal() {
	if (!iscsi.addPortalTarget || !iscsi.addPortalIp) return;
	const ok = await withToast(
		() => client.call('share.iscsi.add_portal', {
			target_id: iscsi.addPortalTarget,
			ip: iscsi.addPortalIp.trim(),
			port: iscsi.addPortalPort,
		}),
		'Portal added',
	);
	if (ok !== undefined) {
		iscsi.addPortalTarget = '';
		iscsi.addPortalIp = '';
		iscsi.addPortalPort = 3260;
		iscsi.addPortalFamily = 'ipv4';
		await iscsiRefresh();
	}
}

export async function iscsiRemovePortal(targetId: string, ip: string, port: number) {
	if (!await confirm(`Remove portal ${ip}:${port}?`)) return;
	await withToast(
		() => client.call('share.iscsi.remove_portal', { target_id: targetId, ip, port }),
		'Portal removed',
	);
	await iscsiRefresh();
}
