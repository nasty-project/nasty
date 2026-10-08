import { beforeEach, describe, expect, it, vi } from 'vitest';
import { buildDiagnosticExport, clearClientTimings, resumeDiagnosticCapture, trackClientTiming, type DiagnosticReport } from './performance-diagnostics';
const report: DiagnosticReport = { schema_version: 1, epoch: 'private-epoch', engine_version: '1', kernel_version: '6.18', cpu_count_bucket: 8, memory_kib_bucket: 1024, pressure: {}, detailed_capture: false, dropped_records: 0, boot_phases: [], pool_context: [], limitations: [], timings: [{ sequence: 900, operation: 'fs.list', stage: 'backend', duration_ms: 15000, outcome: 'ok' }] };
describe('performance report privacy and summaries', () => {
	beforeEach(clearClientTimings);
	it('relabels correlations and excludes raw methods, ids and epochs', () => {
		trackClientTiming({ method: '/fs/private/secret', duration_ms: 10000, outcome: 'timeout', sequence: 900, epoch: report.epoch, late_response_ms: 15050, late_outcome: 'ok' });
		const result = buildDiagnosticExport(report);
		expect(result.client_timings[0]).toMatchObject({ request: 'request-1', operation: 'fs.list', outcome: 'timeout', late_outcome: 'ok' });
		const text = JSON.stringify(result);
		expect(text).not.toContain('private'); expect(text).not.toContain('900');
		expect(result.summary[0]).toMatchObject({ max_ms: 15000, p95_ms: 15000, total_ms: 15000 });
	});
	it('does not correlate records from a prior engine instance', () => {
		trackClientTiming({ method: 'private-method', duration_ms: 10, outcome: 'ok', sequence: 900, epoch: 'previous' });
		expect(buildDiagnosticExport(report).client_timings[0]).toMatchObject({ request: null, operation: 'unknown' });
	});
	it('bounds browser history and clears it', () => {
		for (let i = 0; i < 300; i++) trackClientTiming({ method: 'secret', duration_ms: i, outcome: 'ok' });
		expect(buildDiagnosticExport(report).client_timings).toHaveLength(256);
		clearClientTimings(); expect(buildDiagnosticExport(report).client_timings).toHaveLength(0);
	});
	it('omits future non-allowlisted fields from nested context', () => {
		const unsafe = { ...report, pressure: { cpu_some_avg10: 1, private_host: 5 },
			pool_context: [{ pool: 'pool-1', total_bytes_bucket: 1024, device_count_bucket: 2, serial: 'private-serial' }],
			boot_phases: [{ operation: 'filesystems.restore_mounts', state: 'failed', duration_ms: 10, error: '/fs/private-path' }] };
		const text = JSON.stringify(buildDiagnosticExport(unsafe));
		expect(text).not.toContain('private'); expect(text).toContain('cpu_some_avg10');
	});
	it('identifies an unresolved timeout only using the engine method allowlist', () => {
		trackClientTiming({ method: 'fs.list', duration_ms: 10000, outcome: 'timeout' });
		trackClientTiming({ method: 'private-method', duration_ms: 10000, outcome: 'timeout' });
		const result = buildDiagnosticExport({ ...report, allowed_operations: ['fs.list'] });
		expect(result.client_timings.map(t => t.operation)).toEqual(['fs.list', 'unknown']);
	});
});

describe('resuming detailed capture on navigation', () => {
	it('loads an active capture for an unscoped admin', async () => {
		const active = { ...report, detailed_capture: true };
		const call = vi.fn().mockResolvedValue(active);
		expect(await resumeDiagnosticCapture({ call }, { role: 'admin', scoped: false })).toBe(active);
		expect(call).toHaveBeenCalledWith('system.diagnostics.report');
	});
	it('rechecks capture state and does not restore stopped or expired captures', async () => {
		const call = vi.fn().mockResolvedValueOnce({ ...report, detailed_capture: true }).mockResolvedValueOnce(report);
		expect(await resumeDiagnosticCapture({ call }, { role: 'admin', scoped: false })).not.toBeNull();
		expect(await resumeDiagnosticCapture({ call }, { role: 'admin', scoped: false })).toBeNull();
	});
	it('does not fetch reports for scoped or non-admin identities', async () => {
		const call = vi.fn();
		for (const identity of [{ role: 'admin' as const, scoped: true }, { role: 'operator' as const, scoped: false }, { role: 'readonly' as const, scoped: false }]) {
			expect(await resumeDiagnosticCapture({ call }, identity)).toBeNull();
		}
		expect(call).not.toHaveBeenCalled();
	});
	it('handles unavailable diagnostics without blocking settings', async () => {
		const call = vi.fn().mockRejectedValue(new Error('Unavailable'));
		expect(await resumeDiagnosticCapture({ call }, { role: 'admin', scoped: false })).toBeNull();
	});
});
