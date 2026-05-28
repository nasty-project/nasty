<!--
	Highly-experimental Secure Boot enrollment ceremony wizard. Polls
	`system.secure_boot.enrollment.status` and renders the right step
	for the current phase. Shown only when the readiness probe is
	green — the parent decides visibility via the `visible` prop.

	The state machine lives in the engine; this component is a thin
	view + button driver. Per-vendor BIOS hints are matched on the
	DMI manufacturer string. The generic block is always visible —
	vendor hints rot faster than the generic "Erase PK" instruction.
-->
<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import { rebootState } from '$lib/reboot.svelte';
	import type { SecureBootEnrollmentStatusResponse } from '$lib/types';
	import { ShieldCheck, AlertTriangle, RefreshCw } from '@lucide/svelte';

	interface Props {
		/** Parent gates visibility — typically true when readiness is
		 * green or when enrollment is already in flight. */
		visible: boolean;
		/** `summary.system?.manufacturer` from system.hardware.summary.
		 * Used to pick the vendor-specific BIOS hint. */
		manufacturer: string | null | undefined;
	}
	const { visible, manufacturer }: Props = $props();

	const client = getClient();

	let enrollment: SecureBootEnrollmentStatusResponse | null = $state(null);
	let loading = $state(true);
	let busy = $state(false);
	let showJournal = $state(false);
	let pollTimer: ReturnType<typeof setInterval> | null = null;

	async function refresh() {
		try {
			enrollment = await client.call<SecureBootEnrollmentStatusResponse>(
				'system.secure_boot.enrollment.status',
			);
		} catch {
			enrollment = null;
		}
		loading = false;
	}

	onMount(() => {
		refresh();
		// Light polling so a state transition that happened in another
		// tab (or after the operator's BIOS dance + reboot) shows up
		// without manual refresh. 5 s is conservative; the engine call
		// is just a JSON read so the cost is negligible.
		pollTimer = setInterval(refresh, 5_000);
	});

	onDestroy(() => {
		if (pollTimer) clearInterval(pollTimer);
	});

	async function begin() {
		const ok = await confirm(
			'Begin Secure Boot enrollment',
			'This is highly experimental. NASty will write a Nix overlay that enables lanzaboote + auto-enrollment. Before you commit to firmware-level enrollment you can still abort by clicking Abort below. Continue?',
			{ confirmLabel: 'Begin' },
		);
		if (!ok) return;
		busy = true;
		const result = await withToast(
			() => client.call<SecureBootEnrollmentStatusResponse>('system.secure_boot.enrollment.begin'),
			'Secure Boot enrollment started — overlay written.',
		);
		busy = false;
		if (result) {
			enrollment = result;
			rebootState.set();
		}
	}

	async function abort() {
		// Abort copy depends on whether a rebuild has actually
		// happened in this ceremony. Without that distinction the
		// dialog used to tell every operator they'd need to run
		// nasty-rebuild again to revert — even when the overlay
		// file was the only thing written and the running system
		// was untouched. The engine tracks rebuild_triggered_at;
		// we just branch on it.
		const rebuilt = enrollment?.rebuild_triggered_at !== null
			&& enrollment?.rebuild_triggered_at !== undefined;
		const msg = rebuilt
			? 'You already ran the Rebuild step earlier — the running system has lanzaboote-signed boot artifacts on the ESP. Aborting removes the Nix overlay, but you\'ll need to run Rebuild once more (or click Update) to revert those artifacts before they activate at next reboot. OK to abort?'
			: 'Removes the Nix overlay file. Nothing else was applied yet — the running system is unchanged. OK to abort?';
		const ok = await confirm('Abort enrollment', msg, { confirmLabel: 'Abort' });
		if (!ok) return;
		busy = true;
		const result = await withToast(
			() => client.call<SecureBootEnrollmentStatusResponse>(
				'system.secure_boot.enrollment.abort',
				{ reason: 'operator aborted via wizard' },
			),
			'Secure Boot enrollment aborted.',
		);
		busy = false;
		if (result) enrollment = result;
	}

	async function rebuild() {
		busy = true;
		const result = await withToast(
			() => client.call<{ triggered: boolean }>('system.secure_boot.enrollment.rebuild'),
			'Rebuild started — this will take several minutes.',
		);
		busy = false;
		if (result) {
			// Bump the poll immediately so the status flips to
			// `running` without waiting the 5 s tick.
			await refresh();
		}
	}

	async function complete() {
		busy = true;
		const result = await withToast(
			() => client.call<SecureBootEnrollmentStatusResponse>('system.secure_boot.enrollment.complete'),
			'Secure Boot enrollment marked complete.',
		);
		busy = false;
		if (result) enrollment = result;
	}

	// Per-vendor BIOS hints. Matched case-insensitively on the leading
	// token of the DMI manufacturer string. New vendors land here as
	// operators report them. Falling back to the generic block is fine
	// — vendor steps shouldn't be the only path to enrollment.
	function vendorHint(mfr: string | null | undefined): { vendor: string; steps: string } | null {
		if (!mfr) return null;
		const m = mfr.toLowerCase();
		if (m.startsWith('supermicro')) {
			return {
				vendor: 'Supermicro',
				steps:
					'Reboot, press DEL at POST (or use IPMI iKVM). ' +
					'Security → Secure Boot → Erase All Secure Boot Settings. ' +
					'Save & exit.',
			};
		}
		if (m.startsWith('asrock')) {
			return {
				vendor: 'ASRock Rack',
				steps:
					'Reboot, press DEL at POST. ' +
					'Security → Secure Boot → Key Management → Reset to Setup Mode. ' +
					'Save & exit.',
			};
		}
		if (m.startsWith('asus')) {
			return {
				vendor: 'ASUS',
				steps:
					'Reboot, press DEL at POST. ' +
					'Press F7 for Advanced Mode. ' +
					'Boot → Secure Boot → Key Management → Clear Secure Boot Keys. ' +
					'Save & exit.',
			};
		}
		if (m.startsWith('gigabyte')) {
			return {
				vendor: 'Gigabyte',
				steps:
					'Reboot, press DEL at POST. ' +
					'Boot → Secure Boot → PK Management → Delete PK. ' +
					'Save & exit.',
			};
		}
		if (m.startsWith('lenovo')) {
			return {
				vendor: 'Lenovo',
				steps:
					'Reboot, press F1 at POST. ' +
					'Security → Secure Boot → Reset to Setup Mode. ' +
					'Save & exit.',
			};
		}
		if (m.startsWith('framework')) {
			return {
				vendor: 'Framework',
				steps:
					'Reboot, press F2 at POST. ' +
					'Security → Erase all Secure Boot Settings. ' +
					'Save & exit. (Some Framework batches have a buggy implementation — if it doesn’t take, clear PK, KEK and db individually.)',
			};
		}
		if (m.startsWith('dell')) {
			return {
				vendor: 'Dell',
				steps:
					'Reboot, press F2 at POST. ' +
					'System Security → Secure Boot → Custom Mode → Delete PK. ' +
					'Save & exit.',
			};
		}
		if (m.startsWith('hewlett packard') || m.startsWith('hp')) {
			return {
				vendor: 'HPE / HP',
				steps:
					'Reboot, press F9 at POST. ' +
					'System Configuration → BIOS/Platform → Secure Boot → Reset to Setup Mode. ' +
					'Save & exit.',
			};
		}
		return null;
	}

	const hint = $derived(vendorHint(manufacturer));
</script>

{#if visible}
	<Card class="mb-6">
		<CardContent class="pt-4 pb-4">
			<div class="mb-3 flex items-baseline justify-between">
				<div class="flex items-center gap-2">
					<ShieldCheck size={18} class="text-muted-foreground" />
					<h2 class="text-base font-semibold">Secure Boot enrollment</h2>
					<span class="rounded bg-amber-950/60 px-2 py-0.5 text-[10px] font-bold uppercase tracking-wide text-amber-300">
						Experimental
					</span>
				</div>
				{#if enrollment}
					<span class="text-xs text-muted-foreground">
						Phase: {enrollment.phase.kind.replace('_', ' ')}
					</span>
				{/if}
			</div>

			<div class="mb-3 rounded border border-amber-700/40 bg-amber-950/40 px-3 py-2 text-xs text-amber-200">
				<div class="flex items-start gap-2">
					<AlertTriangle size={14} class="mt-0.5 shrink-0" />
					<div>
						<strong>Highly experimental.</strong>
						This is a multi-step ceremony involving a BIOS visit. Recovery from a bad enrollment requires physical or IPMI access to disable Secure Boot in firmware. Don't run this remotely without a recovery plan. Back up your bcachefs encryption keys before starting.
					</div>
				</div>
			</div>

			{#if loading}
				<div class="text-sm text-muted-foreground">Loading…</div>
			{:else if !enrollment}
				<div class="text-sm text-muted-foreground">Could not load enrollment status.</div>
			{:else if enrollment.phase.kind === 'not_started' || enrollment.phase.kind === 'aborted'}
				{#if enrollment.phase.kind === 'aborted'}
					<p class="mb-3 text-xs text-muted-foreground">
						Previous attempt was aborted: <em>{enrollment.phase.reason}</em>. You can begin again.
					</p>
				{/if}
				<Button size="sm" disabled={busy} onclick={begin}>
					{busy ? 'Starting…' : 'Begin enrollment'}
				</Button>
			{:else if enrollment.phase.kind === 'overlay_written'}
				{@const rebuildStatus = enrollment.rebuild.status}
				{@const rebuildRunning = rebuildStatus === 'running'}
				{@const rebuildSucceeded = rebuildStatus === 'succeeded'}
				{@const rebuildFailed = rebuildStatus === 'failed'}

				<ol class="mb-4 list-decimal space-y-4 pl-5 text-sm">
					<li>
						<strong>Apply the overlay.</strong>
						Engine wrote the Nix overlay during Begin; this step actually runs <code class="rounded bg-muted px-1 font-mono text-xs">nasty-rebuild</code> so lanzaboote-signed boot artifacts land on your ESP and the platform keys get staged for firmware auto-enrollment.
						<div class="mt-2 flex flex-wrap items-center gap-2">
							{#if !rebuildRunning && !rebuildSucceeded}
								<Button size="sm" disabled={busy} onclick={rebuild}>
									{rebuildFailed ? 'Retry rebuild' : 'Rebuild now'}
								</Button>
							{:else if rebuildRunning}
								<span class="inline-flex items-center gap-2 text-xs text-amber-300">
									<RefreshCw size={14} class="animate-spin" />
									Rebuilding… (several minutes; the engine may briefly disconnect during the switch)
								</span>
							{:else if rebuildSucceeded}
								<span class="inline-flex items-center gap-2 text-xs text-emerald-400">
									<ShieldCheck size={14} />
									Rebuild succeeded — signed boot artifacts on ESP.
								</span>
							{/if}
							{#if rebuildFailed}
								<span class="text-xs text-amber-300">
									Rebuild failed{enrollment.rebuild.exit_code !== null
										? ` (exit ${enrollment.rebuild.exit_code})`
										: ''}.
								</span>
							{/if}
							{#if enrollment.rebuild.journal_tail.length > 0}
								<button
									type="button"
									class="text-xs underline text-muted-foreground hover:text-foreground"
									onclick={() => (showJournal = !showJournal)}
								>
									{showJournal ? 'Hide' : 'Show'} rebuild output
								</button>
							{/if}
						</div>
						{#if showJournal && enrollment.rebuild.journal_tail.length > 0}
							<pre class="mt-2 max-h-64 overflow-auto rounded border border-border bg-black/50 p-2 text-[10px] leading-tight text-muted-foreground">{enrollment.rebuild.journal_tail.join('\n')}</pre>
						{/if}
					</li>
					<li class:opacity-50={!rebuildSucceeded}>
						<strong>Reboot into BIOS / UEFI setup</strong> and put firmware in <em>Setup Mode</em>.
						{#if hint}
							<div class="mt-2 rounded border border-border bg-muted/30 p-3 text-xs">
								<div class="mb-1 font-semibold">For your hardware ({hint.vendor})</div>
								<div class="text-muted-foreground">{hint.steps}</div>
							</div>
						{/if}
						<div class="mt-2 rounded border border-border bg-muted/10 p-3 text-xs">
							<div class="mb-1 font-semibold">Generic</div>
							<div class="text-muted-foreground">
								Reboot into BIOS/UEFI setup (typically DEL or F2 at POST), find the Secure Boot section, look for <em>Reset to Setup Mode</em>, <em>Erase Platform Key</em>, or <em>Clear Secure Boot Keys</em>. Save and exit. Exact wording varies by vendor and BIOS version.
							</div>
						</div>
					</li>
					<li class:opacity-50={!rebuildSucceeded}>
						<strong>Reboot back into NASty.</strong>
						systemd-boot will auto-enroll the staged keys on this boot (Setup Mode is required for the firmware to accept them). NASty's engine detects the transition automatically; this wizard will advance to the post-enrollment step.
					</li>
				</ol>

				<div class="flex flex-wrap gap-2">
					<Button size="sm" variant="secondary" disabled={busy} onclick={refresh}>
						{busy ? '…' : 'Refresh status'}
					</Button>
					<Button size="sm" variant="outline" disabled={busy} onclick={abort}>
						Abort (still possible)
					</Button>
				</div>
				<p class="mt-2 text-xs text-muted-foreground">
					{#if enrollment.rebuild_triggered_at === null}
						<strong>Abort is clean</strong> at this point — nothing has been applied to the running system. Aborting just removes the Nix overlay.
					{:else}
						<strong>Abort still possible</strong> — the rebuild has run, so abort + the next reboot are still entirely reversible from software. After your reboot with Setup Mode active, the firmware-level commit becomes irreversible.
					{/if}
				</p>
			{:else if enrollment.phase.kind === 'post_enrollment'}
				<div class="mb-3 flex items-start gap-2 rounded border border-emerald-700/40 bg-emerald-950/40 px-3 py-2 text-xs text-emerald-200">
					<ShieldCheck size={14} class="mt-0.5 shrink-0" />
					<div>
						<strong>Secure Boot is now enforcing.</strong>
						The firmware has accepted NASty's platform keys. Lanzaboote-signed kernel + initrd are loaded on every boot from here on; an unsigned boot artifact (memtest, rescue image, etc.) won't launch unless you disable SB in firmware again.
					</div>
				</div>

				{#if enrollment.phase.stale_tpm_bindings.length > 0}
					<div class="mb-3">
						<div class="mb-2 text-sm font-semibold">
							Re-bind your TPM-sealed filesystems
						</div>
						<p class="mb-2 text-xs text-muted-foreground">
							Secure Boot activation changed your firmware’s PCR-7 reading.
							Filesystems sealed against the old reading can’t auto-unlock until they’re re-bound under the new one.
							Go to <a href="/filesystems" class="underline">Filesystems</a> and click <em>Bind to TPM</em> on each row:
						</p>
						<ul class="mb-2 list-disc pl-5 text-sm font-mono text-amber-200">
							{#each enrollment.phase.stale_tpm_bindings as fs (fs)}
								<li>{fs}</li>
							{/each}
						</ul>
					</div>
				{:else}
					<p class="mb-3 text-xs text-muted-foreground">
						No TPM-sealed filesystems to re-bind on this box.
					</p>
				{/if}

				<Button size="sm" disabled={busy} onclick={complete}>
					{busy ? '…' : 'Mark enrollment complete'}
				</Button>
				<p class="mt-2 text-xs text-muted-foreground">
					Marking complete just dismisses the wizard. You can re-run any of the listed re-bindings later from the Filesystems page.
				</p>
			{:else if enrollment.phase.kind === 'complete'}
				<div class="rounded border border-emerald-700/40 bg-emerald-950/40 px-3 py-2 text-xs text-emerald-200">
					<ShieldCheck size={14} class="mr-1 inline" />
					Secure Boot enrollment complete on {new Date(enrollment.phase.completed_at * 1000).toLocaleString()}.
				</div>
			{/if}
		</CardContent>
	</Card>
{/if}
