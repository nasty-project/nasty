<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { NutConfig, UpsStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';

	const client = getClient();

	let nutConfig: NutConfig | null = $state(null);
	let savingNut = $state(false);
	let upsStatus: UpsStatus | null = $state(null);
	let upsStatusInterval: ReturnType<typeof setInterval> | null = null;
	let nutDriver = $state('');
	let nutPort = $state('');
	let nutUpsName = $state('');
	let nutDescription = $state('');
	let nutShutdownPercent = $state('');
	let nutShutdownSeconds = $state('');
	let nutShutdownCommand = $state('');

	onMount(async () => {
		await loadNut();
	});

	onDestroy(() => {
		stopUpsPolling();
	});

	async function loadNut() {
		nutConfig = await client.call<NutConfig>('system.nut.config.get');
		if (nutConfig) {
			nutDriver = nutConfig.driver;
			nutPort = nutConfig.port;
			nutUpsName = nutConfig.ups_name;
			nutDescription = nutConfig.description;
			nutShutdownPercent = nutConfig.shutdown_on_battery_percent.toString();
			nutShutdownSeconds = nutConfig.shutdown_on_battery_seconds.toString();
			nutShutdownCommand = nutConfig.shutdown_command;
		}
		await refreshUpsStatus();
		startUpsPolling();
	}

	async function saveNut() {
		savingNut = true;
		await withToast(
			() => client.call('system.nut.config.update', {
				driver: nutDriver,
				port: nutPort,
				ups_name: nutUpsName,
				description: nutDescription || undefined,
				shutdown_on_battery_percent: parseInt(nutShutdownPercent) || undefined,
				shutdown_on_battery_seconds: parseInt(nutShutdownSeconds) || undefined,
				shutdown_command: nutShutdownCommand || undefined,
			}),
			'UPS configuration saved'
		);
		savingNut = false;
		await loadNut();
	}

	async function refreshUpsStatus() {
		try {
			upsStatus = await client.call<UpsStatus>('system.nut.status');
		} catch {
			upsStatus = null;
		}
	}

	function startUpsPolling() {
		stopUpsPolling();
		upsStatusInterval = setInterval(refreshUpsStatus, 5000);
	}

	function stopUpsPolling() {
		if (upsStatusInterval) {
			clearInterval(upsStatusInterval);
			upsStatusInterval = null;
		}
	}

	function upsStatusColor(s: string): string {
		if (s === 'OL' || s.startsWith('OL ')) return 'text-green-500';
		if (s.includes('OB')) return 'text-yellow-500';
		if (s.includes('LB')) return 'text-red-500';
		return 'text-muted-foreground';
	}

	function upsStatusLabel(s: string): string {
		return s.split(' ').map(code => {
			switch (code) {
				case 'OL': return 'Online';
				case 'OB': return 'On Battery';
				case 'LB': return 'Low Battery';
				case 'HB': return 'High Battery';
				case 'RB': return 'Replace Battery';
				case 'CHRG': return 'Charging';
				case 'DISCHRG': return 'Discharging';
				case 'BYPASS': return 'Bypass';
				case 'CAL': return 'Calibrating';
				case 'OFF': return 'Offline';
				case 'OVER': return 'Overloaded';
				case 'TRIM': return 'Trimming';
				case 'BOOST': return 'Boosting';
				case 'FSD': return 'Forced Shutdown';
				default: return code;
			}
		}).join(' / ');
	}

	function formatUpsRuntime(seconds: number): string {
		if (seconds >= 3600) {
			const h = Math.floor(seconds / 3600);
			const m = Math.floor((seconds % 3600) / 60);
			return `${h}h ${m}m`;
		}
		const m = Math.floor(seconds / 60);
		const s = seconds % 60;
		return `${m}m ${s}s`;
	}
</script>

<div>
	<h1 class="text-2xl font-bold">UPS</h1>
	<p class="text-sm text-muted-foreground mt-0.5">Uninterruptible power supply monitoring and shutdown policy via NUT.</p>
</div>

<div class="mt-6">
	{#if !nutConfig}
		<p class="text-muted-foreground">Loading...</p>
	{:else}
		<div class="flex flex-col gap-6">
			{#if upsStatus?.available}
				<section class="rounded-lg border border-border p-5">
					<h3 class="mb-4 text-sm font-semibold">UPS Status</h3>
					<div class="grid grid-cols-2 gap-4 sm:grid-cols-4">
						<div>
							<p class="text-xs text-muted-foreground">Status</p>
							<p class="text-lg font-semibold {upsStatusColor(upsStatus.status)}">
								{upsStatusLabel(upsStatus.status)}
							</p>
						</div>
						{#if upsStatus.battery_charge != null}
							<div>
								<p class="text-xs text-muted-foreground">Battery</p>
								<p class="text-lg font-semibold">{upsStatus.battery_charge.toFixed(0)}%</p>
							</div>
						{/if}
						{#if upsStatus.battery_runtime != null}
							<div>
								<p class="text-xs text-muted-foreground">Runtime</p>
								<p class="text-lg font-semibold">{formatUpsRuntime(upsStatus.battery_runtime)}</p>
							</div>
						{/if}
						{#if upsStatus.ups_load != null}
							<div>
								<p class="text-xs text-muted-foreground">Load</p>
								<p class="text-lg font-semibold">{upsStatus.ups_load.toFixed(0)}%</p>
							</div>
						{/if}
						{#if upsStatus.input_voltage != null}
							<div>
								<p class="text-xs text-muted-foreground">Input Voltage</p>
								<p class="text-sm">{upsStatus.input_voltage.toFixed(1)} V</p>
							</div>
						{/if}
						{#if upsStatus.output_voltage != null}
							<div>
								<p class="text-xs text-muted-foreground">Output Voltage</p>
								<p class="text-sm">{upsStatus.output_voltage.toFixed(1)} V</p>
							</div>
						{/if}
						{#if upsStatus.ups_model}
							<div>
								<p class="text-xs text-muted-foreground">Model</p>
								<p class="text-sm">{upsStatus.ups_model}</p>
							</div>
						{/if}
						{#if upsStatus.ups_serial}
							<div>
								<p class="text-xs text-muted-foreground">Serial</p>
								<p class="text-sm font-mono">{upsStatus.ups_serial}</p>
							</div>
						{/if}
					</div>
				</section>
			{/if}

			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">UPS Hardware</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
					<div>
						<label for="nut-driver" class="mb-1 block text-xs text-muted-foreground">Driver</label>
						<select id="nut-driver" bind:value={nutDriver}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm">
							<option value="usbhid-ups">usbhid-ups (USB HID)</option>
							<option value="blazer_usb">blazer_usb (Megatec/Q1 USB)</option>
							<option value="nutdrv_qx">nutdrv_qx (Q* protocol USB)</option>
							<option value="snmp-ups">snmp-ups (SNMP)</option>
							<option value="apcsmart">apcsmart (APC Smart serial)</option>
						</select>
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">NUT driver for your UPS hardware.</p>
					</div>
					<div>
						<label for="nut-port" class="mb-1 block text-xs text-muted-foreground">Port</label>
						<input id="nut-port" type="text" bind:value={nutPort}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">"auto" for USB, or a device path like /dev/ttyS0.</p>
					</div>
					<div>
						<label for="nut-name" class="mb-1 block text-xs text-muted-foreground">UPS Name</label>
						<input id="nut-name" type="text" bind:value={nutUpsName}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Identifier for upsc (e.g. "ups").</p>
					</div>
					<div class="sm:col-span-2 lg:col-span-3">
						<label for="nut-desc" class="mb-1 block text-xs text-muted-foreground">Description</label>
						<input id="nut-desc" type="text" bind:value={nutDescription} placeholder="My UPS"
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
					</div>
				</div>
			</section>

			<section class="rounded-lg border border-border p-5">
				<h3 class="mb-4 text-sm font-semibold">Shutdown Policy</h3>
				<div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
					<div>
						<label for="nut-pct" class="mb-1 block text-xs text-muted-foreground">Battery threshold (%)</label>
						<input id="nut-pct" type="number" min="0" max="100" bind:value={nutShutdownPercent}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Shutdown when battery drops below this.</p>
					</div>
					<div>
						<label for="nut-secs" class="mb-1 block text-xs text-muted-foreground">On-battery timeout (s)</label>
						<input id="nut-secs" type="number" min="0" bind:value={nutShutdownSeconds}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
						<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Shutdown after N seconds on battery. 0 = disabled.</p>
					</div>
					<div>
						<label for="nut-cmd" class="mb-1 block text-xs text-muted-foreground">Shutdown command</label>
						<input id="nut-cmd" type="text" bind:value={nutShutdownCommand}
							class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
					</div>
				</div>
			</section>

			<div>
				<Button onclick={saveNut} disabled={savingNut}>
					{savingNut ? 'Saving...' : 'Save UPS Configuration'}
				</Button>
				<span class="ml-2 text-xs text-muted-foreground">If NUT is running, services will be restarted automatically.</span>
			</div>
		</div>
	{/if}
</div>
