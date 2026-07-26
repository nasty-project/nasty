import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { resetClient } from './client';
import { dismiss, error, getToasts, info, isBusy, success, withToast } from './toast.svelte';

// Toast state is module-scoped, so each test must start from an empty queue.
function clearAllToasts() {
	for (const t of [...getToasts()]) dismiss(t.id);
}

beforeEach(() => {
	vi.useFakeTimers();
	clearAllToasts();
});

afterEach(() => {
	vi.useRealTimers();
});

describe('toast queue', () => {
	test('success / error / info push toasts of the right type', () => {
		success('saved');
		error('boom');
		info('heads up');
		const toasts = getToasts();
		expect(toasts).toHaveLength(3);
		expect(toasts[0]).toMatchObject({ type: 'success', message: 'saved' });
		expect(toasts[1]).toMatchObject({ type: 'error', message: 'boom' });
		expect(toasts[2]).toMatchObject({ type: 'info', message: 'heads up' });
	});

	test('every toast gets a unique numeric id', () => {
		success('a');
		success('b');
		success('c');
		const ids = getToasts().map((t) => t.id);
		expect(new Set(ids).size).toBe(3);
	});

	test('dismiss removes only the matching toast', () => {
		success('keep');
		success('drop');
		const dropId = getToasts().find((t) => t.message === 'drop')!.id;
		dismiss(dropId);
		expect(getToasts().map((t) => t.message)).toEqual(['keep']);
	});

	test('success/info auto-dismiss after 5 seconds', () => {
		success('saved');
		expect(getToasts()).toHaveLength(1);
		vi.advanceTimersByTime(4999);
		expect(getToasts()).toHaveLength(1);
		vi.advanceTimersByTime(1);
		expect(getToasts()).toHaveLength(0);
	});

	test('error toasts stay longer (8 seconds) than success/info', () => {
		error('boom');
		vi.advanceTimersByTime(5000);
		expect(getToasts()).toHaveLength(1); // would have been gone if 5s
		vi.advanceTimersByTime(3000);
		expect(getToasts()).toHaveLength(0);
	});
});

describe('withToast', () => {
	test('returns the wrapped function\'s result', async () => {
		const result = await withToast(async () => 42);
		expect(result).toBe(42);
	});

	test('shows a success toast when successMsg is provided', async () => {
		await withToast(async () => 'ok', 'Saved!');
		expect(getToasts()).toMatchObject([{ type: 'success', message: 'Saved!' }]);
	});

	test('catches Error instances and surfaces .message in an error toast', async () => {
		const result = await withToast(async () => {
			throw new Error('disk full');
		});
		expect(result).toBeUndefined();
		expect(getToasts()).toMatchObject([{ type: 'error', message: 'disk full' }]);
	});

	test('catches JSON-RPC error envelopes ({ code, message }) and surfaces message', async () => {
		// rpc.ts's call() rejects with the raw server error object on JSON-RPC
		// failures — that's not an Error instance.
		const result = await withToast(async () => {
			throw { code: -32601, message: 'no such filesystem' };
		});
		expect(result).toBeUndefined();
		expect(getToasts()).toMatchObject([
			{ type: 'error', message: 'no such filesystem' }
		]);
	});

	test('falls back to String(e) for primitives and unrecognised shapes', async () => {
		const result = await withToast(async () => {
			throw 'plain string';
		});
		expect(result).toBeUndefined();
		expect(getToasts()).toMatchObject([{ type: 'error', message: 'plain string' }]);
	});

	test('isBusy is true while in flight and false again after', async () => {
		expect(isBusy()).toBe(false);
		let releaseInner!: () => void;
		const inner = new Promise<void>((res) => {
			releaseInner = res;
		});
		const op = withToast(async () => {
			await inner;
		});
		expect(isBusy()).toBe(true);
		releaseInner();
		await op;
		expect(isBusy()).toBe(false);
	});

	test('isBusy stays true while any concurrent op is in flight', async () => {
		let releaseA!: () => void;
		let releaseB!: () => void;
		const a = new Promise<void>((res) => {
			releaseA = res;
		});
		const b = new Promise<void>((res) => {
			releaseB = res;
		});
		const opA = withToast(async () => {
			await a;
		});
		const opB = withToast(async () => {
			await b;
		});
		expect(isBusy()).toBe(true);
		releaseA();
		await opA;
		expect(isBusy()).toBe(true); // b still running
		releaseB();
		await opB;
		expect(isBusy()).toBe(false);
	});

	test('does not surface completion from a previous session', async () => {
		let rejectInner!: (error: Error) => void;
		const inner = new Promise<void>((_, reject) => {
			rejectInner = reject;
		});
		const op = withToast(() => inner, 'Old session saved');

		resetClient();
		rejectInner(new Error('Old session disconnected'));
		await op;

		expect(getToasts()).toEqual([]);
	});

	test('reset clears busy state without an old operation decrementing the new session', async () => {
		let releaseInner!: () => void;
		const inner = new Promise<void>((resolve) => {
			releaseInner = resolve;
		});
		const op = withToast(() => inner);
		expect(isBusy()).toBe(true);

		resetClient();
		expect(isBusy()).toBe(false);
		releaseInner();
		await op;

		expect(isBusy()).toBe(false);
	});
});
