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
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';

	let vms: VmStatus[] = $state([]);
	let capabilities: VmCapabilities | null = $state(null);
	let blockSubvolumes: Subvolume[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);
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
	let consoleMode: 'serial' | 'vnc' = $state('serial');
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
		if (showCreate) {
			loadSubvolumes();
			loadFilesystems();
		}
	});

	async function loadFilesystems() {
		try {
			filesystems = await client.call<{ name: string; mounted: boolean }[]>('fs.list');
			filesystems = filesystems.filter(f => f.mounted);
		} catch { /* ignore */ }
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
			const ws = new WebSocket(`${proto}//${window.location.host}/ws/vm/${consoleVm.id}/serial`);
			ws.binaryType = 'arraybuffer';

			ws.onopen = () => {
				ws.send(JSON.stringify({ token }));
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
			const url = `${proto}//${window.location.host}/ws/vm/${consoleVm.id}/vnc`;

			import('@novnc/novnc/lib/rfb.js').then(({ default: RFB }) => {
				// noVNC expects a WebSocket URL. We need to send auth first,
				// so we create a custom WebSocket that sends the token on open.
				const rfb = new RFB(vncEl!, url, {
					wsProtocols: [],
				});
				rfb.scaleViewport = true;
				rfb.resizeSession = true;

				// noVNC doesn't support sending auth on connect natively.
				// We'll use the query param approach instead — but our proxy
				// expects the first message to be auth. For now, we connect
				// directly and the proxy will receive the RFB handshake first.
				// TODO: implement token auth via query param or subprotocol.

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
		await Promise.all([refresh(), loadCapabilities()]);
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
			showCreate = false;
			newName = ''; newCpus = 1; newMemory = 1024; newDisk = '';
			newDiskCreate = false; newDiskFs = ''; newDiskSize = 10;
			newIso = ''; newDescription = ''; newBootOrder = 'disk';
			newAutostart = false; newPassthrough = [];
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
		if (!await confirm('Delete this virtual machine?', 'The VM configuration will be removed. Disk subvolumes are NOT deleted.')) return;
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

	async function toggleAutostart(vm: VmStatus) {
		await withToast(
			() => client.call('vm.update', { id: vm.id, autostart: !vm.autostart }),
			vm.autostart ? 'Autostart disabled' : 'Autostart enabled'
		);
		await refresh();
	}

	function openConsole(vm: VmStatus, mode: 'serial' | 'vnc' = 'serial') {
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
</script>

{#if capabilities}
	<Card class="mb-4">
		<CardContent class="flex items-center gap-4 py-3">
			<Badge variant={capabilities.kvm_available ? 'default' : 'destructive'}>
				{capabilities.kvm_available ? 'KVM Available' : 'No KVM'}
			</Badge>
			<span class="text-sm text-muted-foreground">
				{vms.length} VM{vms.length !== 1 ? 's' : ''}
				&middot; {vms.filter(v => v.running).length} running
				&middot; Arch: <code class="rounded bg-secondary px-1.5 py-0.5 text-xs">{capabilities.arch}</code>
			</span>
			{#if capabilities.uefi_available}
				<Badge variant="secondary">UEFI</Badge>
			{/if}
			{#if capabilities.passthrough_devices.length > 0}
				<Badge variant="secondary">{capabilities.passthrough_devices.length} PCI devices</Badge>
			{/if}
		</CardContent>
	</Card>
{/if}

<div class="mb-4 flex items-center gap-3">
	<Button size="sm" onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create VM'}
	</Button>
	<Input bind:value={search} placeholder="Search..." class="h-9 w-48" />
</div>

{#if showCreate}
	<Card class="mb-6 max-w-lg">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New Virtual Machine</h3>
			<div class="mb-4">
				<Label for="vm-name">Name</Label>
				<Input id="vm-name" bind:value={newName} placeholder="my-vm" class="mt-1" />
			</div>
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
				<Label for="vm-iso">Boot ISO (optional)</Label>
				<Input id="vm-iso" bind:value={newIso} placeholder="/storage/tank/isos/ubuntu.iso" class="mt-1" />
				<span class="mt-1 block text-xs text-muted-foreground">Path to an ISO image for OS installation.</span>
			</div>
			<div class="mb-4">
				<Label for="vm-boot">Boot Order</Label>
				<select id="vm-boot" bind:value={newBootOrder} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="disk">Disk</option>
					<option value="cdrom">CD-ROM (ISO)</option>
					<option value="network">Network (PXE)</option>
				</select>
			</div>
			<div class="mb-4">
				<Label for="vm-desc">Description</Label>
				<Input id="vm-desc" bind:value={newDescription} placeholder="Optional description" class="mt-1" />
			</div>
			{#if availablePciDevices.length > 0}
				<div class="mb-4">
					<Label>PCI Passthrough</Label>
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
				</div>
			{/if}
			<div class="mb-4 flex items-center gap-2">
				<input id="vm-autostart" type="checkbox" bind:checked={newAutostart} class="rounded border-input" />
				<Label for="vm-autostart">Auto-start on NASty boot</Label>
			</div>
			<Button onclick={create} disabled={!newName}>Create</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if vms.length === 0}
	<p class="text-muted-foreground">No virtual machines configured.</p>
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
								<Button variant="outline" size="xs" onclick={() => openConsole(vm)}>
									Console
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
							<div class="space-y-4">
								<!-- Configuration -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Configuration</h4>
									<div class="grid grid-cols-2 gap-x-8 gap-y-1 text-sm">
										<div><span class="text-muted-foreground">CPU:</span> {vm.cpus} core{vm.cpus !== 1 ? 's' : ''}</div>
										<div><span class="text-muted-foreground">Memory:</span> {formatMemory(vm.memory_mib)}</div>
										<div><span class="text-muted-foreground">Boot:</span> {vm.boot_order}</div>
										<div><span class="text-muted-foreground">UEFI:</span> {vm.uefi ? 'Yes' : 'No (Legacy BIOS)'}</div>
										{#if vm.boot_iso}
											<div class="col-span-2"><span class="text-muted-foreground">ISO:</span> <code class="text-xs">{vm.boot_iso}</code></div>
										{/if}
									</div>
								</div>

								<!-- Disks -->
								<div>
									<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Disks</h4>
									{#if vm.disks.length === 0}
										<p class="text-xs text-muted-foreground">No disks attached</p>
									{:else}
										<div class="space-y-1">
											{#each vm.disks as disk, i}
												<div class="flex items-center gap-3 rounded bg-secondary/50 px-2 py-1.5">
													<span class="font-mono text-xs font-semibold">Disk {i}</span>
													<span class="text-xs text-muted-foreground">{disk.path}</span>
													<Badge variant="secondary" class="text-[0.6rem]">{disk.interface}</Badge>
													{#if disk.readonly}
														<Badge variant="secondary" class="text-[0.6rem]">readonly</Badge>
													{/if}
												</div>
											{/each}
										</div>
									{/if}
								</div>

								<!-- Networks -->
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

								<!-- Passthrough -->
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

								<!-- Actions -->
								<div class="flex flex-wrap gap-2 pt-2">
									<Button size="xs" variant="outline" onclick={() => toggleAutostart(vm)}>
										{vm.autostart ? 'Disable Autostart' : 'Enable Autostart'}
									</Button>
									{#if vm.disks.length > 0}
										<Button size="xs" variant="outline" onclick={() => { snapshotVm = vm.id; snapshotName = ''; }}>
											Snapshot
										</Button>
									{/if}
									{#if !vm.running && vm.disks.length > 0}
										<Button size="xs" variant="outline" onclick={() => { cloneVm = vm.id; cloneName = ''; }}>
											Clone
										</Button>
									{/if}
								</div>

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
							class="px-2 py-0.5 text-xs transition-colors {consoleMode === 'serial' ? 'bg-white/20 text-white' : 'text-white/50 hover:text-white/80'}"
							onclick={() => switchConsoleMode('serial')}
						>Serial</button>
						<button
							class="px-2 py-0.5 text-xs transition-colors {consoleMode === 'vnc' ? 'bg-white/20 text-white' : 'text-white/50 hover:text-white/80'}"
							onclick={() => switchConsoleMode('vnc')}
						>VNC</button>
					</div>
				</div>
				<Button variant="ghost" size="xs" onclick={closeConsole} class="text-white hover:text-white/80">
					Close
				</Button>
			</div>
			{#if consoleMode === 'serial'}
				<div class="flex-1 p-2 overflow-hidden" bind:this={consoleEl}></div>
			{:else}
				<div class="flex-1 overflow-hidden" bind:this={vncEl}></div>
			{/if}
		</div>
	</div>
{/if}
