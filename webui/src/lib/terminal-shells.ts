/** Persistent terminal shells (#1085): numbered 1..max per user. */

/** Shell to open: the remembered one if still running, else the first running, else 1. */
export function pickShell(running: number[], remembered: number | null): number {
	if (remembered !== null && running.includes(remembered)) return remembered;
	return running[0] ?? 1;
}

/** Lowest unused shell number, or null when all are in use. */
export function nextFreeShell(running: number[], max: number): number | null {
	for (let n = 1; n <= max; n++) if (!running.includes(n)) return n;
	return null;
}
