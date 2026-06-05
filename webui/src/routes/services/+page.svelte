<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { page } from '$app/stores';
	import { getClient } from '$lib/client';
	import { withToast, error } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { ProtocolStatus, AppsStatus, Filesystem, TuningConfig, NutConfig, UpsStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';

	let protocols: ProtocolStatus[] = $state([]);
	let dockerStatus: AppsStatus | null = $state(null);
	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');
	let dockerEnabling = $state(false);
	let loading = $state(true);

	// Per-service config panels
	let configOpen = $state<string | null>(null);

	// Tuning
	let tuning: TuningConfig | null = $state(null);
	let savingTuning = $state(false);
	let tNfsThreads = $state(''); let tNfsLeaseTime = $state(''); let tNfsGraceTime = $state('');
	let tSmbMaxConnections = $state(''); let tSmbDeadtime = $state(''); let tSmbSocketOptions = $state('');
	let tIscsiCmdsnDepth = $state(''); let tIscsiLoginTimeout = $state('');

	async function loadTuning() {
		if (tuning) return;
		tuning = await client.call<TuningConfig>('system.tuning.get');
		if (tuning) {
			tNfsThreads = tuning.nfs_threads.toString();
			tNfsLeaseTime = tuning.nfs_lease_time.toString();
			tNfsGraceTime = tuning.nfs_grace_time.toString();
			tSmbMaxConnections = tuning.smb_max_connections.toString();
			tSmbDeadtime = tuning.smb_deadtime.toString();
			tSmbSocketOptions = tuning.smb_socket_options;
			tIscsiCmdsnDepth = tuning.iscsi_default_cmdsn_depth.toString();
			tIscsiLoginTimeout = tuning.iscsi_login_timeout.toString();
		}
	}

	async function saveTuning() {
		savingTuning = true;
		await withToast(
			() => client.call('system.tuning.update', {
				nfs_threads: parseInt(tNfsThreads) || undefined,
				nfs_lease_time: parseInt(tNfsLeaseTime) || undefined,
				nfs_grace_time: parseInt(tNfsGraceTime) || undefined,
				smb_max_connections: parseInt(tSmbMaxConnections) ?? undefined,
				smb_deadtime: parseInt(tSmbDeadtime) ?? undefined,
				smb_socket_options: tSmbSocketOptions || undefined,
				iscsi_default_cmdsn_depth: parseInt(tIscsiCmdsnDepth) || undefined,
				iscsi_login_timeout: parseInt(tIscsiLoginTimeout) || undefined,
			}),
			'Settings applied'
		);
		savingTuning = false;
		tuning = null; // force reload
		await loadTuning();
	}

	// UPS (NUT) config
	let nutConfig: NutConfig | null = $state(null);
	let upsStatus: UpsStatus | null = $state(null);
	let savingNut = $state(false);
	let nutDriver = $state(''); let nutPort = $state(''); let nutUpsName = $state('');
	let nutDescription = $state('');
	let nutShutdownPercent = $state(''); let nutShutdownSeconds = $state('');

	async function loadNut() {
		if (nutConfig) return;
		nutConfig = await client.call<NutConfig>('system.nut.config.get');
		if (nutConfig) {
			nutDriver = nutConfig.driver;
			nutPort = nutConfig.port;
			nutUpsName = nutConfig.ups_name;
			nutDescription = nutConfig.description;
			nutShutdownPercent = nutConfig.shutdown_on_battery_percent.toString();
			nutShutdownSeconds = nutConfig.shutdown_on_battery_seconds.toString();
		}
		try { upsStatus = await client.call<UpsStatus>('system.nut.status'); } catch { /* ignore */ }
	}

	async function saveNut() {
		savingNut = true;
		await withToast(
			() => client.call('system.nut.config.update', {
				driver: nutDriver, port: nutPort, ups_name: nutUpsName,
				description: nutDescription || undefined,
				shutdown_on_battery_percent: parseInt(nutShutdownPercent) || undefined,
				shutdown_on_battery_seconds: parseInt(nutShutdownSeconds) || undefined,
			}),
			'UPS configuration saved'
		);
		savingNut = false;
		nutConfig = null;
		await loadNut();
	}

	// SSH config
	let sshKeys: string[] = $state([]);
	let sshPasswordAuth = $state(true);
	let sshNewKey = $state('');
	let sshLoaded = $state(false);

	async function loadSsh() {
		if (sshLoaded) return;
		try {
			const result = await client.call<{ password_auth: boolean; keys: string[] }>('system.ssh.status');
			sshKeys = result.keys;
			sshPasswordAuth = result.password_auth;
			sshLoaded = true;
		} catch { /* ignore */ }
	}

	async function addSshKey() {
		if (!sshNewKey.trim()) return;
		await withToast(() => client.call('system.ssh.add_key', { key: sshNewKey.trim() }), 'SSH key added');
		sshNewKey = '';
		sshLoaded = false;
		await loadSsh();
	}

	async function removeSshKey(key: string) {
		// Removing an SSH key is destructive AND potentially self-locking
		// (operator's current SSH session may be holding the only other
		// path back into the box). Confirm before yanking it.
		const fingerprint = key.split(/\s+/).slice(0, 2).join(' ');
		if (!await confirm(
			'Remove this SSH key?',
			`The key (${fingerprint}…) will no longer be able to log in. If this is the key you used to SSH into this box, you'll lose your shell access.`,
		)) return;
		await withToast(() => client.call('system.ssh.remove_key', { key }), 'SSH key removed');
		sshLoaded = false;
		await loadSsh();
	}

	async function toggleSshPasswordAuth() {
		const newVal = !sshPasswordAuth;
		await withToast(
			() => client.call('system.ssh.set_password_auth', { enabled: newVal }),
			newVal ? 'Password auth enabled' : 'Password auth disabled'
		);
		sshLoaded = false;
		await loadSsh();
	}

	// Base name config for iSCSI/NVMe-oF
	let baseIqn = $state('');
	let baseNqn = $state('');
	let baseNamesLoaded = $state(false);

	async function loadBaseNames() {
		if (baseNamesLoaded) return;
		try {
			const cfg = await client.call<{ iqn_prefix: string; nqn_prefix: string }>('service.base_names.get');
			baseIqn = cfg.iqn_prefix;
			baseNqn = cfg.nqn_prefix;
			baseNamesLoaded = true;
		} catch { /* ignore */ }
	}

	async function saveBaseIqn() {
		await withToast(() => client.call('service.base_names.update', { iqn_prefix: baseIqn }), 'Base IQN updated');
	}

	async function saveBaseNqn() {
		await withToast(() => client.call('service.base_names.update', { nqn_prefix: baseNqn }), 'Base NQN updated');
	}

	function toggleConfig(name: string) {
		if (configOpen === name) { configOpen = null; return; }
		configOpen = name;
		if (['nfs', 'smb', 'iscsi'].includes(name)) loadTuning();
		if (['iscsi', 'nvmeof'].includes(name)) loadBaseNames();
		if (name === 'nut') loadNut();
		if (name === 'ssh') loadSsh();
		if (name === 'rest-server' && !restConfigLoaded) loadRestConfig();
	}

	// Backup Server config
	let restServerPath = $state('');
	let showRestConfig = $state(false);
	let restConfigLoaded = $state(false);
	/** Decrypted basic-auth creds for the rest-server. Loaded lazily
	 * when the operator clicks Show, never on initial page render —
	 * the engine returns plaintext from this RPC and we don't want
	 * it sitting in the WebUI state any longer than needed. */
	let restCredentials: { username: string; password: string } | null = $state(null);
	let restCredentialsLoading = $state(false);
	let restPasswordRevealed = $state(false);

	/** PEM-encoded internal CA cert that signed this box's TLS leaf —
	 * source boxes pointing backups at this Backup Server paste it
	 * into the profile's "Trusted CA certificate" textarea so their
	 * HTTPS connection to the rest-server validates. Loaded lazily
	 * via `system.tls.local_ca_root` (same RPC the /tls page uses
	 * for its file download). */
	let caCertPem: string | null = $state(null);
	let caCertLoading = $state(false);
	let caCertExpanded = $state(true);
	/** `null` until the section opens; drives whether the CA cert
	 * block defaults to expanded. When ACME succeeded, source boxes
	 * already trust the leaf via the system root store, so the
	 * internal CA cert is irrelevant for backups — collapse the
	 * block but still let the operator open it (some operators run
	 * mixed trust setups, or want it for parallel non-backup use). */
	let acmeIsPublic: boolean | null = $state(null);

	async function loadCaCert() {
		if (caCertPem !== null || caCertLoading) return;
		caCertLoading = true;
		try {
			caCertPem = await client.call<string>('system.tls.local_ca_root');
		} catch (e) {
			error(`Load CA cert: ${e}`);
		}
		caCertLoading = false;
	}

	function downloadCaCert() {
		if (!caCertPem) return;
		const blob = new Blob([caCertPem], { type: 'application/x-pem-file' });
		const url = URL.createObjectURL(blob);
		const a = document.createElement('a');
		a.href = url;
		a.download = 'nasty-local-ca.crt';
		document.body.appendChild(a);
		a.click();
		document.body.removeChild(a);
		URL.revokeObjectURL(url);
	}

	async function loadRestConfig() {
		try {
			const cfg = await client.call<{ path: string }>('service.rest_server.config');
			restServerPath = cfg.path;
			restConfigLoaded = true;
		} catch { /* ignore */ }
		// Best-effort ACME state — only used to decide whether the CA
		// cert block defaults expanded or collapsed. A failure here
		// just means we default to expanded (the safer assumption
		// since the internal-CA case is the only one that actually
		// needs operator action source-side).
		try {
			const acme = await client.call<{ state: string }>('system.acme.status');
			acmeIsPublic = acme.state === 'success';
			caCertExpanded = !acmeIsPublic;
		} catch {
			acmeIsPublic = false;
			caCertExpanded = true;
		}
		if (caCertExpanded) loadCaCert();
	}

	async function saveRestConfig() {
		await withToast(
			() => client.call('service.rest_server.configure', { path: restServerPath }),
			'Backup Server path updated'
		);
		showRestConfig = false;
		await refresh();
	}

	async function loadRestCredentials() {
		restCredentialsLoading = true;
		try {
			restCredentials = await client.call<{ username: string; password: string }>(
				'service.rest_server.credentials',
			);
		} catch { /* leave null; UI shows the error state */ }
		restCredentialsLoading = false;
	}

	async function rotateRestCredentials() {
		if (!await confirm(
			'Rotate Backup Server credentials?',
			'A fresh random password is generated and the rest-server is restarted. EVERY source-side backup profile that points at this Backup Server must be updated with the new URL before its next run — otherwise the run will fail with HTTP 401.',
		)) return;
		const fresh = await withToast(
			() => client.call<{ username: string; password: string }>(
				'service.rest_server.rotate_credentials',
				{},
			),
			'Credentials rotated — update source-side profile URLs',
		);
		if (fresh) {
			restCredentials = fresh;
			restPasswordRevealed = true;
		}
	}

	/** Per-button "Copied!" indicator. Holds the caller-supplied key
	 * (e.g. 'rest-username') for ~1.5s after a successful copy, then
	 * resets to null. A failed copy (clipboard API unavailable on
	 * http:// origins, or denied permission) surfaces an error toast
	 * instead so the operator doesn't paste yesterday's value
	 * thinking the new one took. */
	let copiedKey: string | null = $state(null);
	let copyResetTimer: ReturnType<typeof setTimeout> | null = null;
	async function copyToClipboard(s: string, key: string) {
		if (!navigator.clipboard?.writeText) {
			error('Clipboard not available — select the value and copy manually');
			return;
		}
		try {
			await navigator.clipboard.writeText(s);
		} catch {
			error('Copy failed — select the value and copy manually');
			return;
		}
		copiedKey = key;
		if (copyResetTimer !== null) clearTimeout(copyResetTimer);
		copyResetTimer = setTimeout(() => { copiedKey = null; }, 1500);
	}

	const client = getClient();

	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'protocol') refresh();
	}

	// Page-scoped handles for the post-enable Docker-status poll so onDestroy
	// can cancel them on SPA navigation.
	let dockerEnablePoll: ReturnType<typeof setInterval> | null = null;
	let dockerEnableTimeout: ReturnType<typeof setTimeout> | null = null;
	function stopDockerEnablePoll() {
		if (dockerEnablePoll !== null) { clearInterval(dockerEnablePoll); dockerEnablePoll = null; }
		if (dockerEnableTimeout !== null) { clearTimeout(dockerEnableTimeout); dockerEnableTimeout = null; }
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await refresh();
		loading = false;
		// Deep-link: /services?configure=<name> opens that service's config panel.
		// Used by the SSH "Manage SSH" banner button (and any future banners).
		const target = $page.url.searchParams.get('configure');
		if (target) toggleConfig(target);
	});

	onDestroy(() => {
		client.offEvent(handleEvent);
		stopDockerEnablePoll();
	});

	async function refresh() {
		await withToast(async () => {
			[protocols, dockerStatus] = await Promise.all([
				client.call<ProtocolStatus[]>('service.protocol.list'),
				client.call<AppsStatus>('apps.status').catch(() => null),
			]);
		});
	}

	async function loadFilesystems() {
		try { filesystems = await client.call<Filesystem[]>('fs.list'); } catch { /* ignore */ }
		const mounted = filesystems.filter(f => f.mounted);
		if (mounted.length > 0 && !selectedFs) selectedFs = mounted[0].name;
	}

	async function enableDocker() {
		if (!selectedFs) await loadFilesystems();
		dockerEnabling = true;
		await withToast(
			() => client.call('apps.enable', { filesystem: selectedFs || undefined }),
			'Docker enabled — starting runtime'
		);
		dockerEnabling = false;
		// Poll until running.
		stopDockerEnablePoll();
		dockerEnablePoll = setInterval(async () => {
			dockerStatus = await client.call<AppsStatus>('apps.status').catch(() => null);
			if (dockerStatus?.running) stopDockerEnablePoll();
		}, 3000);
		dockerEnableTimeout = setTimeout(stopDockerEnablePoll, 60_000);
	}

	async function disableDocker() {
		await withToast(() => client.call('apps.disable'), 'Docker disabled');
		await refresh();
	}

	async function toggle(proto: ProtocolStatus) {
		const action = proto.enabled ? 'disable' : 'enable';
		await withToast(
			() => client.call(`service.protocol.${action}`, { name: proto.name }),
			`${proto.display_name} ${proto.enabled ? 'disabled' : 'enabled'}`
		);
		await refresh();
	}

	const sharingProtocols = $derived(protocols.filter(p => !p.system_service));
	const systemServices = $derived(protocols.filter(p => p.system_service));
</script>


{#snippet serviceRow(proto: ProtocolStatus)}
				<tr class="border-b border-border">
					<td class="p-3"><strong>{proto.display_name}</strong></td>
					<td class="p-3">
						<Badge variant={proto.enabled ? 'default' : 'secondary'}>
							{proto.enabled ? 'Enabled' : 'Disabled'}
						</Badge>
					</td>
					<td class="p-3">
						<span class="inline-block h-2 w-2 rounded-full {proto.running ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
						<span class="ml-1 text-xs text-muted-foreground">{proto.running ? 'Running' : 'Stopped'}</span>
					</td>
					<td class="p-3">
						<div class="flex gap-1.5">
							<Button
								variant="secondary"
								size="xs"
								class="w-[65px] justify-center"
								onclick={() => toggle(proto)}
							>
								{proto.enabled ? 'Disable' : 'Enable'}
							</Button>
							{#if ['nfs', 'smb', 'iscsi', 'nvmeof', 'nut', 'ssh', 'rest-server'].includes(proto.name)}
								<Button variant="secondary" size="xs" onclick={() => toggleConfig(proto.name)}>
									{configOpen === proto.name ? 'Close' : 'Configure'}
								</Button>
							{/if}
						</div>
					</td>
				</tr>
				{#if configOpen === proto.name}
					<tr class="border-b border-border bg-muted/20">
						<td colspan="4" class="p-4">
							{#if proto.name === 'nfs' && tuning}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-3 max-w-xl">
									<div>
										<label for="s-nfs-threads" class="mb-1 block text-xs text-muted-foreground">Threads</label>
										<input id="s-nfs-threads" type="number" min="1" bind:value={tNfsThreads} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Kernel nfsd threads (default: 8).</p>
									</div>
									<div>
										<label for="s-nfs-lease" class="mb-1 block text-xs text-muted-foreground">Lease time (s)</label>
										<input id="s-nfs-lease" type="number" min="1" bind:value={tNfsLeaseTime} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div>
										<label for="s-nfs-grace" class="mb-1 block text-xs text-muted-foreground">Grace time (s)</label>
										<input id="s-nfs-grace" type="number" min="1" bind:value={tNfsGraceTime} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
								</div>
								<Button size="sm" class="mt-3" onclick={saveTuning} disabled={savingTuning}>{savingTuning ? 'Applying...' : 'Apply'}</Button>
							{:else if proto.name === 'smb' && tuning}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-3 max-w-xl">
									<div>
										<label for="s-smb-max" class="mb-1 block text-xs text-muted-foreground">Max connections</label>
										<input id="s-smb-max" type="number" min="0" bind:value={tSmbMaxConnections} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">0 = unlimited.</p>
									</div>
									<div>
										<label for="s-smb-dead" class="mb-1 block text-xs text-muted-foreground">Dead time (min)</label>
										<input id="s-smb-dead" type="number" min="0" bind:value={tSmbDeadtime} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div class="sm:col-span-3">
										<label for="s-smb-sock" class="mb-1 block text-xs text-muted-foreground">Socket options</label>
										<input id="s-smb-sock" type="text" bind:value={tSmbSocketOptions} placeholder="SO_RCVBUF=131072 SO_SNDBUF=131072" class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
									</div>
								</div>
								<Button size="sm" class="mt-3" onclick={saveTuning} disabled={savingTuning}>{savingTuning ? 'Applying...' : 'Apply'}</Button>
							{:else if proto.name === 'iscsi' && tuning}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-3 max-w-xl">
									<div>
										<label for="s-iscsi-cmd" class="mb-1 block text-xs text-muted-foreground">Command queue depth</label>
										<input id="s-iscsi-cmd" type="number" min="1" bind:value={tIscsiCmdsnDepth} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div>
										<label for="s-iscsi-timeout" class="mb-1 block text-xs text-muted-foreground">Login timeout (s)</label>
										<input id="s-iscsi-timeout" type="number" min="1" bind:value={tIscsiLoginTimeout} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div class="sm:col-span-3">
										<label for="s-base-iqn" class="mb-1 block text-xs text-muted-foreground">Base IQN</label>
										<input id="s-base-iqn" type="text" bind:value={baseIqn} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Prefix for all iSCSI target IQNs (e.g. iqn.2137-04.storage.nasty).</p>
									</div>
								</div>
								<div class="mt-3 flex gap-2">
									<Button size="sm" onclick={saveTuning} disabled={savingTuning}>{savingTuning ? 'Applying...' : 'Apply Tuning'}</Button>
									<Button size="sm" variant="secondary" onclick={saveBaseIqn}>Save IQN</Button>
								</div>
							{:else if proto.name === 'nvmeof'}
								<div class="max-w-xl">
									<label for="s-base-nqn" class="mb-1 block text-xs text-muted-foreground">Base NQN</label>
									<input id="s-base-nqn" type="text" bind:value={baseNqn} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
									<p class="mt-0.5 text-[0.6rem] text-muted-foreground">Prefix for all NVMe-oF subsystem NQNs (e.g. nqn.2137-04.storage.nasty).</p>
								</div>
								<Button size="sm" class="mt-3" onclick={saveBaseNqn}>Save</Button>
							{:else if proto.name === 'nut'}
								{#if upsStatus?.available}
									<div class="mb-3 grid grid-cols-2 gap-3 sm:grid-cols-4 max-w-xl text-xs">
										<div><span class="text-muted-foreground">Status</span><br/><strong>{upsStatus.status}</strong></div>
										{#if upsStatus.battery_charge != null}<div><span class="text-muted-foreground">Battery</span><br/><strong>{upsStatus.battery_charge.toFixed(0)}%</strong></div>{/if}
										{#if upsStatus.input_voltage != null}<div><span class="text-muted-foreground">Input</span><br/><strong>{upsStatus.input_voltage.toFixed(1)}V</strong></div>{/if}
										{#if upsStatus.ups_model}<div><span class="text-muted-foreground">Model</span><br/><strong>{upsStatus.ups_model}</strong></div>{/if}
									</div>
								{/if}
								<div class="grid grid-cols-1 gap-3 sm:grid-cols-3 max-w-xl">
									<div>
										<label for="s-nut-driver" class="mb-1 block text-xs text-muted-foreground">Driver</label>
										<select id="s-nut-driver" bind:value={nutDriver} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm">
											<option value="usbhid-ups">usbhid-ups (USB HID)</option>
											<option value="blazer_usb">blazer_usb (Megatec USB)</option>
											<option value="nutdrv_qx">nutdrv_qx (Q* USB)</option>
											<option value="snmp-ups">snmp-ups (SNMP)</option>
											<option value="apcsmart">apcsmart (APC serial)</option>
										</select>
									</div>
									<div>
										<label for="s-nut-port" class="mb-1 block text-xs text-muted-foreground">Port</label>
										<input id="s-nut-port" type="text" bind:value={nutPort} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm font-mono" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">"auto" for USB.</p>
									</div>
									<div>
										<label for="s-nut-name" class="mb-1 block text-xs text-muted-foreground">UPS Name</label>
										<input id="s-nut-name" type="text" bind:value={nutUpsName} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div>
										<label for="s-nut-pct" class="mb-1 block text-xs text-muted-foreground">Shutdown at battery (%)</label>
										<input id="s-nut-pct" type="number" min="0" max="100" bind:value={nutShutdownPercent} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
									</div>
									<div>
										<label for="s-nut-secs" class="mb-1 block text-xs text-muted-foreground">On-battery timeout (s)</label>
										<input id="s-nut-secs" type="number" min="0" bind:value={nutShutdownSeconds} class="h-8 w-full rounded-md border border-input bg-background px-2 text-sm" />
										<p class="mt-0.5 text-[0.6rem] text-muted-foreground">0 = disabled.</p>
									</div>
								</div>
								<Button size="sm" class="mt-3" onclick={saveNut} disabled={savingNut}>{savingNut ? 'Saving...' : 'Save'}</Button>
							{:else if proto.name === 'ssh' && sshLoaded}
								<div class="max-w-xl space-y-3">
									<label class="flex items-center gap-2 text-sm cursor-pointer">
										<input type="checkbox" checked={sshPasswordAuth} onchange={toggleSshPasswordAuth} class="rounded border-input" />
										<span>Allow password authentication</span>
									</label>
									{#if sshPasswordAuth}
										<p class="text-xs text-amber-400">Password authentication is enabled. Add an SSH key and disable it for better security.</p>
									{/if}

									<div>
										<p class="mb-2 text-xs font-semibold text-muted-foreground">Authorized Keys ({sshKeys.length})</p>
										{#if sshKeys.length > 0}
											<div class="space-y-1 mb-3">
												{#each sshKeys as key}
													<div class="flex items-center gap-2 rounded bg-muted/30 px-2 py-1">
														<code class="flex-1 text-[0.65rem] truncate">{key}</code>
														<button onclick={() => removeSshKey(key)} class="text-xs text-destructive hover:text-destructive/80 shrink-0">Remove</button>
													</div>
												{/each}
											</div>
										{:else}
											<p class="mb-3 text-xs text-muted-foreground">No SSH keys configured.</p>
										{/if}
										<div class="flex gap-2">
											<input type="text" bind:value={sshNewKey} placeholder="ssh-ed25519 AAAA... user@host"
												class="flex-1 h-8 rounded-md border border-input bg-background px-2 text-xs font-mono" />
											<Button size="xs" onclick={addSshKey} disabled={!sshNewKey.trim()}>Add Key</Button>
										</div>
									</div>
								</div>
							{:else if proto.name === 'rest-server'}
								<div class="space-y-4">
									<div class="max-w-md">
										<label for="rest-path" class="text-xs text-muted-foreground">Storage path</label>
										<div class="mt-1 flex items-stretch gap-2">
											<input id="rest-path" type="text" bind:value={restServerPath} placeholder="/fs/first/backups"
												class="flex-1 rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm" />
											<Button size="sm" onclick={saveRestConfig}>Save</Button>
										</div>
										<p class="mt-1 text-xs text-muted-foreground">Subvolume created automatically if path is under /fs/.</p>
									</div>

									<div class="rounded-md border border-border p-3 max-w-md">
										<div class="flex items-center justify-between gap-3">
											<div>
												<p class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">Receiver credentials</p>
												<p class="mt-1 text-xs text-muted-foreground">Required by source-side NASties pointing backups at this Backup Server. Paste into the backup profile URL: <code class="font-mono">https://&lt;user&gt;:&lt;password&gt;@&lt;host&gt;:8000/&lt;path&gt;</code></p>
											</div>
											<div class="flex gap-2">
												{#if restCredentials}
													<Button size="xs" variant="secondary" onclick={() => { restPasswordRevealed = !restPasswordRevealed; }}>
														{restPasswordRevealed ? 'Hide' : 'Show'}
													</Button>
												{:else}
													<Button size="xs" variant="secondary" onclick={loadRestCredentials} disabled={restCredentialsLoading}>
														{restCredentialsLoading ? 'Loading…' : 'Show'}
													</Button>
												{/if}
												<Button size="xs" variant="destructive" onclick={rotateRestCredentials}>Rotate</Button>
											</div>
										</div>
										{#if restCredentials}
											<div class="mt-3 space-y-2 text-xs">
												<div class="flex items-center gap-2">
													<span class="w-24 text-muted-foreground">Username</span>
													<code class="flex-1 rounded bg-muted/40 px-2 py-1 font-mono">{restCredentials.username}</code>
													<Button size="xs" variant="ghost" onclick={() => copyToClipboard(restCredentials!.username, 'rest-username')}>
														{copiedKey === 'rest-username' ? 'Copied!' : 'Copy'}
													</Button>
												</div>
												<div class="flex items-center gap-2">
													<span class="w-24 text-muted-foreground">Password</span>
													<code class="flex-1 rounded bg-muted/40 px-2 py-1 font-mono">
														{restPasswordRevealed ? restCredentials.password : '•'.repeat(restCredentials.password.length)}
													</code>
													<Button size="xs" variant="ghost" onclick={() => copyToClipboard(restCredentials!.password, 'rest-password')}>
														{copiedKey === 'rest-password' ? 'Copied!' : 'Copy'}
													</Button>
												</div>
											</div>
										{/if}
									</div>

									<div class="rounded-md border border-border p-3 max-w-md">
										<button type="button"
											onclick={() => { caCertExpanded = !caCertExpanded; if (caCertExpanded) loadCaCert(); }}
											class="flex w-full items-start justify-between gap-3 text-left">
											<div>
												<p class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">Trusted CA certificate</p>
												<p class="mt-1 text-xs text-muted-foreground">
													{#if acmeIsPublic}
														Not required — this box's TLS leaf is publicly trusted via ACME. Expand only if a source box explicitly needs to pin this CA.
													{:else}
														Paste into the source-side backup profile's "Trusted CA certificate" field so its HTTPS connection to this Backup Server validates.
													{/if}
												</p>
											</div>
											<span class="mt-0.5 shrink-0 text-xs text-muted-foreground">{caCertExpanded ? '▾' : '▸'}</span>
										</button>
										{#if caCertExpanded}
											<div class="mt-3 space-y-2">
												{#if caCertLoading}
													<p class="text-xs text-muted-foreground">Loading…</p>
												{:else if caCertPem}
													<textarea readonly rows="6" value={caCertPem}
														class="w-full rounded-md border border-input bg-muted/20 px-3 py-2 font-mono text-xs"></textarea>
													<div class="flex gap-2">
														<Button size="xs" variant="ghost" onclick={() => copyToClipboard(caCertPem!, 'ca-cert')}>
															{copiedKey === 'ca-cert' ? 'Copied!' : 'Copy'}
														</Button>
														<Button size="xs" variant="ghost" onclick={downloadCaCert}>Download .crt</Button>
													</div>
												{:else}
													<Button size="xs" variant="secondary" onclick={loadCaCert}>Load certificate</Button>
												{/if}
											</div>
										{/if}
									</div>
								</div>
							{:else}
								<p class="text-xs text-muted-foreground">Loading...</p>
							{/if}
						</td>
					</tr>
				{/if}
{/snippet}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else}
	<table class="w-full max-w-3xl text-sm">
		<thead>
			<tr>
				<th class="w-[180px] border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Service</th>
				<th class="w-[100px] border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
				<th class="w-[100px] border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Running</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			<!-- Sharing Protocols -->
			<tr><td colspan="4" class="pt-4 pb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-widest text-muted-foreground/60">Sharing Protocols</td></tr>
			{#each sharingProtocols as proto}
				{@render serviceRow(proto)}
			{/each}

			<!-- App Runtime -->
			<tr><td colspan="4" class="pt-6 pb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-widest text-muted-foreground/60">App Runtime</td></tr>
			<tr class="border-b border-border">
				<td class="p-3"><strong>Docker</strong></td>
				<td class="p-3">
					<Badge variant={dockerStatus?.enabled ? 'default' : 'secondary'}>
						{dockerStatus?.enabled ? 'Enabled' : 'Disabled'}
					</Badge>
				</td>
				<td class="p-3">
					<span class="inline-block h-2 w-2 rounded-full {dockerStatus?.running ? 'bg-green-400' : 'bg-muted-foreground'}"></span>
					<span class="ml-1 text-xs text-muted-foreground">{dockerStatus?.running ? 'Running' : 'Stopped'}</span>
				</td>
				<td class="p-3">
					<div class="flex gap-1.5">
					{#if dockerStatus?.enabled}
							<Button variant="secondary" size="xs" class="w-[65px] justify-center" onclick={disableDocker}>Disable</Button>
					{:else}
							<Button variant="secondary" size="xs" class="w-[65px] justify-center" onclick={async () => { if (!selectedFs) await loadFilesystems(); enableDocker(); }} disabled={dockerEnabling}>
								{dockerEnabling ? 'Enabling...' : 'Enable'}
							</Button>
					{/if}
						<Button variant="secondary" size="xs" onclick={() => { configOpen = configOpen === 'docker' ? null : 'docker'; if (configOpen === 'docker' && !dockerStatus?.enabled) loadFilesystems(); }}>
							{configOpen === 'docker' ? 'Close' : 'Configure'}
						</Button>
					</div>
				</td>
			</tr>
			{#if configOpen === 'docker'}
				<tr class="border-b border-border bg-muted/20">
					<td colspan="4" class="p-4">
						{#if dockerStatus?.enabled}
							<div class="flex flex-wrap gap-4 text-xs">
								{#if dockerStatus.storage_path}
									<span class="text-muted-foreground">Storage: <code class="font-mono">{dockerStatus.storage_path}</code></span>
								{/if}
								{#if dockerStatus.docker_version}
									<span class="text-muted-foreground">Version: {dockerStatus.docker_version}</span>
								{/if}
								{#if dockerStatus.memory_bytes}
									<span class="text-muted-foreground">Memory: {(dockerStatus.memory_bytes / 1048576).toFixed(0)} MiB</span>
								{/if}
							</div>
							<div class="mt-3">
								<Button size="sm" variant="secondary" onclick={async () => {
									await withToast(() => client.call('apps.prune'), 'Cleanup complete');
								}}>Cleanup Unused Images</Button>
							</div>
						{:else}
							<div class="flex items-center gap-2">
								<label for="docker-fs" class="text-xs text-muted-foreground">Storage filesystem:</label>
								<select id="docker-fs" bind:value={selectedFs} class="h-7 rounded-md border border-input bg-transparent px-2 text-xs">
									{#each filesystems.filter(f => f.mounted) as fs}
										<option value={fs.name}>{fs.name}</option>
									{/each}
								</select>
								<p class="text-xs text-muted-foreground">Docker data will be stored on this filesystem.</p>
							</div>
						{/if}
					</td>
				</tr>
			{/if}
			<!-- System Services -->
			<tr><td colspan="4" class="pt-6 pb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-widest text-muted-foreground/60">System Services</td></tr>
			{#each systemServices as proto}
				{@render serviceRow(proto)}
			{/each}
		</tbody>
	</table>
{/if}
