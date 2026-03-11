<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient, resetClient } from '$lib/client';
	import { getToken, clearToken, login as doLogin } from '$lib/auth';
	import { error as showError } from '$lib/toast.svelte';
	import Toasts from '$lib/components/Toasts.svelte';
	import type { AuthResult } from '$lib/rpc';
	import favicon from '$lib/assets/favicon.svg';

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
	<div class="login-page">
		<div class="login-card">
			<h1>NASty</h1>
			<p class="subtitle">Sign in to manage your storage</p>
			{#if loginError}
				<p class="login-error">{loginError}</p>
			{/if}
			<form onsubmit={(e) => { e.preventDefault(); handleLogin(); }}>
				<div class="field">
					<label for="username">Username</label>
					<input id="username" bind:value={loginUser} autocomplete="username" />
				</div>
				<div class="field">
					<label for="password">Password</label>
					<input id="password" type="password" bind:value={loginPass} autocomplete="current-password" />
				</div>
				<button type="submit">Sign In</button>
			</form>
		</div>
	</div>
{:else}
	<div class="app">
		<aside class="sidebar">
			<div class="logo">NASty</div>
			<nav>
				{#each nav as item}
					<a href={item.href}>{item.label}</a>
				{/each}
			</nav>
			<div class="sidebar-footer">
				{#if authInfo}
					<div class="user-info">
						<span class="username">{authInfo.username}</span>
						<span class="role">{authInfo.role}</span>
					</div>
				{/if}
				<button class="logout-btn" onclick={handleLogout}>Sign Out</button>
				<div class="status" class:ok={connected}>
					{connected ? 'Connected' : 'Disconnected'}
				</div>
			</div>
		</aside>
		<main>
			{#if !connected}
				<p>Connecting to middleware...</p>
			{:else}
				{@render children()}
			{/if}
		</main>
	</div>
{/if}

<style>
	:global(body) {
		margin: 0;
		font-family: system-ui, -apple-system, sans-serif;
		background: #0f1117;
		color: #e0e0e0;
	}
	:global(a) { color: #6ea8fe; }
	:global(button) {
		background: #2563eb;
		color: white;
		border: none;
		padding: 0.5rem 1rem;
		border-radius: 4px;
		cursor: pointer;
		font-size: 0.875rem;
	}
	:global(button:hover) { background: #1d4ed8; }
	:global(button.danger) { background: #dc2626; }
	:global(button.danger:hover) { background: #b91c1c; }
	:global(button.secondary) { background: #374151; }
	:global(button.secondary:hover) { background: #4b5563; }
	:global(input, select) {
		background: #1e2130;
		color: #e0e0e0;
		border: 1px solid #374151;
		padding: 0.5rem;
		border-radius: 4px;
		font-size: 0.875rem;
	}
	:global(table) { width: 100%; border-collapse: collapse; }
	:global(th) { text-align: left; padding: 0.75rem; border-bottom: 2px solid #2d3348; font-size: 0.75rem; text-transform: uppercase; color: #9ca3af; }
	:global(td) { padding: 0.75rem; border-bottom: 1px solid #1e2130; }

	/* Login page */
	.login-page {
		display: flex;
		align-items: center;
		justify-content: center;
		min-height: 100vh;
	}
	.login-card {
		background: #161926;
		border: 1px solid #2d3348;
		border-radius: 12px;
		padding: 2.5rem;
		width: 340px;
	}
	.login-card h1 { margin: 0 0 0.25rem; font-size: 2rem; }
	.subtitle { color: #6b7280; margin: 0 0 1.5rem; font-size: 0.875rem; }
	.login-error { color: #f87171; font-size: 0.875rem; margin: 0 0 1rem; }
	.login-card .field { margin-bottom: 1rem; }
	.login-card .field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.login-card .field input { width: 100%; box-sizing: border-box; }
	.login-card button { width: 100%; padding: 0.7rem; font-size: 1rem; }

	/* App layout */
	.app { display: flex; min-height: 100vh; }
	.sidebar {
		width: 200px;
		background: #161926;
		border-right: 1px solid #2d3348;
		display: flex;
		flex-direction: column;
		padding: 1rem 0;
		flex-shrink: 0;
	}
	.logo { font-size: 1.5rem; font-weight: 700; padding: 0 1rem 1rem; border-bottom: 1px solid #2d3348; margin-bottom: 0.5rem; }
	nav { display: flex; flex-direction: column; flex: 1; }
	nav a { padding: 0.6rem 1rem; text-decoration: none; color: #9ca3af; font-size: 0.875rem; }
	nav a:hover { background: #1e2130; color: #e0e0e0; }
	.sidebar-footer { padding: 0.75rem 1rem; border-top: 1px solid #2d3348; }
	.user-info { display: flex; justify-content: space-between; align-items: center; margin-bottom: 0.5rem; }
	.username { font-size: 0.875rem; font-weight: 600; }
	.role { font-size: 0.7rem; color: #6b7280; text-transform: uppercase; background: #1e2130; padding: 0.1rem 0.4rem; border-radius: 3px; }
	.logout-btn { width: 100%; background: #374151; font-size: 0.8rem; padding: 0.4rem; margin-bottom: 0.5rem; }
	.logout-btn:hover { background: #4b5563; }
	.status { font-size: 0.75rem; color: #6b7280; }
	.status.ok { color: #4ade80; }
	main { flex: 1; padding: 2rem; overflow-y: auto; }
</style>
