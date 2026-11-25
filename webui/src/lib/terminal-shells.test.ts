import { describe, expect, it } from 'vitest';
import { nextFreeShell, pickShell } from './terminal-shells';

describe('terminal shells', () => {
	it('reopens the remembered shell only while it runs', () => {
		expect(pickShell([1, 3], 3)).toBe(3);
		expect(pickShell([1, 3], 2)).toBe(1);
		expect(pickShell([], 2)).toBe(1);
		expect(pickShell([2], null)).toBe(2);
	});

	it('finds the lowest free shell', () => {
		expect(nextFreeShell([1, 3], 4)).toBe(2);
		expect(nextFreeShell([1, 2, 3, 4], 4)).toBeNull();
	});
});
