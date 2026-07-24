<script lang="ts">
	import { onMount, setContext } from 'svelte';
	import { page } from '$app/stores';
	import { getClient, resetClient } from '$lib/client';
	import { login as doLogin, logout as doLogout, loginWebauthn as doLoginWebauthn } from '$lib/auth';
	import { error as showError, isBusy } from '$lib/toast.svelte';
	import Toasts from '$lib/components/Toasts.svelte';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
	import ConfirmDangerousDialog from '$lib/components/ConfirmDangerousDialog.svelte';
	import UnlockFsDialog from '$lib/components/UnlockFsDialog.svelte';
	import ReconnectSpinner from '$lib/components/ReconnectSpinner.svelte';
	import ClassicSidebarNav from '$lib/components/ClassicSidebarNav.svelte';
	import IconSidebarNav from '$lib/components/IconSidebarNav.svelte';
	import LauncherSidebarNav from '$lib/components/LauncherSidebarNav.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { AuthResult } from '$lib/rpc';
	import type { BootStatus, BootPhase, SystemStatus } from '$lib/types';
	import favicon from '$lib/assets/favicon.svg';
	import logoLight from '$lib/assets/nasty.svg';
	import logoDark from '$lib/assets/nasty-white.svg';
	import { uiPrefs } from '$lib/uiPrefs.svelte';
	import {
		activeNavigationGroup,
		currentNavigationItem,
		NAVIGATION_CONTEXT,
		navigationForMode,
		resolveNavigation,
		searchNavigation,
		type NavEntry,
		type NavMode
	} from '$lib/navigation';
	import '../app.css';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import {
		Settings,
		RefreshCw,
		Power,
		RotateCcw,
		PowerOff,
		LogOut,
		User,
		Sun,
		Moon,
		PanelLeftClose,
		PanelLeftOpen,
		Bug,
		CircleHelp,
		ExternalLink,
		MessageCircle,
		Code2,
		Search,
		AlertTriangle,
		EyeOff,
	} from '@lucide/svelte';
	import { goto } from '$app/navigation';
	import { refreshState } from '$lib/refresh.svelte';
	import { rebootState } from '$lib/reboot.svelte';
	import { rollbackState, confirmRollback, loadPendingRollback } from '$lib/rollbackState.svelte';
	import { tempUnit } from '$lib/temperature.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';
	import { theme } from '$lib/theme.svelte';
	import { terminalStatus } from '$lib/terminalStatus.svelte';

	let { children } = $props();
	let connected = $state(false);
	let authInfo: AuthResult | null = $state(null);

	// Debug helper for the reconnect / power-cycle state machine.
	// Mirrors `NastyClient.debug` in `rpc.ts` — both are gated on the
	// same localStorage flag (`localStorage.setItem('nasty-debug', '1')`
	// then reload). Used to trace the path through `onDisconnect` →
	// 800ms timer → `reconnecting = true` → `onReconnect`, which is
	// load-bearing for the "Reconnecting…" spinner appearing during
	// reboots. Cached at module load — set the flag then reload.
	const _uiDebug =
		typeof localStorage !== 'undefined'
		&& localStorage.getItem('nasty-debug') === '1';
	function dbg(...args: unknown[]) {
		if (_uiDebug) console.debug('[ui]', ...args);
	}

	// Login form
	let showLogin = $state(false);
	let loginUser = $state('admin');
	let loginPass = $state('');
	let loginError = $state('');
	let ssoEnabled = $state(false);
	// Whether the engine reports at least one user has registered a
	// WebAuthn credential. Gates the "Sign in with security key"
	// button on top of the browser-capability check — on a fresh
	// install with no keys yet, clicking the button just fails at
	// "no credentials for user", so hide it until there's something
	// to sign in with. Populated by /api/auth/webauthn/available
	// (unauthenticated, parallel to /api/auth/oidc/available).
	let webauthnHasCredentials = $state(false);

	// Boot status — populated by polling /api/boot_status. While
	// `overall === 'booting'` we show a TrueNAS-style overlay
	// instead of the login form because the engine isn't yet able
	// to authenticate anyone. After READY the same snapshot lets
	// us surface `ready_with_errors` as a persistent banner so the
	// operator knows a phase failed at boot and can act on it.
	// See engine PRs #300 + #301 (#299 design issue).
	let bootStatus = $state<BootStatus | null>(null);
	/// Dismiss flag for the post-login "boot had errors" banner so
	/// it doesn't reappear every page load until manually fixed.
	const BOOT_BANNER_DISMISSED_KEY = 'nasty:boot_errors_dismissed';
	let bootBannerDismissed = $state(
		typeof localStorage !== 'undefined'
			&& localStorage.getItem(BOOT_BANNER_DISMISSED_KEY) === '1'
	);
	function dismissBootBanner() {
		bootBannerDismissed = true;
		localStorage.setItem(BOOT_BANNER_DISMISSED_KEY, '1');
	}

	/**
	 * Fetch the engine's boot snapshot. Returns null on network
	 * error (engine not even listening yet) so the caller can
	 * decide whether to keep polling or give up.
	 */
	async function fetchBootStatus(): Promise<BootStatus | null> {
		try {
			const res = await fetch('/api/boot_status');
			if (!res.ok) return null;
			return (await res.json()) as BootStatus;
		} catch {
			return null;
		}
	}

	/**
	 * Poll /api/boot_status until the engine reports a non-booting
	 * overall state, then stop. Updates the `bootStatus` $state
	 * after every poll so the overlay UI stays current. Resolves
	 * with the final snapshot (so callers waiting for "engine is
	 * ready" have something to inspect).
	 */
	async function pollUntilReady(): Promise<BootStatus | null> {
		// 600ms is a fast-enough cadence that phase transitions
		// feel live without hammering the engine. The overlay also
		// renders incremental progress (which phases are Ok vs
		// still Running) so the user sees motion even between
		// polls.
		for (;;) {
			const snap = await fetchBootStatus();
			if (snap) {
				bootStatus = snap;
				if (snap.overall !== 'booting') return snap;
			} else {
				// Couldn't reach the engine. Show a placeholder
				// state so the overlay can render "waiting for
				// engine…" rather than flashing the login form.
				if (!bootStatus) {
					bootStatus = {
						overall: 'booting',
						phases: [],
						process_started_at_unix: Math.floor(Date.now() / 1000),
						ready_at_ms: null,
					};
				}
			}
			await new Promise((r) => setTimeout(r, 600));
		}
	}

	// Consume an OIDC redirect's URL fragment. The fragment used to carry
	// `#nasty_token=…` but the engine now sets the session via httpOnly
	// Set-Cookie, so the fragment is just a flag (`#oidc=1`) for "we just
	// came back from SSO, treat the session as live". `#oidc_error=…` still
	// carries a human-readable failure message.
	function consumeSsoFragment(): boolean {
		if (typeof window === 'undefined' || !window.location.hash) return false;
		const params = new URLSearchParams(window.location.hash.slice(1));
		const oidcOk = params.get('oidc') === '1';
		const err = params.get('oidc_error');
		if (oidcOk) {
			history.replaceState(null, '', window.location.pathname + window.location.search);
			return true;
		}
		if (err) {
			loginError = err;
			history.replaceState(null, '', window.location.pathname + window.location.search);
		}
		return false;
	}

	async function refreshSsoAvailability() {
		try {
			const res = await fetch('/api/auth/oidc/available');
			if (res.ok) {
				const body = await res.json();
				ssoEnabled = !!body.enabled;
			} else {
				ssoEnabled = false;
			}
		} catch { ssoEnabled = false; }
	}

	async function refreshWebauthnAvailability() {
		try {
			const res = await fetch('/api/auth/webauthn/available');
			if (res.ok) {
				const body = await res.json();
				webauthnHasCredentials = !!body.has_credentials;
			} else {
				webauthnHasCredentials = false;
			}
		} catch { webauthnHasCredentials = false; }
	}

	function startSso() {
		window.location.assign('/api/auth/oidc/start');
	}

	// Engine version tracking — used to detect updates during reconnect
	let initialCommit: string | null = null;

	// Power menu
	let powerOpen = $state(false);
	let powering = $state(false);

	// Profile menu
	let profileOpen = $state(false);
	let helpOpen = $state(false);

	// SSH password auth warning
	const SSH_DISMISSED_KEY = 'nasty:ssh_password_auth_dismissed';
	let sshPasswordAuth = $state(false);
	let sshPasswordAuthDismissed = $state(
		typeof localStorage !== 'undefined' && localStorage.getItem(SSH_DISMISSED_KEY) === '1'
	);
	async function checkSshStatus() {
		if (!connected || sshPasswordAuthDismissed) return;
		try {
			const result = await getClient().call<{ password_auth: boolean; keys: string[] }>('system.ssh.status');
			sshPasswordAuth = result.password_auth;
		} catch { /* ignore */ }
	}
	function dismissSshPasswordAuth() {
		sshPasswordAuthDismissed = true;
		sshPasswordAuth = false;
		localStorage.setItem(SSH_DISMISSED_KEY, '1');
	}

	// Config backup warning
	const BACKUP_DISMISSED_KEY = 'nasty:config_backup_dismissed';
	let configBackupMissing = $state(false);
	let configBackupDismissed = $state(
		typeof localStorage !== 'undefined' && localStorage.getItem(BACKUP_DISMISSED_KEY) === '1'
	);
	async function checkConfigBackup() {
		if (!connected || configBackupDismissed) return;
		try {
			const profiles = await getClient().call<{ sources: string[] }[]>('backup.profile.list');
			configBackupMissing = !profiles.some(p => p.sources.some(s => s.includes('/var/lib/nasty')));
		} catch { /* ignore */ }
	}
	function dismissConfigBackup() {
		configBackupDismissed = true;
		configBackupMissing = false;
		localStorage.setItem(BACKUP_DISMISSED_KEY, '1');
	}

	// Forced password change
	let showPasswordChange = $state(false);
	let newPassword = $state('');
	let confirmPassword = $state('');
	let passwordError = $state('');

	// Sidebar collapse — default collapsed on mobile (<768px), expanded on desktop.
	// Persisted in localStorage so the user's choice sticks.
	const SIDEBAR_KEY = 'nasty:sidebar_collapsed';
	let sidebarCollapsed = $state(
		typeof localStorage !== 'undefined'
			? localStorage.getItem(SIDEBAR_KEY) === '1'
				|| (localStorage.getItem(SIDEBAR_KEY) === null && typeof window !== 'undefined' && window.innerWidth < 768)
			: false
	);
	function toggleSidebar() {
		sidebarCollapsed = !sidebarCollapsed;
		localStorage.setItem(SIDEBAR_KEY, sidebarCollapsed ? '1' : '0');
	}

	// Version info (loaded once after connect)
	let sysInfo: { hostname: string; version: string; kernel: string; bcachefs_version: string; bcachefs_commit: string | null; bcachefs_pinned_ref: string | null; bcachefs_recommended_ref: string | null; bcachefs_is_custom: boolean; bcachefs_debug_checks: boolean; kvm_available: boolean; is_virtual: boolean } | null = $state(null);
	setContext(NAVIGATION_CONTEXT, {
		get kvmAvailable() { return sysInfo?.kvm_available === true; }
	});
	// bcachefs "update available": the pin differs from the version this
	// NASty build ships, so a one-click sync is offered. Distinct from
	// "reboot pending" (bcachefs_is_custom), which the restart banner owns.
	const bcachefsUpdateAvail = $derived.by(() => {
		const rec = sysInfo?.bcachefs_recommended_ref;
		return !!rec && rec !== sysInfo?.bcachefs_pinned_ref;
	});
	let clock24h = $state(true);

	// Network rollback countdown — ticks once per second while a rollback is
	// pending so the banner can show "Xs left to keep changes". Auto-clears
	// the local store at deadline; the engine has already reverted by then.
	//
	// `tick` is just a re-run trigger for the $derived; the actual time is
	// read from Date.now() at derivation time so the very first frame after
	// `pending` appears is correct. (Storing a `nowSec` $state initialized
	// at layout mount made the first render show seconds-remaining computed
	// against page-load time — visible as a brief "429s"-style flash before
	// the first interval tick corrected it.)
	let tick = $state(0);
	$effect(() => {
		if (!rollbackState.pending) return;
		const handle = setInterval(() => {
			tick++;
			if (
				rollbackState.pending &&
				Math.floor(Date.now() / 1000) >= rollbackState.pending.revertAtUnix
			) {
				rollbackState.clear();
			}
		}, 1000);
		return () => clearInterval(handle);
	});
	let rollbackSecondsLeft = $derived.by(() => {
		void tick;
		if (!rollbackState.pending) return 0;
		return Math.max(0, rollbackState.pending.revertAtUnix - Math.floor(Date.now() / 1000));
	});

	$effect(() => {
		const _r = sysInfoRefresh.count; // track refresh triggers
		if (connected) {
			getClient().call('system.info').then((info: any) => { sysInfo = info; }).catch(() => {});
			getClient().call('system.settings.get').then((s: any) => {
				clock24h = s.clock_24h ?? true;
				tempUnit.set(s.temp_unit ?? 'celsius');
			}).catch(() => {});
		}
	});

	async function checkAuth() {
		if (!connected) return;
		try {
			// Cookie auth: same-origin fetch sends `nasty_session` automatically.
			const res = await fetch('/api/auth/check');
			if (res.status === 401) {
				resetClient();
				location.reload();
			}
		} catch { /* network error — reconnect spinner handles this */ }
	}

	function checkRebootRequired() {
		if (connected) {
			getClient().call<boolean>('system.reboot_required').then((v) => {
				if (v) rebootState.set(); else rebootState.clear();
			}).catch(() => {});
		}
	}

	// Persistent sidebar status band (#528): poll the aggregated system status
	// (level + headline + in-progress array operations). Null = hide the band
	// (e.g. older engine without the RPC), so it degrades gracefully.
	let systemStatus = $state<SystemStatus | null>(null);
	let statusExpanded = $state(false);
	// Literal Tailwind classes (purge-safe) chosen by level — green Healthy /
	// amber Activity / red Critical, per #528.
	const statusDot = $derived(
		systemStatus?.level === 'critical' ? 'bg-red-500'
		: systemStatus?.level === 'activity' ? 'bg-amber-500'
		: 'bg-green-500'
	);
	const statusText = $derived(
		systemStatus?.level === 'critical' ? 'text-red-400'
		: systemStatus?.level === 'activity' ? 'text-amber-400'
		: 'text-green-400'
	);
	const statusHasDetail = $derived(
		!!systemStatus && (systemStatus.operations.length > 0
			|| systemStatus.critical_count + systemStatus.warning_count > 0)
	);
	function refreshSystemStatus() {
		if (!connected || document.hidden) return;
		getClient().call<SystemStatus>('system.status')
			.then((s) => { systemStatus = s; })
			.catch(() => {});
	}

	$effect(() => {
		if (connected) checkRebootRequired();
	});
	$effect(() => {
		if (connected) refreshSystemStatus();
	});

	// Recover any pending rollback the server is tracking — covers the
	// "user changed mgmt-iface IP and reconnected on the new address"
	// case, where the original session that initiated the change has
	// already been torn down. The txn is still alive server-side; this
	// fetch puts the banner back so the user can confirm.
	$effect(() => {
		if (connected) loadPendingRollback();
	});

	// Clock
	let now = $state(new Date());
	const clockFmt = $derived(new Intl.DateTimeFormat(undefined, {
		hour: '2-digit', minute: '2-digit', second: '2-digit',
		hour12: !clock24h,
	}));

	let reconnecting = $state(false);
	// Debounce timer for the reconnect overlay. WebSocket disconnects of
	// well under a second happen routinely (page navigations, NixOS
	// activations restarting the engine in ~2s, transient network blips
	// behind a Caddy reverse_proxy, etc.) and the WS auto-reconnects fast
	// enough that flashing the full-screen overlay just makes the UI feel
	// flaky. Only show the overlay if the disconnect persists past this
	// threshold; if reconnect happens first we cancel the pending state-
	// change and the user sees nothing.
	let reconnectingTimer: ReturnType<typeof setTimeout> | null = null;
	const RECONNECT_OVERLAY_DELAY_MS = 800;

	// Public guest-share pages (/share/[token]) are the one route group that
	// renders without a session: a recipient who was handed a link has no
	// NASty account. We skip the auth probe + WebSocket entirely and render
	// the page bare (no sidebar, no login gate) — it talks to the engine
	// only through the unauthenticated /api/public/share/* endpoints.
	const isPublicShare = $derived($page.url.pathname.startsWith('/share/'));

	onMount(() => {
		if (isPublicShare) return;
		tryConnect();
		const onReconnect = async () => {
			dbg(`reconnect cb fired — clearing powering=${powering}, reconnecting=${reconnecting}, timer=${reconnectingTimer ? 'armed' : 'none'}`);
			powering = false;
			if (reconnectingTimer) {
				clearTimeout(reconnectingTimer);
				reconnectingTimer = null;
			}
			reconnecting = false;
			// Check if engine was updated while we were disconnected.
			// If the commit changed, the WebUI bundle likely changed too — force reload.
			try {
				const res = await fetch('/health');
				const health = await res.json();
				if (initialCommit && health.commit && health.commit !== initialCommit) {
					console.log(`Engine commit changed: ${initialCommit} → ${health.commit} — reloading`);
					location.reload();
					return;
				}
			} catch { /* health check failed, continue with stale UI */ }
			// Engine binary is the same, but the kernel/bcachefs module may have
			// changed underneath (e.g. a bcachefs-tools-only bump rebuilt the DKMS
			// module without moving the engine commit). The footer reads
			// system.info, which is fetched once on connect and cached client-side
			// — without this trigger it shows the pre-reboot version until the
			// user hits cmd+R.
			sysInfoRefresh.trigger();
		};
		const onDisconnect = () => {
			dbg(`disconnect cb fired — arming ${RECONNECT_OVERLAY_DELAY_MS}ms reconnect-overlay timer (powering=${powering}, prior timer=${reconnectingTimer ? 'rearming' : 'fresh'})`);
			if (reconnectingTimer) clearTimeout(reconnectingTimer);
			reconnectingTimer = setTimeout(() => {
				dbg(`reconnect-overlay timer fired — reconnecting=true (powering=${powering})`);
				reconnecting = true;
				reconnectingTimer = null;
			}, RECONNECT_OVERLAY_DELAY_MS);
		};
		getClient().onReconnect(onReconnect);
		getClient().onDisconnect(onDisconnect);
		const tick = setInterval(() => { now = new Date(); }, 1000);
		const rebootPoll = setInterval(checkRebootRequired, 30_000);
		const statusPoll = setInterval(refreshSystemStatus, 20_000);
		const authPoll = setInterval(checkAuth, 60_000);
		const sshPoll = setInterval(checkSshStatus, 30_000);
		const backupPoll = setInterval(checkConfigBackup, 30_000);
		return () => {
			if (reconnectingTimer) clearTimeout(reconnectingTimer);
			getClient().offReconnect(onReconnect);
			getClient().offDisconnect(onDisconnect);
			getClient().disconnect();
			clearInterval(sshPoll);
			clearInterval(backupPoll);
			clearInterval(tick);
			clearInterval(rebootPoll);
			clearInterval(statusPoll);
			clearInterval(authPoll);
		};
	});

	async function tryConnect() {
		consumeSsoFragment();
		// Check engine boot state before anything else. If the engine
		// is still walking its startup phases (Type=notify + 17-step
		// restoration sequence — see #299), there's no point hitting
		// /api/auth/check yet: at best it'd 502 through Caddy, at
		// worst it'd race a half-initialized auth service. The boot
		// overlay shows live progress instead. Once the engine reports
		// `ready` or `ready_with_errors` we fall through to the normal
		// connect flow.
		const initial = await fetchBootStatus();
		if (initial) {
			bootStatus = initial;
			if (initial.overall === 'booting') {
				await pollUntilReady();
			}
		}
		// Probe the cookie before opening a WS — saves us from the WS auth
		// timeout when the user isn't logged in yet.
		try {
			const probe = await fetch('/api/auth/check');
			if (probe.status !== 200) {
				refreshSsoAvailability();
				refreshWebauthnAvailability();
				showLogin = true;
				return;
			}
		} catch {
			// Engine offline — let the reconnect machinery surface that, but
			// don't block the login form.
			refreshSsoAvailability();
			showLogin = true;
			return;
		}
		try {
			const client = getClient();
			authInfo = await client.connect();
			connected = true;
			showLogin = false;
			checkSshStatus();
			checkConfigBackup();
			// Capture engine commit on first connect for reconnect version check
			if (!initialCommit) {
				try {
					const health = await fetch('/health').then(r => r.json());
					initialCommit = health.commit ?? null;
				} catch { /* ignore */ }
			}
			showPasswordChange = !!authInfo?.must_change_password;
		} catch (e) {
			resetClient();
			refreshSsoAvailability();
			refreshWebauthnAvailability();
			showLogin = true;
			if (e instanceof Error && e.message !== 'WebSocket connection failed') {
				showError('Session expired, please sign in again');
			}
		}
	}

	async function handleLogin() {
		loginError = '';
		try {
			await doLogin(loginUser, loginPass);
			loginPass = '';
			await tryConnect();
		} catch (e) {
			loginError = e instanceof Error ? e.message : 'Login failed';
		}
	}

	// WebAuthn sign-in (issue #289 PR #2). Visible alongside the
	// password form when the browser exposes the WebAuthn API and
	// the operator has typed a username. Same lockout/audit machinery
	// on the server side as the password path, so this isn't an
	// alternative "bypass" — just a credential-only login flow.
	let webauthnPending = $state(false);
	const webauthnLoginSupported = $derived(
		typeof window !== 'undefined'
			&& 'PublicKeyCredential' in window
			&& typeof navigator !== 'undefined'
			&& !!navigator.credentials?.get,
	);

	async function handleWebauthnLogin() {
		loginError = '';
		const username = loginUser.trim();
		if (!username) {
			loginError = 'Enter your username first.';
			return;
		}
		webauthnPending = true;
		try {
			await doLoginWebauthn(username);
			loginPass = '';
			await tryConnect();
		} catch (e) {
			loginError = e instanceof Error ? e.message : 'Sign-in failed';
		} finally {
			webauthnPending = false;
		}
	}

	async function handlePasswordChange() {
		passwordError = '';
		if (newPassword.length < 8) {
			passwordError = 'Password must be at least 8 characters';
			return;
		}
		if (newPassword !== confirmPassword) {
			passwordError = 'Passwords do not match';
			return;
		}
		try {
			await getClient().call('auth.change_password', {
				username: authInfo?.username,
				new_password: newPassword,
			});
			newPassword = '';
			confirmPassword = '';
			// Reconnect so the WebSocket picks up the cleared must_change_password flag
			getClient().disconnect();
			resetClient();
			await tryConnect();
		} catch (e) {
			passwordError = e instanceof Error ? e.message : 'Failed to change password';
		}
	}

	async function handleLogout() {
		// /api/logout revokes the session AND tells the browser to drop the
		// httpOnly cookie. Hitting the WS auth.logout RPC isn't enough on its
		// own because JS can't clear an httpOnly cookie itself.
		await doLogout();
		resetClient();
		connected = false;
		authInfo = null;
		showLogin = true;
	}

	async function handleRestart() {
		powerOpen = false;
		if (!await confirm('Restart NASty?', 'All active connections will be dropped.')) return;
		dbg('handleRestart — powering=true, calling system.reboot');
		powering = true;
		rebootState.clear();
		// Safety net: the spinner relies on `onDisconnect` firing and
		// arming the 800ms overlay timer. If anything in that path
		// silently breaks (we hit this once on a first-reboot post-
		// upgrade — couldn't repro deterministically), the operator
		// stares at "Shutting down…" forever with no motion.
		// Promote `reconnecting` to true after 5s regardless if
		// `powering` is still set; cleared cleanly by `onReconnect`
		// when the engine comes back. Costs nothing on the normal
		// path because `onReconnect` clears both flags long before 5s.
		const safetyTimer = setTimeout(() => {
			if (powering && !reconnecting) {
				console.warn(
					'[nasty] reboot: spinner safety net fired at 5s — onDisconnect path did not arm the reconnect overlay. Enable nasty-debug for a state-machine trace.'
				);
				reconnecting = true;
			}
		}, 5000);
		try { await getClient().call('system.reboot'); } catch { /* expected — engine dies */ }
		// Don't clear the safety timer on RPC completion — the engine
		// returning success here doesn't mean the reboot finished, just
		// that the request was accepted. The timer self-cancels via the
		// `powering` check if `onReconnect` has already cleared things.
		void safetyTimer;
	}

	async function handleShutdown() {
		powerOpen = false;
		if (!await confirm('Shut down NASty?', 'The system will power off. All active connections will be dropped.')) return;
		powering = true;
		try { await getClient().call('system.shutdown'); } catch { /* expected — engine dies */ }
	}

	const nav = $derived.by((): NavEntry[] => resolveNavigation({ kvmAvailable: sysInfo?.kvm_available === true }));

	// ── Nav mode (#588): opt-in "Common" short menu vs the full grouped tree ──
	const NAV_MODE_KEY = 'nasty:nav_mode';
	let navMode: NavMode = $state(
		typeof localStorage !== 'undefined' && localStorage.getItem(NAV_MODE_KEY) === 'common'
			? 'common'
			: 'full'
	);
	function setNavMode(m: NavMode) {
		navMode = m;
		if (typeof localStorage !== 'undefined') localStorage.setItem(NAV_MODE_KEY, m);
	}

	const currentNav = $derived(currentNavigationItem($page.url.pathname, nav));

	// Track which groups are expanded — auto-expand based on active route
	const SIDEBAR_GROUPS_KEY = 'nasty:sidebar_groups';
	function loadExpandedGroups(): Record<string, boolean> {
		if (typeof localStorage === 'undefined') return {};
		try {
			const stored = JSON.parse(localStorage.getItem(SIDEBAR_GROUPS_KEY) || '{}') as Record<string, boolean>;
			return {
				...stored,
				storage: stored.storage ?? stored.Storage ?? false,
				protection: stored.protection ?? stored.Protection ?? false,
				compute: stored.compute ?? stored.Compute ?? false,
				system: stored.system ?? stored.System ?? false
			};
		} catch {
			return {};
		}
	}
	let expandedGroups: Record<string, boolean> = $state(loadExpandedGroups());

	function toggleGroup(id: string) {
		expandedGroups[id] = !expandedGroups[id];
		if (typeof localStorage !== 'undefined') {
			localStorage.setItem(SIDEBAR_GROUPS_KEY, JSON.stringify(expandedGroups));
		}
	}

	const activeGroup = $derived(activeNavigationGroup($page.url.pathname, nav));

	// ── Sidebar search ──────────────────────────────
	let sidebarSearch = $state('');
	const searchMatches = $derived(searchNavigation(nav, sidebarSearch));
	const isSearching = $derived(sidebarSearch.trim().length > 0);

	// While searching, render the full grouped tree so search can reach every page;
	// otherwise honor the chosen nav mode (Full grouped tree vs Common short-list).
	const renderNav = $derived<NavEntry[]>(isSearching ? nav : navigationForMode(nav, navMode));
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<title>{sysInfo?.hostname ? `${sysInfo.hostname} — NASty` : 'NASty'}</title>
</svelte:head>

<Toasts />
<ConfirmDialog />
<ConfirmDangerousDialog />
<UnlockFsDialog />

{#if isPublicShare}
	<!--
		Guest share landing — no sidebar, no login, no engine WebSocket.
		The page owns its own chrome and uses only public endpoints.
	-->
	{@render children()}
{:else if bootStatus && bootStatus.overall === 'booting'}
	<!--
		Engine is mid-startup. Show the per-phase checklist so the
		operator sees motion and can spot whether a specific phase
		is stuck rather than staring at a generic spinner. This is
		also the only thing rendered while the engine isn't yet
		accepting auth — login is meaningless here.
	-->
	<div class="flex min-h-screen items-center justify-center bg-background p-6">
		<div class="w-full max-w-lg rounded-xl border border-border bg-card p-8">
			<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="mb-4 h-32 mx-auto" />
			<h1 class="text-center text-lg font-semibold">NASty is starting up…</h1>
			<p class="mt-1 text-center text-sm text-muted-foreground">
				Waiting for the engine to finish restoring system state. Login will appear automatically once it's ready.
			</p>
			<ul class="mt-6 divide-y divide-border/40">
				{#each bootStatus.phases as phase (phase.name)}
					<li class="flex items-center justify-between gap-3 py-2 text-sm">
						<span class="flex items-center gap-2 min-w-0">
							{#if phase.state === 'pending'}
								<span class="inline-block h-2 w-2 shrink-0 rounded-full bg-muted-foreground/40" aria-label="pending"></span>
							{:else if phase.state === 'running'}
								<span class="inline-block h-2 w-2 shrink-0 animate-pulse rounded-full bg-blue-400" aria-label="running"></span>
							{:else if phase.state === 'ok'}
								<span class="inline-block h-2 w-2 shrink-0 rounded-full bg-emerald-500" aria-label="ok"></span>
							{:else}
								<span class="inline-block h-2 w-2 shrink-0 rounded-full bg-amber-500" aria-label="failed"></span>
							{/if}
							<code class="truncate font-mono text-xs text-muted-foreground">{phase.name}</code>
						</span>
						<span class="text-xs text-muted-foreground tabular-nums">
							{#if phase.state === 'pending'}—{/if}
							{#if phase.state === 'running'}…{/if}
							{#if phase.duration_ms != null}
								{(phase.duration_ms / 1000).toFixed(1)}s
							{/if}
						</span>
					</li>
				{:else}
					<li class="py-2 text-center text-sm text-muted-foreground">Connecting to engine…</li>
				{/each}
			</ul>
		</div>
	</div>
{:else if showLogin}
	<div class="flex min-h-screen items-center justify-center">
		<div class="w-[340px] rounded-xl border border-border bg-card p-8">
			<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="mb-4 h-48 mx-auto" />
			<p class="mb-6 text-sm text-muted-foreground">Sign in to manage your storage</p>
			{#if loginError}
				<p class="mb-4 text-sm text-destructive">{loginError}</p>
			{/if}
			<form onsubmit={(e) => { e.preventDefault(); handleLogin(); }}>
				<div class="mb-4">
					<Label for="username">Username</Label>
					<Input id="username" bind:value={loginUser} autocomplete="username" class="mt-1" />
				</div>
				<div class="mb-4">
					<Label for="password">Password</Label>
					<Input id="password" type="password" bind:value={loginPass} autocomplete="current-password" class="mt-1" />
				</div>
				<Button type="submit" class="w-full" disabled={webauthnPending}>Sign In</Button>
			</form>
			{#if (webauthnLoginSupported && webauthnHasCredentials) || ssoEnabled}
				<div class="my-4 flex items-center gap-3 text-xs text-muted-foreground">
					<div class="h-px flex-1 bg-border"></div>
					<span>or</span>
					<div class="h-px flex-1 bg-border"></div>
				</div>
			{/if}
			{#if webauthnLoginSupported && webauthnHasCredentials}
				<Button
					type="button"
					variant="outline"
					class="w-full {ssoEnabled ? 'mb-2' : ''}"
					disabled={webauthnPending}
					onclick={handleWebauthnLogin}
				>
					{#if webauthnPending}Tap your security key…{:else}Sign in with security key{/if}
				</Button>
			{/if}
			{#if ssoEnabled}
				<Button type="button" variant="outline" class="w-full" onclick={startSso}>Sign in with SSO</Button>
			{/if}
		</div>
	</div>
{:else if showPasswordChange}
	<div class="flex min-h-screen items-center justify-center">
		<div class="w-[380px] rounded-xl border border-border bg-card p-8">
			<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="mb-4 h-48 mx-auto" />
			<h2 class="mb-2 text-lg font-semibold">Change your password</h2>
			<p class="mb-6 text-sm text-muted-foreground">The default password must be changed before continuing.</p>
			{#if passwordError}
				<p class="mb-4 text-sm text-destructive">{passwordError}</p>
			{/if}
			<form onsubmit={(e) => { e.preventDefault(); handlePasswordChange(); }}>
				<div class="mb-4">
					<Label for="new-password">New password</Label>
					<Input id="new-password" type="password" bind:value={newPassword} autocomplete="new-password" class="mt-1" />
				</div>
				<div class="mb-4">
					<Label for="confirm-password">Confirm password</Label>
					<Input id="confirm-password" type="password" bind:value={confirmPassword} autocomplete="new-password" class="mt-1" />
				</div>
				<Button type="submit" class="w-full">Set password</Button>
			</form>
		</div>
	</div>
{:else}
	<div class="relative flex h-screen overflow-hidden">
		<!-- Sidebar -->
		<aside class="flex {sidebarCollapsed ? (uiPrefs.menuStyle === 'icons' ? 'w-[72px]' : 'w-[52px]') : 'w-[200px]'} shrink-0 flex-col border-r border-border bg-card transition-[width] duration-200">
			<!-- Logo / collapse toggle -->
			{#if sidebarCollapsed}
				<div class="shrink-0 border-b border-border flex items-center justify-center py-3">
					<button onclick={toggleSidebar} class="text-muted-foreground hover:text-foreground transition-colors" title="Expand sidebar">
						<PanelLeftOpen size={18} />
					</button>
				</div>
			{:else if uiPrefs.logoHidden}
				<!-- Logo hidden (restore via Settings → Appearance). Slim bar keeps
				     the collapse toggle reachable and reclaims the logo's height. -->
				<div class="shrink-0 border-b border-border px-2 py-1.5 flex justify-end">
					<button onclick={toggleSidebar} class="text-muted-foreground/50 hover:text-foreground transition-colors" title="Collapse sidebar">
						<PanelLeftClose size={15} />
					</button>
				</div>
			{:else}
				<div class="shrink-0 border-b border-border px-4 py-4 relative">
					<a href="https://github.com/nasty-project" target="_blank" rel="noopener noreferrer">
					<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="h-40" />
				</a>
					<button onclick={() => uiPrefs.setLogoHidden(true)} class="absolute top-2 right-7 text-muted-foreground/50 hover:text-foreground transition-colors" title="Hide logo (restore in Settings → Appearance)">
						<EyeOff size={15} />
					</button>
					<button onclick={toggleSidebar} class="absolute top-2 right-2 text-muted-foreground/50 hover:text-foreground transition-colors" title="Collapse sidebar">
						<PanelLeftClose size={15} />
					</button>
				</div>
			{/if}

			<!-- System status band (#528): always-visible health / activity / critical -->
			{#if systemStatus}
				{#if sidebarCollapsed}
					<div class="shrink-0 border-b border-border flex items-center justify-center py-2.5" title={systemStatus.headline}>
						<span class="h-2.5 w-2.5 rounded-full {statusDot}"></span>
					</div>
				{:else}
					<div class="shrink-0 border-b border-border px-2 py-1.5">
						<button
							class="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors {statusHasDetail ? 'cursor-pointer hover:bg-accent/50' : 'cursor-default'}"
							onclick={() => { if (statusHasDetail) statusExpanded = !statusExpanded; }}
						>
							<span class="h-2 w-2 shrink-0 rounded-full {statusDot}"></span>
							<span class="min-w-0 flex-1 truncate font-medium {statusText}">{systemStatus.headline}</span>
							{#if statusHasDetail}
								<span class="shrink-0 text-muted-foreground/60">{statusExpanded ? '−' : '+'}</span>
							{/if}
						</button>
						{#if statusExpanded && statusHasDetail}
							<div class="mt-1 space-y-1 px-2 pb-1 text-[0.7rem] text-muted-foreground">
								{#each systemStatus.operations as op}
									<a href="/operations" class="block truncate hover:text-foreground">• {op.detail}</a>
								{/each}
								{#if systemStatus.critical_count + systemStatus.warning_count > 0}
									<a href="/alerts" class="block hover:text-foreground">
										{systemStatus.critical_count} critical · {systemStatus.warning_count} warning
									</a>
								{/if}
							</div>
						{/if}
					</div>
				{/if}
			{/if}

			<!-- Search bar -->
			{#if !sidebarCollapsed && uiPrefs.menuStyle !== 'launcher'}
				<div class="shrink-0 px-2 pt-2 relative">
					<div class="relative">
						<Search size={13} class="absolute left-2.5 top-1/2 -translate-y-1/2 text-muted-foreground/50" />
						<input
							type="text"
							bind:value={sidebarSearch}
							placeholder="Search..."
							class="w-full rounded-md border border-border bg-transparent pl-8 pr-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground/40 focus:outline-none focus:ring-1 focus:ring-ring"
						/>
					</div>
				</div>
			{/if}

			<!-- Nav mode toggle (#588): Common short-list vs Full grouped tree — hidden while searching -->
			{#if !sidebarCollapsed && !isSearching && uiPrefs.menuStyle !== 'launcher'}
				<div class="shrink-0 px-2 pt-2">
					<div class="flex rounded-md border border-border p-0.5 text-[0.7rem]">
						<button
							onclick={() => setNavMode('common')}
							class="flex-1 rounded px-2 py-1 transition-colors {navMode === 'common' ? 'bg-accent text-foreground font-medium' : 'text-muted-foreground hover:text-foreground'}"
							title="Show a short list of the most-used pages"
						>Common</button>
						<button
							onclick={() => setNavMode('full')}
							class="flex-1 rounded px-2 py-1 transition-colors {navMode === 'full' ? 'bg-accent text-foreground font-medium' : 'text-muted-foreground hover:text-foreground'}"
							title="Show every page, grouped"
						>Full</button>
					</div>
				</div>
			{/if}

			<!-- Nav — scrollable -->
			{#if uiPrefs.menuStyle === 'launcher'}
				<LauncherSidebarNav collapsed={sidebarCollapsed} active={$page.url.pathname === '/menu'} />
			{:else if uiPrefs.menuStyle === 'icons'}
				<IconSidebarNav
					entries={renderNav}
					fullEntries={nav}
					mode={navMode}
					currentHref={currentNav.href}
					activeGroupId={activeGroup}
					collapsed={sidebarCollapsed}
					{isSearching}
					{searchMatches}
					onNavigate={() => { sidebarSearch = ''; }}
				/>
			{:else}
				<ClassicSidebarNav
					entries={renderNav}
					currentHref={currentNav.href}
					activeGroupId={activeGroup}
					{expandedGroups}
					collapsed={sidebarCollapsed}
					{isSearching}
					{searchMatches}
					onToggleGroup={toggleGroup}
					onNavigate={() => { sidebarSearch = ''; }}
				/>
			{/if}

			{#if !sidebarCollapsed}
				<!-- Clock — centered above the footer separator -->
				<div class="shrink-0 px-4 pt-2 pb-1 text-center font-mono text-sm tabular-nums text-muted-foreground/60">{clockFmt.format(now)}</div>

				<!-- Footer — version info -->
				<div class="shrink-0 border-t border-border px-4 py-3">
					{#if sysInfo}
						<div class="flex items-center justify-between">
							<a href="/licenses" class="text-[0.68rem] text-muted-foreground/50 hover:text-muted-foreground transition-colors">NASty</a>
							<span class="text-[0.68rem] font-mono text-muted-foreground/70">{sysInfo.version}</span>
						</div>
						<div class="flex items-center justify-between mt-0.5">
							<span class="text-[0.68rem] text-muted-foreground/50">kernel</span>
							<span class="text-[0.68rem] font-mono text-muted-foreground/70 truncate ml-2 text-right" title={sysInfo.kernel}>{sysInfo.kernel}</span>
						</div>
						{@const bcachefsCommit = sysInfo.bcachefs_is_custom && sysInfo.bcachefs_commit && !/^v\d/.test(sysInfo.bcachefs_pinned_ref ?? '') ? sysInfo.bcachefs_commit : null}
						{#if bcachefsCommit}
							<div class="mt-0.5">
								<span class="text-[0.68rem] text-muted-foreground/50">bcachefs</span>
								<div class="text-[0.68rem] font-mono text-muted-foreground/70">{sysInfo.bcachefs_version} @ {bcachefsCommit}</div>
							</div>
						{:else}
							<div class="flex items-center justify-between mt-0.5">
								<span class="text-[0.68rem] text-muted-foreground/50">bcachefs</span>
								<span class="text-[0.68rem] font-mono text-muted-foreground/70">{sysInfo.bcachefs_version}</span>
							</div>
						{/if}
					{:else}
						<div class="text-[0.68rem] text-muted-foreground/40">Loading…</div>
					{/if}
				</div>
			{/if}
		</aside>

		<!-- Right side: top bar + content -->
		<div class="flex flex-1 flex-col overflow-hidden">
			<!-- Top bar -->
			<header class="relative flex h-14 shrink-0 items-center justify-between border-b border-border bg-card px-6">
				<div class="flex items-center gap-2 text-base">
					{#if currentNav.icon}{@const NavIcon = currentNav.icon}<NavIcon size={17} class="text-muted-foreground" />{/if}
					<span class="font-medium">{currentNav.label}</span>
					{#if currentNav.href === '/terminal' && terminalStatus.value !== 'idle'}
						<span class="text-[0.65rem] uppercase tracking-wide {
							terminalStatus.value === 'connected' ? 'text-green-400' :
							terminalStatus.value === 'connecting' ? 'text-amber-500' : 'text-muted-foreground/50'
						}">{terminalStatus.value}</span>
					{/if}
				</div>

				<!-- Centered banners — reload and reboot notifications -->
				<div class="absolute left-1/2 -translate-x-1/2 flex items-center gap-3">
					{#if refreshState.needed}
						<button
							onclick={() => location.reload()}
							class="flex items-center gap-2 rounded-md border-2 border-amber-500/70 px-3 py-1.5 text-sm text-amber-400 transition-all animate-pulse hover:animate-none hover:bg-amber-500/10 hover:border-amber-400 hover:shadow-[0_0_16px_rgba(251,191,36,0.5)] active:shadow-none"
						>
							<RefreshCw size={15} />
							Reload required — click to refresh
						</button>
					{/if}
					{#if rebootState.needed}
						<button
							onclick={handleRestart}
							class="flex items-center gap-2 rounded-md border-2 border-amber-500/70 px-3 py-1.5 text-sm text-amber-400 transition-all animate-pulse hover:animate-none hover:bg-amber-500/10 hover:border-amber-400 hover:shadow-[0_0_16px_rgba(251,191,36,0.5)] active:shadow-none"
						>
							<RotateCcw size={15} />
							Kernel/driver update — click to restart
						</button>
					{/if}
					<!-- bcachefs chip. Two distinct states, two distinct owners:
					     - "update available" (pinned ref differs from the version
					       this NASty build ships) → THIS chip, blue + arrow,
					       click to switch the pin.
					     - "reboot pending" (running module differs from the pin)
					       → the amber "Kernel/driver update — click to restart"
					       banner ABOVE, which already fires whenever the
					       kernel-modules closure changes. We don't duplicate that
					       action here; the gear icon is just a passive glance cue.
					     The chip otherwise renders as a quiet status pill (debug
					     build flags). Note the render condition includes the sync
					     case directly — previously the offer was hidden unless a
					     debug flag or pending reboot happened to be set too. -->
					{#if sysInfo && (bcachefsUpdateAvail || sysInfo.bcachefs_is_custom || sysInfo.bcachefs_debug_checks)}
						<a
							href="/update#bcachefs"
							class={bcachefsUpdateAvail
								? 'flex items-center gap-2 rounded-md border-2 border-blue-500/70 px-3 py-1.5 text-sm text-blue-400 no-underline transition-all hover:bg-blue-500/10 hover:border-blue-400 hover:shadow-[0_0_16px_rgba(96,165,250,0.5)]'
								: 'flex items-center gap-2 rounded-md border-2 border-white/15 px-3 py-1.5 text-sm text-muted-foreground/80 no-underline transition-all hover:bg-white/5 hover:border-white/30'}
							title={bcachefsUpdateAvail
								? `bcachefs update available — NASty ships ${sysInfo.bcachefs_recommended_ref} (you're pinned at ${sysInfo.bcachefs_pinned_ref ?? '—'}). Click to switch.`
								: 'bcachefs status — click for details'}
						>
							<span>bcachefs</span>
							{#if bcachefsUpdateAvail}
								<span class="font-mono text-xs">→ {sysInfo.bcachefs_recommended_ref}</span>
							{/if}
							<span class="flex items-center gap-1.5">
								<span title="Reboot pending — running module differs from the pinned version"><Settings size={14} class={sysInfo.bcachefs_is_custom ? 'text-amber-400' : 'text-muted-foreground/30'} /></span>
								<span title="Debug checks enabled in the running module"><Bug size={14} class={sysInfo.bcachefs_debug_checks ? 'text-blue-400' : 'text-muted-foreground/30'} /></span>
							</span>
						</a>
					{/if}
					{#if rollbackState.pending}
						<!-- Pending network rollback. Sticky on every page so the
						     user can confirm even after navigating away from
						     /settings. The engine reverts at revertAtUnix; this
						     banner auto-clears when the countdown hits zero. -->
						<div
							class="flex items-center gap-3 rounded-md border-2 border-amber-500/70 px-3 py-1.5 text-sm text-amber-400 animate-pulse hover:animate-none"
							role="status"
							title={rollbackState.pending.riskReason ?? ''}
						>
							<AlertTriangle size={15} />
							<span class="font-medium">
								{rollbackSecondsLeft}s to keep network change
							</span>
							<button
								onclick={() => confirmRollback()}
								class="rounded border border-amber-400/50 px-2 py-0.5 text-xs hover:bg-amber-500/10 hover:border-amber-400 active:bg-amber-500/20"
							>
								Keep changes
							</button>
						</div>
					{/if}
				</div>

				<div class="flex items-center gap-2.5">
					{#if powering}
						<span class="text-sm text-amber-500">Shutting down…</span>
					{/if}

					<!-- Help menu -->
					<div class="relative">
						<button
							onclick={() => { helpOpen = !helpOpen; profileOpen = false; powerOpen = false; }}
							class="flex items-center rounded-md border-2 border-blue-500/50 px-2.5 py-1.5 text-muted-foreground transition-all hover:bg-accent hover:text-accent-foreground hover:border-blue-400/80 hover:shadow-[0_0_12px_rgba(96,165,250,0.4)] active:shadow-none"
							title="Help & Community"
						>
							<CircleHelp size={15} />
						</button>
						{#if helpOpen}
							<!-- svelte-ignore a11y_no_static_element_interactions -->
							<div class="absolute right-0 top-full mt-2 z-50 w-64 rounded-md border border-border bg-popover p-2 shadow-lg"
								onmouseleave={() => helpOpen = false}>
								<a href="/help" onclick={() => helpOpen = false}
									class="flex items-center gap-2 rounded px-3 py-2 text-sm text-popover-foreground no-underline hover:bg-accent transition-colors">
									<CircleHelp size={15} />
									Glossary
								</a>
								<div class="my-1 border-t border-border"></div>
								<a href="https://github.com/nasty-project" target="_blank" rel="noopener noreferrer"
									class="flex items-center gap-2 rounded px-3 py-2 text-sm text-popover-foreground no-underline hover:bg-accent transition-colors">
									<Code2 size={15} />
									GitHub
									<ExternalLink size={12} class="ml-auto text-muted-foreground" />
								</a>
								<a href="https://webchat.oftc.net/?channels=#bcachefs" target="_blank" rel="noopener noreferrer"
									class="flex items-center gap-2 rounded px-3 py-2 text-sm text-popover-foreground no-underline hover:bg-accent transition-colors">
									<MessageCircle size={15} />
									bcachefs IRC (OFTC)
									<ExternalLink size={12} class="ml-auto text-muted-foreground" />
								</a>
								<a href="https://matrix.to/#/#_oftc_%23bcache:matrix.org" target="_blank" rel="noopener noreferrer"
									class="flex items-center gap-2 rounded px-3 py-2 text-sm text-popover-foreground no-underline hover:bg-accent transition-colors">
									<MessageCircle size={15} />
									bcachefs Matrix
									<ExternalLink size={12} class="ml-auto text-muted-foreground" />
								</a>
								<a href="https://www.reddit.com/r/bcachefs/" target="_blank" rel="noopener noreferrer"
									class="flex items-center gap-2 rounded px-3 py-2 text-sm text-popover-foreground no-underline hover:bg-accent transition-colors">
									<MessageCircle size={15} />
									r/bcachefs
									<ExternalLink size={12} class="ml-auto text-muted-foreground" />
								</a>
							</div>
						{/if}
					</div>

					<!-- Profile button -->
					<div class="relative">
						<button
							onclick={() => { profileOpen = !profileOpen; powerOpen = false; }}
							class="flex items-center gap-2 rounded-md border-2 border-blue-500/50 px-3 py-1.5 text-sm text-muted-foreground transition-all hover:bg-accent hover:text-accent-foreground hover:border-blue-400/80 hover:shadow-[0_0_12px_rgba(96,165,250,0.4)] active:shadow-none"
						>
							<User size={15} />
							{authInfo?.username ?? ''}
						</button>
						{#if profileOpen}
							<!-- svelte-ignore a11y_no_static_element_interactions -->
							<div
								class="absolute right-0 top-10 z-50 min-w-[160px] rounded-lg border border-border bg-card shadow-lg"
								onmouseleave={() => profileOpen = false}
							>
								{#if authInfo}
									<div class="border-b border-border px-4 py-2.5">
										<div class="text-sm font-medium">{authInfo.username}</div>
										<div class="text-xs text-muted-foreground uppercase">{authInfo.role}</div>
									</div>
								{/if}
								<button
									onclick={() => theme.toggle()}
									class="flex w-full items-center gap-2.5 px-4 py-2.5 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground rounded-t-lg"
								>
									{#if theme.isDark}
										<Sun size={14} />
										Light mode
									{:else}
										<Moon size={14} />
										Dark mode
									{/if}
								</button>
								<div class="border-t border-border"></div>
								<button
									onclick={handleLogout}
									class="flex w-full items-center gap-2.5 px-4 py-2.5 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground rounded-b-lg"
								>
									<LogOut size={14} />
									Sign Out
								</button>
							</div>
						{/if}
					</div>

					<!-- Power button -->
					<div class="relative">
						<button
							onclick={() => { powerOpen = !powerOpen; profileOpen = false; }}
							disabled={powering}
							title="Power"
							aria-label="Power"
							class="flex items-center rounded-md border-2 border-blue-500/50 px-2.5 py-1.5 text-muted-foreground transition-all hover:bg-accent hover:text-accent-foreground hover:border-blue-400/80 hover:shadow-[0_0_12px_rgba(96,165,250,0.4)] active:shadow-none disabled:opacity-50"
						>
							<Power size={15} />
						</button>
						{#if powerOpen}
							<!-- svelte-ignore a11y_no_static_element_interactions -->
							<div
								class="absolute right-0 top-10 z-50 min-w-[160px] rounded-lg border border-border bg-card shadow-lg"
								onmouseleave={() => powerOpen = false}
							>
								<button
									onclick={handleRestart}
									class="flex w-full items-center gap-2.5 px-4 py-2.5 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground rounded-t-lg"
								>
									<RotateCcw size={14} />
									Restart
								</button>
								<div class="border-t border-border"></div>
								<button
									onclick={handleShutdown}
									class="flex w-full items-center gap-2.5 px-4 py-2.5 text-sm text-destructive transition-colors hover:bg-destructive/10 rounded-b-lg"
								>
									<PowerOff size={14} />
									Shut Down
								</button>
							</div>
						{/if}
					</div>
				</div>
			</header>

			<!-- Page content -->
			<main class="flex-1 overflow-y-auto {currentNav.href === '/terminal' ? 'p-2' : 'p-6'}">
				{#if isBusy()}
					<div class="fixed top-0 left-0 right-0 z-50 h-0.5 bg-primary/20">
						<div class="h-full w-1/3 bg-primary animate-[indeterminate_1.5s_ease-in-out_infinite]"></div>
					</div>
				{/if}
				{#if sshPasswordAuth}
					<div class="mb-4 flex items-center gap-3 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-sm text-amber-400">
						<span class="flex-1">SSH password authentication is enabled — disable it for better security.</span>
						<Button size="sm" onclick={() => goto('/services?configure=ssh')}>Configure SSH</Button>
						<button onclick={dismissSshPasswordAuth} class="text-xs text-amber-400/60 hover:text-amber-400 shrink-0">dismiss</button>
					</div>
				{/if}
				{#if configBackupMissing}
					<div class="mb-4 flex items-center gap-3 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-sm text-amber-400">
						<span class="flex-1">NASty configuration is not backed up.</span>
						<Button size="sm" onclick={() => goto('/backups?create=config')}>Create Backup</Button>
						<button onclick={dismissConfigBackup} class="text-xs text-amber-400/60 hover:text-amber-400 shrink-0">dismiss</button>
					</div>
				{/if}
				{#if bootStatus && bootStatus.overall === 'ready_with_errors' && !bootBannerDismissed}
					{@const failed = bootStatus.phases.filter((p: BootPhase) => p.state === 'failed')}
					<div class="mb-4 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-sm text-amber-400">
						<div class="flex items-start gap-3">
							<span class="flex-1">
								<strong>{failed.length} boot phase{failed.length === 1 ? '' : 's'} didn't complete cleanly.</strong>
								The engine is up, but the listed subsystems weren't restored. Check Logs for details and retry the affected operation (e.g. mount a filesystem, restart a protocol) once you've addressed the cause.
							</span>
							<button onclick={dismissBootBanner} class="text-xs text-amber-400/60 hover:text-amber-400 shrink-0">dismiss</button>
						</div>
						<ul class="mt-2 ml-1 space-y-0.5">
							{#each failed as p (p.name)}
								<li class="font-mono text-xs">
									<span class="text-amber-300">{p.name}</span>
									{#if p.error}<span class="text-muted-foreground"> — {p.error}</span>{/if}
								</li>
							{/each}
						</ul>
					</div>
				{/if}
				{#if !connected}
					<p class="text-muted-foreground">Connecting to engine...</p>
				{:else}
					{@render children()}
				{/if}
			</main>
		</div>

		{#if reconnecting}
			<div class="absolute inset-0 z-50 flex items-center justify-center bg-background/60 backdrop-blur-[2px]">
				<ReconnectSpinner />
			</div>
		{/if}
	</div>
{/if}
