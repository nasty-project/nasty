<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { AppsStatus, App, AppIngress, AppConfig, ImageInspectResult, AppContainer, MappedPort, PruneResult } from '$lib/types';
	import { formatBytes } from '$lib/format';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import CodeEditor from '$lib/components/CodeEditor.svelte';
	import { CircleCheck, Circle } from '@lucide/svelte';
	import type { Filesystem, FsDependents } from '$lib/types';
	import { unlockFs } from '$lib/unlock-fs.svelte';
	import { Lock } from '@lucide/svelte';

	// Deploy stream state
	let deployLog: string[] = $state([]);
	let deploying = $state(false);
	let deployDone = $state(false);
	let deployError = $state('');

	function streamDeploy(params: Record<string, unknown>): Promise<boolean> {
		return new Promise((resolve) => {
			deploying = true;
			deployDone = false;
			deployError = '';
			deployLog = [];

			const wsProto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
			const ws = new WebSocket(`${wsProto}//${window.location.host}/ws/apps/deploy`);

			ws.onopen = () => {
				// Cookie auth — the WS upgrade already carried the session.
				ws.send(JSON.stringify(params));
			};

			ws.onmessage = (event) => {
				try {
					const msg = JSON.parse(event.data);
					if (msg.type === 'log') {
						// Deduplicate progress lines (Extracting, Downloading, Waiting, Pulling fs layer, etc.)
						// by replacing the previous line if it has the same hash prefix + action
						const prev = deployLog.length > 0 ? deployLog[deployLog.length - 1] : '';
						const progressRe = /^[0-9a-f]{12}\s+(Extracting|Downloading|Waiting|Verifying Checksum|Download complete)\b/;
						const prevMatch = prev.match(progressRe);
						const currMatch = msg.data.match(progressRe);
						if (prevMatch && currMatch && prev.slice(0, 12) === msg.data.slice(0, 12) && prevMatch[1] === currMatch[1]) {
							deployLog = [...deployLog.slice(0, -1), msg.data];
						} else {
							deployLog = [...deployLog, msg.data];
						}
					} else if (msg.type === 'error') {
						deployError = msg.data;
						deployLog = [...deployLog, `ERROR: ${msg.data}`];
						deploying = false;
						resolve(false);
						ws.close();
					} else if (msg.type === 'done') {
						deployDone = true;
						deploying = false;
						resolve(true);
						ws.close();
					}
				} catch { /* ignore */ }
			};

			ws.onerror = () => {
				deployError = 'WebSocket connection failed';
				deploying = false;
				resolve(false);
			};

			ws.onclose = () => {
				if (!deployDone && !deployError) {
					deployError = 'Connection closed unexpectedly';
					deploying = false;
					resolve(false);
				}
			};
		});
	}

	function closeDeployLog() {
		deployLog = [];
		deployDone = false;
		deployError = '';
	}

	$effect(() => {
		if (deployLog.length > 0) {
			// Auto-scroll deploy output to bottom
			requestAnimationFrame(() => {
				const el = document.getElementById('deploy-output');
				if (el) el.scrollTop = el.scrollHeight;
			});
		}
	});

	let status: AppsStatus | null = $state(null);
	let apps: App[] = $state([]);
	// Map of app-name → locked-FS-name. Populated from
	// `fs.locked_dependents` so each app row can show a "🔒 on tank"
	// badge that opens the global unlock dialog. Stays empty when
	// nothing's locked, so non-encrypted setups pay zero overhead.
	let lockedFsByApp = $state(new Map<string, string>());
	let loading = $state(true);
	let enabling = $state(false);
	// Inline enable prompt shown when the user clicks Install App while the
	// runtime is disabled — keeps the empty-state apps page looking like the
	// running page so the user doesn't see two unrelated layouts.
	let showEnablePrompt = $state(false);
	let showInstall = $state(false);
	let editingApp: string | null = $state(null);
	let logsApp: string | null = $state(null);
	let logsContent = $state('');
	let inspectData: string | null = $state(null);
	let inspectName: string | null = $state(null);
	let installMode: 'simple' | 'compose' = $state('simple');
	let showRuntimeDetails = $state(false);
	let showPasteDocker = $state(false);
	let pasteDockerCmd = $state('');

	/** Expand common shell variables in paths to NASty-appropriate defaults. */
	function expandShellVars(s: string): string {
		return s
			.replace(/\$HOME|\$\{HOME\}|~/g, '/root')
			.replace(/\$PWD|\$\{PWD\}/g, '/var/lib/nasty/apps')
			.replace(/\$[A-Z_]+/g, ''); // strip any remaining unresolved vars
	}

	function highlightJson(json: string): string {
		// Escape first so attacker-controlled values inside the JSON (container
		// names, env vars, labels) cannot reconstruct script tags. Spans below
		// are added after escaping, so they remain live HTML.
		const escaped = json
			.replace(/&/g, '&amp;')
			.replace(/</g, '&lt;')
			.replace(/>/g, '&gt;')
			.replace(/"/g, '&quot;');
		return escaped
			.replace(/(&quot;(?:\\.|[^&\\]|&(?!quot;))*?&quot;)\s*:/g, '<span class="text-purple-400">$1</span>:')
			.replace(/:\s*(&quot;(?:\\.|[^&\\]|&(?!quot;))*?&quot;)/g, ': <span class="text-green-400">$1</span>')
			.replace(/:\s*(true|false)/g, ': <span class="text-amber-400">$1</span>')
			.replace(/:\s*(\d+\.?\d*)/g, ': <span class="text-blue-400">$1</span>')
			.replace(/:\s*(null)/g, ': <span class="text-red-400">$1</span>');
	}

	function parseDockerRun(cmd: string) {
		// Normalize: join backslash-continuations, trim
		const line = cmd.replace(/\\\s*\n/g, ' ').replace(/^\s*(sudo\s+)?docker\s+run\s*/, '').trim();
		const tokens: string[] = [];
		let current = '';
		let inQuote = '';
		for (const ch of line) {
			if (inQuote) {
				if (ch === inQuote) { inQuote = ''; } else { current += ch; }
			} else if (ch === "'" || ch === '"') {
				inQuote = ch;
			} else if (ch === ' ' || ch === '\t') {
				if (current) { tokens.push(current); current = ''; }
			} else {
				current += ch;
			}
		}
		if (current) tokens.push(current);

		let name = '';
		let image = '';
		const ports: typeof newPorts = [];
		const envs: typeof newEnvs = [];
		const volumes: typeof newVolumes = [];

		let i = 0;
		while (i < tokens.length) {
			const t = tokens[i];
			if (t === '--name' && i + 1 < tokens.length) {
				name = tokens[++i];
			} else if ((t === '-p' || t === '--publish') && i + 1 < tokens.length) {
				const parts = tokens[++i].split(':');
				const host = parts.length >= 2 ? parts[0] : '';
				const container = parts.length >= 2 ? parts[1] : parts[0];
				const proto = parts.length >= 3 ? parts[2]?.toUpperCase() : 'TCP';
				ports.push({ name: `port-${ports.length}`, container_port: parseInt(container) || 80, host_port: host, protocol: proto || 'TCP' });
			} else if ((t === '-e' || t === '--env') && i + 1 < tokens.length) {
				const val = tokens[++i];
				const eq = val.indexOf('=');
				if (eq > 0) {
					envs.push({ name: val.slice(0, eq), value: val.slice(eq + 1) });
				}
			} else if ((t === '-v' || t === '--volume') && i + 1 < tokens.length) {
				const parts = tokens[++i].split(':');
				if (parts.length >= 2) {
					volumes.push({ name: `vol-${volumes.length}`, host_path: expandShellVars(parts[0]), mount_path: parts[1] });
				}
			} else if (t === '-d' || t === '--detach' || t === '--restart' || t === '--restart=always' || t.startsWith('--restart=')) {
				// skip flags we handle implicitly
			} else if (t.startsWith('-')) {
				// Unknown flag — skip its value if it looks like a flag with arg
				if (!t.includes('=') && i + 1 < tokens.length && !tokens[i + 1].startsWith('-')) { i++; }
			} else {
				// Positional: image name (last non-flag token)
				image = t;
			}
			i++;
		}

		// Apply to form
		if (name) newName = name.toLowerCase();
		if (image) newImage = image;
		if (ports.length > 0) newPorts = ports;
		if (envs.length > 0) newEnvs = envs;
		if (volumes.length > 0) newVolumes = volumes;

		showPasteDocker = false;
		pasteDockerCmd = '';
	}

	// Setup wizard state
	let filesystems: Filesystem[] = $state([]);
	let selectedFs = $state('');

	// Compose mode state
	let composeName = $state('');
	let composeContent = $state('');
	let showCompose = $state(false);
	let editingCompose: string | null = $state(null);

	// Install form
	let newName = $state('');
	let newImage = $state('');
	let newPorts = $state<{ name: string; container_port: number; host_port: string; protocol: string }[]>([]);
	let newEnvs = $state<{ name: string; value: string }[]>([]);
	let newVolumes = $state<{ name: string; mount_path: string; host_path: string }[]>([]);
	let newCpuLimit = $state('');
	let newMemoryLimit = $state('');
	/** Opt out of the strict bind-mount sandbox for simple apps. Persisted per-app. */
	let newAllowUnsafe = $state(false);
	/** Same flag, separate state for the compose dialog. */
	let composeAllowUnsafe = $state(false);
	let inspecting = $state(false);
	let lastInspectedImage = '';
	/** Inline status from the last apps.inspect_image call so the user sees
	 *  why ports weren't auto-detected (image unreachable, no EXPOSE, etc.). */
	let inspectMsg: string | null = $state(null);

	// Port conflict state
	let portConflicts = $state<{ port: number; used_by: string }[]>([]);
	let composePortErrorLines = $state<number[]>([]);
	let composePortLineMap = $state<Map<number, number>>(new Map()); // port → line number
	let checkingPorts = $state(false);
	let portCheckTimer: ReturnType<typeof setTimeout> | null = null;

	// Device existence state — populated for compose mode only (the simple
	// installer doesn't surface a `devices:` field).
	let deviceMissing = $state<{ path: string; parent_exists: boolean }[]>([]);
	let composeDeviceErrorLines = $state<number[]>([]);
	let composeDeviceLineMap = $state<Map<string, number>>(new Map()); // path → line number

	// Reverse-proxy ingress picker — populated from TCP ports parsed
	// out of the compose YAML. Auto-defaults to the first TCP port,
	// follows user-picks until the port disappears from the compose,
	// at which point it falls back to the new first-TCP. UDP ports
	// are excluded — nginx's proxy_pass is TCP-only.
	let composeTcpPorts = $state<{ host_port: number; container_port: number; line: number }[]>([]);
	let newIngressPort = $state<number | null>(null);
	$effect(() => {
		if (composeTcpPorts.length === 0) {
			newIngressPort = null;
		} else if (newIngressPort == null || !composeTcpPorts.some(p => p.host_port === newIngressPort)) {
			newIngressPort = composeTcpPorts[0].host_port;
		}
	});

	// Volume-permission warnings — bind-mount sources whose owner doesn't
	// match the service's `user:` field (or PUID/PGID).
	type VolumeMismatch = {
		service: string;
		host_path: string;
		mount_path: string;
		expected_uid: number;
		expected_gid: number | null;
		current_uid: number | null;
		current_gid: number | null;
		exists: boolean;
		line: number | null;
	};
	let volumeMismatches = $state<VolumeMismatch[]>([]);
	let composeVolumeErrorLines = $derived(
		volumeMismatches
			.filter(m => m.exists && m.line != null)
			.map(m => m.line as number)
	);
	let fixingVolume = $state<string | null>(null); // host_path currently being chowned

	// Editor underlines port-conflict, missing-device, and existing-but-wrong-owner lines.
	const composeErrorLines = $derived([
		...composePortErrorLines,
		...composeDeviceErrorLines,
		...composeVolumeErrorLines,
	]);

	const client = getClient();
	let startupPoll: ReturnType<typeof setInterval> | null = null;
	const APP_NAME_RE = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$/;

	function isValidAppName(name: string): boolean {
		return name.length > 0 && name.length <= 53 && APP_NAME_RE.test(name);
	}

	async function inspectImage() {
		const image = newImage.trim();
		if (!image || image === lastInspectedImage) return;
		lastInspectedImage = image;
		inspecting = true;
		inspectMsg = null;
		try {
			const result = await client.call<ImageInspectResult>('apps.inspect_image', { image });
			if (result.ports.length > 0) {
				newPorts = result.ports.map(p => ({
					name: p.name,
					container_port: p.container_port,
					host_port: '',
					protocol: p.protocol,
				}));
			} else {
				// Image declares no EXPOSE — let the user know they need to
				// set the internal port manually below.
				inspectMsg = 'Image declares no exposed ports — set the internal port manually.';
			}
		} catch (e) {
			// Registry unreachable / image not found / private without auth.
			// Surface inline so the user knows to fall back to manual entry.
			const msg = e instanceof Error ? e.message : typeof e === 'object' && e !== null && 'message' in e ? String((e as { message: unknown }).message) : String(e);
			inspectMsg = `Could not inspect image (${msg}) — set ports manually.`;
		}
		inspecting = false;
		checkPortConflicts();
	}

	function checkPortConflicts(excludeApp?: string) {
		if (portCheckTimer) clearTimeout(portCheckTimer);
		portCheckTimer = setTimeout(async () => {
			const ports = newPorts
				.map(p => p.host_port ? parseInt(p.host_port) : p.container_port)
				.filter(p => p > 0);
			if (ports.length === 0) {
				portConflicts = [];
				return;
			}
			checkingPorts = true;
			try {
				portConflicts = await client.call<{ port: number; used_by: string }[]>(
					'apps.check_ports',
					{ ports, exclude_app: excludeApp ?? null }
				);
			} catch {
				portConflicts = [];
			}
			checkingPorts = false;
		}, 300);
	}

	function checkComposeConflicts() {
		checkComposePortConflicts();
		checkComposeDeviceConflicts();
		checkComposeVolumePerms();
	}

	function checkComposeVolumePerms() {
		// Server parses the YAML and stat()s each bind source. We just
		// hand it the full text — keeps client-side parsing minimal and
		// avoids drifting from the deploy-time validator.
		client.call<VolumeMismatch[]>('apps.check_volumes', { compose: composeContent })
			.then(r => { volumeMismatches = r; })
			.catch(() => { volumeMismatches = []; });
	}

	async function fixVolume(host_path: string, uid: number, gid: number | null, recursive: boolean) {
		const effectiveGid = gid ?? uid; // mirror docker's "user: 1000" → gid=1000 default
		fixingVolume = host_path;
		try {
			await withToast(
				() => client.call('apps.fix_volume_perms', {
					host_path,
					uid,
					gid: effectiveGid,
					recursive,
				}),
				`Chowned ${host_path} to ${uid}:${effectiveGid}${recursive ? ' (recursive)' : ''}`
			);
			// Re-check so the row disappears from the warning list.
			checkComposeVolumePerms();
		} finally {
			fixingVolume = null;
		}
	}

	function checkComposePortConflicts() {
		// Parse host ports from compose YAML (best-effort), tracking line
		// numbers + protocol. Compose accepts `8096:8096`, `8096:8096/tcp`,
		// or `8096:8096/udp`. Missing protocol defaults to TCP per the
		// docker spec.
		const portLines: { port: number; container_port: number; proto: string; line: number }[] = [];
		const lines = composeContent.split('\n');
		for (let i = 0; i < lines.length; i++) {
			const m = lines[i].match(/^\s*-\s*"?(\d+):(\d+)(?:\/(tcp|udp))?/i);
			if (m) {
				portLines.push({
					port: parseInt(m[1]),
					container_port: parseInt(m[2]),
					proto: (m[3] ?? 'tcp').toLowerCase(),
					line: i + 1,
				});
			}
		}
		// Refresh the ingress picker's source list — TCP only.
		composeTcpPorts = portLines
			.filter(p => p.proto === 'tcp')
			.map(p => ({ host_port: p.port, container_port: p.container_port, line: p.line }));

		const ports = portLines.map(p => p.port);
		if (ports.length === 0) {
			portConflicts = [];
			composePortErrorLines = [];
			composePortLineMap = new Map();
			return;
		}
		checkingPorts = true;
		client.call<{ port: number; used_by: string }[]>(
			'apps.check_ports',
			{ ports, exclude_app: editingCompose ?? null }
		).then(r => {
			portConflicts = r;
			const conflictPorts = new Set(r.map(c => c.port));
			composePortErrorLines = portLines.filter(p => conflictPorts.has(p.port)).map(p => p.line);
			composePortLineMap = new Map(portLines.map(p => [p.port, p.line]));
		}).catch(() => { portConflicts = []; composePortErrorLines = []; composePortLineMap = new Map(); }).finally(() => { checkingPorts = false; });
	}

	/** Parse `devices:` block entries from compose YAML, tracking the host
	 * path of each entry (everything before the first colon) and its
	 * source-line number. We track the section by indent rather than
	 * regex-on-every-line so we don't mis-classify volumes or env vars
	 * that happen to start with a dash. */
	function parseComposeDeviceLines(content: string): { path: string; line: number }[] {
		const out: { path: string; line: number }[] = [];
		const lines = content.split('\n');
		let inDevices = false;
		let devicesIndent = -1;
		for (let i = 0; i < lines.length; i++) {
			const line = lines[i];
			const indentMatch = line.match(/^(\s*)/);
			const indent = indentMatch ? indentMatch[1].length : 0;
			const trimmed = line.trim();
			if (!trimmed || trimmed.startsWith('#')) continue;
			if (inDevices && indent <= devicesIndent && !trimmed.startsWith('-')) {
				inDevices = false;
			}
			if (trimmed.match(/^devices:\s*(#.*)?$/)) {
				inDevices = true;
				devicesIndent = indent;
				continue;
			}
			if (inDevices) {
				// `- /dev/foo`, `- /dev/foo:/dev/foo`, `- "/dev/foo:/dev/foo:rwm"`
				const m = line.match(/^\s*-\s*"?([^":\s]+)/);
				if (m) out.push({ path: m[1], line: i + 1 });
			}
		}
		return out;
	}

	function checkComposeDeviceConflicts() {
		const deviceLines = parseComposeDeviceLines(composeContent);
		if (deviceLines.length === 0) {
			deviceMissing = [];
			composeDeviceErrorLines = [];
			composeDeviceLineMap = new Map();
			return;
		}
		const paths = deviceLines.map(d => d.path);
		client.call<{ path: string; parent_exists: boolean }[]>(
			'apps.check_devices',
			{ paths }
		).then(r => {
			deviceMissing = r;
			const missingSet = new Set(r.map(m => m.path));
			composeDeviceErrorLines = deviceLines.filter(d => missingSet.has(d.path)).map(d => d.line);
			composeDeviceLineMap = new Map(deviceLines.map(d => [d.path, d.line]));
		}).catch(() => { deviceMissing = []; composeDeviceErrorLines = []; composeDeviceLineMap = new Map(); });
	}

	onMount(async () => {
		await Promise.all([refresh(), loadFilesystems()]);
		loading = false;
		if (status?.enabled && !status?.running) startStartupPolling();
	});

	onDestroy(() => {
		stopStartupPolling();
	});

	function startStartupPolling() {
		stopStartupPolling();
		startupPoll = setInterval(async () => {
			await refresh();
			if (status?.running) stopStartupPolling();
		}, 5000);
	}

	function stopStartupPolling() {
		if (startupPoll) {
			clearInterval(startupPoll);
			startupPoll = null;
		}
	}

	async function loadFilesystems() {
		try {
			const all = await client.call<Filesystem[]>('fs.list');
			filesystems = all.filter(f => f.mounted);
			if (filesystems.length > 0 && !selectedFs) {
				selectedFs = filesystems[0].name;
			}
		} catch { /* ignore */ }
	}

	async function refresh() {
		try {
			status = await client.call<AppsStatus>('apps.status');
			apps = await client.call<App[]>('apps.list');
			if (status.enabled && status.running) {
				await loadIngresses();
			} else {
				ingresses = [];
			}
		} catch { /* ignore */ }
		await loadLockedFsByApp();
	}

	/** Build the {appName: lockedFsName} map for the per-row badge.
	 * Best-effort — failure leaves the map empty (no badges, no error
	 * toast). Runs alongside every refresh so an unlock from this
	 * page or another tab clears the badge promptly. */
	async function loadLockedFsByApp() {
		try {
			const locked = await client.call<FsDependents[]>('fs.locked_dependents');
			const next = new Map<string, string>();
			for (const fs of locked) {
				for (const appName of fs.apps) next.set(appName, fs.filesystem);
			}
			lockedFsByApp = next;
		} catch {
			lockedFsByApp = new Map();
		}
	}

	async function unlockBlockingFs(fsName: string) {
		// Imperative dialog mounted in root layout. Truthy resolution
		// = unlock RPC succeeded; refresh to clear the badge. The app
		// itself stays stopped — per #86 PR-B's decision, no
		// auto-restart on unlock.
		if (await unlockFs(fsName)) {
			await refresh();
		}
	}

	async function enableApps() {
		enabling = true;
		await withToast(
			() => client.call('apps.enable', { filesystem: selectedFs || undefined }),
			'Apps runtime enabled — starting Docker'
		);
		enabling = false;
		await refresh();
		if (status?.enabled && !status?.running) startStartupPolling();
	}

	function addPort() {
		newPorts = [...newPorts, { name: newPorts.length === 0 ? 'http' : `port-${newPorts.length}`, container_port: 80, host_port: '', protocol: 'TCP' }];
	}

	function removePort(i: number) {
		newPorts = newPorts.filter((_, idx) => idx !== i);
	}

	function addEnv() {
		newEnvs = [...newEnvs, { name: '', value: '' }];
	}

	function removeEnv(i: number) {
		newEnvs = newEnvs.filter((_, idx) => idx !== i);
	}

	function addVolume() {
		newVolumes = [...newVolumes, { name: `data${newVolumes.length}`, mount_path: '', host_path: '' }];
	}

	function removeVolume(i: number) {
		newVolumes = newVolumes.filter((_, idx) => idx !== i);
	}

	/** Wait until Docker is actually ready before submitting. */
	async function waitForDocker(): Promise<boolean> {
		if (status?.running) return true;
		for (let i = 0; i < 30; i++) {
			await new Promise(r => setTimeout(r, 2000));
			await refresh();
			if (status?.running) return true;
		}
		await withToast(async () => { throw new Error('Docker failed to start in time'); }, '');
		return false;
	}

	async function install() {
		if (!newName || !newImage) return;
		if (!await waitForDocker()) return;
		const appName = newName.toLowerCase();
		if (!isValidAppName(appName)) {
			await withToast(async () => { throw new Error('Invalid app name: use lowercase letters, numbers, hyphens, and dots (max 53 chars)'); }, '');
			return;
		}
		const params: Record<string, unknown> = {
			name: appName,
			image: newImage,
		};
		if (newPorts.length > 0) {
			params.ports = newPorts.map(p => ({
				name: p.name,
				container_port: p.container_port,
				host_port: p.host_port ? parseInt(p.host_port) : undefined,
				protocol: p.protocol,
			}));
		}
		if (newEnvs.length > 0) {
			params.env = newEnvs.filter(e => e.name);
		}
		if (newVolumes.length > 0) {
			params.volumes = newVolumes.filter(v => v.name && v.mount_path).map(v => ({
				name: v.name,
				mount_path: v.mount_path,
				host_path: v.host_path || '',
			}));
		}
		if (newCpuLimit) params.cpu_limit = newCpuLimit;
		if (newMemoryLimit) params.memory_limit = newMemoryLimit;

		const ok = await streamDeploy({
			kind: 'simple',
			name: appName,
			image: newImage,
			install_params: params,
			allow_unsafe: newAllowUnsafe,
		});
		if (ok) {
			showInstall = false;
			resetForm();
		}
		await refresh();
	}

	async function editApp(name: string) {
		const config = await withToast(
			() => client.call<AppConfig>('apps.config', { name }),
			''
		);
		if (!config) return;
		editingApp = name;
		newName = config.name;
		newImage = config.image;
		newPorts = config.ports.map(p => ({
			name: p.name,
			container_port: p.container_port,
			host_port: p.host_port?.toString() ?? '',
			protocol: p.protocol,
		}));
		newEnvs = config.env.map(e => ({ name: e.name, value: e.value }));
		newVolumes = config.volumes.map(v => ({ name: v.name, mount_path: v.mount_path, host_path: v.host_path }));
		newCpuLimit = config.cpu_limit ?? '';
		newMemoryLimit = config.memory_limit ?? '';
		newAllowUnsafe = config.allow_unsafe ?? false;
		installMode = 'simple';
		showInstall = true;
	}

	async function updateApp() {
		if (!editingApp || !newImage) return;
		const params: Record<string, unknown> = {
			name: editingApp,
			image: newImage,
		};
		if (newPorts.length > 0) {
			params.ports = newPorts.map(p => ({
				name: p.name,
				container_port: p.container_port,
				host_port: p.host_port ? parseInt(p.host_port) : undefined,
				protocol: p.protocol,
			}));
		}
		if (newEnvs.length > 0) {
			params.env = newEnvs.filter(e => e.name);
		}
		if (newVolumes.length > 0) {
			params.volumes = newVolumes.filter(v => v.name && v.mount_path).map(v => ({
				name: v.name,
				mount_path: v.mount_path,
				host_path: v.host_path || '',
			}));
		}
		if (newCpuLimit) params.cpu_limit = newCpuLimit;
		if (newMemoryLimit) params.memory_limit = newMemoryLimit;
		params.allow_unsafe = newAllowUnsafe;

		const result = await withToast(
			() => client.call('apps.update', params, 300_000),
			'App updated'
		);
		if (result !== undefined) {
			showInstall = false;
			editingApp = null;
			resetForm();
		}
		await refresh();
	}

	function resetForm() {
		newName = ''; newImage = ''; newPorts = []; newEnvs = []; newVolumes = [];
		newCpuLimit = ''; newMemoryLimit = '';
		newAllowUnsafe = false;
		lastInspectedImage = '';
	}

	function cancelEdit() {
		showInstall = false;
		editingApp = null;
		portConflicts = [];
		resetForm();
	}

	async function removeApp(name: string) {
		if (!await confirm(`Remove app "${name}"?`, 'The app and its containers will be deleted. Persistent data on the filesystem is preserved.')) return;
		await withToast(
			() => client.call('apps.remove', { name }),
			'App removed'
		);
		await refresh();
	}

	async function stopApp(name: string) {
		await withToast(() => client.call('apps.stop', { name }), 'App stopped');
		await refresh();
	}

	async function startApp(name: string) {
		await withToast(() => client.call('apps.start', { name }), 'App started');
		await refresh();
	}

	async function restartApp(name: string) {
		await withToast(() => client.call('apps.restart', { name }), 'App restarted');
		await refresh();
	}

	async function pullApp(name: string) {
		await streamDeploy({ kind: 'pull', name });
		await refresh();
	}

	async function pruneDocker() {
		const result = await withToast(
			() => client.call<PruneResult>('apps.prune'),
			'Cleanup complete'
		);
		if (result) {
			await withToast(async () => {
				const msg = `Removed ${result.images_removed} images, reclaimed ${formatBytes(result.space_reclaimed_bytes)}`;
				return msg;
			}, '');
		}
		await refresh();
	}

	async function openShell(name: string) {
		const cmd = await client.call<string>('apps.exec_command', { name });
		// Navigate to terminal with pre-filled command
		goto(`/terminal?cmd=${encodeURIComponent(cmd)}`);
	}

	let expanded: Record<string, boolean> = $state({});

	async function inspectApp(name: string) {
		inspectName = name;
		inspectData = 'Loading...';
		try {
			const result = await client.call<unknown>('apps.inspect', { name });
			inspectData = JSON.stringify(result, null, 2);
		} catch (e) {
			inspectData = `Failed to inspect: ${e}`;
		}
	}

	async function showLogs(name: string, kind: string) {
		logsApp = name;
		logsContent = 'Loading...';
		try {
			if (kind === 'container') {
				logsContent = await client.call<string>('apps.container.logs', { container_id: name, tail: 200 });
			} else {
				const method = kind === 'compose' ? 'apps.compose.logs' : 'apps.logs';
				logsContent = await client.call<string>(method, { name, tail: 200 });
			}
		} catch (e) {
			logsContent = `Failed to load logs: ${e}`;
		}
	}

	// Compose functions
	async function installCompose() {
		if (!composeName || !composeContent.trim()) return;
		if (!await waitForDocker()) return;
		const name = composeName.toLowerCase();
		if (!isValidAppName(name)) {
			await withToast(async () => { throw new Error('Invalid app name'); }, '');
			return;
		}
		const ok = await streamDeploy({
			kind: 'compose',
			name,
			compose_file: composeContent,
			allow_unsafe: composeAllowUnsafe,
			ingress_host_port: newIngressPort,
		});
		if (ok) {
			showCompose = false;
			editingCompose = null;
			composeName = ''; composeContent = '';
			composeAllowUnsafe = false;
		}
		await refresh();
	}

	async function editCompose(name: string) {
		const content = await withToast(
			() => client.call<string>('apps.compose.get', { name }),
			''
		);
		if (content === undefined) return;
		editingCompose = name;
		composeName = name;
		composeContent = content;
		// Pre-fill the unsafe flag from the live apps list — compose apps store
		// it in .nasty-meta.json next to the yaml; the engine surfaces it on App.
		composeAllowUnsafe = apps.find(a => a.name === name)?.unsafe_mode ?? false;
		installMode = 'compose';
		showCompose = true;
	}

	function cancelCompose() {
		showCompose = false;
		editingCompose = null;
		portConflicts = [];
		composePortErrorLines = [];
		composePortLineMap = new Map();
		deviceMissing = [];
		composeDeviceErrorLines = [];
		composeDeviceLineMap = new Map();
		volumeMismatches = [];
		composeTcpPorts = [];
		newIngressPort = null;
		composeName = ''; composeContent = '';
		composeAllowUnsafe = false;
	}

	// Ingress
	let ingresses: AppIngress[] = $state([]);

	async function loadIngresses() {
		try { ingresses = await client.call('apps.ingress.list'); } catch { ingresses = []; }
	}

	function getIngress(appName: string) {
		return ingresses.find(r => r.name === appName);
	}

	/** Pick the port the direct-LAN link and reverse-proxy "Open" button
	 * should target. Prefers the user's chosen ingress port; falls back
	 * to the first TCP port (UDP can't serve HTTP). Returns null when
	 * the app has no TCP-reachable port. */
	function primaryPort(app: App): MappedPort | null {
		const ing = getIngress(app.name);
		if (ing && app.ports) {
			const match = app.ports.find(p => p.host_port === ing.host_port);
			if (match) return match;
		}
		return (app.ports ?? []).find(p => p.protocol?.toLowerCase() === 'tcp') ?? null;
	}

	let switchingIngressFor = $state<string | null>(null);

	/** Re-point an app's ingress at the given host port. Used by the
	 * port chips when the user clicks a non-current TCP port. */
	async function setIngressPort(appName: string, hostPort: number) {
		switchingIngressFor = appName;
		try {
			await withToast(
				() => client.call('apps.ingress.set', { name: appName, host_port: hostPort }),
				`Ingress for ${appName} → :${hostPort}`
			);
			await loadIngresses();
		} finally {
			switchingIngressFor = null;
		}
	}

	let search = $state('');
	let sortDir = $state<'asc' | 'desc'>('asc');

	function toggleSort() {
		sortDir = sortDir === 'asc' ? 'desc' : 'asc';
	}

	const filtered = $derived(
		search.trim()
			? apps.filter(a => a.name.toLowerCase().includes(search.toLowerCase()))
			: apps
	);

	const sorted = $derived.by(() => {
		return [...filtered].sort((a, b) => {
			const cmp = a.name.localeCompare(b.name);
			return sortDir === 'asc' ? cmp : -cmp;
		});
	});
</script>

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if filesystems.length === 0}
	<div class="flex flex-col items-center justify-center py-12 text-center">
		<p class="text-muted-foreground">Apps need a filesystem to store data.</p>
		<Button size="sm" class="mt-2" onclick={() => goto('/filesystems?create')}>Create Filesystem</Button>
	</div>
{:else if status?.enabled && !status?.running}
	<Card class="mb-4">
		<CardContent class="py-8 text-center">
			<div class="mx-auto mb-4 h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary"></div>
			<p class="font-medium">Starting app runtime</p>
			<p class="mt-1 text-sm text-muted-foreground">Docker is starting up. This should only take a few seconds.</p>
		</CardContent>
	</Card>
{:else}
	<!-- Docker status bar -->
	{#if status}
		<div class="mb-4 flex items-center gap-4 rounded-lg border border-border px-4 py-2.5 text-sm">
			<div class="flex items-center gap-2">
				<span class="h-2 w-2 rounded-full {status.running ? 'bg-green-400' : 'bg-red-400'}"></span>
				<span class="text-muted-foreground">Docker {status.docker_version ?? ''}</span>
			</div>
			{#if status.app_count > 0}
				<span class="text-muted-foreground">{status.app_count} app{status.app_count !== 1 ? 's' : ''}</span>
			{/if}
			{#if status.memory_bytes}
				<span class="text-muted-foreground">{formatBytes(status.memory_bytes)} RAM</span>
			{/if}
			{#if status.disk_usage_bytes != null && status.disk_usage_bytes > 0}
				<span class="text-muted-foreground">{formatBytes(status.disk_usage_bytes)} disk</span>
			{/if}
			{#if !status.storage_ok && status.storage_path}
				<span class="text-destructive">Storage missing</span>
			{/if}
			<button onclick={() => showRuntimeDetails = !showRuntimeDetails} class="ml-auto text-xs text-muted-foreground hover:text-foreground">
				{showRuntimeDetails ? 'Hide details' : 'Details'}
			</button>
		</div>

		{#if showRuntimeDetails}
			<div class="mb-4 grid grid-cols-1 gap-3 max-w-2xl sm:grid-cols-2">
				<Card>
					<CardContent class="py-4">
						<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Storage</h4>
						<div class="text-sm space-y-1">
							<div class="flex justify-between"><span class="text-muted-foreground">Path</span> <code class="text-xs">{status.storage_path ?? 'Not configured'}</code></div>
							<div class="flex justify-between"><span class="text-muted-foreground">Status</span> <span>{status.storage_ok ? 'OK' : 'Not available'}</span></div>
						</div>
					</CardContent>
				</Card>
				<Card>
					<CardContent class="py-4">
						<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Maintenance</h4>
						<div class="flex flex-col gap-2">
							<Button size="sm" variant="outline" onclick={pruneDocker}>Cleanup Unused Images</Button>
							<div class="flex items-center gap-2 text-sm text-muted-foreground">
								<span>Manage in</span>
								<Button size="sm" onclick={() => goto('/services')}>Services</Button>
							</div>
						</div>
					</CardContent>
				</Card>
			</div>
		{/if}
	{/if}

	<!-- Action bar -->
	<div class="mb-4 flex items-center gap-3">
		<Button size="sm" onclick={() => {
			if (showInstall || showCompose) { cancelEdit(); cancelCompose(); }
			else if (showEnablePrompt) { showEnablePrompt = false; }
			else if (!status?.enabled) { showEnablePrompt = true; }
			else { editingApp = null; newPorts = [{ name: 'http', container_port: 80, host_port: '', protocol: 'TCP' }]; showInstall = true; installMode = 'simple'; }
		}}>
			{showInstall || showCompose || showEnablePrompt ? 'Cancel' : 'Install App'}
		</Button>
		{#if apps.length > 3}
			<Input bind:value={search} placeholder="Filter..." class="h-9 w-40" />
		{/if}
	</div>

	{#if showEnablePrompt}
		<Card class="mb-4 max-w-xl">
			<CardContent class="py-6">
				<p class="mb-1 font-medium">Apps need the Docker runtime</p>
				<p class="mb-4 text-sm text-muted-foreground">
					Container apps run on a Docker daemon NASty manages as a service. Pick a filesystem for Docker's data, then enable the runtime — once it's up, click Install App again to continue.
				</p>
				{#if filesystems.length === 0}
					<p class="mb-4 text-sm text-amber-400">You need at least one mounted filesystem before enabling Apps.</p>
					<Button variant="secondary" onclick={() => goto('/filesystems')}>Go to Filesystems →</Button>
				{:else}
					<div class="mb-4 flex items-center gap-2 text-sm">
						{#if filesystems.length > 1}
							<label for="apps-fs" class="text-muted-foreground">Storage filesystem:</label>
							<select id="apps-fs" bind:value={selectedFs} class="h-8 rounded-md border border-input bg-transparent px-2 text-sm">
								{#each filesystems as fs}
									<option value={fs.name}>{fs.name}</option>
								{/each}
							</select>
						{:else}
							<span class="text-muted-foreground">Storage filesystem: <code class="font-mono">{selectedFs}</code></span>
						{/if}
					</div>
					<div class="flex gap-2">
						<Button onclick={async () => { await enableApps(); showEnablePrompt = false; }} disabled={enabling || !selectedFs}>
							{enabling ? 'Enabling…' : 'Enable Apps'}
						</Button>
						<Button variant="ghost" onclick={() => { showEnablePrompt = false; }}>Cancel</Button>
					</div>
				{/if}
			</CardContent>
		</Card>
	{/if}

	{#if showInstall || showCompose}
		<Card class="mb-6 {(showCompose || editingCompose) ? 'max-w-6xl' : 'max-w-2xl'}">
			<CardContent class="pt-6">
				<h3 class="mb-4 text-lg font-semibold">{editingApp ? `Edit ${editingApp}` : editingCompose ? `Edit ${editingCompose}` : 'Install App'}</h3>

				<!-- Mode toggle (only for new installs, not edits) -->
				{#if !editingApp && !editingCompose}
					<div class="mb-4 flex rounded-md border border-border w-fit">
						<button
							onclick={() => { installMode = 'simple'; showCompose = false; showInstall = true; }}
							class="px-4 py-1.5 text-xs font-medium transition-colors rounded-l-md {installMode === 'simple' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
						>Container</button>
						<button
							onclick={() => { installMode = 'compose'; showInstall = false; showCompose = true; }}
							class="px-4 py-1.5 text-xs font-medium transition-colors rounded-r-md {installMode === 'compose' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}"
						>Compose</button>
					</div>
				{/if}

				{#if installMode === 'simple' && (showInstall || editingApp)}

				<!-- Paste docker run -->
				{#if !editingApp}
					{#if showPasteDocker}
						<div class="mb-4 rounded-lg border border-border bg-secondary/20 p-3 space-y-2">
							<div class="text-xs font-medium">Paste a <code class="font-mono">docker run</code> command</div>
							<p class="text-xs text-muted-foreground">Paste a command from documentation or tutorials — NASty will fill in the form automatically.</p>
							<textarea
								bind:value={pasteDockerCmd}
								placeholder={"docker run -d --name signal-api -p 8080:8080 \\\n  -v /data:/home/.local/share/signal-cli \\\n  -e 'MODE=native' bbernhard/signal-cli-rest-api"}
								rows="4"
								class="w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-xs"
							></textarea>
							<div class="flex gap-2">
								<Button size="xs" onclick={() => parseDockerRun(pasteDockerCmd)} disabled={!pasteDockerCmd.trim()}>Apply</Button>
								<Button size="xs" variant="secondary" onclick={() => { showPasteDocker = false; pasteDockerCmd = ''; }}>Cancel</Button>
							</div>
						</div>
					{:else}
						<button
							onclick={() => showPasteDocker = true}
							class="mb-4 text-xs text-primary hover:underline"
						>Paste a docker run command</button>
					{/if}
				{/if}

				<div class="mb-4">
					<Label for="app-name">App Name</Label>
					<Input id="app-name" value={newName} oninput={(e) => { newName = (e.currentTarget as HTMLInputElement).value.toLowerCase(); }} placeholder="whoami" class="mt-1" disabled={!!editingApp} />
					{#if newName && !isValidAppName(newName)}
						<span class="mt-1 block text-xs text-red-500">Must be lowercase letters, numbers, hyphens, dots. Max 53 chars.</span>
					{:else}
						<span class="mt-1 block text-xs text-muted-foreground">Must be DNS-safe (lowercase, no spaces).</span>
					{/if}
				</div>
				<div class="mb-4">
					<Label for="app-image">Container Image</Label>
					<Input id="app-image" bind:value={newImage} placeholder="traefik/whoami:latest" class="mt-1" onblur={inspectImage} />
					{#if inspecting}
						<span class="mt-1 block text-xs text-muted-foreground">Detecting exposed ports...</span>
					{:else if inspectMsg}
						<span class="mt-1 block text-xs text-amber-500">{inspectMsg}</span>
					{/if}
				</div>

				<!-- Ports -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Ports</Label>
						<Button size="xs" variant="outline" onclick={addPort}>+ Add Port</Button>
					</div>
					{#if newPorts.length > 0}
						<div class="grid grid-cols-[1fr_80px_90px_60px_auto] gap-2 mb-1">
							<span class="text-[0.65rem] text-muted-foreground">Name</span>
							<span class="text-[0.65rem] text-muted-foreground">Internal</span>
							<span class="text-[0.65rem] text-muted-foreground">Exposed</span>
							<span class="text-[0.65rem] text-muted-foreground"></span>
							<span></span>
						</div>
					{/if}
					{#each newPorts as port, i}
						{@const hasConflict = portConflicts.some(c => c.port === (parseInt(port.host_port) || port.container_port))}
						<div class="grid grid-cols-[1fr_80px_90px_60px_auto] gap-2 mt-1 items-center">
							<Input bind:value={port.name} placeholder="e.g. http" class="h-8 text-xs" />
							<Input type="number" bind:value={port.container_port} placeholder="Port" class="h-8 text-xs" oninput={() => checkPortConflicts(editingApp ?? undefined)} />
							<Input bind:value={port.host_port} placeholder={String(port.container_port)} class="h-8 text-xs {hasConflict ? 'border-amber-500 ring-1 ring-amber-500/50' : ''}" oninput={() => checkPortConflicts(editingApp ?? undefined)} />
							<select bind:value={port.protocol} class="h-8 rounded-md border border-input bg-transparent px-1 text-xs">
								<option>TCP</option>
								<option>UDP</option>
							</select>
							<Button size="xs" variant="ghost" onclick={() => removePort(i)}>x</Button>
						</div>
					{/each}
					{#if portConflicts.length > 0}
						<div class="mt-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
							{#each portConflicts as c}
								{@const isAuto = newPorts.some(p => !p.host_port && p.container_port === c.port)}
								<div>Port {c.port} is already in use by <span class="font-semibold">{c.used_by}</span>{#if isAuto} — set an Exposed port to avoid the conflict{/if}</div>
							{/each}
						</div>
					{/if}
					<p class="mt-1 text-[0.6rem] text-muted-foreground">Internal = port inside the container. Exposed = port on the host (defaults to internal port if empty). App is also accessible at /apps/{'{name}'}/ via reverse proxy.</p>
				</div>

				<!-- Environment Variables -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Environment Variables</Label>
						<Button size="xs" variant="outline" onclick={addEnv}>+ Add</Button>
					</div>
					{#each newEnvs as env, i}
						<div class="grid grid-cols-[1fr_1fr_auto] gap-2 mt-1 items-center">
							<Input bind:value={env.name} placeholder="Name" class="h-8 text-xs" />
							<Input bind:value={env.value} placeholder="Value" class="h-8 text-xs" />
							<Button size="xs" variant="ghost" onclick={() => removeEnv(i)}>x</Button>
						</div>
					{/each}
				</div>

				<!-- Volumes -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Volumes</Label>
						<Button size="xs" variant="outline" onclick={addVolume}>+ Add Volume</Button>
					</div>
					{#each newVolumes as vol, i}
						<div class="grid grid-cols-[1fr_1fr_auto] gap-2 mt-1 items-center">
							<Input bind:value={vol.mount_path} placeholder="/config" class="h-8 text-xs" />
							<Input bind:value={vol.host_path} placeholder="auto (bcachefs)" class="h-8 text-xs" />
							<Button size="xs" variant="ghost" onclick={() => removeVolume(i)}>x</Button>
						</div>
					{/each}
					{#if newVolumes.length > 0}
						<span class="mt-1 block text-xs text-muted-foreground">Host path is auto-generated under apps storage if left empty.</span>
					{/if}
				</div>

				<!-- Resource Limits -->
				<div class="mb-4">
					<Label>Resource Limits (optional)</Label>
					<div class="grid grid-cols-2 gap-3 mt-1">
						<div>
							<Label class="text-xs">CPU</Label>
							<Input bind:value={newCpuLimit} placeholder="e.g. 0.5 or 2" class="mt-1 h-8 text-xs" />
						</div>
						<div>
							<Label class="text-xs">Memory</Label>
							<Input bind:value={newMemoryLimit} placeholder="e.g. 256m or 1g" class="mt-1 h-8 text-xs" />
						</div>
					</div>
				</div>

				<!-- Allow unsafe — opt out of strict bind-mount sandbox -->
				<div class="mb-4 rounded-md border border-border p-3">
					<label class="flex items-start gap-2 cursor-pointer">
						<input type="checkbox" bind:checked={newAllowUnsafe} class="mt-0.5" />
						<span class="flex-1">
							<span class="block text-sm font-medium">Allow unsafe</span>
							<span class="block text-xs text-muted-foreground">
								Permit bind mounts outside the app's data dir and <code>/fs/</code>.
								Engine state under <code>/var/lib/nasty/</code>, the host root, and
								<code>..</code> traversals are still rejected.
							</span>
						</span>
					</label>
					{#if newAllowUnsafe}
						<p class="mt-2 text-xs text-amber-500">
							⚠ This app will run with relaxed sandbox rules. The deploy is logged in the audit trail and shown with an unsafe badge in the app list.
						</p>
					{/if}
				</div>

				<div class="flex gap-2">
					{#if editingApp}
						<Button onclick={updateApp} disabled={!newImage}>Save</Button>
					{:else}
						<Button onclick={install} disabled={!newName || !newImage || !isValidAppName(newName)}>Install</Button>
					{/if}
					<Button variant="secondary" onclick={cancelEdit}>Cancel</Button>
				</div>
				{:else if installMode === 'compose' && (showCompose || editingCompose)}
				<!-- Compose form: two columns at lg+ — form on the left, warnings + ingress picker on the right. -->
				<div class="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_minmax(0,360px)]">
					<div>
						<div class="mb-4">
							<Label for="compose-name">App Name</Label>
							<Input id="compose-name" value={composeName} oninput={(e) => { composeName = (e.currentTarget as HTMLInputElement).value.toLowerCase(); }} placeholder="my-stack" class="mt-1" disabled={!!editingCompose} />
							{#if composeName && !isValidAppName(composeName)}
								<span class="mt-1 block text-xs text-red-500">Must be lowercase letters, numbers, hyphens, dots. Max 53 chars.</span>
							{/if}
						</div>
						<div class="mb-4">
							<Label for="compose-file">docker-compose.yml</Label>
							<CodeEditor
								bind:value={composeContent}
								lang="yaml"
								errorLines={composeErrorLines}
								oninput={checkComposeConflicts}
								class="mt-1 h-96"
							/>
						</div>
						<!-- Allow unsafe — opt out of strict compose sandbox -->
						<div class="mb-4 rounded-md border border-border p-3">
							<label class="flex items-start gap-2 cursor-pointer">
								<input type="checkbox" bind:checked={composeAllowUnsafe} class="mt-0.5" />
								<span class="flex-1">
									<span class="block text-sm font-medium">Allow unsafe</span>
									<span class="block text-xs text-muted-foreground">
										Permit <code>privileged</code>, host namespaces, dangerous capabilities,
										host devices, and bind mounts outside the standard sandbox.
										Required for Tailscale, Plex/Jellyfin GPU transcoding, and similar workloads.
										Engine state under <code>/var/lib/nasty/</code>, the host root, and
										<code>..</code> traversals are still rejected.
									</span>
								</span>
							</label>
							{#if composeAllowUnsafe}
								<p class="mt-2 text-xs text-amber-500">
									⚠ This stack will run with elevated privileges. The deploy is logged in the audit trail and shown with an unsafe badge in the app list.
								</p>
							{/if}
						</div>
						<div class="flex gap-2">
							<Button onclick={installCompose} disabled={!composeName || !composeContent.trim() || (!editingCompose && !isValidAppName(composeName))}>
								{editingCompose ? 'Update' : 'Deploy'}
							</Button>
							<Button variant="secondary" onclick={cancelCompose}>Cancel</Button>
						</div>
					</div>
					<!-- Right column: ingress picker + warnings. Hidden when empty so the editor breathes. -->
					<div class="space-y-3">
						{#if composeTcpPorts.length > 0}
							<div class="rounded-md border border-border bg-secondary/20 p-3">
								<Label for="ingress-port" class="text-xs">Reverse-proxy target</Label>
								<select id="ingress-port" bind:value={newIngressPort} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm">
									{#each composeTcpPorts as p}
										<option value={p.host_port}>:{p.host_port} → container :{p.container_port}</option>
									{/each}
								</select>
								<p class="mt-1.5 text-xs text-muted-foreground">
									Which TCP port <code>/apps/{composeName || '&lt;name&gt;'}/</code> proxies to. UDP ports are excluded — nginx is TCP-only.
								</p>
							</div>
						{/if}
						{#if portConflicts.length > 0}
							<div class="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
								<p class="mb-1 font-medium">Port conflicts</p>
								{#each portConflicts as c}
									{@const alt = c.port < 1000 ? c.port + 8000 : c.port + 1}
									{@const lineNo = composePortLineMap.get(c.port)}
								<div><span class="font-semibold">Line {lineNo}:</span> port {c.port} is already in use by <span class="font-semibold">{c.used_by}</span> — change to e.g. <code>{alt}</code></div>
								{/each}
							</div>
						{/if}
						{#if deviceMissing.length > 0}
							<div class="rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-400">
								<p class="mb-1 font-medium">Missing devices</p>
								{#each deviceMissing as d}
									{@const lineNo = composeDeviceLineMap.get(d.path)}
									<div>
										<span class="font-semibold">Line {lineNo}:</span> device <code>{d.path}</code> doesn't exist on this host
										{#if !d.parent_exists}
											— parent directory missing too, the kernel driver may not be loaded (e.g. <code>i915</code> for Intel GPU)
										{:else}
											— check the device name (<code>ls {(d.path.split('/').slice(0, -1).join('/')) || '/'}</code>) or whether the device is physically present
										{/if}
									</div>
								{/each}
							</div>
						{/if}
						{#if volumeMismatches.length > 0}
							<div class="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
								<p class="mb-2 font-medium">
									Bind-mount permissions don't match the container's <code>user:</code> — the container will likely fail with <em>Permission denied</em>.
								</p>
								{#each volumeMismatches as m}
									{@const expectedLabel = `${m.expected_uid}:${m.expected_gid ?? m.expected_uid}`}
									<div class="mb-2 last:mb-0">
										<div>
											<span class="font-semibold">Line {m.line ?? '?'}:</span>
											<code>{m.host_path}</code>
											{#if m.exists}
												is owned by <code>{m.current_uid}:{m.current_gid}</code>, but
											{:else}
												doesn't exist yet —
											{/if}
											service <span class="font-semibold">{m.service}</span> will run as <code>{expectedLabel}</code>.
										</div>
										{#if m.exists}
											<div class="mt-1 flex flex-wrap gap-2">
												<Button
													size="xs"
													variant="secondary"
													disabled={fixingVolume === m.host_path}
													onclick={() => fixVolume(m.host_path, m.expected_uid, m.expected_gid, false)}
												>
													{fixingVolume === m.host_path ? 'Chowning…' : `Chown to ${expectedLabel}`}
												</Button>
												<Button
													size="xs"
													variant="ghost"
													disabled={fixingVolume === m.host_path}
													onclick={() => fixVolume(m.host_path, m.expected_uid, m.expected_gid, true)}
													title="Recursive — rewrites every existing file's owner"
												>
													…and contents
												</Button>
											</div>
										{:else}
											<div class="mt-1 text-muted-foreground">Will be created with the right ownership when you deploy.</div>
										{/if}
									</div>
								{/each}
							</div>
						{/if}
					</div>
				</div>
				{/if}
			</CardContent>
		</Card>
	{/if}

	{#if apps.length === 0 && !showInstall && !showCompose}
		<p class="text-muted-foreground">No apps installed.</p>
	{:else if apps.length > 0 && !(status?.enabled && status?.running)}
		<div class="mb-3 flex items-center gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-sm text-amber-400">
			<span class="flex-1">Docker runtime is not running. Apps are shown but cannot be managed until the runtime is started.</span>
			<a href="/services" class="text-xs text-amber-400 no-underline hover:text-amber-300 shrink-0">Enable in Services →</a>
		</div>
	{/if}

	<!-- Installed apps table -->
	{#if apps.length > 0}
		<h3 class="text-lg font-semibold mt-6 mb-3">Installed Apps</h3>
		<table class="w-full text-sm">
			<thead>
				<tr>
					<SortTh label="Name" active={true} dir={sortDir} onclick={toggleSort} />
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Image</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Ports</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
					<th class="w-px border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each sorted as app}
					<tr class="border-b border-border hover:bg-muted/30 transition-colors">
						<td class="p-3">
							<div class="flex items-center gap-2">
								<span class="font-semibold">{app.name}</span>
								<Badge variant="outline" class="text-[0.6rem]">{app.kind}</Badge>
								{#if app.unsafe_mode}
									<Badge
										class="bg-amber-500/15 text-amber-400 border-amber-500/40 text-[0.6rem]"
										variant="outline"
										title="Deployed with allow_unsafe — relaxed sandbox rules"
									>
										unsafe
									</Badge>
								{/if}
								{#if lockedFsByApp.get(app.name)}
									{@const blockingFs = lockedFsByApp.get(app.name)!}
									<button
										type="button"
										onclick={() => unlockBlockingFs(blockingFs)}
										class="inline-flex items-center gap-1 rounded-md border border-amber-500/40 bg-amber-500/15 px-1.5 py-0.5 text-[0.6rem] font-medium text-amber-400 hover:bg-amber-500/25"
										title="App is on a locked filesystem. Click to unlock."
									>
										<Lock size={10} />
										on {blockingFs}
									</button>
								{/if}
								{#if app.containers && app.containers.length > 1}
									<button class="text-xs text-muted-foreground hover:text-foreground" onclick={() => expanded[app.name] = !expanded[app.name]}>
										{app.containers.length} containers {expanded[app.name] ? '▾' : '▸'}
									</button>
								{/if}
							</div>
						</td>
						<td class="p-3 text-xs text-muted-foreground font-mono max-w-[200px] truncate">{app.kind === 'compose' && (app.containers?.length ?? 0) > 1 ? `${app.containers?.length} images` : app.image}</td>
						<td class="p-3 text-xs font-mono">
							{#if app.ports && app.ports.length > 0}
								{@const currentIngress = getIngress(app.name)?.host_port}
								<div class="flex flex-wrap items-center gap-1">
									{#each app.ports as p}
										{@const isTcp = (p.protocol ?? 'tcp').toLowerCase() === 'tcp'}
										{@const isIngress = isTcp && currentIngress === p.host_port}
										{#if isTcp}
											<button
												type="button"
												disabled={isIngress || switchingIngressFor === app.name}
												onclick={() => setIngressPort(app.name, p.host_port)}
												class="inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 transition-colors {isIngress ? 'border-blue-500/40 bg-blue-500/15 text-blue-400 cursor-default' : 'border-border text-muted-foreground hover:bg-muted hover:text-foreground'}"
												title={isIngress ? 'Current reverse-proxy target' : `Click to make :${p.host_port} the reverse-proxy target`}
											>
												{#if isIngress}<span aria-hidden="true">★</span>{/if}
												{p.host_port}:{p.container_port}
											</button>
										{:else}
											<span
												class="inline-flex items-center gap-1 rounded-md border border-border/60 px-1.5 py-0.5 text-muted-foreground/70"
												title="UDP — reverse proxy doesn't apply"
											>
												{p.host_port}:{p.container_port}/udp
											</span>
										{/if}
									{/each}
								</div>
							{/if}
						</td>
						<td class="p-3">
							<Badge variant={app.status === 'running' ? 'default' : 'secondary'}>
								{app.status}
							</Badge>
						</td>
						<td class="p-3">
							<div class="flex items-center gap-1.5">
								{#if primaryPort(app)}
									{@const pp = primaryPort(app)!}
									<a href="/apps/{app.name}/" target="_blank" class="inline-flex items-center whitespace-nowrap rounded-md border border-blue-500/30 bg-blue-500/10 px-2 py-0.5 text-xs text-blue-400 hover:bg-blue-500/20" title="Reverse proxy target: {pp.host_port}">
										Open
									</a>
									<a href="http://{window.location.hostname}:{pp.host_port}" target="_blank" class="inline-flex items-center whitespace-nowrap rounded-md border border-border px-2 py-0.5 text-xs text-muted-foreground hover:text-foreground hover:bg-muted" title="Direct port access (LAN)">
										:{pp.host_port}
									</a>
								{/if}
								{#if status?.running}
									{#if app.status === 'running'}
										<Button variant="outline" size="xs" onclick={() => stopApp(app.name)}>Stop</Button>
									{:else}
										<Button variant="outline" size="xs" onclick={() => startApp(app.name)}>Start</Button>
									{/if}
									<Button variant="outline" size="xs" onclick={() => showLogs(app.name, app.kind)}>Logs</Button>
									<div class="relative">
										<Button variant="outline" size="xs" onclick={() => expanded[`menu-${app.name}`] = !expanded[`menu-${app.name}`]}>···</Button>
										{#if expanded[`menu-${app.name}`]}
											<div class="absolute right-0 top-full z-10 mt-1 min-w-[120px] rounded-md border border-border bg-popover py-1 shadow-md">
												{#if app.status === 'running'}
													<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; restartApp(app.name); }}>Restart</button>
													<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; openShell(app.name); }}>Shell</button>
												{/if}
												<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; pullApp(app.name); }}>Pull image</button>
												{#if app.kind === 'simple'}
													<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; inspectApp(app.name); }}>Inspect</button>
												{/if}
												{#if app.kind === 'simple'}
													<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; editApp(app.name); }}>Edit</button>
												{:else}
													<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; editCompose(app.name); }}>Edit</button>
												{/if}
												<button class="w-full px-3 py-1.5 text-left text-xs text-destructive hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; removeApp(app.name); }}>Remove</button>
											</div>
										{/if}
									</div>
								{:else}
									<span class="text-xs text-muted-foreground">Docker stopped</span>
								{/if}
							</div>
						</td>
					</tr>
					{#if expanded[app.name] && app.containers && app.containers.length > 1}
						{#each app.containers as ct}
							<tr class="bg-muted/20">
								<td class="pl-8 pr-3 py-1.5 text-xs text-muted-foreground">{ct.name}</td>
								<td class="p-1.5 text-xs text-muted-foreground font-mono">{ct.image}</td>
								<td class="p-1.5"></td>
								<td class="p-1.5">
									<Badge variant={ct.status === 'running' ? 'default' : 'secondary'} class="text-[0.6rem]">{ct.status}</Badge>
								</td>
								<td class="p-1.5">
									{#if ct.status === 'running' && ct.container_id}
										<div class="flex items-center gap-1.5">
											<button class="rounded border border-border px-1.5 py-0.5 text-[0.65rem] text-muted-foreground hover:bg-muted hover:text-foreground" onclick={() => goto(`/terminal?cmd=${encodeURIComponent(`docker exec -it ${ct.container_id} /bin/sh`)}`)}>Shell</button>
											<button class="rounded border border-border px-1.5 py-0.5 text-[0.65rem] text-muted-foreground hover:bg-muted hover:text-foreground" onclick={() => showLogs(ct.container_id, 'container')}>Logs</button>
											<button class="rounded border border-border px-1.5 py-0.5 text-[0.65rem] text-muted-foreground hover:bg-muted hover:text-foreground" onclick={() => inspectApp(ct.name)}>Inspect</button>
										</div>
									{/if}
								</td>
							</tr>
						{/each}
					{/if}
				{/each}
			</tbody>
		</table>
	{/if}
{/if}

<!-- Deploy Output Modal -->
{#if deployLog.length > 0 || deploying}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="flex flex-col w-[90vw] max-w-4xl h-[70vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl">
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<div class="flex items-center gap-2">
					{#if deploying}
						<div class="h-3 w-3 animate-spin rounded-full border-2 border-muted border-t-green-400"></div>
					{:else if deployError}
						<div class="h-3 w-3 rounded-full bg-red-500"></div>
					{:else}
						<div class="h-3 w-3 rounded-full bg-green-500"></div>
					{/if}
					<span class="text-sm font-semibold text-white">
						{deploying ? 'Deploying...' : deployError ? 'Deploy Failed' : 'Deploy Complete'}
					</span>
				</div>
				{#if !deploying}
					<Button variant="ghost" size="xs" onclick={closeDeployLog} class="text-white hover:text-white/80">
						Close
					</Button>
				{/if}
			</div>
			<pre
				class="flex-1 p-4 overflow-auto text-xs font-mono whitespace-pre-wrap {deployError ? 'text-red-400' : 'text-green-400'}"
				id="deploy-output"
			>{deployLog.join('\n')}</pre>
		</div>
	</div>
{/if}

<!-- Logs Modal -->
{#if inspectName}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="flex flex-col w-[90vw] max-w-4xl h-[70vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl">
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<span class="text-sm font-semibold text-white">Inspect: {inspectName}</span>
				<Button variant="ghost" size="xs" onclick={() => inspectName = null} class="text-white hover:text-white/80">
					Close
				</Button>
			</div>
			<pre class="flex-1 p-4 overflow-auto text-xs font-mono whitespace-pre-wrap">{@html highlightJson(inspectData ?? '')}</pre>
		</div>
	</div>
{/if}

{#if logsApp}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="flex flex-col w-[90vw] max-w-4xl h-[70vh] rounded-lg border border-border bg-[#0f1117] shadow-2xl">
			<div class="flex items-center justify-between px-4 py-2 border-b border-border">
				<span class="text-sm font-semibold text-white">Logs: {logsApp}</span>
				<Button variant="ghost" size="xs" onclick={() => logsApp = null} class="text-white hover:text-white/80">
					Close
				</Button>
			</div>
			<pre class="flex-1 p-4 overflow-auto text-xs text-green-400 font-mono whitespace-pre-wrap">{logsContent}</pre>
		</div>
	</div>
{/if}
