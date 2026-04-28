<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { Settings } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let settings: Settings | null = $state(null);
	let tlsDomain = $state('');
	let tlsAcmeEmail = $state('');
	let tlsAcmeEnabled = $state(false);
	let acmeStatus: { state: string; message: string; domain?: string; last_attempt?: string } | null = $state(null);
	let tlsAcmeStaging = $state(false);
	let tlsChallengeType = $state<'tls-alpn' | 'dns'>('tls-alpn');
	let tlsDnsProvider = $state('');
	let tlsDnsCredentials = $state('');
	let savingTls = $state(false);
	let tlsChanged = $state(false);

	const popularDnsProviders = [
		{ code: 'cloudflare', name: 'Cloudflare' },
		{ code: 'route53', name: 'Amazon Route 53' },
		{ code: 'gcloud', name: 'Google Cloud' },
		{ code: 'azuredns', name: 'Azure DNS' },
		{ code: 'digitalocean', name: 'DigitalOcean' },
		{ code: 'hetzner', name: 'Hetzner' },
		{ code: 'godaddy', name: 'GoDaddy' },
		{ code: 'namecheap', name: 'Namecheap' },
		{ code: 'ovh', name: 'OVH' },
		{ code: 'porkbun', name: 'Porkbun' },
		{ code: 'vultr', name: 'Vultr' },
		{ code: 'linode', name: 'Linode' },
		{ code: 'duckdns', name: 'Duck DNS' },
		{ code: 'desec', name: 'deSEC.io' },
		{ code: 'oraclecloud', name: 'Oracle Cloud' },
	];

	onMount(async () => {
		settings = await client.call<Settings>('system.settings.get');
		tlsDomain = settings?.tls_domain ?? '';
		tlsAcmeEmail = settings?.tls_acme_email ?? '';
		tlsAcmeEnabled = settings?.tls_acme_enabled ?? false;
		tlsChallengeType = settings?.tls_challenge_type ?? 'tls-alpn';
		tlsDnsProvider = settings?.tls_dns_provider ?? '';
		tlsDnsCredentials = settings?.tls_dns_credentials ?? '';
		tlsAcmeStaging = (settings as any)?.tls_acme_staging ?? false;
		try { acmeStatus = await client.call('system.acme.status'); } catch { /* ignore */ }
	});

	async function saveTls() {
		savingTls = true;
		const result = await withToast(
			() => client.call<Settings>('system.settings.update', {
				tls_domain: tlsDomain || null,
				tls_acme_email: tlsAcmeEmail || null,
				tls_acme_enabled: tlsAcmeEnabled,
				tls_challenge_type: tlsChallengeType,
				tls_dns_provider: tlsDnsProvider || null,
				tls_dns_credentials: tlsDnsCredentials || null,
				tls_acme_staging: tlsAcmeStaging,
			}),
			tlsAcmeEnabled ? 'Let\'s Encrypt certificate requested — check status below' : 'TLS settings saved'
		);
		if (result !== undefined) {
			settings = result;
			tlsChanged = false;
			if (tlsAcmeEnabled) {
				const poll = setInterval(async () => {
					try { acmeStatus = await client.call('system.acme.status'); } catch { /* ignore */ }
					if (acmeStatus && (acmeStatus.state === 'success' || acmeStatus.state === 'error')) {
						clearInterval(poll);
					}
				}, 2000);
				setTimeout(() => clearInterval(poll), 300000); // 5 min (DNS propagation can be slow)
			}
		}
		savingTls = false;
	}
</script>

<div>
	<h1 class="text-2xl font-bold">TLS Certificate</h1>
	<p class="text-sm text-muted-foreground mt-0.5">Manage HTTPS certificates for the NASty web interface.</p>
</div>

<div class="mt-6 grid grid-cols-1 gap-6 lg:grid-cols-2">
	<section class="rounded-lg border border-border p-5">
		<p class="mb-5 text-sm text-muted-foreground">
			NASty uses a self-signed certificate by default. Enable Let's Encrypt for a trusted certificate
			that browsers accept without warnings.
		</p>

		<div class="mb-4">
			<label class="flex items-center gap-2 text-sm cursor-pointer">
				<input
					type="checkbox"
					bind:checked={tlsAcmeEnabled}
					onchange={() => tlsChanged = true}
					class="rounded border-input"
				/>
				<span class="font-medium">Enable Let's Encrypt</span>
			</label>
			{#if tlsAcmeEnabled}
				<label class="flex items-center gap-2 text-xs text-muted-foreground cursor-pointer mt-2 ml-6">
					<input type="checkbox" bind:checked={tlsAcmeStaging} onchange={() => tlsChanged = true} class="rounded border-input" />
					Use staging environment (for testing, certs not trusted by browsers)
				</label>
			{/if}
		</div>

		{#if tlsAcmeEnabled}
			<div class="mb-4">
				<label for="tls-domain" class="mb-1 block text-xs text-muted-foreground">Domain Name</label>
				<input
					id="tls-domain"
					type="text"
					bind:value={tlsDomain}
					oninput={() => tlsChanged = true}
					class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
					placeholder="nasty.example.com"
				/>
				<span class="mt-1 block text-xs text-muted-foreground">Must resolve to this machine's public IP.</span>
			</div>

			<div class="mb-4">
				<label for="tls-email" class="mb-1 block text-xs text-muted-foreground">Email</label>
				<input
					id="tls-email"
					type="email"
					bind:value={tlsAcmeEmail}
					oninput={() => tlsChanged = true}
					class="w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
					placeholder="admin@example.com"
				/>
				<span class="mt-1 block text-xs text-muted-foreground">Let's Encrypt sends expiry warnings here.</span>
			</div>

			<div class="mb-4">
				<span class="mb-1 block text-xs text-muted-foreground">Challenge Type</span>
				<div class="flex w-fit rounded-md border border-border text-sm">
					<button
						onclick={() => { tlsChallengeType = 'tls-alpn'; tlsChanged = true; }}
						class="rounded-l-md px-4 py-1.5 font-medium transition-colors {tlsChallengeType === 'tls-alpn' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
					>TLS (port 443)</button>
					<button
						onclick={() => { tlsChallengeType = 'dns'; tlsChanged = true; }}
						class="rounded-r-md px-4 py-1.5 font-medium transition-colors {tlsChallengeType === 'dns' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
					>DNS</button>
				</div>
			</div>

			{#if tlsChallengeType === 'tls-alpn'}
				<div class="mb-4 rounded-lg border border-blue-800 bg-blue-950 px-4 py-3 text-xs text-blue-200">
					The TLS-ALPN-01 challenge verifies domain ownership over port 443. No additional ports needed,
					but port 443 must be reachable from the internet.
				</div>
			{:else}
				<div class="mb-4">
					<label for="tls-dns-provider" class="mb-1 block text-xs text-muted-foreground">DNS Provider</label>
					<select
						id="tls-dns-provider"
						bind:value={tlsDnsProvider}
						onchange={() => tlsChanged = true}
						class="w-full rounded-md border border-input bg-transparent px-3 py-1.5 text-sm"
					>
						<option value="">Select provider...</option>
						{#each popularDnsProviders as p}
							<option value={p.code}>{p.name}</option>
						{/each}
						<option disabled>───────────</option>
						<option value="_custom">Other (enter code manually)</option>
					</select>
					{#if tlsDnsProvider === '_custom'}
						<input
							type="text"
							bind:value={tlsDnsProvider}
							oninput={() => tlsChanged = true}
							class="mt-2 w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
							placeholder="provider code (e.g. inwx, gandi)"
						/>
					{/if}
					<span class="mt-1 block text-xs text-muted-foreground">
						See <a href="https://go-acme.github.io/lego/dns/" target="_blank" class="text-blue-400 hover:underline">lego DNS providers</a> for the full list and required credentials.
					</span>
				</div>

				<div class="mb-4">
					<label for="tls-dns-creds" class="mb-1 block text-xs text-muted-foreground">API Credentials</label>
					<textarea
						id="tls-dns-creds"
						bind:value={tlsDnsCredentials}
						oninput={() => tlsChanged = true}
						rows={4}
						class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-xs focus:outline-none focus:ring-2 focus:ring-ring"
						placeholder={"CLOUDFLARE_DNS_API_TOKEN=xxxxx\nCLOUDFLARE_ZONE_API_TOKEN=xxxxx"}
					></textarea>
					<span class="mt-1 block text-xs text-muted-foreground">
						One KEY=VALUE per line. These are passed as environment variables to the ACME client.
						No inbound ports needed — verification happens via DNS records.
					</span>
				</div>
			{/if}

			{#if !tlsDomain.trim() || !tlsAcmeEmail.trim() || (tlsChallengeType === 'dns' && !tlsDnsProvider)}
				<p class="mb-3 text-xs text-destructive">
					{#if !tlsDomain.trim()}Domain is required.
					{:else if !tlsAcmeEmail.trim()}Email is required.
					{:else}DNS provider is required.
					{/if}
				</p>
			{/if}

		{/if}

		<Button size="sm" onclick={saveTls} disabled={savingTls || !tlsChanged}>
			{savingTls ? 'Saving…' : 'Save'}
		</Button>
	</section>

	<!-- Status panel (right column) -->
	<section class="rounded-lg border border-border p-5 self-start">
		<h3 class="mb-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Certificate Status</h3>
		{#if !acmeStatus || acmeStatus.state === 'idle'}
			<div class="flex items-center gap-2 text-sm">
				<span class="h-2 w-2 rounded-full bg-muted-foreground"></span>
				<span class="text-muted-foreground">Self-signed (default)</span>
			</div>
			<p class="mt-2 text-xs text-muted-foreground">Browsers will show a security warning. Enable Let's Encrypt for a trusted certificate.</p>
		{:else if acmeStatus.state === 'running'}
			<div class="flex items-center gap-2 text-sm">
				<span class="h-2 w-2 rounded-full bg-yellow-500 animate-pulse"></span>
				<span class="text-yellow-500 font-medium">Provisioning</span>
			</div>
			{#if acmeStatus.domain}
				<p class="mt-1 text-xs text-muted-foreground">{acmeStatus.domain}</p>
			{/if}
			<div class="mt-3 rounded bg-muted/30 p-3">
				<p class="text-xs text-muted-foreground whitespace-pre-wrap break-words">{acmeStatus.message}</p>
			</div>
			<div class="mt-3 h-1 overflow-hidden rounded-full bg-secondary">
				<div class="h-full w-1/3 bg-yellow-500 animate-[indeterminate_1.5s_ease-in-out_infinite]"></div>
			</div>
			<Button size="xs" variant="secondary" class="mt-3" onclick={async () => { await client.call('system.acme.reset'); acmeStatus = await client.call('system.acme.status'); }}>
				Dismiss
			</Button>
		{:else if acmeStatus.state === 'success'}
			<div class="flex items-center gap-2 text-sm">
				<span class="h-2 w-2 rounded-full bg-green-500"></span>
				<span class="text-green-500 font-medium">Certificate active</span>
			</div>
			{#if acmeStatus.domain}
				<p class="mt-1 text-xs font-mono">{acmeStatus.domain}</p>
			{/if}
			{#if acmeStatus.message}
				<p class="mt-2 text-xs text-muted-foreground">{acmeStatus.message}</p>
			{/if}
		{:else if acmeStatus.state === 'error'}
			<div class="flex items-center gap-2 text-sm">
				<span class="h-2 w-2 rounded-full bg-red-500"></span>
				<span class="text-red-500 font-medium">Error</span>
			</div>
			{#if acmeStatus.domain}
				<p class="mt-1 text-xs font-mono">{acmeStatus.domain}</p>
			{/if}
			{#if acmeStatus.message}
				<pre class="mt-2 max-h-48 overflow-auto rounded bg-red-950/30 p-3 text-xs text-red-300 whitespace-pre-wrap break-words">{acmeStatus.message}</pre>
			{/if}
			<Button size="xs" variant="secondary" class="mt-3" onclick={async () => { await client.call('system.acme.reset'); acmeStatus = await client.call('system.acme.status'); }}>
				Dismiss
			</Button>
		{/if}
	</section>
</div>
