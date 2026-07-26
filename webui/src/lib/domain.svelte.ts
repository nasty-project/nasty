/** AD membership state + handlers (Settings → Directory card). */
import { getClient, getSessionGeneration, registerSessionReset } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import { confirm } from '$lib/confirm.svelte';
import type { DomainStatus, DomainPrincipal } from '$lib/types';

function initialDomainState() {
	return {
		status: null as DomainStatus | null,
		loading: true,
		joining: false,
		// join form
		realm: '',
		username: '',
		password: '',
		ou: '',
	};
}

export const domain = $state(initialDomainState());

export function resetDomainState() {
	Object.assign(domain, initialDomainState());
}

registerSessionReset(resetDomainState);

export async function domainRefresh() {
	const generation = getSessionGeneration();
	try {
		const status = await getClient().call<DomainStatus>('domain.status');
		if (generation === getSessionGeneration()) domain.status = status;
	} catch { /* engine without domain support */ }
	if (generation === getSessionGeneration()) domain.loading = false;
}

export async function domainJoin() {
	if (!domain.realm || !domain.username || !domain.password) return;
	domain.joining = true;
	const params: Record<string, unknown> = {
		realm: domain.realm.trim(),
		username: domain.username.trim(),
		password: domain.password,
	};
	if (domain.ou.trim()) params.ou = domain.ou.trim();
	const ok = await withToast(() => getClient().call<DomainStatus>('domain.join', params), 'Joined domain');
	domain.password = '';
	if (ok !== undefined) {
		domain.status = ok;
		domain.realm = ''; domain.username = ''; domain.ou = '';
	}
	domain.joining = false;
}

export async function domainLeave(force: boolean, username?: string, password?: string) {
	if (!await confirm(
		'Leave the domain?',
		force
			? 'Local-only leave: the computer account stays behind in AD. Shares referencing domain users keep their entries but domain logons stop working.'
			: 'The computer account will be removed from AD. Shares referencing domain users keep their entries but domain logons stop working.'
	)) return;
	const params: Record<string, unknown> = force ? { force: true } : { username, password };
	await withToast(() => getClient().call('domain.leave', params), 'Left domain');
	await domainRefresh();
}

/** Prefix search for the share permissions picker (2+ chars). */
export async function domainSearchUsers(prefix: string): Promise<DomainPrincipal[]> {
	if (prefix.trim().length < 2 || !domain.status?.joined) return [];
	try {
		return await getClient().call<DomainPrincipal[]>('domain.user.list', { prefix: prefix.trim() });
	} catch {
		return [];
	}
}
