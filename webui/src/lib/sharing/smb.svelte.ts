/** SMB share state + handlers.
 *
 * Same shape as the NFS module — runes wrapped in a plain object so
 * the state survives module imports. Slightly larger surface because
 * SMB shares have per-share `valid_users` (system users) and
 * `valid_users[@group]` (groups), both of which the panel renders
 * pickers for and the create wizard offers an inline-create flow on
 * top of. The wizard-side state (`inlineUsername`/`inlinePassword`/
 * etc.) for that inline-create stays on the page — only the
 * "existing shares" half lives here. */

import { getClient } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import { confirm } from '$lib/confirm.svelte';
import type { SmbShare, SmbGroup, Subvolume, ProtocolStatus } from '$lib/types';

const client = getClient();

export type SmbSortKey = 'name' | 'path' | 'status';

export const smb = $state({
	shares: [] as SmbShare[],
	loading: true,
	protocol: null as ProtocolStatus | null,
	showCreate: false,
	subvolumes: [] as Subvolume[],
	newSubvolume: '',
	newName: '',
	newComment: '',
	newReadOnly: false,
	newGuestOk: false,
	newTimeMachine: false,
	newTmMaxSize: null as number | null,
	expanded: {} as Record<string, boolean>,
	addUserShare: null as string | null,
	addUserName: '',
	groups: [] as SmbGroup[],
	// Shared between the panel's "add user to share" picker and the
	// create wizard's "valid users" picker on the page; lazy-loaded
	// when either UI is opened to avoid a round-trip on every page
	// load.
	systemUsers: [] as { username: string; uid: number }[],
	search: '',
	sortKey: null as SmbSortKey | null,
	sortDir: 'asc' as 'asc' | 'desc',
});

export function smbToggleSort(key: SmbSortKey) {
	if (smb.sortKey === key) {
		smb.sortDir = smb.sortDir === 'asc' ? 'desc' : 'asc';
	} else {
		smb.sortKey = key;
		smb.sortDir = 'asc';
	}
}

export async function smbRefresh() {
	await withToast(async () => {
		const [shares, groups] = await Promise.all([
			client.call<SmbShare[]>('share.smb.list'),
			client.call<SmbGroup[]>('smb.group.list').catch(() => [] as SmbGroup[]),
		]);
		smb.shares = shares;
		smb.groups = groups;
	});
}

export async function smbLoadProtocol() {
	try {
		const all = await client.call<ProtocolStatus[]>('service.protocol.list');
		smb.protocol = all.find(p => p.name === 'smb') ?? null;
	} catch { /* ignore */ }
}

export async function smbLoadSubvolumes() {
	await withToast(async () => {
		const all = await client.call<Subvolume[]>('subvolume.list_all');
		smb.subvolumes = all.filter(s => s.subvolume_type === 'filesystem');
	});
}

export function smbOnSubvolumeSelect() {
	if (smb.newSubvolume && !smb.newName) {
		const sv = smb.subvolumes.find(s => s.path === smb.newSubvolume);
		if (sv) smb.newName = sv.name;
	}
}

export async function smbCreate() {
	if (!smb.newName || !smb.newSubvolume) return;
	const ok = await withToast(
		() => client.call('share.smb.create', {
			name: smb.newName,
			path: smb.newSubvolume,
			comment: smb.newComment || undefined,
			read_only: smb.newReadOnly,
			guest_ok: smb.newGuestOk,
			time_machine: smb.newTimeMachine,
			time_machine_max_size_gib: smb.newTmMaxSize ?? undefined,
		}),
		'SMB share created'
	);
	if (ok !== undefined) {
		smb.showCreate = false;
		smb.newSubvolume = '';
		smb.newName = '';
		smb.newComment = '';
		smb.newTimeMachine = false;
		smb.newTmMaxSize = null;
		await smbRefresh();
	}
}

export async function smbToggleEnabled(share: SmbShare) {
	await withToast(
		() => client.call('share.smb.update', { id: share.id, enabled: !share.enabled }),
		`Share ${share.enabled ? 'disabled' : 'enabled'}`
	);
	await smbRefresh();
}

export async function smbRemove(id: string) {
	if (!await confirm('Delete this SMB share?')) return;
	await withToast(() => client.call('share.smb.delete', { id }), 'SMB share deleted');
	await smbRefresh();
}

export async function smbToggleField(share: SmbShare, field: 'read_only' | 'browseable' | 'guest_ok') {
	await withToast(
		() => client.call('share.smb.update', { id: share.id, [field]: !share[field] }),
		'Share updated'
	);
	await smbRefresh();
}

export async function smbRemoveUser(share: SmbShare, username: string) {
	const valid_users = share.valid_users.filter(u => u !== username);
	await withToast(() => client.call('share.smb.update', { id: share.id, valid_users }), 'User removed');
	await smbRefresh();
}

/** Lazy-load the system user list. Both the panel's add-user flow
 * and the wizard's valid-users picker call this on first open. */
export async function smbEnsureSystemUsers() {
	if (smb.systemUsers.length > 0) return;
	try {
		smb.systemUsers = await client.call<{ username: string; uid: number }[]>('smb.user.list');
	} catch { /* ignore */ }
}
