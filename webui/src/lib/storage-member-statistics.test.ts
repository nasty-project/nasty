import { describe, expect, it } from 'vitest';
import type { DiskHealth, FilesystemDevice, FsDeviceUsage } from '$lib/types';
import { memberUsage, memberHealthEntries, memberRate } from './storage-member-statistics';

const device: FilesystemDevice = {
	path: '/dev/disk/by-id/wwn-disk-a-part1', kernel_path: '/dev/sda1', parent_path: '/dev/sda',
	label: 'hdd.a', durability: 1, state: 'rw', data_allowed: null, has_data: null, discard: null
};
const usage = (path: string): FsDeviceUsage => ({ path, used_bytes: 20, free_bytes: 80, total_bytes: 100 });
const health = (device: string): DiskHealth => ({ device, temperature_c: 35, health_passed: true, smart_status: 'PASSED' } as DiskHealth);
const rate = { readRate: 10, writeRate: 20 };

describe('storage member statistics', () => {
	it('joins by-id capacity to the live partition, not the parent or another disk', () => {
		const correct = usage('/dev/sda1');
		expect(memberUsage(device, [usage('/dev/sdb1'), usage('/dev/sda'), correct])).toBe(correct);
		expect(memberUsage(device, [usage('sda1')])?.total_bytes).toBe(100);
		expect(device.path).toBe('/dev/disk/by-id/wwn-disk-a-part1');
	});
	it('uses the explicit parent for SMART/temperature and whole-disk I/O', () => {
		const correct = health('/dev/sda');
		expect(memberHealthEntries(device, [health('/dev/sdb'), correct])).toEqual([correct]);
		expect(memberRate(device, new Map([['sda', rate], ['sdb', { readRate: 99, writeRate: 99 }]]))).toBe(rate);
	});
	it('prefers member-specific I/O and SMART over parent statistics', () => {
		const partitionRate = { readRate: 1, writeRate: 2 };
		expect(memberRate(device, new Map([['sda', rate], ['sda1', partitionRate]]))).toBe(partitionRate);
		const partitionHealth = health('/dev/sda1');
		expect(memberHealthEntries(device, [health('/dev/sda'), partitionHealth])).toEqual([partitionHealth]);
	});
	it('handles direct paths and old engines without the optional mapping', () => {
		const direct = { ...device, path: '/dev/sda1', kernel_path: undefined, parent_path: undefined };
		expect(memberUsage(direct, [usage('/dev/sda1')])).toBeDefined();
		expect(memberRate(direct, new Map([['sda1', rate]]))).toBe(rate);
		expect(memberHealthEntries(direct, [health('/dev/sda1')])).toHaveLength(1);
	});
	it.each([
		{ ...device, missing: true },
		{ ...device, kernel_path: undefined, parent_path: undefined },
		{ ...device, path: '(missing dev-0)', kernel_path: undefined, parent_path: undefined }
	])('does not borrow statistics when a member is missing or unresolved', (member) => {
		expect(memberUsage(member, [usage('/dev/sda1')])).toBeUndefined();
		expect(memberHealthEntries(member, [health('/dev/sda')])).toEqual([]);
		expect(memberRate(member, new Map([['sda', rate]]))).toBeUndefined();
	});
	it('does not equate unrelated alias basenames or guess parents', () => {
		expect(memberUsage(device, [usage('/other/wwn-disk-a-part1')])).toBeUndefined();
		expect(memberRate(device, new Map([['wwn-disk-a-part1', rate]]))).toBeUndefined();
		const nvme = { ...device, kernel_path: '/dev/nvme0n1p2', parent_path: '/dev/nvme0n1' };
		expect(memberRate(nvme, new Map([['nvme0n1', rate]]))).toBe(rate);
	});
	it('leaves ambiguous capacity data unavailable', () => {
		expect(memberUsage(device, [usage('/dev/sda1'), usage(device.path)])).toBeUndefined();
	});
});
