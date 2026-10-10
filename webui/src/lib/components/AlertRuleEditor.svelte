<script lang="ts">
	import { untrack } from 'svelte';
	import type { AlertRule, AlertSeverity, TempUnit } from '$lib/types';
	import { alertRuleEditPatch, displayedAlertThreshold } from '$lib/alert-rule-edit';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	let { rule, unit, metricLabel, conditionLabel, onSave, onCancel }: {
		rule: AlertRule; unit: TempUnit; metricLabel: string; conditionLabel: string;
		onSave: (patch: ReturnType<typeof alertRuleEditPatch>) => Promise<boolean>;
		onCancel: () => void;
	} = $props();
	let name = $state(untrack(() => rule.name));
	let threshold = $state<number | undefined>(untrack(() => displayedAlertThreshold(rule, unit)));
	let severity = $state<AlertSeverity>(untrack(() => rule.severity));
	let saving = $state(false);
	let error = $state('');
	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (saving) return;
		error = '';
		try {
			const patch = alertRuleEditPatch(rule, unit, name, threshold, severity);
			saving = true;
			if (await onSave(patch)) onCancel();
		} catch (e) { error = e instanceof Error ? e.message : 'Could not save rule.'; }
		finally { saving = false; }
	}
</script>

<Card class="mb-6 max-w-lg">
	<CardContent class="pt-6">
		<form onsubmit={save}>
			<h3 class="mb-4 text-lg font-semibold">Edit Alert Rule</h3>
			<p class="mb-4 text-sm text-muted-foreground">{metricLabel} · {conditionLabel}. Metric and condition remain unchanged.</p>
			<div class="mb-4">
				<Label for="edit-rule-name">Name</Label>
				<Input id="edit-rule-name" bind:value={name} required disabled={saving} class="mt-1" />
			</div>
			{#if rule.metric !== 'backup_failure'}
				<div class="mb-4">
					<Label for="edit-rule-threshold">Threshold{rule.metric === 'disk_temperature' ? ` (${unit === 'fahrenheit' ? '°F' : '°C'})` : ''}</Label>
					<Input id="edit-rule-threshold" type="number" step="any" bind:value={threshold} required disabled={saving} class="mt-1" />
					{#if rule.metric === 'disk_temperature'}<p class="mt-1 text-xs text-muted-foreground">Stored in Celsius. This threshold applies to all monitored disks, not just one device.</p>{/if}
				</div>
			{/if}
			<div class="mb-4">
				<Label for="edit-rule-severity">Severity</Label>
				<select id="edit-rule-severity" bind:value={severity} disabled={saving} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="warning">Warning</option><option value="critical">Critical</option>
				</select>
			</div>
			{#if error}<p role="alert" class="mb-3 text-sm text-red-400">{error}</p>{/if}
			<div class="flex gap-2">
				<Button type="submit" disabled={saving}>{saving ? 'Saving...' : 'Save changes'}</Button>
				<Button type="button" variant="secondary" disabled={saving} onclick={onCancel}>Cancel</Button>
			</div>
		</form>
	</CardContent>
</Card>
