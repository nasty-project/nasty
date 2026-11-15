<script lang="ts">
	import { onMount, onDestroy, tick } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { AuthMe, Operation, ScrubScheduleStatus } from '$lib/types';
	import { operationDetail } from '$lib/operations';
	import {
		formatUtcTimestamp,
		scrubSchedulePreset,
		scrubScheduleSummary,
		scrubScheduleValue,
		type ScrubSchedulePreset,
	} from '$lib/scrubSchedule';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { RefreshCw } from '@lucide/svelte';

	let operations: Operation[] = $state([]);
	let loading = $state(true);
	let busy = $state<string | null>(null);
	let isAdmin = $state(false);
	let editingSchedule = $state<string | null>(null);
	let schedulePreset = $state<ScrubSchedulePreset>('manual');
	let customSchedule = $state('');
	let savingSchedule = $state(false);
	let scheduleSelect = $state<HTMLSelectElement | null>(null);
	let scheduleTrigger: HTMLElement | null = null;
	let pollInterval: ReturnType<typeof setInterval> | null = null;
	let hasVisibleScheduleEditor = $derived(
		editingSchedule !== null && operations.some((op) => opKey(op) === editingSchedule)
	);
	let customScheduleInvalid = $derived(
		schedulePreset === 'custom' && scrubScheduleValue(schedulePreset, customSchedule) === undefined
	);

	const client = getClient();

	// Stable key per operation so the busy-state and {#each} track correctly.
	function opKey(op: Operation): string {
		return `${op.kind}:${op.fs}:${op.target ?? ''}`;
	}

	function scheduleEditorId(op: Operation): string {
		return `scrub-schedule-editor-${encodeURIComponent(opKey(op))}`;
	}

	async function load() {
		try {
			const next = await client.call<Operation[]>('system.operations.list');
			const edited = editingSchedule === null
				? undefined
				: operations.find((op) => opKey(op) === editingSchedule);
			operations = edited && !next.some((op) => opKey(op) === editingSchedule)
				? [...next, edited]
				: next;
		} catch { /* Keep the last good rows during transient poll failures. */ }
		loading = false;
	}

	onMount(() => {
		load();
		client.call<AuthMe>('auth.me')
			.then((identity) => { isAdmin = identity.role === 'admin'; })
			.catch(() => { isAdmin = false; });
		// Poll fairly often — scrub progress and pause/resume should feel live.
		pollInterval = setInterval(load, 4000);
	});
	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});

	function kindLabel(kind: string): string {
		return (
			{ scrub: 'Scrub', evacuate: 'Evacuate', remove: 'Remove', reconcile: 'Reconcile', copygc: 'Copygc' }[kind] ??
			kind
		);
	}

	function stateClass(state: string): string {
		return (
			{
				running: 'text-amber-400',
				active: 'text-amber-400',
				idle: 'text-emerald-400',
				paused: 'text-muted-foreground',
			}[state] ?? 'text-muted-foreground'
		);
	}

	async function act(op: Operation) {
		const key = opKey(op);
		if (op.control === 'cancel') {
			const what =
				op.kind === 'scrub'
					? `the scrub on ${op.fs}`
					: `${op.kind === 'remove' ? 'removal' : 'evacuation'} of ${op.target} on ${op.fs}`;
			const body =
				op.kind === 'scrub'
					? 'The scrub stops where it is; already-checked data stays verified. You can start a fresh scrub later.'
					: 'The device stops draining, stays in the pool and returns to read-write. Data already moved off stays moved; the device keeps what is left.';
			if (!(await confirm(`Cancel ${what}?`, body))) return;
		}

		busy = key;
		const method =
			op.kind === 'scrub'
				? op.control === 'start'
					? 'fs.scrub.start'
					: 'fs.scrub.cancel'
				: op.kind === 'evacuate' || op.kind === 'remove'
					? 'fs.device.evacuate.cancel'
					: op.kind === 'reconcile'
						? op.control === 'resume'
							? 'fs.reconcile.enable'
							: 'fs.reconcile.disable'
						: op.control === 'resume'
							? 'fs.copygc.enable'
							: 'fs.copygc.disable';
		const params =
			op.kind === 'evacuate' || op.kind === 'remove'
				? { filesystem: op.fs, device: op.target }
				: op.kind === 'scrub' && op.control === 'cancel'
					? { name: op.fs, run_id: op.run_id }
					: { name: op.fs };

		const verb =
			op.control === 'start'
				? 'started'
				: op.control === 'cancel'
					? 'cancelled'
					: op.control === 'resume'
						? 'resumed'
						: 'paused';
		const ok = await withToast(
			() => client.call(method, params),
			`${kindLabel(op.kind)} on ${op.fs} ${verb}`
		);
		if (ok !== undefined) await load();
		busy = null;
	}

	function actionLabel(op: Operation): string {
		return (
			{ start: 'Start', cancel: 'Cancel', pause: 'Pause', resume: 'Resume' }[op.control] ?? ''
		);
	}

	async function openScheduleEditor(op: Operation, trigger: HTMLElement) {
		const schedule = op.schedule?.trim() ?? '';
		scheduleTrigger = trigger;
		editingSchedule = opKey(op);
		schedulePreset = scrubSchedulePreset(schedule);
		customSchedule = schedule;
		await tick();
		scheduleSelect?.focus();
	}

	async function closeScheduleEditor() {
		const trigger = scheduleTrigger;
		editingSchedule = null;
		schedulePreset = 'manual';
		customSchedule = '';
		scheduleSelect = null;
		await tick();
		trigger?.focus();
		scheduleTrigger = null;
	}

	async function saveSchedule(op: Operation) {
		const schedule = scrubScheduleValue(schedulePreset, customSchedule);
		if (schedule === undefined) return;
		savingSchedule = true;
		const updated = await withToast(
			() => client.call<ScrubScheduleStatus>('fs.scrub.schedule.update', {
				name: op.fs,
				schedule,
			}),
			`Scrub schedule on ${op.fs} updated`
		);
		savingSchedule = false;
		if (updated === undefined) return;
		await closeScheduleEditor();
		await load();
	}
</script>

<div class="mx-auto max-w-4xl p-4 sm:p-6">
	<p class="mb-6 flex items-center gap-2 text-muted-foreground">
		<span>Live array operations across your pools — start or cancel a scrub, pause or resume
		background reconcile and copy-GC, and watch evacuations in progress.</span>
		{#if loading}
			<RefreshCw class="h-4 w-4 animate-spin text-muted-foreground" />
		{/if}
	</p>

	{#if !loading && operations.length === 0}
		<Card>
			<CardContent class="py-10 text-center text-muted-foreground">
				Nothing running. Scrubs and evacuations appear here while in progress; reconcile and
				copy-GC appear when a pool exposes them.
			</CardContent>
		</Card>
	{:else}
		<div class="space-y-2">
			{#each operations as op (opKey(op))}
				<Card>
					<CardContent class="py-3">
						<div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:gap-4">
							<div class="w-full shrink-0 sm:w-28">
								<div class="text-sm font-semibold">{kindLabel(op.kind)}</div>
								{#if op.fs}
									<div class="truncate font-mono text-xs text-muted-foreground" title={op.fs}>
										{op.fs}
									</div>
								{/if}
							</div>
							<div class="min-w-0 flex-1">
								<div
									class="truncate text-sm"
									title={op.kind === 'scrub' && op.state !== 'running' && op.last_run_at != null
										? new Date(op.last_run_at * 1000).toLocaleString()
										: undefined}
								>{operationDetail(op)}</div>
								<div class="text-xs {stateClass(op.state)}">{op.state}</div>
								{#if op.kind === 'scrub'}
									{#if op.schedule_error}
										<div class="mt-1 break-words text-xs text-red-400" role="status">
											{op.schedule ? 'Invalid schedule' : 'Schedule unavailable'}: {op.schedule_error}
										</div>
									{:else}
										<div class="mt-1 flex flex-wrap gap-x-2 text-xs text-muted-foreground">
											<span class={scrubSchedulePreset(op.schedule) === 'custom' ? 'min-w-0 max-w-full break-all' : ''}>
												{scrubScheduleSummary(op.schedule)}
											</span>
											{#if op.next_run_at}
												<span>Next cron: {formatUtcTimestamp(op.next_run_at)}</span>
											{/if}
										</div>
									{/if}
								{/if}
								{#if op.progress_percent != null}
									<div class="mt-1 h-1.5 w-full overflow-hidden rounded bg-muted">
										<div
											class="h-full bg-amber-500 transition-all"
											style="width: {Math.max(0, Math.min(100, op.progress_percent))}%"
										></div>
									</div>
								{/if}
							</div>
							<div class="flex shrink-0 items-center gap-2 self-end sm:self-auto">
								{#if isAdmin && op.kind === 'scrub'}
									<Button
										variant="ghost-border"
										size="xs"
										disabled={hasVisibleScheduleEditor || savingSchedule}
										aria-label={`Schedule scrub for ${op.fs}`}
										aria-expanded={editingSchedule === opKey(op)}
										aria-controls={editingSchedule === opKey(op) ? scheduleEditorId(op) : undefined}
										onclick={(event) => openScheduleEditor(op, event.currentTarget)}
									>
										Schedule
									</Button>
								{/if}
								{#if op.control !== 'none'}
									<Button
										variant={op.control === 'cancel' ? 'destructive' : op.control === 'start' ? 'default' : 'outline'}
										size="sm"
										disabled={busy === opKey(op)}
										onclick={() => act(op)}
									>
										{actionLabel(op)}
									</Button>
								{/if}
							</div>
						</div>

						{#if editingSchedule === opKey(op)}
							<div id={scheduleEditorId(op)} class="mt-3 border-t border-border pt-3">
								<div class="grid gap-3 sm:grid-cols-[minmax(0,14rem)_minmax(0,1fr)]">
									<label class="text-xs font-medium">
										Schedule
										<select
											bind:this={scheduleSelect}
											bind:value={schedulePreset}
											disabled={savingSchedule}
											class="mt-1 h-8 w-full rounded-md border border-input bg-background px-2 text-sm"
										>
											<option value="manual">Manual only</option>
											<option value="daily">Daily (02:00 UTC)</option>
											<option value="weekly">Weekly (Sun 02:00 UTC)</option>
											<option value="monthly">Monthly (day 1, 02:00 UTC)</option>
											<option value="custom">Custom</option>
										</select>
									</label>
									{#if schedulePreset === 'custom'}
										<label class="text-xs font-medium">
											Cron expression
											<input
												type="text"
												bind:value={customSchedule}
												disabled={savingSchedule}
												aria-invalid={customScheduleInvalid}
												aria-describedby={customScheduleInvalid
													? `${scheduleEditorId(op)}-custom-help ${scheduleEditorId(op)}-custom-error`
													: `${scheduleEditorId(op)}-custom-help`}
												autocomplete="off"
												spellcheck="false"
												placeholder="0 2 * * *"
												class="mt-1 h-8 w-full rounded-md border border-input bg-background px-2 font-mono text-sm"
											/>
											<span id={`${scheduleEditorId(op)}-custom-help`} class="mt-1 block font-normal text-muted-foreground">Five-field POSIX cron, evaluated in UTC.</span>
											{#if customScheduleInvalid}
												<span id={`${scheduleEditorId(op)}-custom-error`} class="mt-1 block font-normal text-red-400" role="alert">Enter a cron expression or choose Manual only.</span>
											{/if}
										</label>
									{/if}
								</div>
								<div class="mt-3 flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
									<p class="text-xs text-muted-foreground">Changes affect future runs only; a running scrub continues.</p>
									<div class="flex justify-end gap-2">
										<Button variant="secondary" size="xs" disabled={savingSchedule} onclick={closeScheduleEditor}>Cancel</Button>
										<Button size="xs" disabled={savingSchedule || customScheduleInvalid} onclick={() => saveSchedule(op)}>{savingSchedule ? 'Saving...' : 'Save'}</Button>
									</div>
								</div>
							</div>
						{/if}
					</CardContent>
				</Card>
			{/each}
		</div>
	{/if}
</div>
