import { describe, expect, it } from 'vitest';
import {
	reachedUpdatePhase,
	shouldShowUpdateStatus,
	versionUpdatePhases
} from './update-progress';

describe('version update progress', () => {
	it('tracks the transactional update markers in order', () => {
		let log = '';
		expect(reachedUpdatePhase(log, versionUpdatePhases)).toBe(-1);

		for (let i = 0; i < versionUpdatePhases.length; i++) {
			log += `${versionUpdatePhases[i].marker}\n`;
			expect(reachedUpdatePhase(log, versionUpdatePhases)).toBe(i);
		}
	});

	it('keeps a running transaction visible before a marker is recognized', () => {
		expect(shouldShowUpdateStatus('running')).toBe(true);
		expect(reachedUpdatePhase('preparing transaction\n', versionUpdatePhases)).toBe(-1);
	});

	it('keeps completed and failed transaction output available', () => {
		expect(shouldShowUpdateStatus('success')).toBe(true);
		expect(shouldShowUpdateStatus('failed')).toBe(true);
		expect(shouldShowUpdateStatus('idle')).toBe(false);
		expect(shouldShowUpdateStatus(null)).toBe(false);
	});
});
