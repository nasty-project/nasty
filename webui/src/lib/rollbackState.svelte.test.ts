import { beforeEach, describe, expect, it, vi } from 'vitest';

// Stub the underlying RPC client so we can drive `loadPendingRollback`
// deterministically — no real WebSocket needed for these tests.
const { callMock, toastErrorMock } = vi.hoisted(() => ({
	callMock: vi.fn(),
	toastErrorMock: vi.fn(),
}));
vi.mock('./client', () => ({
	getClient: () => ({ call: callMock }),
	getSessionGeneration: () => 0,
	registerSessionReset: vi.fn(),
}));
vi.mock('./toast.svelte', () => ({
	withToast: async (fn: () => Promise<unknown>) => {
		try {
			return await fn();
		} catch {
			return undefined;
		}
	},
	error: toastErrorMock,
}));

import { applyNetworkUpdate, confirmRollback, loadPendingRollback, recoverPendingRollback, rollbackState } from './rollbackState.svelte';

beforeEach(() => {
	callMock.mockReset();
	toastErrorMock.mockReset();
	rollbackState.clear();
});

describe('confirmRollback', () => {
	it('clears matching state after successful confirmation', async () => {
		rollbackState.set({ txnId: 'txn-1', revertAtUnix: 2000, riskReason: null });
		callMock.mockResolvedValueOnce(undefined);

		await confirmRollback();

		expect(callMock).toHaveBeenCalledWith('system.network.confirm', { txn_id: 'txn-1' });
		expect(rollbackState.pending).toBeNull();
		expect(rollbackState.confirmationError).toBeNull();
	});

	it('retains the transaction and error when confirmation fails', async () => {
		rollbackState.set({ txnId: 'txn-1', revertAtUnix: 2000, riskReason: null });
		callMock.mockRejectedValueOnce({ code: -32000, message: 'WebSocket disconnected' });

		await confirmRollback();

		expect(rollbackState.pending?.txnId).toBe('txn-1');
		expect(rollbackState.confirmationError).toBe('WebSocket disconnected');
		expect(toastErrorMock).toHaveBeenCalledWith('WebSocket disconnected');
	});

	it('preserves an error when the same transaction reloads', async () => {
		rollbackState.set({ txnId: 'txn-1', revertAtUnix: 2000, riskReason: null });
		callMock.mockRejectedValueOnce(new Error('Request timed out'));
		await confirmRollback();
		callMock.mockResolvedValueOnce([
			{ txn_id: 'txn-1', revert_at_unix: 2100, risk_reason: 'management changed' },
		]);

		await loadPendingRollback();

		expect(rollbackState.confirmationError).toBe('Request timed out');
		expect(rollbackState.pending?.revertAtUnix).toBe(2100);
	});

	it('does not attach a stale failure to a newer transaction', async () => {
		let rejectConfirmation: (error: unknown) => void = () => {};
		callMock.mockImplementationOnce(() => new Promise((_, reject) => {
			rejectConfirmation = reject;
		}));
		rollbackState.set({ txnId: 'txn-1', revertAtUnix: 2000, riskReason: null });
		const confirmation = confirmRollback();
		rollbackState.set({ txnId: 'txn-2', revertAtUnix: 3000, riskReason: null });
		rejectConfirmation(new Error('old failure'));

		await confirmation;

		expect(rollbackState.pending?.txnId).toBe('txn-2');
		expect(rollbackState.confirmationError).toBeNull();
		expect(toastErrorMock).not.toHaveBeenCalled();
	});
});

describe('applyNetworkUpdate', () => {
	it('blocks overlapping updates while a rollback decision is pending', async () => {
		rollbackState.set({
			txnId: 'txn-1',
			revertAtUnix: Math.floor(Date.now() / 1000) + 30,
			riskReason: null,
		});

		const result = await applyNetworkUpdate({
			interfaces: [],
			dns: [],
			bonds: [],
			vlans: [],
			bridges: [],
		}, 'Applied');

		expect(result).toBeUndefined();
		expect(callMock).not.toHaveBeenCalled();
		expect(toastErrorMock).toHaveBeenCalledWith(
			'Confirm or wait for the pending network change before applying another update',
		);
	});

	it('blocks a second update while the first RPC is in flight', async () => {
		let resolveUpdate: (value: unknown) => void = () => {};
		callMock.mockImplementationOnce(() => new Promise((resolve) => {
			resolveUpdate = resolve;
		}));
		const payload = { interfaces: [], dns: [], bonds: [], vlans: [], bridges: [] };
		const first = applyNetworkUpdate(payload, 'Applied');

		const second = await applyNetworkUpdate(payload, 'Applied again');

		expect(second).toBeUndefined();
		expect(callMock).toHaveBeenCalledOnce();
		resolveUpdate({});
		await first;
	});

	it('recovers a transaction registered after an ambiguous update failure', async () => {
		callMock
			.mockRejectedValueOnce(new Error('Request timed out'))
			.mockResolvedValueOnce([{ txn_id: 'txn-after-timeout', revert_at_unix: 3000, risk_reason: 'IP changed' }]);

		const result = await applyNetworkUpdate(
			{ interfaces: [], dns: [], bonds: [], vlans: [], bridges: [] },
			'Applied',
		);
		await vi.waitFor(() => expect(rollbackState.pending?.txnId).toBe('txn-after-timeout'));

		expect(result).toBeUndefined();
		expect(callMock).toHaveBeenCalledTimes(2);
	});
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

	it('continues recovery when transaction registration takes more than four seconds', async () => {
		vi.useFakeTimers();
		try {
			callMock
				.mockResolvedValueOnce([])
				.mockResolvedValueOnce([])
				.mockResolvedValueOnce([])
				.mockResolvedValueOnce([{ txn_id: 'txn-late', revert_at_unix: 3000, risk_reason: 'IP changed' }]);
			const recovery = recoverPendingRollback();

			await vi.advanceTimersByTimeAsync(9000);
			await recovery;

			expect(rollbackState.pending?.txnId).toBe('txn-late');
			expect(callMock).toHaveBeenCalledTimes(4);
		} finally {
			vi.useRealTimers();
		}
	});
});
