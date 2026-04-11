<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { NutConfig, UpsStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';

	const client = getClient();

	let config: NutConfig | null = $state(null);
	let status: UpsStatus | null = $state(null);
	let saving = $state(false);
	let loading = $state(true);

	// Config form state
	let fDriver = $state('');
	let fPort = $state('');
	let fUpsName = $state('');
	let fDescription = $state('');
	let fShutdownPercent = $state('');
	let fShutdownSeconds = $state('');
	let fShutdownCommand = $state('');

	let statusInterval: ReturnType<typeof setInterval> | null = null;

	onMount(async () => {
		await loadAll();
		loading = false;
		statusInterval = setInterval(refreshStatus, 5000);
	});

	onDestroy(() => {
		if (statusInterval) clearInterval(statusInterval);
	});

	async function loadAll() {
		[config, status] = await Promise.all([
			client.call<NutConfig>('system.nut.config.get'),
			client.call<UpsStatus>('system.nut.status'),
		]);
		syncForm();
	}

	async function refreshStatus() {
		try {
			status = await client.call<UpsStatus>('system.nut.status');
		} catch { /* ignore polling errors */ }
	}

	function syncForm() {
		if (!config) return;
		fDriver = config.driver;
		fPort = config.port;
		fUpsName = config.ups_name;
		fDescription = config.description;
		fShutdownPercent = config.shutdown_on_battery_percent.toString();
		fShutdownSeconds = config.shutdown_on_battery_seconds.toString();
		fShutdownCommand = config.shutdown_command;
	}

	async function saveConfig() {
		saving = true;
		const result = await withToast(
			() => client.call<NutConfig>('system.nut.config.update', {
				driver: fDriver,
				port: fPort,
				ups_name: fUpsName,
				description: fDescription || undefined,
				shutdown_on_battery_percent: parseInt(fShutdownPercent) || undefined,
				shutdown_on_battery_seconds: parseInt(fShutdownSeconds) || undefined,
				shutdown_command: fShutdownCommand || undefined,
			}),
			'UPS configuration saved'
		);
		if (result !== undefined) {
			config = result;
			syncForm();
		}
		saving = false;
	}

	function statusColor(s: string): string {
		if (s === 'OL' || s.startsWith('OL ')) return 'text-green-500';
		if (s.includes('OB')) return 'text-yellow-500';
		if (s.includes('LB')) return 'text-red-500';
		return 'text-muted-foreground';
	}

	function statusLabel(s: string): string {
		const parts = s.split(' ').map(code => {
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
		});
		return parts.join(' / ');
	}

	function formatRuntime(seconds: number): string {
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

<h1 class="mb-6 text-xl font-bold">UPS</h1>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}

<!-- Status -->
<section class="mb-6 rounded-lg border border-border p-5">
	<h2 class="mb-4 text-sm font-semibold">Status</h2>

	{#if !status?.available}
		<p class="text-sm text-muted-foreground">UPS not available. Enable the UPS (NUT) service in Services, then configure the driver below.</p>
	{:else}
		<div class="grid grid-cols-2 gap-4 sm:grid-cols-4">
			<div>
				<p class="text-xs text-muted-foreground">Status</p>
				<p class="text-lg font-semibold {statusColor(status.status)}">
					{statusLabel(status.status)}
				</p>
			</div>
			{#if status.battery_charge != null}
				<div>
					<p class="text-xs text-muted-foreground">Battery</p>
					<p class="text-lg font-semibold">{status.battery_charge.toFixed(0)}%</p>
				</div>
			{/if}
			{#if status.battery_runtime != null}
				<div>
					<p class="text-xs text-muted-foreground">Runtime</p>
					<p class="text-lg font-semibold">{formatRuntime(status.battery_runtime)}</p>
				</div>
			{/if}
			{#if status.ups_load != null}
				<div>
					<p class="text-xs text-muted-foreground">Load</p>
					<p class="text-lg font-semibold">{status.ups_load.toFixed(0)}%</p>
				</div>
			{/if}
			{#if status.input_voltage != null}
				<div>
					<p class="text-xs text-muted-foreground">Input Voltage</p>
					<p class="text-sm">{status.input_voltage.toFixed(1)} V</p>
				</div>
			{/if}
			{#if status.output_voltage != null}
				<div>
					<p class="text-xs text-muted-foreground">Output Voltage</p>
					<p class="text-sm">{status.output_voltage.toFixed(1)} V</p>
				</div>
			{/if}
			{#if status.ups_model}
				<div>
					<p class="text-xs text-muted-foreground">Model</p>
					<p class="text-sm">{status.ups_model}</p>
				</div>
			{/if}
			{#if status.ups_serial}
				<div>
					<p class="text-xs text-muted-foreground">Serial</p>
					<p class="text-sm font-mono">{status.ups_serial}</p>
				</div>
			{/if}
		</div>
	{/if}
</section>

<!-- Configuration -->
<section class="rounded-lg border border-border p-5">
	<h2 class="mb-4 text-sm font-semibold">Configuration</h2>

	<div class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
		<div>
			<label for="nut-driver" class="mb-1 block text-xs text-muted-foreground">Driver</label>
			<select id="nut-driver" bind:value={fDriver}
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm">
				<option value="usbhid-ups">usbhid-ups (USB HID)</option>
				<option value="blazer_usb">blazer_usb (Megatec/Q1 USB)</option>
				<option value="nutdrv_qx">nutdrv_qx (Q* protocol USB)</option>
				<option value="snmp-ups">snmp-ups (SNMP)</option>
				<option value="apcsmart">apcsmart (APC Smart serial)</option>
				<option value="usbhid-ups">cyberpower (CyberPower USB)</option>
			</select>
			<p class="mt-0.5 text-[0.6rem] text-muted-foreground">NUT driver for your UPS hardware.</p>
		</div>
		<div>
			<label for="nut-port" class="mb-1 block text-xs text-muted-foreground">Port</label>
			<input id="nut-port" type="text" bind:value={fPort}
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
			<p class="mt-0.5 text-[0.6rem] text-muted-foreground">"auto" for USB, or a device path like /dev/ttyS0.</p>
		</div>
		<div>
			<label for="nut-name" class="mb-1 block text-xs text-muted-foreground">UPS Name</label>
			<input id="nut-name" type="text" bind:value={fUpsName}
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
			<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Identifier used by upsc (e.g. "ups").</p>
		</div>
		<div class="sm:col-span-2 lg:col-span-3">
			<label for="nut-desc" class="mb-1 block text-xs text-muted-foreground">Description</label>
			<input id="nut-desc" type="text" bind:value={fDescription} placeholder="My UPS"
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
		</div>
	</div>

	<h3 class="mb-3 mt-5 text-xs font-semibold text-muted-foreground">Shutdown Policy</h3>
	<div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
		<div>
			<label for="nut-pct" class="mb-1 block text-xs text-muted-foreground">Battery threshold (%)</label>
			<input id="nut-pct" type="number" min="0" max="100" bind:value={fShutdownPercent}
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
			<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Shutdown when battery drops below this.</p>
		</div>
		<div>
			<label for="nut-secs" class="mb-1 block text-xs text-muted-foreground">On-battery timeout (s)</label>
			<input id="nut-secs" type="number" min="0" bind:value={fShutdownSeconds}
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
			<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Shutdown after N seconds on battery. 0 = disabled.</p>
		</div>
		<div>
			<label for="nut-cmd" class="mb-1 block text-xs text-muted-foreground">Shutdown command</label>
			<input id="nut-cmd" type="text" bind:value={fShutdownCommand}
				class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
		</div>
	</div>

	<div class="mt-5">
		<Button onclick={saveConfig} disabled={saving}>
			{saving ? 'Saving...' : 'Save Configuration'}
		</Button>
		<span class="ml-2 text-xs text-muted-foreground">If NUT is running, services will be restarted automatically.</span>
	</div>
</section>

{/if}
