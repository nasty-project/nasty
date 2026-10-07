import { describe, expect, it } from 'vitest';
import type { Subvolume } from './types';
import { attachableVmDisks, validNewVmDisk, type VmDiskCandidate } from './vm-disk-safety';

const candidate = (name: string, consumers: string[] = [], device: string | null = `/dev/${name}`): VmDiskCandidate => ({
	subvolume: { name, filesystem: 'tank', subvolume_type: 'block', block_device: device } as Subvolume,
	consumers
});

describe('VM disk attachment choices', () => {
	it('excludes attached, offline, CSI, stopped-VM, export, and locally mounted disks', () => {
		const items = [candidate('free'), candidate('self'), candidate('offline', [], null),
			candidate('csi', ['Kubernetes CSI volume']), candidate('vm', ["VM other (stopped)"]),
			candidate('iscsi', ['iSCSI target']), candidate('nvme', ['NVMe-oF subsystem']), candidate('local', ['Local mount'])];
		expect(attachableVmDisks(items, ['/dev/self']).map(s => s.name)).toEqual(['free']);
	});
	it('does not admit filesystem subvolumes or erase consumer information', () => {
		const used = candidate('used', ['VM A', 'iSCSI B']);
		const fs = candidate('fs');
		fs.subvolume.subvolume_type = 'filesystem';
		expect(attachableVmDisks([used, fs])).toEqual([]);
		expect(used.consumers).toEqual(['VM A', 'iSCSI B']);
	});
});

describe('inline disk creation validation', () => {
	it('accepts a valid named volume and size', () => {
		expect(validNewVmDisk('tank', 'mos-data', 10)).toBe(true);
	});
	it.each([0, -1, NaN, Infinity, Number.MAX_SAFE_INTEGER])('rejects invalid or overflowing size %s', size => {
		expect(validNewVmDisk('tank', 'data', size)).toBe(false);
	});
	it('requires a filesystem and nonblank name', () => {
		expect(validNewVmDisk('', 'data', 10)).toBe(false);
		expect(validNewVmDisk('tank', '  ', 10)).toBe(false);
	});
});
