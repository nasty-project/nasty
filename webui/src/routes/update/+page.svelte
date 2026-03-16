<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { UpdateInfo, UpdateStatus, BcachefsToolsInfo } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { refreshState } from '$lib/refresh.svelte';

	let activeTab: 'system' | 'bcachefs' = $state('system');

	let info: UpdateInfo | null = $state(null);
	let status: UpdateStatus | null = $state(null);
	let loading = $state(true);
	let checking = $state(false);
	let confirmAction: 'update' | 'rollback' | null = $state(null);
	let confirmTimer: ReturnType<typeof setTimeout> | null = null;
	let pollInterval: ReturnType<typeof setInterval> | null = null;
	let logEl: HTMLPreElement | undefined = $state();
	let logCollapsed = $state(false);

	// bcachefs-tools switching state
	let bcachefsInfo: BcachefsToolsInfo | null = $state(null);
	let bcachefsStatus: UpdateStatus | null = $state(null);
	let bcachefsRef = $state('');
	let bcachefsSwitching = $state(false);
	let bcachefsLogEl: HTMLPreElement | undefined = $state();
	let bcachefsPollInterval: ReturnType<typeof setInterval> | null = null;
	let bcachefsLogCollapsed = $state(false);

	const phases = [
		{ label: 'Fetch',    marker: '==> Pulling' },
		{ label: 'Build',    marker: '==> Rebuilding' },
		{ label: 'Activate', marker: 'activating the configuration' },
		{ label: 'Done',     marker: '==> Update complete!' },
	];

	const currentPhase = $derived.by(() => {
		const log = status?.log ?? '';
		let reached = -1;
		for (let i = 0; i < phases.length; i++) {
			if (log.includes(phases[i].marker)) reached = i;
		}
		return reached;
	});

	const bcachefsPhases = [
		{ label: 'Fetch',    marker: '==> Switching' },
		{ label: 'Build',    marker: '==> Rebuilding' },
		{ label: 'Activate', marker: 'activating the configuration' },
		{ label: 'Done',     marker: '==> bcachefs-tools switch complete!' },
	];

	const bcachefsCurrentPhase = $derived.by(() => {
		const log = bcachefsStatus?.log ?? '';
		let reached = -1;
		for (let i = 0; i < bcachefsPhases.length; i++) {
			if (log.includes(bcachefsPhases[i].marker)) reached = i;
		}
		return reached;
	});

	const bcachefsWarnVisible = $derived(
		bcachefsRef.trim() !== '' && bcachefsRef.trim() !== ((bcachefsInfo as BcachefsToolsInfo | null)?.default_ref ?? '')
	);

	// True when the entered ref looks like a branch name rather than a tag (v*) or commit SHA ([0-9a-f]{7,40})
	const bcachefsRefIsBranch = $derived.by(() => {
		const r = bcachefsRef.trim();
		if (!r) return false;
		if (/^v\d/.test(r)) return false;           // tag: v1.37.0
		if (/^[0-9a-f]{7,40}$/.test(r)) return false; // commit SHA
		return true;
	});

	const client = getClient();

	$effect(() => {
		if (status?.log && logEl) {
			logEl.scrollTop = logEl.scrollHeight;
		}
	});

	$effect(() => {
		if (bcachefsStatus?.log && bcachefsLogEl) {
			bcachefsLogEl.scrollTop = bcachefsLogEl.scrollHeight;
		}
	});

	$effect(() => {
		if (bcachefsStatus?.state === 'running') {
			startBcachefsPolling();
		}
	});

	onMount(async () => {
		await loadVersion();
		await loadStatus();
		await loadBcachefsInfo();
		await loadBcachefsStatus();
		loading = false;
	});

	onDestroy(() => {
		stopPolling();
		stopBcachefsPolling();
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
		logCollapsed = false;
		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const ok = await withToast(
			() => client.call('system.update.apply'),
			'Update started'
		);
		if (ok !== undefined) {
			startPolling();
		}
	}

	async function doRollback() {
		logCollapsed = false;
		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
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
				if (status && (status.state === 'success' || status.state === 'failed')) {
					stopPolling();
					await loadVersion();
					if (status.state === 'success') {
						if (status.webui_changed) refreshState.set();
						setTimeout(() => { logCollapsed = true; }, 5000);
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

	async function loadBcachefsInfo() {
		await withToast(async () => {
			bcachefsInfo = await client.call<BcachefsToolsInfo>('bcachefs.tools.info');
		});
	}

	async function loadBcachefsStatus() {
		await withToast(async () => {
			bcachefsStatus = await client.call<UpdateStatus>('bcachefs.tools.status');
			if (bcachefsStatus?.state === 'running') {
				startBcachefsPolling();
			}
		});
	}

	async function doBcachefsSwitch() {
		const ref = bcachefsRef.trim();
		if (!ref) return;
		bcachefsSwitching = true;
		bcachefsLogCollapsed = false;
		bcachefsStatus = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const result = await withToast(
			() => client.call('bcachefs.tools.switch', { git_ref: ref }),
			'bcachefs-tools switch started'
		);
		if (result !== undefined) {
			startBcachefsPolling();
		}
		bcachefsSwitching = false;
	}

	function startBcachefsPolling() {
		stopBcachefsPolling();
		bcachefsPollInterval = setInterval(async () => {
			try {
				bcachefsStatus = await client.call<UpdateStatus>('bcachefs.tools.status');
				// Only stop on terminal states. 'idle' can occur transiently when systemd
				// restarts during switch-to-configuration and the unit state is briefly lost.
				if (bcachefsStatus && (bcachefsStatus.state === 'success' || bcachefsStatus.state === 'failed')) {
					stopBcachefsPolling();
					await loadBcachefsInfo();
					if (bcachefsStatus.state === 'success') {
						// No page reload needed — only bcachefs-tools changed, not the webui JS.
						bcachefsRef = '';
						setTimeout(() => { bcachefsLogCollapsed = true; }, 5000);
					}
				}
			} catch {
				// Connection may drop during rebuild, keep polling
			}
		}, 3000);
	}

	function stopBcachefsPolling() {
		if (bcachefsPollInterval) {
			clearInterval(bcachefsPollInterval);
			bcachefsPollInterval = null;
		}
	}

	/** Break systemd's single-line "Consumed …" summary into one stat per line. */
	function formatLog(log: string): string {
		return log.replace(
			/(^.+: Consumed .+)$/m,
			(line) => line.replace(/, /g, ',\n  ')
		);
	}
</script>


<!-- Global banners — shown regardless of active tab -->

{#if status?.reboot_required}
	<div class="mb-4 rounded-lg border border-amber-800 bg-amber-950 px-4 py-3 text-sm text-amber-200">
		A kernel update was installed. Use the <strong>Power → Restart</strong> button in the top bar to activate it.
	</div>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<!-- Tab bar -->
	<div class="mb-6 flex w-fit rounded-md border border-border text-sm">
		<button
			onclick={() => activeTab = 'system'}
			class="rounded-l-md px-5 py-1.5 font-medium transition-colors
				{activeTab === 'system' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
		>
			System
		</button>
		<button
			onclick={() => activeTab = 'bcachefs'}
			class="flex items-center gap-2 rounded-r-md px-5 py-1.5 font-medium transition-colors
				{activeTab === 'bcachefs' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
		>
			bcachefs
			{#if bcachefsInfo?.is_custom}
				<span class="h-1.5 w-1.5 rounded-full {activeTab === 'bcachefs' ? 'bg-amber-300' : 'bg-amber-400'}"></span>
			{/if}
		</button>
	</div>

	<!-- System tab -->
	{#if activeTab === 'system'}
		<Card class="mb-6">
			<CardContent class="py-5">
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
			<Card class="mb-6">
				<CardContent class="py-5">
					<div class="mb-5 flex items-center">
						{#each phases as phase, i}
							{@const done = currentPhase >= i}
							{@const active = status.state === 'running' && currentPhase === i - 1}
							{@const failed = status.state === 'failed' && !done}
							<div class="flex items-center gap-0">
								<div class="flex flex-col items-center gap-1">
									<div class="flex h-7 w-7 items-center justify-center rounded-full border-2 text-xs font-semibold transition-all {
										done   ? 'border-blue-500 bg-blue-500 text-white' :
										active ? 'border-blue-400 bg-transparent text-blue-400 animate-pulse' :
										failed ? 'border-red-700 bg-transparent text-red-500' :
										         'border-border bg-transparent text-muted-foreground/30'
									}">
										{#if done}✓{:else if active}…{:else if failed}✕{:else}{i + 1}{/if}
									</div>
									<span class="text-[0.65rem] font-medium {done ? 'text-blue-400' : active ? 'text-blue-400/70' : failed ? 'text-red-500/70' : 'text-muted-foreground/40'}">{phase.label}</span>
								</div>
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
						{#if status.state !== 'running'}
							<button
								onclick={() => logCollapsed = !logCollapsed}
								class="mt-3 flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
							>
								<span class="transition-transform {logCollapsed ? '' : 'rotate-180'} inline-block">▾</span>
								{logCollapsed ? 'Show output' : 'Hide output'}
							</button>
						{/if}
						{#if status.state === 'running' || !logCollapsed}
							<pre bind:this={logEl} class="mt-3 max-h-64 overflow-auto rounded bg-secondary p-3 text-xs leading-relaxed">{formatLog(status.log)}</pre>
						{/if}
					{/if}
					{#if status.state === 'failed'}
						<div class="mt-4 flex gap-2">
							<Button size="sm" onclick={doApplyUpdate}>Retry</Button>
							<Button variant="secondary" size="sm" onclick={() => status = { state: 'idle', log: '', reboot_required: false, webui_changed: false }}>Dismiss</Button>
						</div>
					{/if}
				</CardContent>
			</Card>
		{/if}

		<p class="text-xs text-muted-foreground">
			Updates are fetched from GitHub and applied using NixOS rebuild.
			The system will atomically switch to the new version, restarting services as needed.
			If anything goes wrong, use Rollback to return to the previous version.
		</p>

	<!-- bcachefs tab -->
	{:else if activeTab === 'bcachefs'}
		{#if bcachefsInfo?.is_custom}
			<div class="mb-4 flex items-center gap-3 rounded-lg border border-amber-700 bg-amber-950 px-4 py-3 text-sm text-amber-200">
				<span class="flex-1"><strong>Non-standard version in use.</strong> You are running a custom bcachefs-tools version ({bcachefsInfo.pinned_ref ?? 'unknown'}) instead of the default ({bcachefsInfo.default_ref}). Switch back when stability is more important than bleeding-edge fixes.</span>
				<Button variant="secondary" size="xs" onclick={() => { bcachefsRef = bcachefsInfo!.default_ref; }} disabled={bcachefsSwitching || bcachefsStatus?.state === 'running'}>
					Restore default
				</Button>
			</div>
		{/if}

		<Card class="mb-6">
			<CardContent class="py-5">
				<div class="mb-5 flex flex-wrap items-start gap-6">
					<div>
						<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Pinned</div>
						<div class="font-mono text-sm font-semibold">
							{#if bcachefsInfo?.pinned_ref}
								{bcachefsInfo.pinned_ref}{#if bcachefsInfo.pinned_rev} <span class="text-muted-foreground">({bcachefsInfo.pinned_rev})</span>{/if}
							{:else}
								<span class="text-muted-foreground">unknown</span>
							{/if}
						</div>
					</div>
					<div>
						<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Running</div>
						<div class="font-mono text-sm font-semibold">{bcachefsInfo?.running_version ?? 'unknown'}</div>
					{#if bcachefsInfo?.kernel_rust != null}
						<div class="mt-0.5 text-xs text-muted-foreground">
							kernel Rust: <span class="{bcachefsInfo.kernel_rust ? 'text-green-400' : 'text-yellow-400'}">{bcachefsInfo.kernel_rust ? 'yes' : 'no'}</span>
						</div>
					{/if}
					</div>
					<div>
						<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Default</div>
						<div class="font-mono text-sm font-semibold text-muted-foreground">{bcachefsInfo?.default_ref ?? 'unknown'}</div>
					</div>
				</div>

				<div class="mb-3 flex flex-wrap gap-2">
					<input
						type="text"
						class="h-9 w-64 rounded-md border border-input bg-background px-3 py-1 font-mono text-sm shadow-sm focus:outline-none focus:ring-1 focus:ring-ring"
						placeholder="e.g. v1.38.0, master, abc1234"
						bind:value={bcachefsRef}
						disabled={bcachefsSwitching || bcachefsStatus?.state === 'running'}
					/>
					{#if bcachefsInfo?.default_ref}
						<Button
							variant="secondary"
							size="sm"
							onclick={() => { bcachefsRef = bcachefsInfo!.default_ref; }}
							disabled={bcachefsSwitching || bcachefsStatus?.state === 'running'}
						>
							{bcachefsInfo.default_ref}
						</Button>
					{/if}
					<Button
						variant="secondary"
						size="sm"
						onclick={() => { bcachefsRef = 'master'; }}
						disabled={bcachefsSwitching || bcachefsStatus?.state === 'running'}
					>
						master
					</Button>
				</div>

				{#if bcachefsWarnVisible}
					<div class="mb-3 rounded-lg border border-amber-700 bg-amber-950 px-4 py-3 text-sm text-amber-200">
						<strong>Warning:</strong> Switching versions carries risk. Downgrading after a newer format version
						was written to your pools may leave them unmountable. Consult the bcachefs author before downgrading.
					</div>
				{/if}
				{#if bcachefsRefIsBranch}
					<div class="mb-3 rounded-lg border border-blue-800 bg-blue-950 px-4 py-3 text-sm text-blue-200">
						<strong>Branch detected:</strong> <code class="font-mono">{bcachefsRef.trim()}</code> will be resolved
						to the exact commit it points to right now and pinned there. Future system updates won't follow the
						branch tip. Use a specific tag or commit SHA for full control.
					</div>
				{/if}

				<Button
					size="sm"
					onclick={doBcachefsSwitch}
					disabled={!bcachefsRef.trim() || bcachefsSwitching || bcachefsStatus?.state === 'running'}
				>
					{bcachefsSwitching ? 'Starting...' : 'Switch'}
				</Button>
			</CardContent>
		</Card>

		{#if bcachefsStatus && bcachefsStatus.state !== 'idle'}
			<Card class="mb-6">
				<CardContent class="py-5">
					<div class="mb-5 flex items-center">
						{#each bcachefsPhases as phase, i}
							{@const done = bcachefsCurrentPhase >= i}
							{@const active = bcachefsStatus.state === 'running' && bcachefsCurrentPhase === i - 1}
							{@const failed = bcachefsStatus.state === 'failed' && !done}
							<div class="flex items-center gap-0">
								<div class="flex flex-col items-center gap-1">
									<div class="flex h-7 w-7 items-center justify-center rounded-full border-2 text-xs font-semibold transition-all {
										done   ? 'border-blue-500 bg-blue-500 text-white' :
										active ? 'border-blue-400 bg-transparent text-blue-400 animate-pulse' :
										failed ? 'border-red-700 bg-transparent text-red-500' :
										         'border-border bg-transparent text-muted-foreground/30'
									}">
										{#if done}✓{:else if active}…{:else if failed}✕{:else}{i + 1}{/if}
									</div>
									<span class="text-[0.65rem] font-medium {done ? 'text-blue-400' : active ? 'text-blue-400/70' : failed ? 'text-red-500/70' : 'text-muted-foreground/40'}">{phase.label}</span>
								</div>
								{#if i < bcachefsPhases.length - 1}
									<div class="mb-3.5 h-px w-12 {bcachefsCurrentPhase > i ? 'bg-blue-500' : 'bg-border'} mx-1"></div>
								{/if}
							</div>
						{/each}
						{#if bcachefsStatus.state === 'failed'}
							<span class="ml-4 text-sm text-destructive">Failed</span>
						{/if}
					</div>

					{#if bcachefsStatus.log}
						{#if bcachefsStatus.state !== 'running'}
							<button
								onclick={() => bcachefsLogCollapsed = !bcachefsLogCollapsed}
								class="mt-3 flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
							>
								<span class="transition-transform {bcachefsLogCollapsed ? '' : 'rotate-180'} inline-block">▾</span>
								{bcachefsLogCollapsed ? 'Show output' : 'Hide output'}
							</button>
						{/if}
						{#if bcachefsStatus.state === 'running' || !bcachefsLogCollapsed}
							<pre bind:this={bcachefsLogEl} class="mt-3 max-h-64 overflow-auto rounded bg-secondary p-3 text-xs leading-relaxed">{formatLog(bcachefsStatus.log)}</pre>
						{/if}
					{/if}
					{#if bcachefsStatus.state === 'failed'}
						<div class="mt-4 flex gap-2">
							<Button size="sm" onclick={doBcachefsSwitch} disabled={!bcachefsRef.trim()}>Retry</Button>
							<Button variant="secondary" size="sm" onclick={() => bcachefsStatus = { state: 'idle', log: '', reboot_required: false, webui_changed: false }}>Dismiss</Button>
						</div>
					{/if}
				</CardContent>
			</Card>
		{/if}
	{/if}
{/if}
