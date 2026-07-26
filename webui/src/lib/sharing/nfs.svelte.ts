/** NFS share state + handlers.
 *
 * Lives in a module so the sharing page and `<NfsPanel>` view both
 * read from a single source of truth (vs. inline state on the page
 * with the `<NfsPanel>` reaching back into props). The page hits
 * `nfsRefresh()` on event broadcasts and from the cross-protocol
 * create wizard; the panel reads `nfs.shares` directly and dispatches
 * its own per-share actions through the exported handlers.
 *
 * Pattern: `$state` on a plain object so nested fields are reactive
 * across module boundaries — bare `let nfsLoading = $state(true)`
 * inside a module wouldn't survive import. */

import { getClient, registerSessionReset } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import { confirm } from '$lib/confirm.svelte';
import type { NfsShare, Subvolume, ProtocolStatus } from '$lib/types';

export type NfsSortKey = 'path' | 'status';

function initialNfsState() {
	return {
		shares: [] as NfsShare[],
		loading: true,
		protocol: null as ProtocolStatus | null,
		showCreate: false,
		subvolumes: [] as Subvolume[],
		newSubvolume: '',
		newComment: '',
		newHost: '',
		newOptions: 'rw,sync,no_subtree_check',
		expanded: {} as Record<string, boolean>,
		addClientShare: null as string | null,
		addClientHost: '',
		addClientOptions: 'rw,sync,no_subtree_check',
		search: '',
		sortKey: null as NfsSortKey | null,
		sortDir: 'asc' as 'asc' | 'desc',
	};
}

export const nfs = $state(initialNfsState());

export function resetNfsState() {
	Object.assign(nfs, initialNfsState());
}

registerSessionReset(resetNfsState);

export function nfsToggleSort(key: NfsSortKey) {
	if (nfs.sortKey === key) {
		nfs.sortDir = nfs.sortDir === 'asc' ? 'desc' : 'asc';
	} else {
		nfs.sortKey = key;
		nfs.sortDir = 'asc';
	}
}

export async function nfsRefresh() {
	await withToast(async () => { nfs.shares = await getClient().call<NfsShare[]>('share.nfs.list'); });
}

export async function nfsLoadProtocol() {
	try {
		const all = await getClient().call<ProtocolStatus[]>('service.protocol.list');
		nfs.protocol = all.find(p => p.name === 'nfs') ?? null;
	} catch { /* ignore */ }
}

export async function nfsLoadSubvolumes() {
	await withToast(async () => {
		const all = await getClient().call<Subvolume[]>('subvolume.list_all');
		nfs.subvolumes = all.filter(s => s.subvolume_type === 'filesystem');
	});
}

export async function nfsCreate() {
	if (!nfs.newSubvolume || !nfs.newHost) return;
	const ok = await withToast(
		() => getClient().call('share.nfs.create', {
			path: nfs.newSubvolume,
			comment: nfs.newComment || undefined,
			clients: [{ host: nfs.newHost, options: nfs.newOptions }],
		}),
		'NFS share created'
	);
	if (ok !== undefined) {
		nfs.showCreate = false;
		nfs.newSubvolume = '';
		nfs.newComment = '';
		nfs.newHost = '';
		await nfsRefresh();
	}
}

export async function nfsToggleEnabled(share: NfsShare) {
	await withToast(
		() => getClient().call('share.nfs.update', { id: share.id, enabled: !share.enabled }),
		`Share ${share.enabled ? 'disabled' : 'enabled'}`
	);
	await nfsRefresh();
}

export async function nfsRemove(id: string) {
	if (!await confirm('Delete this NFS share?')) return;
	await withToast(() => getClient().call('share.nfs.delete', { id }), 'NFS share deleted');
	await nfsRefresh();
}

export async function nfsRemoveClient(share: NfsShare, host: string) {
	const clients = share.clients.filter(c => c.host !== host);
	await withToast(() => getClient().call('share.nfs.update', { id: share.id, clients }), 'Client removed');
	await nfsRefresh();
}

export async function nfsAddClient(share: NfsShare) {
	if (!nfs.addClientHost) return;
	const clients = [...share.clients, { host: nfs.addClientHost, options: nfs.addClientOptions }];
	const ok = await withToast(
		() => getClient().call('share.nfs.update', { id: share.id, clients }),
		'Client added'
	);
	if (ok !== undefined) {
		nfs.addClientShare = null;
		nfs.addClientHost = '';
		nfs.addClientOptions = 'rw,sync,no_subtree_check';
	}
	await nfsRefresh();
}
