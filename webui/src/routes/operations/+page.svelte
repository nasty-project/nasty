<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { Operation } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { RefreshCw } from '@lucide/svelte';

	let operations: Operation[] = $state([]);
	let loading = $state(true);
	let busy = $state<string | null>(null);
	let pollInterval: ReturnType<typeof setInterval> | null = null;

	const client = getClient();

	// Stable key per operation so the busy-state and {#each} track correctly.
	function opKey(op: Operation): string {
		return `${op.kind}:${op.fs}:${op.target ?? ''}`;
	}

	async function load() {
		try {
			operations = await client.call<Operation[]>('system.operations.list');
		} catch {
			operations = [];
		}
		loading = false;
	}

	onMount(() => {
		load();
		// Poll fairly often — scrub progress and pause/resume should feel live.
		pollInterval = setInterval(load, 4000);
	});
	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});

	function kindLabel(kind: string): string {
		return (
			{ scrub: 'Scrub', evacuate: 'Evacuate', reconcile: 'Reconcile', copygc: 'Copygc' }[kind] ??
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
					: `evacuation of ${op.target} on ${op.fs}`;
			const body =
				op.kind === 'scrub'
					? 'The scrub stops where it is; already-checked data stays verified. You can start a fresh scrub later.'
					: 'The device stops draining and returns to read-write. Data already moved off stays moved; the device keeps what is left.';
			if (!(await confirm(`Cancel ${what}?`, body))) return;
		}

		busy = key;
		const method =
			op.kind === 'scrub'
				? op.control === 'start'
					? 'fs.scrub.start'
					: 'fs.scrub.cancel'
				: op.kind === 'evacuate'
					? 'fs.device.evacuate.cancel'
					: op.kind === 'reconcile'
						? op.control === 'resume'
							? 'fs.reconcile.enable'
							: 'fs.reconcile.disable'
						: op.control === 'resume'
							? 'fs.copygc.enable'
							: 'fs.copygc.disable';
		const params =
			op.kind === 'evacuate' ? { filesystem: op.fs, device: op.target } : { name: op.fs };

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
</script>

<div class="mx-auto max-w-4xl p-6">
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
					<CardContent class="flex items-center gap-4 py-3">
						<div class="w-24 shrink-0">
							<span class="text-sm font-semibold">{kindLabel(op.kind)}</span>
						</div>
						<div class="min-w-0 flex-1">
							<div class="truncate text-sm">{op.detail}</div>
							<div class="text-xs {stateClass(op.state)}">{op.state}</div>
							{#if op.progress_percent != null}
								<div class="mt-1 h-1.5 w-full overflow-hidden rounded bg-muted">
									<div
										class="h-full bg-amber-500 transition-all"
										style="width: {Math.max(0, Math.min(100, op.progress_percent))}%"
									></div>
								</div>
							{/if}
						</div>
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
					</CardContent>
				</Card>
			{/each}
		</div>
	{/if}
</div>
