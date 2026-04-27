<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	interface TailscaleStatus {
		enabled: boolean;
		daemon_running: boolean;
		connected: boolean;
		ip?: string;
		hostname?: string;
		version?: string;
		has_auth_key: boolean;
	}

	let tsStatus: TailscaleStatus | null = $state(null);
	let tsAuthKey = $state('');
	let tsLoading = $state(false);

	onMount(async () => {
		try {
			tsStatus = await client.call<TailscaleStatus>('system.tailscale.get');
		} catch { /* tailscale module may not be enabled */ }
	});
</script>

<div>
	<h1 class="text-2xl font-bold">VPN</h1>
	<p class="text-sm text-muted-foreground mt-0.5">Secure remote access via Tailscale.</p>
</div>

<div class="mt-6 max-w-xl space-y-6">
	<div>
		<h3 class="text-lg font-semibold mb-1">Tailscale VPN</h3>
		<p class="text-sm text-muted-foreground">Connect your NASty to a Tailscale network for secure remote access.</p>
	</div>

	{#if !tsStatus}
		<p class="text-muted-foreground">Loading...</p>
	{:else if tsStatus.connected}
		<!-- Connected state -->
		<div class="rounded-lg border border-green-500/30 bg-green-500/5 p-4 space-y-2">
			<div class="flex items-center gap-2">
				<span class="w-2 h-2 rounded-full bg-green-500"></span>
				<span class="text-sm font-medium text-green-500">Connected</span>
			</div>
			{#if tsStatus.ip}
				<div class="text-sm"><span class="text-muted-foreground">Tailscale IP:</span> <span class="font-mono">{tsStatus.ip}</span></div>
			{/if}
			{#if tsStatus.hostname}
				<div class="text-sm"><span class="text-muted-foreground">Hostname:</span> {tsStatus.hostname}</div>
			{/if}
			{#if tsStatus.version}
				<div class="text-sm"><span class="text-muted-foreground">Version:</span> {tsStatus.version}</div>
			{/if}
		</div>

		<Button
			disabled={tsLoading}
			variant="destructive"
			onclick={async () => {
				tsLoading = true;
				const result = await withToast(
					() => client.call('system.tailscale.disconnect'),
					'Tailscale disconnected'
				);
				if (result) {
					tsStatus = result as TailscaleStatus;
					tsAuthKey = '';
				}
				tsLoading = false;
			}}
		>
			{tsLoading ? 'Disconnecting...' : 'Disconnect'}
		</Button>
	{:else}
		<!-- Disconnected state -->
		<div class="rounded-lg border p-4">
			<div class="flex items-center gap-2">
				<span class="w-2 h-2 rounded-full bg-muted-foreground"></span>
				<span class="text-sm text-muted-foreground">Not connected</span>
			</div>
		</div>

		<div class="space-y-4">
			{#if tsStatus?.has_auth_key}
				<p class="text-xs text-muted-foreground">A stored auth key is available. Click Reconnect to use it, or enter a new key below.</p>
				<Button
					disabled={tsLoading}
					onclick={async () => {
						tsLoading = true;
						const result = await withToast(
							() => client.call('system.tailscale.connect', { auth_key: '' }),
							'Tailscale connected'
						);
						if (result) tsStatus = result as TailscaleStatus;
						tsLoading = false;
					}}
				>
					{tsLoading ? 'Connecting...' : 'Reconnect'}
				</Button>
			{/if}

			<div>
				<label for="ts-authkey" class="block text-sm font-medium mb-1">{tsStatus?.has_auth_key ? 'New Auth Key (optional)' : 'Auth Key'}</label>
				<input
					id="ts-authkey"
					type="password"
					bind:value={tsAuthKey}
					placeholder="tskey-auth-..."
					class="w-full max-w-md rounded-md border bg-background px-3 py-2 text-sm"
				/>
				<p class="text-xs text-muted-foreground mt-1">
					Generate at <a href="https://login.tailscale.com/admin/settings/keys" target="_blank" class="underline">Tailscale admin console</a>. Use a reusable key for persistent connections.
				</p>
			</div>

			<Button
				disabled={!tsAuthKey || tsLoading}
				onclick={async () => {
					tsLoading = true;
					const result = await withToast(
						() => client.call('system.tailscale.connect', { auth_key: tsAuthKey }),
						'Tailscale connected'
					);
					if (result) {
						tsStatus = result as TailscaleStatus;
						tsAuthKey = '';
					}
					tsLoading = false;
				}}
			>
				{tsLoading ? 'Connecting...' : 'Connect with new key'}
			</Button>
		</div>
	{/if}
</div>
