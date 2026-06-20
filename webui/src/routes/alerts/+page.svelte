<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import { tempUnit, cToF, tempUnitLabel } from '$lib/temperature.svelte';
	import type { AlertRule, ActiveAlert, AlertMetric, AlertCondition, AlertSeverity } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';

	let rules: AlertRule[] = $state([]);
	let activeAlerts: ActiveAlert[] = $state([]);
	let loading = $state(true);
	let showCreate = $state(false);

	let newName = $state('');
	let createTried = $state(false);
	let newMetric = $state<AlertMetric>('fs_usage_percent');
	let newCondition = $state<AlertCondition>('above');
	let newThreshold = $state(80);
	let newSeverity = $state<AlertSeverity>('warning');

	const client = getClient();

	// Disk temperature label tracks the user's display-unit preference;
	// thresholds remain Celsius internally (so an existing rule keeps its
	// alerting semantics if the user toggles units), but we convert at
	// the input/display boundary in `createRule` and the rules table.
	const metricLabels: Record<AlertMetric, string> = $derived({
		fs_usage_percent: 'Filesystem Usage (%)',
		cpu_load_percent: 'CPU Load (%)',
		memory_usage_percent: 'Memory Usage (%)',
		disk_temperature: `Disk Temperature (${tempUnitLabel(tempUnit.current)})`,
		smart_health: 'SMART Health Failure',
		swap_usage_percent: 'Swap Usage (%)',
		bcachefs_degraded: 'bcachefs Degraded Mode',
		bcachefs_device_error: 'bcachefs Device Errors',
		bcachefs_device_state: 'bcachefs Device State',
		bcachefs_io_errors: 'bcachefs IO Errors',
		bcachefs_scrub_errors: 'bcachefs Scrub Corruption',
		bcachefs_reconcile_stalled: 'bcachefs Reconcile Stalled',
		root_disk_free_gb: 'Root Partition Free (GB)',
		boot_disk_free_mb: '/boot (ESP) Free (MB)',
		kernel_errors: 'Kernel Errors',
	});

	const conditionLabels: Record<AlertCondition, string> = {
		above: 'Above',
		below: 'Below',
		equals: 'Equals',
	};

	// ── Column sorting ──────────────────────────────────────────────────
	type RuleSortKey = 'name' | 'metric' | 'severity' | 'status';
	let ruleSortKey = $state<RuleSortKey>('name');
	let ruleSortDir = $state<'asc' | 'desc'>('asc');
	function toggleRuleSort(key: RuleSortKey) {
		if (ruleSortKey === key) ruleSortDir = ruleSortDir === 'asc' ? 'desc' : 'asc';
		else { ruleSortKey = key; ruleSortDir = 'asc'; }
	}
	const sortedRules = $derived.by(() => {
		const sign = ruleSortDir === 'asc' ? 1 : -1;
		// critical sorts above warning when descending.
		const sevRank = (s: AlertSeverity) => (s === 'critical' ? 2 : 1);
		return [...rules].sort((a, b) => {
			let cmp = 0;
			if (ruleSortKey === 'name') cmp = a.name.localeCompare(b.name, undefined, { numeric: true });
			else if (ruleSortKey === 'metric')
				cmp = (metricLabels[a.metric] ?? a.metric).localeCompare(metricLabels[b.metric] ?? b.metric);
			else if (ruleSortKey === 'severity') cmp = sevRank(a.severity) - sevRank(b.severity);
			else cmp = Number(a.enabled) - Number(b.enabled);
			if (cmp === 0) cmp = a.name.localeCompare(b.name, undefined, { numeric: true });
			return sign * cmp;
		});
	});

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'alert') refresh();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		await withToast(async () => {
			[rules, activeAlerts] = await Promise.all([
				client.call<AlertRule[]>('alert.rules.list'),
				client.call<ActiveAlert[]>('system.alerts'),
			]);
		});
	}

	async function createRule() {
		if (!newName) { createTried = true; return; }
		createTried = false;
		// disk_temperature thresholds are stored in Celsius. If the user is
		// viewing Fahrenheit, the value typed in the input is in °F — convert
		// before sending so the alert evaluator sees the canonical unit.
		const stored =
			newMetric === 'disk_temperature' && tempUnit.current === 'fahrenheit'
				? Math.round(((newThreshold - 32) * 5) / 9)
				: newThreshold;
		const ok = await withToast(
			() => client.call('alert.rules.create', {
				id: '',
				name: newName,
				enabled: true,
				metric: newMetric,
				condition: newCondition,
				threshold: stored,
				severity: newSeverity,
			}),
			'Alert rule created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			createTried = false;
			newThreshold = 80;
			await refresh();
		}
	}

	async function toggleRule(rule: AlertRule) {
		await withToast(
			() => client.call('alert.rules.update', { id: rule.id, enabled: !rule.enabled }),
			`Rule ${rule.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}

	async function deleteRule(id: string) {
		if (!await confirm('Delete this alert rule?')) return;
		await withToast(
			() => client.call('alert.rules.delete', { id }),
			'Alert rule deleted'
		);
		await refresh();
	}
</script>


{#if activeAlerts.length > 0}
	<div class="mb-6">
		<h2 class="mb-3 text-base font-semibold">Active Alerts ({activeAlerts.length})</h2>
		{#each activeAlerts as alert}
			<div class="mb-2 flex items-center gap-3 rounded-lg border px-4 py-2.5 text-sm {
				alert.severity === 'critical' ? 'border-red-800 bg-red-950 text-red-200' : 'border-amber-800 bg-amber-950 text-amber-200'
			}">
				<span class="rounded px-1.5 py-0.5 text-[0.7rem] font-semibold uppercase {
					alert.severity === 'critical' ? 'bg-red-900 text-red-200' : 'bg-amber-900 text-amber-200'
				}">{alert.severity}</span>
				<span class="flex-1">{alert.message}</span>
				<span class="font-mono text-xs opacity-70">{alert.source}</span>
			</div>
		{/each}
	</div>
{:else if !loading}
	<div class="mb-6 rounded-lg border border-green-900 bg-green-950 px-4 py-2.5 text-sm text-green-400">
		No active alerts
	</div>
{/if}

<div class="mb-4 flex items-center gap-3">
	<Button size="sm" onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Rule'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Alert Rule</h3>
			<div class="mb-4">
				<Label for="rule-name">Name {#if !newName && createTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
				<Input id="rule-name" bind:value={newName} placeholder="My alert rule" class="mt-1 {requiredFieldCls(!newName, createTried)}" />
			</div>
			<div class="mb-4 flex gap-4">
				<div class="flex-1">
					<Label for="rule-metric">Metric</Label>
					<select id="rule-metric" bind:value={newMetric} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						{#each Object.entries(metricLabels) as [val, label]}
							<option value={val}>{label}</option>
						{/each}
					</select>
				</div>
				<div class="flex-1">
					<Label for="rule-condition">Condition</Label>
					<select id="rule-condition" bind:value={newCondition} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						{#each Object.entries(conditionLabels) as [val, label]}
							<option value={val}>{label}</option>
						{/each}
					</select>
				</div>
			</div>
			<div class="mb-4 flex gap-4">
				<div class="flex-1">
					<Label for="rule-threshold">Threshold</Label>
					<Input id="rule-threshold" type="number" bind:value={newThreshold} class="mt-1" />
				</div>
				<div class="flex-1">
					<Label for="rule-severity">Severity</Label>
					<select id="rule-severity" bind:value={newSeverity} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						<option value="warning">Warning</option>
						<option value="critical">Critical</option>
					</select>
				</div>
			</div>
			<Button onclick={createRule}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={ruleSortKey === 'name'} dir={ruleSortDir} onclick={() => toggleRuleSort('name')} />
				<SortTh label="Metric" active={ruleSortKey === 'metric'} dir={ruleSortDir} onclick={() => toggleRuleSort('metric')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Condition</th>
				<SortTh label="Severity" active={ruleSortKey === 'severity'} dir={ruleSortDir} onclick={() => toggleRuleSort('severity')} />
				<SortTh label="Status" active={ruleSortKey === 'status'} dir={ruleSortDir} onclick={() => toggleRuleSort('status')} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sortedRules as rule}
				<tr class="border-b border-border {!rule.enabled ? 'opacity-50' : ''}">
					<td class="p-3"><strong>{rule.name}</strong></td>
					<td class="p-3">{metricLabels[rule.metric] ?? rule.metric}</td>
					<td class="p-3">
						{conditionLabels[rule.condition] ?? rule.condition}
						{rule.metric === 'disk_temperature' && tempUnit.current === 'fahrenheit'
							? Math.round(cToF(rule.threshold))
							: rule.threshold}
					</td>
					<td class="p-3">
						<span class="rounded px-1.5 py-0.5 text-[0.7rem] font-semibold uppercase {
							rule.severity === 'critical' ? 'bg-red-950 text-red-200' : 'bg-amber-950 text-amber-200'
						}">{rule.severity}</span>
					</td>
					<td class="p-3">
						<Badge variant={rule.enabled ? 'default' : 'secondary'}>
							{rule.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" size="xs" onclick={() => toggleRule(rule)}>
								{rule.enabled ? 'Disable' : 'Enable'}
							</Button>
							<Button variant="destructive" size="xs" onclick={() => deleteRule(rule.id)}>Delete</Button>
						</div>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}
