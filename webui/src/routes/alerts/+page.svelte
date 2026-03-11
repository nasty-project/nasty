<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { AlertRule, ActiveAlert, AlertMetric, AlertCondition, AlertSeverity } from '$lib/types';

	let rules: AlertRule[] = $state([]);
	let activeAlerts: ActiveAlert[] = $state([]);
	let loading = $state(true);
	let showCreate = $state(false);

	// Create form
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

	onMount(async () => {
		await refresh();
		loading = false;
	});

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
		if (!confirm('Delete this alert rule?')) return;
		await withToast(
			() => client.call('alert.rules.delete', { id }),
			'Alert rule deleted'
		);
		await refresh();
	}
</script>

<h1>Alerts</h1>

{#if activeAlerts.length > 0}
	<div class="active-section">
		<h2>Active Alerts ({activeAlerts.length})</h2>
		{#each activeAlerts as alert}
			<div class="active-alert" class:warning={alert.severity === 'warning'} class:critical={alert.severity === 'critical'}>
				<span class="severity-badge" class:warning={alert.severity === 'warning'} class:critical={alert.severity === 'critical'}>
					{alert.severity.toUpperCase()}
				</span>
				<span class="alert-message">{alert.message}</span>
				<span class="alert-source">{alert.source}</span>
			</div>
		{/each}
	</div>
{:else if !loading}
	<div class="no-alerts">No active alerts</div>
{/if}

<div class="toolbar">
	<h2>Alert Rules</h2>
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Rule'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>New Alert Rule</h3>
		<div class="field">
			<label for="rule-name">Name</label>
			<input id="rule-name" bind:value={newName} placeholder="My alert rule" />
		</div>
		<div class="field-row">
			<div class="field">
				<label for="rule-metric">Metric</label>
				<select id="rule-metric" bind:value={newMetric}>
					{#each Object.entries(metricLabels) as [val, label]}
						<option value={val}>{label}</option>
					{/each}
				</select>
			</div>
			<div class="field">
				<label for="rule-condition">Condition</label>
				<select id="rule-condition" bind:value={newCondition}>
					{#each Object.entries(conditionLabels) as [val, label]}
						<option value={val}>{label}</option>
					{/each}
				</select>
			</div>
		</div>
		<div class="field-row">
			<div class="field">
				<label for="rule-threshold">Threshold</label>
				<input id="rule-threshold" type="number" bind:value={newThreshold} />
			</div>
			<div class="field">
				<label for="rule-severity">Severity</label>
				<select id="rule-severity" bind:value={newSeverity}>
					<option value="warning">Warning</option>
					<option value="critical">Critical</option>
				</select>
			</div>
		</div>
		<button onclick={createRule} disabled={!newName}>Create</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else}
	<table>
		<thead>
			<tr>
				<th>Name</th>
				<th>Metric</th>
				<th>Condition</th>
				<th>Severity</th>
				<th>Status</th>
				<th>Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each rules as rule}
				<tr class:disabled-row={!rule.enabled}>
					<td><strong>{rule.name}</strong></td>
					<td>{metricLabels[rule.metric] ?? rule.metric}</td>
					<td>{conditionLabels[rule.condition] ?? rule.condition} {rule.threshold}</td>
					<td>
						<span class="severity-badge" class:warning={rule.severity === 'warning'} class:critical={rule.severity === 'critical'}>
							{rule.severity}
						</span>
					</td>
					<td>
						<span class="status-badge" class:enabled={rule.enabled} class:disabled={!rule.enabled}>
							{rule.enabled ? 'Enabled' : 'Disabled'}
						</span>
					</td>
					<td class="actions">
						<button class="secondary" onclick={() => toggleRule(rule)}>
							{rule.enabled ? 'Disable' : 'Enable'}
						</button>
						<button class="danger" onclick={() => deleteRule(rule.id)}>Delete</button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<style>
	h2 { font-size: 1rem; margin: 0; }

	/* Active alerts */
	.active-section { margin-bottom: 1.5rem; }
	.active-section h2 { margin-bottom: 0.75rem; }
	.active-alert { display: flex; align-items: center; gap: 0.75rem; padding: 0.6rem 1rem; border-radius: 6px; margin-bottom: 0.5rem; font-size: 0.875rem; }
	.active-alert.warning { background: #422006; border: 1px solid #854d0e; color: #fde68a; }
	.active-alert.critical { background: #450a0a; border: 1px solid #991b1b; color: #fca5a5; }
	.alert-message { flex: 1; }
	.alert-source { font-size: 0.75rem; opacity: 0.7; font-family: monospace; }
	.no-alerts { color: #4ade80; font-size: 0.9rem; margin: 1rem 0; padding: 0.6rem 1rem; background: #064e3b; border: 1px solid #065f46; border-radius: 6px; }

	/* Toolbar */
	.toolbar { display: flex; align-items: center; justify-content: space-between; margin: 1.5rem 0 1rem; }

	/* Form */
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 500px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input, .field select { width: 100%; box-sizing: border-box; }
	.field-row { display: flex; gap: 1rem; }
	.field-row .field { flex: 1; }

	/* Badges */
	.severity-badge { padding: 0.15rem 0.4rem; border-radius: 3px; font-size: 0.7rem; font-weight: 600; text-transform: uppercase; }
	.severity-badge.warning { background: #422006; color: #fde68a; }
	.severity-badge.critical { background: #450a0a; color: #fca5a5; }
	.status-badge { padding: 0.2rem 0.5rem; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
	.status-badge.enabled { background: #064e3b; color: #4ade80; }
	.status-badge.disabled { background: #374151; color: #9ca3af; }

	.disabled-row { opacity: 0.5; }
	.actions { display: flex; gap: 0.5rem; }
</style>
