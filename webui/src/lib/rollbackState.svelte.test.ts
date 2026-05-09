import { beforeEach, describe, expect, it, vi } from 'vitest';

// Stub the underlying RPC client so we can drive `loadPendingRollback`
// deterministically — no real WebSocket needed for these tests.
const callMock = vi.fn();
vi.mock('./client', () => ({
	getClient: () => ({ call: callMock }),
}));
vi.mock('./toast.svelte', () => ({
	withToast: async (fn: () => Promise<unknown>) => fn(),
}));

import { loadPendingRollback, rollbackState } from './rollbackState.svelte';

beforeEach(() => {
	callMock.mockReset();
	rollbackState.clear();
});

describe('loadPendingRollback', () => {
	it('populates the store from a single pending txn', async () => {
		// The headline case: user changed the mgmt iface IP, original
		// session got dropped, fresh session connects on the new IP and
		// pulls the pending txn so the banner reappears.
		callMock.mockResolvedValueOnce([
			{
				txn_id: 'txn-abc',
				revert_at_unix: 1234567890,
				risk_reason: 'IP config of management iface eth0 is changing',
			},
		]);
		await loadPendingRollback();
		expect(rollbackState.pending).toEqual({
			txnId: 'txn-abc',
			revertAtUnix: 1234567890,
			riskReason: 'IP config of management iface eth0 is changing',
		});
	});

	it('clears local state when the server reports nothing pending', async () => {
		// On reconnect after the rollback already fired (or after the
		// user confirmed in another tab), the local store may still
		// hold a stale entry. The server's empty response is the
		// authoritative "nothing pending" signal.
		rollbackState.set({ txnId: 'stale', revertAtUnix: 1, riskReason: null });
		callMock.mockResolvedValueOnce([]);
		await loadPendingRollback();
		expect(rollbackState.pending).toBeNull();
	});

	it('picks the soonest-expiring txn when multiple are pending', async () => {
		// Pathological case (the server table almost never has more
		// than one entry), but if it does we'd rather show the user
		// the most-urgent one — they need to make a decision sooner.
		callMock.mockResolvedValueOnce([
			{ txn_id: 'later', revert_at_unix: 2000, risk_reason: 'a' },
			{ txn_id: 'sooner', revert_at_unix: 1500, risk_reason: 'b' },
			{ txn_id: 'middle', revert_at_unix: 1700, risk_reason: 'c' },
		]);
		await loadPendingRollback();
		expect(rollbackState.pending?.txnId).toBe('sooner');
	});

	it('leaves local state alone if the RPC fails', async () => {
		// An older engine (pre-this-PR) doesn't have the RPC, so
		// the call rejects with method-not-found. We must not
		// clobber whatever pending state we have locally — the user
		// might have just gotten it from `applyNetworkUpdate`.
		rollbackState.set({ txnId: 'local', revertAtUnix: 999, riskReason: null });
		callMock.mockRejectedValueOnce(new Error('method not found'));
		await loadPendingRollback();
		expect(rollbackState.pending?.txnId).toBe('local');
	});

	it('maps empty risk_reason string to null', async () => {
		// Server-side risk_reason is non-Optional (always a string,
		// possibly empty). The local PendingRollback shape uses
		// `string | null` because the banner code treats null as
		// "no tooltip". Empty string would render as an empty tooltip.
		callMock.mockResolvedValueOnce([
			{ txn_id: 'txn-1', revert_at_unix: 1, risk_reason: '' },
		]);
		await loadPendingRollback();
		expect(rollbackState.pending?.riskReason).toBeNull();
	});
});
