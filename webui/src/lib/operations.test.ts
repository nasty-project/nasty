import { describe, expect, test } from 'vitest';
import { operationDetail } from './operations';
import type { Operation } from './types';

function scrub(overrides: Partial<Operation> = {}): Operation {
	return {
		kind: 'scrub',
		fs: 'first',
		state: 'idle',
		detail: 'Last run clean',
		control: 'start',
		...overrides,
	};
}

describe('operation details', () => {
	test('shows scrub age, outcome, and duration', () => {
		expect(operationDetail(scrub({
			last_run_at: 1_000,
			last_duration_secs: 7_260,
			last_outcome: 'ok',
		}), 1_000 + 3 * 86400)).toBe('Last run 3d ago · clean · took 2h 1m');
	});

	test('preserves live and non-scrub details', () => {
		expect(operationDetail(scrub({ state: 'running', detail: '42%' }), 2_000)).toBe('42%');
		expect(operationDetail({
			kind: 'reconcile', fs: 'first', state: 'idle', detail: 'Enabled', control: 'pause',
		}, 2_000)).toBe('Enabled');
	});

	test('falls back for old responses without structured history', () => {
		expect(operationDetail(scrub(), 2_000)).toBe('Last run clean');
	});

	test('shows a zero-second duration', () => {
		expect(operationDetail(scrub({
			last_run_at: 1_000,
			last_duration_secs: 0,
			last_outcome: 'ok',
		}), 1_000)).toBe('Last run 0s ago · clean · took 0s');
	});
});
