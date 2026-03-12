<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient, resetClient } from '$lib/client';
	import { getToken, clearToken, login as doLogin } from '$lib/auth';
	import { error as showError } from '$lib/toast.svelte';
	import Toasts from '$lib/components/Toasts.svelte';
	import type { AuthResult } from '$lib/rpc';
	import favicon from '$lib/assets/favicon.svg';
	import '../app.css';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Separator } from '$lib/components/ui/separator';

	let { children } = $props();
	let connected = $state(false);
	let authInfo: AuthResult | null = $state(null);
	let error = $state('');

	// Login form
	let showLogin = $state(false);
	let loginUser = $state('admin');
	let loginPass = $state('');
	let loginError = $state('');

	onMount(() => {
		tryConnect();
		return () => getClient().disconnect();
	});

	async function tryConnect() {
		const token = getToken();
		if (!token) {
			showLogin = true;
			return;
		}

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
		try {
			const client = getClient();
			await client.call('auth.logout');
		} catch {
			// Ignore errors during logout
		}
		clearToken();
		resetClient();
		connected = false;
		authInfo = null;
		showLogin = true;
	}

	const nav = [
		{ href: '/', label: 'Dashboard' },
		{ href: '/pools', label: 'Storage Pools' },
		{ href: '/subvolumes', label: 'Subvolumes' },
		{ href: '/shares/nfs', label: 'NFS' },
		{ href: '/shares/smb', label: 'SMB' },
		{ href: '/shares/iscsi', label: 'iSCSI' },
		{ href: '/shares/nvmeof', label: 'NVMe-oF' },
		{ href: '/disks', label: 'Disks' },
		{ href: '/alerts', label: 'Alerts' },
		{ href: '/services', label: 'Services' },
		{ href: '/update', label: 'Update' },
		{ href: '/terminal', label: 'Terminal' },
		{ href: '/users', label: 'Users' },
	];
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<title>NASty</title>
</svelte:head>

<Toasts />

{#if showLogin}
	<div class="flex min-h-screen items-center justify-center">
		<div class="w-[340px] rounded-xl border border-border bg-card p-8">
			<h1 class="mb-1 text-2xl font-bold text-foreground">NASty</h1>
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
	<div class="flex min-h-screen">
		<aside class="flex w-[200px] shrink-0 flex-col border-r border-border bg-card py-4">
			<div class="mb-2 border-b border-border px-4 pb-4 text-xl font-bold">NASty</div>
			<nav class="flex flex-1 flex-col">
				{#each nav as item}
					<a href={item.href} class="px-4 py-2 text-sm text-muted-foreground no-underline transition-colors hover:bg-accent hover:text-accent-foreground">{item.label}</a>
				{/each}
			</nav>
			<div class="border-t border-border px-4 pt-3">
				{#if authInfo}
					<div class="mb-2 flex items-center justify-between">
						<span class="text-sm font-semibold">{authInfo.username}</span>
						<span class="rounded bg-secondary px-1.5 py-0.5 text-[0.7rem] uppercase text-muted-foreground">{authInfo.role}</span>
					</div>
				{/if}
				<Button variant="secondary" size="sm" class="mb-2 w-full" onclick={handleLogout}>Sign Out</Button>
				<div class="text-xs {connected ? 'text-green-400' : 'text-muted-foreground'}">
					{connected ? 'Connected' : 'Disconnected'}
				</div>
			</div>
		</aside>
		<main class="flex-1 overflow-y-auto p-6">
			{#if !connected}
				<p class="text-muted-foreground">Connecting to middleware...</p>
			{:else}
				{@render children()}
			{/if}
		</main>
	</div>
{/if}
