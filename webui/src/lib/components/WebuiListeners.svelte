<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { hasRootEquivalentAccess } from '$lib/access';
	import type { AuthMe } from '$lib/types';
	import { listenerUrl, onListener, validListenerPorts, type ListenerState } from '$lib/webui-listeners';
	import { Button } from '$lib/components/ui/button';
	let listenerState = $state<ListenerState | null>(null);
	let https = $state(443);
	let http = $state(80);
	let httpEnabled = $state(true);
	let busy = $state(false);
	let clock = $state(Date.now());
	let currentUrl = $state('');
	let permitted = $state(false);
	const pending = $derived(listenerState?.pending);
	const proposedUrl = $derived(currentUrl && validListenerPorts(https, httpEnabled ? http : null) ? listenerUrl(currentUrl, https) : '');
	const nextUrl = $derived(currentUrl && pending ? listenerUrl(currentUrl, pending.ports.https_port) : '');
	const canConfirm = $derived(!!pending && !!currentUrl && onListener(currentUrl, pending.ports.https_port));
	async function refresh() {
		const value = await getClient().call<ListenerState>('system.webui.get');
		if (!listenerState) {
			const ports = value.pending?.ports ?? value.confirmed;
			https = ports.https_port; http = ports.http_port ?? 80; httpEnabled = ports.http_port !== null;
		}
		listenerState = value;
	}
	onMount(() => {
		let disposed = false;
		currentUrl = window.location.href;
		void withToast(async () => {
			const me = await getClient().call<AuthMe>('auth.me');
			if (disposed || !hasRootEquivalentAccess(me.role, me.scoped)) return;
			permitted = true;
			await refresh();
		});
		const timer = setInterval(() => {
			clock = Date.now();
			if (permitted && !busy) void refresh().catch(() => { /* Connection can drop during a port change. */ });
		}, 2000);
		return () => { disposed = true; clearInterval(timer); };
	});
	async function apply() {
		if (!validListenerPorts(https, httpEnabled ? http : null)) return;
		if (!await confirm('Change WebUI ports?', 'Open the new HTTPS URL and confirm within 120 seconds, otherwise the previous ports will be restored. Re-login or certificate trust may be required. This also changes the listener used by app ingress. SSH is unchanged.')) return;
		busy = true;
		await withToast(async () => {
			listenerState = await getClient().call<ListenerState>('system.webui.update', { https_port: https, http_port: httpEnabled ? http : null });
		});
		busy = false;
	}
	async function finish(method: 'confirm' | 'rollback') {
		busy = true;
		await withToast(async () => {
			listenerState = await getClient().call<ListenerState>(`system.webui.${method}`, method === 'confirm' ? { txn_id: pending?.txn_id } : {});
		}, method === 'confirm' ? 'WebUI ports confirmed' : 'Previous WebUI ports restored');
		busy = false;
	}
</script>

{#if permitted}
	<section class="rounded-lg border border-border p-4 space-y-3">
		<h3 class="text-sm font-semibold">WebUI listening ports</h3>
		<p class="text-xs text-muted-foreground">HTTPS is required. HTTP only redirects to HTTPS; it can be disabled. Firewall source/interface restrictions remain in effect for IPv4 and IPv6.</p>
		{#if listenerState}
			<p class="text-xs">Confirmed: HTTPS {listenerState.confirmed.https_port}; HTTP {listenerState.confirmed.http_port ?? 'disabled'}</p>
			<div class="flex flex-wrap items-center gap-3">
				<label class="text-sm">HTTPS <input aria-label="WebUI HTTPS port" type="number" min="1" max="65535" bind:value={https} disabled={busy || !!pending} class="ml-2 w-24 rounded border border-input bg-transparent px-2 py-1" /></label>
				<label class="text-sm"><input type="checkbox" bind:checked={httpEnabled} disabled={busy || !!pending} /> Enable HTTP redirect</label>
				{#if httpEnabled}<label class="text-sm">HTTP <input aria-label="WebUI HTTP redirect port" type="number" min="1" max="65535" bind:value={http} disabled={busy || !!pending} class="ml-2 w-24 rounded border border-input bg-transparent px-2 py-1" /></label>{/if}
				<Button size="sm" disabled={busy || !!pending || !validListenerPorts(https, httpEnabled ? http : null)} onclick={apply}>Apply ports</Button>
			</div>
			{#if proposedUrl}<p class="text-xs">If applying disconnects this page, open <a href={proposedUrl} target="_blank" rel="noopener noreferrer" class="underline">{proposedUrl}</a> to confirm, or wait 120 seconds for rollback.</p>{/if}
			{#if !validListenerPorts(https, httpEnabled ? http : null)}<p class="text-xs text-destructive">Choose distinct ports between 1 and 65535. Ports 2019 and 2137 are reserved.</p>{/if}
			{#if pending}
				<p class="text-sm text-amber-500">Unconfirmed change: automatic rollback in {Math.max(0, Math.ceil(pending.deadline - clock / 1000))} seconds.</p>
				<a href={nextUrl} target="_blank" rel="noopener noreferrer" class="text-sm underline">Open new HTTPS URL: {nextUrl}</a>
				<div class="flex gap-2"><Button size="sm" disabled={busy || !canConfirm} onclick={() => finish('confirm')}>Confirm these ports</Button><Button size="sm" variant="outline" disabled={busy} onclick={() => finish('rollback')}>Roll back now</Button></div>
				{#if !canConfirm}<p class="text-xs text-muted-foreground">Confirm from the Settings page opened on the new HTTPS port. If it cannot be reached, wait for rollback.</p>{/if}
			{/if}
			{#if listenerState.last_error}<p class="text-xs text-destructive">{listenerState.last_error}</p>{/if}
		{:else}<p class="text-xs text-muted-foreground">Listener settings unavailable. No changes can be applied.</p>{/if}
		<p class="text-xs text-muted-foreground">Blocked public ports 80/443 require DNS-01 for ACME certificates; alternative local ports do not change the CA challenge ports. Prefer VPN access over exposing NAS administration directly to the Internet.</p>
	</section>
{/if}
