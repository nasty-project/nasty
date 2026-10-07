import { describe, expect, it } from 'vitest';
import type { Subvolume } from './types';
import { attachableVmDisks, validNewVmDisk, diskUsagePresentation, otherDiskConsumers, type VmDiskCandidate } from './vm-disk-safety';

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

describe('disk usage presentation', () => {
	it('leads with the PVC namespace/name and deduplicates compact usage categories', () => {
		const item = candidate('pvc-long-uuid', ['Kubernetes CSI volume (PVC db/postgres-3); reserved even when not mounted', "iSCSI target 'long-iqn'", "iSCSI target 'another-iqn'"]);
		item.subvolume.properties = { 'nasty-csi:pvc_name': 'postgres-3', 'nasty-csi:pvc_namespace': 'db' };
		expect(diskUsagePresentation(item)).toEqual({ name: 'db/postgres-3', categories: ['Kubernetes', 'iSCSI'] });
		expect(item.subvolume.name).toBe('pvc-long-uuid');
		expect(item.consumers).toHaveLength(3);
		expect(attachableVmDisks([item])).toEqual([]);
	});
	it('falls back to the real volume name without inventing workload names', () => {
		const item = candidate('data', ['Unknown consumer']);
		expect(diskUsagePresentation(item)).toEqual({ name: 'tank/data', categories: ['Other usage'] });
		item.subvolume.properties = { 'nasty-csi:pvc_name': 'partial-metadata' };
		expect(diskUsagePresentation(item).name).toBe('tank/data');
	});
	it('recognizes VM, NVMe-oF, and local mount usage without displaying identifiers as badges', () => {
		const item = candidate('data', ["VM 'other' (stopped)", "NVMe-oF subsystem 'nqn-long'", "Local mount '/mnt/data'"]);
		expect(diskUsagePresentation(item).categories).toEqual(['Virtual machine', 'NVMe-oF', 'Local mount']);
	});
	it('omits only the obvious current VM attachment from attached-row summaries', () => {
		expect(otherDiskConsumers(["VM 'mos' (running)"], 'mos')).toEqual([]);
		const consumers = ["VM 'mos' (stopped)", 'Kubernetes CSI volume', "iSCSI target 'iqn'"];
		expect(otherDiskConsumers(consumers, 'mos')).toEqual(['Kubernetes CSI volume', "iSCSI target 'iqn'"]);
		expect(consumers).toHaveLength(3);
		expect(otherDiskConsumers(["VM 'other' (stopped)"], 'mos')).toEqual(["VM 'other' (stopped)"]);
	});
	it('does not hide another VM reservation even when names coincide', () => {
		expect(otherDiskConsumers(["VM 'mos' (stopped)", "VM 'mos' (stopped)"], 'mos')).toEqual(["VM 'mos' (stopped)"]);
	});
});
