<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/stores';
	import { getClient, resetClient } from '$lib/client';
	import { getToken, clearToken, login as doLogin } from '$lib/auth';
	import { error as showError } from '$lib/toast.svelte';
	import Toasts from '$lib/components/Toasts.svelte';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
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
		FolderOpen,
		Share2,
		Server,
		HardDrive,
		Bell,
		Settings,
		RefreshCw,
		Terminal,
		ShieldCheck,
		Network,
		Zap,
		Power,
		RotateCcw,
		PowerOff,
		LogOut,
		User,
		Sun,
		Moon,
		PanelLeftClose,
		PanelLeftOpen,
	} from '@lucide/svelte';
	import { refreshState } from '$lib/refresh.svelte';
	import { rebootState } from '$lib/reboot.svelte';
	import { sysInfoRefresh } from '$lib/sysInfoRefresh.svelte';
	import { theme } from '$lib/theme.svelte';

	let { children } = $props();
	let connected = $state(false);
	let authInfo: AuthResult | null = $state(null);

	// Login form
	let showLogin = $state(false);
	let loginUser = $state('admin');
	let loginPass = $state('');
	let loginError = $state('');

	// Power menu
	let powerOpen = $state(false);
	let powering = $state(false);

	// Profile menu
	let profileOpen = $state(false);

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
	let sysInfo: { version: string; kernel: string; bcachefs_version: string; bcachefs_commit: string | null; bcachefs_pinned_ref: string | null; bcachefs_is_custom: boolean } | null = $state(null);
	let clock24h = $state(true);

	$effect(() => {
		const _r = sysInfoRefresh.count; // track refresh triggers
		if (connected) {
			getClient().call('system.info').then((info: any) => { sysInfo = info; }).catch(() => {});
			getClient().call('system.settings.get').then((s: any) => { clock24h = s.clock_24h ?? true; }).catch(() => {});
		}
	});

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

	onMount(() => {
		tryConnect();
		const onReconnect = () => { powering = false; };
		getClient().onReconnect(onReconnect);
		const tick = setInterval(() => { now = new Date(); }, 1000);
		const rebootPoll = setInterval(checkRebootRequired, 30_000);
		return () => {
			getClient().offReconnect(onReconnect);
			getClient().disconnect();
			clearInterval(tick);
			clearInterval(rebootPoll);
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

	const nav = [
		{ href: '/',              label: 'Dashboard',      icon: LayoutDashboard },
		{ href: '/pools',         label: 'Storage Pools',  icon: Database },
		{ href: '/subvolumes',    label: 'Subvolumes',     icon: Layers },
		{ href: '/shares/nfs',    label: 'NFS',            icon: FolderOpen },
		{ href: '/shares/smb',    label: 'SMB',            icon: Share2 },
		{ href: '/shares/iscsi',  label: 'iSCSI',          icon: Server },
		{ href: '/shares/nvmeof', label: 'NVMe-oF',        icon: Zap },
		{ href: '/disks',         label: 'Disks',           icon: HardDrive },
		{ href: '/alerts',        label: 'Alerts',          icon: Bell },
		{ href: '/services',      label: 'Services',        icon: Network },
		{ href: '/update',        label: 'Update',          icon: RefreshCw },
		{ href: '/terminal',      label: 'Terminal',        icon: Terminal },
		{ href: '/users',         label: 'Access Control',  icon: ShieldCheck },
		{ href: '/settings',      label: 'Settings',        icon: Settings },
	];

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
	<title>NASty</title>
</svelte:head>

<Toasts />
<ConfirmDialog />

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
{:else}
	<div class="flex h-screen overflow-hidden">
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
					<img src={theme.isDark ? logoDark : logoLight} alt="NASty" class="h-40" />
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
							<span class="text-[0.68rem] text-muted-foreground/50">NASty</span>
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
				</div>

				<div class="flex items-center gap-2.5">
					{#if powering}
						<span class="text-sm text-amber-500">Shutting down…</span>
					{/if}

					<!-- Theme toggle -->
					<button
						onclick={() => theme.toggle()}
						class="flex items-center rounded-md border-2 border-blue-500/50 p-1.5 text-muted-foreground transition-all hover:bg-accent hover:text-accent-foreground hover:border-blue-400/80 hover:shadow-[0_0_12px_rgba(96,165,250,0.4)] active:shadow-none"
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
			<main class="flex-1 overflow-y-auto p-6">
				{#if !connected}
					<p class="text-muted-foreground">Connecting to engine...</p>
				{:else}
					{@render children()}
				{/if}
			</main>
		</div>
	</div>
{/if}
