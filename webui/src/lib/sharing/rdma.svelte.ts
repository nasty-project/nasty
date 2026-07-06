import { getClient } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import type { RdmaStatus } from '$lib/types';

const client = getClient();

export const rdma = $state({
	status: null as RdmaStatus | null,
	loading: false,
});

export async function rdmaLoad() {
	rdma.loading = true;
	try {
		rdma.status = await client.call<RdmaStatus>('system.rdma.status');
	} catch {
		// Older engine without the RPC: hide the card instead of failing
		// the sharing page.
		rdma.status = null;
	}
	rdma.loading = false;
}

export async function rdmaSet(enabled: boolean) {
	const res = await withToast(
		() => client.call<RdmaStatus>('system.rdma.set', { enabled }),
		enabled ? 'RDMA transports enabled' : 'RDMA transports disabled',
	);
	if (res) rdma.status = res;
}
