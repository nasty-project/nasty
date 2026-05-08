/** Pending network rollback state.
 *
 * After `system.network.update` returns a `txn_id`, the engine has scheduled
 * an automatic rollback to the prior config at `revert_at_unix`. The user
 * has to call `system.network.confirm` before then to keep the change.
 *
 * This store is the global single source of truth so a banner can be
 * rendered persistently in the root layout — the user might navigate away
 * from the settings page during the confirm window. */

import { getClient } from './client';
import { withToast } from './toast.svelte';
import type { NetworkUpdateRequest, NetworkUpdateResponse } from './types';

export interface PendingRollback {
	txnId: string;
	revertAtUnix: number;
	riskReason: string | null;
}

let _pending = $state<PendingRollback | null>(null);

export const rollbackState = {
	get pending(): PendingRollback | null {
		return _pending;
	},
	set(p: PendingRollback) {
		_pending = p;
	},
	clear() {
		_pending = null;
	},
	/** Compute remaining seconds until the server-side rollback fires. */
	secondsRemaining(): number {
		if (!_pending) return 0;
		const now = Math.floor(Date.now() / 1000);
		return Math.max(0, _pending.revertAtUnix - now);
	},
};

/** Submit a network config change. Captures the response and, if the server
 * scheduled a rollback, populates the global store so the layout banner
 * shows up. Always shows a toast on success/error. */
export async function applyNetworkUpdate(
	payload: NetworkUpdateRequest,
	successMsg: string,
): Promise<NetworkUpdateResponse | undefined> {
	const client = getClient();
	const res = await withToast(
		() => client.call<NetworkUpdateResponse>('system.network.update', payload),
		successMsg,
	);
	if (res?.txn_id && res.revert_at_unix) {
		_pending = {
			txnId: res.txn_id,
			revertAtUnix: res.revert_at_unix,
			riskReason: res.risk_reason ?? null,
		};
	}
	return res;
}

/** Confirm a pending rollback — keeps the change. Clears the local store
 * even if the RPC fails (the server may have already reverted, in which
 * case the banner should disappear regardless). */
export async function confirmRollback(): Promise<void> {
	const txn = _pending;
	if (!txn) return;
	const client = getClient();
	try {
		await client.call('system.network.confirm', { txn_id: txn.txnId });
	} finally {
		// Clear regardless: if the server didn't know the txn, the rollback
		// already fired and the banner is stale.
		if (_pending?.txnId === txn.txnId) {
			_pending = null;
		}
	}
}
