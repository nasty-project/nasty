<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { Settings } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import SortTh from '$lib/components/SortTh.svelte';

	const client = getClient();

	let settings: Settings | null = $state(null);
	let tlsDomain = $state('');
	let tlsAcmeEmail = $state('');
	let tlsAcmeEnabled = $state(false);
	let acmeStatus: { state: string; message: string; domain?: string; expires?: string; issued?: string; issuer?: string; last_attempt?: string } | null = $state(null);
	let tlsAcmeStaging = $state(false);
	let tlsChallengeType = $state<'tls-alpn' | 'http' | 'dns'>('tls-alpn');
	let tlsDnsProvider = $state('');
	let tlsDnsCredentials = $state('');
	/** Credentials exist server-side (sealed or legacy) — the engine
	 * never returns them, so this drives the "stored" marker and the
	 * keep-on-blank-save behavior. */
	let dnsCredsStored = $state(false);
	/** Operator explicitly asked to clear the stored credentials. */
	let dnsCredsClear = $state(false);
	let tlsDnsResolver = $state('');
	let tlsDnsPropagationWait = $state(0);
	let showAdvancedDns = $state(false);
	let savingTls = $state(false);
	let tlsChanged = $state(false);
	let editing = $state(false);
	const certActive = $derived.by(() => Boolean(acmeStatus && acmeStatus.state === 'success' && tlsAcmeEnabled));

	// DNS-01 plugins compiled into the Caddy binary shipped with NASty
	// (see `pkgs.caddy.withPlugins` in nixos/modules/nasty.nix). Picking
	// anything outside this list will fail at Caddy reload time with an
	// "unknown module" error, so we don't expose them.
	const popularDnsProviders = [
		{ code: 'cloudflare', name: 'Cloudflare' },
		{ code: 'route53', name: 'Amazon Route 53' },
		{ code: 'hetzner', name: 'Hetzner' },
		{ code: 'linode', name: 'Linode' },
		{ code: 'porkbun', name: 'Porkbun' },
		{ code: 'namecheap', name: 'Namecheap' },
		{ code: 'duckdns', name: 'Duck DNS' },
		{ code: 'desec', name: 'deSEC.io' },
		{ code: 'rfc2136', name: 'RFC 2136 (BIND / Knot / PowerDNS)' },
	];

	onMount(async () => {
		settings = await client.call<Settings>('system.settings.get');
		tlsDomain = settings?.tls_domain ?? '';
		tlsAcmeEmail = settings?.tls_acme_email ?? '';
		tlsAcmeEnabled = settings?.tls_acme_enabled ?? false;
		tlsChallengeType = settings?.tls_challenge_type ?? 'tls-alpn';
		tlsDnsProvider = settings?.tls_dns_provider ?? '';
		// Credentials are encrypted at rest and not returned once sealed —
		// the textarea starts blank and a marker shows whether something
		// is stored. Saving with the field blank keeps the stored value
		// (the engine receives the "<unchanged>" sentinel).
		tlsDnsCredentials = '';
		dnsCredsStored = !!(settings?.tls_dns_credentials || settings?.tls_dns_credentials_encrypted);
		dnsCredsClear = false;
		tlsDnsResolver = settings?.tls_dns_resolver ?? '';
		tlsDnsPropagationWait = settings?.tls_dns_propagation_wait ?? 0;
		// Expand the advanced section if the operator has previously
		// set either knob — they shouldn't have to hunt for the
		// non-default value they last saved.
		showAdvancedDns = !!tlsDnsResolver || tlsDnsPropagationWait > 0;
		tlsAcmeStaging = settings?.tls_acme_staging ?? false;
		try { acmeStatus = await client.call('system.acme.status'); } catch { /* ignore */ }
		refreshHostStatuses();
		// Poll every 10s so the operator sees state transitions
		// (issuing → active) without manually refreshing. Cheap
		// (admin-API read + journalctl tail).
		hostStatusPollHandle = setInterval(refreshHostStatuses, 10_000) as unknown as number;
	});

	type HostTlsStatus = {
		host: string;
		state: 'active' | 'issuing' | 'failed' | 'pending';
		issuer?: string;
		issued?: string;
		expires?: string;
		expires_in_days?: number;
		message?: string;
		app?: string;
	};

	let hostStatuses: HostTlsStatus[] = $state([]);
	let hostStatusPollHandle: number | null = $state(null);

	// ── Column sorting ──────────────────────────────────────────────────
	type HostSortKey = 'host' | 'state' | 'expires';
	let hostSortKey = $state<HostSortKey>('host');
	let hostSortDir = $state<'asc' | 'desc'>('asc');
	function toggleHostSort(key: HostSortKey) {
		if (hostSortKey === key) hostSortDir = hostSortDir === 'asc' ? 'desc' : 'asc';
		else { hostSortKey = key; hostSortDir = 'asc'; }
	}
	const sortedHostStatuses = $derived.by(() => {
		const sign = hostSortDir === 'asc' ? 1 : -1;
		return [...hostStatuses].sort((a, b) => {
			let cmp = 0;
			if (hostSortKey === 'host') cmp = a.host.localeCompare(b.host, undefined, { numeric: true });
			else if (hostSortKey === 'state') cmp = a.state.localeCompare(b.state);
			else {
				// Sort by days-to-expiry; unknown expiry sorts last.
				const av = a.expires_in_days ?? Infinity;
				const bv = b.expires_in_days ?? Infinity;
				cmp = av - bv;
			}
			if (cmp === 0) cmp = a.host.localeCompare(b.host, undefined, { numeric: true });
			return sign * cmp;
		});
	});
	// Page-scoped handles for the post-save / Retry ACME-status pollers so
	// onDestroy can cancel them on SPA navigation — otherwise the 2-second
	// interval (plus its 5-min setTimeout backstop) keeps hammering
	// system.acme.status long after the user has left the page.
	let acmePollHandle: ReturnType<typeof setInterval> | null = null;
	let acmePollTimeout: ReturnType<typeof setTimeout> | null = null;

	async function refreshHostStatuses() {
		try {
			hostStatuses = await client.call<HostTlsStatus[]>('system.tls.host_statuses');
		} catch {
			// engine restart / transient network — keep prior list, retry next tick
		}
	}

	function stopAcmePolling() {
		if (acmePollHandle !== null) { clearInterval(acmePollHandle); acmePollHandle = null; }
		if (acmePollTimeout !== null) { clearTimeout(acmePollTimeout); acmePollTimeout = null; }
	}

	function startAcmePolling() {
		stopAcmePolling();
		acmePollHandle = setInterval(async () => {
			try { acmeStatus = await client.call('system.acme.status'); } catch { /* ignore */ }
			if (acmeStatus && (acmeStatus.state === 'success' || acmeStatus.state === 'error')) {
				stopAcmePolling();
			}
		}, 2000);
		// 5 min backstop: DNS propagation can be slow, but at some point
		// the poller stops being useful and just costs RPC traffic.
		acmePollTimeout = setTimeout(stopAcmePolling, 300_000);
	}

	onDestroy(() => {
		if (hostStatusPollHandle !== null) clearInterval(hostStatusPollHandle);
		stopAcmePolling();
	});

	function badgeForState(s: string): { label: string; cls: string } {
		switch (s) {
			case 'active': return { label: 'active', cls: 'bg-green-500/15 text-green-400 border-green-500/40' };
			case 'issuing': return { label: 'issuing…', cls: 'bg-yellow-500/15 text-yellow-400 border-yellow-500/40' };
			case 'failed': return { label: 'failed', cls: 'bg-red-500/15 text-red-400 border-red-500/40' };
			default: return { label: 'pending', cls: 'bg-muted text-muted-foreground border-border' };
		}
	}

	let downloadingCaRoot = $state(false);

	async function downloadCaRoot() {
		downloadingCaRoot = true;
		try {
			const pem = await client.call<string>('system.tls.local_ca_root');
			const blob = new Blob([pem], { type: 'application/x-pem-file' });
			const url = URL.createObjectURL(blob);
			const a = document.createElement('a');
			a.href = url;
			a.download = `nasty-local-ca-${settings?.hostname || 'root'}.crt`;
			document.body.appendChild(a);
			a.click();
			document.body.removeChild(a);
			URL.revokeObjectURL(url);
		} catch (e) {
			const toast = await import('$lib/toast.svelte');
			toast.error(String(e));
		}
		downloadingCaRoot = false;
	}

	async function saveTls() {
		savingTls = true;
		const result = await withToast(
			() => client.call<Settings>('system.settings.update', {
				tls_domain: tlsDomain || null,
				tls_acme_email: tlsAcmeEmail || null,
				tls_acme_enabled: tlsAcmeEnabled,
				tls_challenge_type: tlsChallengeType,
				tls_dns_provider: tlsDnsProvider || null,
				tls_dns_credentials: tlsDnsCredentials
					? tlsDnsCredentials
					: dnsCredsStored && !dnsCredsClear
						? '<unchanged>'
						: null,
				tls_dns_resolver: tlsDnsResolver || '',
				tls_dns_propagation_wait: tlsDnsPropagationWait || 0,
				tls_acme_staging: tlsAcmeStaging,
			}),
			tlsAcmeEnabled ? 'Let\'s Encrypt certificate requested — check status below' : 'TLS settings saved'
		);
		if (result !== undefined) {
			settings = result;
			tlsChanged = false;
			editing = false;
			tlsDnsCredentials = '';
			dnsCredsStored = !!(result.tls_dns_credentials || result.tls_dns_credentials_encrypted);
			dnsCredsClear = false;
			if (tlsAcmeEnabled) startAcmePolling();
		}
		savingTls = false;
	}
</script>

<div>
	<p class="text-sm text-muted-foreground mt-0.5">Manage HTTPS certificates for the NASty web interface.</p>
</div>

<div class="mt-6 grid grid-cols-1 gap-6 lg:grid-cols-2">
	<section class="rounded-lg border border-border p-5">
		{#if certActive && !editing}
			<!-- Summary view when cert is active -->
			<div class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
				<span class="text-muted-foreground">Domain</span>
				<span class="font-mono">{tlsDomain}</span>
				<span class="text-muted-foreground">Challenge</span>
				<span>{tlsChallengeType === 'dns' ? `DNS (${tlsDnsProvider || 'custom'})` : 'TLS-ALPN (port 443)'}</span>
				<span class="text-muted-foreground">Email</span>
				<span>{tlsAcmeEmail}</span>
			</div>
			<div class="mt-4 flex gap-2">
				<Button size="sm" variant="secondary" onclick={() => editing = true}>Reconfigure</Button>
				<Button size="sm" variant="destructive" onclick={async () => {
					tlsAcmeEnabled = false;
					tlsChanged = true;
					await saveTls();
					editing = false;
				}}>Disable</Button>
			</div>
		{:else}
		<!-- Full form -->
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
						The provider must be one of the plugins compiled into NASty's Caddy build (the dropdown lists all of them).
						Need a different one? Open an issue — adding it is a one-line change to the Nix package definition.
					</span>
				</div>

				<div class="mb-4">
					<div class="mb-1 flex items-center justify-between">
						<label for="tls-dns-creds" class="block text-xs text-muted-foreground">API Credentials</label>
						{#if dnsCredsStored && !dnsCredsClear}
							<span class="flex items-center gap-2 text-xs">
								<span class="text-green-600" title="Credentials are stored encrypted at rest and never sent back to the browser.">stored (encrypted)</span>
								<button type="button" class="text-muted-foreground underline hover:text-destructive" onclick={() => { dnsCredsClear = true; tlsChanged = true; }}>clear</button>
							</span>
						{:else if dnsCredsClear}
							<span class="text-xs text-destructive">will be cleared on save <button type="button" class="text-muted-foreground underline" onclick={() => { dnsCredsClear = false; }}>undo</button></span>
						{/if}
					</div>
					<textarea
						id="tls-dns-creds"
						bind:value={tlsDnsCredentials}
						oninput={() => tlsChanged = true}
						rows={4}
						class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-xs focus:outline-none focus:ring-2 focus:ring-ring"
						placeholder={dnsCredsStored && !dnsCredsClear ? 'stored — leave blank to keep, paste to replace' : 'CF_API_TOKEN=xxxxx'}
					></textarea>
					<span class="mt-1 block text-xs text-muted-foreground">
						One KEY=VALUE per line. Written to a Caddy <code>EnvironmentFile</code> and referenced from the
						generated <code>tls</code> block via <code>{'{env.KEY}'}</code> placeholders. No inbound ports needed —
						verification happens via DNS records. Stored encrypted at rest (systemd-creds).
					</span>
				</div>

				<div class="mb-4">
					<button
						type="button"
						onclick={() => showAdvancedDns = !showAdvancedDns}
						class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
					>
						<span>{showAdvancedDns ? '▾' : '▸'}</span>
						<span>Advanced DNS settings</span>
					</button>
					{#if showAdvancedDns}
						<div class="mt-3 space-y-3 rounded-md border border-border p-3">
							<div>
								<label for="tls-dns-resolver" class="mb-1 block text-xs text-muted-foreground">
									Resolvers (comma-separated)
								</label>
								<input
									id="tls-dns-resolver"
									type="text"
									bind:value={tlsDnsResolver}
									oninput={() => tlsChanged = true}
									class="w-full rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
									placeholder="1.1.1.1, 8.8.8.8"
								/>
								<span class="mt-1 block text-xs text-muted-foreground">
									DNS servers used to verify TXT-record propagation. Default: <code>1.1.1.1, 8.8.8.8</code>.
									Override when the box can't reach those (split-horizon DNS, air-gapped networks).
								</span>
							</div>
							<div>
								<label for="tls-dns-wait" class="mb-1 block text-xs text-muted-foreground">
									Propagation wait (seconds)
								</label>
								<input
									id="tls-dns-wait"
									type="number"
									min="0"
									bind:value={tlsDnsPropagationWait}
									oninput={() => tlsChanged = true}
									class="w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
									placeholder="30"
								/>
								<span class="mt-1 block text-xs text-muted-foreground">
									Sleep this long after creating the TXT record before checking propagation. Default: 30s.
									Bump if issuance keeps timing out — some resolvers cache NXDOMAIN aggressively (SOA MINIMUM
									TTL up to an hour). Set <code>0</code> to use the default.
								</span>
							</div>
						</div>
					{/if}
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

		<div class="flex gap-2">
			<Button size="sm" onclick={saveTls} disabled={savingTls || !tlsChanged}>
				{savingTls ? 'Saving…' : 'Save'}
			</Button>
			{#if tlsAcmeEnabled && acmeStatus?.state !== 'running'}
				<Button size="sm" variant="secondary" onclick={async () => {
					await withToast(() => client.call('system.acme.retry'), 'Provisioning started');
					startAcmePolling();
				}}>
					Retry
				</Button>
			{/if}
		</div>
		{/if}
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
			<div class="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
				{#if acmeStatus.issuer}
					<span class="text-muted-foreground">Issuer</span>
					<span>{acmeStatus.issuer}</span>
				{/if}
				{#if acmeStatus.issued}
					<span class="text-muted-foreground">Issued</span>
					<span>{acmeStatus.issued}</span>
				{/if}
				{#if acmeStatus.expires}
					<span class="text-muted-foreground">Expires</span>
					<span>{acmeStatus.expires}</span>
				{/if}
			</div>
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

	<!-- Managed certificates — per-host issuance state from Caddy.
	     Auto-refreshes every 10s. Replaces the operator's previous
	     workflow of "ssh in and grep journalctl" when an ingress sits
	     on `pending` for longer than expected. -->
	<section class="rounded-lg border border-border p-5 lg:col-span-2">
		<div class="flex items-baseline justify-between gap-3 mb-3">
			<h3 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">Managed Certificates</h3>
			<span class="text-xs text-muted-foreground">Updates every 10s</span>
		</div>
		{#if hostStatuses.length === 0}
			<p class="text-sm text-muted-foreground">
				No managed hostnames yet. Hosts appear here once you enable Let's Encrypt above or assign a
				subdomain ingress to an app — the engine pushes each one to Caddy and tracks issuance.
			</p>
		{:else}
			<table class="w-full text-sm">
				<thead>
					<tr>
						<SortTh label="Host" active={hostSortKey === 'host'} dir={hostSortDir} onclick={() => toggleHostSort('host')} thClass="pb-2 font-medium" />
						<th class="pb-2 font-medium text-left text-xs uppercase text-muted-foreground">App</th>
						<SortTh label="State" active={hostSortKey === 'state'} dir={hostSortDir} onclick={() => toggleHostSort('state')} thClass="pb-2 font-medium" />
						<SortTh label="Issuer / Expires" active={hostSortKey === 'expires'} dir={hostSortDir} onclick={() => toggleHostSort('expires')} thClass="pb-2 font-medium" />
						<th class="pb-2 font-medium text-left text-xs uppercase text-muted-foreground">Detail</th>
					</tr>
				</thead>
				<tbody>
					{#each sortedHostStatuses as h}
						{@const badge = badgeForState(h.state)}
						<tr class="border-t border-border">
							<td class="py-2 pr-3 font-mono text-xs">{h.host}</td>
							<td class="py-2 pr-3 text-xs">
								{#if h.app}
									<a href="/apps" class="text-primary hover:underline">{h.app}</a>
								{:else}
									<span class="text-muted-foreground">—</span>
								{/if}
							</td>
							<td class="py-2 pr-3">
								<span class="inline-flex items-center rounded-md border px-2 py-0.5 text-[0.65rem] {badge.cls}">{badge.label}</span>
							</td>
							<td class="py-2 pr-3 text-xs">
								{#if h.state === 'active'}
									<div>{h.issuer || '—'}</div>
									{#if h.expires}
										<div class="text-muted-foreground">{h.expires}{#if h.expires_in_days !== undefined && h.expires_in_days !== null} ({h.expires_in_days}d){/if}</div>
									{/if}
								{:else}
									<span class="text-muted-foreground">—</span>
								{/if}
							</td>
							<td class="py-2 text-xs">
								{#if h.message}
									<span class="text-muted-foreground break-all">{h.message}</span>
								{:else if h.state === 'pending'}
									<span class="text-muted-foreground">Waiting for Caddy to start issuance…</span>
								{:else}
									<span class="text-muted-foreground">—</span>
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}
	</section>

	<!-- Local CA root (always shown — covers the IP-direct / no-ACME case) -->
	<section class="rounded-lg border border-border p-5 lg:col-span-2">
		<h3 class="mb-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Local CA Root</h3>
		<p class="text-sm text-muted-foreground mb-3">
			Caddy serves a self-signed certificate (issued by Caddy's internal CA) for direct-IP access and any
			hostname that doesn't have a managed Let's Encrypt cert. Browsers don't trust this CA by default —
			import this root certificate into your OS or browser trust store once and every NASty service signed
			by it stops triggering security warnings on this machine.
		</p>
		<Button size="sm" variant="secondary" onclick={downloadCaRoot} disabled={downloadingCaRoot}>
			{downloadingCaRoot ? 'Preparing…' : 'Download root certificate (.crt)'}
		</Button>
		<p class="mt-2 text-xs text-muted-foreground">
			Each NASty box has its own CA root, so you'll need to import one per box. The downloaded file is the
			PEM-encoded root cert; import via your OS keychain (macOS), <code>certmgr</code> / Group Policy
			(Windows), <code>update-ca-certificates</code> (Linux), or your browser's Authorities tab.
		</p>
	</section>
</div>
