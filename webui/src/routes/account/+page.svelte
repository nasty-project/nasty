<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Card, CardContent } from '$lib/components/ui/card';
	import type {
		WebauthnConfigInfo,
		WebauthnCredentialSummary,
		WebauthnRegisterStart,
	} from '$lib/types';
	import { KeyRound, Trash2, Plus } from '@lucide/svelte';
	import { startRegistration } from '@simplewebauthn/browser';

	const client = getClient();

	let credentials: WebauthnCredentialSummary[] = $state([]);
	let webauthnConfig: WebauthnConfigInfo | null = $state(null);
	let loading = $state(true);
	let registering = $state(false);
	let deleting = $state<string | null>(null);
	let showAddForm = $state(false);
	let newLabel = $state('');

	// Browser-level WebAuthn capability sniff. If the API isn't there
	// (very old browsers, embedded webviews, http://) we show a clear
	// error rather than a broken button. https-only is the WebAuthn
	// spec; on http://localhost the API is exposed for development
	// but we don't promise that for NASty.
	const browserSupported = $derived(
		typeof window !== 'undefined'
			&& 'PublicKeyCredential' in window
			&& typeof navigator !== 'undefined'
			&& !!navigator.credentials?.create,
	);

	// Origin precheck: WebAuthn rejects every registration on origins
	// that can't satisfy the engine's pinned RP ID. The browser's own
	// error ("'rp.id' cannot be used with the current origin") is
	// useless to operators — surface the real cause inline with the
	// hostname they need to switch to.
	const isLikelyIp = $derived.by((): boolean => {
		if (typeof window === 'undefined') return false;
		const h = window.location.hostname;
		// IPv4 dotted-quad, or IPv6 in URL brackets. Cheap regex —
		// `URL.canParse` doesn't give us "is this an IP" directly.
		return /^\d{1,3}(\.\d{1,3}){3}$/.test(h) || h.startsWith('[');
	});
	const isSecureContext = $derived(
		typeof window !== 'undefined'
			&& (window.isSecureContext || window.location.hostname === 'localhost'),
	);
	const hostnameMatchesRpId = $derived.by((): boolean => {
		if (typeof window === 'undefined' || !webauthnConfig) return true;
		const h = window.location.hostname.toLowerCase();
		const rp = webauthnConfig.rp_id.toLowerCase();
		// Spec: RP ID must equal the effective domain or be a
		// registrable suffix. For NASty's use we accept exact match
		// or `*.<rp_id>` subdomain — the engine never pins a wider
		// RP ID than what `tls_domain` / `nasty.local` is set to.
		return h === rp || h.endsWith('.' + rp);
	});
	const originBlocker = $derived.by((): string | null => {
		if (isLikelyIp) {
			return webauthnConfig
				? `WebAuthn cannot be used over an IP address. Visit https://${webauthnConfig.rp_id} (or a subdomain of it) to register security keys here.`
				: 'WebAuthn cannot be used over an IP address — visit this NASty by hostname.';
		}
		if (!isSecureContext) {
			return webauthnConfig
				? `WebAuthn requires HTTPS. Visit https://${webauthnConfig.rp_id} to register security keys.`
				: 'WebAuthn requires HTTPS.';
		}
		if (!hostnameMatchesRpId && webauthnConfig) {
			return `You're on ${window.location.hostname}, but this NASty registers security keys under ${webauthnConfig.rp_id}. Visit https://${webauthnConfig.rp_id} to register here.`;
		}
		return null;
	});
	const canRegister = $derived(browserSupported && originBlocker === null);

	async function loadCredentials() {
		try {
			const [list, config] = await Promise.all([
				client.call<WebauthnCredentialSummary[]>('auth.webauthn.list'),
				client.call<WebauthnConfigInfo>('auth.webauthn.config'),
			]);
			credentials = list;
			webauthnConfig = config;
		} catch {
			credentials = [];
			webauthnConfig = null;
		}
		loading = false;
	}

	async function registerNew(e: SubmitEvent) {
		e.preventDefault();
		const label = newLabel.trim();
		if (!label) return;
		registering = true;
		try {
			// Step 1 — engine builds the challenge.
			const start = await client.call<WebauthnRegisterStart>(
				'auth.webauthn.register.start',
				{ label },
			);
			// Step 2 — browser prompts user to tap their authenticator
			// (or use Touch ID / Windows Hello / etc.). simplewebauthn
			// handles the WebAuthn JSON ↔ ArrayBuffer conversion both
			// ways.
			//
			// The cast through `unknown` is because the engine sends us
			// webauthn-rs's `CreationChallengeResponse` shape directly;
			// simplewebauthn accepts the spec JSON form (which is the
			// same shape).
			//
			// eslint-disable-next-line @typescript-eslint/no-explicit-any
			const response = await startRegistration({
				optionsJSON: (start.creation_options as { publicKey?: unknown }).publicKey
					?? start.creation_options,
			} as Parameters<typeof startRegistration>[0]);
			// Step 3 — engine verifies attestation + persists.
			await withToast(
				() => client.call('auth.webauthn.register.finish', {
					registration_id: start.registration_id,
					response,
				}),
				`Security key "${label}" registered`,
			);
			newLabel = '';
			showAddForm = false;
			await loadCredentials();
		} catch (err) {
			// Most common failure: user dismissed the browser prompt
			// (NotAllowedError / AbortError). Surface a short toast
			// rather than a stack trace.
			const msg = err instanceof Error ? err.message : String(err);
			await withToast(
				() => Promise.reject(msg),
				'Security key registration failed',
			).catch(() => {});
		} finally {
			registering = false;
		}
	}

	async function deleteCredential(cred: WebauthnCredentialSummary) {
		if (!confirm(`Delete security key "${cred.label}"? You won't be able to use it to sign in again.`)) {
			return;
		}
		deleting = cred.credential_id;
		try {
			await withToast(
				() => client.call('auth.webauthn.delete', {
					credential_id: cred.credential_id,
				}),
				`Security key "${cred.label}" deleted`,
			);
			await loadCredentials();
		} finally {
			deleting = null;
		}
	}

	function formatDate(unix: number): string {
		return new Date(unix * 1000).toLocaleString();
	}

	onMount(loadCredentials);
</script>

<div class="mb-4">
	<h1 class="text-2xl font-semibold">Account</h1>
	<p class="text-sm text-muted-foreground">
		Manage your own credentials. Admins can manage other users on the
		<a class="underline" href="/users">Access Control</a> page.
	</p>
</div>

<section class="rounded-lg border border-border p-5">
	<div class="mb-4 flex items-center justify-between">
		<div class="flex items-center gap-2">
			<KeyRound size={18} class="text-muted-foreground" />
			<h2 class="text-base font-semibold">Security keys</h2>
		</div>
		{#if !showAddForm && canRegister}
			<Button size="sm" onclick={() => (showAddForm = true)}>
				<Plus size={14} />
				Add security key
			</Button>
		{/if}
	</div>

	<p class="mb-4 text-xs text-muted-foreground">
		Register a hardware key (YubiKey, Solo 2, Trezor), a platform
		authenticator (Touch ID, Windows Hello), or a syncable passkey.
		Sign-in via security key arrives in a follow-up; for now,
		registration only.
	</p>

	{#if !browserSupported}
		<div class="mb-3 rounded border border-amber-700/40 bg-amber-950/40 px-3 py-2 text-xs text-amber-200">
			This browser doesn't expose the WebAuthn API. Use an up-to-date
			browser served over HTTPS to register security keys.
		</div>
	{:else if originBlocker}
		<!-- The browser will reject `navigator.credentials.create`
			 with a cryptic error on origins that can't satisfy the
			 engine's pinned RP ID. Surface the actual cause inline
			 before the operator even types a label. -->
		<div class="mb-3 rounded border border-amber-700/40 bg-amber-950/40 px-3 py-2 text-xs text-amber-200">
			<strong>Can't register security keys on this origin.</strong>
			<div class="mt-1">{originBlocker}</div>
		</div>
	{/if}

	{#if showAddForm}
		<form onsubmit={registerNew} class="mb-4 rounded border border-border bg-muted/20 p-4">
			<label class="block text-xs font-medium" for="cred-label">
				Label (so you can recognise this key in the list)
			</label>
			<Input
				id="cred-label"
				bind:value={newLabel}
				placeholder="e.g. Personal YubiKey, Laptop Touch ID"
				class="mt-1"
				disabled={registering}
				maxlength={128}
				autocomplete="off"
			/>
			<div class="mt-3 flex gap-2">
				<Button type="submit" size="sm" disabled={registering || !newLabel.trim()}>
					{#if registering}Tap your key…{:else}Register{/if}
				</Button>
				<Button
					type="button"
					size="sm"
					variant="secondary"
					disabled={registering}
					onclick={() => {
						showAddForm = false;
						newLabel = '';
					}}
				>
					Cancel
				</Button>
			</div>
		</form>
	{/if}

	{#if loading}
		<div class="text-sm text-muted-foreground">Loading…</div>
	{:else if credentials.length === 0}
		<div class="rounded border border-border/40 bg-muted/10 px-4 py-6 text-center text-sm text-muted-foreground">
			No security keys registered yet.
		</div>
	{:else}
		<table class="w-full text-sm">
			<thead>
				<tr class="text-left text-xs uppercase tracking-wide text-muted-foreground">
					<th class="pb-2 font-medium">Label</th>
					<th class="pb-2 font-medium">Added</th>
					<th class="pb-2"></th>
				</tr>
			</thead>
			<tbody>
				{#each credentials as cred (cred.credential_id)}
					<tr class="border-t border-border/40">
						<td class="py-2"><strong>{cred.label}</strong></td>
						<td class="py-2 text-xs text-muted-foreground">{formatDate(cred.created_at)}</td>
						<td class="py-2 text-right">
							<Button
								size="xs"
								variant="ghost"
								disabled={deleting === cred.credential_id}
								onclick={() => deleteCredential(cred)}
								title="Delete this security key"
							>
								<Trash2 size={14} />
							</Button>
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
</section>
