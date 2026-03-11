<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { UserInfo } from '$lib/types';

	let users: UserInfo[] = $state([]);
	let loading = $state(true);
	let showCreate = $state(false);

	// Create form
	let newUsername = $state('');
	let newPassword = $state('');
	let newPasswordConfirm = $state('');
	let newRole = $state<'admin' | 'readonly'>('readonly');

	// Change password modal
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
			users = await client.call<UserInfo[]>('auth.list_users');
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
</script>

<h1>Users</h1>

<div class="toolbar">
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create User'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>New User</h3>
		<div class="field">
			<label for="new-username">Username</label>
			<input id="new-username" bind:value={newUsername} placeholder="johndoe" autocomplete="off" />
		</div>
		<div class="field">
			<label for="new-password">Password</label>
			<input id="new-password" type="password" bind:value={newPassword} placeholder="Min 8 characters" autocomplete="new-password" />
		</div>
		<div class="field">
			<label for="new-password-confirm">Confirm Password</label>
			<input id="new-password-confirm" type="password" bind:value={newPasswordConfirm} autocomplete="new-password" />
			{#if newPasswordConfirm && newPassword !== newPasswordConfirm}
				<span class="field-error">Passwords do not match</span>
			{/if}
		</div>
		<div class="field">
			<label for="new-role">Role</label>
			<select id="new-role" bind:value={newRole}>
				<option value="readonly">Read Only</option>
				<option value="admin">Admin</option>
			</select>
		</div>
		<button onclick={createUser} disabled={!newUsername || !newPassword || newPassword.length < 8 || newPassword !== newPasswordConfirm}>
			Create
		</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if users.length === 0}
	<p class="muted">No users configured.</p>
{:else}
	<table>
		<thead>
			<tr>
				<th>Username</th>
				<th>Role</th>
				<th>Actions</th>
			</tr>
		</thead>
		<tbody>
			{#each users as user}
				<tr>
					<td><strong>{user.username}</strong></td>
					<td>
						<span class="badge" class:admin={user.role === 'admin'} class:readonly={user.role === 'readonly'}>
							{user.role === 'admin' ? 'Admin' : 'Read Only'}
						</span>
					</td>
					<td class="actions">
						<button class="secondary" onclick={() => { pwUser = user.username; pwNew = ''; pwConfirm = ''; }}>
							Change Password
						</button>
						<button class="danger" onclick={() => deleteUser(user.username)}>Delete</button>
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

{#if pwUser}
	<div class="modal-overlay" role="presentation" onclick={() => pwUser = null} onkeydown={(e) => { if (e.key === 'Escape') pwUser = null; }}>
		<div class="modal" role="dialog" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<h3>Change Password for "{pwUser}"</h3>
			<div class="field">
				<label for="pw-new">New Password</label>
				<input id="pw-new" type="password" bind:value={pwNew} placeholder="Min 8 characters" autocomplete="new-password" />
			</div>
			<div class="field">
				<label for="pw-confirm">Confirm Password</label>
				<input id="pw-confirm" type="password" bind:value={pwConfirm} autocomplete="new-password" />
				{#if pwConfirm && pwNew !== pwConfirm}
					<span class="field-error">Passwords do not match</span>
				{/if}
			</div>
			<div class="modal-actions">
				<button onclick={changePassword} disabled={!pwNew || pwNew.length < 8 || pwNew !== pwConfirm}>
					Change Password
				</button>
				<button class="secondary" onclick={() => pwUser = null}>Cancel</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.toolbar { margin: 1rem 0; }
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 400px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input, .field select { width: 100%; box-sizing: border-box; }
	.field-error { color: #f87171; font-size: 0.75rem; margin-top: 0.25rem; display: block; }
	.muted { color: #6b7280; }
	.badge { padding: 0.2rem 0.5rem; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
	.badge.admin { background: #1e3a5f; color: #60a5fa; }
	.badge.readonly { background: #374151; color: #9ca3af; }
	.actions { display: flex; gap: 0.5rem; }
	.modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.6); display: flex; align-items: center; justify-content: center; z-index: 100; }
	.modal { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; min-width: 350px; }
	.modal h3 { margin: 0 0 1rem; }
	.modal-actions { display: flex; gap: 0.5rem; }
</style>
