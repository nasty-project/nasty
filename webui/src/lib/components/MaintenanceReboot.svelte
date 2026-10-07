<script lang="ts">
	import { onMount } from 'svelte';
	let waiting = $state(false);
	onMount(() => {
		let disposed = false;
		let timer: ReturnType<typeof setTimeout>;
		const controller = new AbortController();
		async function poll() {
			try {
				const response = await fetch('/api/maintenance/status', { cache: 'no-store', credentials: 'omit', signal: AbortSignal.any([controller.signal, AbortSignal.timeout(4000)]) });
				if (response.ok) {
					const status = await response.json();
					if (!disposed && status.maintenance === true && typeof status.boot_id === 'string' && status.boot_id) {
						window.location.reload();
						return;
					}
				}
			} catch { /* Expected while the NAS is rebooting. */ }
			if (!disposed) { waiting = true; timer = setTimeout(poll, 2000); }
		}
		void poll();
		return () => { disposed = true; clearTimeout(timer); controller.abort(); };
	});
</script>

<div class="absolute inset-0 z-50 flex items-center justify-center bg-background/95">
	<div class="max-w-lg space-y-4 p-6 text-center" role="status" aria-live="polite">
		<h2 class="text-lg font-semibold">Rebooting into storage maintenance</h2>
		<p class="text-sm text-muted-foreground">{waiting ? 'Waiting for the NAS to return…' : 'Maintenance reboot requested.'} This page will open the maintenance landing page when it is available.</p>
		<p class="text-sm text-muted-foreground">Data pools and consumers stay offline. No repairs run automatically. The landing page will check SSH readiness and show connection instructions.</p>
		<p class="text-sm text-muted-foreground">If the page cannot be reached, use your existing SSH access or the console. You may need to accept a certificate warning if the usual certificate is unavailable.</p>
		<a href="/" class="inline-block text-sm underline">Check the maintenance page manually</a>
		<p class="text-sm">To exit over SSH or the console: <code>sudo nasty-maintenance exit</code></p>
	</div>
</div>
