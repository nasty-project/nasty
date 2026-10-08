<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { hasRootEquivalentAccess } from '$lib/access';
	import type { AuthMe } from '$lib/types';
	import { buildDiagnosticExport, clearClientTimings, type DiagnosticReport } from '$lib/performance-diagnostics';
	let allowed = $state(false);
	let busy = $state(false);
	let error = $state('');
	let detailed = $state(false);
	let preview = $state<ReturnType<typeof buildDiagnosticExport> | null>(null);
	onMount(() => { void getClient().call<AuthMe>('auth.me').then(me => { allowed = hasRootEquivalentAccess(me.role, me.scoped); }).catch(() => {}); });
	async function action(method: string, params?: unknown) {
		busy = true; error = '';
		try {
			if (method !== 'system.diagnostics.report') await getClient().call(method, params);
			if (method === 'system.diagnostics.clear') { clearClientTimings(); preview = null; }
			else {
				const report = await getClient().call<DiagnosticReport>('system.diagnostics.report');
				detailed = report.detailed_capture;
				preview = buildDiagnosticExport(report);
			}
		} catch { error = 'Could not collect diagnostics. Try again when the engine is responsive.'; }
		finally { busy = false; }
	}
	function download() {
		if (!preview) return;
		const url = URL.createObjectURL(new Blob([JSON.stringify(preview, null, 2)], { type: 'application/json' }));
		const link = document.createElement('a'); link.href = url; link.download = 'nasty-performance-report.json'; link.click();
		setTimeout(() => URL.revokeObjectURL(url), 1000);
	}
</script>

{#if allowed}
<section class="mb-6 rounded-lg border border-border p-4 space-y-3">
	<h2 class="text-lg font-semibold">Performance diagnostics</h2>
	<p class="text-sm text-muted-foreground">Bounded timing history stays on this NAS and in this browser. Detailed capture adds subprocess and filesystem-stage timings for 15 minutes. No additional filesystem scans or automatic uploads.</p>
	<p class="text-sm text-muted-foreground">Reports exclude raw logs, paths, filenames, credentials, usernames, addresses, serial numbers, and command arguments/output. Bucketed system and pool sizes still reveal some workload characteristics. Review before sharing.</p>
	<div class="flex flex-wrap gap-2">
		<button class="rounded border px-3 py-2 text-sm" disabled={busy} onclick={() => action('system.diagnostics.capture', { enabled: !detailed })}>{detailed ? 'Stop detailed capture' : 'Capture details for 15 minutes'}</button>
		<button class="rounded border px-3 py-2 text-sm" disabled={busy} onclick={() => action('system.diagnostics.report')}>Preview report</button>
		<button class="rounded border px-3 py-2 text-sm" disabled={busy} onclick={() => action('system.diagnostics.clear')}>Clear history</button>
	</div>
	{#if error}<p role="alert" class="text-sm text-red-400">{error}</p>{/if}
	{#if preview}
		<p class="text-sm">{preview.backend_timings.length} backend timings; {preview.client_timings.length} browser timings; {preview.dropped_records} evicted records. Detailed capture {preview.detailed_capture ? 'active' : 'inactive'} at preview time.</p>
		<div class="overflow-x-auto"><table class="w-full text-sm"><thead><tr><th class="text-left">Operation / stage</th><th>Count</th><th>Total ms</th><th>Max ms</th><th>p95 ms</th></tr></thead><tbody>
		{#each preview.summary.slice(0, 10) as row}<tr><td>{row.operation_stage}</td><td class="text-center">{row.count}</td><td class="text-center">{row.total_ms}</td><td class="text-center">{row.max_ms}</td><td class="text-center">{row.p95_ms}</td></tr>{/each}
		</tbody></table></div>
		<p class="text-sm text-muted-foreground">Stage totals overlap with backend totals; do not add them together. A browser timeout does not cancel server work. Late responses are included when this browser receives them.</p>
		<details><summary class="cursor-pointer text-sm">Review complete report</summary><pre class="mt-2 max-h-96 overflow-auto whitespace-pre-wrap text-xs">{JSON.stringify(preview, null, 2)}</pre></details>
		<button class="rounded border px-3 py-2 text-sm" onclick={download}>Download reviewed JSON report</button>
		<p class="text-sm text-muted-foreground">Nothing is sent to NASty developers. Attach this file yourself if you choose to share it. History is lost on engine restart or browser reload.</p>
	{/if}
</section>
{/if}
