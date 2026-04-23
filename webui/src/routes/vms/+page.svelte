<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { getToken } from '$lib/auth';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { VmStatus, VmCapabilities, Subvolume } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import { CircleCheck, Circle, CircleX } from '@lucide/svelte';
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';

	let vms: VmStatus[] = $state([]);
	let capabilities: VmCapabilities | null = $state(null);
	let blockSubvolumes: Subvolume[] = $state([]);
	let wizardStep: -1 | 0 | 1 | 2 | 3 | 4 | 5 | 6 = $state(0); // 0=hidden, -1=prerequisites
	let loading = $state(true);
	let editTab: 'general' | 'system' | 'storage' | 'network' | 'passthrough' = $state('general');

	const WIZARD_STEPS: [string, string][] = [
		['1', 'General'],
		['2', 'System'],
		['3', 'Storage'],
		['4', 'Network'],
		['5', 'Passthrough'],
		['6', 'Review'],
	];
	let expanded: Record<string, boolean> = $state({});
	let actionInProgress: Record<string, boolean> = $state({});

	// Create form
	let newName = $state('');
	let newCpus = $state(1);
	let newMemory = $state(1024);
	let newDisk = $state('');
	let newDiskCreate = $state(false);
	let newDiskFs = $state('');
	let newDiskSize = $state(10); // GiB
	let newIso = $state('');
	let imageFiles: { name: string; path: string; filesystem: string; size_bytes: number }[] = $state([]);
	let noImagesSubvolume = $state(false);
	let uploading = $state(false);
	let uploadProgress = $state(0);
	let newDescription = $state('');
	let newBootOrder = $state('disk');
	let newAutostart = $state(false);
	let newPassthrough = $state<string[]>([]);

	// Passthrough edit state (for running/stopped VM detail view)
	let editPtVm = $state<string | null>(null);

	// Snapshot/clone dialogs
	let snapshotVm = $state<string | null>(null);
	let snapshotName = $state('');
	let cloneVm = $state<string | null>(null);
	let cloneName = $state('');

	// Console state
	let consoleVm: VmStatus | null = $state(null);
	let consoleMode: 'serial' | 'vnc' = $state('vnc');
	let consoleEl: HTMLDivElement | undefined = $state(undefined);
	let consoleTerm: Terminal | null = $state(null);
	let consoleWs: WebSocket | null = $state(null);
	let consoleFit: FitAddon | null = $state(null);
	// VNC state
	let vncEl: HTMLDivElement | undefined = $state(undefined);
	let vncRfb: any = $state(null);

	const client = getClient();

	let filesystems: { name: string; mounted: boolean }[] = $state([]);

	$effect(() => {
		if (wizardStep > 0) {
			loadSubvolumes();
			loadFilesystems();
			loadImages();
		}
	});

	function openWizard() {
		wizardStep = canCreateVm ? 1 : -1; // -1 = prerequisites
		newName = ''; newCpus = 1; newMemory = 1024; newDisk = ''; newDiskCreate = false;
		newDiskFs = ''; newDiskSize = 10; newIso = ''; newDescription = '';
		newBootOrder = 'disk'; newAutostart = false; newPassthrough = [];
	}

	// Network state for create wizard
	let newNetMode = $state('user');
	let newNetBridge = $state('');
	let newNetMac = $state('');

	async function loadFilesystems() {
		try {
			filesystems = await client.call<{ name: string; mounted: boolean }[]>('fs.list');
			filesystems = filesystems.filter(f => f.mounted);
		} catch { /* ignore */ }
	}

	async function loadImages() {
		try {
			const result = await client.call<{ subvolume_exists: boolean; images: typeof imageFiles }>('vm.images.list');
			imageFiles = result.images;
			noImagesSubvolume = !result.subvolume_exists;
		} catch { imageFiles = []; noImagesSubvolume = true; }
	}

	async function createImagesSubvolume(filesystem: string) {
		await withToast(
			() => client.call('vm.images.ensure', { filesystem }),
			'Image storage created'
		);
		await loadImages();
		noImagesSubvolume = false;
	}

	async function uploadImage(event: Event) {
		const input = event.target as HTMLInputElement;
		if (!input.files || input.files.length === 0) return;

		const file = input.files[0];
		uploading = true;
		uploadProgress = 0;

		try {
			const formData = new FormData();
			formData.append('file', file);

			const xhr = new XMLHttpRequest();
			xhr.open('POST', '/api/upload/vm-image');

			const token = getToken();
			if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`);

			xhr.upload.onprogress = (e) => {
				if (e.lengthComputable) {
					uploadProgress = Math.round((e.loaded / e.total) * 100);
				}
			};
			xhr.timeout = 3600_000; // 1 hour, matching nginx proxy timeouts

			await new Promise<void>((resolve, reject) => {
				xhr.ontimeout = () => reject(new Error('Upload timed out'));
				xhr.onload = () => {
					if (xhr.status >= 200 && xhr.status < 300) {
						resolve();
					} else {
						reject(new Error(xhr.responseText || 'Upload failed'));
					}
				};
				xhr.onerror = () => reject(new Error('Upload failed'));
				xhr.send(formData);
			});

			await withToast(
				async () => { await loadImages(); },
				`Uploaded ${file.name}`
			);
		} catch (e) {
			alert(`Upload failed: ${e}`);
		} finally {
			uploading = false;
			uploadProgress = 0;
			input.value = '';
		}
	}

	function formatSize(bytes: number): string {
		if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
		if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(0)} MiB`;
		return `${bytes} B`;
	}

	// Initialize serial console (xterm) when element mounts
	$effect(() => {
		if (consoleEl && consoleVm && consoleMode === 'serial' && !consoleTerm) {
			const term = new Terminal({
				cursorBlink: true,
				fontSize: 14,
				fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
				theme: {
					background: '#0f1117',
					foreground: '#e0e0e0',
					cursor: '#e0e0e0',
				},
			});
			const fit = new FitAddon();
			term.loadAddon(fit);
			term.open(consoleEl);
			fit.fit();

			consoleTerm = term;
			consoleFit = fit;

			const token = getToken();
			const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
			const ws = new WebSocket(`${proto}//${window.location.host}/ws/vm/${consoleVm.id}/serial?token=${encodeURIComponent(token ?? '')}`);
			ws.binaryType = 'arraybuffer';

			ws.onopen = () => {
				term.writeln('\x1b[33mConnecting to serial console...\x1b[0m\r\n');
			};

			ws.onmessage = (event) => {
				if (event.data instanceof ArrayBuffer) {
					term.write(new Uint8Array(event.data));
				} else {
					term.write(event.data);
				}
			};

			ws.onclose = () => {
				term.writeln('\r\n\x1b[31mConsole disconnected.\x1b[0m');
			};

			term.onData((input) => {
				if (ws.readyState === WebSocket.OPEN) {
					ws.send(input);
				}
			});

			consoleWs = ws;
		}
	});

	// Initialize VNC console (noVNC) when element mounts
	$effect(() => {
		if (vncEl && consoleVm && consoleMode === 'vnc' && !vncRfb) {
			const token = getToken();
			const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
			const url = `${proto}//${window.location.host}/ws/vm/${consoleVm.id}/vnc?token=${encodeURIComponent(token ?? '')}`;

			import('@novnc/novnc/lib/rfb.js').then(({ default: RFB }) => {
				const rfb = new RFB(vncEl!, url, {
					wsProtocols: [],
				});
				rfb.scaleViewport = true;
				rfb.resizeSession = true;

				vncRfb = rfb;

				rfb.addEventListener('disconnect', () => {
					vncRfb = null;
				});
			}).catch((e) => {
				console.error('Failed to load noVNC:', e);
			});
		}
	});

	onMount(async () => {
		await Promise.all([refresh(), loadCapabilities(), loadFilesystems(), loadImages(), loadSubvolumes()]);
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			vms = await client.call<VmStatus[]>('vm.list');
		});
	}

	async function loadCapabilities() {
		try {
			capabilities = await client.call<VmCapabilities>('vm.capabilities');
		} catch { /* ignore */ }
	}

	async function loadSubvolumes() {
		await withToast(async () => {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			blockSubvolumes = all.filter(s => s.subvolume_type === 'block' && s.block_device);
		});
	}

	function toggle(id: string) {
		expanded[id] = !expanded[id];
	}

	async function create() {
		if (!newName) return;

		let diskPath = newDisk;

		// Create a new block subvolume if requested
		if (newDiskCreate && newDiskFs && newDiskSize > 0) {
			const svName = `vm-${newName}`;
			const sizeBytes = newDiskSize * 1024 * 1024 * 1024;
			const svResult = await withToast(
				() => client.call('subvolume.create', {
					filesystem: newDiskFs,
					name: svName,
					subvolume_type: 'block',
					volsize_bytes: sizeBytes,
				}),
				'Disk subvolume created'
			);
			if (svResult === undefined) return; // creation failed
			diskPath = (svResult as any).block_device ?? '';
			if (!diskPath) {
				await withToast(async () => { throw new Error('Block device not attached'); }, '');
				return;
			}
		}

		const params: Record<string, unknown> = {
			name: newName,
			cpus: newCpus,
			memory_mib: newMemory,
			boot_order: newBootOrder,
			autostart: newAutostart,
		};
		if (diskPath) {
			params.disks = [{ path: diskPath, interface: 'virtio', readonly: false }];
		}
		if (newIso) {
			params.boot_iso = newIso;
			if (!newDisk) params.boot_order = 'cdrom';
		}
		if (newDescription) params.description = newDescription;
		// Network
		const net: Record<string, unknown> = { mode: newNetMode };
		if (newNetMode === 'bridge' && newNetBridge) net.bridge = newNetBridge;
		if (newNetMac) net.mac = newNetMac;
		params.networks = [net];
		// Passthrough
		if (newPassthrough.length > 0) {
			params.passthrough_devices = newPassthrough.map(addr => {
				const dev = capabilities?.passthrough_devices.find(d => d.address === addr);
				return { address: addr, label: dev?.description ?? null };
			});
		}

		const ok = await withToast(
			() => client.call('vm.create', params),
			'Virtual machine created'
		);
		if (ok !== undefined) {
			wizardStep = 0;
			newName = ''; newCpus = 1; newMemory = 1024; newDisk = '';
			newDiskCreate = false; newDiskFs = ''; newDiskSize = 10;
			newIso = ''; newDescription = ''; newBootOrder = 'disk';
			newAutostart = false; newPassthrough = [];
			newNetMode = 'user'; newNetBridge = ''; newNetMac = '';
			await refresh();
		}
	}

	async function startVm(id: string) {
		actionInProgress[id] = true;
		await withToast(
			() => client.call('vm.start', { id }),
			'VM started'
		);
		actionInProgress[id] = false;
		await refresh();
	}

	async function stopVm(id: string) {
		actionInProgress[id] = true;
		await withToast(
			() => client.call('vm.stop', { id }),
			'Shutdown signal sent'
		);
		actionInProgress[id] = false;
		// Poll for status change
		setTimeout(() => refresh(), 3000);
	}

	async function killVm(id: string) {
		if (!await confirm('Force kill this VM?', 'This is equivalent to pulling the power cord. Data loss may occur.')) return;
		actionInProgress[id] = true;
		await withToast(
			() => client.call('vm.kill', { id }),
			'VM force-killed'
		);
		actionInProgress[id] = false;
		await refresh();
	}

	async function deleteVm(id: string) {
		if (!await confirm('Delete this VM?', 'The VM configuration will be removed. Disk subvolumes are NOT deleted.')) return;
		await withToast(
			() => client.call('vm.delete', { id }),
			'VM deleted'
		);
		await refresh();
	}

	async function snapshotVmAction() {
		if (!snapshotVm || !snapshotName) return;
		await withToast(
			() => client.call('vm.snapshot', { id: snapshotVm, name: snapshotName }),
			'VM snapshot created'
		);
		snapshotVm = null;
		snapshotName = '';
	}

	async function cloneVmAction() {
		if (!cloneVm || !cloneName) return;
		await withToast(
			() => client.call('vm.clone', { id: cloneVm, new_name: cloneName }),
			'VM cloned'
		);
		cloneVm = null;
		cloneName = '';
		await refresh();
	}

	async function updateVmField(vmId: string, field: string, value: unknown) {
		await withToast(
			() => client.call('vm.update', { id: vmId, [field]: value }),
			'VM updated'
		);
		await refresh();
	}

	async function detachDisk(vm: VmStatus, index: number) {
		const disks = vm.disks.filter((_, i) => i !== index);
		await withToast(
			() => client.call('vm.update', { id: vm.id, disks }),
			'Disk detached'
		);
		await refresh();
	}

	async function attachDisk(vmId: string, currentDisks: VmStatus['disks'], blockDevice: string) {
		const disks = [...currentDisks, { path: blockDevice, interface: 'virtio', readonly: false }];
		await withToast(
			() => client.call('vm.update', { id: vmId, disks }),
			'Disk attached'
		);
		await refresh();
	}

	async function toggleAutostart(vm: VmStatus) {
		await withToast(
			() => client.call('vm.update', { id: vm.id, autostart: !vm.autostart }),
			vm.autostart ? 'Autostart disabled' : 'Autostart enabled'
		);
		await refresh();
	}

	function openConsole(vm: VmStatus, mode: 'serial' | 'vnc' = 'vnc') {
		closeConsole();
		consoleMode = mode;
		consoleVm = vm;
	}

	function switchConsoleMode(mode: 'serial' | 'vnc') {
		const vm = consoleVm;
		if (!vm) return;
		closeConsole();
		consoleMode = mode;
		consoleVm = vm;
	}

	function closeConsole() {
		consoleWs?.close();
		consoleTerm?.dispose();
		if (vncRfb) {
			try { vncRfb.disconnect(); } catch { /* ignore */ }
		}
		consoleVm = null;
		consoleTerm = null;
		consoleWs = null;
		consoleFit = null;
		vncRfb = null;
	}

	async function addPassthroughDevice(vmId: string, address: string) {
		const vm = vms.find(v => v.id === vmId);
		if (!vm) return;
		const existing = vm.passthrough_devices.map(d => d.address);
		if (existing.includes(address)) return;
		const dev = capabilities?.passthrough_devices.find(d => d.address === address);
		const updated = [...vm.passthrough_devices, { address, label: dev?.description ?? null }];
		await withToast(
			() => client.call('vm.update', { id: vmId, passthrough_devices: updated }),
			'Passthrough device added'
		);
		await refresh();
	}

	async function removePassthroughDevice(vmId: string, address: string) {
		const vm = vms.find(v => v.id === vmId);
		if (!vm) return;
		const updated = vm.passthrough_devices.filter(d => d.address !== address);
		await withToast(
			() => client.call('vm.update', { id: vmId, passthrough_devices: updated }),
			'Passthrough device removed'
		);
		await refresh();
	}

	// PCI devices not assigned to any VM
	const availablePciDevices = $derived.by(() => {
		if (!capabilities) return [];
		const assigned = new Set(vms.flatMap(v => v.passthrough_devices.map(d => d.address)));
		return capabilities.passthrough_devices.filter(d => !assigned.has(d.address));
	});

	function formatMemory(mib: number): string {
		if (mib >= 1024) return `${(mib / 1024).toFixed(1)} GiB`;
		return `${mib} MiB`;
	}

	let search = $state('');
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort() {
		sortDir = sortDir === 'asc' ? 'desc' : 'asc';
	}

	const filtered = $derived(
		search.trim()
			? vms.filter(v => v.name.toLowerCase().includes(search.toLowerCase()))
			: vms
	);

	const sorted = $derived.by(() => {
		return [...filtered].sort((a, b) => {
			const cmp = a.name.localeCompare(b.name);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});

	const envReady = $derived.by(() => {
		if (!capabilities) return null;
		return {
			kvm: capabilities.kvm_available,
			uefi: capabilities.uefi_available,
			filesystem: filesystems.length > 0,
			imageStorage: !noImagesSubvolume,
		};
	});

	const envFullyReady = $derived(
		envReady !== null &&
		envReady.kvm &&
		envReady.uefi &&
		envReady.filesystem &&
		envReady.imageStorage
	);

	const canCreateVm = $derived(
		envReady !== null &&
		envReady.kvm &&
		envReady.uefi &&
		envReady.filesystem
	);
</script>

{#if capabilities}
	<div class="mb-4 flex items-center gap-4 rounded-lg border border-border px-4 py-2.5 text-sm">
		<div class="flex items-center gap-2">
			<span class="h-2 w-2 rounded-full {capabilities.kvm_available ? 'bg-green-400' : 'bg-red-400'}"></span>
			<span class="text-muted-foreground">{vms.length} VM{vms.length !== 1 ? 's' : ''}</span>
		</div>
		<span class="text-muted-foreground">{vms.filter(v => v.running).length} running</span>
	</div>
{/if}
<div class="mb-4 flex items-center gap-3">
	<Button size="sm" onclick={() => wizardStep === 0 ? openWizard() : (wizardStep = 0)}>
		{wizardStep !== 0 ? 'Cancel' : 'Create VM'}
	</Button>
	{#if vms.length > 3}
		<Input bind:value={search} placeholder="Filter..." class="h-9 w-40" />
	{/if}
</div>

{#if wizardStep !== 0}
	<Card class="mb-6 max-w-4xl">
		<CardContent class="pt-6">
			<!-- Step indicator -->
			<div class="mb-6 flex items-center gap-0">
				{#each WIZARD_STEPS as [num, label], i}
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
						{#if i < WIZARD_STEPS.length - 1}
							<div class="mx-3 h-px w-8 bg-border"></div>
						{/if}
					</div>
				{/each}
			</div>

			<!-- Prerequisites -->
			{#if wizardStep === -1}
			<h3 class="mb-1 text-lg font-semibold">VM Environment Setup</h3>
			<p class="mb-4 text-sm text-muted-foreground">Complete these steps before creating VMs.</p>
			<div class="space-y-2">
				{#if envReady}
					<div class="flex items-start gap-3 rounded-lg border border-border px-3 py-2.5">
						{#if envReady.kvm}
							<CircleCheck size={18} class="mt-0.5 shrink-0 text-green-500" />
						{:else}
							<CircleX size={18} class="mt-0.5 shrink-0 text-destructive" />
						{/if}
						<div class="flex-1 min-w-0">
							<div class="text-sm font-medium">KVM Virtualization</div>
							<div class="text-xs text-muted-foreground">
								{envReady.kvm ? 'Hardware virtualization available' : 'Not available — requires bare-metal host with CPU virtualization enabled'}
							</div>
						</div>
					</div>
					<div class="flex items-start gap-3 rounded-lg border border-border px-3 py-2.5">
						{#if envReady.uefi}
							<CircleCheck size={18} class="mt-0.5 shrink-0 text-green-500" />
						{:else}
							<CircleX size={18} class="mt-0.5 shrink-0 text-destructive" />
						{/if}
						<div class="flex-1 min-w-0">
							<div class="text-sm font-medium">UEFI Firmware</div>
							<div class="text-xs text-muted-foreground">
								{envReady.uefi ? 'Boot firmware ready' : 'OVMF firmware not found'}
							</div>
						</div>
					</div>
					<div class="flex items-start gap-3 rounded-lg border border-border px-3 py-2.5">
						{#if envReady.filesystem}
							<CircleCheck size={18} class="mt-0.5 shrink-0 text-green-500" />
						{:else}
							<Circle size={18} class="mt-0.5 shrink-0 text-muted-foreground" />
						{/if}
						<div class="flex-1 min-w-0">
							<div class="text-sm font-medium">Filesystem</div>
							<div class="text-xs text-muted-foreground">
								{envReady.filesystem
									? `${filesystems.length} filesystem${filesystems.length !== 1 ? 's' : ''} available`
									: 'No filesystem found — create one in Filesystems first'}
							</div>
						</div>
						{#if !envReady.filesystem}
							<Button size="xs" variant="outline" onclick={() => window.location.href = '/filesystems'}>Go to Filesystems</Button>
						{/if}
					</div>
					<div class="flex items-start gap-3 rounded-lg border border-border px-3 py-2.5">
						{#if envReady.imageStorage}
							<CircleCheck size={18} class="mt-0.5 shrink-0 text-green-500" />
						{:else}
							<Circle size={18} class="mt-0.5 shrink-0 text-muted-foreground" />
						{/if}
						<div class="flex-1 min-w-0">
							<div class="text-sm font-medium">Image Storage</div>
							<div class="text-xs text-muted-foreground">
								{envReady.imageStorage
									? 'Images subvolume ready'
									: envReady.filesystem
										? 'Create an images subvolume to store ISOs and disk images'
										: 'Requires a filesystem first'}
							</div>
						</div>
						{#if !envReady.imageStorage && envReady.filesystem}
							{#each filesystems as fs}
								<Button size="xs" variant="outline" onclick={() => createImagesSubvolume(fs.name)}>Create on {fs.name}</Button>
							{/each}
						{/if}
					</div>
				{/if}
			</div>
			<div class="mt-4">
				<Button size="sm" onclick={() => wizardStep = 1} disabled={!canCreateVm}>
					{canCreateVm ? 'Next: General →' : 'Complete prerequisites first'}
				</Button>
			</div>

			<!-- Step 1: General -->
			{:else if wizardStep === 1}
			<div class="mb-4">
				<Label for="vm-name">Name</Label>
				<Input id="vm-name" bind:value={newName} placeholder="my-vm" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="vm-desc">Description</Label>
				<Input id="vm-desc" bind:value={newDescription} placeholder="Optional description" class="mt-1" />
			</div>
			<div class="mb-4 flex items-center gap-2">
				<input id="vm-autostart" type="checkbox" bind:checked={newAutostart} class="rounded border-input" />
				<Label for="vm-autostart">Auto-start on NASty boot</Label>
			</div>
			<div class="flex gap-2">
				<Button size="sm" onclick={() => wizardStep = 2} disabled={!newName}>Next: System →</Button>
			</div>

			<!-- Step 2: System -->
			{:else if wizardStep === 2}
			<div class="grid grid-cols-2 gap-4 mb-4">
				<div>
					<Label for="vm-cpus">CPU Cores</Label>
					<Input id="vm-cpus" type="number" bind:value={newCpus} min={1} max={64} class="mt-1" />
				</div>
				<div>
					<Label for="vm-memory">Memory (MiB)</Label>
					<Input id="vm-memory" type="number" bind:value={newMemory} min={128} step={128} class="mt-1" />
				</div>
			</div>
			<details class="mb-4">
				<summary class="cursor-pointer text-xs text-muted-foreground hover:text-foreground">Advanced system options</summary>
				<div class="mt-2 grid grid-cols-2 gap-4">
					<div>
						<Label class="text-xs">CPU Model</Label>
						<select class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value="host">host (passthrough)</option>
							<option value="max">max (all features)</option>
							<option value="qemu64">qemu64 (generic)</option>
						</select>
					</div>
					<div>
						<Label class="text-xs">Machine Type</Label>
						<select class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value="q35">Q35 (modern, PCIe)</option>
							<option value="i440fx">i440FX (legacy PCI)</option>
						</select>
					</div>
					<div>
						<Label class="text-xs">VGA Type</Label>
						<select class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
							<option value="virtio">Virtio GPU</option>
							<option value="qxl">QXL (SPICE)</option>
							<option value="std">Standard VGA</option>
							<option value="none">None</option>
						</select>
					</div>
				</div>
			</details>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 1}>← Back</Button>
				<Button size="sm" onclick={() => wizardStep = 3}>Next: Storage →</Button>
			</div>

			<!-- Step 3: Storage -->
			{:else if wizardStep === 3}
			<div class="mb-4">
				<div class="flex items-center justify-between">
					<Label>Boot Disk</Label>
					<label class="flex items-center gap-1.5 text-xs text-muted-foreground cursor-pointer">
						<input type="checkbox" bind:checked={newDiskCreate} class="rounded border-input" />
						Create new disk
					</label>
				</div>
				{#if newDiskCreate}
					<div class="grid grid-cols-2 gap-3 mt-1">
						<div>
							<Label class="text-xs">Filesystem</Label>
							<select bind:value={newDiskFs} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
								<option value="">Select...</option>
								{#each filesystems as fs}
									<option value={fs.name}>{fs.name}</option>
								{/each}
							</select>
						</div>
						<div>
							<Label class="text-xs">Size (GiB)</Label>
							<Input type="number" bind:value={newDiskSize} min={1} class="mt-1" />
						</div>
					</div>
					<span class="mt-1 block text-xs text-muted-foreground">A block subvolume named "vm-{newName || '...'}" will be created.</span>
				{:else}
					<select bind:value={newDisk} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						<option value="">None (ISO boot only)</option>
						{#each blockSubvolumes as sv}
							<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
						{/each}
					</select>
				{/if}
			</div>
			<div class="mb-4">
				<Label>Boot Image (optional)</Label>
				{#if !noImagesSubvolume}
					<select bind:value={newIso} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
						<option value="">None</option>
						{#each imageFiles as iso}
							<option value={iso.path}>{iso.name} ({iso.filesystem}, {formatSize(iso.size_bytes)})</option>
						{/each}
					</select>
					<div class="mt-2 flex items-center gap-3">
						<Button
							size="sm"
							variant="outline"
							disabled={uploading}
							onclick={() => document.getElementById('file-upload')?.click()}
						>
							{uploading ? 'Uploading...' : 'Upload new image'}
						</Button>
						<input
							id="file-upload"
							type="file"
							accept=".iso,.qcow2,.img,.raw"
							class="hidden"
							onchange={uploadImage}
							disabled={uploading}
						/>
						<span class="text-xs text-muted-foreground">ISO, qcow2, img, raw</span>
					</div>
					{#if uploading}
						<div class="mt-2 flex items-center gap-2">
							<div class="h-2 flex-1 rounded-full bg-muted overflow-hidden">
								<div class="h-full bg-primary transition-all" style="width: {uploadProgress}%"></div>
							</div>
							<span class="text-xs text-muted-foreground w-10 text-right">{uploadProgress}%</span>
						</div>
					{/if}
				{:else if filesystems.length > 0}
					<div class="mt-1 rounded border border-dashed border-muted-foreground/30 p-3 text-sm text-muted-foreground">
						<p class="mb-2">No image storage found. Create an "images" subvolume to store VM images.</p>
						<div class="flex gap-2 items-center">
							{#each filesystems as fs}
								<Button size="xs" variant="outline" onclick={() => createImagesSubvolume(fs.name)}>
									Create on {fs.name}
								</Button>
							{/each}
						</div>
					</div>
				{:else}
					<Input bind:value={newIso} placeholder="No images available" class="mt-1" disabled />
				{/if}
			</div>
			<div class="mb-4">
				<Label for="vm-boot">Boot Order</Label>
				<select id="vm-boot" bind:value={newBootOrder} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="disk">Disk</option>
					<option value="cdrom">CD-ROM (ISO)</option>
					<option value="network">Network (PXE)</option>
				</select>
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 2}>← Back</Button>
				<Button size="sm" onclick={() => wizardStep = 4}>Next: Network →</Button>
			</div>

			<!-- Step 4: Network -->
			{:else if wizardStep === 4}
			<div class="mb-4">
				<Label>Network Mode</Label>
				<select bind:value={newNetMode} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="user">NAT (User mode — simple, no host config needed)</option>
					<option value="bridge">Bridge (VM on host network, requires bridge interface)</option>
				</select>
			</div>
			{#if newNetMode === 'bridge'}
				<div class="mb-4">
					<Label>Bridge Interface</Label>
					<Input bind:value={newNetBridge} placeholder="br0" class="mt-1" />
				</div>
			{/if}
			<details class="mb-4">
				<summary class="cursor-pointer text-xs text-muted-foreground hover:text-foreground">Advanced network options</summary>
				<div class="mt-2">
					<Label class="text-xs">MAC Address</Label>
					<Input bind:value={newNetMac} placeholder="Auto-generated if empty" class="mt-1 font-mono text-xs" />
				</div>
			</details>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 3}>← Back</Button>
				<Button size="sm" onclick={() => wizardStep = 5}>Next: Passthrough →</Button>
			</div>

			<!-- Step 5: Passthrough -->
			{:else if wizardStep === 5}
			<div class="mb-4">
				<Label>PCI Passthrough</Label>
				{#if availablePciDevices.length > 0}
					<div class="mt-1 max-h-40 overflow-y-auto rounded border border-input p-2 space-y-1">
						{#each availablePciDevices as dev}
							<label class="flex items-start gap-2 text-xs cursor-pointer hover:bg-muted/30 rounded p-1">
								<input
									type="checkbox"
									class="mt-0.5 rounded border-input"
									checked={newPassthrough.includes(dev.address)}
									onchange={() => {
										if (newPassthrough.includes(dev.address)) {
											newPassthrough = newPassthrough.filter(a => a !== dev.address);
										} else {
											newPassthrough = [...newPassthrough, dev.address];
										}
									}}
								/>
								<div>
									<span class="font-mono">{dev.address}</span>
									<span class="text-muted-foreground ml-1">{dev.description}</span>
									<span class="text-muted-foreground ml-1">(IOMMU group {dev.iommu_group})</span>
								</div>
							</label>
						{/each}
					</div>
					<span class="mt-1 block text-xs text-muted-foreground">Devices are bound to vfio-pci when the VM starts.</span>
				{:else}
					<p class="mt-1 text-xs text-muted-foreground">No PCI devices available. Enable VT-d (Intel) or AMD-Vi in BIOS/UEFI settings. Not available on virtual machines.</p>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 4}>← Back</Button>
				<Button size="sm" onclick={() => wizardStep = 6}>Next: Review →</Button>
			</div>

			<!-- Step 6: Review -->
			{:else if wizardStep === 6}
			<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
				<span class="text-muted-foreground">Name</span>
				<span class="font-mono">{newName}</span>
				<span class="text-muted-foreground">CPU</span>
				<span>{newCpus} core{newCpus !== 1 ? 's' : ''}</span>
				<span class="text-muted-foreground">Memory</span>
				<span>{newMemory} MiB</span>
				<span class="text-muted-foreground">Boot Order</span>
				<span>{newBootOrder}</span>
				{#if newDisk || newDiskCreate}
					<span class="text-muted-foreground">Disk</span>
					<span class="font-mono text-xs">{newDiskCreate ? `vm-${newName} (${newDiskSize} GiB, new)` : newDisk}</span>
				{/if}
				{#if newIso}
					<span class="text-muted-foreground">Boot ISO</span>
					<span class="font-mono text-xs">{newIso}</span>
				{/if}
				<span class="text-muted-foreground">Network</span>
				<span>{newNetMode === 'bridge' ? `Bridge (${newNetBridge || 'br0'})` : 'NAT'}</span>
				{#if newPassthrough.length > 0}
					<span class="text-muted-foreground">Passthrough</span>
					<span>{newPassthrough.length} device{newPassthrough.length !== 1 ? 's' : ''}</span>
				{/if}
				{#if newDescription}
					<span class="text-muted-foreground">Description</span>
					<span>{newDescription}</span>
				{/if}
				{#if newAutostart}
					<span class="text-muted-foreground">Autostart</span>
					<span>Yes</span>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => wizardStep = 5}>← Back</Button>
				<Button size="sm" onclick={create} disabled={!newName}>Create VM</Button>
			</div>
			{/if}
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if vms.length === 0}
	<p class="text-muted-foreground">No VMs installed.</p>
{:else}
	<table class="w-full text-sm">
		<thead>
			<tr>
				<SortTh label="Name" active={true} dir={sortDir} onclick={toggleSort} />
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Resources</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each sorted as vm}
				<tr class="border-b border-border cursor-pointer hover:bg-muted/30 transition-colors" onclick={() => toggle(vm.id)}>
					<td class="p-3">
						<span class="font-semibold">{vm.name}</span>
						{#if vm.autostart}
							<Badge variant="secondary" class="ml-2 text-[0.6rem]">autostart</Badge>
						{/if}
						{#if vm.description}
							<span class="ml-2 text-xs text-muted-foreground">{vm.description}</span>
						{/if}
					</td>
					<td class="p-3 text-xs text-muted-foreground">
						{vm.cpus} vCPU{vm.cpus !== 1 ? 's' : ''}
						&middot; {formatMemory(vm.memory_mib)}
						&middot; {vm.disks.length} disk{vm.disks.length !== 1 ? 's' : ''}
					</td>
					<td class="p-3">
						<Badge variant={vm.running ? 'default' : 'secondary'}>
							{vm.running ? 'Running' : 'Stopped'}
						</Badge>
						{#if vm.pid}
							<span class="ml-1 text-xs text-muted-foreground">PID {vm.pid}</span>
						{/if}
					</td>
					<td class="p-3" onclick={(e) => e.stopPropagation()}>
						<div class="flex gap-2">
							{#if vm.running}
								<Button variant="outline" size="xs" onclick={() => openConsole(vm, 'vnc')}>
									Display
								</Button>
								<Button variant="outline" size="xs" onclick={() => openConsole(vm, 'serial')}>
									Serial
								</Button>
								<Button variant="secondary" size="xs" onclick={() => stopVm(vm.id)} disabled={actionInProgress[vm.id]}>
									Stop
								</Button>
								<Button variant="destructive" size="xs" onclick={() => killVm(vm.id)} disabled={actionInProgress[vm.id]}>
									Kill
								</Button>
							{:else}
								<Button variant="default" size="xs" onclick={() => startVm(vm.id)} disabled={actionInProgress[vm.id]}>
									Start
								</Button>
								<Button variant="destructive" size="xs" onclick={() => deleteVm(vm.id)}>
									Delete
								</Button>
							{/if}
						</div>
					</td>
				</tr>
				{#if expanded[vm.id]}
					<tr class="border-b border-border bg-secondary/20">
						<td colspan="4" class="px-4 py-4">
							<!-- Detail view tabs -->
							<div class="mb-4 flex border-b border-border">
								{#each ['general', 'system', 'storage', 'network', 'passthrough'] as tab}
									<button
										onclick={() => editTab = tab as typeof editTab}
										class="px-3 py-1.5 text-xs font-medium transition-colors capitalize border-b-2 -mb-px
											{editTab === tab
												? 'border-primary text-foreground'
												: 'border-transparent text-muted-foreground hover:text-foreground'}"
									>{tab}</button>
								{/each}
							</div>

							<div>
								<!-- General tab -->
								<!-- General tab -->
								{#if editTab === 'general'}
								<div class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
									<span class="text-muted-foreground">Name</span>
									<span class="font-semibold">{vm.name}</span>
									<span class="text-muted-foreground">Description</span>
									{#if !vm.running}
										<input type="text" value={vm.description ?? ''} placeholder="Optional"
											class="h-7 rounded-md border border-input bg-transparent px-2 text-sm"
											onchange={(e) => updateVmField(vm.id, 'description', (e.target as HTMLInputElement).value)} />
									{:else}
										<span>{vm.description || '—'}</span>
									{/if}
									<span class="text-muted-foreground">Autostart</span>
									<span>{vm.autostart ? 'Yes' : 'No'}
										{#if !vm.running}
											<Button variant="ghost" size="xs" class="ml-2" onclick={() => toggleAutostart(vm)}>
												{vm.autostart ? 'Disable' : 'Enable'}
											</Button>
										{/if}
									</span>
								</div>
								{#if vm.disks.length > 0}
									<div class="mt-3 flex flex-wrap gap-2">
										<Button size="xs" variant="outline" onclick={() => { snapshotVm = vm.id; snapshotName = ''; }}>Snapshot</Button>
										{#if !vm.running}
											<Button size="xs" variant="outline" onclick={() => { cloneVm = vm.id; cloneName = ''; }}>Clone</Button>
										{/if}
									</div>
								{/if}

								<!-- System tab -->
								{:else if editTab === 'system'}
								{#if vm.running}
									<div class="grid grid-cols-2 gap-x-8 gap-y-1 text-sm">
										<div><span class="text-muted-foreground">CPU:</span> {vm.cpus} core{vm.cpus !== 1 ? 's' : ''}{vm.cpu_model ? ` (${vm.cpu_model})` : ''}</div>
										<div><span class="text-muted-foreground">Memory:</span> {formatMemory(vm.memory_mib)}</div>
										<div><span class="text-muted-foreground">UEFI:</span> {vm.uefi ? 'Yes' : 'No'}</div>
										{#if vm.vga}<div><span class="text-muted-foreground">VGA:</span> {vm.vga}</div>{/if}
										{#if vm.machine_type}<div><span class="text-muted-foreground">Machine:</span> {vm.machine_type}</div>{/if}
									</div>
								{:else}
									<div class="grid grid-cols-2 gap-4 text-sm">
										<div>
											<span class="text-xs text-muted-foreground">CPU Cores</span>
											<input type="number" value={vm.cpus} min={1} max={64}
												class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm"
												onchange={(e) => updateVmField(vm.id, 'cpus', parseInt((e.target as HTMLInputElement).value))} />
										</div>
										<div>
											<span class="text-xs text-muted-foreground">Memory (MiB)</span>
											<input type="number" value={vm.memory_mib} min={128} step={128}
												class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm"
												onchange={(e) => updateVmField(vm.id, 'memory_mib', parseInt((e.target as HTMLInputElement).value))} />
										</div>
										<div>
											<span class="text-xs text-muted-foreground">CPU Model</span>
											<select value={vm.cpu_model ?? 'host'}
												class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm"
												onchange={(e) => updateVmField(vm.id, 'cpu_model', (e.target as HTMLSelectElement).value)}>
												<option value="host">host (passthrough)</option>
												<option value="max">max (all features)</option>
												<option value="qemu64">qemu64 (generic)</option>
											</select>
										</div>
										<div>
											<span class="text-xs text-muted-foreground">Machine Type</span>
											<select value={vm.machine_type ?? 'q35'}
												class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm"
												onchange={(e) => updateVmField(vm.id, 'machine_type', (e.target as HTMLSelectElement).value)}>
												<option value="q35">Q35 (modern, PCIe)</option>
												<option value="i440fx">i440FX (legacy PCI)</option>
											</select>
										</div>
										<div>
											<span class="text-xs text-muted-foreground">VGA Type</span>
											<select value={vm.vga ?? 'virtio'}
												class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm"
												onchange={(e) => updateVmField(vm.id, 'vga', (e.target as HTMLSelectElement).value)}>
												<option value="virtio">Virtio GPU</option>
												<option value="qxl">QXL (SPICE)</option>
												<option value="std">Standard VGA</option>
												<option value="none">None</option>
											</select>
										</div>
										<div>
											<span class="text-xs text-muted-foreground">Extra QEMU Args</span>
											<input type="text" value={vm.extra_args?.join(' ') ?? ''} placeholder="-device usb-tablet"
												class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs font-mono"
												onchange={(e) => {
													const val = (e.target as HTMLInputElement).value.trim();
													updateVmField(vm.id, 'extra_args', val ? val.split(/\s+/) : []);
												}} />
										</div>
									</div>
								{/if}

								<!-- Storage tab -->
								{:else if editTab === 'storage'}
								<div class="mb-3">
									<div class="grid grid-cols-2 gap-4 text-sm mb-3">
										<div>
											<span class="text-xs text-muted-foreground">Boot Order</span>
											{#if !vm.running}
												<select value={vm.boot_order}
													class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm"
													onchange={(e) => updateVmField(vm.id, 'boot_order', (e.target as HTMLSelectElement).value)}>
													<option value="disk">Disk</option>
													<option value="cdrom">CD-ROM (ISO)</option>
													<option value="network">Network (PXE)</option>
												</select>
											{:else}
												<div class="mt-0.5 text-sm">{vm.boot_order}</div>
											{/if}
										</div>
										<div>
											<span class="text-xs text-muted-foreground">Boot ISO</span>
											{#if !vm.running}
												<select value={vm.boot_iso ?? ''}
													class="mt-0.5 h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs"
													onchange={(e) => updateVmField(vm.id, 'boot_iso', (e.target as HTMLSelectElement).value)}>
													<option value="">None (no ISO)</option>
													{#each imageFiles as iso}
														<option value={iso.path}>{iso.name}</option>
													{/each}
												</select>
											{:else}
												<div class="mt-0.5 text-xs font-mono">{vm.boot_iso || 'None'}</div>
											{/if}
										</div>
									</div>
								</div>
								<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Disks</h4>

								<!-- Disks -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Disks</h4>
									{#if vm.disks.length === 0}
										<p class="text-xs text-muted-foreground">No disks attached</p>
									{:else}
										<div class="space-y-1">
											{#each vm.disks as disk, i}
												{@const sv = blockSubvolumes.find(s => s.block_device === disk.path)}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs font-semibold">Disk {i}</span>
													{#if sv}
														<span class="text-xs">{sv.filesystem}/{sv.name}</span>
														<span class="text-xs text-muted-foreground">{disk.path}</span>
													{:else}
														<span class="text-xs text-muted-foreground">{disk.path}</span>
													{/if}
													<Badge variant="secondary" class="text-[0.6rem]">{disk.interface}</Badge>
													{#if disk.readonly}
														<Badge variant="secondary" class="text-[0.6rem]">readonly</Badge>
													{/if}
													{#if disk.cache}
														<Badge variant="secondary" class="text-[0.6rem]">cache={disk.cache}</Badge>
													{/if}
													{#if disk.discard}
														<Badge variant="secondary" class="text-[0.6rem]">discard={disk.discard}</Badge>
													{/if}
													{#if !vm.running}
														<Button variant="ghost" size="xs" class="ml-auto text-destructive hover:text-destructive" onclick={() => detachDisk(vm, i)}>
															Detach
														</Button>
													{/if}
												</div>
											{/each}
										</div>
									{/if}
									{#if !vm.running}
										{@const attachedPaths = new Set(vm.disks.map(d => d.path))}
										{@const available = blockSubvolumes.filter(s => s.block_device && !attachedPaths.has(s.block_device))}
										{#if available.length > 0}
											<div class="mt-2 flex items-center gap-2">
												<select
													id="attach-disk-{vm.id}"
													class="h-7 rounded-md border border-input bg-transparent px-2 text-xs"
												>
													{#each available as sv}
														<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
													{/each}
												</select>
												<Button variant="outline" size="xs" onclick={() => {
													const sel = document.getElementById(`attach-disk-${vm.id}`) as HTMLSelectElement;
													if (sel?.value) attachDisk(vm.id, vm.disks, sel.value);
												}}>
													Attach disk
												</Button>
											</div>
										{:else if vm.disks.length === 0}
											<p class="mt-1 text-xs text-muted-foreground">No block subvolumes available. Create one in Subvolumes first.</p>
										{/if}
									{/if}
								</div>

								<!-- Network tab -->
								{:else if editTab === 'network'}
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Networks</h4>
									{#if vm.networks.length === 0}
										<p class="text-xs text-muted-foreground">No network interfaces</p>
									{:else}
										<div class="space-y-1">
											{#each vm.networks as net, i}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs font-semibold">NIC {i}</span>
													<Badge variant="secondary" class="text-[0.6rem]">{net.mode}</Badge>
													{#if net.bridge}
														<span class="text-xs text-muted-foreground">bridge: {net.bridge}</span>
													{/if}
													{#if net.mac}
														<span class="font-mono text-xs text-muted-foreground">{net.mac}</span>
													{/if}
												</div>
											{/each}
										</div>
									{/if}
								</div>

								<!-- Passthrough tab -->
								{:else if editTab === 'passthrough'}
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">PCI Passthrough</h4>
									{#if vm.passthrough_devices.length === 0}
										<p class="text-xs text-muted-foreground">No devices assigned</p>
									{:else}
										<div class="space-y-1">
											{#each vm.passthrough_devices as dev}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs">{dev.address}</span>
													{#if dev.label}
														<span class="text-xs text-muted-foreground truncate">{dev.label}</span>
													{/if}
													{#if !vm.running}
														<Button variant="destructive" size="xs" onclick={() => removePassthroughDevice(vm.id, dev.address)}>Remove</Button>
													{/if}
												</div>
											{/each}
										</div>
									{/if}
									{#if !vm.running && availablePciDevices.length > 0}
										{#if editPtVm === vm.id}
											<div class="mt-2 max-h-32 overflow-y-auto rounded border p-2 space-y-1">
												{#each availablePciDevices as dev}
													<div class="flex items-center gap-2 text-xs hover:bg-muted/30 rounded p-1">
														<Button size="xs" variant="outline" onclick={() => { addPassthroughDevice(vm.id, dev.address); editPtVm = null; }}>Add</Button>
														<span class="font-mono">{dev.address}</span>
														<span class="text-muted-foreground truncate">{dev.description}</span>
													</div>
												{/each}
											</div>
											<Button size="xs" variant="ghost" class="mt-1" onclick={() => editPtVm = null}>Cancel</Button>
										{:else}
											<Button size="xs" variant="outline" class="mt-2" onclick={() => editPtVm = vm.id}>+ Add Device</Button>
										{/if}
									{/if}
								</div>

								{/if}

								<!-- Inline snapshot form -->
								{#if snapshotVm === vm.id}
									<div class="mt-3 rounded border p-3 max-w-sm">
										<Label class="text-xs">Snapshot Name</Label>
										<div class="flex gap-2 mt-1">
											<Input bind:value={snapshotName} placeholder="before-upgrade" class="h-8 text-xs" />
											<Button size="xs" onclick={snapshotVmAction} disabled={!snapshotName}>Create</Button>
											<Button size="xs" variant="ghost" onclick={() => snapshotVm = null}>Cancel</Button>
										</div>
										<span class="mt-1 block text-xs text-muted-foreground">
											{vm.running ? 'Snapshot will attempt to freeze guest filesystems first.' : 'VM is stopped — snapshot will be crash-consistent.'}
										</span>
									</div>
								{/if}

								<!-- Inline clone form -->
								{#if cloneVm === vm.id}
									<div class="mt-3 rounded border p-3 max-w-sm">
										<Label class="text-xs">Clone Name</Label>
										<div class="flex gap-2 mt-1">
											<Input bind:value={cloneName} placeholder="my-vm-copy" class="h-8 text-xs" />
											<Button size="xs" onclick={cloneVmAction} disabled={!cloneName}>Clone</Button>
											<Button size="xs" variant="ghost" onclick={() => cloneVm = null}>Cancel</Button>
										</div>
										<span class="mt-1 block text-xs text-muted-foreground">
											Creates a new VM with COW-cloned disk subvolumes. Passthrough devices are not copied.
										</span>
									</div>
								{/if}
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/if}

<!-- Console Modal -->
{#if consoleVm}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="flex flex-col w-[90vw] max-w-4xl h-[70vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl">
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<div class="flex items-center gap-3">
					<span class="text-sm font-semibold text-white">{consoleVm.name}</span>
					<div class="flex rounded-md overflow-hidden border border-white/20">
						<button
							class="px-2 py-0.5 text-xs transition-colors {consoleMode === 'vnc' ? 'bg-white/20 text-white' : 'text-white/50 hover:text-white/80'}"
							onclick={() => switchConsoleMode('vnc')}
						>Display</button>
						<button
							class="px-2 py-0.5 text-xs transition-colors {consoleMode === 'serial' ? 'bg-white/20 text-white' : 'text-white/50 hover:text-white/80'}"
							onclick={() => switchConsoleMode('serial')}
						>Serial</button>
					</div>
				</div>
				<Button variant="ghost" size="xs" onclick={closeConsole} class="text-white hover:text-white/80">
					Close
				</Button>
			</div>
			{#if consoleMode === 'serial'}
				<div class="flex-1 p-2 overflow-hidden relative" bind:this={consoleEl}>
					<div class="absolute bottom-3 right-4 text-xs text-white/30 pointer-events-none">
						Guest OS must enable serial console (e.g. console=ttyS0,115200)
					</div>
				</div>
			{:else}
				<div class="flex-1 overflow-hidden" bind:this={vncEl}></div>
			{/if}
		</div>
	</div>
{/if}
