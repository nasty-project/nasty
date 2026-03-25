<script lang="ts">
	import { onMount } from 'svelte';
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

	let status: AppsStatus | null = $state(null);
	let apps: App[] = $state([]);
	let loading = $state(true);
	let enabling = $state(false);
	let showInstall = $state(false);
	let expanded: Record<string, boolean> = $state({});
	let logsApp: string | null = $state(null);
	let logsContent = $state('');
	let mode: 'easy' | 'expert' = $state('easy');

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

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		try {
			status = await client.call<AppsStatus>('apps.status');
			if (status.enabled && status.running) {
				apps = await client.call<App[]>('apps.list');
			} else {
				apps = [];
			}
		} catch { /* ignore */ }
	}

	async function enableApps() {
		enabling = true;
		await withToast(
			() => client.call('apps.enable'),
			'Apps runtime enabled'
		);
		enabling = false;
		await refresh();
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
		newPorts = [...newPorts, { name: `port${newPorts.length}`, container_port: 8080, node_port: '', protocol: 'TCP' }];
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

		const ok = await withToast(
			() => client.call('apps.install', params),
			'App installed'
		);
		if (ok !== undefined) {
			showInstall = false;
			newName = ''; newImage = ''; newPorts = []; newEnvs = []; newVolumes = [];
			await refresh();
		}
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

<!-- Status Card -->
{#if status}
	<Card class="mb-4">
		<CardContent class="flex items-center gap-4 py-3">
			{#if status.enabled}
				<Badge variant={status.running ? 'default' : 'destructive'}>
					{status.running ? 'Running' : 'Starting...'}
				</Badge>
				<span class="text-sm text-muted-foreground">
					{status.app_count} app{status.app_count !== 1 ? 's' : ''}
					{#if status.memory_bytes}
						&middot; k3s using {formatMemory(status.memory_bytes)}
					{/if}
				</span>
				<Button size="xs" variant="destructive" onclick={disableApps}>
					Disable Apps
				</Button>
			{:else}
				<Badge variant="secondary">Disabled</Badge>
				<span class="text-sm text-muted-foreground">
					App runtime is not running. Enable to deploy containerized applications.
				</span>
			{/if}
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if !status?.enabled}
	<!-- Enable prompt -->
	<Card class="max-w-lg">
		<CardContent class="pt-6 text-center">
			<h3 class="text-lg font-semibold mb-2">App Runtime</h3>
			<p class="text-sm text-muted-foreground mb-4">
				NASty can run containerized applications using a lightweight Kubernetes runtime (k3s).
				This uses approximately 500 MiB–1 GiB of RAM.
			</p>
			<Button onclick={enableApps} disabled={enabling}>
				{enabling ? 'Enabling...' : 'Enable Apps'}
			</Button>
		</CardContent>
	</Card>
{:else if !status?.running}
	<p class="text-muted-foreground">Waiting for app runtime to start...</p>
{:else}
	<!-- Mode tabs -->
	<div class="mb-4 flex items-center gap-4">
		<div class="flex rounded-md overflow-hidden border border-border">
			<button
				class="px-3 py-1.5 text-sm transition-colors {mode === 'easy' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'}"
				onclick={() => mode = 'easy'}
			>Easy</button>
			<button
				class="px-3 py-1.5 text-sm transition-colors {mode === 'expert' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'}"
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
							<Input bind:value={port.name} placeholder="Name" class="h-8 text-xs" />
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
