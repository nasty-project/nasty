<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import type { AppsStatus, App, AppIngress, AppConfig, ImageInspectResult, AppContainer, AppStats, MappedPort, PruneResult, SubPathRecipe, NetworkSummary, ManagedNetwork, NetworkState, ComposeStartupEntry } from '$lib/types';
	import { formatBytes } from '$lib/format';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import CodeEditor from '$lib/components/CodeEditor.svelte';
	import PathPicker from '$lib/components/PathPicker.svelte';
	import { CircleCheck, Circle, FolderOpen } from '@lucide/svelte';
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
			deployAction = null;

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
					} else if (msg.type === 'action' && msg.action === 'create_macvlan' && msg.bridge) {
						// Structured hint (#429): the referenced external network
						// is a host bridge. Surface a "create macvlan + retry" button.
						deployAction = { action: msg.action, bridge: msg.bridge };
						deployLog = [...deployLog, msg.data];
					}
				} catch { /* ignore */ }
			};

			ws.onerror = () => {
				deployError = 'WebSocket connection failed';
				deploying = false;
				resolve(false);
			};

			ws.onclose = async () => {
				if (deployDone || deployError) return;
				// WS closed without a 'done' or 'error' message. The
				// install/uninstall the engine kicked off may have
				// finished anyway — the deploy stream is just the
				// progress channel, not the source of truth. Query
				// apps.list to see what actually happened before
				// declaring failure, otherwise a transient WS blip
				// during the docker create_container step shows a red
				// "Connection closed unexpectedly" modal even though
				// the container is up and running.
				const expectedName = (params.name as string) ?? '';
				try {
					const list = await client.call<App[]>('apps.list');
					const installed = list.some(a => a.name === expectedName);
					if (installed) {
						// The app exists — install must have completed
						// server-side after the WS dropped. Treat as
						// success so the modal closes cleanly.
						deployDone = true;
						deploying = false;
						deployLog = [...deployLog, '(connection dropped, but app is installed — treating as success)'];
						resolve(true);
						return;
					}
				} catch { /* fall through to the error path */ }
				deployError = 'Connection closed unexpectedly';
				deploying = false;
				resolve(false);
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
	// Compose stack startup config (#437), for the Startup-order card.
	let composeStartup: ComposeStartupEntry[] = $state([]);
	// Map of app-name → locked-FS-name. Populated from
	// `fs.locked_dependents` so each app row can show a "🔒 on tank"
	// badge that opens the global unlock dialog. Stays empty when
	// nothing's locked, so non-encrypted setups pay zero overhead.
	let lockedFsByApp = $state(new Map<string, string>());
	// Map of app-name → latest stats sample. Empty until `apps.stats`
	// returns its first response, and emptied when the page is hidden so
	// we don't poll Docker from a background tab.
	let appStats = $state<Record<string, AppStats>>({});
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
					// `-v src:dest`: src is a *bind mount* only when it's an
					// absolute host path (Docker rule — anything else, like
					// `haze-data:/var/lib/haze`, is a Docker named volume).
					// For named volumes we want NASty's auto-managed storage
					// instead — leave host_path empty, the install pipeline
					// auto-creates a chowned dir under /fs/<x>/apps/<name>/.
					// Without this branch the parser was stuffing the volume
					// name (`haze-data`) into host_path, which the engine
					// then rejects as not under /fs/.
					const src = parts[0];
					const isBindMount = src.startsWith('/');
					volumes.push({
						name: isBindMount ? `vol-${volumes.length}` : src,
						host_path: isBindMount ? expandShellVars(src) : '',
						mount_path: parts[1],
					});
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
	/** `is_image_default` = the row's value came from the image's own
	 * `Config.Env` (not set by the user). Greyed out in Edit, shown with
	 * an "Override" button that flips `overriding` to true and makes the
	 * row editable — only overridden rows end up in updateApp's req.env,
	 * so the user's override list stays clean of image internals. */
	let newEnvs = $state<{ name: string; value: string; is_image_default?: boolean; overriding?: boolean }[]>([]);
	let newVolumes = $state<{ name: string; mount_path: string; host_path: string }[]>([]);
	/** Index into `newVolumes` of the row whose host_path picker is
	 * currently open; null when no picker is up. */
	let volumePickerIndex = $state<number | null>(null);
	let newCpuLimit = $state('');
	let newMemoryLimit = $state('');
	/** Optional FQDN for subdomain-mode ingress at install time
	 * (`jellyfin.example.com`). Empty = path-prefix mode, the default.
	 * Live-checked against engine conflict state below as the operator
	 * types so a hostname that's already in use surfaces immediately. */
	let newSubdomain = $state('');
	/** Engine-reported conflict for `newSubdomain`; '' when clear. */
	let newSubdomainConflict = $state('');
	let newSubdomainCheckSeq = 0;
	let newSubdomainCheckTimer: ReturnType<typeof setTimeout> | null = null;
	/** Opt out of the strict bind-mount sandbox for simple apps. Persisted per-app. */
	let newAllowUnsafe = $state(false);
	/** Same flag, separate state for the compose dialog. */
	let composeAllowUnsafe = $state(false);

	// ── Docker networks (#435 / #438) ─────────────────────────
	/** Managed-network attachment in the simple-app install/edit form. */
	let newNetwork = $state('');
	let newStaticIp = $state('');
	/** All NASty-manageable Docker networks (apps.networks.list). */
	let appNetworks = $state<NetworkSummary[]>([]);
	/** Host interfaces/bridges for the create-network parent picker. */
	let netState = $state<NetworkState | null>(null);
	/** True when the chosen install network gives the app its own LAN IP
	 * (macvlan/ipvlan) — published host ports + reverse-proxy ingress don't
	 * apply, so the form hides those sections to match the engine. */
	let installLanIp = $derived(
		appNetworks.some(
			(n) => n.name === newNetwork && (n.driver === 'macvlan' || n.driver === 'ipvlan'),
		),
	);
	// Create-network dialog state.
	let showNetCreate = $state(false);
	let ncName = $state('');
	let ncDriver = $state('macvlan');
	let ncParent = $state('');
	let ncSubnet = $state('');
	let ncGateway = $state('');
	let ncIpRange = $state('');
	let ncVlan = $state('');
	let ncHostShim = $state(false);
	let ncShimIp = $state('');
	let ncError = $state('');
	let ncSaving = $state(false);
	/** True when the chosen parent is the management interface — a host
	 * shim there risks lockout, so the engine refuses it and we disable it. */
	let ncParentIsMgmt = $derived(!!netState && netState.mgmt_iface === ncParent);
	/** Actionable hint surfaced by the deploy stream (#429): the compose
	 * referenced a host bridge — offer to create a macvlan on it + retry. */
	let deployAction = $state<{ action: string; bridge: string } | null>(null);

	async function loadAppNetworks() {
		try {
			appNetworks = await client.call<NetworkSummary[]>('apps.networks.list');
		} catch {
			appNetworks = [];
		}
	}

	/** Parent candidates for macvlan/ipvlan: bridges (preferred) + standalone
	 * NICs, each labeled with its role. Bridge-member NICs are excluded — the
	 * bridge itself is the correct parent (engine rejects otherwise). */
	let parentChoices = $derived.by(() => {
		const out: { name: string; label: string }[] = [];
		if (!netState) return out;
		const members = new Set(netState.config.bridges.flatMap((b) => b.members));
		for (const b of netState.config.bridges) {
			const role = b.members.length ? b.members.join(', ') : 'host-internal';
			const mgmt = netState.mgmt_iface === b.name ? '; management' : '';
			out.push({ name: b.name, label: `${b.name} — bridge (${role}${mgmt})` });
		}
		for (const i of netState.interfaces) {
			if (members.has(i.name) || i.kind === 'bridge' || i.kind === 'virtual') continue;
			const mgmt = netState.mgmt_iface === i.name ? ' — management' : '';
			out.push({ name: i.name, label: `${i.name} — ${i.kind}${mgmt}` });
		}
		return out;
	});

	async function openNetCreate(prefillBridge?: string) {
		ncName = prefillBridge ?? '';
		ncDriver = 'macvlan';
		ncParent = prefillBridge ?? '';
		ncSubnet = '';
		ncGateway = '';
		ncIpRange = '';
		ncVlan = '';
		ncHostShim = false;
		ncShimIp = '';
		ncError = '';
		try {
			netState = await client.call<NetworkState>('system.network.get');
		} catch {
			netState = null;
		}
		showNetCreate = true;
	}

	async function createNetwork() {
		ncSaving = true;
		ncError = '';
		const spec: ManagedNetwork = {
			name: ncName.trim(),
			driver: ncDriver,
			parent: ncDriver === 'bridge' ? null : ncParent || null,
			subnet: ncSubnet.trim() || null,
			gateway: ncGateway.trim() || null,
			ip_range: ncIpRange.trim() || null,
			vlan: ncVlan.trim() ? parseInt(ncVlan) : null,
			host_shim: ncDriver === 'macvlan' && ncHostShim && !ncParentIsMgmt,
			shim_ip: ncDriver === 'macvlan' && ncHostShim ? ncShimIp.trim() || null : null,
		};
		try {
			await client.call('apps.networks.create', spec);
			showNetCreate = false;
			await loadAppNetworks();
		} catch (e) {
			ncError = e instanceof Error ? e.message : String(e);
		} finally {
			ncSaving = false;
		}
	}

	async function removeNetwork(name: string) {
		const r = await withToast(
			() => client.call('apps.networks.remove', { name }),
			'Network removed',
		);
		if (r !== undefined) await loadAppNetworks();
	}
	/** Has the operator clicked Install / Deploy on this form at least
	 * once? Gates the amber required-field decoration so a fresh form
	 * opens clean rather than lit up with "required" everywhere. Set
	 * true by the submit handlers when a required field is empty;
	 * cleared by resetForm / cancelCompose. */
	let installTried = $state(false);
	let composeTried = $state(false);
	let inspecting = $state(false);
	let lastInspectedImage = '';
	/** Inline status from the last apps.inspect_image call so the user sees
	 *  why ports weren't auto-detected (image unreachable, no EXPOSE, etc.). */
	let inspectMsg: string | null = $state(null);
	/** Curated recipe for serving this image under /apps/<name>/, if the
	 *  engine recognised the image. Surfaced as an "Apply <Name> sub-path
	 *  mode" button next to the Environment Variables section — clicking
	 *  it appends the recipe's env entries to newEnvs with `{name}`,
	 *  `{host}`, `{scheme}` placeholders substituted. */
	let subpathRecipe = $state<SubPathRecipe | null>(null);

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
	// are excluded — Caddy's reverse_proxy handles HTTP/TCP only.
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
		filesystem_missing?: boolean;
		line: number | null;
	};
	let volumeMismatches = $state<VolumeMismatch[]>([]);
	// Underline every actionable line: existing-owner mismatches need
	// the user's attention, and a missing-filesystem path is a hard
	// error in the source field.
	let composeVolumeErrorLines = $derived(
		volumeMismatches
			.filter(m => (m.exists || m.filesystem_missing) && m.line != null)
			.map(m => m.line as number)
	);
	let fixingVolume = $state<string | null>(null); // host_path currently being chowned

	/** One owner-mismatch group: a parent path plus any of its
	 * descendant binds that share the same expected (uid, gid). A
	 * single recursive chown of the parent covers all descendants,
	 * so we render them folded under the parent — saves the user
	 * clicking N near-identical Chown buttons in a row. */
	type AggregatedMismatch = {
		parent: VolumeMismatch;
		descendants: VolumeMismatch[];
	};

	function effectiveGid(m: VolumeMismatch): number {
		return m.expected_gid ?? m.expected_uid;
	}

	function aggregateOwnerMismatches(items: VolumeMismatch[]): AggregatedMismatch[] {
		// Shortest paths first — any parent is shorter than its child.
		const sorted = [...items].sort((a, b) => a.host_path.length - b.host_path.length);
		const taken = new Set<number>();
		const out: AggregatedMismatch[] = [];
		for (let i = 0; i < sorted.length; i++) {
			if (taken.has(i)) continue;
			const parent = sorted[i];
			const descendants: VolumeMismatch[] = [];
			for (let j = i + 1; j < sorted.length; j++) {
				if (taken.has(j)) continue;
				const child = sorted[j];
				// Strict-prefix check with the `/` separator so
				// `/data` doesn't capture `/data-other`.
				if (!child.host_path.startsWith(parent.host_path + '/')) continue;
				// Recursive chown only covers descendants if they
				// share the parent's expected owner. Different
				// `user:` per service → can't fold.
				if (child.expected_uid !== parent.expected_uid) continue;
				if (effectiveGid(child) !== effectiveGid(parent)) continue;
				descendants.push(child);
				taken.add(j);
			}
			out.push({ parent, descendants });
		}
		return out;
	}

	// ── Live compose lint (#439): YAML syntax + compose schema, checked
	// server-side with the same `docker compose config` deploy runs, so
	// the editor can't approve something deploy would reject. ──
	type ComposeDiagnostic = { line?: number | null; message: string };
	type CheckComposeResult = {
		schema_checked: boolean;
		valid: boolean;
		diagnostics: ComposeDiagnostic[];
	};
	let composeLint = $state<CheckComposeResult | null>(null);
	let composeLintTimer: ReturnType<typeof setTimeout> | null = null;
	let composeLintErrorLines = $derived(
		(composeLint?.diagnostics ?? []).filter(d => d.line != null).map(d => d.line as number)
	);

	// Editor underlines syntax/schema, port-conflict, missing-device,
	// and existing-but-wrong-owner lines.
	const composeErrorLines = $derived([
		...composeLintErrorLines,
		...composePortErrorLines,
		...composeDeviceErrorLines,
		...composeVolumeErrorLines,
	]);

	const client = getClient();
	let startupPoll: ReturnType<typeof setInterval> | null = null;
	let statsPoll: ReturnType<typeof setInterval> | null = null;
	/** Idle poll for the apps list itself. Without this, the page only
	 * refreshes apps.list after a user action — so a container that
	 * crashes, or an install kicked off via another browser tab, leaves
	 * the page silently stale until the user reloads. Lighter cadence
	 * than the 2s stats poll: list cardinality changes are rare. */
	let listPoll: ReturnType<typeof setInterval> | null = null;
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
			// Prefill VOLUME declarations from the image so single-image apps
			// that need persistent storage (e.g. haze's /var/lib/haze for
			// SQLite) don't get installed with a writable-layer-only mount
			// and crash-loop on first write. Only seed when the user hasn't
			// already added rows manually.
			if (result.volumes && result.volumes.length > 0 && newVolumes.length === 0) {
				newVolumes = result.volumes.map(v => ({
					name: v.name,
					mount_path: v.mount_path,
					host_path: v.host_path ?? '',
				}));
			}
			// Surface the runtime user so the operator knows what UID the
			// auto-created volume dirs will be chowned to. Most images
			// running as root won't have this field set.
			if (result.user) {
				const userMsg = `Image runs as ${result.user} — auto-created volume dirs will be chowned to that identity.`;
				inspectMsg = inspectMsg ? `${inspectMsg} ${userMsg}` : userMsg;
			}
			// Stash any sub-path recipe so the UI can render the
			// "Apply <recipe> sub-path mode" button. We don't auto-apply;
			// the user opts in so they see what's being added.
			subpathRecipe = result.subpath_recipe ?? null;
		} catch (e) {
			// Registry unreachable / image not found / private without auth.
			// Surface inline so the user knows to fall back to manual entry.
			const msg = e instanceof Error ? e.message : typeof e === 'object' && e !== null && 'message' in e ? String((e as { message: unknown }).message) : String(e);
			inspectMsg = `Could not inspect image (${msg}) — set ports manually.`;
		}
		inspecting = false;
		checkPortConflicts();
	}

	/** Apply the engine-supplied sub-path recipe to the install form.
	 * Substitutes `{name}` with the App Name field, and `{host}`/`{scheme}`
	 * with what the browser sees right now — so Vaultwarden's `DOMAIN`
	 * matches the origin the user will type into their browser (CSRF
	 * checks rely on exact match). Appends entries to newEnvs rather
	 * than overwriting, and skips keys the user has already set so the
	 * user's manual config wins. After apply, clear the recipe state
	 * so the button doesn't re-trigger on every keystroke.
	 */
	function applySubpathRecipe() {
		if (!subpathRecipe) return;
		const host = window.location.host;
		const scheme = window.location.protocol.replace(/:$/, '');
		const existing = new Set(newEnvs.map(e => e.name));
		const additions = subpathRecipe.env
			.filter(e => !existing.has(e.name))
			.map(e => ({
				name: e.name,
				value: e.value
					.replaceAll('{name}', newName || 'app')
					.replaceAll('{host}', host)
					.replaceAll('{scheme}', scheme),
			}));
		if (additions.length > 0) {
			newEnvs = [...newEnvs, ...additions];
		}
		subpathRecipe = null;
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
		scheduleComposeLint();
	}

	function scheduleComposeLint() {
		// Schema validation spawns `docker compose config` server-side —
		// debounce so we lint typing pauses, not every keystroke.
		if (composeLintTimer) clearTimeout(composeLintTimer);
		composeLintTimer = setTimeout(() => {
			client.call<CheckComposeResult>('apps.check_compose', { compose: composeContent })
				.then(r => { composeLint = r; })
				.catch(() => { composeLint = null; });
		}, 500);
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
		await Promise.all([refresh(), loadFilesystems(), refreshAppdataStatus()]);
		loading = false;
		if (status?.enabled && !status?.running) startStartupPolling();
		if (status?.enabled && status?.running) {
			startStatsPolling();
			startListPolling();
		}
		document.addEventListener('visibilitychange', onVisibilityChange);
	});

	onDestroy(() => {
		stopStartupPolling();
		stopStatsPolling();
		stopListPolling();
		stopAppdataPolling();
		document.removeEventListener('visibilitychange', onVisibilityChange);
	});

	// ── Appdata location (#436) ──
	type AppdataRelocateStatus = {
		running: boolean;
		phase: string;
		target_fs: string;
		error?: string;
		old_path?: string;
		affected_apps: string[];
	};
	let appdataRelocate = $state<AppdataRelocateStatus | null>(null);
	let appdataTargetFs = $state('');
	let appdataPoll: ReturnType<typeof setInterval> | null = null;

	async function refreshAppdataStatus() {
		try {
			appdataRelocate = await client.call<AppdataRelocateStatus | null>('apps.appdata.status');
			if (appdataRelocate?.running) startAppdataPolling();
		} catch { /* apps disabled — card stays passive */ }
	}

	function startAppdataPolling() {
		if (appdataPoll) return;
		appdataPoll = setInterval(async () => {
			await refreshAppdataStatus();
			if (!appdataRelocate?.running && appdataPoll) {
				clearInterval(appdataPoll);
				appdataPoll = null;
				// Pick up restarted apps + the new appdata path.
				void refresh();
			}
		}, 2000);
	}

	function stopAppdataPolling() {
		if (appdataPoll) { clearInterval(appdataPoll); appdataPoll = null; }
	}

	async function relocateAppdata() {
		if (!appdataTargetFs) return;
		if (!await confirm(
			`Move appdata to "${appdataTargetFs}"?`,
			`Apps that bind /appdata will be stopped while the data is copied, then restarted. Compose references keep working — /appdata simply points at the new filesystem. The old copy stays in place until you delete it.`
		)) return;
		await withToast(
			() => client.call('apps.appdata.relocate', { filesystem: appdataTargetFs }),
			`Relocating appdata to "${appdataTargetFs}"`
		);
		appdataTargetFs = '';
		await refreshAppdataStatus();
	}

	function startStartupPolling() {
		stopStartupPolling();
		startupPoll = setInterval(async () => {
			await refresh();
			if (status?.running) {
				stopStartupPolling();
				startStatsPolling();
			}
		}, 5000);
	}

	function stopStartupPolling() {
		if (startupPoll) {
			clearInterval(startupPoll);
			startupPoll = null;
		}
	}

	// Live per-app resource usage. Docker's stats endpoint walks
	// cgroups on every call, so we pace at 2s (matches the "feels live"
	// threshold without thrashing the daemon) and pause entirely when
	// the tab is backgrounded — a hidden tab polling stats earned no
	// useful information and cost real CPU on the box.
	function startStatsPolling() {
		stopStatsPolling();
		void refreshStats();
		statsPoll = setInterval(refreshStats, 2000);
	}

	function stopStatsPolling() {
		if (statsPoll) {
			clearInterval(statsPoll);
			statsPoll = null;
		}
	}

	/** Poll apps.list (+ status + ingresses) so the table reflects out-of-band
	 * changes — a container crashing on its own, an install kicked off from
	 * another tab, or our own deploy where the WS finished but the post-install
	 * ingress probe was still tightening things up server-side. 5s is the same
	 * pace as the existing startup poll and far enough below the cost of an
	 * apps.list call (one Docker API request + a few manifest reads) that
	 * the daemon won't notice.
	 *
	 * Paused while the tab is hidden — see onVisibilityChange — for the same
	 * reason stats polling pauses: a hidden tab issuing polls every 5s earns
	 * no useful information and just costs CPU on the box. */
	function startListPolling() {
		stopListPolling();
		listPoll = setInterval(() => { void refresh(); }, 5000);
	}

	function stopListPolling() {
		if (listPoll) {
			clearInterval(listPoll);
			listPoll = null;
		}
	}

	async function refreshStats() {
		if (document.hidden) return;
		try {
			const list = await client.call<AppStats[]>('apps.stats');
			const next: Record<string, AppStats> = {};
			for (const s of list) next[s.name] = s;
			appStats = next;
		} catch { /* ignore — keep last good values */ }
	}

	function onVisibilityChange() {
		if (document.hidden) {
			stopStatsPolling();
			stopListPolling();
		} else if (status?.enabled && status?.running) {
			startStatsPolling();
			startListPolling();
			// Tab just came back — refresh once immediately so the user
			// doesn't stare at stale data for up to 5s waiting for the
			// next interval tick.
			void refresh();
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
				await loadAppNetworks();
				try {
					composeStartup = await client.call<ComposeStartupEntry[]>('apps.compose.startup.list');
				} catch { composeStartup = []; }
				if (!statsPoll) startStatsPolling();
				// Also lift the idle list-poll here so Docker coming up
				// after the page loaded (e.g. user enabled apps from this
				// session) kicks polling on without needing a manual reload.
				// Skip when the tab's hidden — onVisibilityChange will lift
				// it when the user comes back.
				if (!listPoll && !document.hidden) startListPolling();
			} else {
				ingresses = [];
				stopStatsPolling();
				stopListPolling();
				appStats = {};
			}
		} catch { /* ignore */ }
		await loadLockedFsByApp();
	}

	// ── Compose startup ordering (#437) ──────────────────────────────
	let startupBusy = $state<string | null>(null);
	const composeStacks = $derived(apps.filter((a) => a.kind === 'compose'));
	/** Startup config for a stack, defaulting to unmanaged when absent. */
	function startupOf(name: string): ComposeStartupEntry {
		return composeStartup.find((e) => e.name === name) ?? { name, managed: false, order: 0, delay_secs: 0 };
	}
	async function setComposeStartup(name: string, managed: boolean, order: number, delay_secs: number) {
		startupBusy = name;
		await withToast(
			() => client.call('apps.compose.set_startup', {
				name,
				managed,
				order: Math.max(0, Math.floor(order) || 0),
				delay_secs: Math.max(0, Math.floor(delay_secs) || 0),
			}),
			managed ? 'Startup updated' : 'Removed from managed startup'
		);
		startupBusy = null;
		await refresh();
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
		// Flip the "tried" gate before bailing on missing fields so the
		// amber required-field decoration shows up exactly when the
		// operator expects: after they clicked Install and it didn't
		// proceed, never before.
		if (!newName || !newImage) { installTried = true; return; }
		installTried = false;
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
		// A LAN-IP (macvlan/ipvlan) app gets its own address — published host
		// ports and reverse-proxy ingress don't apply (engine rejects them).
		if (newPorts.length > 0 && !installLanIp) {
			params.ports = newPorts.map(p => ({
				name: p.name,
				container_port: p.container_port,
				host_port: p.host_port ? parseInt(p.host_port) : undefined,
				protocol: p.protocol,
			}));
		}
		if (newEnvs.length > 0) {
			// Same image-default filter as updateApp — symmetric so an
			// install path that somehow seeded image-default rows (e.g.
			// re-using Edit state, or a future paste-from-running flow)
			// wouldn't quietly pin those values either.
			params.env = newEnvs
				.filter(e => e.name && (!e.is_image_default || e.overriding))
				.map(e => ({ name: e.name, value: e.value }));
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
		// Subdomain-mode opt-in. Engine validates again (and re-checks
		// for conflicts on its own) so this is just passthrough — if the
		// operator typed a hostname that became taken between the live
		// conflict check and Save, install fails with a clear error.
		const subdomainTrimmed = newSubdomain.trim();
		if (subdomainTrimmed && !installLanIp) params.subdomain = subdomainTrimmed;
		// Managed-network attachment (+ optional static IP).
		if (newNetwork) params.network = newNetwork;
		if (newStaticIp.trim()) params.static_ip = newStaticIp.trim();

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
		newEnvs = config.env.map(e => ({
			name: e.name,
			value: e.value,
			is_image_default: e.is_image_default ?? false,
			overriding: false,
		}));
		newVolumes = config.volumes.map(v => ({ name: v.name, mount_path: v.mount_path, host_path: v.host_path }));
		newCpuLimit = config.cpu_limit ?? '';
		newMemoryLimit = config.memory_limit ?? '';
		newAllowUnsafe = config.allow_unsafe ?? false;
		newNetwork = config.network ?? '';
		newStaticIp = config.static_ip ?? '';
		loadAppNetworks();
		installMode = 'simple';
		showInstall = true;
	}

	async function updateApp() {
		if (!editingApp || !newImage) return;
		const params: Record<string, unknown> = {
			name: editingApp,
			image: newImage,
		};
		if (newPorts.length > 0 && !installLanIp) {
			params.ports = newPorts.map(p => ({
				name: p.name,
				container_port: p.container_port,
				host_port: p.host_port ? parseInt(p.host_port) : undefined,
				protocol: p.protocol,
			}));
		}
		if (newEnvs.length > 0) {
			// Drop image-default rows unless the user explicitly clicked
			// Override on them. Without this, every Edit + Save would
			// pin all image defaults into req.env — silently masking
			// any future upstream change to those defaults.
			params.env = newEnvs
				.filter(e => e.name && (!e.is_image_default || e.overriding))
				.map(e => ({ name: e.name, value: e.value }));
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
		// Round-trip the managed-network attachment so Edit doesn't detach it.
		if (newNetwork) params.network = newNetwork;
		if (newStaticIp.trim()) params.static_ip = newStaticIp.trim();

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
		// Simple-installer create form. One field per line so a newly
		// added `new*` state var only needs to land here in addition
		// to its declaration — the scan-for-omissions cost stays
		// linear instead of hiding inside a dense one-liner.
		newName = '';
		newImage = '';
		newPorts = [];
		newEnvs = [];
		newVolumes = [];
		newCpuLimit = '';
		newMemoryLimit = '';
		newAllowUnsafe = false;
		newSubdomain = '';
		newSubdomainConflict = '';
		newNetwork = '';
		newStaticIp = '';
		installTried = false;
		lastInspectedImage = '';
		subpathRecipe = null;
	}

	/** Debounced 300ms check of the install-form Subdomain field against
	 * engine state (other apps' subdomains + the WebUI hostname). Mirrors
	 * the per-app dialog's check from #231 — same RPC, same sequence guard
	 * so a stale slow response can't overwrite a fresh fast one. The
	 * `name` we send is the install form's appName so the engine's
	 * self-exclusion lines up with what install() will register. */
	function scheduleNewSubdomainConflictCheck() {
		if (newSubdomainCheckTimer) clearTimeout(newSubdomainCheckTimer);
		const trimmed = newSubdomain.trim();
		if (!trimmed) {
			newSubdomainConflict = '';
			return;
		}
		newSubdomainCheckSeq += 1;
		const seq = newSubdomainCheckSeq;
		const appName = (newName || '').trim().toLowerCase();
		newSubdomainCheckTimer = setTimeout(async () => {
			try {
				const reason = await client.call<string>('apps.ingress.check_conflict', {
					name: appName,
					subdomain: trimmed,
				});
				if (seq === newSubdomainCheckSeq) {
					newSubdomainConflict = reason ?? '';
				}
			} catch {
				/* leave the previous result rather than flashing "no conflict" */
			}
		}, 300);
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
			async () => {
				try {
					await client.call('apps.remove', { name });
				} catch (e: unknown) {
					// The remove RPC takes a few seconds (docker stop + rm
					// + ingress detach) and can land on a brief WS blip,
					// at which point the call rejects with "WebSocket
					// disconnected" even though the server-side work is
					// in flight and usually completes. Verify the actual
					// state after a reconnect before surfacing the
					// failure — re-issuing apps.remove is idempotent if
					// the engine already finished (returns AppNotFound
					// → we treat that as success too).
					const msg = e instanceof Error ? e.message :
						(typeof e === 'object' && e !== null && 'message' in e) ?
						String((e as { message: unknown }).message) : String(e);
					if (msg !== 'WebSocket disconnected') throw e;
					// Wait for the client to reconnect (its internal
					// _readyPromise resolves on next successful auth),
					// then verify by listing apps.
					try {
						const list = await client.call<App[]>('apps.list');
						if (!list.some(a => a.name === name)) return;
					} catch { /* fall through */ }
					throw e;
				}
			},
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
		// apps.exec_command errors when the container has no shell (scratch
		// or distroless images — haze is the canonical case). Without the
		// try/catch the rejected promise silently swallowed the click: no
		// goto, no toast, the user just saw nothing happen. Surface the
		// engine's message so the operator knows *why* Shell didn't work.
		let cmd: string;
		try {
			cmd = await client.call<string>('apps.exec_command', { name });
		} catch (e) {
			const msg = e instanceof Error ? e.message :
				(typeof e === 'object' && e !== null && 'message' in e) ?
				String((e as { message: unknown }).message) : String(e);
			await withToast(async () => { throw new Error(msg); }, '');
			return;
		}
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
		if (!composeName || !composeContent.trim()) { composeTried = true; return; }
		composeTried = false;
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
			composeName = ''; composeContent = ''; composeTried = false;
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
		composeLint = null;
		if (composeLintTimer) { clearTimeout(composeLintTimer); composeLintTimer = null; }
		composeTcpPorts = [];
		newIngressPort = null;
		composeName = ''; composeContent = ''; composeTried = false;
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
	 * port chips when the user clicks a non-current TCP port. Preserves
	 * subdomain mode if the current ingress already had one. */
	async function setIngressPort(appName: string, hostPort: number) {
		switchingIngressFor = appName;
		try {
			const current = getIngress(appName);
			await withToast(
				() => client.call('apps.ingress.set', {
					name: appName,
					host_port: hostPort,
					// Sticky: switching the host port shouldn't drop a
					// subdomain choice the operator already made.
					subdomain: current?.subdomain ?? null,
				}),
				`Ingress for ${appName} → :${hostPort}`
			);
			await loadIngresses();
		} finally {
			switchingIngressFor = null;
		}
	}

	// ── Subdomain mode dialog ─────────────────────────────────
	/** Per-app modal opened by the "···" menu's "Subdomain" item.
	 * `null` = closed; otherwise carries the app name being edited
	 * (host_port is read from the current ingress at submit time). */
	let subdomainDialog = $state<{ appName: string; value: string; host_port: number } | null>(null);
	/** Live conflict-check feedback for the subdomain input. Empty
	 * string when the value is fine, non-empty when the engine detected
	 * a conflict (another app already using it, or the WebUI hostname).
	 * Save remains gated server-side too — this is just for fast feedback. */
	let subdomainConflict = $state('');
	let subdomainCheckSeq = 0;
	let subdomainCheckTimer: ReturnType<typeof setTimeout> | null = null;

	function openSubdomainDialog(appName: string) {
		const current = getIngress(appName);
		// When no ingress exists (e.g. the post-install probe disabled
		// one for a haze-class app), fall back to the app's first TCP
		// port so saveSubdomain can synthesise a fresh ingress. Without
		// this fallback the dialog used to be hidden entirely from the
		// menu — operators of probe-disabled apps had no UI path to
		// rescue them into subdomain mode.
		const app = apps.find(a => a.name === appName);
		const fallbackPort = app?.ports?.find(p => p.protocol?.toLowerCase() === 'tcp')?.host_port;
		const host_port = current?.host_port ?? fallbackPort ?? 0;
		subdomainDialog = { appName, value: current?.subdomain ?? '', host_port };
		subdomainConflict = '';
	}

	/** Debounced conflict check — runs 300ms after the operator stops
	 * typing so we don't fire an RPC on every keystroke. Sequence number
	 * guards against an older-but-slower response overwriting a newer
	 * answer. */
	function scheduleSubdomainConflictCheck() {
		if (subdomainCheckTimer) clearTimeout(subdomainCheckTimer);
		const dialog = subdomainDialog;
		if (!dialog) return;
		const trimmed = dialog.value.trim();
		if (!trimmed) {
			subdomainConflict = '';
			return;
		}
		subdomainCheckSeq += 1;
		const seq = subdomainCheckSeq;
		subdomainCheckTimer = setTimeout(async () => {
			try {
				const reason = await client.call<string>('apps.ingress.check_conflict', {
					name: dialog.appName,
					subdomain: trimmed,
				});
				if (seq === subdomainCheckSeq) {
					subdomainConflict = reason ?? '';
				}
			} catch {
				// Network glitch — leave the previous result rather than
				// flashing a misleading "no conflict" when we can't tell.
			}
		}, 300);
	}

	async function saveSubdomain() {
		if (!subdomainDialog) return;
		const { appName, value, host_port } = subdomainDialog;
		const current = getIngress(appName);
		const subdomain = value.trim() || null;
		// No-op guard for the haze-class case: app has no current ingress
		// (probe disabled it) AND operator saved with an empty subdomain.
		// Creating a path-prefix ingress in that scenario would just be
		// killed by the probe again, leaving the operator confused. Close
		// the dialog silently — saving with a non-empty subdomain still
		// creates the ingress, which is the actual rescue path.
		if (!current && !subdomain) {
			subdomainDialog = null;
			return;
		}
		if (host_port === 0) {
			subdomainDialog = null;
			return;
		}
		switchingIngressFor = appName;
		try {
			await withToast(
				() => client.call('apps.ingress.set', {
					name: appName,
					host_port,
					subdomain,
				}),
				subdomain ? `Ingress for ${appName} → ${subdomain}` : `Ingress for ${appName} → /apps/${appName}/`
			);
			await loadIngresses();
			subdomainDialog = null;
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
				<Card>
					<CardContent class="py-4">
						<h4 class="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Appdata</h4>
						<div class="text-sm space-y-1">
							<div class="flex justify-between"><span class="text-muted-foreground">Stable path</span> <code class="text-xs">/appdata</code></div>
							<div class="flex justify-between"><span class="text-muted-foreground">Lives on</span> <code class="text-xs">{status.appdata_path ?? 'not set up yet'}</code></div>
							{#if status.appdata_path && !status.appdata_ok}
								<p class="text-xs text-destructive">/appdata does not resolve — is that filesystem mounted?</p>
							{/if}
						</div>
						{#if appdataRelocate?.running}
							<div class="mt-2 rounded-md bg-secondary/50 px-2 py-1.5 text-xs">
								<span class="mr-1 inline-block h-2 w-2 animate-pulse rounded-full bg-yellow-500"></span>
								Relocating to <code>{appdataRelocate.target_fs}</code> — {appdataRelocate.phase.replace('_', ' ')}
								{#if appdataRelocate.affected_apps.length}
									<div class="mt-0.5 text-muted-foreground">apps paused: {appdataRelocate.affected_apps.join(', ')}</div>
								{/if}
							</div>
						{:else}
							{#if appdataRelocate?.phase === 'failed'}
								<p class="mt-2 text-xs text-destructive">Relocation failed: {appdataRelocate.error}</p>
							{:else if appdataRelocate?.phase === 'done'}
								<p class="mt-2 text-xs text-green-600">Moved. Old copy left at <code>{appdataRelocate.old_path}</code> — delete it once you've verified the apps.{#if appdataRelocate.error} {appdataRelocate.error}{/if}</p>
							{/if}
							{#if status.appdata_path}
								{@const currentFs = status.appdata_path.match(/^\/fs\/([^/]+)\//)?.[1]}
								{@const targets = filesystems.filter(f => f.name !== currentFs)}
								{#if targets.length > 0}
									<div class="mt-2 flex items-center gap-2">
										<select bind:value={appdataTargetFs} class="h-8 flex-1 rounded-md border border-input bg-transparent px-2 text-xs">
											<option value="">Move to filesystem…</option>
											{#each targets as f}<option value={f.name}>{f.name}</option>{/each}
										</select>
										<Button size="sm" variant="outline" disabled={!appdataTargetFs} onclick={relocateAppdata}>Relocate</Button>
									</div>
								{/if}
							{/if}
							<p class="mt-2 text-[0.65rem] text-muted-foreground">Bind app data as <code>/appdata/&lt;app&gt;/…</code> — references survive relocation. Snapshot or back it up from Subvolumes.</p>
						{/if}
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
						<!-- Discoverable button instead of a quiet link: pre-#232 it
						     was a small underlined text line that operators routinely
						     missed (rendered like a paragraph, not a button) and
						     filled the form by hand. Outline + clipboard glyph make
						     it scan as an action affordance at a glance. -->
						<Button
							variant="outline"
							size="sm"
							class="mb-4 gap-2"
							onclick={() => showPasteDocker = true}
						>
							<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
								<rect width="8" height="4" x="8" y="2" rx="1" ry="1"/>
								<path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/>
							</svg>
							Paste a <code class="font-mono">docker run</code> command
						</Button>
					{/if}
				{/if}

				<!-- "Missing required" decoration is gated on installTried so the
				     form opens clean and only goes amber after the operator
				     hits Install with an empty field. The invalid-format
				     check stays always-on (user just typed something wrong,
				     they want to know immediately). -->
				{@const appNameMissing = !editingApp && !newName && installTried}
				{@const appNameInvalid = !!newName && !isValidAppName(newName)}
				{@const imageMissing = !editingApp && !newImage && installTried}
				<div class="mb-4">
					<Label for="app-name">App Name {#if appNameMissing}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
					<Input id="app-name" value={newName} oninput={(e) => { newName = (e.currentTarget as HTMLInputElement).value.toLowerCase(); }} placeholder="whoami" class="mt-1 {requiredFieldCls(appNameMissing || appNameInvalid)}" disabled={!!editingApp} />
					{#if appNameInvalid}
						<span class="mt-1 block text-xs text-red-500">Must be lowercase letters, numbers, hyphens, dots. Max 53 chars.</span>
					{:else}
						<span class="mt-1 block text-xs text-muted-foreground">Must be DNS-safe (lowercase, no spaces).</span>
					{/if}
				</div>
				<div class="mb-4">
					<Label for="app-image">Container Image {#if imageMissing}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
					<Input id="app-image" bind:value={newImage} placeholder="traefik/whoami:latest" class="mt-1 {requiredFieldCls(imageMissing)}" onblur={inspectImage} />
					{#if inspecting}
						<span class="mt-1 block text-xs text-muted-foreground">Detecting exposed ports...</span>
					{:else if inspectMsg}
						<span class="mt-1 block text-xs text-amber-500">{inspectMsg}</span>
					{/if}
				</div>

				<!-- Network attachment (#435 / #438) -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label for="app-network">Network</Label>
						<Button size="xs" variant="outline" onclick={() => openNetCreate()}>+ New network</Button>
					</div>
					<select id="app-network" bind:value={newNetwork} class="h-9 w-full rounded-md border border-input bg-background px-2 text-sm">
						<option value="">Default bridge (reverse-proxy ingress)</option>
						{#each appNetworks as n}
							<option value={n.name}>{n.name} ({n.driver}{n.parent ? ` on ${n.parent}` : ''})</option>
						{/each}
					</select>
					{#if installLanIp}
						<p class="mt-1 text-xs text-amber-500">This app gets its own LAN IP — host ports and reverse-proxy ingress don't apply and are hidden below.</p>
						<div class="mt-2">
							<Label for="app-static-ip">Static IP (optional)</Label>
							<Input id="app-static-ip" bind:value={newStaticIp} placeholder="e.g. 192.168.1.50" class="mt-1" />
						</div>
					{/if}
				</div>

				{#if !installLanIp}
				<!-- Ports -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Ports</Label>
						<Button size="xs" variant="outline" onclick={addPort}>+ Add Port</Button>
					</div>
					{#if newPorts.length > 0}
						<!-- Column order matches `docker run -p HOST:CONTAINER` —
						     Exposed (host) first, Internal (container) second.
						     The listing and the dropdown elsewhere on this page
						     also read host:container, so all three surfaces line
						     up. See issue #271. -->
						<div class="grid grid-cols-[1fr_90px_80px_60px_auto] gap-2 mb-1">
							<span class="text-[0.65rem] text-muted-foreground">Name</span>
							<span class="text-[0.65rem] text-muted-foreground">Exposed</span>
							<span class="text-[0.65rem] text-muted-foreground">Internal</span>
							<span class="text-[0.65rem] text-muted-foreground"></span>
							<span></span>
						</div>
					{/if}
					{#each newPorts as port, i}
						{@const hasConflict = portConflicts.some(c => c.port === (parseInt(port.host_port) || port.container_port))}
						<div class="grid grid-cols-[1fr_90px_80px_60px_auto] gap-2 mt-1 items-center">
							<Input bind:value={port.name} placeholder="e.g. http" class="h-8 text-xs" />
							<Input bind:value={port.host_port} placeholder={String(port.container_port)} class="h-8 text-xs {hasConflict ? 'border-amber-500 ring-1 ring-amber-500/50' : ''}" oninput={() => checkPortConflicts(editingApp ?? undefined)} />
							<Input type="number" bind:value={port.container_port} placeholder="Port" class="h-8 text-xs" oninput={() => checkPortConflicts(editingApp ?? undefined)} />
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
					<p class="mt-1 text-[0.6rem] text-muted-foreground">Exposed = port on the host (what clients connect to). Internal = port inside the container. Leave Exposed blank to use the same number as Internal. App is also accessible at /apps/{'{name}'}/ via reverse proxy.</p>
				</div>
				{/if}

				<!-- Environment Variables -->
				<div class="mb-4">
					<div class="flex items-center justify-between mb-1">
						<Label>Environment Variables</Label>
						<Button size="xs" variant="outline" onclick={addEnv}>+ Add</Button>
					</div>
					{#if subpathRecipe}
						<!-- Engine recognised this image as one with a known sub-path recipe
						     (Grafana, Vaultwarden, ...). Show the recipe behind an opt-in
						     button so the user confirms before any env vars are added —
						     the values are templated against window.location at click time
						     so they match the origin the browser actually uses. -->
						<div class="mt-1 flex items-center gap-2 rounded-md border border-blue-500/30 bg-blue-500/10 px-2 py-1.5 text-xs">
							<span class="text-blue-400">Reverse-proxy at <code>/apps/{newName || '<name>'}/</code> supported.</span>
							<Button size="xs" variant="outline" onclick={applySubpathRecipe}>Apply {subpathRecipe.display_name}</Button>
						</div>
					{/if}
					{#each newEnvs as env, i}
						{@const greyed = env.is_image_default && !env.overriding}
						<div class="grid grid-cols-[1fr_1fr_auto] gap-2 mt-1 items-center" title={greyed ? "Image default — click Override to change." : ""}>
							<Input bind:value={env.name} placeholder="Name" class="h-8 text-xs {greyed ? 'opacity-60' : ''}" disabled={greyed} />
							<Input bind:value={env.value} placeholder="Value" class="h-8 text-xs {greyed ? 'opacity-60' : ''}" disabled={greyed} />
							{#if env.is_image_default && !env.overriding}
								<!-- Image-default rows show Override (not "x") — removing an image-default
								     would be a no-op since Docker still injects it from the image. Override
								     flips the row to user-owned so updateApp pins this exact value. -->
								<Button size="xs" variant="outline" onclick={() => { env.overriding = true; }}>Override</Button>
							{:else if env.is_image_default && env.overriding}
								<!-- Reverting drops the explicit override; Docker falls back to
								     whatever the image declares (including any future upstream
								     change). The typed value stays in the input but is ignored
								     on save — the greyed appearance signals "won't be saved". -->
								<Button size="xs" variant="ghost" onclick={() => { env.overriding = false; }} title="Revert to image default on save">↺</Button>
							{:else}
								<Button size="xs" variant="ghost" onclick={() => removeEnv(i)}>x</Button>
							{/if}
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
						<div class="grid grid-cols-[1fr_1fr_auto_auto] gap-2 mt-1 items-center">
							<Input bind:value={vol.mount_path} placeholder="/config" class="h-8 text-xs" />
							<Input bind:value={vol.host_path} placeholder="auto (bcachefs)" class="h-8 text-xs font-mono" />
							<Button size="xs" variant="outline" onclick={() => { volumePickerIndex = i; }} title="Browse for a host folder or subvolume">
								<FolderOpen size={12} />
							</Button>
							<Button size="xs" variant="ghost" onclick={() => removeVolume(i)}>x</Button>
						</div>
					{/each}
					{#if newVolumes.length > 0}
						<span class="mt-1 block text-xs text-muted-foreground">Leave empty to auto-create under apps storage, or pick an existing folder/subvolume with the browse button.</span>
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

				{#if !installLanIp}
				<!-- Subdomain (optional) — opt into subdomain-mode ingress from
				     day one. Empty = path-prefix mode + post-install probe (the
				     historical default). Non-empty = host-match Caddy route +
				     probe skipped. Live conflict check below. -->
				<div class="mb-4">
					<Label>Subdomain <span class="text-muted-foreground font-normal">(optional)</span></Label>
					<Input
						bind:value={newSubdomain}
						oninput={scheduleNewSubdomainConflictCheck}
						placeholder="jellyfin.example.com"
						class="mt-1 h-8 text-xs {newSubdomainConflict ? 'border-amber-500 ring-1 ring-amber-500/50' : ''}"
					/>
					{#if newSubdomainConflict}
						<p class="mt-1 text-xs text-amber-500">⚠ {newSubdomainConflict}</p>
					{:else}
						<p class="mt-1 text-[0.65rem] text-muted-foreground">
							Leave empty to serve under <code>/apps/{newName || '<name>'}/</code>.
							Set an FQDN to serve at its own root — required for apps that emit
							absolute paths (haze, jellyfin). Caddy uses the existing TLS/ACME
							config; the operator points DNS at NASty.
						</p>
					{/if}
				</div>
				{/if}

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
						<!-- Install stays enabled even when required fields are empty
						     so clicking it triggers the amber decoration. Real
						     blockers (invalid name format, subdomain conflict) keep
						     it disabled — those are user-correctable but should never
						     reach the server. -->
						<Button onclick={install} disabled={(!!newName && !isValidAppName(newName)) || !!newSubdomainConflict}>Install</Button>
					{/if}
					<Button variant="secondary" onclick={cancelEdit}>Cancel</Button>
				</div>
				{:else if installMode === 'compose' && (showCompose || editingCompose)}
				<!-- composeTried gates the amber treatment so the form opens
				     clean. composeNameInvalid stays always-on because that's
				     "you typed something invalid", not "you forgot a field". -->
				{@const composeNameMissing = !editingCompose && !composeName && composeTried}
				{@const composeNameInvalid = !!composeName && !isValidAppName(composeName)}
				{@const composeContentMissing = !composeContent.trim() && composeTried}
				<!-- Compose form: two columns at lg+ — form on the left, warnings + ingress picker on the right. -->
				<div class="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_minmax(0,360px)]">
					<div>
						<div class="mb-4">
							<Label for="compose-name">App Name {#if composeNameMissing}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
							<Input id="compose-name" value={composeName} oninput={(e) => { composeName = (e.currentTarget as HTMLInputElement).value.toLowerCase(); }} placeholder="my-stack" class="mt-1 {requiredFieldCls(composeNameMissing || composeNameInvalid)}" disabled={!!editingCompose} />
							{#if composeNameInvalid}
								<span class="mt-1 block text-xs text-red-500">Must be lowercase letters, numbers, hyphens, dots. Max 53 chars.</span>
							{/if}
						</div>
						<div class="mb-4">
							<Label for="compose-file">docker-compose.yml {#if composeContentMissing}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
							<CodeEditor
								bind:value={composeContent}
								lang="yaml"
								errorLines={composeErrorLines}
								oninput={checkComposeConflicts}
								class="mt-1 h-96 {composeContentMissing ? 'ring-1 ring-amber-500/50 rounded-md' : ''}"
							/>
							{#if composeLint && !composeLint.valid}
								<div class="mt-2 rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-400">
									<p class="mb-1 font-medium">Compose file is not valid — deploy will fail with this content.</p>
									{#each composeLint.diagnostics as d}
										<div class="font-mono">
											{#if d.line != null}<span class="font-semibold">Line {d.line}:</span>{/if}
											{d.message}
										</div>
									{/each}
								</div>
							{:else if composeLint?.valid && composeLint.schema_checked && composeContent.trim()}
								<p class="mt-1 text-xs text-green-600">✓ Valid compose file</p>
							{/if}
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
							<Button onclick={installCompose} disabled={!editingCompose && !!composeName && !isValidAppName(composeName)}>
								{editingCompose ? 'Update' : 'Deploy'}
							</Button>
							<Button variant="secondary" onclick={cancelCompose}>Cancel</Button>
						</div>
					</div>
					<!-- Right column: ingress picker + warnings. Hidden when empty so the editor breathes. -->
					<div class="space-y-3">
						{#if composeTcpPorts.length > 0}
							<div class="rounded-md border border-border bg-secondary/20 px-3 py-2 text-xs">
								<p class="mb-1 font-medium"><label for="ingress-port">Reverse-proxy target</label></p>
								<select id="ingress-port" bind:value={newIngressPort} class="h-8 w-full rounded-md border border-input bg-transparent px-2 text-xs">
									{#each composeTcpPorts as p}
										<option value={p.host_port}>:{p.host_port} → container :{p.container_port}</option>
									{/each}
								</select>
								<p class="mt-1.5 text-muted-foreground">
									Which TCP port <code>/apps/{composeName || '<name>'}/</code> proxies to. UDP ports are excluded — Caddy's reverse_proxy is HTTP-only.
								</p>
							</div>
						{/if}
						{#if portConflicts.length > 0}
							<div class="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
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
							{@const fsMissing = volumeMismatches.filter(m => m.filesystem_missing)}
							{@const ownerMismatches = volumeMismatches.filter(m => !m.filesystem_missing)}
							{#if fsMissing.length > 0}
								<div class="rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-400">
									<p class="mb-2 font-medium">
										Bind source points at a filesystem that isn't mounted — fix the source path, the deploy will fail otherwise.
									</p>
									{#each fsMissing as m}
										{@const fsName = m.host_path.replace(/^\/fs\//, '').split('/')[0]}
										<div class="mb-1 last:mb-0">
											<span class="font-semibold">Line {m.line ?? '?'}:</span>
											<code>{m.host_path}</code> — no filesystem is mounted at
											<code>/fs/{fsName}</code>. Pick an existing filesystem (see Storage → Filesystems).
										</div>
									{/each}
								</div>
							{/if}
							{#if ownerMismatches.length > 0}
								<div class="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
									<p class="mb-2 font-medium">
										Bind-mount permissions don't match the container's <code>user:</code> — the container will likely fail with <em>Permission denied</em>.
									</p>
									{#each aggregateOwnerMismatches(ownerMismatches) as group}
										{@const m = group.parent}
										{@const expectedLabel = `${m.expected_uid}:${m.expected_gid ?? m.expected_uid}`}
										{@const nested = group.descendants.length}
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
											{#if nested > 0}
												<div class="mt-1 text-muted-foreground">
													↳ {nested} nested bind{nested === 1 ? '' : 's'} share{nested === 1 ? 's' : ''} the same expected owner
													{#each group.descendants as d, i}{#if i === 0} ({:else}, {/if}line {d.line ?? '?'}: <code>{d.host_path}</code>{#if i === group.descendants.length - 1}){/if}{/each}
													— a recursive chown of the parent covers them all.
												</div>
											{/if}
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
														variant={nested > 0 ? 'secondary' : 'ghost'}
														disabled={fixingVolume === m.host_path}
														onclick={() => fixVolume(m.host_path, m.expected_uid, m.expected_gid, true)}
														title="Recursive — rewrites every existing file's owner"
													>
														…and contents
													</Button>
												</div>
											{:else}
												<div class="mt-1 text-muted-foreground">
													Will be created with the right ownership when you deploy{nested > 0 ? ` (along with the ${nested} nested bind${nested === 1 ? '' : 's'})` : ''}.
												</div>
											{/if}
										</div>
									{/each}
							</div>
						{/if}
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

	<!-- Docker networks (#435 / #438) -->
	{#if status?.running}
		<div class="mt-6 flex items-center justify-between">
			<h3 class="text-lg font-semibold">Networks</h3>
			<Button size="xs" variant="outline" onclick={() => openNetCreate()}>+ Create network</Button>
		</div>
		{#if appNetworks.length === 0}
			<p class="mt-1 text-xs text-muted-foreground">No managed networks. Create a macvlan/ipvlan network to give apps their own LAN IP, or a user bridge for inter-container DNS.</p>
		{:else}
			<table class="mt-2 w-full text-sm">
				<thead>
					<tr class="text-left text-xs uppercase text-muted-foreground">
						<th class="p-2">Name</th>
						<th class="p-2">Driver</th>
						<th class="p-2">Parent</th>
						<th class="p-2">Subnet</th>
						<th class="p-2">Attached</th>
						<th class="p-2"></th>
					</tr>
				</thead>
				<tbody>
					{#each appNetworks as n}
						<tr class="border-t border-border/40">
							<td class="p-2 font-mono">{n.name}{#if !n.exists} <span class="text-amber-500" title="Persisted but missing from Docker — recreated on boot">(missing)</span>{/if}</td>
							<td class="p-2">{n.driver}</td>
							<td class="p-2 font-mono">{n.parent ?? '—'}{#if n.vlan}.{n.vlan}{/if}</td>
							<td class="p-2 font-mono">{n.subnet ?? '—'}</td>
							<td class="p-2">{n.attached_apps.length > 0 ? n.attached_apps.join(', ') : '—'}</td>
							<td class="p-2 text-right">
								{#if n.managed}
									<Button size="xs" variant="ghost" disabled={n.attached_apps.length > 0} title={n.attached_apps.length > 0 ? 'Detach all apps first' : 'Remove network'} onclick={() => removeNetwork(n.name)}>Remove</Button>
								{:else}
									<span class="text-xs text-muted-foreground" title="Not created by NASty">external</span>
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}
	{/if}

	<!-- Startup order (#437): NASty-managed boot ordering for compose stacks -->
	{#if composeStacks.length > 0}
		<h3 class="text-lg font-semibold mt-6 mb-1">Compose Startup Order</h3>
		<p class="mb-3 max-w-3xl text-xs text-muted-foreground">
			Let NASty bring compose stacks up at boot in a set order, waiting a few seconds after each
			to let things settle (e.g. a stack that creates shared Docker networks before the stacks
			that use them). Managed stacks are pinned to <code>restart: "no"</code> so Docker won't
			race the engine — your compose files are left untouched.
		</p>
		<table class="w-full max-w-3xl text-sm">
			<thead>
				<tr>
					<th class="border-b-2 border-border p-2 text-left text-xs uppercase text-muted-foreground">Managed</th>
					<th class="border-b-2 border-border p-2 text-left text-xs uppercase text-muted-foreground">Stack</th>
					<th class="border-b-2 border-border p-2 text-right text-xs uppercase text-muted-foreground">Order</th>
					<th class="border-b-2 border-border p-2 text-right text-xs uppercase text-muted-foreground">Delay (s)</th>
				</tr>
			</thead>
			<tbody>
				{#each composeStacks.map((a) => startupOf(a.name)).sort((x, y) => x.managed === y.managed ? (x.order - y.order || x.name.localeCompare(y.name)) : (x.managed ? -1 : 1)) as e (e.name)}
					<tr class="border-b border-border {e.managed ? '' : 'opacity-60'}">
						<td class="p-2">
							<input
								type="checkbox"
								class="h-4 w-4"
								checked={e.managed}
								disabled={startupBusy === e.name}
								onchange={(ev) => setComposeStartup(e.name, (ev.target as HTMLInputElement).checked, e.order, e.delay_secs)} />
						</td>
						<td class="p-2 font-medium">{e.name}</td>
						<td class="p-2 text-right">
							<input
								type="number" min="0"
								class="h-8 w-20 rounded-md border border-input bg-transparent px-2 text-right text-sm disabled:opacity-50"
								value={e.order}
								disabled={!e.managed || startupBusy === e.name}
								onchange={(ev) => setComposeStartup(e.name, true, Number((ev.target as HTMLInputElement).value), e.delay_secs)} />
						</td>
						<td class="p-2 text-right">
							<input
								type="number" min="0" max="300"
								class="h-8 w-20 rounded-md border border-input bg-transparent px-2 text-right text-sm disabled:opacity-50"
								value={e.delay_secs}
								disabled={!e.managed || startupBusy === e.name}
								onchange={(ev) => setComposeStartup(e.name, true, e.order, Number((ev.target as HTMLInputElement).value))} />
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	<!-- Installed apps table -->
	{#if apps.length > 0}
		<h3 class="text-lg font-semibold mt-6 mb-3">Installed Apps</h3>
		<table class="w-full text-sm">
			<thead>
				<tr>
					<SortTh label="Name" active={true} dir={sortDir} onclick={toggleSort} />
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Image</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap">CPU</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap">Memory</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap" title="Cumulative bytes since container start">Net&nbsp;I/O</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap" title="Cumulative block-device read/write since container start">Disk&nbsp;I/O</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground" title="host:container — same direction as `docker run -p HOST:CONTAINER`">Ports</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Status</th>
					<th class="w-px border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground whitespace-nowrap">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each sorted as app}
					{@const s = appStats[app.name]}
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
						<td class="p-3 text-xs font-mono whitespace-nowrap">
							{#if s}{s.cpu_percent.toFixed(1)}%{:else}<span class="text-muted-foreground">—</span>{/if}
						</td>
						<td class="p-3 text-xs font-mono whitespace-nowrap" title={s ? `cgroup limit: ${formatBytes(s.memory_limit_bytes)}` : ''}>
							{#if s}{formatBytes(s.memory_bytes)}{:else}<span class="text-muted-foreground">—</span>{/if}
						</td>
						<td class="p-3 text-xs font-mono whitespace-nowrap" title="rx ↓ / tx ↑ since container start">
							{#if s}<span class="text-muted-foreground">↓</span>&nbsp;{formatBytes(s.net_rx_bytes)}&nbsp;&nbsp;<span class="text-muted-foreground">↑</span>&nbsp;{formatBytes(s.net_tx_bytes)}{:else}<span class="text-muted-foreground">—</span>{/if}
						</td>
						<td class="p-3 text-xs font-mono whitespace-nowrap" title="block read / write since container start">
							{#if s}<span class="text-muted-foreground">R</span>&nbsp;{formatBytes(s.block_read_bytes)}&nbsp;&nbsp;<span class="text-muted-foreground">W</span>&nbsp;{formatBytes(s.block_write_bytes)}{:else}<span class="text-muted-foreground">—</span>{/if}
						</td>
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
									{@const ing = getIngress(app.name)}
									{#if ing}
										{@const openHref = ing.subdomain
											? `${window.location.protocol}//${ing.subdomain}/`
											: `/apps/${app.name}/`}
										{@const openTitle = ing.subdomain
											? `Reverse proxy: ${ing.subdomain} → :${pp.host_port}`
											: `Reverse proxy target: ${pp.host_port}`}
										<a href={openHref} target="_blank" class="inline-flex items-center whitespace-nowrap rounded-md border border-blue-500/30 bg-blue-500/10 px-2 py-0.5 text-xs text-blue-400 hover:bg-blue-500/20" title={openTitle}>
											Open
										</a>
									{:else if app.proxy_disabled_reason}
										<!-- Engine probe disabled the reverse-proxy ingress because the app emits
										     absolute root-path assets (haze-class apps). Tooltip carries the reason so
										     the user understands why only the direct-port link is offered. -->
										<span class="inline-flex items-center whitespace-nowrap rounded-md border border-amber-500/30 bg-amber-500/10 px-2 py-0.5 text-xs text-amber-500 cursor-help" title={app.proxy_disabled_reason}>
											Direct port only
										</span>
									{/if}
									<a href="http://{window.location.hostname}:{pp.host_port}" target="_blank" class="inline-flex items-center whitespace-nowrap rounded-md border border-border px-2 py-0.5 text-xs text-muted-foreground hover:text-foreground hover:bg-muted" title="Direct port access (LAN)">
										:{pp.host_port}
									</a>
								{/if}
								{#if app.network}
									<!-- App on a managed Docker network — show the network (and
									     its own LAN IP for macvlan/ipvlan). No reverse-proxy
									     "Open" link: it's reached directly on its IP. -->
									<span class="inline-flex items-center whitespace-nowrap rounded-md border border-purple-500/30 bg-purple-500/10 px-2 py-0.5 text-xs text-purple-300" title="Docker network: {app.network}">
										{app.network}{app.network_ip ? ` @ ${app.network_ip}` : ''}
									</span>
								{/if}
								{#if status?.running}
									<!-- Stop also for "restarting" — that is the crash-loop state, and Start would be a no-op. -->
									{#if app.status === 'running' || app.status === 'restarting'}
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
												{#if app.ports?.some(p => p.protocol?.toLowerCase() === 'tcp')}
													<!-- Always shown for apps with a TCP port. Lets the operator
													     opt into / out of subdomain mode regardless of whether the
													     current ingress is path-prefix, subdomain, or absent (the
													     post-install probe disabled it for a haze-class app). -->
													<button class="w-full px-3 py-1.5 text-left text-xs hover:bg-muted" onclick={() => { expanded[`menu-${app.name}`] = false; openSubdomainDialog(app.name); }}>Subdomain…</button>
												{/if}
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

<!-- Subdomain ingress dialog (per-app "···" → "Subdomain…") -->
{#if subdomainDialog}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
		<div class="w-[90vw] max-w-md rounded-lg border border-border bg-[#0f1117] p-5 shadow-2xl">
			<h3 class="mb-1 text-base font-semibold">Reverse-proxy ingress for <code class="text-blue-400">{subdomainDialog.appName}</code></h3>
			<p class="mb-3 text-xs text-muted-foreground">
				Empty = serve at <code>/apps/{subdomainDialog.appName}/</code> (path-prefix mode, the default).
				Set a fully-qualified hostname to serve the app at its own root — works for apps that emit absolute paths (haze, jellyfin, etc.).
				Caddy uses the existing TLS/ACME config; the operator needs DNS for the hostname to resolve to NASty.
			</p>
			<Label class="text-xs">Subdomain</Label>
			<Input
				bind:value={subdomainDialog.value}
				oninput={scheduleSubdomainConflictCheck}
				placeholder="jellyfin.example.com"
				class="mt-1 {subdomainConflict ? 'border-amber-500 ring-1 ring-amber-500/50' : ''}"
			/>
			{#if subdomainConflict}
				<!-- Engine-side check: another engine app already claims this
				     hostname, or it matches NASty's own WebUI hostname (would
				     shadow the management interface). Save stays gated server-
				     side too — this is just fast feedback so the operator
				     doesn't get a surprise toast after clicking. -->
				<p class="mt-1 text-xs text-amber-500">⚠ {subdomainConflict}</p>
			{/if}
			<div class="mt-4 flex justify-end gap-2">
				<Button variant="outline" size="sm" onclick={() => { subdomainDialog = null; }}>Cancel</Button>
				<Button size="sm" onclick={saveSubdomain} disabled={switchingIngressFor === subdomainDialog.appName || !!subdomainConflict}>Save</Button>
			</div>
		</div>
	</div>
{/if}

<!-- Volume host-path picker (simple installer Volume rows) -->
<PathPicker
	open={volumePickerIndex !== null}
	initialPath={volumePickerIndex !== null ? newVolumes[volumePickerIndex]?.host_path ?? '' : ''}
	title="Pick host folder for this volume"
	onPick={(path) => {
		if (volumePickerIndex !== null) {
			newVolumes[volumePickerIndex].host_path = path;
		}
		volumePickerIndex = null;
	}}
	onClose={() => { volumePickerIndex = null; }}
/>

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
			{#if deployAction && deployAction.action === 'create_macvlan'}
				<div class="flex items-center justify-between gap-2 border-t border-border px-4 py-2">
					<span class="text-xs text-amber-300">'{deployAction.bridge}' is a host bridge — create a macvlan network on it to put this app on your LAN.</span>
					<Button size="xs" onclick={() => { const b = deployAction!.bridge; closeDeployLog(); openNetCreate(b); }}>
						Create macvlan on {deployAction.bridge}
					</Button>
				</div>
			{/if}
		</div>
	</div>
{/if}

<!-- Create Docker network dialog (#435 / #438) -->
{#if showNetCreate}
	<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) showNetCreate = false; }}>
		<div class="w-[90vw] max-w-md rounded-lg border border-border bg-card p-5 shadow-2xl">
			<h3 class="mb-3 text-lg font-semibold">Create Docker network</h3>
			<div class="space-y-3">
				<div>
					<Label for="nc-name">Name</Label>
					<Input id="nc-name" bind:value={ncName} placeholder="lan" class="mt-1" />
				</div>
				<div>
					<Label for="nc-driver">Driver</Label>
					<select id="nc-driver" bind:value={ncDriver} class="mt-1 h-9 w-full rounded-md border border-input bg-background px-2 text-sm">
						<option value="macvlan">macvlan — own LAN IP (separate MAC)</option>
						<option value="ipvlan">ipvlan — own LAN IP (shared MAC; WiFi/MAC-limited)</option>
						<option value="bridge">bridge — NAT'd, reached via ingress</option>
					</select>
				</div>
				{#if ncDriver !== 'bridge'}
					<div>
						<Label for="nc-parent">Parent interface</Label>
						<select id="nc-parent" bind:value={ncParent} class="mt-1 h-9 w-full rounded-md border border-input bg-background px-2 text-sm">
							<option value="" disabled>Select a bridge or NIC…</option>
							{#each parentChoices as p}
								<option value={p.name}>{p.label}</option>
							{/each}
						</select>
						<p class="mt-1 text-[0.65rem] text-muted-foreground">Pick the bridge your VMs use to share the same LAN segment. Bridge-member NICs are excluded — use the bridge itself.</p>
					</div>
					<div>
						<Label for="nc-vlan">VLAN tag (optional)</Label>
						<Input id="nc-vlan" bind:value={ncVlan} placeholder="e.g. 10" class="mt-1" />
					</div>
				{/if}
				<div>
					<Label for="nc-subnet">Subnet (optional)</Label>
					<Input id="nc-subnet" bind:value={ncSubnet} placeholder="192.168.1.0/24" class="mt-1" />
				</div>
				<div class="grid grid-cols-2 gap-2">
					<div>
						<Label for="nc-gw">Gateway (optional)</Label>
						<Input id="nc-gw" bind:value={ncGateway} placeholder="192.168.1.1" class="mt-1" />
					</div>
					<div>
						<Label for="nc-range">IP range (optional)</Label>
						<Input id="nc-range" bind:value={ncIpRange} placeholder="192.168.1.64/27" class="mt-1" />
					</div>
				</div>
				{#if ncDriver === 'macvlan'}
					<!-- Host↔container shim (#448): advanced, mutates host networking. -->
					<div class="rounded-md border border-border p-2">
						<label class="flex items-start gap-2 text-xs {ncParentIsMgmt ? 'opacity-50' : 'cursor-pointer'}">
							<input type="checkbox" class="mt-0.5" bind:checked={ncHostShim} disabled={ncParentIsMgmt} />
							<span>
								<span class="font-medium">Host shim</span> — let this host reach containers on this network.
								<span class="block text-[0.65rem] text-amber-300">Advanced: adds a macvlan interface + route on the host. May affect host networking.{#if ncParentIsMgmt} Disabled — parent is the management interface.{/if}</span>
							</span>
						</label>
						{#if ncHostShim && !ncParentIsMgmt}
							<div class="mt-2">
								<Label for="nc-shim-ip">Host shim IP</Label>
								<Input id="nc-shim-ip" bind:value={ncShimIp} placeholder="192.168.1.2/24" class="mt-1" />
								<p class="mt-1 text-[0.65rem] text-muted-foreground">The host's own address on the container subnet (inside the subnet, outside the IP range/gateway).</p>
							</div>
						{/if}
					</div>
				{/if}
				{#if ncError}<p class="text-xs text-red-500">{ncError}</p>{/if}
			</div>
			<div class="mt-4 flex justify-end gap-2">
				<Button variant="secondary" size="sm" onclick={() => showNetCreate = false}>Cancel</Button>
				<Button size="sm" onclick={createNetwork} disabled={ncSaving || !ncName.trim() || (ncDriver !== 'bridge' && !ncParent) || (ncHostShim && !ncParentIsMgmt && !ncShimIp.trim())}>
					{ncSaving ? 'Creating…' : 'Create'}
				</Button>
			</div>
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
