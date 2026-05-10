import { describe, expect, it } from 'vitest';
import { summarizeDependents } from './fs-dependents';
import type { FsDependents } from './types';

function deps(over: Partial<FsDependents> = {}): FsDependents {
	return {
		filesystem: 'tank',
		mounted: true,
		subvolumes: [],
		apps: [],
		vms: [],
		backup_jobs: [],
		nfs_shares: [],
		smb_shares: [],
		iscsi_targets: [],
		nvmeof_subsystems: [],
		...over,
	};
}

describe('summarizeDependents', () => {
	it('returns null when nothing depends on the filesystem', () => {
		// Caller short-circuits to the simple confirm dialog when this
		// helper returns null — no point listing zero things.
		expect(summarizeDependents(deps())).toBeNull();
	});

	it('singular noun when only one item in a category', () => {
		// "1 app (jellyfin)" not "1 apps (jellyfin)". Hand-rolled
		// pluralization is fragile but the labels are hand-curated.
		const summary = summarizeDependents(deps({ apps: ['jellyfin'] }));
		expect(summary).toBe('• 1 app (jellyfin)');
	});

	it('plural noun and comma-joined names when multiple items', () => {
		// Issue #86 reproducer-style content: jellyfin + transmission
		// running on the FS. The summary line shows count + names.
		const summary = summarizeDependents(deps({ apps: ['jellyfin', 'transmission'] }));
		expect(summary).toBe('• 2 apps (jellyfin, transmission)');
	});

	it('emits one bullet per non-empty category, in stable order', () => {
		// Order matters for predictable rendering across reloads.
		// Subvolumes first, then apps, then VMs, etc. — matches
		// the order users typically scan.
		const summary = summarizeDependents(
			deps({
				subvolumes: ['data'],
				apps: ['jellyfin'],
				vms: ['win11'],
			}),
		);
		expect(summary).toBe(
			'• 1 subvolume (data)\n' + '• 1 app (jellyfin)\n' + '• 1 VM (win11)',
		);
	});

	it('caps the inline list at 5 items with a +N more summary', () => {
		// Busy box could have 30 backup jobs touching the FS — we
		// truncate so the dialog doesn't sprawl. The cap is exposed
		// in this test so a future tweak shows up here first.
		const apps = ['a', 'b', 'c', 'd', 'e', 'f', 'g'];
		const summary = summarizeDependents(deps({ apps }));
		expect(summary).toBe('• 7 apps (a, b, c, d, e, … +2 more)');
	});

	it('formats VMs with the uppercase label', () => {
		// Hand-curated label list keeps the abbreviation correct.
		const summary = summarizeDependents(deps({ vms: ['lin', 'win'] }));
		expect(summary).toBe('• 2 VMs (lin, win)');
	});
});
