<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { UpdateInfo, UpdateStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';

	let info: UpdateInfo | null = $state(null);
	let status: UpdateStatus | null = $state(null);
	let loading = $state(true);
	let checking = $state(false);
	let needsRefresh = $state(false);
	let confirmAction: 'update' | 'rollback' | null = $state(null);
	let confirmTimer: ReturnType<typeof setTimeout> | null = null;
	let pollInterval: ReturnType<typeof setInterval> | null = $state(null);
	let logEl: HTMLPreElement | undefined = $state();

	const phases = [
		{ label: 'Fetch',    marker: '==> Pulling' },
		{ label: 'Build',    marker: '==> Rebuilding' },
		{ label: 'Activate', marker: 'activating the configuration' },
		{ label: 'Done',     marker: '==> Update complete!' },
	];

	// Returns the index of the last phase whose marker appears in the log.
	// -1 = none seen yet (just started), phases.length = all done.
	const currentPhase = $derived.by(() => {
		const log = status?.log ?? '';
		let reached = -1;
		for (let i = 0; i < phases.length; i++) {
			if (log.includes(phases[i].marker)) reached = i;
		}
		return reached;
	});

	const client = getClient();

	$effect(() => {
		if (status?.log && logEl) {
			logEl.scrollTop = logEl.scrollHeight;
		}
	});

	onMount(async () => {
		await loadVersion();
		await loadStatus();
		loading = false;
	});

	onDestroy(() => {
		stopPolling();
		if (confirmTimer) clearTimeout(confirmTimer);
	});

	async function loadVersion() {
		await withToast(async () => {
			info = await client.call<UpdateInfo>('system.update.version');
		});
	}

	async function loadStatus() {
		await withToast(async () => {
			status = await client.call<UpdateStatus>('system.update.status');
			if (status?.state === 'running') {
				startPolling();
			}
		});
	}

	async function checkForUpdates() {
		checking = true;
		const result = await withToast(
			() => client.call<UpdateInfo>('system.update.check'),
			'Update check complete'
		);
		if (result !== undefined) {
			info = result;
		}
		checking = false;
	}

	function requestAction(action: 'update' | 'rollback') {
		if (confirmAction === action) {
			clearConfirm();
			if (action === 'update') doApplyUpdate();
			else doRollback();
		} else {
			confirmAction = action;
			if (confirmTimer) clearTimeout(confirmTimer);
			confirmTimer = setTimeout(clearConfirm, 4000);
		}
	}

	function clearConfirm() {
		confirmAction = null;
		if (confirmTimer) { clearTimeout(confirmTimer); confirmTimer = null; }
	}

	async function doApplyUpdate() {
		status = { state: 'running', log: '', reboot_required: false };
		const ok = await withToast(
			() => client.call('system.update.apply'),
			'Update started'
		);
		if (ok !== undefined) {
			startPolling();
		}
	}

	async function doRollback() {
		status = { state: 'running', log: '', reboot_required: false };
		const ok = await withToast(
			() => client.call('system.update.rollback'),
			'Rollback started'
		);
		if (ok !== undefined) {
			startPolling();
		}
	}

	function startPolling() {
		stopPolling();
		pollInterval = setInterval(async () => {
			try {
				status = await client.call<UpdateStatus>('system.update.status');
				if (status && status.state !== 'running') {
					stopPolling();
					await loadVersion();
					if (status.state === 'success') {
						needsRefresh = true;
					}
				}
			} catch {
				// Connection may drop during update, keep polling
			}
		}, 3000);
	}

	function stopPolling() {
		if (pollInterval) {
			clearInterval(pollInterval);
			pollInterval = null;
		}
	}
</script>


{#if needsRefresh}
	<div class="mb-4 flex items-center gap-4 rounded-lg border border-blue-800 bg-blue-950 px-4 py-3 text-sm text-blue-200">
		<span class="flex-1">Update applied. Refresh your browser to load the new WebUI.</span>
		<Button variant="secondary" size="xs" onclick={() => location.reload()}>
			Refresh Now
		</Button>
	</div>
{/if}

{#if status?.reboot_required}
	<div class="mb-4 rounded-lg border border-amber-800 bg-amber-950 px-4 py-3 text-sm text-amber-200">
		A kernel update was installed. Use the <strong>Power → Restart</strong> button in the top bar to activate it.
	</div>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<Card class="mb-6">
		<CardContent class="py-5">
			<!-- Version + status row -->
			<div class="mb-5 flex items-center gap-8">
				<div>
					<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Installed</div>
					<div class="font-mono text-xl font-semibold">{info?.current_version ?? 'unknown'}</div>
				</div>
				{#if info?.latest_version}
					<div class="text-lg text-muted-foreground/30">→</div>
					<div>
						<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Available</div>
						<div class="font-mono text-xl font-semibold {info.update_available ? 'text-blue-400' : ''}">{info.latest_version}</div>
					</div>
				{/if}
				<div class="flex items-end pb-0.5">
					{#if info?.update_available === true}
						<Badge variant="default">Update available</Badge>
					{:else if info?.update_available === false}
						<Badge variant="secondary">Up to date</Badge>
					{/if}
				</div>
			</div>

			<!-- Actions -->
			<div class="flex gap-2">
				<Button size="sm" onclick={checkForUpdates} disabled={checking || status?.state === 'running'}>
					{checking ? 'Checking...' : 'Check for Updates'}
				</Button>
				{#if info?.update_available}
					<Button
						variant={confirmAction === 'update' ? 'destructive' : 'default'}
						size="sm"
						onclick={() => requestAction('update')}
						disabled={status?.state === 'running'}
					>
						{confirmAction === 'update' ? 'Confirm?' : 'Update Now'}
					</Button>
				{/if}
				<Button
					variant={confirmAction === 'rollback' ? 'destructive' : 'secondary'}
					size="sm"
					onclick={() => requestAction('rollback')}
					disabled={status?.state === 'running'}
				>
					{confirmAction === 'rollback' ? 'Confirm?' : 'Rollback'}
				</Button>
			</div>
		</CardContent>
	</Card>

	{#if status && status.state !== 'idle'}
		<Card>
			<CardContent class="py-5">
				<!-- Phase stepper -->
				<div class="mb-5 flex items-center">
					{#each phases as phase, i}
						{@const done = currentPhase >= i}
						{@const active = status.state === 'running' && currentPhase === i - 1}
						{@const failed = status.state === 'failed' && !done}
						<div class="flex items-center gap-0">
							<!-- Circle -->
							<div class="flex flex-col items-center gap-1">
								<div class="flex h-7 w-7 items-center justify-center rounded-full border-2 text-xs font-semibold transition-all {
									done   ? 'border-blue-500 bg-blue-500 text-white' :
									active ? 'border-blue-400 bg-transparent text-blue-400 animate-pulse' :
									failed ? 'border-border bg-transparent text-muted-foreground/30' :
									         'border-border bg-transparent text-muted-foreground/30'
								}">
									{#if done}✓{:else if active}…{:else}{i + 1}{/if}
								</div>
								<span class="text-[0.65rem] font-medium {done ? 'text-blue-400' : active ? 'text-blue-400/70' : 'text-muted-foreground/40'}">{phase.label}</span>
							</div>
							<!-- Connector line -->
							{#if i < phases.length - 1}
								<div class="mb-3.5 h-px w-12 {currentPhase > i ? 'bg-blue-500' : 'bg-border'} mx-1"></div>
							{/if}
						</div>
					{/each}
					{#if status.state === 'failed'}
						<span class="ml-4 text-sm text-destructive">Failed</span>
					{/if}
				</div>

				{#if status.log}
					<pre bind:this={logEl} class="max-h-64 overflow-auto rounded bg-secondary p-3 text-xs leading-relaxed">{status.log}</pre>
				{/if}
			</CardContent>
		</Card>
	{/if}

	<p class="mt-6 text-xs text-muted-foreground">
		Updates are fetched from GitHub and applied using NixOS rebuild.
		The system will atomically switch to the new version, restarting services as needed.
		If anything goes wrong, use Rollback to return to the previous version.
	</p>
{/if}
