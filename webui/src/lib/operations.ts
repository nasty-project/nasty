import type { Operation } from './types';

function relativeAge(unixSeconds: number, nowSeconds: number): string {
	const delta = Math.max(0, Math.floor(nowSeconds) - unixSeconds);
	if (delta < 60) return `${delta}s ago`;
	if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
	if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
	return `${Math.floor(delta / 86400)}d ago`;
}

function duration(seconds: number): string {
	if (seconds < 60) return `${seconds}s`;
	if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
	const hours = Math.floor(seconds / 3600);
	const minutes = Math.floor((seconds % 3600) / 60);
	return minutes ? `${hours}h ${minutes}m` : `${hours}h`;
}

export function operationDetail(
	operation: Operation,
	nowSeconds = Date.now() / 1000,
): string {
	if (operation.kind !== 'scrub' || operation.state === 'running' || operation.last_run_at == null) {
		return operation.detail;
	}
	const outcomeLabels = {
		ok: 'clean',
		errors: 'found errors',
		failed: 'failed',
		cancelled: 'cancelled',
	} satisfies Record<NonNullable<Operation['last_outcome']>, string>;
	const outcome = operation.last_outcome ? outcomeLabels[operation.last_outcome] : 'completed';
	const took = operation.last_duration_secs != null
		? ` · took ${duration(operation.last_duration_secs)}`
		: '';
	return `Last run ${relativeAge(operation.last_run_at, nowSeconds)} · ${outcome}${took}`;
}
