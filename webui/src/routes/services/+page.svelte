<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { ProtocolStatus, AppsStatus, Filesystem } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';

	let protocols: ProtocolStatus[] = $state([]);
	let dockerStatus: AppsStatus | null = $state(null);
	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');
	let dockerEnabling = $state(false);
	let loading = $state(true);

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


{#snippet serviceTable(rows: ProtocolStatus[])}
	<table class="w-full max-w-2xl text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Service</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Running</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each rows as proto}
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
							{#if proto.name === 'rest-server'}
								<Button variant="secondary" size="xs" onclick={() => { showRestConfig = !showRestConfig; if (showRestConfig && !restConfigLoaded) loadRestConfig(); }}>
									Configure
								</Button>
							{/if}
						</div>
					</td>
				</tr>
				{#if proto.name === 'rest-server' && showRestConfig}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="4" class="p-3">
							<div class="flex items-end gap-2">
								<div class="flex-1 max-w-md">
									<label for="rest-path" class="text-xs text-muted-foreground">Storage path</label>
									<input
										id="rest-path"
										type="text"
										bind:value={restServerPath}
										placeholder="/fs/first/backups"
										class="mt-1 w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm"
									/>
									<p class="mt-1 text-xs text-muted-foreground">Use a path on bcachefs (e.g. /fs/first/backups). A subvolume will be created automatically if it doesn't exist.</p>
								</div>
								<Button size="sm" onclick={saveRestConfig}>Save</Button>
								<Button size="sm" variant="secondary" onclick={() => showRestConfig = false}>Cancel</Button>
							</div>
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
{/snippet}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<h2 class="mb-3 text-sm font-semibold uppercase tracking-wide text-muted-foreground">Sharing Protocols</h2>
	{@render serviceTable(sharingProtocols)}

	<h2 class="mb-3 mt-8 text-sm font-semibold uppercase tracking-wide text-muted-foreground">App Runtime</h2>
	<table class="w-full max-w-2xl text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Service</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Running</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
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
						<Button variant="secondary" size="xs" onclick={disableDocker}>Disable</Button>
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
		</tbody>
	</table>

	<h2 class="mb-3 mt-8 text-sm font-semibold uppercase tracking-wide text-muted-foreground">System Services</h2>
	{@render serviceTable(systemServices)}
{/if}
