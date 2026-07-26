import { beforeEach, describe, expect, test, vi } from 'vitest';
import { getClient, registerSessionReset, resetClient } from './client';

beforeEach(() => resetClient());

describe('client session lifecycle', () => {
	test('creates a fresh client after reset', () => {
		const first = getClient();
		expect(getClient()).toBe(first);

		resetClient();

		expect(getClient()).not.toBe(first);
	});

	test('runs registered session reset handlers', () => {
		const reset = vi.fn();
		const unregister = registerSessionReset(reset);

		resetClient();
		expect(reset).toHaveBeenCalledOnce();

		unregister();
		resetClient();
		expect(reset).toHaveBeenCalledOnce();
	});
});
