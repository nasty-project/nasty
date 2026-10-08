import { beforeEach, describe, expect, it } from 'vitest';
import { buildDiagnosticExport, clearClientTimings, trackClientTiming, type DiagnosticReport } from './performance-diagnostics';
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
