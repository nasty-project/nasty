<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { AlertRule, ActiveAlert, AlertMetric, AlertCondition, AlertSeverity } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';

	let rules: AlertRule[] = $state([]);
	let activeAlerts: ActiveAlert[] = $state([]);
	let loading = $state(true);
	let showCreate = $state(false);

	let newName = $state('');
	let newMetric = $state<AlertMetric>('pool_usage_percent');
	let newCondition = $state<AlertCondition>('above');
	let newThreshold = $state(80);
	let newSeverity = $state<AlertSeverity>('warning');

	const client = getClient();

	const metricLabels: Record<AlertMetric, string> = {
		pool_usage_percent: 'Pool Usage (%)',
		cpu_load_percent: 'CPU Load (%)',
		memory_usage_percent: 'Memory Usage (%)',
		disk_temperature: 'Disk Temperature (°C)',
		smart_health: 'SMART Health Failure',
		swap_usage_percent: 'Swap Usage (%)',
	};

	const conditionLabels: Record<AlertCondition, string> = {
		above: 'Above',
		below: 'Below',
		equals: 'Equals',
	};

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
		if (!newName) return;
		const ok = await withToast(
			() => client.call('alert.rules.create', {
				id: '',
				name: newName,
				enabled: true,
				metric: newMetric,
				condition: newCondition,
				threshold: newThreshold,
				severity: newSeverity,
			}),
			'Alert rule created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
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
		if (!await confirm('Delete Alert Rule', 'Delete this alert rule?')) return;
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

<div class="mb-4 flex items-center justify-between">
	<h2 class="text-base font-semibold">Alert Rules</h2>
	<Button size="sm" onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Rule'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Alert Rule</h3>
			<div class="mb-4">
				<Label for="rule-name">Name</Label>
				<Input id="rule-name" bind:value={newName} placeholder="My alert rule" class="mt-1" />
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
			<Button onclick={createRule} disabled={!newName}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Name</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Metric</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Condition</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Severity</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each rules as rule}
				<tr class="border-b border-border {!rule.enabled ? 'opacity-50' : ''}">
					<td class="p-3"><strong>{rule.name}</strong></td>
					<td class="p-3">{metricLabels[rule.metric] ?? rule.metric}</td>
					<td class="p-3">{conditionLabels[rule.condition] ?? rule.condition} {rule.threshold}</td>
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
