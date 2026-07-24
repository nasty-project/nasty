import { describe, expect, test } from 'vitest';
import {
	activeNavigationGroup,
	commonNavigation,
	currentNavigationItem,
	flattenNavigation,
	isNavGroup,
	resolveNavigation,
	searchNavigation
} from './navigation';

describe('navigation model', () => {
	test('preserves the full hierarchy and order', () => {
		const entries = resolveNavigation({ kvmAvailable: true });
		expect(entries.map((entry) => entry.label)).toEqual([
			'Dashboard', 'Storage', 'Sharing', 'Protection', 'Compute', 'Terminal', 'System'
		]);
		const storage = entries.find((entry) => entry.id === 'storage');
		expect(storage && isNavGroup(storage) ? storage.children.map((item) => item.label) : [])
			.toEqual(['Filesystems', 'Subvolumes', 'Disks', 'Operations', 'Files']);
		expect(flattenNavigation(entries).map((entry) => entry.href)).toEqual([
			'/',
			'/filesystems', '/subvolumes', '/disks', '/operations', '/files',
			'/sharing',
			'/backups', '/alerts', '/firewall', '/tls', '/ingress', '/vpn',
			'/vms', '/apps',
			'/terminal',
			'/services', '/hardware', '/logs', '/update', '/users', '/settings'
		]);
	});

	test('uses unique stable IDs and routes', () => {
		const items = flattenNavigation(resolveNavigation({ kvmAvailable: true }));
		expect(new Set(items.map((entry) => entry.id)).size).toBe(items.length);
		expect(new Set(items.map((entry) => entry.href)).size).toBe(items.length);
	});

	test('gates VMs on KVM without changing Apps', () => {
		const withoutKvm = flattenNavigation(resolveNavigation({ kvmAvailable: false }));
		expect(withoutKvm.some((item) => item.href === '/vms')).toBe(false);
		expect(withoutKvm.some((item) => item.href === '/apps')).toBe(true);

		const withKvm = flattenNavigation(resolveNavigation({ kvmAvailable: true }));
		expect(withKvm.some((item) => item.href === '/vms')).toBe(true);
	});

	test('derives the existing Common menu order', () => {
		const common = commonNavigation(resolveNavigation({ kvmAvailable: true }));
		expect(common.map((item) => item.href)).toEqual([
			'/', '/filesystems', '/subvolumes', '/disks', '/files', '/apps', '/vms', '/backups', '/logs'
		]);
	});

	test('matches active items and their owning group', () => {
		const entries = resolveNavigation({ kvmAvailable: true });
		expect(currentNavigationItem('/subvolumes/details', entries).href).toBe('/subvolumes');
		expect(activeNavigationGroup('/subvolumes/details', entries)).toBe('storage');
		expect(activeNavigationGroup('/sharing', entries)).toBeNull();
	});

	test('search uses item keywords and group labels', () => {
		const entries = resolveNavigation({ kvmAvailable: true });
		expect(searchNavigation(entries, 'copygc')).toEqual(new Set(['/operations']));
		expect(searchNavigation(entries, 'storage')).toEqual(new Set([
			'/filesystems', '/subvolumes', '/disks', '/operations', '/files'
		]));
	});

	test('search cannot expose capability-gated entries', () => {
		const entries = resolveNavigation({ kvmAvailable: false });
		expect(searchNavigation(entries, 'virtual machine')).toEqual(new Set());
	});
});
