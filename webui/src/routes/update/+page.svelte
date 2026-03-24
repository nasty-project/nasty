<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { UpdateInfo, UpdateStatus, BcachefsToolsInfo, Generation } from '$lib/types';
	import { Tag, Trash2, ArrowRightLeft, Pencil, X, Check } from '@lucide/svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { refreshState } from '$lib/refresh.svelte';
	import { rebootState } from '$lib/reboot.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';

	let activeTab: 'system' | 'generations' | 'bcachefs' = $state(
		typeof window !== 'undefined' && window.location.hash === '#bcachefs' ? 'bcachefs'
		: typeof window !== 'undefined' && window.location.hash === '#generations' ? 'generations'
		: 'system'
	);

	// ── Generations tab state ───────────────────────────────
	let generations: Generation[] = $state([]);
	let generationsLoading = $state(false);
	let generationsLoaded = $state(false);
	let editingLabel: number | null = $state(null);
	let editLabelValue = $state('');
	let labelFilter = $state('');

	const filteredGenerations = $derived(
		labelFilter
			? generations.filter(g => g.label?.toLowerCase().includes(labelFilter.toLowerCase()))
			: generations
	);

	const availableLabels = $derived(
		[...new Set(generations.map(g => g.label).filter((l): l is string => !!l))].sort()
	);

	let info: UpdateInfo | null = $state(null);
	let status: UpdateStatus | null = $state(null);
	let loading = $state(true);
	let checking = $state(false);
	let pollInterval: ReturnType<typeof setInterval> | null = null;
	let logEl: HTMLPreElement | undefined = $state();
	let logCollapsed = $state(false);

	// bcachefs-tools switching state
	let bcachefsInfo: BcachefsToolsInfo | null = $state(null);
	let bcachefsStatus: UpdateStatus | null = $state(null);
	let bcachefsRef = $state('');
	let bcachefsDebugChecks = $state(false);
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

	const genPhases = [
		{ label: 'Switch',   marker: '==> Switching to generation' },
		{ label: 'Activate', marker: '==> Activating generation' },
		{ label: 'Done',     marker: '==> Switch to generation' },
	];

	const genCurrentPhase = $derived.by(() => {
		const log = status?.log ?? '';
		let reached = -1;
		for (let i = 0; i < genPhases.length; i++) {
			if (log.includes(genPhases[i].marker)) reached = i;
		}
		return reached;
	});

	const bcachefsPhases = [
		{ label: 'Fetch',    marker: '==> Switching' },
		{ label: 'Build',    marker: '==> Rebuilding' },
		{ label: 'Activate', marker: 'activating the configuration' },
		{ label: 'Done',     marker: '==> bcachefs switch complete!' },
	];

	const bcachefsCurrentPhase = $derived.by(() => {
		const log = bcachefsStatus?.log ?? '';
		let reached = -1;
		for (let i = 0; i < bcachefsPhases.length; i++) {
			if (log.includes(bcachefsPhases[i].marker)) reached = i;
		}
		return reached;
	});

	const bcachefsDebugChanged = $derived(
		bcachefsInfo != null && bcachefsDebugChecks !== (bcachefsInfo as BcachefsToolsInfo).debug_checks
	);

	const bcachefsCanSwitch = $derived(
		bcachefsRef.trim() !== '' || bcachefsDebugChanged
	);

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

	const onReconnect = () => {
		Promise.all([loadVersion(), loadBcachefsInfo()]);
	};

	onMount(() => {
		const t0 = performance.now();
		Promise.all([loadVersion(), loadStatus(), loadBcachefsInfo(), loadBcachefsStatus()]).then(() => {
			if (localStorage.getItem('nasty-debug') === '1') {
				console.debug(`[page] update: ${(performance.now() - t0).toFixed(0)}ms total`);
			}
			loading = false;
		});
		client.onReconnect(onReconnect);
		return () => client.offReconnect(onReconnect);
	});

	onDestroy(() => {
		stopPolling();
		stopBcachefsPolling();
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

	async function applyUpdate() {
		if (!await confirm('Apply System Update?', 'NASty will fetch and apply the latest version. Services will restart. If the update includes UI changes, you will be prompted to reload the page. This can take several minutes.')) return;
		doApplyUpdate();
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
						if (status.reboot_required) rebootState.set();
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
			if (bcachefsInfo) {
				bcachefsDebugChecks = bcachefsInfo.debug_checks;
			}
		});
		sysInfoRefresh.trigger(); // keep sidebar bcachefs version in sync
	}

	async function loadBcachefsStatus() {
		await withToast(async () => {
			bcachefsStatus = await client.call<UpdateStatus>('bcachefs.tools.status');
			if (bcachefsStatus?.state === 'running') {
				startBcachefsPolling();
			}
		});
	}

	async function requestBcachefsSwitch() {
		const ref = bcachefsRef.trim() || bcachefsInfo?.pinned_ref || bcachefsInfo?.default_ref || '';
		if (!ref) return;
		const desc = bcachefsRef.trim()
			? `Switch bcachefs-tools to ${ref}?`
			: `Rebuild bcachefs-tools ${ref} with updated build flags?`;
		if (!await confirm(
			desc,
			'The system will rebuild with the new version. The first build may take 5–20 minutes. If the new version introduced an incompatible on-disk format, downgrading may leave filesystems unmountable.'
		)) return;
		doBcachefsSwitch();
	}

	async function doBcachefsSwitch() {
		const ref = bcachefsRef.trim() || bcachefsInfo?.pinned_ref || bcachefsInfo?.default_ref || '';
		if (!ref) return;
		bcachefsSwitching = true;
		bcachefsLogCollapsed = false;
		bcachefsStatus = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const result = await withToast(
			() => client.call('bcachefs.tools.switch', { git_ref: ref, debug_checks: bcachefsDebugChecks }),
			'bcachefs switch started'
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
						if (bcachefsStatus.reboot_required) rebootState.set();
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

	// ── Generations ─────────────────────────────────────────

	async function loadGenerations() {
		generationsLoading = true;
		try {
			generations = await client.call<Generation[]>('system.generations.list');
		} catch {
			generations = [];
		}
		generationsLoading = false;
		generationsLoaded = true;
	}

	async function switchGeneration(gen: number) {
		if (!await confirm(
			`Switch to Generation ${gen}?`,
			'The system will activate this generation. Services will restart. A reboot may be required if the kernel changed.'
		)) return;

		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const ok = await withToast(
			() => client.call('system.generations.switch', { generation: gen }),
			`Switching to generation ${gen}`
		);
		if (ok !== undefined) {
			startPolling();
		}
	}

	async function saveLabel(gen: number) {
		await withToast(
			() => client.call('system.generations.label', {
				generation: gen,
				label: editLabelValue.trim() || null,
			}),
			editLabelValue.trim() ? 'Label saved' : 'Label removed'
		);
		editingLabel = null;
		await loadGenerations();
	}

	async function deleteGeneration(gen: number) {
		if (!await confirm(
			`Delete Generation ${gen}?`,
			'This generation will be removed. You can reclaim disk space by running garbage collection afterwards.'
		)) return;

		await withToast(
			() => client.call('system.generations.delete', { generation: gen }),
			`Generation ${gen} deleted`
		);
		await loadGenerations();
	}

	function startEditLabel(gen: Generation) {
		editingLabel = gen.generation;
		editLabelValue = gen.label ?? '';
	}
</script>


<!-- Global banners — shown regardless of active tab -->


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
			onclick={() => { activeTab = 'generations'; loadGenerations(); }}
			class="px-5 py-1.5 font-medium transition-colors
				{activeTab === 'generations' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
		>
			Generations
		</button>
		<button
			onclick={() => activeTab = 'bcachefs'}
			class="flex items-center gap-2 rounded-r-md px-5 py-1.5 font-medium transition-colors
				{activeTab === 'bcachefs' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
		>
			bcachefs
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
							<span class="rounded-md border border-amber-600 bg-amber-950 px-2.5 py-0.5 text-xs font-medium text-amber-400">Update available</span>
						{:else if info?.update_available === false}
							<span class="rounded-md border border-green-700 bg-green-950 px-2.5 py-0.5 text-xs font-medium text-green-400">Up to date</span>
						{/if}
					</div>
				</div>

				<div class="flex gap-2">
					<Button size="sm" onclick={checkForUpdates} disabled={checking || status?.state === 'running'}>
						{checking ? 'Checking...' : 'Check for Updates'}
					</Button>
					{#if info?.update_available}
						<Button
							variant="default"
							size="sm"
							onclick={applyUpdate}
							disabled={status?.state === 'running'}
						>
							Update Now
						</Button>
					{/if}
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
			Updates are fetched and applied atomically. If the build fails, the running system is not affected.
			Use the Generations tab to switch to any previous system version.
		</p>

	<!-- Generations tab -->
	{:else if activeTab === 'generations'}
		<Card>
			<CardContent class="py-5">
				<div class="mb-4 flex items-center justify-between">
					<div>
						<h2 class="text-base font-semibold">System Generations</h2>
						<p class="text-xs text-muted-foreground">Each update creates a new generation. Switch to any previous version or label known-good configurations.</p>
					</div>
					<div class="flex items-center gap-2">
						{#if availableLabels.length > 0}
							<select
								bind:value={labelFilter}
								class="rounded-md border border-input bg-background px-2 py-1 text-xs focus:outline-none focus:ring-1 focus:ring-ring"
							>
								<option value="">All labels</option>
								{#each availableLabels as label}
									<option value={label}>{label}</option>
								{/each}
							</select>
						{/if}
						<Button size="sm" variant="outline" onclick={loadGenerations} disabled={generationsLoading}>
							{generationsLoading ? 'Loading…' : 'Refresh'}
						</Button>
					</div>
				</div>

				{#if generationsLoading && !generationsLoaded}
					<p class="text-sm text-muted-foreground">Loading generations...</p>
				{:else if filteredGenerations.length === 0}
					<p class="text-sm text-muted-foreground">{labelFilter ? 'No generations match this label.' : 'No generations found.'}</p>
				{:else}
					<div class="overflow-x-auto">
						<table class="w-full text-sm">
							<thead>
								<tr class="border-b border-border text-left text-xs text-muted-foreground">
									<th class="pb-2 pr-4">#</th>
									<th class="pb-2 pr-4">Date</th>
									<th class="pb-2 pr-4">NASty</th>
									<th class="pb-2 pr-4">Kernel</th>
									<th class="pb-2 pr-4">Status</th>
									<th class="pb-2 pr-4">Label</th>
									<th class="pb-2 text-right">Actions</th>
								</tr>
							</thead>
							<tbody>
								{#each filteredGenerations as gen}
									<tr class="border-b border-border/50 {gen.current ? 'bg-blue-500/5' : ''} {gen.booted && !gen.current ? 'bg-amber-500/5' : ''}">
										<td class="py-2.5 pr-4 font-mono font-semibold">{gen.generation}</td>
										<td class="py-2.5 pr-4 font-mono text-xs">{gen.date}</td>
										<td class="py-2.5 pr-4 font-mono text-xs">{gen.nasty_version ?? '—'}</td>
										<td class="py-2.5 pr-4 font-mono text-xs">{gen.kernel_version}</td>
										<td class="py-2.5 pr-4">
											{#if gen.current && gen.booted}
												<span class="rounded-md border border-green-700 bg-green-950 px-2 py-0.5 text-xs font-medium text-green-400">Active & Booted</span>
											{:else if gen.current}
												<span class="rounded-md border border-blue-700 bg-blue-950 px-2 py-0.5 text-xs font-medium text-blue-400">Active</span>
											{:else if gen.booted}
												<span class="rounded-md border border-amber-700 bg-amber-950 px-2 py-0.5 text-xs font-medium text-amber-400">Booted</span>
											{/if}
										</td>
										<td class="py-2.5 pr-4">
											{#if editingLabel === gen.generation}
												<div class="flex items-center gap-1">
													<input
														type="text"
														bind:value={editLabelValue}
														class="w-28 rounded-md border border-input bg-background px-2 py-0.5 text-xs focus:outline-none focus:ring-1 focus:ring-ring"
														placeholder="e.g. stable"
														onkeydown={(e) => { if (e.key === 'Enter') saveLabel(gen.generation); if (e.key === 'Escape') editingLabel = null; }}
													/>
													<button onclick={() => saveLabel(gen.generation)} class="text-green-400 hover:text-green-300" title="Save">
														<Check class="h-3.5 w-3.5" />
													</button>
													<button onclick={() => editingLabel = null} class="text-muted-foreground hover:text-foreground" title="Cancel">
														<X class="h-3.5 w-3.5" />
													</button>
												</div>
											{:else if gen.label}
												<button
													onclick={() => startEditLabel(gen)}
													class="flex items-center gap-1 rounded-md border border-border px-2 py-0.5 text-xs text-foreground hover:bg-accent transition-colors"
												>
													<Tag class="h-3 w-3" />{gen.label}
												</button>
											{:else}
												<button
													onclick={() => startEditLabel(gen)}
													class="text-muted-foreground/50 hover:text-muted-foreground transition-colors" title="Add label"
												>
													<Tag class="h-3.5 w-3.5" />
												</button>
											{/if}
										</td>
										<td class="py-2.5 text-right">
											<div class="flex items-center justify-end gap-1">
												{#if !gen.current}
													<button
														onclick={() => switchGeneration(gen.generation)}
														class="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground transition-colors"
														title="Switch to this generation"
														disabled={status?.state === 'running'}
													>
														<ArrowRightLeft class="h-4 w-4" />
													</button>
													<button
														onclick={() => deleteGeneration(gen.generation)}
														class="rounded-md p-1 text-muted-foreground hover:bg-destructive/10 hover:text-destructive transition-colors"
														title="Delete this generation"
														disabled={gen.booted || status?.state === 'running'}
													>
														<Trash2 class="h-4 w-4" />
													</button>
												{/if}
											</div>
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			</CardContent>
		</Card>

		{#if status && status.state !== 'idle'}
			<Card class="mt-6">
				<CardContent class="py-5">
					<div class="mb-5 flex items-center">
						{#each genPhases as phase, i}
							{@const done = genCurrentPhase >= i}
							{@const active = status.state === 'running' && genCurrentPhase === i - 1}
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
								{#if i < genPhases.length - 1}
									<div class="mb-3.5 h-px w-12 {genCurrentPhase > i ? 'bg-blue-500' : 'bg-border'} mx-1"></div>
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
								class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
							>
								<span class="transition-transform {logCollapsed ? '' : 'rotate-180'} inline-block">▾</span>
								{logCollapsed ? 'Show output' : 'Hide output'}
							</button>
						{/if}
						{#if status.state === 'running' || !logCollapsed}
							<pre bind:this={logEl} class="mt-3 max-h-64 overflow-auto rounded bg-secondary p-3 text-xs leading-relaxed">{formatLog(status.log)}</pre>
						{/if}
					{/if}
				</CardContent>
			</Card>
		{/if}

	<!-- bcachefs tab -->
	{:else if activeTab === 'bcachefs'}
		{#if bcachefsInfo?.is_custom_running}
			<div class="mb-4 flex items-center gap-3 rounded-lg border border-amber-700 bg-amber-950 px-4 py-3 text-sm text-amber-200">
				<span class="flex-1"><strong>Non-standard version in use.</strong> You are running a custom bcachefs version ({bcachefsInfo.running_version}) instead of the default ({bcachefsInfo.default_ref}). Switch back when stability is more important than bleeding-edge fixes.</span>
				<Button variant="secondary" size="xs" onclick={() => { bcachefsRef = bcachefsInfo!.default_ref; }} disabled={bcachefsSwitching || bcachefsStatus?.state === 'running'}>
					Restore default
				</Button>
			</div>
		{/if}
		{#if bcachefsInfo?.debug_checks_running}
			<div class="mb-4 rounded-lg border border-blue-800 bg-blue-950 px-4 py-3 text-sm text-blue-200">
				<strong>Debug checks enabled.</strong> The bcachefs kernel module is running with extra runtime assertions (CONFIG_BCACHEFS_DEBUG). This adds overhead to every filesystem operation. Disable when no longer needed for development or debugging.
			</div>
		{/if}

		<Card class="mb-6">
			<CardContent class="py-5">
				<div class="mb-5 flex flex-wrap items-start gap-6">
					<div>
						<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Pinned</div>
						<div class="font-mono text-sm font-semibold">
							{#if bcachefsInfo?.pinned_ref}
								{bcachefsInfo.pinned_ref}{#if bcachefsInfo.pinned_rev && !/^[0-9a-f]+$/i.test(bcachefsInfo.pinned_ref)} <span class="text-muted-foreground">({bcachefsInfo.pinned_rev})</span>{/if}
							{:else}
								<span class="text-muted-foreground">unknown</span>
							{/if}
						</div>
					</div>
					<div>
						<div class="mb-0.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">Running</div>
						<div class="font-mono text-sm font-semibold">{bcachefsInfo?.running_version ?? 'unknown'}{bcachefsInfo?.is_custom && bcachefsInfo?.pinned_rev && !/^v\d/.test(bcachefsInfo.pinned_ref ?? '') ? ' @ ' + bcachefsInfo.pinned_rev : ''}</div>
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
						class="h-9 w-96 rounded-md border border-input bg-background px-3 py-1 font-mono text-sm shadow-sm focus:outline-none focus:ring-1 focus:ring-ring"
						placeholder="e.g. v1.38.0, master, 098dad22ef7725620930587a047813469fceedce"
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
					<div class="mb-3 rounded-lg border border-amber-700 bg-amber-950 px-4 py-3 text-sm text-amber-200 space-y-1.5">
						<div><strong>Build time:</strong> bcachefs-tools is compiled from source — the first build can take 5–20 minutes depending on your hardware.</div>
						<div><strong>Compatibility risk:</strong> If a newer on-disk format was already written to your filesystems, downgrading may leave them unmountable. Reach out to the bcachefs devs if you run into problems.</div>
					</div>
				{/if}
				{#if bcachefsRefIsBranch}
					<div class="mb-3 rounded-lg border border-blue-800 bg-blue-950 px-4 py-3 text-sm text-blue-200">
						<strong>Branch detected:</strong> <code class="font-mono">{bcachefsRef.trim()}</code> will be resolved
						to the exact commit it points to right now and pinned there. Future system updates won't follow the
						branch tip. Use a specific tag or commit SHA for full control.
					</div>
				{/if}

				<div class="mb-3 flex items-start gap-6">
					<label class="flex items-start gap-2 text-sm text-muted-foreground" title="Debug symbols are inherited from the NixOS kernel config (CONFIG_DEBUG_INFO=y)">
						<input
							type="checkbox"
							checked={bcachefsInfo?.debug_symbols ?? false}
							disabled
							class="mt-0.5 rounded border-input opacity-50"
						/>
						<span>
							<span class="text-muted-foreground font-medium">Debug symbols</span>
							<span class="block text-xs text-muted-foreground/50 mt-0.5">Inherited from NixOS kernel config (CONFIG_DEBUG_INFO=y).<br/>Adds source-level debug info for readable stack traces.<br/>No runtime cost.</span>
						</span>
					</label>
					<label class="flex items-start gap-2 text-sm text-muted-foreground cursor-pointer">
						<input
							type="checkbox"
							bind:checked={bcachefsDebugChecks}
							disabled={bcachefsSwitching || bcachefsStatus?.state === 'running'}
							class="mt-0.5 rounded border-input"
						/>
						<span>
							<span class="text-foreground font-medium">Debug checks</span>
							<span class="block text-xs text-muted-foreground/70 mt-0.5">Enables extra runtime assertions inside bcachefs<br/>(btree locking, iterator validation, bkey verification).<br/><strong>Has performance cost</strong> — use for development and debugging, not production.</span>
						</span>
					</label>
				</div>

				<Button
					variant="default"
					size="sm"
					onclick={requestBcachefsSwitch}
					disabled={!bcachefsCanSwitch || bcachefsSwitching || bcachefsStatus?.state === 'running'}
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
