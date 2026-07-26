/** Pending network rollback state.
 *
 * After `system.network.update` returns a `txn_id`, the engine has scheduled
 * an automatic rollback to the prior config at `revert_at_unix`. The user
 * has to call `system.network.confirm` before then to keep the change.
 *
 * This store is the global single source of truth so a banner can be
 * rendered persistently in the root layout — the user might navigate away
 * from the settings page during the confirm window. */

import { getClient, getSessionGeneration, registerSessionReset } from './client';
import { error as toastError, withToast } from './toast.svelte';
import type { NetworkPendingTxn, NetworkUpdateRequest, NetworkUpdateResponse } from './types';

export interface PendingRollback {
	txnId: string;
	revertAtUnix: number;
	riskReason: string | null;
}

let _pending = $state<PendingRollback | null>(null);
let _confirmationError = $state<{ txnId: string; message: string } | null>(null);
let _confirmingTxnId = $state<string | null>(null);
let _applying = $state(false);
let expiryRefresh: Promise<void> | null = null;
let recoveryGeneration = 0;

function setPending(pending: PendingRollback | null) {
	if (pending?.txnId !== _pending?.txnId) {
		_confirmationError = null;
		_confirmingTxnId = null;
	}
	_pending = pending;
}

function errorMessage(error: unknown): string {
	return error instanceof Error
		? error.message
		: typeof error === 'object' && error !== null && 'message' in error
			? String((error as { message: unknown }).message)
			: String(error);
}

export const rollbackState = {
	get pending(): PendingRollback | null {
		return _pending;
	},
	set(p: PendingRollback) {
		setPending(p);
	},
	clear() {
		setPending(null);
		_applying = false;
	},
	get confirmationError(): string | null {
		return _confirmationError && _confirmationError.txnId === _pending?.txnId
			? _confirmationError.message
			: null;
	},
	get confirming(): boolean {
		return _confirmingTxnId !== null && _confirmingTxnId === _pending?.txnId;
	},
	get applying(): boolean {
		return _applying;
	},
	/** Compute remaining seconds until the server-side rollback fires. */
	secondsRemaining(): number {
		if (!_pending) return 0;
		const now = Math.floor(Date.now() / 1000);
		return Math.max(0, _pending.revertAtUnix - now);
	},
};

registerSessionReset(() => {
	recoveryGeneration++;
	rollbackState.clear();
});

/** Submit a network config change. Captures the response and, if the server
 * scheduled a rollback, populates the global store so the layout banner
 * shows up. Always shows a toast on success/error. */
export async function applyNetworkUpdate(
	payload: NetworkUpdateRequest,
	successMsg: string,
): Promise<NetworkUpdateResponse | undefined> {
	if (_applying || _pending) {
		toastError('Confirm or wait for the pending network change before applying another update');
		return undefined;
	}
	_applying = true;
	const generation = getSessionGeneration();
	const client = getClient();
	try {
		const res = await withToast(
			() => client.call<NetworkUpdateResponse>('system.network.update', payload),
			successMsg,
		);
		if (generation !== getSessionGeneration()) return undefined;
		if (!res) {
			void recoverPendingRollback();
			return undefined;
		}
		if (res?.txn_id && res.revert_at_unix) {
			setPending({
				txnId: res.txn_id,
				revertAtUnix: res.revert_at_unix,
				riskReason: res.risk_reason ?? null,
			});
		}
		// Surface per-connection NM errors as an explicit error toast.
		if (res?.apply_errors && res.apply_errors.length > 0) {
			const lines = res.apply_errors
				.map((e) => `${e.connection_id}: ${e.message}`)
				.join('\n');
			toastError(
				`Network applied, but ${res.apply_errors.length} connection(s) reported errors:\n${lines}`,
			);
		}
		return res;
	} finally {
		if (generation === getSessionGeneration()) _applying = false;
	}
}

/** Query the server for any active rollback transactions and populate the
 * local store. Called on every (re)connect so a fresh session can recover
 * the banner — important when the user just changed the management iface
 * IP and reconnected on the new address: the txn is still pending on the
 * server, but the *original* browser session that initiated it is gone.
 *
 * Best-effort: if the RPC fails (older engine, transient error), we leave
 * the local state alone. Picks the soonest-expiring txn if multiple are
 * pending — pathological in practice (the in-memory table rarely has more
 * than one entry) but we want stable behavior. */
export async function loadPendingRollback(): Promise<void> {
	const generation = getSessionGeneration();
	const client = getClient();
	let txns: NetworkPendingTxn[];
	try {
		txns = await client.call<NetworkPendingTxn[]>('system.network.pending');
	} catch {
		return;
	}
	if (generation !== getSessionGeneration()) return;
	if (txns.length === 0) {
		// Server says nothing pending — clear local in case we'd been
		// holding a stale entry from a prior session.
		setPending(null);
		return;
	}
	const soonest = txns.reduce((a, b) => (a.revert_at_unix <= b.revert_at_unix ? a : b));
	setPending({
		txnId: soonest.txn_id,
		revertAtUnix: soonest.revert_at_unix,
		riskReason: soonest.risk_reason || null,
	});
}

/** Re-query instead of trusting the browser clock when a countdown expires. */
export function reconcileExpiredRollback(): void {
	if (!_pending || rollbackState.secondsRemaining() > 0 || expiryRefresh) return;
	expiryRefresh = loadPendingRollback().finally(() => {
		expiryRefresh = null;
	});
}

/** Retry after reconnect because a risky update may register after connectivity returns. */
export async function recoverPendingRollback(): Promise<void> {
	const generation = getSessionGeneration();
	const recovery = ++recoveryGeneration;
	for (const delay of [0, 1000, 3000, 5000, 7000, 7000, 7000]) {
		if (delay) await new Promise((resolve) => setTimeout(resolve, delay));
		if (generation !== getSessionGeneration() || recovery !== recoveryGeneration) return;
		await loadPendingRollback();
		if (_pending) return;
	}
}

/** Confirm a pending rollback and clear only after the server acknowledges it. */
export async function confirmRollback(): Promise<void> {
	const txn = _pending;
	if (!txn || _confirmingTxnId === txn.txnId) return;
	_confirmingTxnId = txn.txnId;
	const generation = getSessionGeneration();
	const client = getClient();
	try {
		await client.call('system.network.confirm', { txn_id: txn.txnId });
		if (generation !== getSessionGeneration()) return;
		if (_pending?.txnId === txn.txnId) {
			setPending(null);
		}
	} catch (error) {
		if (generation !== getSessionGeneration()) return;
		if (_pending?.txnId !== txn.txnId) return;
		const message = errorMessage(error);
		_confirmationError = { txnId: txn.txnId, message };
		toastError(message);
	} finally {
		if (generation === getSessionGeneration() && _confirmingTxnId === txn.txnId) {
			_confirmingTxnId = null;
		}
	}
}
