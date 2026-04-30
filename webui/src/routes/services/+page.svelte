<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { ProtocolStatus, AppsStatus, Filesystem, TuningConfig } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';

	let protocols: ProtocolStatus[] = $state([]);
	let dockerStatus: AppsStatus | null = $state(null);
	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');
	let dockerEnabling = $state(false);
	let loading = $state(true);

	// Per-service config panels
	let configOpen = $state<string | null>(null);

	// Tuning
	let tuning: TuningConfig | null = $state(null);
	let savingTuning = $state(false);
	let tNfsThreads = $state(''); let tNfsLeaseTime = $state(''); let tNfsGraceTime = $state('');
	let tSmbMaxConnections = $state(''); let tSmbDeadtime = $state(''); let tSmbSocketOptions = $state('');
	let tIscsiCmdsnDepth = $state(''); let tIscsiLoginTimeout = $state('');

	async function loadTuning() {
		if (tuning) return;
		tuning = await client.call<TuningConfig>('system.tuning.get');
		if (tuning) {
			tNfsThreads = tuning.nfs_threads.toString();
			tNfsLeaseTime = tuning.nfs_lease_time.toString();
			tNfsGraceTime = tuning.nfs_grace_time.toString();
			tSmbMaxConnections = tuning.smb_max_connections.toString();
			tSmbDeadtime = tuning.smb_deadtime.toString();
			tSmbSocketOptions = tuning.smb_socket_options;
			tIscsiCmdsnDepth = tuning.iscsi_default_cmdsn_depth.toString();
			tIscsiLoginTimeout = tuning.iscsi_login_timeout.toString();
		}
	}

	async function saveTuning() {
		savingTuning = true;
		await withToast(
			() => client.call('system.tuning.update', {
				nfs_threads: parseInt(tNfsThreads) || undefined,
				nfs_lease_time: parseInt(tNfsLeaseTime) || undefined,
				nfs_grace_time: parseInt(tNfsGraceTime) || undefined,
				smb_max_connections: parseInt(tSmbMaxConnections) ?? undefined,
				smb_deadtime: parseInt(tSmbDeadtime) ?? undefined,
				smb_socket_options: tSmbSocketOptions || undefined,
				iscsi_default_cmdsn_depth: parseInt(tIscsiCmdsnDepth) || undefined,
				iscsi_login_timeout: parseInt(tIscsiLoginTimeout) || undefined,
			}),
			'Settings applied'
		);
		savingTuning = false;
		tuning = null; // force reload
		await loadTuning();
	}

	function toggleConfig(name: string) {
		if (configOpen === name) { configOpen = null; return; }
		configOpen = name;
		if (['nfs', 'smb', 'iscsi'].includes(name)) loadTuning();
		if (name === 'rest-server' && !restConfigLoaded) loadRestConfig();
	}

	// Backup Server config
	let restServerPath = $state('');
	let showRestConfig = $state(false);
	let restConfigLoaded = $state(false);

	async function loadRestConfig() {
		try {
			const cfg = await client.call<{ path: string }>('service.rest_server.config');
			restServerPath = cfg.path;
			restConfigLoaded = true;
		} catch { /* ignore */ }
	}

	async function saveRestConfig() {
		await withToast(
			() => client.call('service.rest_server.configure', { path: restServerPath }),
			'Backup Server path updated'
		);
		showRestConfig = false;
		await refresh();
	}

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'protocol') refresh();
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		loading = false;
	});

	onDestroy(() => client.offEvent(handleEvent));

	async function refresh() {
		await withToast(async () => {
			[protocols, dockerStatus] = await Promise.all([
				client.call<ProtocolStatus[]>('service.protocol.list'),
				client.call<AppsStatus>('apps.status').catch(() => null),
			]);
		});
	}

	async function loadFilesystems() {
		try { filesystems = await client.call<Filesystem[]>('fs.list'); } catch { /* ignore */ }
		const mounted = filesystems.filter(f => f.mounted);
		if (mounted.length > 0 && !selectedFs) selectedFs = mounted[0].name;
	}

	async function enableDocker() {
		if (!selectedFs) await loadFilesystems();
		dockerEnabling = true;
		await withToast(
			() => client.call('apps.enable', { filesystem: selectedFs || undefined }),
			'Docker enabled — starting runtime'
		);
		dockerEnabling = false;
		// Poll until running
		const poll = setInterval(async () => {
			dockerStatus = await client.call<AppsStatus>('apps.status').catch(() => null);
			if (dockerStatus?.running) { clearInterval(poll); }
		}, 3000);
		setTimeout(() => clearInterval(poll), 60000);
	}

	async function disableDocker() {
		await withToast(() => client.call('apps.disable'), 'Docker disabled');
		await refresh();
	}

	async function toggle(proto: ProtocolStatus) {
		const action = proto.enabled ? 'disable' : 'enable';
		await withToast(
			() => client.call(`service.protocol.${action}`, { name: proto.name }),
			`${proto.display_name} ${proto.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}

	const sharingProtocols = $derived(protocols.filter(p => !p.system_service));
	const systemServices = $derived(protocols.filter(p => p.system_service));
</script>


{#snippet serviceRow(proto: ProtocolStatus)}
				<tr class="border-b border-border">
					<td class="p-3"><strong>{proto.display_name}</strong></td>
					<td class="p-3">
						<Badge variant={proto.enabled ? 'default' : 'secondary'}>
							{proto.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3">
						<span class="inline-block h-2 w-2 rounded-full {proto.running ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
						<span class="ml-1 text-xs text-muted-foreground">{proto.running ? 'Running' : 'Stopped'}</span>
					</td>
					<td class="p-3">
						<div class="flex gap-1.5">
							<Button
								variant={proto.enabled ? 'secondary' : 'default'}
								size="xs"
								onclick={() => toggle(proto)}
							>
								{proto.enabled ? 'Disable' : 'Enable'}
							</Button>
							{#if ['nfs', 'smb', 'iscsi', 'rest-server'].includes(proto.name)}
								<Button variant="secondary" size="xs" onclick={() => toggleConfig(proto.name)}>
									{configOpen === proto.name ? 'Close' : 'Configure'}
								</Button>
							{/if}
						</div>
					</td>
				</tr>
				{#if configOpen === proto.name}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="4" class="p-4">
							{#if proto.name === 'nfs' && tuning}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-3 max-w-xl">
									<div>
										<label for="s-nfs-threads" class="mb-1 block text-xs text-muted-foreground">Threads</label>
										<input id="s-nfs-threads" type="number" min="1" bind:value={tNfsThreads} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Kernel nfsd threads (default: 8).</p>
									</div>
									<div>
										<label for="s-nfs-lease" class="mb-1 block text-xs text-muted-foreground">Lease time (s)</label>
										<input id="s-nfs-lease" type="number" min="1" bind:value={tNfsLeaseTime} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div>
										<label for="s-nfs-grace" class="mb-1 block text-xs text-muted-foreground">Grace time (s)</label>
										<input id="s-nfs-grace" type="number" min="1" bind:value={tNfsGraceTime} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
								</div>
								<Button size="sm" class="mt-3" onclick={saveTuning} disabled={savingTuning}>{savingTuning ? 'Applying...' : 'Apply'}</Button>
							{:else if proto.name === 'smb' && tuning}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-3 max-w-xl">
									<div>
										<label for="s-smb-max" class="mb-1 block text-xs text-muted-foreground">Max connections</label>
										<input id="s-smb-max" type="number" min="0" bind:value={tSmbMaxConnections} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">0 = unlimited.</p>
									</div>
									<div>
										<label for="s-smb-dead" class="mb-1 block text-xs text-muted-foreground">Dead time (min)</label>
										<input id="s-smb-dead" type="number" min="0" bind:value={tSmbDeadtime} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div class="sm:col-span-3">
										<label for="s-smb-sock" class="mb-1 block text-xs text-muted-foreground">Socket options</label>
										<input id="s-smb-sock" type="text" bind:value={tSmbSocketOptions} placeholder="SO_RCVBUF=131072 SO_SNDBUF=131072" class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
									</div>
								</div>
								<Button size="sm" class="mt-3" onclick={saveTuning} disabled={savingTuning}>{savingTuning ? 'Applying...' : 'Apply'}</Button>
							{:else if proto.name === 'iscsi' && tuning}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-2 max-w-md">
									<div>
										<label for="s-iscsi-cmd" class="mb-1 block text-xs text-muted-foreground">Command queue depth</label>
										<input id="s-iscsi-cmd" type="number" min="1" bind:value={tIscsiCmdsnDepth} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div>
										<label for="s-iscsi-timeout" class="mb-1 block text-xs text-muted-foreground">Login timeout (s)</label>
										<input id="s-iscsi-timeout" type="number" min="1" bind:value={tIscsiLoginTimeout} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
								</div>
								<Button size="sm" class="mt-3" onclick={saveTuning} disabled={savingTuning}>{savingTuning ? 'Applying...' : 'Apply'}</Button>
							{:else if proto.name === 'rest-server'}
								<div class="flex items-end gap-2">
									<div class="flex-1 max-w-md">
										<label for="rest-path" class="text-xs text-muted-foreground">Storage path</label>
										<input id="rest-path" type="text" bind:value={restServerPath} placeholder="/fs/first/backups"
											class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm" />
										<p class="mt-1 text-xs text-muted-foreground">Subvolume created automatically if path is under /fs/.</p>
									</div>
									<Button size="sm" onclick={saveRestConfig}>Save</Button>
								</div>
							{:else}
								<p class="text-xs text-muted-foreground">Loading...</p>
							{/if}
						</td>
					</tr>
				{/if}
{/snippet}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<table class="w-full max-w-3xl text-sm">
		<thead>
			<tr>
				<th class="w-[180px] border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Service</th>
				<th class="w-[100px] border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="w-[100px] border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Running</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			<!-- Sharing Protocols -->
			<tr><td colspan="4" class="pt-4 pb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-widest text-muted-foreground/60">Sharing Protocols</td></tr>
			{#each sharingProtocols as proto}
				{@render serviceRow(proto)}
			{/each}

			<!-- App Runtime -->
			<tr><td colspan="4" class="pt-6 pb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-widest text-muted-foreground/60">App Runtime</td></tr>
			<tr class="border-b border-border">
				<td class="p-3"><strong>Docker</strong></td>
				<td class="p-3">
					<Badge variant={dockerStatus?.enabled ? 'default' : 'secondary'}>
						{dockerStatus?.enabled ? 'Enabled' : 'Disabled'}
					</Badge>
				</td>
				<td class="p-3">
					<span class="inline-block h-2 w-2 rounded-full {dockerStatus?.running ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
					<span class="ml-1 text-xs text-muted-foreground">{dockerStatus?.running ? 'Running' : 'Stopped'}</span>
				</td>
				<td class="p-3">
					{#if dockerStatus?.enabled}
						<div class="flex gap-1.5">
							<Button variant="secondary" size="xs" onclick={disableDocker}>Disable</Button>
							<Button variant="secondary" size="xs" onclick={() => configOpen = configOpen === 'docker' ? null : 'docker'}>
								{configOpen === 'docker' ? 'Close' : 'Configure'}
							</Button>
						</div>
					{:else}
						<div class="flex items-center gap-2">
							{#if filesystems.length === 0}
								<Button size="xs" onclick={async () => { await loadFilesystems(); enableDocker(); }} disabled={dockerEnabling}>
									{dockerEnabling ? 'Enabling...' : 'Enable'}
								</Button>
							{:else}
								<select bind:value={selectedFs} class="h-7 rounded-md border border-input bg-transparent px-2 text-xs">
									{#each filesystems.filter(f => f.mounted) as fs}
										<option value={fs.name}>{fs.name}</option>
									{/each}
								</select>
								<Button size="xs" onclick={enableDocker} disabled={dockerEnabling}>
									{dockerEnabling ? 'Enabling...' : 'Enable'}
								</Button>
							{/if}
						</div>
					{/if}
				</td>
			</tr>
			{#if configOpen === 'docker' && dockerStatus?.enabled}
				<tr class="border-b border-border bg-muted/20">
					<td colspan="4" class="p-4">
						<div class="flex flex-wrap gap-4 text-xs">
							{#if dockerStatus.storage_path}
								<span class="text-muted-foreground">Storage: <code class="font-mono">{dockerStatus.storage_path}</code></span>
							{/if}
							{#if dockerStatus.docker_version}
								<span class="text-muted-foreground">Version: {dockerStatus.docker_version}</span>
							{/if}
							{#if dockerStatus.memory_bytes}
								<span class="text-muted-foreground">Memory: {(dockerStatus.memory_bytes / 1048576).toFixed(0)} MiB</span>
							{/if}
						</div>
						<div class="mt-3">
							<Button size="sm" variant="secondary" onclick={async () => {
								await withToast(() => client.call('apps.prune'), 'Cleanup complete');
							}}>Cleanup Unused Images</Button>
						</div>
					</td>
				</tr>
			{/if}
			<!-- System Services -->
			<tr><td colspan="4" class="pt-6 pb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-widest text-muted-foreground/60">System Services</td></tr>
			{#each systemServices as proto}
				{@render serviceRow(proto)}
			{/each}
		</tbody>
	</table>
{/if}
