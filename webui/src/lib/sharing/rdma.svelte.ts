import { getClient, registerSessionReset } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import type { RdmaStatus } from '$lib/types';

function initialRdmaState() {
	return { status: null as RdmaStatus | null, loading: false };
}

export const rdma = $state(initialRdmaState());

export function resetRdmaState() {
	Object.assign(rdma, initialRdmaState());
}

registerSessionReset(resetRdmaState);

export async function rdmaLoad() {
	rdma.loading = true;
	try {
		rdma.status = await getClient().call<RdmaStatus>('system.rdma.status');
	} catch {
		// Older engine without the RPC: hide the card instead of failing
		// the sharing page.
		rdma.status = null;
	}
	rdma.loading = false;
}

export async function rdmaSet(enabled: boolean) {
	const res = await withToast(
		() => getClient().call<RdmaStatus>('system.rdma.set', { enabled }),
		enabled ? 'RDMA transports enabled' : 'RDMA transports disabled',
	);
	if (res) rdma.status = res;
}
