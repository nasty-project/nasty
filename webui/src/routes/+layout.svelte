<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/stores';
	import { getClient, resetClient } from '$lib/client';
	import { getToken, clearToken, login as doLogin } from '$lib/auth';
	import { error as showError, isBusy } from '$lib/toast.svelte';
	import Toasts from '$lib/components/Toasts.svelte';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
	import ConfirmDangerousDialog from '$lib/components/ConfirmDangerousDialog.svelte';
	import ReconnectSpinner from '$lib/components/ReconnectSpinner.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { AuthResult } from '$lib/rpc';
	import favicon from '$lib/assets/favicon.svg';
	import logoLight from '$lib/assets/nasty.svg';
	import logoDark from '$lib/assets/nasty-white.svg';
	import '../app.css';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import {
		LayoutDashboard,
		Database,
		Layers,
		Share2,
		HardDrive,
		Archive,
		Bell,
		Settings,
		RefreshCw,
		Terminal,
		ShieldCheck,
		Network,
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
		Monitor,
		Box,
		FolderOpen,
		CircleHelp,
		ExternalLink,
		MessageCircle,
		Code2,
		ChevronRight,
		Shield,
		Flame,
		Lock,
		Zap,
		Globe,
		Cpu,
		Server,
		Wrench,
		ScrollText,
		Search,
	} from '@lucide/svelte';
	import { goto } from '$app/navigation';
	import { refreshState } from '$lib/refresh.svelte';
	import { rebootState } from '$lib/reboot.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';
	import { theme } from '$lib/theme.svelte';
	import { terminalStatus } from '$lib/terminalStatus.svelte';

	let { children } = $props();
	let connected = $state(false);
	let authInfo: AuthResult | null = $state(null);

	// Login form
	let showLogin = $state(false);
	let loginUser = $state('admin');
	let loginPass = $state('');
	let loginError = $state('');

	// Engine version tracking — used to detect updates during reconnect
	let initialCommit: string | null = null;

	// Power menu
	let powerOpen = $state(false);
	let powering = $state(false);

	// Profile menu
	let profileOpen = $state(false);
	let helpOpen = $state(false);

	// SSH password auth warning
	let sshPasswordAuth = $state(false);
	async function checkSshStatus() {
		if (!connected) return;
		try {
			const result = await getClient().call<{ password_auth: boolean; keys: string[] }>('system.ssh.status');
			sshPasswordAuth = result.password_auth;
		} catch { /* ignore */ }
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
	let sysInfo: { hostname: string; version: string; kernel: string; bcachefs_version: string; bcachefs_commit: string | null; bcachefs_pinned_ref: string | null; bcachefs_is_custom: boolean; bcachefs_debug_checks: boolean; kvm_available: boolean; is_virtual: boolean } | null = $state(null);
	let clock24h = $state(true);

	$effect(() => {
		const _r = sysInfoRefresh.count; // track refresh triggers
		if (connected) {
			getClient().call('system.info').then((info: any) => { sysInfo = info; }).catch(() => {});
			getClient().call('system.settings.get').then((s: any) => { clock24h = s.clock_24h ?? true; }).catch(() => {});
		}
	});

	async function checkAuth() {
		const token = getToken();
		if (!token || !connected) return;
		try {
			const res = await fetch('/api/auth/check', {
				headers: { 'Authorization': `Bearer ${token}` },
			});
			if (res.status === 401) {
				clearToken();
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

	$effect(() => {
		if (connected) checkRebootRequired();
	});

	// Clock
	let now = $state(new Date());
	const clockFmt = $derived(new Intl.DateTimeFormat(undefined, {
		hour: '2-digit', minute: '2-digit', second: '2-digit',
		hour12: !clock24h,
	}));

	let reconnecting = $state(false);

	onMount(() => {
		tryConnect();
		const onReconnect = async () => {
			powering = false;
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
		};
		const onDisconnect = () => { reconnecting = true; };
		getClient().onReconnect(onReconnect);
		getClient().onDisconnect(onDisconnect);
		const tick = setInterval(() => { now = new Date(); }, 1000);
		const rebootPoll = setInterval(checkRebootRequired, 30_000);
		const authPoll = setInterval(checkAuth, 60_000);
		const sshPoll = setInterval(checkSshStatus, 30_000);
		return () => {
			getClient().offReconnect(onReconnect);
			getClient().offDisconnect(onDisconnect);
			getClient().disconnect();
			clearInterval(sshPoll);
			clearInterval(tick);
			clearInterval(rebootPoll);
			clearInterval(authPoll);
		};
	});

	async function tryConnect() {
		const token = getToken();
		if (!token) { showLogin = true; return; }
		try {
			const client = getClient();
			authInfo = await client.connect(token);
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
			clearToken();
			resetClient();
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
		try { await getClient().call('auth.logout'); } catch { /* ignore */ }
		clearToken();
		resetClient();
		connected = false;
		authInfo = null;
		showLogin = true;
	}

	async function handleRestart() {
		powerOpen = false;
		if (!await confirm('Restart NASty?', 'All active connections will be dropped.')) return;
		powering = true;
		rebootState.clear();
		try { await getClient().call('system.reboot'); } catch { /* expected — engine dies */ }
	}

	async function handleShutdown() {
		powerOpen = false;
		if (!await confirm('Shut down NASty?', 'The system will power off. All active connections will be dropped.')) return;
		powering = true;
		try { await getClient().call('system.shutdown'); } catch { /* expected — engine dies */ }
	}

	type NavItem = { href: string; label: string; icon: any };
	type NavGroup = { label: string; icon: any; children: NavItem[] };
	type NavEntry = NavItem | NavGroup;
	function isGroup(e: NavEntry): e is NavGroup { return 'children' in e; }

	const nav = $derived.by((): NavEntry[] => {
		const computeChildren: NavItem[] = [{ href: '/apps', label: 'Apps', icon: Box }];
		if (sysInfo?.kvm_available) {
			computeChildren.unshift({ href: '/vms', label: 'VMs', icon: Monitor });
		}
		return [
			{ href: '/', label: 'Dashboard', icon: LayoutDashboard },
			{ label: 'Storage', icon: Database, children: [
				{ href: '/filesystems', label: 'Filesystems', icon: Database },
				{ href: '/subvolumes', label: 'Subvolumes', icon: Layers },
				{ href: '/disks',      label: 'Disks',       icon: HardDrive },
				{ href: '/files',      label: 'Files',       icon: FolderOpen },
			]},
			{ href: '/sharing', label: 'Sharing', icon: Share2 },
			{ label: 'Protection', icon: Shield, children: [
				{ href: '/backups',  label: 'Backups',  icon: Archive },
				{ href: '/alerts',   label: 'Alerts',   icon: Bell },
				{ href: '/firewall', label: 'Firewall', icon: Flame },
				{ href: '/tls',      label: 'TLS',      icon: Lock },
				{ href: '/ups',      label: 'UPS',      icon: Zap },
				{ href: '/vpn',      label: 'VPN',      icon: Globe },
			]},
			{ label: 'Compute', icon: Cpu, children: computeChildren },
			{ href: '/terminal', label: 'Terminal', icon: Terminal },
			{ label: 'System', icon: Wrench, children: [
				{ href: '/services', label: 'Services',       icon: Server },
				{ href: '/logs',     label: 'Logs',           icon: ScrollText },
				{ href: '/update',   label: 'Update',         icon: RefreshCw },
				{ href: '/users',    label: 'Access Control', icon: ShieldCheck },
				{ href: '/settings', label: 'Settings',       icon: Settings },
			]},
		];
	});

	// Flatten all nav items for matching
	const allNavItems = $derived.by((): NavItem[] => {
		const items: NavItem[] = [];
		for (const entry of nav) {
			if (isGroup(entry)) items.push(...entry.children);
			else items.push(entry);
		}
		return items;
	});

	// Derive current nav entry from path
	const currentNav = $derived.by(() => {
		const path = $page.url.pathname;
		return [...allNavItems].sort((a, b) => b.href.length - a.href.length)
			.find(n => path === n.href || (n.href !== '/' && path.startsWith(n.href))) ?? allNavItems[0];
	});

	// Track which groups are expanded — auto-expand based on active route
	const SIDEBAR_GROUPS_KEY = 'nasty:sidebar_groups';
	let expandedGroups: Record<string, boolean> = $state(
		typeof localStorage !== 'undefined'
			? JSON.parse(localStorage.getItem(SIDEBAR_GROUPS_KEY) || '{}')
			: {}
	);

	function toggleGroup(label: string) {
		expandedGroups[label] = !expandedGroups[label];
		localStorage.setItem(SIDEBAR_GROUPS_KEY, JSON.stringify(expandedGroups));
	}

	// Auto-expand the group containing the active route
	const activeGroup = $derived.by((): string | null => {
		const path = $page.url.pathname;
		for (const entry of nav) {
			if (isGroup(entry) && entry.children.some(c => path === c.href || (c.href !== '/' && path.startsWith(c.href)))) {
				return entry.label;
			}
		}
		return null;
	});

	// ── Sidebar search ──────────────────────────────
	interface SearchEntry { href: string; label: string; keywords: string[] }
	const searchIndex: SearchEntry[] = [
		{ href: '/', label: 'Dashboard', keywords: ['dashboard', 'overview', 'stats', 'cpu', 'memory', 'load', 'charts', 'home'] },
		{ href: '/filesystems', label: 'Filesystems', keywords: ['filesystem', 'pool', 'bcachefs', 'format', 'mount', 'unmount', 'create', 'encryption', 'replicas', 'erasure', 'tiering', 'compression'] },
		{ href: '/subvolumes', label: 'Subvolumes', keywords: ['subvolume', 'snapshot', 'quota', 'block device', 'dataset'] },
		{ href: '/disks', label: 'Disks', keywords: ['disk', 'drive', 'smart', 'health', 'temperature', 'ssd', 'hdd', 'nvme', 'topology', 'device'] },
		{ href: '/files', label: 'Files', keywords: ['file', 'browser', 'folder', 'directory', 'upload', 'download'] },
		{ href: '/sharing', label: 'Sharing', keywords: ['share', 'nfs', 'smb', 'samba', 'cifs', 'iscsi', 'nvmeof', 'nvme-of', 'export', 'target', 'lun', 'acl'] },
		{ href: '/backups', label: 'Backups', keywords: ['backup', 'restore', 'rustic', 'restic', 'snapshot', 'retention', 'schedule', 'offsite', 'encrypt'] },
		{ href: '/alerts', label: 'Alerts', keywords: ['alert', 'notification', 'warning', 'critical', 'rule', 'threshold', 'monitor'] },
		{ href: '/firewall', label: 'Firewall', keywords: ['firewall', 'port', 'nftables', 'restrict', 'ip', 'interface', 'security', 'network'] },
		{ href: '/tls', label: 'TLS', keywords: ['tls', 'ssl', 'certificate', 'https', 'encrypt', 'letsencrypt', 'acme', 'dns', 'cloudflare', 'domain'] },
		{ href: '/ups', label: 'UPS', keywords: ['ups', 'nut', 'battery', 'power', 'shutdown', 'uninterruptible'] },
		{ href: '/vpn', label: 'VPN', keywords: ['vpn', 'tailscale', 'remote', 'access', 'tunnel', 'wireguard'] },
		{ href: '/vms', label: 'VMs', keywords: ['vm', 'virtual', 'machine', 'qemu', 'kvm', 'cpu', 'vnc', 'passthrough'] },
		{ href: '/apps', label: 'Apps', keywords: ['app', 'docker', 'container', 'compose', 'install', 'image', 'port'] },
		{ href: '/terminal', label: 'Terminal', keywords: ['terminal', 'shell', 'ssh', 'console', 'command', 'bash'] },
		{ href: '/services', label: 'Services', keywords: ['service', 'protocol', 'nfs', 'smb', 'iscsi', 'smart', 'avahi', 'mdns', 'enable', 'disable', 'rest server'] },
		{ href: '/logs', label: 'Logs', keywords: ['log', 'journal', 'systemd', 'debug', 'error', 'follow', 'stream'] },
		{ href: '/update', label: 'Update', keywords: ['update', 'upgrade', 'version', 'release', 'nixos', 'rebuild', 'generation'] },
		{ href: '/users', label: 'Access Control', keywords: ['user', 'password', 'role', 'admin', 'group', 'permission', 'token', 'api', 'access', 'auth', 'login'] },
		{ href: '/settings', label: 'Settings', keywords: ['setting', 'hostname', 'timezone', 'clock', 'network', 'ip', 'dhcp', 'dns', 'bond', 'vlan', 'notification', 'email', 'smtp', 'telegram', 'webhook', 'tuning', 'nfs threads', 'metrics', 'prometheus', 'telemetry', 'log level'] },
	];

	let sidebarSearch = $state('');
	const searchResults = $derived.by(() => {
		const q = sidebarSearch.trim().toLowerCase();
		if (!q) return [];
		return searchIndex.filter(e =>
			e.label.toLowerCase().includes(q) ||
			e.keywords.some(k => k.includes(q))
		);
	});

	function selectSearchResult(href: string) {
		sidebarSearch = '';
		goto(href);
	}

	function isGroupExpanded(label: string): boolean {
		return expandedGroups[label] || activeGroup === label;
	}
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<title>{sysInfo?.hostname ? `${sysInfo.hostname} — NASty` : 'NASty'}</title>
</svelte:head>

<Toasts />
<ConfirmDialog />
<ConfirmDangerousDialog />

{#if showLogin}
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
				<Button type="submit" class="w-full">Sign In</Button>
			</form>
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
		<aside class="flex {sidebarCollapsed ? 'w-[52px]' : 'w-[200px]'} shrink-0 flex-col border-r border-border bg-card transition-[width] duration-200">
			<!-- Logo / collapse toggle -->
			{#if sidebarCollapsed}
				<div class="shrink-0 border-b border-border flex items-center justify-center py-3">
					<button onclick={toggleSidebar} class="text-muted-foreground hover:text-foreground transition-colors" title="Expand sidebar">
						<PanelLeftOpen size={18} />
					</button>
				</div>
			{:else}
				<div class="shrink-0 border-b border-border px-4 py-4 relative">
					<a href="https://github.com/nasty-project" target="_blank" rel="noopener noreferrer">
					<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="h-40" />
				</a>
					<button onclick={toggleSidebar} class="absolute top-2 right-2 text-muted-foreground/50 hover:text-foreground transition-colors" title="Collapse sidebar">
						<PanelLeftClose size={15} />
					</button>
				</div>
			{/if}

			<!-- Search bar -->
			{#if !sidebarCollapsed}
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
					{#if searchResults.length > 0}
						<div class="absolute left-2 right-2 top-full z-50 mt-1 rounded-md border border-border bg-popover py-1 shadow-lg">
							{#each searchResults as result}
								<button
									class="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-popover-foreground hover:bg-accent transition-colors text-left"
									onclick={() => selectSearchResult(result.href)}
								>
									{result.label}
								</button>
							{/each}
						</div>
					{/if}
				</div>
			{/if}

			<!-- Nav — scrollable -->
			<nav class="flex-1 overflow-y-auto py-2">
				{#each nav as entry}
					{#if isGroup(entry)}
						{@const GroupIcon = entry.icon}
						{@const expanded = isGroupExpanded(entry.label)}
						{@const groupActive = activeGroup === entry.label}
						{#if sidebarCollapsed}
							{#each entry.children as child}
								{@const ChildIcon = child.icon}
								{@const active = currentNav.href === child.href}
								<a
									href={child.href}
									title={child.label}
									class="relative mx-2 flex items-center justify-center rounded-md py-2 text-sm no-underline transition-all border-2
										{active
											? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
											: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
								>
									<ChildIcon size={15} class="shrink-0" />
								</a>
							{/each}
						{:else}
							<button
								onclick={() => toggleGroup(entry.label)}
								class="mx-2 flex w-[calc(100%-1rem)] items-center gap-2.5 rounded-md py-2 pl-4 pr-3 text-sm transition-colors
									{groupActive ? 'text-foreground font-medium' : 'text-muted-foreground hover:text-foreground'}"
							>
								<GroupIcon size={15} class="shrink-0" />
								{entry.label}
								<ChevronRight size={13} class="ml-auto shrink-0 transition-transform duration-200 {expanded ? 'rotate-90' : ''}" />
							</button>
							{#if expanded}
								{#each entry.children as child}
									{@const ChildIcon = child.icon}
									{@const active = currentNav.href === child.href}
									<a
										href={child.href}
										class="relative mx-2 flex items-center gap-2.5 rounded-md py-1.5 pl-8 pr-4 text-sm no-underline transition-all border-2
											{active
												? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
												: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
									>
										<ChildIcon size={14} class="shrink-0" />
										{child.label}
									</a>
								{/each}
							{/if}
						{/if}
					{:else}
						{@const Icon = entry.icon}
						{@const active = currentNav.href === entry.href}
						<a
							href={entry.href}
							title={sidebarCollapsed ? entry.label : undefined}
							class="relative mx-2 flex items-center rounded-md py-2 text-sm no-underline transition-all border-2
								{sidebarCollapsed ? 'justify-center px-0' : 'gap-2.5 pl-4 pr-4'}
								{active
									? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
									: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
						>
							<Icon size={15} class="shrink-0" />
							{#if !sidebarCollapsed}{entry.label}{/if}
						</a>
					{/if}
				{/each}
			</nav>

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
					{#if sysInfo?.bcachefs_is_custom || sysInfo?.bcachefs_debug_checks}
						<a
							href="/update#bcachefs"
							class="flex items-center gap-2 rounded-md border-2 border-blue-500/70 px-3 py-1.5 text-sm text-blue-400 no-underline transition-all hover:bg-blue-500/10 hover:border-blue-400 hover:shadow-[0_0_16px_rgba(96,165,250,0.5)]"
						>
							<span>bcachefs</span>
							<span class="flex items-center gap-1.5">
								<span title="Custom version"><Settings size={14} class={sysInfo.bcachefs_is_custom ? 'text-amber-400' : 'text-muted-foreground/30'} /></span>
								<span title="Debug checks"><Bug size={14} class={sysInfo.bcachefs_debug_checks ? 'text-blue-400' : 'text-muted-foreground/30'} /></span>
							</span>
						</a>
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
							</div>
						{/if}
					</div>

					<!-- Theme toggle -->
					<button
						onclick={() => theme.toggle()}
						class="flex items-center rounded-md border-2 border-blue-500/50 px-2.5 py-1.5 text-muted-foreground transition-all hover:bg-accent hover:text-accent-foreground hover:border-blue-400/80 hover:shadow-[0_0_12px_rgba(96,165,250,0.4)] active:shadow-none"
						title={theme.isDark ? 'Switch to light mode' : 'Switch to dark mode'}
					>
						{#if theme.isDark}
							<Sun size={15} />
						{:else}
							<Moon size={15} />
						{/if}
					</button>

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
							class="flex items-center gap-2 rounded-md border-2 border-blue-500/50 px-3 py-1.5 text-sm text-muted-foreground transition-all hover:bg-accent hover:text-accent-foreground hover:border-blue-400/80 hover:shadow-[0_0_12px_rgba(96,165,250,0.4)] active:shadow-none disabled:opacity-50"
						>
							<Power size={15} />
							Power
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
					<a href="/settings" onclick={() => {}} class="mb-4 flex items-center gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-sm text-amber-400 no-underline hover:bg-amber-500/20 transition-colors">
						<span>SSH password authentication is enabled — add an SSH key and disable it for better security.</span>
						<span class="ml-auto text-xs shrink-0">Settings &rarr;</span>
					</a>
				{/if}
				{#if configBackupMissing}
					<div class="mb-4 flex items-center gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-sm text-amber-400">
						<span class="flex-1">NASty configuration is not backed up.</span>
						<a href="/backups?create=config" class="text-xs font-medium text-amber-400 hover:text-amber-300 shrink-0 no-underline">Create backup</a>
						<span class="text-amber-400/30">|</span>
						<button onclick={dismissConfigBackup} class="text-xs text-amber-400/60 hover:text-amber-400 shrink-0">dismiss</button>
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
