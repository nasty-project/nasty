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
	import logo from '$lib/assets/nasty-white.svg';
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
	} from '@lucide/svelte';

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

	onMount(() => {
		tryConnect();
		return () => getClient().disconnect();
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
		{ href: '/disks',         label: 'S.M.A.R.T.',     icon: HardDrive },
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
			<img src={logo} alt="NASty" class="mb-4 h-48 mx-auto" />
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
		<aside class="flex w-[200px] shrink-0 flex-col border-r border-border bg-card">
			<!-- Logo -->
			<div class="shrink-0 border-b border-border px-4 py-4">
				<img src={logo} alt="NASty" class="h-40" />
			</div>

			<!-- Nav — scrollable -->
			<nav class="flex-1 overflow-y-auto py-2">
				{#each nav as item}
					{@const Icon = item.icon}
					{@const active = currentNav.href === item.href}
					<a
						href={item.href}
						class="flex items-center gap-2.5 px-4 py-2 text-sm no-underline transition-colors
							{active
								? 'bg-accent text-accent-foreground font-medium'
								: 'text-muted-foreground hover:bg-accent hover:text-accent-foreground'}"
					>
						<Icon size={15} class="shrink-0" />
						{item.label}
					</a>
				{/each}
			</nav>

			<!-- Footer — always visible -->
			<div class="shrink-0 border-t border-border px-4 py-3">
				{#if authInfo}
					<div class="mb-2 flex items-center justify-between">
						<span class="text-sm font-semibold">{authInfo.username}</span>
						<span class="rounded bg-secondary px-1.5 py-0.5 text-[0.7rem] uppercase text-muted-foreground">{authInfo.role}</span>
					</div>
				{/if}
				<button
					onclick={handleLogout}
					class="flex w-full items-center gap-2 rounded-md px-3 py-1.5 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground"
				>
					<LogOut size={14} />
					Sign Out
				</button>
				<div class="mt-2 text-xs {connected ? 'text-green-400' : 'text-muted-foreground'}">
					{connected ? '● Connected' : '○ Disconnected'}
				</div>
			</div>
		</aside>

		<!-- Right side: top bar + content -->
		<div class="flex flex-1 flex-col overflow-hidden">
			<!-- Top bar -->
			<header class="flex h-12 shrink-0 items-center justify-between border-b border-border bg-card px-6">
				<div class="flex items-center gap-2 text-sm">
					<svelte:component this={currentNav.icon} size={15} class="text-muted-foreground" />
					<span class="font-medium">{currentNav.label}</span>
				</div>

				<div class="relative flex items-center gap-2">
					{#if powering}
						<span class="text-xs text-amber-500">Shutting down…</span>
					{/if}
					<button
						onclick={() => powerOpen = !powerOpen}
						disabled={powering}
						class="flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1 text-xs text-muted-foreground transition-colors hover:border-border hover:bg-accent hover:text-accent-foreground disabled:opacity-50"
					>
						<Power size={13} />
						Power
					</button>
					{#if powerOpen}
						<!-- svelte-ignore a11y_no_static_element_interactions -->
						<div
							class="absolute right-0 top-8 z-50 min-w-[160px] rounded-lg border border-border bg-card shadow-lg"
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
			</header>

			<!-- Page content -->
			<main class="flex-1 overflow-y-auto p-6">
				{#if !connected}
					<p class="text-muted-foreground">Connecting to middleware...</p>
				{:else}
					{@render children()}
				{/if}
			</main>
		</div>
	</div>
{/if}
