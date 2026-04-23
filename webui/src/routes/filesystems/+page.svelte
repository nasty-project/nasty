<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { formatBytes, formatPercent } from '$lib/format';
	import { withToast } from '$lib/toast.svelte';

	let pageTab = $state<'manage' | 'diagnostics'>(
		typeof window !== 'undefined' && window.location.hash === '#diagnostics' ? 'diagnostics' : 'manage'
	);
	import { confirm } from '$lib/confirm.svelte';
	import { confirmDangerous } from '$lib/confirm-dangerous.svelte';
	import type { Filesystem, FilesystemDevice, BlockDevice, DeviceState, ScrubStatus, ReconcileStatus, TieringProfile, TieringProfileId } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { RefreshCw } from '@lucide/svelte';

	let filesystems: Filesystem[] = $state([]);
	let devices: BlockDevice[] = $state([]);
	let wizardStep: 0 | 1 | 2 | 3 = $state(0); // 0=hidden, 1=name+devices, 2=profile, 3=review
	let loading = $state(true);

	// Wizard state
	let newName = $state('first');
	let selectedPaths: string[] = $state([]);
	let wizardProfile: TieringProfileId = $state('single');
	let replicas = $state(1);
	let compression = $state('');
	let showPartitions = $state(false);
	let erasureCode = $state(false);
	let versionUpgrade = $state('');
	let encryption = $state(false);
	let passphrase = $state('');
	let passphraseConfirm = $state('');
	let storeKey = $state(true);
	let dataChecksum = $state('');
	let metadataChecksum = $state('');
	let bucketSize = $state('');
	let encodedExtentMax = $state('');
	let degraded = $state(false);
	let verbose = $state(false);
	let mountFsck = $state(false);
	let journalFlushDisabled = $state(false);
	let journalFlushDelay = $state('');
	let ioScheduler = $state('');

	// Manual tiering state
	let manualLabels: Record<string, string> = $state({});
	let manualFgTarget = $state('');
	let manualMetaTarget = $state('');
	let manualBgTarget = $state('');
	let manualPromoteTarget = $state('');

	let expandedFs: string | null = $state(null);
	let editOptionsFs: string | null = $state(null);
	let editCompression = $state('');
	let editBgCompression = $state('');
	let addDeviceFs: string | null = $state(null);
	let addDevicePath = $state('');
	let addDeviceLabel = $state('');
	let showAddPartitions = $state(false);
	let editErasureCode = $state(false);
	let editDataChecksum = $state('');
	let editMetadataChecksum = $state('');
	let editVersionUpgrade = $state('');
	let editDataReplicas = $state(1);
	let editMetadataReplicas = $state(1);
	let editMoveIos = $state(32);
	let editMoveBytes = $state('');
	let unlockFs: string | null = $state(null);
	let unlockPassphrase = $state('');
	let editDegraded = $state(false);
	let editVerbose = $state(false);
	let editFsck = $state(false);
	let editJournalFlushDisabled = $state(false);
	let editJournalFlushDelay = $state('');
	let editIoScheduler = $state('');

	// Inline label editing: key is "fsName|devicePath"
	let editingLabel: string | null = $state(null);
	let editLabelValue = $state('');

	async function saveDeviceLabel(fsName: string, devicePath: string) {
		const key = `${fsName}|${devicePath}`;
		if (editingLabel !== key) return;
		editingLabel = null;
		const label = editLabelValue.trim();
		await withToast(
			() => client.call('fs.device.set_label', { filesystem: fsName, device: devicePath, label }),
			`Label updated for ${devicePath}`
		);
		await refresh();
	}

	function startEditLabel(fsName: string, dev: FilesystemDevice) {
		editingLabel = `${fsName}|${dev.path}`;
		editLabelValue = dev.label ?? '';
	}

	let healthFs: string | null = $state(null);
	let scrubStatus: ScrubStatus | null = $state(null);
	let reconcileStatus: ReconcileStatus | null = $state(null);
	let healthLoading = $state(false);

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'filesystem') refresh();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		await withToast(async () => {
			filesystems = await client.call<Filesystem[]>('fs.list');
			devices = await client.call<BlockDevice[]>('device.list');
		});
	}

	// ── Tiering profile logic ────────────────────────────────────

	function selectedDeviceObjects(): BlockDevice[] {
		return selectedPaths
			.map(p => devices.find(d => d.path === p))
			.filter(Boolean) as BlockDevice[];
	}

	function buildProfiles(): TieringProfile[] {
		const sel = selectedDeviceObjects();
		const hasNvme = sel.some(d => d.device_class === 'nvme');
		const hasSsd  = sel.some(d => d.device_class === 'ssd');
		const hasHdd  = sel.some(d => d.device_class === 'hdd');
		const hasFast = hasNvme || hasSsd;
		const hasSlow = hasHdd;
		const has3Tiers = hasNvme && (hasSsd || hasHdd);

		// Single label for single-tier: use filesystem name as group
		const singleLabels: Record<string, string> = {};
		sel.forEach(d => { singleLabels[d.path] = newName; });

		// Write-cache labels: fast = nvme/ssd → "fast", hdd → "slow"
		const wcLabels: Record<string, string> = {};
		sel.forEach(d => { wcLabels[d.path] = d.device_class === 'hdd' ? 'slow' : 'fast'; });

		// Full-tier labels by device class
		const ftLabels: Record<string, string> = {};
		sel.forEach(d => { ftLabels[d.path] = d.device_class; });

		// Full-tier targets
		let ftFg: string | null = null;
		let ftMeta: string | null = null;
		let ftBg: string | null = null;
		let ftPromote: string | null = null;
		if (hasNvme) {
			ftFg = 'nvme'; ftMeta = 'nvme';
			if (hasHdd) { ftBg = 'hdd'; if (hasSsd) ftPromote = 'ssd'; }
			else if (hasSsd) { ftBg = 'ssd'; }
		}

		const recommended = hasNvme && (hasSsd || hasHdd) ? 'full_tiering'
			: hasFast && hasSlow ? 'write_cache'
			: 'single';

		return [
			{
				id: 'single',
				name: 'Single Tier',
				tagline: 'Simple — all devices in one filesystem',
				description: 'All devices are treated as equal peers. bcachefs stripes data across them based on capacity. No performance tiers.',
				available: true,
				recommended: recommended === 'single',
				foreground_target: null,
				metadata_target: null,
				background_target: null,
				promote_target: null,
				device_labels: {},
			},
			{
				id: 'write_cache',
				name: 'Write Cache + Cold Storage',
				tagline: 'Writes land on fast devices, cold data migrates to slow',
				description: 'Writes go to the fast tier first (NVMe/SSD). Over time, background I/O migrates cold data to the slow tier (HDD), freeing fast space for new writes.',

				available: hasFast && hasSlow,
				recommended: recommended === 'write_cache',
				foreground_target: 'fast',
				metadata_target: 'fast',
				background_target: 'slow',
				promote_target: null,
				device_labels: wcLabels,
			},
			{
				id: 'full_tiering',
				name: 'Full Tiering',
				tagline: 'NVMe writes, SSD read cache, HDD cold storage',
				description: `NVMe handles all writes and metadata. Hot reads are served from the SSD read cache. Cold data moves to HDD in the background. Maximum performance with large capacity.${!has3Tiers ? ' (You can add SSD devices later to enable the read-cache tier.)' : ''}`,
				available: has3Tiers,
				recommended: recommended === 'full_tiering',
				foreground_target: ftFg,
				metadata_target: ftMeta,
				background_target: ftBg,
				promote_target: ftPromote,
				device_labels: ftLabels,
			},
			{
				id: 'none',
				name: 'No Tiering',
				tagline: 'No labels or targets — bcachefs default behavior',
				description: 'No device labels or IO targets are set. bcachefs will distribute data evenly across all devices using its built-in balancing. Useful when all devices are equivalent and you want the simplest possible setup.',
				available: true,
				recommended: false,
				foreground_target: null,
				metadata_target: null,
				background_target: null,
				promote_target: null,
				device_labels: {},
			},
			{
				id: 'manual',
				name: 'Manual',
				tagline: 'Set device labels and IO targets manually',
				description: 'Assign custom labels to each device and configure foreground, metadata, background, and promote targets manually. For advanced users who want full control over tiering behavior.',
				available: true,
				recommended: false,
				foreground_target: manualFgTarget || null,
				metadata_target: manualMetaTarget || null,
				background_target: manualBgTarget || null,
				promote_target: manualPromoteTarget || null,
				device_labels: { ...manualLabels },
			},
		];
	}

	function activeProfile(): TieringProfile {
		return buildProfiles().find(p => p.id === wizardProfile) ?? buildProfiles()[0];
	}

	function buildFormatCommand(): string[] {
		const profile = activeProfile();
		const args = ['bcachefs', 'format'];

		if (replicas > 1) args.push(`--replicas=${replicas}`);
		if (compression) args.push(`--compression=${compression}`);
		if (profile.foreground_target) args.push(`--foreground_target=${profile.foreground_target}`);
		if (profile.metadata_target) args.push(`--metadata_target=${profile.metadata_target}`);
		if (profile.background_target) args.push(`--background_target=${profile.background_target}`);
		if (profile.promote_target) args.push(`--promote_target=${profile.promote_target}`);
		if (encryption) args.push('--encrypted');
		if (erasureCode) args.push('--erasure_code');
		if (dataChecksum) args.push(`--data_checksum=${dataChecksum}`);
		if (metadataChecksum) args.push(`--metadata_checksum=${metadataChecksum}`);
		if (bucketSize) args.push(`--bucket=${bucketSize}`);
		if (encodedExtentMax) args.push(`--encoded_extent_max=${encodedExtentMax}`);

		const hasTargets = !!(profile.foreground_target || profile.metadata_target || profile.background_target || profile.promote_target);
		for (const path of selectedPaths) {
			const label = profile.device_labels[path];
			if (label) {
				args.push(`--label=${label}`);
			} else if (hasTargets) {
				args.push(`--label=${newName}`);
			}
			args.push(path);
		}

		return args;
	}

	function buildMountCommand(): string[] {
		const deviceArg = selectedPaths.join(':');
		const opts = ['prjquota'];
		if (versionUpgrade) opts.push(`version_upgrade=${versionUpgrade}`);
		if (degraded) opts.push('degraded');
		if (verbose) opts.push('verbose');
		if (mountFsck) opts.push('fsck');
		if (journalFlushDisabled) opts.push('journal_flush_disabled');
		if (journalFlushDelay) opts.push(`journal_flush_delay=${journalFlushDelay}`);
		return ['bcachefs', 'mount', '-o', opts.join(','), deviceArg, `/fs/${newName}`];
	}

	function formatCommandLines(args: string[]): string {
		if (args.length <= 4) return args.join(' ');
		const parts: string[] = [args[0] + ' ' + args[1]]; // "bcachefs format"
		for (let i = 2; i < args.length; i++) {
			const arg = args[i];
			if (arg.startsWith('--label=')) {
				// Per-device group: label + device path on one line
				const next = args[i + 1] && !args[i + 1].startsWith('--') ? ' ' + args[++i] : '';
				parts.push('  ' + arg + next);
			} else if (arg.startsWith('--')) {
				// Global option
				parts.push('  ' + arg);
			} else {
				// Bare device path (no label)
				parts.push('  ' + arg);
			}
		}
		return parts.join(' \\\n');
	}

	$effect(() => {
		if (erasureCode && replicas < 2) replicas = 2;
		if (erasureCode && selectedPaths.length < 3) erasureCode = false;
	});

	async function createFs() {
		if (!newName || selectedPaths.length === 0) return;
		if (erasureCode && selectedPaths.length < replicas + 1) return;
		if (encryption && (!passphrase || passphrase !== passphraseConfirm)) return;
		const profile = activeProfile();
		const ok = await withToast(
			() => client.call('fs.create', {
				name: newName,
				devices: selectedPaths.map(path => ({
					path,
					label: profile.device_labels[path] || undefined,
				})),
				replicas,
				compression: compression || undefined,
				foreground_target: profile.foreground_target || undefined,
				metadata_target: profile.metadata_target || undefined,
				background_target: profile.background_target || undefined,
				promote_target: profile.promote_target || undefined,
				erasure_code: erasureCode || undefined,
				encryption: encryption || undefined,
				passphrase: encryption ? passphrase : undefined,
				store_key: encryption ? storeKey : undefined,
				data_checksum: dataChecksum || undefined,
				metadata_checksum: metadataChecksum || undefined,
				bucket_size: bucketSize || undefined,
				encoded_extent_max: encodedExtentMax || undefined,
				version_upgrade: versionUpgrade || undefined,
				journal_flush_delay: journalFlushDelay ? parseInt(journalFlushDelay) : undefined,
				io_scheduler: ioScheduler || undefined,
			}),
			`Filesystem "${newName}" created`
		);
		if (ok !== undefined) {
			wizardStep = 0;
			newName = 'first';
			selectedPaths = [];
			wizardProfile = 'single';
			manualLabels = {};
			manualFgTarget = '';
			manualMetaTarget = '';
			manualBgTarget = '';
			manualPromoteTarget = '';
			erasureCode = false;
			versionUpgrade = '';
			encryption = false;
			passphrase = '';
			passphraseConfirm = '';
			storeKey = true;
			dataChecksum = '';
			metadataChecksum = '';
			bucketSize = '';
			encodedExtentMax = '';
			degraded = false;
			verbose = false;
			mountFsck = false;
			journalFlushDisabled = false;
			journalFlushDelay = '';
			ioScheduler = '';
			await refresh();
		}
	}

	function openWizard() {
		newName = 'first';
		selectedPaths = [];
		wizardProfile = 'single';
		replicas = 1;
		compression = '';
		showPartitions = false;
		manualLabels = {};
		manualFgTarget = '';
		manualMetaTarget = '';
		manualBgTarget = '';
		manualPromoteTarget = '';
		erasureCode = false;
		wizardStep = 1;
	}

	function wizardNext() {
		if (wizardStep === 1) {
			// Auto-select recommended profile
			const rec = buildProfiles().find(p => p.recommended && p.available);
			if (rec) wizardProfile = rec.id;
		}
		wizardStep = (wizardStep + 1) as 1 | 2 | 3;
	}

	async function destroyFs(name: string) {
		if (!await confirmDangerous(
			`Destroy Filesystem "${name}"`,
			`This will unmount the filesystem and wipe all device superblocks. Type the filesystem name to confirm.`,
			name,
		)) return;
		await withToast(
			() => client.call('fs.destroy', { name, confirm_name: name }),
			`Filesystem "${name}" destroyed`
		);
		await refresh();
	}

	let mountingFs = $state<string | null>(null);

	async function toggleMount(fs: Filesystem) {
		if (fs.mounted) {
			if (!await confirm(`Unmount Filesystem "${fs.name}"`, `Any active NFS, SMB, iSCSI, and NVMe-oF shares on this filesystem will be stopped first.`)) return;
		}
		const action = fs.mounted ? 'unmount' : 'mount';
		mountingFs = fs.name;
		await withToast(
			() => fs.mounted
				? client.call('fs.unmount', { name: fs.name }, 120000)
				: client.call('fs.mount', { name: fs.name }, 120000),
			`Filesystem "${fs.name}" ${action}ed`
		);
		mountingFs = null;
		await refresh();
	}

	async function addDevice(fsName: string) {
		if (!addDevicePath) return;
		const ok = await withToast(
			() => client.call('fs.device.add', {
				filesystem: fsName,
				device: {
					path: addDevicePath,
					label: addDeviceLabel || undefined,
				},
			}),
			`Device ${addDevicePath} added to "${fsName}"`
		);
		if (ok !== undefined) {
			addDeviceFs = null;
			addDevicePath = '';
			addDeviceLabel = '';
			await refresh();
		}
	}

	async function removeDevice(fsName: string, devicePath: string) {
		if (!await confirm(`Remove ${devicePath}?`, `Data will be evacuated from filesystem "${fsName}" first.`)) return;
		await withToast(
			() => client.call('fs.device.remove', { filesystem: fsName, device: devicePath }),
			`Device ${devicePath} removed from "${fsName}"`
		);
		await refresh();
	}

	async function evacuateDevice(fsName: string, devicePath: string) {
		if (!await confirm(`Evacuate all data from ${devicePath}?`)) return;
		await withToast(
			() => client.call('fs.device.evacuate', { filesystem: fsName, device: devicePath }),
			`Evacuating ${devicePath} — this may take several minutes`
		);
		await refresh();
	}

	async function setDeviceState(fsName: string, devicePath: string, state: DeviceState) {
		if (state === 'ro') {
			if (!await confirm(`Set ${devicePath} read-only?`, `The device will stop accepting writes. Use Set RW to revert.`)) return;
		}
		await withToast(
			() => client.call('fs.device.set_state', { filesystem: fsName, device: devicePath, state }),
			`Device ${devicePath} set to ${state}`
		);
		await refresh();
	}

	async function onlineDevice(fsName: string, devicePath: string) {
		await withToast(
			() => client.call('fs.device.online', { filesystem: fsName, device: devicePath }),
			`Device ${devicePath} online`
		);
		await refresh();
	}

	async function offlineDevice(fsName: string, devicePath: string) {
		if (!await confirm(`Take ${devicePath} offline?`)) return;
		await withToast(
			() => client.call('fs.device.offline', { filesystem: fsName, device: devicePath }),
			`Device ${devicePath} offline`
		);
		await refresh();
	}

	function openEditOptions(fs: Filesystem) {
		if (editOptionsFs === fs.name) {
			editOptionsFs = null;
			return;
		}
		editOptionsFs = fs.name;
		editCompression = fs.options.compression ?? '';
		editBgCompression = fs.options.background_compression ?? '';
	editErasureCode = fs.options.erasure_code ?? false;
		editDataChecksum = fs.options.data_checksum ?? 'none';
		editMetadataChecksum = fs.options.metadata_checksum ?? 'none';
		editVersionUpgrade = fs.options.version_upgrade ?? '';
		editDataReplicas = fs.options.data_replicas ?? 1;
		editMetadataReplicas = fs.options.metadata_replicas ?? 1;
		editMoveIos = fs.options.move_ios_in_flight ?? 32;
		editMoveBytes = fs.options.move_bytes_in_flight ?? '';
		editDegraded = fs.options.degraded ?? false;
		editVerbose = fs.options.verbose ?? false;
		editFsck = fs.options.fsck ?? false;
		editJournalFlushDisabled = fs.options.journal_flush_disabled ?? false;
		editJournalFlushDelay = fs.options.journal_flush_delay?.toString() ?? '';
		editIoScheduler = fs.options.io_scheduler ?? '';
	}

	async function doUnlock() {
		if (!unlockFs || !unlockPassphrase) return;
		const name = unlockFs;
		await withToast(
			() => client.call('fs.unlock', { name, passphrase: unlockPassphrase }),
			`Filesystem "${name}" unlocked`
		);
		unlockFs = null;
		unlockPassphrase = '';
		await refresh();
	}

	async function saveOptions(fsName: string) {
		await withToast(
			() => client.call('fs.options.update', {
				name: fsName,
				compression: editCompression || 'none',
				background_compression: editBgCompression || 'none',
				erasure_code: editErasureCode,
				data_checksum: editDataChecksum || 'none',
				metadata_checksum: editMetadataChecksum || 'none',
				data_replicas: editDataReplicas,
				metadata_replicas: editMetadataReplicas,
				move_ios_in_flight: editMoveIos,
				move_bytes_in_flight: editMoveBytes || undefined,
				version_upgrade: editVersionUpgrade || undefined,
				degraded: editDegraded || undefined,
				verbose: editVerbose || undefined,
				fsck: editFsck || undefined,
				journal_flush_disabled: editJournalFlushDisabled || undefined,
				journal_flush_delay: editJournalFlushDelay ? parseInt(editJournalFlushDelay) : undefined,
				io_scheduler: editIoScheduler || undefined,
			}, 60_000),
			`Options updated for "${fsName}"`
		);
		editOptionsFs = null;
		await refresh();
	}

	// Auto-load health data when filesystem details are expanded
	$effect(() => {
		const fs = expandedFs;
		if (fs) {
			healthFs = fs;
			refreshHealth(fs);
		} else {
			healthFs = null;
			scrubStatus = null;
			reconcileStatus = null;
		}
	});

	async function refreshHealth(fsName: string) {
		healthLoading = true;
		try {
			[scrubStatus, reconcileStatus] = await Promise.all([
				client.call<ScrubStatus>('fs.scrub.status', { name: fsName }),
				client.call<ReconcileStatus>('fs.reconcile.status', { name: fsName }),
			]);
		} catch {
			// Individual calls may fail
		}
		healthLoading = false;
	}

	async function startScrub(fsName: string) {
		await withToast(
			() => client.call('fs.scrub.start', { name: fsName }),
			`Scrub started on "${fsName}"`
		);
		await refreshHealth(fsName);
	}

	function toggleDevice(path: string) {
		if (selectedPaths.includes(path)) {
			selectedPaths = selectedPaths.filter(p => p !== path);
		} else {
			selectedPaths = [...selectedPaths, path];
		}
		if (selectedPaths.length <= 1) { replicas = 1; erasureCode = false; }
		else if (erasureCode && selectedPaths.length < replicas + 1) erasureCode = false;
	}

	function availableDevices(): BlockDevice[] {
		return devices.filter(d => !d.in_use && (showPartitions || d.dev_type !== 'part'));
	}

	function availableDevicesForAdd(): BlockDevice[] {
		return devices.filter(d => !d.in_use && (showAddPartitions || d.dev_type !== 'part'));
	}

	function devDisplayState(dev: FilesystemDevice): string | null {
		return dev.state;
	}

	function stateColor(state: string | null): string {
		switch (state) {
			case 'rw': return 'bg-green-950 text-green-400';
			case 'ro': return 'bg-blue-950 text-blue-400';
			case 'failed': return 'bg-red-950 text-red-400';
			case 'spare': return 'bg-amber-950 text-amber-400';
			case 'evacuating': return 'bg-yellow-950 text-yellow-400 animate-pulse';
			case 'evacuated': return 'bg-teal-950 text-teal-400';
			default: return 'bg-secondary text-muted-foreground';
		}
	}

	function classColor(cls: string): string {
		switch (cls) {
			case 'nvme': return 'bg-violet-950 text-violet-300';
			case 'ssd':  return 'bg-blue-950 text-blue-300';
			case 'hdd':  return 'bg-amber-950 text-amber-300';
			default:     return 'bg-secondary text-muted-foreground';
		}
	}
</script>


<!-- Page-level tabs -->
<div class="mb-4 flex items-center gap-4 border-b border-border">
	<button
		onclick={() => { pageTab = 'manage'; history.replaceState(null, '', '#manage'); }}
		class="px-3 py-2 text-sm font-medium transition-colors border-b-2 -mb-px
			{pageTab === 'manage' ? 'border-primary text-foreground' : 'border-transparent text-muted-foreground hover:text-foreground'}"
	>Manage</button>
	<button
		onclick={() => { pageTab = 'diagnostics'; history.replaceState(null, '', '#diagnostics'); }}
		class="px-3 py-2 text-sm font-medium transition-colors border-b-2 -mb-px
			{pageTab === 'diagnostics' ? 'border-primary text-foreground' : 'border-transparent text-muted-foreground hover:text-foreground'}"
	>Diagnostics</button>
</div>

{#if pageTab === 'diagnostics'}
	{#await import('$lib/components/BcachefsDiagnostics.svelte') then module}
		<module.default />
	{/await}
{:else}

<div class="mb-4">
	<Button size="sm" onclick={() => wizardStep === 0 ? openWizard() : (wizardStep = 0)}>
		{wizardStep !== 0 ? 'Cancel' : 'Create Filesystem'}
	</Button>
</div>

{#if wizardStep !== 0}
	<Card class="mb-6 max-w-4xl">
		<CardContent class="pt-6">
			<!-- Step indicator -->
			<div class="mb-6 flex items-center gap-0">
				{#each [['1', 'Devices'], ['2', 'Tiering'], ['3', 'Review']] as [num, label], i}
					<div class="flex items-center">
						<div class="flex items-center gap-2">
							<div class="flex h-6 w-6 items-center justify-center rounded-full text-xs font-semibold
								{wizardStep > i + 1 ? 'bg-primary text-primary-foreground' :
								 wizardStep === i + 1 ? 'bg-primary text-primary-foreground' :
								 'bg-secondary text-muted-foreground'}">
								{num}
							</div>
							<span class="text-xs {wizardStep === i + 1 ? 'text-foreground font-medium' : 'text-muted-foreground'}">{label}</span>
						</div>
						{#if i < 2}
							<div class="mx-3 h-px w-8 bg-border"></div>
						{/if}
					</div>
				{/each}
			</div>

			<!-- Step 1: Name + Devices -->
			{#if wizardStep === 1}
				<div class="mb-4">
					<Label for="fs-name">Filesystem Name</Label>
					<Input id="fs-name" bind:value={newName} class="mt-1 max-w-xs" />
				</div>
				<div class="mb-4">
					<div class="mb-2 flex items-center justify-between">
						<Label>Select Devices</Label>
						<label class="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
							<input type="checkbox" bind:checked={showPartitions} class="h-3.5 w-3.5" />
							Show partitions
						</label>
					</div>
					{#if availableDevices().length === 0}
						<p class="text-sm text-muted-foreground">No available devices. You may need to prepare your disks first.</p>
						<Button size="sm" class="mt-2" onclick={() => window.location.href = '/disks'}>Go to Disks</Button>
					{:else}
						<div class="space-y-1.5">
							{#each availableDevices() as dev}
								{#if dev.dev_type === 'free'}
									{@const diskPath = dev.path.replace(':free', '')}
									{@const existingParts = devices.filter(d => d.dev_type === 'part' && d.path.startsWith(diskPath))}
									<div class="rounded-lg border border-border overflow-hidden">
										<label class="flex cursor-pointer items-center gap-3 px-3 py-2 text-sm
											{selectedPaths.includes(dev.path) ? 'border-primary bg-primary/5' : 'hover:bg-secondary/50'}">
											<input type="checkbox" checked={selectedPaths.includes(dev.path)}
												onchange={() => toggleDevice(dev.path)} class="h-4 w-4 shrink-0" />
											<span class="font-mono text-xs shrink-0">{diskPath}</span>
											<span class="rounded bg-green-900 px-1.5 py-0.5 text-[10px] font-semibold uppercase text-green-300">free space</span>
											<span class="text-muted-foreground">{formatBytes(dev.size_bytes)}</span>
											<span class="text-xs text-muted-foreground">(new partition will be created)</span>
										</label>
										{#if existingParts.length > 0}
											<div class="border-t border-border bg-muted/20 px-3 py-1.5">
												{#each existingParts as part}
													<div class="flex items-center gap-2 text-xs text-muted-foreground/60 py-0.5">
														<span class="font-mono">{part.path}</span>
														<span>{formatBytes(part.size_bytes)}</span>
														{#if part.mount_point}<span>mounted at {part.mount_point}</span>{/if}
														{#if part.fs_type}<span class="font-mono">{part.fs_type}</span>{/if}
														<span class="italic">not touched</span>
													</div>
												{/each}
											</div>
										{/if}
									</div>
								{:else}
									<label class="flex cursor-pointer items-center gap-3 rounded-lg border border-border px-3 py-2 text-sm
										{selectedPaths.includes(dev.path) ? 'border-primary bg-primary/5' : 'hover:bg-secondary/50'}">
										<input type="checkbox" checked={selectedPaths.includes(dev.path)}
											onchange={() => toggleDevice(dev.path)} class="h-4 w-4 shrink-0" />
										<span class="font-mono text-xs shrink-0">{dev.path}</span>
										<span class="rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">
											{dev.device_class}
										</span>
										<span class="text-muted-foreground">{formatBytes(dev.size_bytes)}</span>
										{#if dev.fs_type}
											<span class="rounded border border-amber-700 px-1.5 py-0.5 text-[10px] text-amber-400">has signatures · wipe first</span>
										{/if}
									</label>
								{/if}
							{/each}
						</div>
					{/if}
				</div>
				<div class="flex gap-2">
					<Button size="sm" onclick={wizardNext} disabled={!newName || selectedPaths.length === 0}>
						Next: Choose Tiering →
					</Button>
				</div>

			<!-- Step 2: Tiering Profile -->
			{:else if wizardStep === 2}
				{@const profiles = buildProfiles()}
				<div class="mb-4 space-y-3">
					{#each profiles as profile}
						<button
							disabled={!profile.available}
							onclick={() => { if (profile.available) wizardProfile = profile.id; }}
							class="w-full rounded-lg border-2 px-4 py-3 text-left transition-colors
								{!profile.available ? 'cursor-not-allowed border-border opacity-40' :
								 wizardProfile === profile.id ? 'border-primary bg-primary/5' :
								 'border-border hover:border-primary/50 hover:bg-secondary/30'}">
							<div class="flex items-center justify-between">
								<div class="flex items-center gap-2">
									<div class="h-4 w-4 rounded-full border-2 flex items-center justify-center
										{wizardProfile === profile.id && profile.available ? 'border-primary' : 'border-muted-foreground'}">
										{#if wizardProfile === profile.id && profile.available}
											<div class="h-2 w-2 rounded-full bg-primary"></div>
										{/if}
									</div>
									<span class="font-semibold text-sm">{profile.name}</span>
									{#if profile.recommended && profile.available}
										<span class="rounded bg-primary/20 px-1.5 py-0.5 text-[10px] font-semibold text-primary uppercase">recommended</span>
									{/if}
								</div>
							</div>
							<p class="mt-1 ml-6 text-xs text-muted-foreground">{profile.tagline}</p>
							{#if wizardProfile === profile.id && profile.available}
								<p class="mt-2 ml-6 text-xs text-foreground/80">{profile.description}</p>
								<!-- Tier diagram -->
								<div class="mt-3 ml-6">
									{#if profile.id === 'single'}
										<div class="flex flex-wrap gap-2">
											{#each selectedDeviceObjects() as dev}
												<div class="flex items-center gap-1.5 rounded border border-border px-2 py-1">
													<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
													<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
												</div>
											{/each}
										</div>
									{:else if profile.id === 'write_cache'}
										<div class="flex items-start gap-6">
											<div>
												<div class="mb-1 text-[10px] text-muted-foreground uppercase tracking-wide">Fast (writes + metadata)</div>
												<div class="flex flex-col gap-1">
													{#each selectedDeviceObjects().filter(d => d.device_class !== 'hdd') as dev}
														<div class="flex items-center gap-1.5 rounded border border-blue-800/50 bg-blue-950/30 px-2 py-1">
															<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
															<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
														</div>
													{/each}
												</div>
											</div>
											<div class="mt-4 text-muted-foreground">→</div>
											<div>
												<div class="mb-1 text-[10px] text-muted-foreground uppercase tracking-wide">Slow (cold data)</div>
												<div class="flex flex-col gap-1">
													{#each selectedDeviceObjects().filter(d => d.device_class === 'hdd') as dev}
														<div class="flex items-center gap-1.5 rounded border border-amber-800/50 bg-amber-950/30 px-2 py-1">
															<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
															<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
														</div>
													{/each}
												</div>
											</div>
										</div>
									{:else if profile.id === 'full_tiering'}
										<div class="flex items-start gap-4">
											{#each [['nvme', 'Writes + Metadata', 'border-violet-800/50 bg-violet-950/30'],
											         ['ssd', 'Read Cache', 'border-blue-800/50 bg-blue-950/30'],
											         ['hdd', 'Cold Storage', 'border-amber-800/50 bg-amber-950/30']] as [cls, role, colors]}
												{@const devs = selectedDeviceObjects().filter(d => d.device_class === cls)}
												{#if devs.length > 0}
													<div>
														<div class="mb-1 text-[10px] text-muted-foreground uppercase tracking-wide">{role}</div>
														<div class="flex flex-col gap-1">
															{#each devs as dev}
																<div class="flex items-center gap-1.5 rounded border {colors} px-2 py-1">
																	<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(cls)}">{cls}</span>
																	<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
																</div>
															{/each}
														</div>
													</div>
												{/if}
											{/each}
										</div>
									{:else if profile.id === 'none'}
									<div class="flex flex-wrap gap-2">
										{#each selectedDeviceObjects() as dev}
											<div class="flex items-center gap-1.5 rounded border border-border px-2 py-1">
												<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
												<span class="font-mono text-[10px] text-muted-foreground">{dev.path}</span>
											</div>
										{/each}
									</div>
								{:else if profile.id === 'manual'}
									<div class="space-y-3">
										<div>
											<div class="mb-1.5 text-[10px] uppercase tracking-wide text-muted-foreground">Device Labels</div>
											<div class="space-y-1.5">
												{#each selectedDeviceObjects() as dev}
													<div class="flex items-center gap-2">
														<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
														<span class="w-28 font-mono text-[10px] text-muted-foreground shrink-0">{dev.path}</span>
														<input
															type="text"
															value={manualLabels[dev.path] ?? ''}
															oninput={(e) => { manualLabels = { ...manualLabels, [dev.path]: (e.target as HTMLInputElement).value }; }}
															placeholder="label (e.g. fast, slow)"
															class="h-7 flex-1 rounded border border-input bg-transparent px-2 text-xs"
														/>
													</div>
												{/each}
											</div>
										</div>
										<div class="grid grid-cols-2 gap-2">
											<div>
												<div class="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">Foreground Target</div>
												<input type="text" bind:value={manualFgTarget} placeholder="label or empty" class="h-7 w-full rounded border border-input bg-transparent px-2 text-xs" />
											</div>
											<div>
												<div class="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">Metadata Target</div>
												<input type="text" bind:value={manualMetaTarget} placeholder="label or empty" class="h-7 w-full rounded border border-input bg-transparent px-2 text-xs" />
											</div>
											<div>
												<div class="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">Background Target</div>
												<input type="text" bind:value={manualBgTarget} placeholder="label or empty" class="h-7 w-full rounded border border-input bg-transparent px-2 text-xs" />
											</div>
											<div>
												<div class="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">Promote Target</div>
												<input type="text" bind:value={manualPromoteTarget} placeholder="label or empty" class="h-7 w-full rounded border border-input bg-transparent px-2 text-xs" />
											</div>
										</div>
									</div>
								{/if}
								</div>
							{/if}
						</button>
					{/each}
				</div>
				<div class="flex gap-2">
					<Button variant="secondary" size="sm" onclick={() => wizardStep = 1}>← Back</Button>
					<Button size="sm" onclick={wizardNext}>Next: Review →</Button>
				</div>

			<!-- Step 3: Review + Options -->
			{:else if wizardStep === 3}
				{@const profile = activeProfile()}
				<div class="mb-5 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
					<span class="text-muted-foreground">Name</span>
					<span class="font-mono">{newName}</span>
					<span class="text-muted-foreground">Devices</span>
					<div class="flex flex-wrap gap-1.5">
						{#each selectedDeviceObjects() as dev}
							<span class="flex items-center gap-1 rounded border border-border px-1.5 py-0.5 text-xs">
								{#if dev.dev_type === 'free'}
									<span class="rounded bg-green-900 px-1 py-0.5 text-[10px] font-semibold uppercase text-green-300">free</span>
									<span class="font-mono">{dev.path.replace(':free', '')} (new partition)</span>
								{:else}
									<span class="rounded px-1 py-0.5 text-[10px] font-semibold uppercase {classColor(dev.device_class)}">{dev.device_class}</span>
									<span class="font-mono">{dev.path}</span>
								{/if}
								{#if profile.device_labels[dev.path]}
									<span class="text-muted-foreground">→ {profile.device_labels[dev.path]}</span>
								{/if}
							</span>
						{/each}
					</div>
					<span class="text-muted-foreground">Tiering</span>
					<span>{profile.name}</span>
					{#if profile.foreground_target}
						<span class="text-muted-foreground">Foreground Target</span><span>{profile.foreground_target}</span>
					{/if}
					{#if profile.metadata_target}
						<span class="text-muted-foreground">Metadata Target</span><span>{profile.metadata_target}</span>
					{/if}
					{#if profile.background_target}
						<span class="text-muted-foreground">Background Target</span><span>{profile.background_target}</span>
					{/if}
					{#if profile.promote_target}
						<span class="text-muted-foreground">Promote Target</span><span>{profile.promote_target}</span>
					{/if}
					</div>

				<div class="mb-5 grid grid-cols-2 gap-4">
					<div>
						<Label for="replicas">Replicas</Label>
						<select id="replicas" bind:value={replicas} disabled={selectedPaths.length <= 1 || erasureCode}
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							{#if !erasureCode}
								<option value={1}>1 (no redundancy)</option>
							{/if}
							<option value={2}>2{erasureCode ? ' (RAID-5)' : ' (mirrored)'}</option>
							{#if selectedPaths.length >= 4 || !erasureCode}
								<option value={3}>3{erasureCode ? ' (RAID-6)' : ''}</option>
							{/if}
						</select>
						{#if selectedPaths.length <= 1}
							<span class="text-xs text-muted-foreground">Requires multiple devices</span>
						{:else if erasureCode}
							<span class="text-xs text-muted-foreground">Set by erasure coding</span>
						{/if}
					</div>
					<div>
						<Label for="compression">Compression</Label>
						<select id="compression" bind:value={compression}
							class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value="">None</option>
							<option value="lz4">LZ4</option>
							<option value="zstd">Zstd</option>
							<option value="gzip">Gzip</option>
						</select>
					</div>
				</div>

				{#if selectedPaths.length >= 3}
				<div class="mb-5">
					<label class="flex cursor-pointer items-center gap-2 text-sm">
						<input type="checkbox" bind:checked={erasureCode} disabled={selectedPaths.length < 3} class="h-4 w-4" />
						<span class="font-medium">Erasure Coding</span>
						<span class="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-amber-400">Tech Preview</span>
						{#if erasureCode}
							<span class="text-xs text-amber-400">({replicas === 2 ? 'RAID-5' : 'RAID-6'}, {replicas}+1 across {selectedPaths.length} devices)</span>
						{:else}
							<span class="text-xs text-muted-foreground">(Reed-Solomon parity, requires 3+ devices)</span>
						{/if}
					</label>
					{#if erasureCode}
						<p class="mt-1 ml-6 text-xs text-muted-foreground">Data is written as {replicas} replicas, then converted to parity stripes in the background. Needs {replicas + 1}+ devices.</p>
						{#if selectedPaths.length < replicas + 1}
							<p class="mt-1 ml-6 text-xs text-destructive">Not enough devices: need at least {replicas + 1} for {replicas === 2 ? 'RAID-5' : 'RAID-6'} (have {selectedPaths.length}).</p>
						{/if}
					{/if}
				</div>
				{/if}

				<!-- Encryption -->
				<div class="mb-5 rounded-lg border border-border p-4">
					<label class="flex cursor-pointer items-center gap-2 text-sm font-medium">
						<input type="checkbox" bind:checked={encryption} class="h-4 w-4" />
						Encrypt filesystem
					</label>
					{#if encryption}
						<div class="mt-3 grid grid-cols-2 gap-4">
							<div>
								<Label for="passphrase">Passphrase</Label>
								<input id="passphrase" type="password" bind:value={passphrase}
									class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm"
									placeholder="Enter passphrase" />
							</div>
							<div>
								<Label for="passphrase-confirm">Confirm</Label>
								<input id="passphrase-confirm" type="password" bind:value={passphraseConfirm}
									class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm"
									placeholder="Confirm passphrase" />
							</div>
						</div>
						{#if passphrase && passphraseConfirm && passphrase !== passphraseConfirm}
							<p class="mt-1 text-xs text-destructive">Passphrases do not match.</p>
						{/if}
						<label class="mt-3 flex cursor-pointer items-center gap-2 text-sm">
							<input type="checkbox" bind:checked={storeKey} class="h-4 w-4" />
							Store key for auto-unlock on boot
						</label>
						<p class="mt-1 text-xs text-muted-foreground">
							{#if storeKey}
								Key stored on boot drive. Filesystem auto-unlocks on boot. Protects data at rest against drive theft.
							{:else}
								Passphrase required after every reboot via WebUI. More secure but requires manual intervention.
							{/if}
						</p>
						<p class="mt-2 text-xs text-amber-400">Warning: losing the passphrase with no stored key means permanent data loss.</p>
					{:else}
						<p class="mt-1 text-xs text-muted-foreground">Data at rest will not be encrypted.</p>
					{/if}
				</div>

				<!-- Advanced format options -->
				<details class="mb-5">
					<summary class="cursor-pointer text-sm text-muted-foreground hover:text-foreground">Advanced options</summary>
					<p class="mt-2 text-xs text-amber-400">Defaults are recommended for most setups. Only change these if you understand their impact.</p>
					<div class="mt-3 flex flex-wrap gap-4">
						<div class="flex-1 min-w-[140px]">
							<Label for="version-upgrade">Version Upgrade</Label>
							<select id="version-upgrade" bind:value={versionUpgrade}
								class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">None (don't upgrade)</option>
								<option value="compatible">Compatible</option>
								<option value="incompatible">Incompatible (latest)</option>
							</select>
						</div>
						<div class="flex-1 min-w-[140px]">
							<Label for="data-checksum">Data Checksum</Label>
							<select id="data-checksum" bind:value={dataChecksum}
								class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">Default (crc32c)</option>
								<option value="crc32c">CRC32C</option>
								<option value="crc64">CRC64</option>
								<option value="xxhash">xxHash</option>
								<option value="none">None</option>
							</select>
						</div>
						<div class="flex-1 min-w-[140px]">
							<Label for="meta-checksum">Metadata Checksum</Label>
							<select id="meta-checksum" bind:value={metadataChecksum}
								class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">Default (crc32c)</option>
								<option value="crc32c">CRC32C</option>
								<option value="crc64">CRC64</option>
								<option value="xxhash">xxHash</option>
								<option value="none">None</option>
							</select>
						</div>
						<div class="flex-1 min-w-[140px]">
							<Label for="bucket-size">Bucket Size</Label>
							<select id="bucket-size" bind:value={bucketSize}
								class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">Default</option>
								<option value="256k">256 KiB</option>
								<option value="512k">512 KiB</option>
								<option value="1M">1 MiB</option>
								<option value="2M">2 MiB</option>
							</select>
						</div>
						<div class="flex-1 min-w-[140px]">
							<Label for="extent-max">Max Encoded Extent</Label>
							<select id="extent-max" bind:value={encodedExtentMax}
								class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">Default</option>
								<option value="64k">64 KiB</option>
								<option value="128k">128 KiB</option>
								<option value="256k">256 KiB</option>
								<option value="512k">512 KiB</option>
							</select>
						</div>
					</div>
					<div class="mt-3 flex flex-wrap gap-4">
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<input type="checkbox" bind:checked={degraded} class="h-4 w-4" />
							Degraded mode
						</label>
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<input type="checkbox" bind:checked={mountFsck} class="h-4 w-4" />
							Fsck on mount
						</label>
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<input type="checkbox" bind:checked={verbose} class="h-4 w-4" />
							Verbose
						</label>
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<input type="checkbox" bind:checked={journalFlushDisabled} class="h-4 w-4" />
							Disable journal flush
						</label>
					</div>
					<div class="mt-3 grid grid-cols-2 gap-3">
						<div>
							<Label class="text-xs">Journal flush delay (µs)</Label>
							<Input type="number" bind:value={journalFlushDelay} placeholder="1000" class="mt-1 h-8 text-xs" />
						</div>
						<div>
							<Label class="text-xs">I/O scheduler</Label>
							<select bind:value={ioScheduler} class="mt-1 h-8 w-full rounded-md border border-input bg-background px-2 text-xs">
								<option value="">Default</option>
								<option value="none">none (recommended for SSDs)</option>
								<option value="mq-deadline">mq-deadline</option>
								<option value="kyber">kyber</option>
							</select>
						</div>
					</div>
					<p class="mt-2 text-xs text-muted-foreground">Checksum and bucket size are set at format time. Mount options can be changed later via Edit Options.</p>
				</details>

				<div class="mb-5">
					<Label>Commands</Label>
					<pre class="mt-1 rounded-md border border-border bg-black/40 p-3 text-xs font-mono text-muted-foreground overflow-x-auto whitespace-pre-wrap">{formatCommandLines(buildFormatCommand())}

{buildMountCommand().join(' ')}</pre>
				</div>

				<div class="flex gap-2">
					<Button variant="secondary" size="sm" onclick={() => wizardStep = 2}>← Back</Button>
					<Button size="sm" onclick={createFs} disabled={(erasureCode && selectedPaths.length < replicas + 1) || (encryption && (!passphrase || passphrase !== passphraseConfirm))}>Create Filesystem</Button>
				</div>
			{/if}
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if filesystems.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">No filesystems configured yet.</p>
		<p class="mt-1 text-sm text-muted-foreground">Use the <strong>Create Filesystem</strong> button above to get started.</p>
		<Button size="sm" class="mt-2" onclick={() => window.location.href = '/disks'}>Prepare Disks</Button>
	</div>
{:else}
	{#each filesystems as fs}
		<Card class="mb-4">
			<CardContent class="pt-4">
				<div class="flex flex-wrap items-center justify-between gap-4">
					<div class="flex cursor-pointer items-center gap-3" role="button" tabindex="0"
						onclick={() => expandedFs = expandedFs === fs.name ? null : fs.name}
						onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') expandedFs = expandedFs === fs.name ? null : fs.name; }}>
						<strong class="text-lg">{fs.name}</strong>
						<Badge variant={fs.mounted ? 'default' : 'destructive'}>
							{fs.mounted ? 'Mounted' : fs.options.locked ? 'Locked' : 'Unmounted'}
						</Badge>
						{#if fs.mounted && fs.mount_point}
							<span class="font-mono text-xs text-muted-foreground">{fs.mount_point}</span>
						{/if}
					</div>
					<div class="flex gap-2">
						<Button variant="secondary" size="xs" onclick={() => expandedFs = expandedFs === fs.name ? null : fs.name}>
							{expandedFs === fs.name ? 'Hide Details' : 'Details'}
						</Button>
						{#if fs.mounted}
							<Button variant="secondary" size="xs" onclick={() => openEditOptions(fs)}>
								{editOptionsFs === fs.name ? 'Hide Options' : 'Options'}
							</Button>
						{/if}
						{#if fs.options.encrypted && fs.options.locked}
							<Button variant="default" size="xs" onclick={() => { unlockFs = fs.name; unlockPassphrase = ''; }}>
								Unlock
							</Button>
						{/if}
						<Button variant="secondary" size="xs" onclick={() => toggleMount(fs)}
							disabled={mountingFs === fs.name || (fs.options.encrypted && fs.options.locked && !fs.mounted)}>
							{mountingFs === fs.name ? (fs.mounted ? 'Unmounting...' : 'Mounting...') : (fs.mounted ? 'Unmount' : 'Mount')}
						</Button>
						{#if fs.options.encrypted && fs.options.key_stored}
							<Button variant="secondary" size="xs" onclick={async () => {
								const key = await client.call<string>('fs.key.export', { name: fs.name });
								const blob = new Blob([key], { type: 'text/plain' });
								const a = document.createElement('a');
								a.href = URL.createObjectURL(blob);
								a.download = `${fs.name}.key`;
								a.click();
							}}>
								Export Key
							</Button>
						{/if}
						<Button variant="destructive" size="xs" onclick={() => destroyFs(fs.name)}>Destroy</Button>
					</div>
				</div>

				{#if fs.total_bytes > 0}
					<div class="mt-3">
						<div class="mb-1 h-1.5 overflow-hidden rounded-full bg-secondary">
							<div class="h-full rounded-full bg-primary" style="width: {(fs.used_bytes / fs.total_bytes) * 100}%"></div>
						</div>
						<span class="text-xs text-muted-foreground">
							{formatBytes(fs.used_bytes)} / {formatBytes(fs.total_bytes)} ({formatPercent(fs.used_bytes, fs.total_bytes)})
							{#if fs.options.data_replicas && fs.options.data_replicas > 1} · {fs.options.data_replicas} replicas{/if}
							{#if fs.options.compression} · {fs.options.compression}{/if}
						</span>
					</div>
				{/if}

				{#if editOptionsFs === fs.name}
				<div class="mt-4 border-t border-border pt-4">
					<h4 class="mb-4 text-xs uppercase tracking-wide text-muted-foreground">Edit Options</h4>
					<div class="grid grid-cols-1 gap-5 sm:grid-cols-2">
						<!-- Data Protection -->
						<fieldset class="rounded-md border border-border p-3">
							<legend class="px-1.5 text-[0.65rem] uppercase tracking-wide text-muted-foreground">Data Protection</legend>
							<div class="grid grid-cols-2 gap-3">
								<div>
									<label for="edit-data-replicas-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Data Replicas</label>
									<input id="edit-data-replicas-{fs.name}" type="number" min="1" max="4" bind:value={editDataReplicas} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm" />
								</div>
								<div>
									<label for="edit-meta-replicas-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Metadata Replicas</label>
									<input id="edit-meta-replicas-{fs.name}" type="number" min="1" max="4" bind:value={editMetadataReplicas} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm" />
								</div>
							</div>
							<label class="mt-2 flex cursor-pointer items-center gap-2 text-sm">
								<input id="edit-erasure-{fs.name}" type="checkbox" bind:checked={editErasureCode} class="h-4 w-4" />
								<span class="text-xs">Erasure coding</span>
								<span class="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-amber-400">Tech Preview</span>
							</label>
							<div class="mt-3 grid grid-cols-2 gap-3">
								<div>
									<label for="edit-data-checksum-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Data Checksum</label>
									<select id="edit-data-checksum-{fs.name}" bind:value={editDataChecksum} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
										<option value="none">None</option>
										<option value="crc32c">CRC32C</option>
										<option value="crc64">CRC64</option>
										<option value="xxhash">xxHash</option>
									</select>
								</div>
								<div>
									<label for="edit-meta-checksum-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Metadata Checksum</label>
									<select id="edit-meta-checksum-{fs.name}" bind:value={editMetadataChecksum} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
										<option value="none">None</option>
										<option value="crc32c">CRC32C</option>
										<option value="crc64">CRC64</option>
										<option value="xxhash">xxHash</option>
									</select>
								</div>
							</div>
						</fieldset>
						<!-- Compression -->
						<fieldset class="rounded-md border border-border p-3">
							<legend class="px-1.5 text-[0.65rem] uppercase tracking-wide text-muted-foreground">Compression</legend>
							<div class="grid grid-cols-2 gap-3">
								<div>
									<label for="edit-compression-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Foreground</label>
									<select id="edit-compression-{fs.name}" bind:value={editCompression} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
										<option value="">None</option>
										<option value="lz4">LZ4</option>
										<option value="zstd">Zstd</option>
										<option value="gzip">Gzip</option>
									</select>
								</div>
								<div>
									<label for="edit-bg-compression-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Background</label>
									<select id="edit-bg-compression-{fs.name}" bind:value={editBgCompression} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
										<option value="">None</option>
										<option value="lz4">LZ4</option>
										<option value="zstd">Zstd</option>
										<option value="gzip">Gzip</option>
									</select>
								</div>
							</div>
						</fieldset>
						<!-- Background Mover -->
						<fieldset class="rounded-md border border-border p-3">
							<legend class="px-1.5 text-[0.65rem] uppercase tracking-wide text-muted-foreground">Background Mover</legend>
							<div class="grid grid-cols-2 gap-3">
								<div>
									<label for="edit-move-ios-{fs.name}" class="mb-1 block text-xs text-muted-foreground">IOs in Flight</label>
									<input id="edit-move-ios-{fs.name}" type="number" min="1" max="256" bind:value={editMoveIos} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm" />
								</div>
								<div>
									<label for="edit-move-bytes-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Bytes in Flight</label>
									<input id="edit-move-bytes-{fs.name}" type="text" placeholder="e.g. 8.0M" bind:value={editMoveBytes} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm" />
								</div>
							</div>
						</fieldset>
						<!-- Mount Options -->
						<fieldset class="rounded-md border border-border p-3">
							<legend class="px-1.5 text-[0.65rem] uppercase tracking-wide text-muted-foreground">Mount Options</legend>
							<div>
								<label for="edit-vu-{fs.name}" class="mb-1 block text-xs text-muted-foreground">Version Upgrade</label>
								<select id="edit-vu-{fs.name}" bind:value={editVersionUpgrade} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
									<option value="">None</option>
									<option value="compatible">Compatible</option>
									<option value="incompatible">Incompatible</option>
								</select>
							</div>
							<div class="mt-2 grid grid-cols-2 gap-x-3 gap-y-1.5">
								<label class="flex cursor-pointer items-center gap-2">
									<input type="checkbox" bind:checked={editDegraded} class="h-3.5 w-3.5" />
									<span class="text-xs">Degraded mode</span>
								</label>
								<label class="flex cursor-pointer items-center gap-2">
									<input type="checkbox" bind:checked={editFsck} class="h-3.5 w-3.5" />
									<span class="text-xs">Fsck on mount</span>
								</label>
								<label class="flex cursor-pointer items-center gap-2">
									<input type="checkbox" bind:checked={editVerbose} class="h-3.5 w-3.5" />
									<span class="text-xs">Verbose</span>
								</label>
								<label class="flex cursor-pointer items-center gap-2">
									<input type="checkbox" bind:checked={editJournalFlushDisabled} class="h-3.5 w-3.5" />
									<span class="text-xs">Disable journal flush</span>
								</label>
							</div>
							<div class="mt-2 grid grid-cols-2 gap-2">
								<div>
									<Label class="text-[0.65rem]">Journal flush delay (µs)</Label>
									<Input type="number" bind:value={editJournalFlushDelay} placeholder="1000" class="mt-0.5 h-7 text-xs" />
								</div>
								<div>
									<Label class="text-[0.65rem]">I/O scheduler</Label>
									<select bind:value={editIoScheduler} class="mt-0.5 h-7 w-full rounded-md border border-input bg-background px-2 text-xs">
										<option value="">Default</option>
										<option value="none">none (recommended for SSDs)</option>
										<option value="mq-deadline">mq-deadline</option>
										<option value="kyber">kyber</option>
									</select>
								</div>
							</div>
							<p class="mt-1.5 text-[0.6rem] text-muted-foreground">These require a remount to take effect.</p>
						</fieldset>
					</div>
					<div class="mt-4 flex gap-2">
						<Button size="xs" onclick={() => saveOptions(fs.name)}>Save</Button>
						<Button variant="secondary" size="xs" onclick={() => editOptionsFs = null}>Cancel</Button>
					</div>
				</div>
			{/if}

				{#if expandedFs === fs.name}
					<div class="mt-4 border-t border-border pt-4">
						<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5 text-xs">
							<span class="text-muted-foreground">Replicas</span>
							<span>{fs.options.data_replicas ?? 1}</span>
							<span class="text-muted-foreground">Checksum</span>
							<span>{fs.options.data_checksum ?? '—'}</span>
							<span class="text-muted-foreground">Compression</span>
							<span>{fs.options.compression ?? 'none'}{#if fs.options.background_compression} / bg: {fs.options.background_compression}{/if}</span>
							<span class="text-muted-foreground">Erasure Code</span>
							<span>{fs.options.erasure_code ? 'Enabled' : 'No'}</span>
							<span class="text-muted-foreground">Encrypted</span>
							<span>
								{#if fs.options.encrypted}
									Yes
									{#if fs.options.locked}
										<Badge variant="destructive" class="ml-1 text-[0.6rem]">Locked</Badge>
									{:else}
										<Badge variant="default" class="ml-1 text-[0.6rem]">Unlocked</Badge>
									{/if}
									{#if fs.options.key_stored}
										<Badge variant="secondary" class="ml-1 text-[0.6rem]">Auto-unlock</Badge>
									{/if}
								{:else}
									No
								{/if}
							</span>
							{#if fs.options.foreground_target}
								<span class="text-muted-foreground">Foreground Target</span>
								<span>{fs.options.foreground_target}</span>
							{/if}
							{#if fs.options.background_target}
								<span class="text-muted-foreground">Background Target</span>
								<span>{fs.options.background_target}</span>
							{/if}
							{#if fs.options.promote_target}
								<span class="text-muted-foreground">Promote Target</span>
								<span>{fs.options.promote_target}</span>
							{/if}
							{#if fs.options.metadata_target}
								<span class="text-muted-foreground">Metadata Target</span>
								<span>{fs.options.metadata_target}</span>
							{/if}
							{#if fs.options.error_action}
								<span class="text-muted-foreground">Error Action</span>
								<span>{fs.options.error_action}</span>
							{/if}
							{#if fs.options.journal_flush_delay}
								<span class="text-muted-foreground">Journal Flush Delay</span>
								<span>{fs.options.journal_flush_delay} µs</span>
							{/if}
							{#if fs.options.io_scheduler}
								<span class="text-muted-foreground">I/O Scheduler</span>
								<span>{fs.options.io_scheduler}</span>
							{/if}
						</div>

						<table class="w-full text-sm">
							<thead>
								<tr>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Device</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Label</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">State</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Data Allowed</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground">Has Data</th>
									<th class="p-2 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
								</tr>
							</thead>
							<tbody>
								{#each fs.devices as dev}
									<tr class="border-b border-border">
										<td class="p-2 font-mono text-xs">
											{dev.path}
											{#if dev.durability !== null && dev.durability !== undefined && dev.durability !== 1}
												<span class="ml-1 rounded bg-secondary px-1 py-0.5 text-[10px] text-muted-foreground">durability={dev.durability}</span>
											{/if}
											{#if dev.discard}
												<span class="ml-1 rounded bg-secondary px-1 py-0.5 text-[10px] text-muted-foreground">discard</span>
											{/if}
										</td>
										<td class="p-2 text-xs">
										{#if fs.mounted && editingLabel === `${fs.name}|${dev.path}`}
											<!-- svelte-ignore a11y_autofocus -->
											<input
												class="w-28 rounded border border-input bg-background px-1.5 py-0.5 font-mono text-xs focus:outline-none focus:ring-1 focus:ring-ring"
												bind:value={editLabelValue}
												onblur={() => saveDeviceLabel(fs.name, dev.path)}
												onkeydown={(e) => { if (e.key === 'Enter') saveDeviceLabel(fs.name, dev.path); if (e.key === 'Escape') editingLabel = null; }}
												autofocus
											/>
										{:else}
											<button
												class="rounded px-1 py-0.5 text-left hover:bg-secondary {dev.label ? '' : 'text-muted-foreground'} {fs.mounted ? 'cursor-text' : 'cursor-default'}"
												onclick={() => { if (fs.mounted) startEditLabel(fs.name, dev); }}
												title={fs.mounted ? 'Click to edit label' : ''}
											>
												{dev.label ?? '—'}
											</button>
										{/if}
									</td>
										<td class="p-2">
											{#if dev.state !== null}
												{@const ds = devDisplayState(dev)}
												<span class="rounded px-2 py-0.5 text-xs font-semibold {stateColor(ds)}">
													{ds}
												</span>
											{:else}
												<span class="text-muted-foreground">—</span>
											{/if}
										</td>
										<td class="p-2 font-mono text-xs text-muted-foreground">{dev.data_allowed ?? '—'}</td>
										<td class="p-2 font-mono text-xs text-muted-foreground">{dev.has_data ?? '—'}</td>
										<td class="p-2 w-px whitespace-nowrap">
											<div class="flex gap-1.5 items-center">
											{#if fs.mounted}
												{@const ds = devDisplayState(dev)}
												{#if ds === 'evacuating'}
													<Button variant="destructive" size="xs" onclick={() => removeDevice(fs.name, dev.path)}>Remove</Button>
												{:else}
													{#if ds === 'rw'}
														<Button variant="secondary" size="xs" onclick={() => setDeviceState(fs.name, dev.path, 'ro')}>Set RO</Button>
														<Button variant="secondary" size="xs" onclick={() => offlineDevice(fs.name, dev.path)}>Offline</Button>
													{:else if ds === 'ro'}
														<Button variant="secondary" size="xs" onclick={() => setDeviceState(fs.name, dev.path, 'rw')}>Set RW</Button>
													{/if}
													{#if ds !== 'spare'}
														<Button variant="secondary" size="xs" onclick={() => evacuateDevice(fs.name, dev.path)}>Evacuate</Button>
													{/if}
													<Button variant="destructive" size="xs" onclick={() => removeDevice(fs.name, dev.path)}>Remove</Button>
												{/if}
											{/if}
											</div>
										</td>
									</tr>
								{/each}
							</tbody>
						</table>

						{#if fs.mounted}
							{#if addDeviceFs === fs.name}
								<div class="mt-3 rounded-lg bg-secondary p-3">
									<div class="mb-2 flex items-center justify-between">
										<Label>Add Device</Label>
										<label class="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
											<input type="checkbox" bind:checked={showAddPartitions} class="h-3.5 w-3.5" />
											Show partitions
										</label>
									</div>
									{#if availableDevicesForAdd().length === 0}
										<p class="text-sm text-muted-foreground">No available devices</p>
									{:else}
										{#each availableDevicesForAdd() as dev}
											<label class="flex cursor-pointer items-center gap-2 py-1 text-sm">
												<input type="radio" name="add-device" value={dev.path} bind:group={addDevicePath} class="h-4 w-4" />
												{dev.path} ({formatBytes(dev.size_bytes)}) {dev.dev_type === 'part' ? '[part]' : ''} {dev.fs_type ? `[${dev.fs_type}]` : ''}
											</label>
										{/each}
									{/if}
									{#if addDevicePath}
										<div class="mt-2">
											<Label for="add-dev-label">Label (optional)</Label>
											<Input id="add-dev-label" bind:value={addDeviceLabel} placeholder="e.g. ssd.fast" class="mt-1" />
										</div>
									{/if}
									<div class="mt-2 flex gap-2">
										<Button size="xs" onclick={() => addDevice(fs.name)} disabled={!addDevicePath}>Add</Button>
										<Button variant="secondary" size="xs" onclick={() => { addDeviceFs = null; addDevicePath = ''; addDeviceLabel = ''; }}>Cancel</Button>
									</div>
								</div>
							{:else}
								<Button variant="secondary" size="xs" class="mt-3" onclick={() => addDeviceFs = fs.name}>+ Add Device</Button>
							{/if}
						{/if}
					</div>
				{/if}
			</CardContent>
		</Card>
	{/each}
{/if}

{/if}
<!-- end pageTab === 'manage' -->

<!-- Unlock Modal -->
{#if unlockFs}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<Card class="w-full max-w-sm">
			<CardContent class="pt-6">
				<h3 class="mb-2 text-lg font-semibold">Unlock "{unlockFs}"</h3>
				<p class="mb-4 text-sm text-muted-foreground">Enter the passphrase to unlock this encrypted filesystem.</p>
				<input type="password" bind:value={unlockPassphrase}
					class="mb-4 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm"
					placeholder="Passphrase"
					onkeydown={(e) => { if (e.key === 'Enter' && unlockPassphrase) doUnlock(); }} />
				<div class="flex gap-2">
					<Button onclick={doUnlock} disabled={!unlockPassphrase}>Unlock</Button>
					<Button variant="secondary" onclick={() => unlockFs = null}>Cancel</Button>
				</div>
			</CardContent>
		</Card>
	</div>
{/if}
