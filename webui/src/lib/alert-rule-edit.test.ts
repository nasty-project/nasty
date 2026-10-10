import { describe, expect, it } from 'vitest';
import { alertRuleEditPatch, displayedAlertThreshold } from './alert-rule-edit';
import type { AlertRule } from './types';

const rule: AlertRule = { id: 'disk-temp-warn', name: 'Disk temperature warning', enabled: true, metric: 'disk_temperature', condition: 'above', threshold: 50, severity: 'warning' };

describe('editing existing alert rules', () => {
	it('edits a default Celsius threshold without replacing the rule or enabled state', () => {
		expect(alertRuleEditPatch(rule, 'celsius', rule.name, 65, 'warning')).toEqual({ id: rule.id, threshold: 65 });
	});
	it('prefills Fahrenheit and converts an edited value back to Celsius', () => {
		expect(displayedAlertThreshold(rule, 'fahrenheit')).toBe(122);
		expect(alertRuleEditPatch(rule, 'fahrenheit', rule.name, 140, 'critical')).toEqual({ id: rule.id, threshold: 60, severity: 'critical' });
	});
	it('preserves exact stored thresholds when only name or severity changes', () => {
		const precise = { ...rule, threshold: 50.125 };
		expect(alertRuleEditPatch(precise, 'fahrenheit', 'Renamed', displayedAlertThreshold(precise, 'fahrenheit'), 'critical')).toEqual({ id: rule.id, name: 'Renamed', severity: 'critical' });
	});
	it('does not temperature-convert other metrics or alter fixed backup-failure semantics', () => {
		const usage = { ...rule, metric: 'fs_usage_percent' as const, threshold: 80 };
		expect(alertRuleEditPatch(usage, 'fahrenheit', usage.name, 90, 'warning')).toEqual({ id: rule.id, threshold: 90 });
		const backup = { ...rule, metric: 'backup_failure' as const, threshold: 1 };
		expect(alertRuleEditPatch(backup, 'celsius', backup.name, 5, 'critical')).toEqual({ id: rule.id, severity: 'critical' });
	});
	it('rejects empty names and missing/nonfinite thresholds', () => {
		expect(() => alertRuleEditPatch(rule, 'celsius', '  ', 60, 'warning')).toThrow('Name is required');
		for (const threshold of [undefined, NaN, Infinity, -Infinity]) {
			expect(() => alertRuleEditPatch(rule, 'celsius', rule.name, threshold, 'warning')).toThrow('finite threshold');
		}
	});
});
