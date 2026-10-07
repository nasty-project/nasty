import type { Subvolume } from './types';

export type VmDiskCandidate = { subvolume: Subvolume; consumers: string[] };

/** Presentation only: keep the original consumer strings for safety/details. */
export function diskUsagePresentation(candidate: VmDiskCandidate): { name: string; categories: string[] } {
	const { subvolume, consumers } = candidate;
	const properties = subvolume.properties ?? {};
	const pvc = properties['nasty-csi:pvc_name'];
	const namespace = properties['nasty-csi:pvc_namespace'];
	const categories = [...new Set(consumers.map(consumer => {
		if (consumer.startsWith('Kubernetes CSI volume')) return 'Kubernetes';
		if (consumer.startsWith('iSCSI target')) return 'iSCSI';
		if (consumer.startsWith('NVMe-oF subsystem')) return 'NVMe-oF';
		if (consumer.startsWith('VM ')) return 'Virtual machine';
		if (consumer.startsWith('Local mount')) return 'Local mount';
		return 'Other usage';
	}))];
	return { name: pvc && namespace ? `${namespace}/${pvc}` : `${subvolume.filesystem}/${subvolume.name}`, categories };
}

/** Omit the obvious current attachment, but never hide a second reservation. */
export function otherDiskConsumers(consumers: string[], vmName: string): string[] {
	const own = consumers.findIndex(consumer => consumer === `VM '${vmName}' (running)` || consumer === `VM '${vmName}' (stopped)`);
	return consumers.filter((_, index) => index !== own);
}

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
