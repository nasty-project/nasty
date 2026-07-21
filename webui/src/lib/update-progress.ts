export interface UpdatePhase {
	label: string;
	marker: string;
}

export const versionUpdatePhases = [
	{ label: 'Fetch', marker: '==> Updating staged system flake...' },
	{ label: 'Build', marker: '==> Building staged system...' },
	{ label: 'Activate', marker: '==> Activating verified system closure...' },
	{ label: 'Done', marker: '==> Update complete!' }
] as const satisfies readonly UpdatePhase[];

export function reachedUpdatePhase(log: string, phases: readonly UpdatePhase[]): number {
	let reached = -1;
	for (let i = 0; i < phases.length; i++) {
		if (log.includes(phases[i].marker)) reached = i;
	}
	return reached;
}

export function shouldShowUpdateStatus(state: string | null): boolean {
	return state !== null && state !== 'idle';
}
