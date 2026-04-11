<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { AppsStatus, App, HelmRepo, HelmChart } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import { CircleCheck, Circle } from '@lucide/svelte';
	import type { Filesystem } from '$lib/types';

	let status: AppsStatus | null = $state(null);
	let apps: App[] = $state([]);
	let loading = $state(true);
	let enabling = $state(false);
	let showInstall = $state(false);
	let expanded: Record<string, boolean> = $state({});
	let logsApp: string | null = $state(null);
	let logsContent = $state('');
	let page: 'apps' | 'runtime' = $state('apps');
	let mode: 'easy' | 'expert' = $state('easy');

	// Setup wizard state
	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');

	// Expert mode state
	let repos: HelmRepo[] = $state([]);
	let searchResults: HelmChart[] = $state([]);
	let searchQuery = $state('');
	let searching = $state(false);
	let newRepoName = $state('');
	let newRepoUrl = $state('');
	let showAddRepo = $state(false);

	// Expert install
	let expertInstall: HelmChart | null = $state(null);
	let expertReleaseName = $state('');
	let expertValues = $state('');

	// Install form
	let newName = $state('');
	let newImage = $state('');
	let newPorts = $state<{ name: string; container_port: number; node_port: string; protocol: string }[]>([]);
	let newEnvs = $state<{ name: string; value: string }[]>([]);
	let newVolumes = $state<{ name: string; mount_path: string; size: string }[]>([]);
	let newCpuLimit = $state('');
	let newMemoryLimit = $state('');


	const client = getClient();
	let startupPoll: ReturnType<typeof setInterval> | null = null;

	onMount(async () => {
		await Promise.all([refresh(), loadFilesystems()]);
		loading = false;
		if (status?.enabled && !status?.running) startStartupPolling();
	});

	onDestroy(() => {
		stopStartupPolling();
	});

	function startStartupPolling() {
		stopStartupPolling();
		startupPoll = setInterval(async () => {
			await refresh();
			if (status?.running) stopStartupPolling();
		}, 5000);
	}

	function stopStartupPolling() {
		if (startupPoll) {
			clearInterval(startupPoll);
			startupPoll = null;
		}
	}

	async function loadFilesystems() {
		try {
			const all = await client.call<Filesystem[]>('fs.list');
			filesystems = all.filter(f => f.mounted);
			if (filesystems.length > 0 && !selectedFs) {
				selectedFs = filesystems[0].name;
			}
		} catch { /* ignore */ }
	}

	async function refresh() {
		try {
			status = await client.call<AppsStatus>('apps.status');
			if (status.enabled && status.running) {
				apps = await client.call<App[]>('apps.list');
				await loadForwards();
			} else {
				apps = [];
				forwards = [];
			}
		} catch { /* ignore */ }
	}

	async function enableApps() {
		enabling = true;
		await withToast(
			() => client.call('apps.enable', { filesystem: selectedFs || undefined }),
			'Apps runtime enabled — starting k3s'
		);
		enabling = false;
		await refresh();
		if (status?.enabled && !status?.running) startStartupPolling();
	}

	async function disableApps() {
		if (!await confirm(
			'Disable apps runtime?',
			'All running apps will be stopped. k3s will be shut down to free memory. App configurations are preserved.'
		)) return;
		await withToast(
			() => client.call('apps.disable'),
			'Apps runtime disabled'
		);
		await refresh();
	}

	function addPort() {
		newPorts = [...newPorts, { name: newPorts.length === 0 ? 'http' : `port-${newPorts.length}`, container_port: 8080, node_port: '', protocol: 'TCP' }];
	}

	function removePort(i: number) {
		newPorts = newPorts.filter((_, idx) => idx !== i);
	}

	function addEnv() {
		newEnvs = [...newEnvs, { name: '', value: '' }];
	}

	function removeEnv(i: number) {
		newEnvs = newEnvs.filter((_, idx) => idx !== i);
	}

	function addVolume() {
		newVolumes = [...newVolumes, { name: `data${newVolumes.length}`, mount_path: '', size: '1Gi' }];
	}

	function removeVolume(i: number) {
		newVolumes = newVolumes.filter((_, idx) => idx !== i);
	}

	async function install() {
		if (!newName || !newImage) return;
		const params: Record<string, unknown> = {
			name: newName,
			image: newImage,
		};
		if (newPorts.length > 0) {
			params.ports = newPorts.map(p => ({
				name: p.name,
				container_port: p.container_port,
				node_port: p.node_port ? parseInt(p.node_port) : undefined,
				protocol: p.protocol,
			}));
		}
		if (newEnvs.length > 0) {
			params.env = newEnvs.filter(e => e.name);
		}
		if (newVolumes.length > 0) {
			params.volumes = newVolumes.filter(v => v.name && v.mount_path);
		}
		if (newCpuLimit) params.cpu_limit = newCpuLimit;
		if (newMemoryLimit) params.memory_limit = newMemoryLimit;

		const ok = await withToast(
			() => client.call('apps.install', params),
			'App installed'
		);
		if (ok !== undefined) {
			showInstall = false;
			newName = ''; newImage = ''; newPorts = []; newEnvs = []; newVolumes = [];
			newCpuLimit = ''; newMemoryLimit = '';
		}
		// Always refresh — app may have been partially created even on failure
		await refresh();
	}

	async function removeApp(name: string) {
		if (!await confirm(`Remove app "${name}"?`, 'The app and its resources will be deleted. Persistent volumes may be retained.')) return;
		await withToast(
			() => client.call('apps.remove', { name }),
			'App removed'
		);
		await refresh();
	}

	async function showLogs(name: string) {
		logsApp = name;
		logsContent = 'Loading...';
		try {
			logsContent = await client.call<string>('apps.logs', { name, tail: 200 });
		} catch (e) {
			logsContent = `Failed to load logs: ${e}`;
		}
	}

	// Port-forward functions
	let forwards: { name: string; local_port: number; container_port: number; pod: string }[] = $state([]);

	async function loadForwards() {
		try { forwards = await client.call('apps.forward.list'); } catch { forwards = []; }
	}

	async function startForward(appName: string) {
		await withToast(
			() => client.call('apps.forward.start', { name: appName }),
			'Port-forward started'
		);
		await loadForwards();
	}

	async function stopForward(appName: string) {
		await withToast(
			() => client.call('apps.forward.stop', { name: appName }),
			'Port-forward stopped'
		);
		await loadForwards();
	}

	function getForward(appName: string) {
		return forwards.find(f => f.name === appName);
	}

	// Expert mode functions
	async function loadRepos() {
		try {
			repos = await client.call<HelmRepo[]>('apps.repo.list');
		} catch { repos = []; }
	}

	async function addRepo() {
		if (!newRepoName || !newRepoUrl) return;
		await withToast(
			() => client.call('apps.repo.add', { name: newRepoName, url: newRepoUrl }),
			'Repo added'
		);
		showAddRepo = false;
		newRepoName = ''; newRepoUrl = '';
		await loadRepos();
	}

	async function removeRepo(name: string) {
		if (!await confirm(`Remove Helm repo "${name}"?`)) return;
		await withToast(() => client.call('apps.repo.remove', { name }), 'Repo removed');
		await loadRepos();
	}

	async function updateRepos() {
		await withToast(() => client.call('apps.repo.update'), 'Repos updated');
	}

	async function searchCharts() {
		if (!searchQuery.trim()) { searchResults = []; return; }
		searching = true;
		try {
			searchResults = await client.call<HelmChart[]>('apps.search', { query: searchQuery });
		} catch { searchResults = []; }
		searching = false;
	}

	async function installChart() {
		if (!expertInstall || !expertReleaseName) return;
		const params: Record<string, unknown> = {
			name: expertReleaseName,
			chart: `${expertInstall.repo}/${expertInstall.name}`,
			version: expertInstall.version,
		};
		if (expertValues.trim()) {
			try {
				params.values = JSON.parse(expertValues);
			} catch {
				await withToast(async () => { throw new Error('Invalid JSON values'); }, '');
				return;
			}
		}
		const ok = await withToast(
			() => client.call('apps.install_chart', params),
			'Chart installed'
		);
		if (ok !== undefined) {
			expertInstall = null;
			expertReleaseName = ''; expertValues = '';
			await refresh();
		}
	}

	$effect(() => {
		if (mode === 'expert' && status?.running) loadRepos();
	});

	function formatMemory(bytes: number): string {
		if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
		if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(0)} MiB`;
		return `${bytes} B`;
	}

	let search = $state('');
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort() {
		sortDir = sortDir === 'asc' ? 'desc' : 'asc';
	}

	const filtered = $derived(
		search.trim()
			? apps.filter(a => a.name.toLowerCase().includes(search.toLowerCase()))
			: apps
	);

	const sorted = $derived.by(() => {
		return [...filtered].sort((a, b) => {
			const cmp = a.name.localeCompare(b.name);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if !status?.enabled}
	<!-- Setup Wizard -->
	<Card class="mb-4 max-w-2xl">
		<CardContent class="pt-6 pb-4">
			<h3 class="mb-1 text-lg font-semibold">Apps Setup</h3>
			<p class="mb-4 text-sm text-muted-foreground">
				NASty runs containerized applications using a lightweight Kubernetes runtime (k3s).
				Uses approximately 500 MiB–1 GiB of RAM.
			</p>

			<div class="space-y-2">
				<div class="flex items-start gap-3 rounded-lg border border-border px-3 py-2.5">
					{#if filesystems.length > 0}
						<CircleCheck size={18} class="mt-0.5 shrink-0 text-green-500" />
					{:else}
						<Circle size={18} class="mt-0.5 shrink-0 text-muted-foreground" />
					{/if}
					<div class="flex-1 min-w-0">
						<div class="text-sm font-medium">Storage Filesystem</div>
						<div class="text-xs text-muted-foreground">
							{filesystems.length > 0
								? `${filesystems.length} filesystem${filesystems.length !== 1 ? 's' : ''} available`
								: 'No filesystem found — create one in Storage first'}
						</div>
					</div>
					{#if filesystems.length === 0}
						<Button size="xs" variant="outline" onclick={() => window.location.href = '/filesystems'}>
							Go to Storage
						</Button>
					{/if}
				</div>

				<div class="flex items-start gap-3 rounded-lg border border-border px-3 py-2.5">
					{#if filesystems.length > 0}
						<CircleCheck size={18} class="mt-0.5 shrink-0 text-green-500" />
					{:else}
						<Circle size={18} class="mt-0.5 shrink-0 text-muted-foreground" />
					{/if}
					<div class="flex-1 min-w-0">
						<div class="text-sm font-medium">App Storage Location</div>
						{#if filesystems.length > 0}
							<div class="mt-1">
								<select bind:value={selectedFs} class="h-7 rounded-md border border-input bg-transparent px-2 text-xs">
									{#each filesystems as fs}
										<option value={fs.name}>{fs.name}</option>
									{/each}
								</select>
								<span class="ml-2 text-xs text-muted-foreground">Subvolume "apps-data" will be created on this filesystem</span>
							</div>
						{:else}
							<div class="text-xs text-muted-foreground">Requires a filesystem first</div>
						{/if}
					</div>
				</div>
			</div>

			<div class="mt-4">
				<Button onclick={enableApps} disabled={enabling || filesystems.length === 0}>
					{enabling ? 'Enabling...' : 'Enable Apps'}
				</Button>
			</div>
		</CardContent>
	</Card>
{:else if !status?.running}
	<Card>
		<CardContent class="py-8 text-center">
			<div class="mx-auto mb-4 h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary"></div>
			<p class="font-medium">Starting app runtime</p>
			<p class="mt-1 text-sm text-muted-foreground">k3s is bootstrapping. This can take up to a minute on first start.</p>
		</CardContent>
	</Card>
{:else}
	<!-- Top-level tabs with inline status -->
	<div class="mb-6 flex items-center border-b border-border">
		<button
			onclick={() => page = 'apps'}
			class="flex items-center gap-2 px-4 py-2 text-sm font-medium transition-colors {page === 'apps'
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>
			Apps
			{#if status}
				<span class="inline-block h-1.5 w-1.5 rounded-full {status.running ? 'bg-green-500' : 'bg-muted-foreground/40'}"></span>
				{#if status.app_count > 0}
					<span class="text-[0.65rem] text-muted-foreground">{status.app_count}</span>
				{/if}
			{/if}
		</button>
		<button
			onclick={() => page = 'runtime'}
			class="px-4 py-2 text-sm font-medium transition-colors {page === 'runtime'
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>Runtime</button>
		{#if !status?.storage_ok && status?.storage_path}
			<span class="ml-auto text-xs text-destructive">Storage missing</span>
		{/if}
	</div>

	{#if page === 'apps'}
	<!-- Sub-tabs -->
	<div class="mb-4 flex items-center gap-4">
		<div class="flex border-b border-border">
			<button
				class="px-3 py-1.5 text-sm font-medium transition-colors {mode === 'easy'
					? 'border-b-2 border-primary text-foreground'
					: 'text-muted-foreground hover:text-foreground'}"
				onclick={() => mode = 'easy'}
			>Easy</button>
			<button
				class="px-3 py-1.5 text-sm font-medium transition-colors {mode === 'expert'
					? 'border-b-2 border-primary text-foreground'
					: 'text-muted-foreground hover:text-foreground'}"
				onclick={() => mode = 'expert'}
			>Helm Charts</button>
		</div>
		{#if mode === 'easy'}
			<Button size="sm" onclick={() => showInstall = !showInstall}>
				{showInstall ? 'Cancel' : 'Install App'}
			</Button>
		{/if}
		<Input bind:value={search} placeholder="Search installed..." class="h-9 w-48" />
	</div>

	{#if mode === 'easy'}
	{#if showInstall}
		<Card class="mb-6 max-w-xl">
			<CardContent class="pt-6">
				<h3 class="mb-4 text-lg font-semibold">Install App</h3>
				<div class="mb-4">
					<Label for="app-name">App Name</Label>
					<Input id="app-name" bind:value={newName} placeholder="plex" class="mt-1" />
					<span class="mt-1 block text-xs text-muted-foreground">Must be DNS-safe (lowercase, no spaces).</span>
				</div>
				<div class="mb-4">
					<Label for="app-image">Container Image</Label>
					<Input id="app-image" bind:value={newImage} placeholder="lscr.io/linuxserver/plex:latest" class="mt-1" />
				</div>

				<!-- Ports -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Ports</Label>
						<Button size="xs" variant="outline" onclick={addPort}>+ Add Port</Button>
					</div>
					{#each newPorts as port, i}
						<div class="grid grid-cols-[1fr_80px_90px_60px_auto] gap-2 mt-1 items-center">
							<Input bind:value={port.name} placeholder="e.g. http" class="h-8 text-xs" />
							<Input type="number" bind:value={port.container_port} placeholder="Port" class="h-8 text-xs" />
							<Input bind:value={port.node_port} placeholder="NodePort" class="h-8 text-xs" />
							<select bind:value={port.protocol} class="h-8 rounded-md border border-input bg-transparent px-1 text-xs">
								<option>TCP</option>
								<option>UDP</option>
							</select>
							<Button size="xs" variant="ghost" onclick={() => removePort(i)}>x</Button>
						</div>
					{/each}
				</div>

				<!-- Environment Variables -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Environment Variables</Label>
						<Button size="xs" variant="outline" onclick={addEnv}>+ Add</Button>
					</div>
					{#each newEnvs as env, i}
						<div class="grid grid-cols-[1fr_1fr_auto] gap-2 mt-1 items-center">
							<Input bind:value={env.name} placeholder="Name" class="h-8 text-xs" />
							<Input bind:value={env.value} placeholder="Value" class="h-8 text-xs" />
							<Button size="xs" variant="ghost" onclick={() => removeEnv(i)}>x</Button>
						</div>
					{/each}
				</div>

				<!-- Volumes -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Volumes</Label>
						<Button size="xs" variant="outline" onclick={addVolume}>+ Add Volume</Button>
					</div>
					{#each newVolumes as vol, i}
						<div class="grid grid-cols-[1fr_1fr_80px_auto] gap-2 mt-1 items-center">
							<Input bind:value={vol.name} placeholder="Name" class="h-8 text-xs" />
							<Input bind:value={vol.mount_path} placeholder="/config" class="h-8 text-xs" />
							<Input bind:value={vol.size} placeholder="1Gi" class="h-8 text-xs" />
							<Button size="xs" variant="ghost" onclick={() => removeVolume(i)}>x</Button>
						</div>
					{/each}
					{#if newVolumes.length > 0}
						<span class="mt-1 block text-xs text-muted-foreground">Storage provided by nasty-csi via bcachefs subvolumes.</span>
					{/if}
				</div>

				<!-- Resource Limits -->
				<div class="mb-4">
					<Label>Resource Limits (optional)</Label>
					<div class="grid grid-cols-2 gap-3 mt-1">
						<div>
							<Label class="text-xs">CPU</Label>
							<Input bind:value={newCpuLimit} placeholder="e.g. 500m or 2" class="mt-1 h-8 text-xs" />
						</div>
						<div>
							<Label class="text-xs">Memory</Label>
							<Input bind:value={newMemoryLimit} placeholder="e.g. 256Mi or 1Gi" class="mt-1 h-8 text-xs" />
						</div>
					</div>
				</div>

				<Button onclick={install} disabled={!newName || !newImage}>Install</Button>
			</CardContent>
		</Card>
	{/if}

	{#if apps.length === 0 && !showInstall}
		<p class="text-muted-foreground">No apps installed.</p>
	{:else if apps.length > 0}
		<table class="w-full text-sm">
			<thead>
				<tr>
					<SortTh label="Name" active={true} dir={sortDir} onclick={toggleSort} />
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Chart</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each sorted as app}
					<tr class="border-b border-border hover:bg-muted/30 transition-colors">
						<td class="p-3">
							<span class="font-semibold">{app.name}</span>
						</td>
						<td class="p-3 text-xs text-muted-foreground font-mono">
							{app.chart}
						</td>
						<td class="p-3">
							<Badge variant={app.status === 'deployed' ? 'default' : 'secondary'}>
								{app.status}
							</Badge>
						</td>
						<td class="p-3">
							<div class="flex gap-2">
								{#if getForward(app.name)}
									<a href="http://{window.location.hostname}:{getForward(app.name)?.local_port}" target="_blank" class="text-xs text-blue-500 hover:underline self-center">:{getForward(app.name)?.local_port}</a>
									<Button variant="secondary" size="xs" onclick={() => stopForward(app.name)}>Stop</Button>
								{:else}
									<Button variant="outline" size="xs" onclick={() => startForward(app.name)}>Access</Button>
								{/if}
								<Button variant="outline" size="xs" onclick={() => showLogs(app.name)}>
									Logs
								</Button>
								<Button variant="destructive" size="xs" onclick={() => removeApp(app.name)}>
									Remove
								</Button>
							</div>
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
	{:else}
	<!-- Expert mode: Helm repos + chart search -->
	<div class="grid grid-cols-1 lg:grid-cols-2 gap-6">
		<!-- Repos -->
		<Card>
			<CardContent class="pt-6">
				<div class="flex items-center justify-between mb-4">
					<h3 class="text-lg font-semibold">Helm Repositories</h3>
					<div class="flex gap-2">
						<Button size="xs" variant="outline" onclick={updateRepos}>Refresh</Button>
						<Button size="xs" onclick={() => showAddRepo = !showAddRepo}>
							{showAddRepo ? 'Cancel' : 'Add Repo'}
						</Button>
					</div>
				</div>

				{#if showAddRepo}
					<div class="mb-4 rounded border p-3">
						<div class="grid grid-cols-2 gap-2 mb-2">
							<div>
								<Label class="text-xs">Name</Label>
								<Input bind:value={newRepoName} placeholder="bitnami" class="mt-1 h-8 text-xs" />
							</div>
							<div>
								<Label class="text-xs">URL</Label>
								<Input bind:value={newRepoUrl} placeholder="https://charts.bitnami.com/bitnami" class="mt-1 h-8 text-xs" />
							</div>
						</div>
						<Button size="xs" onclick={addRepo} disabled={!newRepoName || !newRepoUrl}>Add</Button>
					</div>
				{/if}

				{#if repos.length === 0}
					<p class="text-sm text-muted-foreground">No repositories configured.</p>
				{:else}
					<div class="space-y-1">
						{#each repos as repo}
							<div class="flex items-center justify-between rounded bg-secondary/50 px-3 py-2">
								<div>
									<span class="font-semibold text-sm">{repo.name}</span>
									<span class="ml-2 text-xs text-muted-foreground truncate">{repo.url}</span>
								</div>
								<Button variant="destructive" size="xs" onclick={() => removeRepo(repo.name)}>Remove</Button>
							</div>
						{/each}
					</div>
				{/if}
			</CardContent>
		</Card>

		<!-- Chart Search -->
		<Card>
			<CardContent class="pt-6">
				<h3 class="text-lg font-semibold mb-4">Search Charts</h3>
				<div class="flex gap-2 mb-4">
					<Input bind:value={searchQuery} placeholder="postgresql, redis, grafana..." class="h-9"
						onkeydown={(e: KeyboardEvent) => e.key === 'Enter' && searchCharts()} />
					<Button size="sm" onclick={searchCharts} disabled={searching}>
						{searching ? 'Searching...' : 'Search'}
					</Button>
				</div>

				{#if searchResults.length > 0}
					<div class="max-h-80 overflow-y-auto space-y-1">
						{#each searchResults as chart}
							<div class="rounded border px-3 py-2 hover:bg-muted/30 transition-colors">
								<div class="flex items-center justify-between">
									<div>
										<span class="font-semibold text-sm">{chart.repo}/{chart.name}</span>
										<Badge variant="secondary" class="ml-2 text-[0.6rem]">v{chart.version}</Badge>
										{#if chart.app_version}
											<span class="ml-1 text-xs text-muted-foreground">app: {chart.app_version}</span>
										{/if}
									</div>
									<Button size="xs" variant="outline" onclick={() => { expertInstall = chart; expertReleaseName = chart.name; expertValues = ''; }}>
										Install
									</Button>
								</div>
								{#if chart.description}
									<p class="text-xs text-muted-foreground mt-1">{chart.description}</p>
								{/if}
							</div>
						{/each}
					</div>
				{:else if searchQuery && !searching}
					<p class="text-sm text-muted-foreground">No charts found.</p>
				{/if}
			</CardContent>
		</Card>
	</div>

	<!-- Expert install dialog -->
	{#if expertInstall}
		<Card class="mt-6 max-w-xl">
			<CardContent class="pt-6">
				<h3 class="mb-4 text-lg font-semibold">Install {expertInstall.repo}/{expertInstall.name}</h3>
				<div class="mb-4">
					<Label for="expert-name">Release Name</Label>
					<Input id="expert-name" bind:value={expertReleaseName} class="mt-1" />
				</div>
				<div class="mb-4">
					<Label for="expert-values">Values (JSON, optional)</Label>
					<textarea
						id="expert-values"
						bind:value={expertValues}
						placeholder={'{"key": "value"}'}
						class="mt-1 w-full h-32 rounded-md border border-input bg-transparent px-3 py-2 text-sm font-mono"
					></textarea>
					<span class="mt-1 block text-xs text-muted-foreground">Override default chart values. Must be valid JSON.</span>
				</div>
				<div class="flex gap-2">
					<Button onclick={installChart} disabled={!expertReleaseName}>Install</Button>
					<Button variant="ghost" onclick={() => expertInstall = null}>Cancel</Button>
				</div>
			</CardContent>
		</Card>
	{/if}

	<!-- Installed apps table (visible in both modes) -->
	{#if apps.length > 0}
		<h3 class="text-lg font-semibold mt-6 mb-3">Installed Apps</h3>
		<table class="w-full text-sm">
			<thead>
				<tr>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Name</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Chart</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground w-px whitespace-nowrap">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each apps as app}
					<tr class="border-b border-border hover:bg-muted/30">
						<td class="p-3 font-semibold">{app.name}</td>
						<td class="p-3 text-xs text-muted-foreground font-mono">{app.chart}</td>
						<td class="p-3"><Badge variant={app.status === 'deployed' ? 'default' : 'secondary'}>{app.status}</Badge></td>
						<td class="p-3">
							<div class="flex gap-2">
								{#if getForward(app.name)}
									<a href="http://{window.location.hostname}:{getForward(app.name)?.local_port}" target="_blank" class="text-xs text-blue-500 hover:underline self-center">:{getForward(app.name)?.local_port}</a>
									<Button variant="secondary" size="xs" onclick={() => stopForward(app.name)}>Stop</Button>
								{:else}
									<Button variant="outline" size="xs" onclick={() => startForward(app.name)}>Access</Button>
								{/if}
								<Button variant="outline" size="xs" onclick={() => showLogs(app.name)}>Logs</Button>
								<Button variant="destructive" size="xs" onclick={() => removeApp(app.name)}>Remove</Button>
							</div>
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
	{/if}
	{:else if page === 'runtime'}
	<!-- Runtime tab -->
	<div class="max-w-2xl space-y-4">
		<Card>
			<CardContent class="pt-6">
				<h4 class="mb-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Cluster</h4>
				<div class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
					<span class="text-muted-foreground">k3s Version</span>
					<span class="font-mono text-xs">{status?.k3s_version ?? 'Unknown'}</span>
					<span class="text-muted-foreground">Node Status</span>
					<span>
						<Badge variant={status?.node_status === 'Ready' ? 'default' : 'destructive'}>
							{status?.node_status ?? 'Unknown'}
						</Badge>
					</span>
					<span class="text-muted-foreground">Apps</span>
					<span>{status?.app_count ?? 0} deployed</span>
					{#if status?.memory_bytes}
						<span class="text-muted-foreground">Memory</span>
						<span>{formatMemory(status.memory_bytes)}</span>
					{/if}
				</div>
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-6">
				<h4 class="mb-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Storage</h4>
				<div class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
					<span class="text-muted-foreground">Path</span>
					<div>
						<code class="text-xs">{status?.storage_path ?? 'Not configured'}</code>
						{#if status && !status.storage_ok && status.storage_path}
							<Badge variant="destructive" class="ml-2 text-[0.6rem]">Missing</Badge>
						{/if}
					</div>
					<span class="text-muted-foreground">Status</span>
					<span>{status?.storage_ok ? 'OK' : 'Not available'}</span>
					<span class="text-muted-foreground">Provisioner</span>
					<span>local-path-provisioner</span>
				</div>
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-6">
				<h4 class="mb-3 text-xs font-semibold uppercase tracking-wide text-destructive">Danger Zone</h4>
				<p class="mb-3 text-sm text-muted-foreground">
					Disabling apps stops the k3s runtime and all running containers. App data on the filesystem is preserved.
				</p>
				<Button variant="destructive" size="sm" onclick={disableApps}>
					Disable Apps
				</Button>
			</CardContent>
		</Card>
	</div>
	{/if}
{/if}

<!-- Logs Modal -->
{#if logsApp}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="flex flex-col w-[90vw] max-w-4xl h-[70vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl">
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<span class="text-sm font-semibold text-white">Logs: {logsApp}</span>
				<Button variant="ghost" size="xs" onclick={() => logsApp = null} class="text-white hover:text-white/80">
					Close
				</Button>
			</div>
			<pre class="flex-1 p-4 overflow-auto text-xs text-green-400 font-mono whitespace-pre-wrap">{logsContent}</pre>
		</div>
	</div>
{/if}
