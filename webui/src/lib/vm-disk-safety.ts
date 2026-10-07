import type { Subvolume } from './types';

export type VmDiskCandidate = { subvolume: Subvolume; consumers: string[] };

export function attachableVmDisks(candidates: VmDiskCandidate[], attached: string[] = []): Subvolume[] {
	return candidates.filter(({ subvolume, consumers }) =>
		subvolume.subvolume_type === 'block' && subvolume.block_device &&
		!attached.includes(subvolume.block_device) && consumers.length === 0
	).map(c => c.subvolume);
}

export function validNewVmDisk(filesystem: string, name: string, sizeGiB: number): boolean {
	return !!filesystem && !!name.trim() && Number.isFinite(sizeGiB) && sizeGiB > 0 &&
		Number.isSafeInteger(sizeGiB * 1073741824);
}
