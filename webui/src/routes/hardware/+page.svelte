<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';
	import type {
		GuestToolsStatus,
		HardwareSummary,
		IommuGroup,
		PassthroughConfig,
		PciDevice,
		SecureBootReadinessReport,
	} from '$lib/types';
	import { formatBytes } from '$lib/format';
	import { rebootState } from '$lib/reboot.svelte';
	import { ChevronDown, ChevronRight, RefreshCw } from '@lucide/svelte';
	import SecureBootEnrollmentWizard from '$lib/components/SecureBootEnrollmentWizard.svelte';

	let summary: HardwareSummary | null = $state(null);
	let groups: IommuGroup[] = $state([]);
	let loading = $state(true);
	let refreshing = $state(false);
	let expanded = $state(new Set<number>());
	let filter = $state('');
	let showAllDimms = $state(false);
	// Readiness probe for the lanzaboote opt-in. Fetched lazily — only
	// when the SB card resolves to a state where opting in could be
	// possible (UEFI + SB currently off + supported). Boxes that
	// can't do SB never trigger this call.
	let sbReadiness: SecureBootReadinessReport | null = $state(null);
	let sbReadinessLoading = $state(false);

	// TPM2_PT_MANUFACTURER 4-char ASCII codes → human-readable vendor names.
	// Source: TCG Vendor ID Registry (the assigned-numbers table that the
	// chips burn into firmware). Trailing-space codes ("IBM ", "ATML")
	// are preserved exactly because that's what the chip publishes.
	const TPM_MANUFACTURERS: Record<string, string> = {
		IFX: 'Infineon',
		STM: 'STMicroelectronics',
		NTC: 'Nuvoton',
		'IBM ': 'swtpm (IBM software TPM)',
		AMD: 'AMD fTPM',
		INTC: 'Intel PTT',
		MSFT: 'Microsoft',
		ATML: 'Atmel',
		BRCM: 'Broadcom',
		HPI: 'HPI',
		HPE: 'HPE',
		LEN: 'Lenovo',
		FLYS: 'Flyslice',
		SMSN: 'Samsung',
		QCOM: 'Qualcomm',
		SNS: 'Sinosun',
		TXN: 'Texas Instruments',
		WEC: 'Winbond',
		ROCC: 'Fuzhou Rockchip',
		GOOG: 'Google',
	};

	// Persisted passthrough config from the engine. `pending` is the
	// local edit set (before Apply); each entry is "vendor:device" so
	// Set membership tests work without a custom equality predicate.
	let passthroughConfig: PassthroughConfig = $state({ devices: [], ids: [] });
	let pending = $state(new Set<string>());
	let saving = $state(false);

	// VM guest-tools opt-in (VMware open-vm-tools / Hyper-V). The QEMU
	// guest agent is always-on, so this card only appears as actionable
	// when a non-KVM hypervisor is detected (or the operator wants it
	// anyway). `guestBusy` covers the set RPC; the rebuild itself is
	// tracked via `guestTools.rebuild_state`, polled while running.
	let guestTools: GuestToolsStatus | null = $state(null);
	let guestBusy = $state(false);
	let guestPoll: ReturnType<typeof setInterval> | null = null;

	const client = getClient();

	function stopGuestPoll() {
		if (guestPoll !== null) {
			clearInterval(guestPoll);
			guestPoll = null;
		}
	}

	// Poll guest-tools status every 3s while a rebuild is in flight, so
	// the card reflects progress and stops once it lands (or fails).
	function startGuestPoll() {
		stopGuestPoll();
		guestPoll = setInterval(async () => {
			try {
				guestTools = await client.call<GuestToolsStatus>('system.guest_tools.status');
			} catch {
				/* transient — keep the last snapshot */
			}
			if (guestTools?.rebuild_state !== 'running') stopGuestPoll();
		}, 3000);
	}

	async function setGuestTools(enabled: boolean) {
		guestBusy = true;
		const result = await withToast(
			() => client.call<GuestToolsStatus>('system.guest_tools.set', { enabled }),
			enabled
				? 'Guest tools enabling — building system…'
				: 'Guest tools disabling — building system…',
		);
		if (result) {
			guestTools = result;
			startGuestPoll();
		}
		guestBusy = false;
	}

	async function load() {
		try {
			const [s, g, p] = await Promise.all([
				client.call<HardwareSummary>('system.hardware.summary'),
				client.call<IommuGroup[]>('system.hardware.iommu'),
				client.call<PassthroughConfig>('system.passthrough.get'),
			]);
			summary = s;
			groups = g;
			passthroughConfig = p;
			pending = new Set((p.devices || []).map((d) => d.address));
		} catch {
			summary = null;
			groups = [];
			passthroughConfig = { devices: [], ids: [] };
			pending = new Set();
		}
		// Only probe SB readiness when the firmware-state read suggests
		// it's worth showing the checklist — i.e. SB is currently off
		// and the firmware can do SB at all. Boxes in `Enabled`,
		// `Unsupported`, or `Unknown` states skip the call entirely,
		// keeping the Hardware page cheap on hardware that can never
		// opt in.
		const sb = summary?.secure_boot;
		const shouldProbe =
			sb?.enabled === false && sb?.unsupported !== true;
		if (shouldProbe) {
			sbReadinessLoading = true;
			try {
				sbReadiness = await client.call<SecureBootReadinessReport>(
					'system.secure_boot.readiness',
				);
			} catch {
				sbReadiness = null;
			}
			sbReadinessLoading = false;
		} else {
			sbReadiness = null;
		}
		// Guest-tools status — separately guarded so an older engine
		// without the RPC just hides the card rather than failing the
		// whole page load.
		try {
			guestTools = await client.call<GuestToolsStatus>('system.guest_tools.status');
			if (guestTools.rebuild_state === 'running') startGuestPoll();
		} catch {
			guestTools = null;
		}
		loading = false;
	}

	// Claims are per-device (BDF): identical siblings — e.g. SR-IOV
	// VFs sharing one vendor:device pair — toggle independently.
	function devKey(d: PciDevice): string {
		return d.bdf;
	}

	function togglePassthrough(d: PciDevice) {
		const key = devKey(d);
		if (pending.has(key)) pending.delete(key);
		else pending.add(key);
		pending = new Set(pending);
	}

	function discardPending() {
		pending = new Set((passthroughConfig.devices || []).map((d) => d.address));
	}

	let dirty = $derived.by(() => {
		const saved = new Set((passthroughConfig.devices || []).map((d) => d.address));
		if (saved.size !== pending.size) return true;
		for (const k of pending) if (!saved.has(k)) return true;
		return false;
	});

	async function applyPassthrough() {
		saving = true;
		const addresses = [...pending];
		const result = await withToast(
			() => client.call<PassthroughConfig>('system.passthrough.update', { addresses }),
			'Passthrough config saved — reboot required to apply',
		);
		if (result) {
			passthroughConfig = result;
			pending = new Set((result.devices || []).map((d) => d.address));
			rebootState.set();
		}
		saving = false;
	}

	async function refresh() {
		refreshing = true;
		await load();
		refreshing = false;
	}

	function toggle(id: number) {
		if (expanded.has(id)) expanded.delete(id);
		else expanded.add(id);
		expanded = new Set(expanded);
	}

	function expandAll() {
		expanded = new Set(filteredGroups.map((g) => g.id));
	}

	function collapseAll() {
		expanded = new Set();
	}

	/** Single-line description for a device. Falls back through human
	 * names → numeric IDs so the user always sees something useful even
	 * on bleeding-edge hardware where pci.ids has no entry. */
	function describe(d: PciDevice): string {
		const vendor = d.vendor_name ?? d.vendor_id;
		const device = d.device_name ?? d.device_id;
		return `${vendor} ${device}`;
	}

	/** Group is "active for passthrough" if any device in it is bound to
	 * vfio-pci. We highlight these so users can see at a glance which
	 * groups are currently claimed for VMs. */
	function isPassthroughGroup(g: IommuGroup): boolean {
		return g.devices.some((d) => d.driver === 'vfio-pci');
	}

	function driverBadgeVariant(driver: string | null): 'default' | 'secondary' | 'outline' {
		if (!driver) return 'outline';
		if (driver === 'vfio-pci') return 'default';
		return 'secondary';
	}

	let filteredGroups = $derived.by(() => {
		const q = filter.trim().toLowerCase();
		if (!q) return groups;
		return groups.filter((g) =>
			g.devices.some((d) => {
				const haystack = [
					d.bdf,
					d.vendor_id,
					d.device_id,
					d.vendor_name,
					d.device_name,
					d.class_name,
					d.driver,
				]
					.filter(Boolean)
					.join(' ')
					.toLowerCase();
				return haystack.includes(q);
			}),
		);
	});

	onMount(load);
	onDestroy(stopGuestPoll);
</script>

<div class="mb-4 flex items-center justify-between">
	<div>
		<p class="text-sm text-muted-foreground">
			Hardware overview and IOMMU groupings. Devices in the same IOMMU group must be passed through
			together.
		</p>
	</div>
	<Button size="sm" variant="secondary" onclick={refresh} disabled={refreshing}>
		<RefreshCw class={refreshing ? 'animate-spin' : ''} size={14} />
		Refresh
	</Button>
</div>

{#if loading}
	<Card>
		<CardContent class="py-12 text-center text-sm text-muted-foreground">Loading…</CardContent>
	</Card>
{:else}
	<!-- ── Hardware overview cards ─────────────────────────────────── -->
	<div class="mb-6 grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4">
		<Card>
			<CardContent class="pt-4 pb-3">
				<h3 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					System
				</h3>
				{#if summary?.system}
					<div class="text-sm font-medium">
						{summary.system.manufacturer ?? 'Unknown'}
						{summary.system.product ?? ''}
					</div>
					{#if summary.system.version}
						<div class="text-xs text-muted-foreground">v{summary.system.version}</div>
					{/if}
				{:else}
					<div class="text-sm text-muted-foreground">—</div>
				{/if}
				{#if summary?.bios}
					<div class="mt-3 border-t border-border/40 pt-2 text-xs text-muted-foreground">
						BIOS: {summary.bios.vendor ?? '—'}
						{#if summary.bios.version} · {summary.bios.version}{/if}
						{#if summary.bios.release_date} · {summary.bios.release_date}{/if}
					</div>
				{/if}
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-4 pb-3">
				<h3 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					CPU
				</h3>
				{#if summary?.cpu}
					<div class="text-sm font-medium">{summary.cpu.model ?? 'Unknown'}</div>
					<div class="mt-1 text-xs text-muted-foreground">
						{summary.cpu.physical_cores > 0
							? `${summary.cpu.physical_cores} core${summary.cpu.physical_cores === 1 ? '' : 's'} · `
							: ''}{summary.cpu.logical_cores} thread{summary.cpu.logical_cores === 1 ? '' : 's'}
						{#if summary.cpu.max_mhz} · {(summary.cpu.max_mhz / 1000).toFixed(2)} GHz{/if}
					</div>
				{:else}
					<div class="text-sm text-muted-foreground">—</div>
				{/if}
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-4 pb-3">
				<h3 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					Memory
				</h3>
				{#if summary && summary.memory.slots_total > 0}
					<div class="text-sm font-medium">{formatBytes(summary.memory.total_bytes)}</div>
					<div class="mt-1 text-xs text-muted-foreground">
						{summary.memory.slots_used} of {summary.memory.slots_total} slot{summary.memory
							.slots_total === 1
							? ''
							: 's'} populated
						{#if summary.memory.ecc} · ECC{/if}
					</div>
				{:else}
					<div class="text-sm text-muted-foreground">—</div>
				{/if}
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-4 pb-3">
				<h3 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					USB
				</h3>
				<div class="text-sm font-medium">
					{summary?.usb.length ?? 0} device{summary?.usb.length === 1 ? '' : 's'}
				</div>
				{#if summary?.usb.length}
					<div class="mt-1 text-xs text-muted-foreground">
						{summary.usb
							.slice(0, 3)
							.map((d) => d.description.split(' ').slice(0, 3).join(' '))
							.join(' · ')}{summary.usb.length > 3 ? ' · …' : ''}
					</div>
				{/if}
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-4 pb-3">
				<h3 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					TPM
				</h3>
				{#if !summary?.tpm}
					<div class="text-sm font-medium">Not present</div>
					<div class="mt-1 text-xs text-muted-foreground">
						Either no TPM chip is installed, the chip is disabled in
						firmware (BIOS fTPM / PTT toggle), or the kernel driver
						isn't loaded.
					</div>
				{:else}
					{@const tpm = summary.tpm}
					{@const isTpm2 = tpm.version_major === 2}
					{@const usable = isTpm2 && tpm.rm_available}
					<div class="text-sm font-medium">
						{#if tpm.version_major}
							TPM {tpm.version_major}.0
						{:else}
							TPM (version unknown)
						{/if}
						{#if usable}
							<span class="ml-1 text-xs text-emerald-400">· ready</span>
						{:else if isTpm2}
							<span class="ml-1 text-xs text-amber-500">· no /dev/tpmrm0</span>
						{:else}
							<span class="ml-1 text-xs text-amber-500">· incompatible (need 2.0)</span>
						{/if}
					</div>
					{@const mfrCode = tpm.manufacturer?.trim()}
					{@const mfrFull = (mfrCode && TPM_MANUFACTURERS[mfrCode]) || mfrCode}
					{@const vendorLine = [mfrFull, tpm.vendor_string?.trim()].filter(Boolean).join(' ')}
					{#if vendorLine}
						<div class="mt-1 text-xs text-muted-foreground">{vendorLine}</div>
					{/if}
				{/if}
			</CardContent>
		</Card>

		<Card>
			<CardContent class="pt-4 pb-3">
				<h3 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					Secure Boot
				</h3>
				{#if summary?.secure_boot.enabled === true}
					<div class="text-sm font-medium">
						Enabled
						<span class="ml-1 text-xs text-emerald-400">· enforcing</span>
					</div>
					{#if summary.secure_boot.setup_mode === true}
						<div class="mt-1 text-xs text-amber-500">
							Firmware in setup mode — accepts unsigned key enrollment.
						</div>
					{/if}
					{#if summary.secure_boot.measured_uki === true}
						<div class="mt-1 text-xs text-emerald-400">
							Measured UKI · kernel and initrd measured into the PCR chain.
						</div>
					{/if}
				{:else if summary?.secure_boot.unsupported === true}
					<div class="text-sm font-medium">
						Unsupported
						<span class="ml-1 text-xs text-muted-foreground">· firmware lacks SB</span>
					</div>
					<div class="mt-1 text-xs text-muted-foreground">
						This UEFI build doesn't support Secure Boot (common on default
						QEMU OVMF). Nothing to enable in firmware — TPM PCR-7 sealing
						still works, just without the measured-boot reinforcement.
					</div>
				{:else if summary?.secure_boot.enabled === false}
					<div class="text-sm font-medium">
						Disabled
						<span class="ml-1 text-xs text-amber-500">· not enforcing</span>
					</div>
					<div class="mt-1 text-xs text-muted-foreground">
						TPM PCR-7 sealing still works but is significantly weaker
						without a measured boot chain.
					</div>
					{#if sbReadinessLoading}
						<div class="mt-2 text-xs text-muted-foreground">Checking readiness…</div>
					{:else if sbReadiness?.ready}
						<div class="mt-2 text-xs text-emerald-400">
							✓ Ready to enable — see checklist below.
						</div>
					{:else if sbReadiness}
						<div class="mt-2 text-xs text-amber-500">
							Not ready — see checklist below.
						</div>
					{/if}
				{:else}
					<div class="text-sm font-medium">Unknown</div>
					<div class="mt-1 text-xs text-muted-foreground">
						{summary?.secure_boot.note ?? 'No status reported.'}
					</div>
				{/if}
			</CardContent>
		</Card>
	</div>

	<!-- ── VM guest tools (only when running under a hypervisor, or already opted in) ─── -->
	{#if guestTools && (guestTools.is_vm || guestTools.enabled)}
		{@const hv = guestTools.hypervisor}
		{@const hvLabel =
			hv === 'vmware'
				? 'VMware / ESXi'
				: hv === 'microsoft'
					? 'Hyper-V'
					: hv === 'kvm' || hv === 'qemu'
						? 'KVM / QEMU'
						: hv}
		{@const building = guestTools.rebuild_state === 'running'}
		{@const failed = guestTools.rebuild_state === 'failed'}
		<Card class="mb-6">
			<CardContent class="pt-4 pb-3">
				<div class="mb-2 flex items-baseline justify-between">
					<h3 class="text-sm font-semibold">VM guest tools</h3>
					<span class="text-xs text-muted-foreground">
						Detected hypervisor: <span class="font-mono">{hvLabel}</span>
					</span>
				</div>

				{#if hv === 'kvm' || hv === 'qemu'}
					<!-- KVM/QEMU is covered by the always-on QEMU guest agent;
						 nothing to enable here. -->
					<div class="text-sm">
						<span class="text-emerald-400">Active</span> · QEMU guest agent runs automatically
						under KVM/Proxmox — graceful shutdown, time sync and guest IP reporting are
						already on. No action needed.
					</div>
					<p class="mt-2 text-xs text-muted-foreground">
						Not seeing the guest IP on your hypervisor? Enable the guest-agent option in
						its VM settings (Proxmox: <span class="font-mono">Options → QEMU Guest Agent</span>)
						and power-cycle the VM — the host only opens the agent channel once that's on.
					</p>
				{:else}
					<p class="mb-3 text-xs text-muted-foreground">
						{#if hv === 'vmware'}
							Enables <code class="font-mono">open-vm-tools</code> so vCenter/ESXi can
							gracefully shut the guest down (clean stop + unmount), sync time, and report
							the guest IP. Downloaded on enable and applied with a system rebuild — no reboot.
						{:else if hv === 'microsoft'}
							Enables Hyper-V integration services (graceful shutdown, time sync, heartbeat).
							Applied with a system rebuild.
						{:else}
							Enables the VMware and Hyper-V integrations. They self-gate on the running
							hypervisor, so only the matching one activates. Applied with a system rebuild.
						{/if}
					</p>

					<div class="flex items-center gap-3">
						{#if guestTools.enabled}
							<Button
								variant="outline"
								size="sm"
								disabled={guestBusy || building}
								onclick={() => setGuestTools(false)}
							>
								Disable
							</Button>
							{#if building}
								<span class="flex items-center gap-2 text-xs text-amber-400">
									<RefreshCw class="h-3 w-3 animate-spin" /> Rebuilding…
								</span>
							{:else if failed}
								<span class="text-xs text-rose-400">Last rebuild failed — see log below.</span>
							{:else}
								<Badge variant="outline" class="text-emerald-400">Enabled</Badge>
							{/if}
						{:else}
							<Button
								size="sm"
								disabled={guestBusy || building}
								onclick={() => setGuestTools(true)}
							>
								{building ? 'Building…' : 'Enable'}
							</Button>
							{#if building}
								<span class="flex items-center gap-2 text-xs text-amber-400">
									<RefreshCw class="h-3 w-3 animate-spin" /> Rebuilding…
								</span>
							{:else if failed}
								<span class="text-xs text-rose-400">Last rebuild failed — see log below.</span>
							{/if}
						{/if}
					</div>

					{#if (building || failed) && guestTools.log_tail}
						<pre class="mt-3 max-h-48 overflow-auto rounded border border-border bg-muted/30 p-2 text-[11px] leading-snug text-muted-foreground whitespace-pre-wrap">{guestTools.log_tail}</pre>
					{/if}
				{/if}
			</CardContent>
		</Card>
	{/if}

	<!-- ── Secure Boot readiness checklist (only when SB is currently off + capable) ─── -->
	{#if sbReadiness}
		<Card class="mb-6">
			<CardContent class="pt-4 pb-3">
				<div class="mb-3 flex items-baseline justify-between">
					<h3 class="text-sm font-semibold">Secure Boot · readiness</h3>
					{#if sbReadiness.ready}
						<span class="text-xs text-emerald-400">All checks pass</span>
					{:else}
						<span class="text-xs text-amber-500">Not ready</span>
					{/if}
				</div>

				{#if !sbReadiness.ready && sbReadiness.blocker}
					<div class="mb-3 rounded border border-amber-700/40 bg-amber-950/40 px-3 py-2 text-xs text-amber-200">
						<strong>Blocker:</strong> {sbReadiness.blocker}
					</div>
				{/if}

				<!-- Per-check rows. Each row is one of ✓ (pass), ✗ (fail),
					 or — (not applicable / unknown). Inlined rather than
					 abstracted into a snippet because each row's params
					 are short and the local repetition is more readable
					 than a typed snippet helper. -->
				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.uefi_boot}<span class="text-emerald-400">✓</span>
						{:else}<span class="text-amber-500">✗</span>{/if}
					</span>
					<span class="flex-1">UEFI boot</span>
				</div>

				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.sb_supported_by_firmware === true}<span class="text-emerald-400">✓</span>
						{:else if sbReadiness.sb_supported_by_firmware === false}<span class="text-amber-500">✗</span>
						{:else}<span class="text-muted-foreground">—</span>{/if}
					</span>
					<span class="flex-1">Firmware supports Secure Boot</span>
				</div>

				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.sb_currently_off === true}<span class="text-emerald-400">✓</span>
						{:else if sbReadiness.sb_currently_off === false}<span class="text-amber-500">✗</span>
						{:else}<span class="text-muted-foreground">—</span>{/if}
					</span>
					<span class="flex-1">Secure Boot currently off (ready to enable)</span>
				</div>

				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.tpm2_available}<span class="text-emerald-400">✓</span>
						{:else}<span class="text-amber-500">✗</span>{/if}
					</span>
					<span class="flex-1">TPM2 available</span>
				</div>

				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.esp_free_bytes === null}<span class="text-muted-foreground">—</span>
						{:else if sbReadiness.esp_free_bytes >= sbReadiness.esp_required_bytes}<span class="text-emerald-400">✓</span>
						{:else}<span class="text-amber-500">✗</span>{/if}
					</span>
					<span class="flex-1">
						ESP headroom
						{#if sbReadiness.esp_free_bytes !== null}
							<span class="ml-1 text-muted-foreground">
								· {formatBytes(sbReadiness.esp_free_bytes)} free ·
								{formatBytes(sbReadiness.esp_required_bytes)} required
							</span>
						{:else}
							<span class="ml-1 text-muted-foreground">· /boot not a separate mount</span>
						{/if}
					</span>
				</div>

				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.wrapper_has_lanzaboote_input === true}<span class="text-emerald-400">✓</span>
						{:else}<span class="text-muted-foreground">—</span>{/if}
					</span>
					<span class="flex-1">
						Wrapper flake declares lanzaboote input
						{#if sbReadiness.wrapper_has_lanzaboote_input === false}
							<span class="ml-1 text-muted-foreground">
								· added automatically when you enable Secure Boot
							</span>
						{:else if sbReadiness.wrapper_has_lanzaboote_input === null}
							<span class="ml-1 text-muted-foreground">
								· /etc/nixos/flake.nix could not be read
							</span>
						{/if}
					</span>
				</div>

				<div class="flex items-start gap-3 py-1 text-xs">
					<span class="w-4 shrink-0 font-mono">
						{#if sbReadiness.sbctl_keys_already_generated}<span class="text-emerald-400">✓</span>
						{:else}<span class="text-muted-foreground">—</span>{/if}
					</span>
					<span class="flex-1">
						sbctl keys already generated
						{#if !sbReadiness.sbctl_keys_already_generated}
							<span class="ml-1 text-muted-foreground">
								· will be created on first SB-enabled boot
							</span>
						{/if}
					</span>
				</div>

				<div class="mt-3 border-t border-border/40 pt-3 text-xs text-muted-foreground">
					{#if sbReadiness.ready}
						This box meets every prerequisite for enabling Secure Boot via
						lanzaboote. The "Enable Secure Boot" action that walks operators
						through the BIOS Setup-Mode visit and re-seal will land in a
						follow-up PR.
					{:else}
						Once the blockers above are resolved, the "Enable Secure Boot"
						action will become available here.
					{/if}
				</div>
			</CardContent>
		</Card>
	{/if}

	<!-- ── Secure Boot enrollment ceremony (experimental) ─────────────
		 Renders when readiness is all-green (the SB-capable box can
		 take the next step) OR when an enrollment is already in flight
		 (so the wizard remains visible after the operator's reboot
		 dance even if readiness now reports SB-already-on, which would
		 hide the readiness card). The component itself decides what
		 to show per phase. -->
	<SecureBootEnrollmentWizard
		visible={sbReadiness?.ready === true
			|| (summary?.secure_boot.enabled === true)}
		manufacturer={summary?.system?.manufacturer}
	/>

	<!-- ── DIMM detail (collapsed by default) ──────────────────────── -->
	{#if summary && summary.memory.dimms.length > 0}
		<Card class="mb-6">
			<CardContent class="p-0">
				<button
					onclick={() => (showAllDimms = !showAllDimms)}
					class="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-accent/50"
				>
					{#if showAllDimms}
						<ChevronDown size={16} class="text-muted-foreground" />
					{:else}
						<ChevronRight size={16} class="text-muted-foreground" />
					{/if}
					<span class="text-sm font-medium">Memory slots</span>
					<span class="text-xs text-muted-foreground">
						{summary.memory.slots_used} populated of {summary.memory.dimms.length}
					</span>
				</button>
				{#if showAllDimms}
					<div class="border-t border-border bg-muted/20 px-4 py-3">
						<table class="w-full text-sm">
							<thead>
								<tr
									class="text-left text-[0.7rem] uppercase tracking-wide text-muted-foreground"
								>
									<th class="pb-2 font-medium">Slot</th>
									<th class="pb-2 font-medium">Size</th>
									<th class="pb-2 font-medium">Type</th>
									<th class="pb-2 font-medium">Speed</th>
									<th class="pb-2 font-medium">Manufacturer</th>
									<th class="pb-2 font-medium">Part #</th>
								</tr>
							</thead>
							<tbody>
								{#each summary.memory.dimms as dimm (dimm.locator)}
									<tr class="border-t border-border/40 {dimm.size_bytes === 0 ? 'opacity-50' : ''}">
										<td class="py-2 pr-3 font-mono text-xs">{dimm.locator}</td>
										<td class="py-2 pr-3"
											>{dimm.size_bytes === 0 ? '—' : formatBytes(dimm.size_bytes)}</td
										>
										<td class="py-2 pr-3 text-xs">{dimm.mem_type ?? '—'}</td>
										<td class="py-2 pr-3 text-xs"
											>{dimm.speed_mts ? `${dimm.speed_mts} MT/s` : '—'}</td
										>
										<td class="py-2 pr-3 text-xs">{dimm.manufacturer ?? '—'}</td>
										<td class="py-2 font-mono text-xs">{dimm.part_number ?? '—'}</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			</CardContent>
		</Card>
	{/if}

	<!-- ── IOMMU group tree ───────────────────────────────────────── -->
	{#if groups.length === 0}
		<Card>
			<CardContent class="py-8 text-center">
				<p class="mb-1 font-medium">No IOMMU groups found</p>
				<p class="text-sm text-muted-foreground">
					IOMMU is likely off in BIOS. Enable VT-d (Intel) or AMD-Vi (AMD) and reboot. NASty's
					kernel params already pass
					<code class="font-mono">intel_iommu=on amd_iommu=on iommu=pt</code>.
				</p>
			</CardContent>
		</Card>
	{:else}
	<h2 class="mb-3 text-base font-semibold">IOMMU groups</h2>
	<p class="mb-3 max-w-3xl text-xs text-muted-foreground">
		Mark a device for passthrough to claim it with <code class="font-mono">vfio-pci</code> at boot
		— the kernel binds it before regular drivers, freeing it for assignment to a VM. Changes
		take effect after the next system update and reboot. Marking by vendor:device claims
		<strong>all</strong> matching devices on the system.
	</p>

	{#if dirty}
		<div
			class="mb-3 flex items-center gap-3 rounded-lg border border-amber-700 bg-amber-950/40 px-4 py-2.5 text-sm"
		>
			<span class="font-medium text-amber-200">{pending.size - (passthroughConfig.devices || []).length >= 0 ? '+' : ''}{pending.size - (passthroughConfig.devices || []).length} change{Math.abs(pending.size - (passthroughConfig.devices || []).length) === 1 ? '' : 's'} pending</span>
			<span class="text-xs text-amber-300/80">
				Apply will rewrite <code class="font-mono">/etc/nixos/passthrough.nix</code> and require a
				reboot to take effect.
			</span>
			<div class="ml-auto flex gap-2">
				<Button size="xs" variant="ghost" onclick={discardPending} disabled={saving}>Discard</Button>
				<Button size="xs" onclick={applyPassthrough} disabled={saving}>
					{saving ? 'Saving…' : 'Apply'}
				</Button>
			</div>
		</div>
	{/if}

	<div class="mb-3 flex items-center gap-3">
		<input
			bind:value={filter}
			placeholder="Filter by BDF, vendor, device, or driver…"
			class="h-9 flex-1 max-w-md rounded-md border border-input bg-background px-3 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
		/>
		<Button size="xs" variant="ghost" onclick={expandAll}>Expand all</Button>
		<Button size="xs" variant="ghost" onclick={collapseAll}>Collapse all</Button>
		<span class="ml-auto text-xs text-muted-foreground">
			{filteredGroups.length} of {groups.length} groups
		</span>
	</div>

	<div class="space-y-2">
		{#each filteredGroups as group (group.id)}
			{@const passthrough = isPassthroughGroup(group)}
			<Card>
				<CardContent class="p-0">
					<button
						onclick={() => toggle(group.id)}
						class="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-accent/50"
					>
						{#if expanded.has(group.id)}
							<ChevronDown size={16} class="text-muted-foreground" />
						{:else}
							<ChevronRight size={16} class="text-muted-foreground" />
						{/if}
						<span class="font-mono text-sm">Group {group.id}</span>
						<span class="text-xs text-muted-foreground"
							>{group.devices.length} device{group.devices.length === 1 ? '' : 's'}</span
						>
						{#if passthrough}
							<Badge variant="default" class="ml-1 text-[0.6rem]">Passthrough</Badge>
						{/if}
						<span class="ml-auto truncate text-xs text-muted-foreground">
							{group.devices
								.map((d) => d.device_name ?? d.device_id)
								.slice(0, 2)
								.join(' · ')}{group.devices.length > 2 ? ' · …' : ''}
						</span>
					</button>

					{#if expanded.has(group.id)}
						<div class="border-t border-border bg-muted/20 px-4 py-3">
							<table class="w-full text-sm">
								<thead>
									<tr class="text-left text-[0.7rem] uppercase tracking-wide text-muted-foreground">
										<th class="pb-2 font-medium">BDF</th>
										<th class="pb-2 font-medium">Class</th>
										<th class="pb-2 font-medium">Device</th>
										<th class="pb-2 font-medium">IDs</th>
										<th class="pb-2 font-medium">Driver</th>
										<th class="pb-2 font-medium">Passthrough</th>
									</tr>
								</thead>
								<tbody>
									{#each group.devices as device (device.bdf)}
										{@const key = devKey(device)}
										{@const marked = pending.has(key)}
										<tr class="border-t border-border/40">
											<td class="py-2 pr-3 font-mono text-xs">{device.bdf}</td>
											<td class="py-2 pr-3 text-xs text-muted-foreground"
												>{device.class_name ?? device.class_id}</td
											>
											<td class="py-2 pr-3">{describe(device)}</td>
											<td class="py-2 pr-3 font-mono text-xs text-muted-foreground"
												>{device.vendor_id}:{device.device_id}</td
											>
											<td class="py-2 pr-3">
												<Badge variant={driverBadgeVariant(device.driver)} class="text-[0.65rem]">
													{device.driver ?? 'unbound'}
												</Badge>
											</td>
											<td class="py-2">
												<label class="inline-flex cursor-pointer items-center gap-2 text-xs">
													<input
														type="checkbox"
														checked={marked}
														onchange={() => togglePassthrough(device)}
														class="h-3.5 w-3.5 cursor-pointer"
													/>
													<span class="text-muted-foreground">
														{marked ? 'Mark vfio-pci' : ''}
													</span>
												</label>
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</CardContent>
			</Card>
		{/each}
	</div>
	{/if}
{/if}
