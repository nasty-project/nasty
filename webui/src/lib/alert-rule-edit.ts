import type { AlertRule, AlertSeverity, TempUnit } from './types';

export function displayedAlertThreshold(rule: AlertRule, unit: TempUnit): number {
	return rule.metric === 'disk_temperature' && unit === 'fahrenheit'
		? rule.threshold * 9 / 5 + 32 : rule.threshold;
}

/** Patch only edited fields; an unchanged Fahrenheit value must not round the
 * original Celsius threshold or overwrite a concurrent enable/disable change. */
export function alertRuleEditPatch(rule: AlertRule, unit: TempUnit, name: string, threshold: number | undefined, severity: AlertSeverity) {
	if (!name.trim()) throw new Error('Name is required.');
	if (threshold === undefined || !Number.isFinite(threshold)) throw new Error('Enter a finite threshold.');
	const patch: { id: string; name?: string; threshold?: number; severity?: AlertSeverity } = { id: rule.id };
	if (name.trim() !== rule.name) patch.name = name.trim();
	if (rule.metric !== 'backup_failure' && threshold !== displayedAlertThreshold(rule, unit)) {
		patch.threshold = rule.metric === 'disk_temperature' && unit === 'fahrenheit'
			? (threshold - 32) / 9 * 5 : threshold;
	}
	if (severity !== rule.severity) patch.severity = severity;
	return patch;
}
