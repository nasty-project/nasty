export function formatBytes(bytes: number): string {
	if (bytes === 0) return '0 B';
	const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
	const i = Math.floor(Math.log(bytes) / Math.log(1024));
	const val = bytes / Math.pow(1024, i);
	return `${val.toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
}

// Returns a formatter that always uses the unit determined by `maxBytes`,
// so all Y-axis ticks on a chart share the same unit.
export function makeBytesFormatter(maxBytes: number): (v: number) => string {
	const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
	const i = maxBytes > 0 ? Math.floor(Math.log(maxBytes) / Math.log(1024)) : 0;
	const divisor = Math.pow(1024, i);
	const unit = units[i];
	return (v: number) => `${(v / divisor).toFixed(i > 0 ? 1 : 0)} ${unit}`;
}

export function formatUptime(seconds: number): string {
	const days = Math.floor(seconds / 86400);
	const hours = Math.floor((seconds % 86400) / 3600);
	const mins = Math.floor((seconds % 3600) / 60);
	if (days > 0) return `${days}d ${hours}h ${mins}m`;
	if (hours > 0) return `${hours}h ${mins}m`;
	return `${mins}m`;
}

export function formatPercent(used: number, total: number): string {
	if (total === 0) return '0%';
	return `${((used / total) * 100).toFixed(1)}%`;
}

/**
 * Fraction of the sum of physical disk sizes that bcachefs actually reports
 * as filesystem capacity after format. The rest is reserved for metadata,
 * journal, btree, and gc_reserve_percent (which defaults to 8). 0.91 is
 * what we observe for fresh single-tier mirrors against bcachefs 1.38 — it
 * varies a couple of points up or down depending on filesystem size and
 * bcachefs version, so anything that uses it is labelled "approximate" in
 * the UI.
 */
export const BCACHEFS_FS_OVERHEAD = 0.91;

/**
 * Estimate user-facing usable bytes given a raw byte total, a device count,
 * a replica count, and whether erasure coding is on. The math:
 *
 *   - Mirror (no EC): usable ≈ rawTotal / replicas
 *   - Erasure code (RAID-5 if replicas=2, RAID-6 if replicas=3):
 *       parity = replicas - 1
 *       usable ≈ rawTotal × (deviceCount - parity) / deviceCount
 *
 * Returns 0 for nonsensical inputs (no devices, EC with too few devices, etc.).
 *
 * Caveat baked into the doc and the UI tooltip: bcachefs lets subvolumes
 * override replica counts and apply different durability per file, so this
 * is an approximation for the simple-case configuration.
 */
export function estimateUsableBytes(
	rawTotal: number,
	deviceCount: number,
	replicas: number,
	erasureCode: boolean
): number {
	if (rawTotal <= 0 || deviceCount <= 0 || replicas <= 0) return 0;
	let factor: number;
	if (erasureCode) {
		const parity = replicas - 1;
		if (parity <= 0 || deviceCount <= parity) return 0;
		factor = (deviceCount - parity) / deviceCount;
	} else {
		factor = 1 / replicas;
	}
	return Math.floor(rawTotal * factor);
}
