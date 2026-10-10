import { describe, expect, it } from 'vitest';
import { render } from 'svelte/server';
import AlertRuleEditor from './AlertRuleEditor.svelte';
import type { AlertRule } from '$lib/types';

const rule: AlertRule = { id: 'disk-temp-warn', name: 'Disk temperature warning', enabled: true, metric: 'disk_temperature', condition: 'above', threshold: 50, severity: 'warning' };
describe('alert rule editor', () => {
	it('renders existing values, Fahrenheit units, and save/cancel controls', () => {
		const { body } = render(AlertRuleEditor, { props: { rule, unit: 'fahrenheit', metricLabel: 'Disk Temperature (°F)', conditionLabel: 'Above', onSave: async () => true, onCancel: () => {} } });
		expect(body).toContain('Edit Alert Rule');
		expect(body).toContain('value="122"');
		expect(body).toContain('Threshold (°F)');
		expect(body).toContain('Save changes');
		expect(body).toContain('Cancel');
		expect(body).toContain('all monitored disks');
	});
	it('does not offer a threshold for fixed backup-failure rules', () => {
		const { body } = render(AlertRuleEditor, { props: { rule: { ...rule, metric: 'backup_failure' }, unit: 'celsius', metricLabel: 'Backup Failure', conditionLabel: 'Equals', onSave: async () => true, onCancel: () => {} } });
		expect(body).not.toContain('edit-rule-threshold');
	});
});
