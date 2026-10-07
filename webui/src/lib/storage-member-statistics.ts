import type { DiskHealth, FilesystemDevice, FsDeviceUsage } from '$lib/types';

type DiskRate = { readRate: number; writeRate: number };

// Usage output may contain bare kernel names. Never equate the basename of
// a persistent alias with a kernel name, or guess partition parents.
function devicePath(path: string): string {
	return path.includes('/') ? path : `/dev/${path}`;
}

function memberPaths(device: FilesystemDevice): string[] {
	return device.missing ? [] : [device.path, device.kernel_path].filter((path): path is string => !!path);
}

export function memberUsage(device: FilesystemDevice, usages: FsDeviceUsage[]): FsDeviceUsage | undefined {
	const paths = memberPaths(device);
	const matches = usages.filter((usage) => paths.includes(devicePath(usage.path)));
	return matches.length === 1 ? matches[0] : undefined;
}

export function memberHealthEntries(device: FilesystemDevice, health: DiskHealth[]): DiskHealth[] {
	const paths = memberPaths(device);
	if (paths.length === 0) return [];
	const direct = health.filter((disk) => paths.includes(devicePath(disk.device)));
	if (direct.length > 0) return direct;
	return health.filter((disk) => device.parent_path === devicePath(disk.device));
}

export function memberRate(device: FilesystemDevice, rates: Map<string, DiskRate>): DiskRate | undefined {
	const paths = memberPaths(device);
	if (paths.length === 0) return undefined;
	for (const path of [...paths, device.parent_path]) {
		if (!path) continue;
		const direct = rates.get(path);
		if (direct) return direct;
		// diskstats uses kernel names, not by-id basenames.
		if (/^\/dev\/[^/]+$/.test(path)) {
			const rate = rates.get(path.slice('/dev/'.length));
			if (rate) return rate;
		}
	}
	return undefined;
}
