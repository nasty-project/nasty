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
	} from '@lucide/svelte';
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
		try {
			const result = await getClient().call<{ password_auth: boolean; keys: string[] }>('system.ssh.status');
			sshPasswordAuth = result.password_auth;
		} catch { /* ignore */ }
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
	let sysInfo: { hostname: string; version: string; kernel: string; bcachefs_version: string; bcachefs_commit: string | null; bcachefs_pinned_ref: string | null; bcachefs_is_custom: boolean; bcachefs_debug_checks: boolean; kvm_available: boolean } | null = $state(null);
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
		return () => {
			getClient().offReconnect(onReconnect);
			getClient().offDisconnect(onDisconnect);
			getClient().disconnect();
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

	const nav = $derived.by(() => {
		const items = [
			{ href: '/',              label: 'Dashboard',      icon: LayoutDashboard },
			{ href: '/filesystems',   label: 'Filesystems',    icon: Database },
			{ href: '/subvolumes',    label: 'Subvolumes',     icon: Layers },
			{ href: '/sharing',       label: 'Sharing',         icon: Share2 },
			{ href: '/files',         label: 'Files',           icon: FolderOpen },
		];
		if (sysInfo?.kvm_available) {
			items.push({ href: '/vms', label: 'VMs', icon: Monitor });
		}
		items.push(
			{ href: '/apps',          label: 'Apps',             icon: Box },
			{ href: '/disks',         label: 'Disks',           icon: HardDrive },
			{ href: '/alerts',        label: 'Alerts',          icon: Bell },
			{ href: '/services',      label: 'Services',        icon: Network },
			{ href: '/update',        label: 'Update',          icon: RefreshCw },
			{ href: '/terminal',      label: 'Terminal',        icon: Terminal },
			{ href: '/users',         label: 'Access Control',  icon: ShieldCheck },
			{ href: '/settings',      label: 'Settings',        icon: Settings },
		);
		return items;
	});

	// Derive current nav entry from path
	const currentNav = $derived.by(() => {
		const path = $page.url.pathname;
		// Match longest prefix first
		return [...nav].sort((a, b) => b.href.length - a.href.length)
			.find(n => path === n.href || (n.href !== '/' && path.startsWith(n.href))) ?? nav[0];
	});
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

			<!-- Nav — scrollable -->
			<nav class="flex-1 overflow-y-auto py-2">
				{#each nav as item}
					{@const Icon = item.icon}
					{@const active = currentNav.href === item.href}
					<a
						href={item.href}
						title={sidebarCollapsed ? item.label : undefined}
						class="relative mx-2 flex items-center rounded-md py-2 text-sm no-underline transition-all border-2
							{sidebarCollapsed ? 'justify-center px-0' : 'gap-2.5 pl-4 pr-4'}
							{active
								? 'text-foreground font-medium border-blue-500/50 shadow-[0_0_8px_rgba(96,165,250,0.25)]'
								: 'text-muted-foreground border-transparent hover:text-foreground hover:border-blue-400/50 hover:shadow-[0_0_10px_rgba(96,165,250,0.25)]'}"
					>
						<Icon size={15} class="shrink-0" />
						{#if !sidebarCollapsed}{item.label}{/if}
					</a>
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
