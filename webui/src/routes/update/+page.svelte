<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type {
		UpdateInfo,
		UpdateStatus,
		Generation,
		FirmwareDevice,
		FirmwareUpdateResult,
		VersionInfo,
		VersionTaggedReleaseStatus
	} from '$lib/types';
	import { Tag, Trash2, ArrowRightLeft, X, Check, ChevronDown, ChevronRight } from '@lucide/svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Input } from '$lib/components/ui/input';
	import { refreshState } from '$lib/refresh.svelte';
	import { rebootState } from '$lib/reboot.svelte';

	type Tab = 'version' | 'generations' | 'firmware';
	type VersionRow = {
		name: string;
		label: string;
		url: string;
		rev: string | null;
		update: boolean;
		initialUrl: string;
		initialRev: string | null;
	};

	type TaggedReleaseBannerState =
		| { kind: 'loading' }
		| { kind: 'switching' }
		| { kind: 'failure' }
		| ({ kind: 'ready' } & VersionTaggedReleaseStatus);

	const client = getClient();
	const VERSION_PAGE_ACTION_KEY = 'nasty.version-page.action';

	let activeTab: Tab = $state(
		typeof window !== 'undefined' && window.location.hash === '#generations' ? 'generations'
		: typeof window !== 'undefined' && window.location.hash === '#firmware' ? 'firmware'
		: 'version'
	);

	let info: UpdateInfo | null = $state(null);
	let checkInfo: UpdateInfo | null = $state(null);
	let checking = $state(false);
	let startingDevUpgrade = $state(false);
	let taggedReleaseBanner: TaggedReleaseBannerState = $state({ kind: 'loading' });
	let versionRows: VersionRow[] = $state([]);
	let status: UpdateStatus | null = $state(null);
	let loading = $state(true);
	let startingSwitch = $state(false);
	let startingUpgrade = $state(false);
	let upstreamExpanded = $state(false);
	let pollInterval: ReturnType<typeof setInterval> | null = null;
	let logEl: HTMLPreElement | undefined = $state();
	let logCollapsed = $state(true);
	let taggedReleaseBannerRequestId = 0;

	let generations: Generation[] = $state([]);
	let generationsLoading = $state(false);
	let generationsLoaded = $state(false);
	let editingLabel: number | null = $state(null);
	let editLabelValue = $state('');
	let labelFilter = $state('');

	let firmwareAvailable = $state(false);
	let firmwareDevices: FirmwareDevice[] = $state([]);
	let firmwareLoading = $state(false);
	let firmwareLoaded = $state(false);
	let firmwareUpdating: Record<string, boolean> = $state({});

	const phases = [
		{ label: 'Fetch', marker: '==> Updating local system flake' },
		{ label: 'Build', marker: '==> Rebuilding' },
		{ label: 'Activate', marker: 'activating the configuration' },
		{ label: 'Done', marker: '==> Update complete!' }
	];

	const genPhases = [
		{ label: 'Switch', marker: '==> Switching to generation' },
		{ label: 'Activate', marker: '==> Activating generation' },
		{ label: 'Done', marker: '==> Switch to generation' }
	];

	const currentPhase = $derived.by(() => {
		const log = status?.log ?? '';
		let reached = -1;
		for (let i = 0; i < phases.length; i++) {
			if (log.includes(phases[i].marker)) reached = i;
		}
		return reached;
	});

	const genCurrentPhase = $derived.by(() => {
		const log = status?.log ?? '';
		let reached = -1;
		for (let i = 0; i < genPhases.length; i++) {
			if (log.includes(genPhases[i].marker)) reached = i;
		}
		return reached;
	});

	const versionDirty = $derived.by(() =>
		versionRows.some((row) => row.url.trim() !== row.initialUrl || row.update)
	);

	const isDevBuild = $derived.by(() => {
		// Use banner data if available (most accurate)
		if (taggedReleaseBanner.kind === 'ready') {
			return !isOfficialTaggedReleaseUrl(taggedReleaseBanner.current_url);
		}
		// Fall back to version rows while banner is loading/switching
		const nastyRow = versionRows.find(r => r.name === 'nasty');
		if (nastyRow) {
			return !isOfficialTaggedReleaseUrl(nastyRow.url);
		}
		return false;
	});

	const upstreamBusy = $derived.by(() =>
		startingSwitch || startingUpgrade || startingDevUpgrade || status?.state === 'running'
	);

	const versionSelectionCount = $derived.by(() =>
		versionRows.filter((row) => row.update || row.url.trim() !== row.initialUrl).length
	);

	const filteredGenerations = $derived(
		labelFilter
			? generations.filter((g) => g.label?.toLowerCase().includes(labelFilter.toLowerCase()))
			: generations
	);

	const availableLabels = $derived(
		[...new Set(generations.map((g) => g.label).filter((l): l is string => !!l))].sort()
	);

	const versionStatusVisible = $derived.by(() => {
		if (!status || status.state === 'idle') return false;
		const log = status.log ?? '';
		return currentPhase >= 0 || log.includes('No flake.lock changes detected');
	});

	const generationStatusVisible = $derived.by(() => {
		if (!status || status.state === 'idle') return false;
		return genCurrentPhase >= 0;
	});

	$effect(() => {
		if (status?.log && logEl) {
			logEl.scrollTop = logEl.scrollHeight;
		}
	});

	onMount(() => {
		const onReconnect = () => {
			Promise.all([
				loadVersionPage(),
				loadStatus(),
				activeTab === 'firmware' && firmwareLoaded ? loadFirmware() : Promise.resolve()
			]);
		};

		Promise.all([
			loadVersionPage(),
			loadStatus()
		]).finally(() => {
			loading = false;
		});

		client.onReconnect(onReconnect);

		return () => {
			client.offReconnect(onReconnect);
		};
	});

	onDestroy(() => {
		stopPolling();
	});

	function versionLabel(name: string): string {
		switch (name) {
			case 'nixpkgs': return 'nixpkgs';
			case 'bcachefs-tools': return 'bcachefs-tools';
			case 'nasty': return 'nasty';
			default: return name;
		}
	}

	function syncVersionRows(next: VersionInfo) {
		versionRows = next.inputs.map((input) => ({
			name: input.name,
			label: versionLabel(input.name),
			url: input.url,
			rev: input.rev,
			update: false,
			initialUrl: input.url,
			initialRev: input.rev
		}));
	}

	function isForcedVersionUpdate(row: VersionRow): boolean {
		return row.url.trim() !== row.initialUrl;
	}

	function isOfficialTaggedReleaseUrl(url: string): boolean {
		return /^github:nasty-project\/nasty\/v\d+\.\d+\.\d+$/.test(url.trim());
	}

	function setTab(tab: Tab) {
		activeTab = tab;
		if (typeof window !== 'undefined') {
			window.location.hash = tab === 'version' ? '#version' : `#${tab}`;
		}
		if (tab === 'generations' && !generationsLoaded) void loadGenerations();
		if (tab === 'firmware' && !firmwareLoaded) void loadFirmware();
	}

	function readVersionPageAction(): string | null {
		if (typeof window === 'undefined') return null;
		return window.sessionStorage.getItem(VERSION_PAGE_ACTION_KEY);
	}

	function writeVersionPageAction(action: string | null) {
		if (typeof window === 'undefined') return;
		if (action) {
			window.sessionStorage.setItem(VERSION_PAGE_ACTION_KEY, action);
		} else {
			window.sessionStorage.removeItem(VERSION_PAGE_ACTION_KEY);
		}
	}

	async function loadVersionPage() {
		await withToast(async () => {
			const [nextInfo, nextVersion] = await Promise.all([
				client.call<VersionInfo>('system.version.get'),
				client.call<UpdateInfo>('system.update.version')
			]);
			info = nextVersion;
			syncVersionRows(nextInfo);
			if (readVersionPageAction() === 'version-switch') {
				taggedReleaseBanner = { kind: 'switching' };
			} else {
				void loadTaggedReleaseBanner();
			}
		});
	}

	async function loadTaggedReleaseBanner() {
		if (readVersionPageAction() === 'version-switch') {
			taggedReleaseBanner = { kind: 'switching' };
			return;
		}
		const requestId = ++taggedReleaseBannerRequestId;
		const prev = taggedReleaseBanner;
		if (prev.kind !== 'ready') {
			taggedReleaseBanner = { kind: 'loading' };
		}
		try {
			const releaseStatus = await client.call<VersionTaggedReleaseStatus>(
				'system.version.tagged_release_notice'
			);
			if (requestId === taggedReleaseBannerRequestId) {
				taggedReleaseBanner = { kind: 'ready', ...releaseStatus };
			}
		} catch {
			if (requestId === taggedReleaseBannerRequestId && prev.kind !== 'ready') {
				taggedReleaseBanner = { kind: 'failure' };
			}
			// If we already had release info, keep it rather than showing an error
		}
	}

	async function loadStatus() {
		await withToast(async () => {
			status = await client.call<UpdateStatus>('system.update.status');
			if (readVersionPageAction() === 'version-switch') {
				if (status?.state === 'running') {
					taggedReleaseBanner = { kind: 'switching' };
				} else {
					writeVersionPageAction(null);
					void loadTaggedReleaseBanner();
				}
			}
			if (status?.state === 'running') startPolling();
		});
	}

	async function requestVersionSwitch() {
		if (!versionDirty) return;
		const changedUrls = versionRows.filter((row) => row.url.trim() !== row.initialUrl);
		const refreshed = versionRows.filter((row) => row.update || row.url.trim() !== row.initialUrl);
		const changedLabel = changedUrls.length > 0
			? changedUrls.map((row) => row.name).join(', ')
			: 'none';
		const refreshedLabel = refreshed.map((row) => row.name).join(', ');

		if (!await confirm(
			'Switch upstream inputs?',
			`This will write the selected input URLs directly into /etc/nixos/flake.nix and refresh these inputs in flake.lock: ${refreshedLabel}. URL changes are always refreshed to keep flake.lock consistent. If flake.lock changes, the system rebuild starts immediately. Changed URLs: ${changedLabel}.`
		)) return;

		await doVersionSwitch();
	}

	async function doVersionSwitch() {
		startingSwitch = true;
		writeVersionPageAction('version-switch');
		taggedReleaseBanner = { kind: 'switching' };
		logCollapsed = false;
		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const result = await withToast(
			() => client.call('system.version.switch', {
				inputs: versionRows.map((row) => ({
					name: row.name,
					url: row.url.trim(),
					update: row.update
				}))
			}),
			'Version switch started'
		);
		if (result !== undefined) {
			startPolling();
		} else {
			writeVersionPageAction(null);
			void loadTaggedReleaseBanner();
		}
		startingSwitch = false;
	}

	async function upgradeTaggedRelease() {
		if (taggedReleaseBanner.kind !== 'ready' || taggedReleaseBanner.current_is_latest_standard_url) return;
		if (!await confirm(
			'Switch to upstream tagged release?',
			`This will switch this system to the upstream tagged release ${taggedReleaseBanner.latest_tag} and start a rebuild immediately.`,
			{ confirmLabel: 'Go on', cancelLabel: 'Decline' }
		)) return;
		startingUpgrade = true;
		logCollapsed = false;
		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const result = await withToast(
			() => client.call('system.version.upgrade_tagged_release'),
			'Tagged release upgrade started'
		);
		if (result !== undefined) {
			startPolling();
		} else {
			status = null;
		}
		startingUpgrade = false;
	}

	async function checkForUpdates() {
		checking = true;
		try {
			checkInfo = await client.call<UpdateInfo>('system.update.check');
		} catch {
			checkInfo = null;
		}
		checking = false;
	}

	async function upgradeDevBuild() {
		if (!await confirm(
			'Update to latest development build?',
			`This will fetch the latest commit from the nasty input and rebuild the system if there are changes.`
		)) return;

		startingDevUpgrade = true;
		logCollapsed = false;
		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const result = await withToast(
			() => client.call('system.version.switch', {
				inputs: versionRows.map((row) => ({
					name: row.name,
					url: row.url.trim(),
					update: row.name === 'nasty'
				}))
			}),
			'Development build update started'
		);
		if (result !== undefined) {
			startPolling();
		} else {
			status = null; // RPC failed — clear running state, keep everything else
		}
		startingDevUpgrade = false;
	}

	function startPolling() {
		stopPolling();
		pollInterval = setInterval(async () => {
			try {
					status = await client.call<UpdateStatus>('system.update.status');
					if (status && (status.state === 'success' || status.state === 'failed')) {
						stopPolling();
						checkInfo = null; // clear stale "available" after upgrade
						await loadVersionPage();
						writeVersionPageAction(null);
						if (status.state === 'success') {
						if (status.webui_changed) refreshState.set();
						if (status.reboot_required) rebootState.set();
						setTimeout(() => { logCollapsed = true; }, 3000);
					}
				}
			} catch {
				// Rebuild can restart services and briefly drop the socket.
			}
		}, 3000);
	}

	function stopPolling() {
		if (pollInterval) {
			clearInterval(pollInterval);
			pollInterval = null;
		}
	}

	function formatLog(log: string): string {
		return log.replace(
			/(^.+: Consumed .+)$/m,
			(line) => line.replace(/, /g, ',\n  ')
		);
	}

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

		logCollapsed = false;
		status = { state: 'running', log: '', reboot_required: false, webui_changed: false };
		const ok = await withToast(
			() => client.call('system.generations.switch', { generation: gen }),
			`Switching to generation ${gen}`
		);
		if (ok !== undefined) startPolling();
	}

	async function saveLabel(gen: number) {
		await withToast(
			() => client.call('system.generations.label', {
				generation: gen,
				label: editLabelValue.trim() || null
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

	async function loadFirmware() {
		firmwareLoading = true;
		try {
			firmwareAvailable = await client.call<boolean>('firmware.available');
			if (firmwareAvailable) {
				firmwareDevices = await client.call<FirmwareDevice[]>('firmware.check');
			}
		} catch {
			// Ignore fwupd errors in the page shell.
		}
		firmwareLoading = false;
		firmwareLoaded = true;
	}

	async function updateFirmware(deviceId: string) {
		if (!await confirm(
			'Apply firmware update?',
			'This will flash new firmware to the device. Do not power off during the update. A reboot may be required.'
		)) return;
		firmwareUpdating[deviceId] = true;
		const result = await withToast(
			() => client.call<FirmwareUpdateResult>('firmware.update', { device_id: deviceId }),
			'Firmware update applied'
		);
		firmwareUpdating[deviceId] = false;
		if (result?.reboot_required) rebootState.set();
		await loadFirmware();
	}
</script>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<div class="mb-6 flex border-b border-border">
		<button
			onclick={() => setTab('version')}
			class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'version'
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>Version</button>
		<button
			onclick={() => setTab('generations')}
			class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'generations'
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>Generations</button>
		<button
			onclick={() => setTab('firmware')}
			class="px-4 py-2 text-sm font-medium transition-colors {activeTab === 'firmware'
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>Firmware</button>
	</div>

	{#if activeTab === 'version'}
		<Card class="mb-6">
			<CardContent class="pt-6">
				<div class="rounded-lg border border-border/60 text-sm">
					<div class="grid grid-cols-1 lg:grid-cols-2">
						<!-- Left: Current build / updates -->
						<div class="{isDevBuild ? 'bg-blue-500/5' : 'bg-muted/10'}">
							<div class="flex items-center gap-2 border-b border-border/30 px-4 py-2">
								<span class="text-xs font-medium uppercase tracking-wide text-muted-foreground">{isDevBuild ? 'Development Build' : 'Current Version'}</span>
								{#if isDevBuild}
									<span class="text-xs text-muted-foreground">·</span>
									<code class="text-xs text-muted-foreground">{taggedReleaseBanner.kind === 'ready' ? taggedReleaseBanner.current_url : ''}</code>
								{/if}
							</div>
							<div class="px-4 py-3">
								<table class="text-sm">
									<tbody>
									<tr>
										<td class="pr-4 text-muted-foreground">Installed</td>
										<td class="font-mono font-semibold">{info?.current_version ?? '—'}</td>
									</tr>
									{#if isDevBuild}
										<tr>
											<td class="pr-4 text-muted-foreground">Available</td>
											<td class="font-mono font-semibold">
												{#if checkInfo?.update_available === true}
													<span class="text-blue-400">{checkInfo.latest_version}</span>
												{:else if checkInfo?.update_available === false}
													<span class="text-muted-foreground">up to date</span>
												{:else}
													<span class="text-muted-foreground">—</span>
												{/if}
											</td>
										</tr>
									{:else}
										<tr>
											<td class="pr-4 text-muted-foreground">Latest</td>
											<td class="font-mono font-semibold">
												{#if taggedReleaseBanner.kind === 'ready'}
													{#if taggedReleaseBanner.current_is_latest_standard_url}
														<span class="text-muted-foreground">up to date</span>
													{:else}
														<span class="text-emerald-400">{taggedReleaseBanner.latest_tag}</span>
													{/if}
												{:else}
													<span class="text-muted-foreground">—</span>
												{/if}
											</td>
										</tr>
									{/if}
									</tbody>
								</table>
								<div class="mt-3 flex gap-2">
									{#if isDevBuild}
										<Button size="sm" variant="secondary" onclick={checkForUpdates} disabled={checking || upstreamBusy}>
											{checking ? 'Checking...' : 'Check for Updates'}
										</Button>
										{#if checkInfo?.update_available}
											<Button size="sm" onclick={upgradeDevBuild} disabled={startingDevUpgrade || status?.state === 'running'}>
												{startingDevUpgrade ? 'Starting...' : 'Upgrade'}
											</Button>
										{/if}
									{:else if taggedReleaseBanner.kind === 'ready' && !taggedReleaseBanner.current_is_latest_standard_url}
										<Button size="sm" onclick={upgradeTaggedRelease} disabled={startingUpgrade || status?.state === 'running'}>
											{startingUpgrade ? 'Starting...' : 'Upgrade'}
										</Button>
									{/if}
								</div>
							</div>
						</div>

						<!-- Right: Tagged release -->
						<div class="border-t lg:border-t-0 lg:border-l border-border/30">
							<div class="flex items-center gap-2 border-b border-border/30 px-4 py-2">
								<span class="text-xs font-medium uppercase tracking-wide text-muted-foreground">Tagged Release</span>
								{#if taggedReleaseBanner.kind === 'loading'}
									<span class="text-xs text-muted-foreground">· Fetching...</span>
								{:else if taggedReleaseBanner.kind === 'failure'}
									<span class="text-xs text-amber-400">· Network failure</span>
								{/if}
							</div>
							<div class="px-4 py-3">
								{#if taggedReleaseBanner.kind === 'ready'}
									<table class="text-sm">
										<tbody>
										<tr>
											<td class="pr-4 text-muted-foreground">Latest</td>
											<td class="font-mono font-semibold">{taggedReleaseBanner.latest_tag}</td>
										</tr>
										</tbody>
									</table>
									{#if isDevBuild && !taggedReleaseBanner.current_is_latest_standard_url}
										<div class="mt-3">
											<Button size="sm" variant="secondary" onclick={upgradeTaggedRelease} disabled={startingUpgrade || status?.state === 'running'}>
												{startingUpgrade ? 'Starting...' : 'Switch to release'}
											</Button>
										</div>
									{:else if taggedReleaseBanner.current_is_latest_standard_url}
										<div class="mt-2 text-xs text-muted-foreground">You are on this release.</div>
									{/if}
								{:else if taggedReleaseBanner.kind !== 'loading'}
									<div class="text-xs text-muted-foreground">Could not fetch release info.</div>
								{/if}
							</div>
						</div>
					</div>
				</div>

				<div class="rounded-lg border border-border/60">
					<button
						onclick={() => { upstreamExpanded = !upstreamExpanded; }}
						class="flex w-full items-center gap-2 px-4 py-3 text-left transition-colors hover:bg-muted/20"
					>
						{#if upstreamExpanded}
							<ChevronDown class="h-4 w-4 shrink-0 text-muted-foreground" />
						{:else}
							<ChevronRight class="h-4 w-4 shrink-0 text-muted-foreground" />
						{/if}
						<div class="flex-1">
							<div class="font-medium">Upstream</div>
							<div class="text-xs text-muted-foreground">Edit live flake input URLs and rebuild from /etc/nixos.</div>
						</div>
					</button>

					{#if upstreamExpanded}
						<div class="space-y-4 border-t border-border/60 p-4 {upstreamBusy ? 'pointer-events-none opacity-50' : ''}">
							<div class="space-y-3">
								{#each versionRows as row}
									<div class="rounded-lg border border-border/60 p-4">
										<div class="mb-2 flex items-center justify-between gap-3">
											<div class="font-medium">{row.label}</div>
											<div class="flex items-center gap-2 text-xs">
												<span class="text-muted-foreground">locked</span>
												<Badge variant="secondary" class="font-mono">{row.rev ?? 'unknown'}</Badge>
											</div>
										</div>
										<div class="flex flex-col gap-3 lg:flex-row lg:items-center">
											<div class="flex-1">
												<Input
													bind:value={row.url}
													disabled={startingSwitch || status?.state === 'running'}
													class="font-mono text-sm"
												/>
											</div>
											<label class="flex items-center gap-2 text-sm text-muted-foreground lg:w-28 lg:justify-end">
												<input
													type="checkbox"
													checked={row.update || isForcedVersionUpdate(row)}
													disabled={startingSwitch || status?.state === 'running' || isForcedVersionUpdate(row)}
													onchange={(event) => {
														row.update = (event.currentTarget as HTMLInputElement).checked;
													}}
													class="h-4 w-4 rounded border-input"
												/>
												<span>Update</span>
											</label>
										</div>
									</div>
								{/each}
							</div>

							<div class="flex flex-wrap items-center justify-between gap-3">
								<p class="text-xs text-muted-foreground">
									{#if versionSelectionCount > 0}
										{versionSelectionCount} input{versionSelectionCount === 1 ? '' : 's'} selected for refresh.
									{:else}
										No refresh selected yet.
									{/if}
								</p>
								<Button
									size="sm"
									onclick={requestVersionSwitch}
									disabled={!versionDirty || startingSwitch || status?.state === 'running'}
								>
									{startingSwitch ? 'Starting...' : 'Switch'}
								</Button>
							</div>
						</div>
					{/if}
				</div>
			</CardContent>
		</Card>

		{#if versionStatusVisible}
			<Card class="mb-6">
				<CardContent class="py-5">
					<div class="mb-5 flex items-center">
						{#each phases as phase, i}
							{@const done = currentPhase >= i}
							{@const active = status?.state === 'running' && currentPhase === i - 1}
							{@const failed = status?.state === 'failed' && !done}
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
									<div class="mx-1 mb-3.5 h-px w-12 {currentPhase > i ? 'bg-blue-500' : 'bg-border'}"></div>
								{/if}
							</div>
						{/each}
						{#if status?.state === 'failed'}
							<span class="ml-4 text-sm text-destructive">Failed</span>
						{/if}
					</div>

					{#if status?.log}
						{#if status.state !== 'running'}
							<button
								onclick={() => logCollapsed = !logCollapsed}
								class="flex items-center gap-1 text-xs text-muted-foreground transition-colors hover:text-foreground"
							>
								<span class="inline-block transition-transform {logCollapsed ? '' : 'rotate-180'}">▾</span>
								{logCollapsed ? 'Show output' : 'Hide output'}
							</button>
						{/if}
						{#if status.state === 'running' || !logCollapsed}
							<pre bind:this={logEl} class="mt-3 max-h-64 overflow-auto rounded bg-secondary p-3 text-xs leading-relaxed">{formatLog(status.log)}</pre>
						{/if}
					{/if}

					{#if status?.state === 'failed'}
						<div class="mt-4 flex gap-2">
							<Button size="sm" onclick={doVersionSwitch}>Retry</Button>
							<Button variant="secondary" size="sm" onclick={() => status = { state: 'idle', log: '', reboot_required: false, webui_changed: false }}>Dismiss</Button>
						</div>
					{/if}
				</CardContent>
			</Card>
		{/if}
	{:else if activeTab === 'generations'}
		<Card>
			<CardContent class="py-5">
				<div class="mb-4 flex items-center justify-between">
					<div>
						<h2 class="text-base font-semibold">System Generations</h2>
						<p class="text-xs text-muted-foreground">Each successful rebuild creates a new generation. Switch back to any previous version or label known-good configurations.</p>
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
														onkeydown={(e) => {
															if (e.key === 'Enter') saveLabel(gen.generation);
															if (e.key === 'Escape') editingLabel = null;
														}}
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
													class="flex items-center gap-1 rounded-md border border-border px-2 py-0.5 text-xs text-foreground transition-colors hover:bg-accent"
												>
													<Tag class="h-3 w-3" />{gen.label}
												</button>
											{:else}
												<button
													onclick={() => startEditLabel(gen)}
													class="text-muted-foreground/50 transition-colors hover:text-muted-foreground"
													title="Add label"
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
														class="rounded-md p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
														title="Switch to this generation"
														disabled={status?.state === 'running'}
													>
														<ArrowRightLeft class="h-4 w-4" />
													</button>
													<button
														onclick={() => deleteGeneration(gen.generation)}
														class="rounded-md p-1 text-muted-foreground transition-colors hover:bg-destructive/10 hover:text-destructive"
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

		{#if generationStatusVisible}
			<Card class="mt-6">
				<CardContent class="py-5">
					<div class="mb-5 flex items-center">
						{#each genPhases as phase, i}
							{@const done = genCurrentPhase >= i}
							{@const active = status?.state === 'running' && genCurrentPhase === i - 1}
							{@const failed = status?.state === 'failed' && !done}
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
									<div class="mx-1 mb-3.5 h-px w-12 {genCurrentPhase > i ? 'bg-blue-500' : 'bg-border'}"></div>
								{/if}
							</div>
						{/each}
						{#if status?.state === 'failed'}
							<span class="ml-4 text-sm text-destructive">Failed</span>
						{/if}
					</div>

					{#if status?.log}
						{#if status.state !== 'running'}
							<button
								onclick={() => logCollapsed = !logCollapsed}
								class="flex items-center gap-1 text-xs text-muted-foreground transition-colors hover:text-foreground"
							>
								<span class="inline-block transition-transform {logCollapsed ? '' : 'rotate-180'}">▾</span>
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
	{:else if activeTab === 'firmware'}
		{#if firmwareLoading}
			<p class="text-muted-foreground">Checking firmware...</p>
		{:else if !firmwareAvailable}
			<Card>
				<CardContent class="py-5">
					<p class="text-muted-foreground">Firmware management is not available on this system (virtual machine detected).</p>
				</CardContent>
			</Card>
		{:else}
			<Card class="mb-4">
				<CardContent class="py-5">
					<div class="mb-4 flex items-center justify-between">
						<div>
							<h3 class="text-lg font-semibold">Firmware Updates</h3>
							<p class="text-sm text-muted-foreground">Manage device firmware via fwupd (LVFS).</p>
						</div>
						<Button size="sm" onclick={loadFirmware} disabled={firmwareLoading}>
							{firmwareLoading ? 'Checking...' : 'Check for Updates'}
						</Button>
					</div>

					{#if firmwareDevices.length === 0}
						<p class="text-sm text-muted-foreground">No firmware-capable devices detected.</p>
					{:else}
						<table class="w-full text-sm">
							<thead>
								<tr>
									<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Device</th>
									<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Vendor</th>
									<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Version</th>
									<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Update</th>
									<th class="w-px border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap">Actions</th>
								</tr>
							</thead>
							<tbody>
								{#each firmwareDevices as dev}
									<tr class="border-b border-border">
										<td class="p-3 font-semibold whitespace-nowrap">{dev.name}</td>
										<td class="p-3 text-muted-foreground whitespace-nowrap">{dev.vendor}</td>
										<td class="p-3 font-mono text-xs">{dev.version}</td>
										<td class="p-3">
											{#if dev.update_available}
												<Badge variant="default">{dev.update_version}</Badge>
												{#if dev.update_description}
													<span class="ml-2 text-xs text-muted-foreground">{dev.update_description}</span>
												{/if}
											{:else}
												<span class="text-xs text-muted-foreground">Up to date</span>
											{/if}
										</td>
										<td class="p-3">
											{#if dev.update_available}
												<Button size="xs" onclick={() => updateFirmware(dev.device_id)} disabled={firmwareUpdating[dev.device_id]}>
													{firmwareUpdating[dev.device_id] ? 'Updating...' : 'Update'}
												</Button>
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					{/if}
				</CardContent>
			</Card>
		{/if}
	{/if}
{/if}
