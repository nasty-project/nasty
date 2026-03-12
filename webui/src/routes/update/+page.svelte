<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { UpdateInfo, UpdateStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';

	let info: UpdateInfo | null = $state(null);
	let status: UpdateStatus | null = $state(null);
	let loading = $state(true);
	let checking = $state(false);
	let pollInterval: ReturnType<typeof setInterval> | null = $state(null);

	const client = getClient();

	onMount(async () => {
		await loadVersion();
		await loadStatus();
		loading = false;
	});

	onDestroy(() => {
		stopPolling();
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
		if (!confirm('Start system update? Services will restart when the update completes.')) return;
		const ok = await withToast(
			() => client.call('system.update.apply'),
			'Update started'
		);
		if (ok !== undefined) {
			startPolling();
		}
	}

	async function rollback() {
		if (!confirm('Rollback to the previous system version?')) return;
		const ok = await withToast(
			() => client.call('system.update.rollback'),
			'Rollback started'
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
				if (status && status.state !== 'running') {
					stopPolling();
					// Reload version info after completion
					await loadVersion();
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
</script>

<h1 class="mb-4 text-2xl font-bold">System Update</h1>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<div class="mb-4 flex items-center justify-between">
				<div>
					<div class="text-sm text-muted-foreground">Current Version</div>
					<div class="font-mono text-lg font-semibold">
						{info?.current_version ?? 'unknown'}
					</div>
				</div>
				{#if info?.latest_version}
					<div class="text-right">
						<div class="text-sm text-muted-foreground">Latest Available</div>
						<div class="font-mono text-lg font-semibold">
							{info.latest_version}
						</div>
					</div>
				{/if}
			</div>

			{#if info?.update_available === true}
				<div class="mb-4">
					<Badge variant="default">Update available</Badge>
				</div>
			{:else if info?.update_available === false}
				<div class="mb-4">
					<Badge variant="secondary">Up to date</Badge>
				</div>
			{/if}

			<div class="flex gap-3">
				<Button onclick={checkForUpdates} disabled={checking || status?.state === 'running'}>
					{checking ? 'Checking...' : 'Check for Updates'}
				</Button>
				{#if info?.update_available}
					<Button onclick={applyUpdate} disabled={status?.state === 'running'}>
						Update Now
					</Button>
				{/if}
				<Button variant="secondary" onclick={rollback} disabled={status?.state === 'running'}>
					Rollback
				</Button>
			</div>
		</CardContent>
	</Card>

	{#if status && status.state !== 'idle'}
		<Card class="max-w-2xl">
			<CardContent class="pt-6">
				<div class="mb-3 flex items-center gap-3">
					<span class="text-sm font-semibold uppercase tracking-wide text-muted-foreground">
						Update Status
					</span>
					<Badge variant={
						status.state === 'running' ? 'default' :
						status.state === 'success' ? 'secondary' :
						'destructive'
					}>
						{status.state === 'running' ? 'In Progress...' :
						 status.state === 'success' ? 'Complete' :
						 'Failed'}
					</Badge>
					{#if status.state === 'running'}
						<span class="inline-block h-2 w-2 animate-pulse rounded-full bg-blue-400"></span>
					{/if}
				</div>
				{#if status.log}
					<pre class="max-h-96 overflow-auto rounded bg-secondary p-3 text-xs leading-relaxed">{status.log}</pre>
				{/if}
			</CardContent>
		</Card>
	{/if}

	<p class="mt-6 max-w-2xl text-xs text-muted-foreground">
		Updates are fetched from GitHub and applied using NixOS rebuild.
		The system will atomically switch to the new version, restarting services as needed.
		If anything goes wrong, use Rollback to return to the previous version.
	</p>
{/if}
