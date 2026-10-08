export interface ClientTiming {
	method: string;
	duration_ms: number;
	outcome: 'pending' | 'ok' | 'error' | 'timeout' | 'disconnected';
	sequence?: number;
	epoch?: string;
	late_response_ms?: number;
	late_outcome?: 'ok' | 'error';
}
const history: ClientTiming[] = [];
export function trackClientTiming(timing: ClientTiming) {
	history.push(timing);
	if (history.length > 256) history.shift();
}
export function clearClientTimings() { history.length = 0; }
export interface BackendTiming { sequence: number; operation: string; stage: string; duration_ms: number; outcome: string }
export interface DiagnosticReport {
	schema_version: number; epoch: string; engine_version: string;
	kernel_version: string | null; cpu_count_bucket: number | null; memory_kib_bucket: number | null;
	pressure: Record<string, number>; detailed_capture: boolean; dropped_records: number;
	timings: BackendTiming[]; boot_phases: { operation: string; state: string; duration_ms: number | null }[];
	pool_context: { pool: string; total_bytes_bucket: number; device_count_bucket: number }[];
	limitations: string[];
	allowed_operations?: string[];
}

/** Re-read engine state on navigation; never assume an earlier capture is active. */
export async function resumeDiagnosticCapture(
	client: { call<T>(method: string): Promise<T> },
	identity: Pick<AuthMe, 'role' | 'scoped'>
): Promise<DiagnosticReport | null> {
	if (!hasRootEquivalentAccess(identity.role, identity.scoped)) return null;
	try {
		const report = await client.call<DiagnosticReport>('system.diagnostics.report');
		return report.detailed_capture ? report : null;
	} catch { return null; }
}
/** Build a fresh allowlisted export; no spread of server or client objects. */
export function buildDiagnosticExport(report: DiagnosticReport) {
	const aliases = new Map<number, string>();
	const alias = (sequence: number) => {
		if (!aliases.has(sequence)) aliases.set(sequence, `request-${aliases.size + 1}`);
		return aliases.get(sequence)!;
	};
	const operations = new Map(report.timings.filter(t => t.stage === 'backend').map(t => [t.sequence, t.operation]));
	const knownOperations = new Set(report.allowed_operations ?? []);
	const timings = report.timings.map(t => ({ request: alias(t.sequence), operation: t.operation, stage: t.stage, duration_ms: t.duration_ms, outcome: t.outcome }));
	const client = history.map(t => {
		const correlated = t.epoch === report.epoch && t.sequence !== undefined && operations.has(t.sequence);
		return { request: correlated ? alias(t.sequence!) : null, operation: correlated ? operations.get(t.sequence!) : knownOperations.has(t.method) ? t.method : 'unknown',
			duration_ms: t.duration_ms, outcome: t.outcome, late_response_ms: t.late_response_ms, late_outcome: t.late_outcome };
	});
	const groups = new Map<string, number[]>();
	for (const t of timings) {
		const key = `${t.operation} / ${t.stage}`;
		const durations = groups.get(key) ?? []; durations.push(t.duration_ms); groups.set(key, durations);
	}
	const summary = [...groups].map(([operation_stage, durations]) => {
		durations.sort((a, b) => a - b);
		return { operation_stage, count: durations.length, total_ms: durations.reduce((a, b) => a + b, 0), max_ms: durations.at(-1)!,
			p50_ms: durations[Math.ceil(durations.length * .5) - 1], p95_ms: durations[Math.ceil(durations.length * .95) - 1] };
	}).sort((a, b) => b.total_ms - a.total_ms);
	const pressure: Record<string, number> = {};
	for (const key of ['cpu_some_avg10', 'cpu_full_avg10', 'memory_some_avg10', 'memory_full_avg10', 'io_some_avg10', 'io_full_avg10']) {
		const value = report.pressure[key];
		if (typeof value === 'number' && Number.isFinite(value)) pressure[key] = value;
	}
	return { schema_version: report.schema_version, engine_version: report.engine_version, kernel_version: report.kernel_version,
		cpu_count_bucket: report.cpu_count_bucket, memory_kib_bucket: report.memory_kib_bucket, pressure,
		dropped_records: report.dropped_records, detailed_capture: report.detailed_capture,
		pool_context: report.pool_context.map(p => ({ pool: p.pool, total_bytes_bucket: p.total_bytes_bucket, device_count_bucket: p.device_count_bucket })),
		boot_phases: report.boot_phases.map(p => ({ operation: p.operation, state: p.state, duration_ms: p.duration_ms })),
		limitations: report.limitations, summary, backend_timings: timings, client_timings: client };
}
import { hasRootEquivalentAccess } from './access';
import type { AuthMe } from './types';
