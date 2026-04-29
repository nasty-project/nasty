<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { formatBytes } from '$lib/format';
	import type { BackupProfile, BackupSnapshot, BackupStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Badge } from '$lib/components/ui/badge';

	const client = getClient();
	let profiles: BackupProfile[] = $state([]);
	let loading = $state(true);
	let showCreate = $state(false);
	let backupStatus: BackupStatus | null = $state(null);

	// Create form
	let newName = $state('');
	let newSources = $state('');
	let newTargetType: 'local' | 's3' | 'sftp' | 'rest' | 'b2' = $state('local');
	let newLocalPath = $state('');
	let newS3Endpoint = $state(''); let newS3Bucket = $state(''); let newS3Key = $state(''); let newS3Secret = $state('');
	let newSftpHost = $state(''); let newSftpUser = $state(''); let newSftpPath = $state('');
	let newRestUrl = $state('');
	let newB2Bucket = $state(''); let newB2Id = $state(''); let newB2Key = $state('');
	let newPassword = $state('');
	let newSchedule = $state('');
	let newKeepLast = $state('7');
	let newKeepDaily = $state('7');
	let newKeepWeekly = $state('4');
	let newKeepMonthly = $state('6');

	// Snapshots viewer
	let viewSnapshotsId: string | null = $state(null);
	let snapshots: BackupSnapshot[] = $state([]);
	let snapshotsLoading = $state(false);

	onMount(async () => {
		await refresh();
		loading = false;
	});

	let snapshotCounts: Record<string, number> = $state({});

	async function refresh() {
		try {
			profiles = await client.call<BackupProfile[]>('backup.profile.list');
			backupStatus = await client.call<BackupStatus>('backup.status');
			// Fetch snapshot counts for initialized repos
			for (const p of profiles.filter(p => p.repo_initialized)) {
				client.call<BackupSnapshot[]>('backup.snapshots', { id: p.id })
					.then(snaps => { snapshotCounts[p.id] = snaps.length; })
					.catch(() => {});
			}
		} catch { /* ignore */ }
	}

	async function createProfile() {
		const target = newTargetType === 'local' ? { type: 'local' as const, path: newLocalPath }
			: newTargetType === 's3' ? { type: 's3' as const, endpoint: newS3Endpoint, bucket: newS3Bucket, access_key: newS3Key, secret_key: newS3Secret }
			: newTargetType === 'sftp' ? { type: 'sftp' as const, host: newSftpHost, user: newSftpUser, path: newSftpPath }
			: newTargetType === 'rest' ? { type: 'rest' as const, url: newRestUrl }
			: { type: 'b2' as const, bucket: newB2Bucket, account_id: newB2Id, account_key: newB2Key };

		const profile = {
			id: '',
			name: newName,
			enabled: true,
			sources: newSources.split(',').map(s => s.trim()).filter(Boolean),
			target,
			schedule: newSchedule || null,
			retention: {
				keep_last: parseInt(newKeepLast) || null,
				keep_daily: parseInt(newKeepDaily) || null,
				keep_weekly: parseInt(newKeepWeekly) || null,
				keep_monthly: parseInt(newKeepMonthly) || null,
				keep_yearly: null,
			},
			password: newPassword,
			snapshot_before: true,
			repo_initialized: false,
			last_run: null,
		};

		await withToast(
			() => client.call('backup.profile.create', profile),
			'Backup profile created'
		);
		showCreate = false;
		newName = ''; newSources = ''; newPassword = '';
		await refresh();
	}

	async function deleteProfile(id: string) {
		if (!await confirm('Delete backup profile?', 'The backup repository and its data will NOT be deleted.')) return;
		await withToast(() => client.call('backup.profile.delete', { id }), 'Profile deleted');
		await refresh();
	}

	async function initRepo(id: string) {
		await withToast(() => client.call('backup.repo.init', { id }), 'Repository initialized');
		await refresh();
	}

	async function runBackup(id: string) {
		await withToast(() => client.call('backup.run', { id }), 'Backup started');
		// Poll status
		const poll = setInterval(async () => {
			backupStatus = await client.call<BackupStatus>('backup.status');
			if (!backupStatus?.running) {
				clearInterval(poll);
				await refresh();
			}
		}, 3000);
	}

	async function checkRepo(id: string) {
		await withToast(() => client.call('backup.repo.check', { id }), 'Repository check passed');
	}

	async function loadSnapshots(id: string) {
		viewSnapshotsId = id;
		snapshotsLoading = true;
		try {
			snapshots = await client.call<BackupSnapshot[]>('backup.snapshots', { id });
		} catch { snapshots = []; }
		snapshotsLoading = false;
	}

	function targetSummary(t: BackupProfile['target']): string {
		if (t.type === 'local') return t.path;
		if (t.type === 's3') return `s3://${t.bucket}`;
		if (t.type === 'sftp') return `${t.user}@${t.host}:${t.path}`;
		if (t.type === 'rest') return t.url;
		if (t.type === 'b2') return `b2:${t.bucket}`;
		return '?';
	}
</script>

<div class="space-y-4">
	<div>
		<h1 class="text-2xl font-bold">Backups</h1>
		<p class="text-sm text-muted-foreground mt-0.5">Deduplicating, encrypted backups with retention policies.</p>
	</div>

	<div class="mb-4 flex items-center gap-3">
		<Button size="sm" onclick={() => showCreate = !showCreate}>
			{showCreate ? 'Cancel' : 'Create Backup'}
		</Button>
	</div>

	{#if !profiles.some(p => p.sources.some(s => s.includes('/var/lib/nasty'))) && !showCreate && !(typeof localStorage !== 'undefined' && localStorage.getItem('nasty:config_backup_dismissed') === '1')}
		<div class="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-3 text-sm">
			<div class="flex-1">
				<p class="font-medium text-amber-400">NASty configuration is not backed up</p>
				<p class="mt-1 text-xs text-amber-400/80">
					Your settings, shares, certificates, and user accounts live in <code class="font-mono">/var/lib/nasty</code>.
					Create a backup profile with this path as a source to protect your configuration.
					{#if profiles.length > 0}
						You can also add it as a source to an existing profile.
					{/if}
				</p>
			</div>
			<button onclick={() => {
				showCreate = true;
				newName = 'NASty Config';
				newSources = '/var/lib/nasty';
				newSchedule = '0 3 * * *';
			}} class="text-xs font-medium text-amber-400 hover:text-amber-300 shrink-0">Create backup</button>
			<span class="text-amber-400/30">|</span>
			<button onclick={() => { localStorage.setItem('nasty:config_backup_dismissed', '1'); location.reload(); }} class="text-xs text-amber-400/60 hover:text-amber-400 shrink-0">dismiss</button>
		</div>
	{/if}

	{#if backupStatus?.running}
		<div class="flex items-center gap-2 rounded-md border border-blue-500/30 bg-blue-500/10 px-4 py-2 text-sm text-blue-400">
			<div class="h-3 w-3 animate-spin rounded-full border-2 border-blue-400 border-t-transparent"></div>
			Backup in progress...
		</div>
	{/if}

	{#if showCreate}
		<Card>
			<CardContent class="pt-6 space-y-4">
				<h3 class="text-lg font-semibold">New Backup Profile</h3>

				<div class="grid grid-cols-2 gap-4">
					<div>
						<Label for="bk-name">Name</Label>
						<Input id="bk-name" bind:value={newName} placeholder="Daily offsite" class="mt-1" />
					</div>
					<div>
						<Label for="bk-sources">Sources (comma-separated paths)</Label>
						<Input id="bk-sources" bind:value={newSources} placeholder="/fs/first/media, /fs/first/docs" class="mt-1 font-mono" />
					</div>
				</div>

				<div>
					<Label>Target</Label>
					<div class="mt-1 flex w-fit rounded-md border border-border text-xs">
						{#each ['local', 's3', 'sftp', 'rest', 'b2'] as t}
							<button onclick={() => newTargetType = t as typeof newTargetType}
								class="px-3 py-1.5 font-medium transition-colors first:rounded-l-md last:rounded-r-md {newTargetType === t ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
							>{t.toUpperCase()}</button>
						{/each}
					</div>
				</div>

				{#if newTargetType === 'local'}
					<div>
						<Label for="bk-local-path">Path</Label>
						<Input id="bk-local-path" bind:value={newLocalPath} placeholder="/fs/first/backups" class="mt-1 font-mono" />
					</div>
				{:else if newTargetType === 's3'}
					<div class="grid grid-cols-2 gap-3">
						<div><Label for="bk-s3-ep">Endpoint</Label><Input id="bk-s3-ep" bind:value={newS3Endpoint} placeholder="s3.amazonaws.com" class="mt-1 font-mono" /></div>
						<div><Label for="bk-s3-bk">Bucket</Label><Input id="bk-s3-bk" bind:value={newS3Bucket} placeholder="my-backups" class="mt-1 font-mono" /></div>
						<div><Label for="bk-s3-key">Access Key</Label><Input id="bk-s3-key" bind:value={newS3Key} class="mt-1 font-mono" /></div>
						<div><Label for="bk-s3-sec">Secret Key</Label><Input id="bk-s3-sec" type="password" bind:value={newS3Secret} class="mt-1 font-mono" /></div>
					</div>
				{:else if newTargetType === 'sftp'}
					<div class="grid grid-cols-3 gap-3">
						<div><Label for="bk-sftp-h">Host</Label><Input id="bk-sftp-h" bind:value={newSftpHost} placeholder="backup.example.com" class="mt-1 font-mono" /></div>
						<div><Label for="bk-sftp-u">User</Label><Input id="bk-sftp-u" bind:value={newSftpUser} placeholder="backup" class="mt-1 font-mono" /></div>
						<div><Label for="bk-sftp-p">Path</Label><Input id="bk-sftp-p" bind:value={newSftpPath} placeholder="/backups/nasty" class="mt-1 font-mono" /></div>
					</div>
				{:else if newTargetType === 'rest'}
					<div><Label for="bk-rest">REST URL</Label><Input id="bk-rest" bind:value={newRestUrl} placeholder="https://rest-server:8000/nasty" class="mt-1 font-mono" /></div>
				{:else if newTargetType === 'b2'}
					<div class="grid grid-cols-3 gap-3">
						<div><Label for="bk-b2-bk">Bucket</Label><Input id="bk-b2-bk" bind:value={newB2Bucket} class="mt-1 font-mono" /></div>
						<div><Label for="bk-b2-id">Account ID</Label><Input id="bk-b2-id" bind:value={newB2Id} class="mt-1 font-mono" /></div>
						<div><Label for="bk-b2-key">Account Key</Label><Input id="bk-b2-key" type="password" bind:value={newB2Key} class="mt-1 font-mono" /></div>
					</div>
				{/if}

				<div>
					<Label for="bk-pass">Encryption Password</Label>
					<Input id="bk-pass" type="password" bind:value={newPassword} placeholder="strong-password" class="mt-1" />
					<p class="mt-1 text-xs text-muted-foreground">Used to encrypt the backup repository. Store this safely — losing it means losing access to backups.</p>
				</div>

				<div>
					<Label for="bk-schedule">Schedule (cron, optional)</Label>
					<Input id="bk-schedule" bind:value={newSchedule} placeholder="0 3 * * *" class="mt-1 font-mono" />
					<p class="mt-1 text-xs text-muted-foreground">Examples: <code>0 3 * * *</code> (daily 3am), <code>0 2 * * 0</code> (weekly Sunday 2am), leave empty for manual only.</p>
				</div>

				<div>
					<Label>Retention</Label>
					<div class="mt-1 grid grid-cols-4 gap-3">
						<div><label for="bk-kl" class="text-xs text-muted-foreground">Keep Last</label><Input id="bk-kl" type="number" bind:value={newKeepLast} class="mt-1" /></div>
						<div><label for="bk-kd" class="text-xs text-muted-foreground">Keep Daily</label><Input id="bk-kd" type="number" bind:value={newKeepDaily} class="mt-1" /></div>
						<div><label for="bk-kw" class="text-xs text-muted-foreground">Keep Weekly</label><Input id="bk-kw" type="number" bind:value={newKeepWeekly} class="mt-1" /></div>
						<div><label for="bk-km" class="text-xs text-muted-foreground">Keep Monthly</label><Input id="bk-km" type="number" bind:value={newKeepMonthly} class="mt-1" /></div>
					</div>
				</div>

				<Button onclick={createProfile} disabled={!newName || !newSources || !newPassword}>Create</Button>
			</CardContent>
		</Card>
	{/if}

	{#if loading}
		<p class="text-muted-foreground">Loading...</p>
	{:else if profiles.length === 0 && !showCreate}
		<div class="flex flex-col items-center justify-center py-12 text-center">
			<p class="text-muted-foreground">No backup profiles configured.</p>
			<p class="mt-1 text-sm text-muted-foreground">Create a backup profile to start protecting your data.</p>
		</div>
	{:else}
		<div class="space-y-3">
			{#each profiles as profile}
				<Card>
					<CardContent class="pt-4 pb-4">
						<div class="flex items-start justify-between">
							<div>
								<div class="flex items-center gap-2">
									<span class="font-semibold">{profile.name}</span>
									<Badge variant={profile.repo_initialized ? 'default' : 'secondary'} class="text-[0.6rem]">
										{profile.repo_initialized ? 'Ready' : 'Not initialized'}
									</Badge>
									{#if profile.schedule}
										<Badge variant="outline" class="text-[0.6rem] font-mono">{profile.schedule}</Badge>
									{/if}
								</div>
								<div class="mt-1 text-xs text-muted-foreground font-mono">{targetSummary(profile.target)}</div>
								<div class="mt-0.5 text-xs text-muted-foreground">{profile.sources.join(', ')}</div>
								<div class="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
									{#if snapshotCounts[profile.id] != null}
										<span>{snapshotCounts[profile.id]} snapshot{snapshotCounts[profile.id] !== 1 ? 's' : ''}</span>
									{/if}
									{#if profile.last_run?.bytes_added != null}
										<span>Last added: {formatBytes(profile.last_run.bytes_added)}</span>
									{/if}
									{#if profile.last_run?.files_new != null || profile.last_run?.files_changed != null}
										<span>{profile.last_run.files_new ?? 0} new, {profile.last_run.files_changed ?? 0} changed</span>
									{/if}
								</div>
								{#if profile.last_run}
									<div class="mt-1 text-xs {profile.last_run.success ? 'text-green-400' : 'text-red-400'}">
										Last: {profile.last_run.success ? 'Success' : 'Failed'} — {profile.last_run.timestamp.slice(0, 19).replace('T', ' ')} ({profile.last_run.duration_secs}s)
									</div>
								{/if}
							</div>
							<div class="flex gap-2">
								{#if !profile.repo_initialized}
									<Button size="xs" onclick={() => initRepo(profile.id)}>Init Repo</Button>
								{:else}
									<Button size="xs" variant="secondary" onclick={() => runBackup(profile.id)}
										disabled={backupStatus?.running === true}>
										{backupStatus?.running && backupStatus?.profile_id === profile.id ? 'Running...' : 'Run Now'}
									</Button>
									<Button size="xs" variant="secondary" onclick={() => loadSnapshots(profile.id)}>Snapshots</Button>
									<Button size="xs" variant="secondary" onclick={() => checkRepo(profile.id)}>Check</Button>
								{/if}
								<Button size="xs" variant="destructive" onclick={() => deleteProfile(profile.id)}>Delete</Button>
							</div>
						</div>
					</CardContent>
				</Card>
			{/each}
		</div>
	{/if}
</div>

<!-- Snapshots modal -->
{#if viewSnapshotsId}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="flex flex-col w-[90vw] max-w-3xl max-h-[70vh] rounded-lg border border-border bg-card shadow-2xl">
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<span class="text-sm font-semibold">Snapshots</span>
				<Button variant="ghost" size="xs" onclick={() => viewSnapshotsId = null}>Close</Button>
			</div>
			<div class="flex-1 overflow-auto p-4">
				{#if snapshotsLoading}
					<p class="text-sm text-muted-foreground">Loading...</p>
				{:else if snapshots.length === 0}
					<p class="text-sm text-muted-foreground">No snapshots yet.</p>
				{:else}
					<table class="w-full text-sm">
						<thead>
							<tr class="border-b border-border text-xs text-muted-foreground">
								<th class="p-2 text-left">ID</th>
								<th class="p-2 text-left">Time</th>
								<th class="p-2 text-left">Host</th>
								<th class="p-2 text-left">Paths</th>
							</tr>
						</thead>
						<tbody>
							{#each snapshots as snap}
								<tr class="border-b border-border/50">
									<td class="p-2 font-mono text-xs">{snap.id.slice(0, 8)}</td>
									<td class="p-2 text-xs">{snap.time.slice(0, 19).replace('T', ' ')}</td>
									<td class="p-2 text-xs">{snap.hostname}</td>
									<td class="p-2 text-xs text-muted-foreground">{snap.paths.join(', ')}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			</div>
		</div>
	</div>
{/if}
