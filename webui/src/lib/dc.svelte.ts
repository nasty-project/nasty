/** AD Domain Controller role state + handlers (Settings → Directory card).
 * Mirrors domain.svelte.ts's conventions: a shared `$state` object plus a
 * small set of top-level lifecycle handlers. Per-principal CRUD (users,
 * groups, computers) lives in DcPanel.svelte itself, same split as
 * domain.svelte.ts (status/join/leave) vs. the SMB users/groups page. */
import { getClient } from '$lib/client';
import { withToast, info } from '$lib/toast.svelte';
import type { DcStatus, DcPrincipal } from '$lib/types';

const client = getClient();

export const dc = $state({
	status: null as DcStatus | null,
	loading: true,
	provisioning: false,
	// provision form
	realm: '',
	adminPassword: '',
	dnsForwarder: '',
	// panel data (users/groups/computers tabs)
	users: [] as DcPrincipal[],
	groups: [] as DcPrincipal[],
	computers: [] as DcPrincipal[],
});

export async function dcRefresh() {
	try {
		dc.status = await client.call<DcStatus>('dc.status');
	} catch { /* engine without dc support */ }
	dc.loading = false;
}

export async function dcProvision(): Promise<boolean> {
	if (!dc.realm || !dc.adminPassword) return false;
	dc.provisioning = true;
	const params: Record<string, unknown> = {
		realm: dc.realm.trim(),
		admin_password: dc.adminPassword,
	};
	if (dc.dnsForwarder.trim()) params.dns_forwarder = dc.dnsForwarder.trim();
	const res = await withToast(
		() => client.call<{ status: DcStatus; warnings: string[] }>('dc.provision', params),
		'Domain provisioned',
	);
	dc.adminPassword = '';
	dc.provisioning = false;
	if (!res) return false;
	dc.status = res.status;
	dc.realm = '';
	dc.dnsForwarder = '';
	for (const w of res.warnings ?? []) {
		info(w);
	}
	return true;
}

export async function dcDemote(realmConfirmation: string): Promise<boolean> {
	const ok = await withToast(
		() => client.call('dc.demote', { realm_confirmation: realmConfirmation }),
		'Domain demoted',
	);
	if (ok !== undefined) await dcRefresh();
	return ok !== undefined;
}

export async function dcLoadPrincipals() {
	try {
		[dc.users, dc.groups, dc.computers] = await Promise.all([
			client.call<DcPrincipal[]>('dc.user.list'),
			client.call<DcPrincipal[]>('dc.group.list'),
			client.call<DcPrincipal[]>('dc.computer.list'),
		]);
	} catch { /* surfaced by panel */ }
}
