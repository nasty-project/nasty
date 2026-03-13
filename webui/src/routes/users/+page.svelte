<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { UserInfo, ApiTokenInfo, ApiTokenCreated, Pool } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import * as Dialog from '$lib/components/ui/dialog';

	let users: UserInfo[] = $state([]);
	let apiTokens: ApiTokenInfo[] = $state([]);
	let pools: Pool[] = $state([]);
	let loading = $state(true);
	let showCreate = $state(false);
	let showCreateToken = $state(false);

	let newUsername = $state('');
	let newPassword = $state('');
	let newPasswordConfirm = $state('');
	let newRole = $state<'admin' | 'readonly' | 'operator'>('readonly');

	let newTokenName = $state('');
	let newTokenRole = $state<'admin' | 'readonly' | 'operator'>('operator');
	let newTokenPool = $state('');
	let newTokenExpiry = $state('');
	let createdToken = $state<ApiTokenCreated | null>(null);
	let tokenCopied = $state(false);

	let pwUser = $state<string | null>(null);
	let pwNew = $state('');
	let pwConfirm = $state('');

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			[users, apiTokens, pools] = await Promise.all([
				client.call<UserInfo[]>('auth.list_users'),
				client.call<ApiTokenInfo[]>('auth.token.list'),
				client.call<Pool[]>('pool.list'),
			]);
		});
	}

	async function createUser() {
		if (!newUsername || !newPassword) return;
		if (newPassword !== newPasswordConfirm) return;
		const ok = await withToast(
			() => client.call('auth.create_user', {
				username: newUsername,
				password: newPassword,
				role: newRole,
			}),
			`User "${newUsername}" created`
		);
		if (ok !== undefined) {
			showCreate = false;
			newUsername = '';
			newPassword = '';
			newPasswordConfirm = '';
			newRole = 'readonly';
			await refresh();
		}
	}

	async function deleteUser(username: string) {
		if (!confirm(`Delete user "${username}"? This will revoke all their sessions.`)) return;
		await withToast(
			() => client.call('auth.delete_user', { username }),
			`User "${username}" deleted`
		);
		await refresh();
	}

	async function changePassword() {
		if (!pwUser || !pwNew) return;
		if (pwNew !== pwConfirm) return;
		const ok = await withToast(
			() => client.call('auth.change_password', {
				username: pwUser,
				new_password: pwNew,
			}),
			`Password changed for "${pwUser}"`
		);
		if (ok !== undefined) {
			pwUser = null;
			pwNew = '';
			pwConfirm = '';
		}
	}

	async function createToken() {
		if (!newTokenName) return;
		const expires_in_secs = newTokenExpiry ? parseInt(newTokenExpiry) : null;
		const result = await withToast(
			() => client.call<ApiTokenCreated>('auth.token.create', {
				name: newTokenName,
				role: newTokenRole,
				pool: newTokenPool || null,
				expires_in_secs,
			}),
			`API token "${newTokenName}" created`
		);
		if (result !== undefined) {
			createdToken = result;
			showCreateToken = false;
			newTokenName = '';
			newTokenRole = 'operator';
			newTokenPool = '';
			newTokenExpiry = '';
			await refresh();
		}
	}

	async function deleteToken(id: string, name: string) {
		if (!confirm(`Revoke API token "${name}"?`)) return;
		await withToast(
			() => client.call('auth.token.delete', { id }),
			`API token "${name}" revoked`
		);
		await refresh();
	}

	async function copyToken() {
		if (!createdToken) return;
		await navigator.clipboard.writeText(createdToken.token);
		tokenCopied = true;
		setTimeout(() => tokenCopied = false, 2000);
	}

	function formatDate(ts: number): string {
		return new Date(ts * 1000).toLocaleDateString();
	}
</script>

<h1 class="mb-4 text-2xl font-bold">Users</h1>

<div class="mb-4">
	<Button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create User'}
	</Button>
</div>

{#if showCreate}
	<Card class="mb-6 max-w-md">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New User</h3>
			<div class="mb-4">
				<Label for="new-username">Username</Label>
				<Input id="new-username" bind:value={newUsername} placeholder="johndoe" autocomplete="off" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="new-password">Password</Label>
				<Input id="new-password" type="password" bind:value={newPassword} placeholder="Min 8 characters" autocomplete="new-password" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="new-password-confirm">Confirm Password</Label>
				<Input id="new-password-confirm" type="password" bind:value={newPasswordConfirm} autocomplete="new-password" class="mt-1" />
				{#if newPasswordConfirm && newPassword !== newPasswordConfirm}
					<span class="mt-1 block text-xs text-destructive">Passwords do not match</span>
				{/if}
			</div>
			<div class="mb-4">
				<Label for="new-role">Role</Label>
				<select id="new-role" bind:value={newRole} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="readonly">Read Only</option>
					<option value="admin">Admin</option>
					<option value="operator">Operator</option>
				</select>
			</div>
			<Button onclick={createUser} disabled={!newUsername || !newPassword || newPassword.length < 8 || newPassword !== newPasswordConfirm}>
				Create
			</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<p class="text-muted-foreground">Loading...</p>
{:else if users.length === 0}
	<p class="text-muted-foreground">No users configured.</p>
{:else}
	<table class="mb-10 w-full text-sm">
		<thead>
			<tr>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Username</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Role</th>
				<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each users as user}
				<tr class="border-b border-border">
					<td class="p-3"><strong>{user.username}</strong></td>
					<td class="p-3">
						<Badge variant="secondary" class={
							user.role === 'admin' ? 'bg-blue-950 text-blue-400' :
							user.role === 'operator' ? 'bg-amber-950 text-amber-400' : ''
						}>
							{user.role === 'admin' ? 'Admin' : user.role === 'operator' ? 'Operator' : 'Read Only'}
						</Badge>
					</td>
					<td class="p-3">
						<div class="flex gap-2">
							<Button variant="secondary" size="sm" onclick={() => { pwUser = user.username; pwNew = ''; pwConfirm = ''; }}>
								Change Password
							</Button>
							<Button variant="destructive" size="sm" onclick={() => deleteUser(user.username)}>Delete</Button>
						</div>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<div class="mb-4 flex items-center justify-between">
	<h2 class="text-xl font-semibold">API Tokens</h2>
	<Button onclick={() => showCreateToken = !showCreateToken}>
		{showCreateToken ? 'Cancel' : 'Create Token'}
	</Button>
</div>
<p class="mb-4 text-sm text-muted-foreground">Long-lived tokens for programmatic access (e.g. k8s CSI driver). Tokens do not expire and are not tied to a user session.</p>

{#if showCreateToken}
	<Card class="mb-6 max-w-md">
		<CardContent class="pt-6">
			<h3 class="mb-4 text-lg font-semibold">New API Token</h3>
			<div class="mb-4">
				<Label for="token-name">Name</Label>
				<Input id="token-name" bind:value={newTokenName} placeholder="e.g. k8s-cluster" autocomplete="off" class="mt-1" />
			</div>
			<div class="mb-4">
				<Label for="token-role">Role</Label>
				<select id="token-role" bind:value={newTokenRole} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="operator">Operator (subvolumes &amp; snapshots only)</option>
					<option value="readonly">Read Only</option>
					<option value="admin">Admin</option>
				</select>
			</div>
			<div class="mb-4">
				<Label for="token-pool">Pool Restriction</Label>
				<select id="token-pool" bind:value={newTokenPool} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">All pools</option>
					{#each pools as pool}
						<option value={pool.name}>{pool.name}</option>
					{/each}
				</select>
				<span class="mt-1 block text-xs text-muted-foreground">Restrict this token to a single pool's subvolumes</span>
			</div>
			<div class="mb-4">
				<Label for="token-expiry">Expiration</Label>
				<select id="token-expiry" bind:value={newTokenExpiry} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Never</option>
					<option value="86400">1 day</option>
					<option value="604800">7 days</option>
					<option value="2592000">30 days</option>
					<option value="7776000">90 days</option>
					<option value="31536000">1 year</option>
				</select>
			</div>
			<Button onclick={createToken} disabled={!newTokenName}>Create Token</Button>
		</CardContent>
	</Card>
{/if}

{#if !loading}
	{#if apiTokens.length === 0}
		<p class="text-sm text-muted-foreground">No API tokens configured.</p>
	{:else}
		<table class="w-full text-sm">
			<thead>
				<tr>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Name</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Role</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Pool</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Created</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Expires</th>
					<th class="border-b-2 border-border p-3 text-left text-xs uppercase text-muted-foreground">Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each apiTokens as token}
					<tr class="border-b border-border">
						<td class="p-3 font-mono text-xs">{token.name}</td>
						<td class="p-3">
							<Badge variant="secondary" class={
								token.role === 'admin' ? 'bg-blue-950 text-blue-400' :
								token.role === 'operator' ? 'bg-amber-950 text-amber-400' : ''
							}>
								{token.role === 'admin' ? 'Admin' : token.role === 'operator' ? 'Operator' : 'Read Only'}
							</Badge>
						</td>
						<td class="p-3 font-mono text-xs text-muted-foreground">{token.pool ?? '—'}</td>
						<td class="p-3 text-xs text-muted-foreground">{formatDate(token.created_at)}</td>
						<td class="p-3 text-xs {token.expires_at && token.expires_at * 1000 < Date.now() ? 'text-destructive' : 'text-muted-foreground'}">
							{token.expires_at ? formatDate(token.expires_at) : '—'}
						</td>
						<td class="p-3">
							<Button variant="destructive" size="sm" onclick={() => deleteToken(token.id, token.name)}>Revoke</Button>
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
{/if}

<!-- Change Password Dialog -->
<Dialog.Root open={pwUser !== null} onOpenChange={(open) => { if (!open) pwUser = null; }}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Change Password for "{pwUser}"</Dialog.Title>
		</Dialog.Header>
		<div class="mb-4">
			<Label for="pw-new">New Password</Label>
			<Input id="pw-new" type="password" bind:value={pwNew} placeholder="Min 8 characters" autocomplete="new-password" class="mt-1" />
		</div>
		<div class="mb-4">
			<Label for="pw-confirm">Confirm Password</Label>
			<Input id="pw-confirm" type="password" bind:value={pwConfirm} autocomplete="new-password" class="mt-1" />
			{#if pwConfirm && pwNew !== pwConfirm}
				<span class="mt-1 block text-xs text-destructive">Passwords do not match</span>
			{/if}
		</div>
		<Dialog.Footer>
			<Button onclick={changePassword} disabled={!pwNew || pwNew.length < 8 || pwNew !== pwConfirm}>
				Change Password
			</Button>
			<Button variant="secondary" onclick={() => pwUser = null}>Cancel</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>

<!-- New Token Created Dialog -->
<Dialog.Root open={createdToken !== null} onOpenChange={(open) => { if (!open) createdToken = null; }}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>API Token Created</Dialog.Title>
		</Dialog.Header>
		<p class="mb-3 text-sm text-muted-foreground">
			Copy this token now — it will not be shown again.
		</p>
		{#if createdToken}
			<div class="mb-4 rounded-lg border border-border bg-secondary p-3">
				<p class="mb-1 text-xs text-muted-foreground">Token for <strong>{createdToken.name}</strong></p>
				<code class="break-all text-xs">{createdToken.token}</code>
			</div>
		{/if}
		<Dialog.Footer>
			<Button onclick={copyToken}>{tokenCopied ? 'Copied!' : 'Copy to Clipboard'}</Button>
			<Button variant="secondary" onclick={() => createdToken = null}>Close</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
