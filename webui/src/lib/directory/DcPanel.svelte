<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast, success } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { requiredFieldCls } from '$lib/utils';
	import { dc, dcLoadPrincipals, dcDemote } from '$lib/dc.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';

	const client = getClient();

	let activeTab: 'users' | 'groups' | 'computers' = $state('users');

	onMount(() => {
		dcLoadPrincipals();
	});

	// ── Users tab ────────────────────────────────────────────────
	let showAddUser = $state(false);
	let newUserName = $state('');
	let newUserPassword = $state('');
	let newUserGiven = $state('');
	let newUserSurname = $state('');
	let newUserTried = $state(false);
	let userBusy: string | null = $state(null);

	let pwUser: string | null = $state(null);
	let pwNew = $state('');
	let pwTried = $state(false);

	async function createUser() {
		if (!newUserName.trim() || !newUserPassword) { newUserTried = true; return; }
		newUserTried = false;
		const params: Record<string, unknown> = {
			name: newUserName.trim(),
			password: newUserPassword,
		};
		if (newUserGiven.trim()) params.given_name = newUserGiven.trim();
		if (newUserSurname.trim()) params.surname = newUserSurname.trim();
		const ok = await withToast(
			() => client.call('dc.user.create', params),
			`User "${newUserName.trim()}" created`,
		);
		newUserPassword = '';
		if (ok !== undefined) {
			showAddUser = false;
			newUserName = '';
			newUserGiven = '';
			newUserSurname = '';
			newUserTried = false;
			await dcLoadPrincipals();
		}
	}

	async function deleteUser(name: string) {
		if (!await confirm(`Delete user "${name}"?`, 'This permanently removes the AD account.')) return;
		await withToast(() => client.call('dc.user.delete', { name }), `User "${name}" deleted`);
		await dcLoadPrincipals();
	}

	async function setUserEnabled(name: string, enabled: boolean) {
		userBusy = name;
		await withToast(
			() => client.call(enabled ? 'dc.user.enable' : 'dc.user.disable', { name }),
			`User "${name}" ${enabled ? 'enabled' : 'disabled'}`,
		);
		userBusy = null;
		await dcLoadPrincipals();
	}

	function openResetPassword(name: string) {
		pwUser = pwUser === name ? null : name;
		pwNew = '';
		pwTried = false;
	}

	async function resetPassword() {
		if (!pwUser) return;
		if (!pwNew) { pwTried = true; return; }
		pwTried = false;
		const ok = await withToast(
			() => client.call('dc.user.set_password', { name: pwUser, password: pwNew }),
			`Password reset for "${pwUser}"`,
		);
		pwNew = '';
		if (ok !== undefined) {
			pwUser = null;
			pwTried = false;
			await dcLoadPrincipals();
		}
	}

	// ── Groups tab ───────────────────────────────────────────────
	let newGroupName = $state('');
	let newGroupTried = $state(false);
	let memberGroup = $state('');
	let memberName = $state('');

	async function createGroup() {
		if (!newGroupName.trim()) { newGroupTried = true; return; }
		newGroupTried = false;
		const ok = await withToast(
			() => client.call('dc.group.create', { name: newGroupName.trim() }),
			`Group "${newGroupName.trim()}" created`,
		);
		if (ok !== undefined) {
			newGroupName = '';
			newGroupTried = false;
			await dcLoadPrincipals();
		}
	}

	async function deleteGroup(name: string) {
		if (!await confirm(`Delete group "${name}"?`, 'Members lose whatever access this group granted.')) return;
		await withToast(() => client.call('dc.group.delete', { name }), `Group "${name}" deleted`);
		await dcLoadPrincipals();
	}

	async function addMember() {
		if (!memberGroup.trim() || !memberName.trim()) return;
		await withToast(
			() => client.call('dc.group.add_member', { group: memberGroup.trim(), member: memberName.trim() }),
			`Added "${memberName.trim()}" to "${memberGroup.trim()}"`,
		);
		memberName = '';
		await dcLoadPrincipals();
	}

	async function removeMember() {
		if (!memberGroup.trim() || !memberName.trim()) return;
		await withToast(
			() => client.call('dc.group.remove_member', { group: memberGroup.trim(), member: memberName.trim() }),
			`Removed "${memberName.trim()}" from "${memberGroup.trim()}"`,
		);
		memberName = '';
		await dcLoadPrincipals();
	}

	// ── Back up domain ───────────────────────────────────────────
	let backupDest = $state('');
	let backingUp = $state(false);

	async function runBackup() {
		if (!backupDest.trim()) return;
		backingUp = true;
		const res = await withToast(() => client.call<{ path: string }>('dc.backup', { dest: backupDest.trim() }));
		backingUp = false;
		if (res) {
			success(`Backup written to ${res.path}`);
			backupDest = '';
		}
	}

	// ── Danger zone: demote ──────────────────────────────────────
	let demoteOpen = $state(false);
	let demoteTyped = $state('');
	let demoting = $state(false);

	async function doDemote() {
		demoting = true;
		const ok = await dcDemote(demoteTyped);
		demoting = false;
		if (ok) {
			demoteOpen = false;
			demoteTyped = '';
		}
	}
</script>

<!-- Header: realm, workgroup, health -->
<div class="mb-5 flex flex-wrap items-center justify-between gap-3">
	<div>
		<div class="flex items-center gap-2">
			<span class="text-sm font-medium font-mono">{dc.status?.realm ?? '—'}</span>
			<Badge variant="outline" class="text-[0.65rem]">{dc.status?.workgroup ?? '—'}</Badge>
		</div>
		<div class="mt-1.5 flex items-center gap-1.5">
			<span class="h-2 w-2 rounded-full shrink-0 {dc.status?.service_healthy ? 'bg-green-400' : 'bg-amber-400'}"></span>
			<span class="text-xs text-muted-foreground">
				{dc.status?.service_healthy ? 'Running' : 'Not running — check journalctl -u samba-dc'}
			</span>
		</div>
	</div>
</div>

<!-- Tabs -->
<div class="mb-4 flex w-fit rounded-md border border-border text-xs">
	<button
		onclick={() => activeTab = 'users'}
		class="rounded-l-md px-3 py-1 font-medium transition-colors {activeTab === 'users' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
	>Users</button>
	<button
		onclick={() => activeTab = 'groups'}
		class="px-3 py-1 font-medium transition-colors {activeTab === 'groups' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
	>Groups</button>
	<button
		onclick={() => activeTab = 'computers'}
		class="rounded-r-md px-3 py-1 font-medium transition-colors {activeTab === 'computers' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}"
	>Computers</button>
</div>

{#if activeTab === 'users'}
	{#if dc.users.length === 0}
		<p class="mb-3 text-sm text-muted-foreground">No users.</p>
	{:else}
		<div class="mb-3 divide-y divide-border rounded-md border border-border">
			{#each dc.users as user (user.name)}
				<div class="flex flex-wrap items-center justify-between gap-2 px-3 py-2 text-sm">
					<span class="font-mono">{user.name}</span>
					<div class="flex flex-wrap gap-1.5">
						<Button size="xs" variant="secondary" disabled={userBusy === user.name} onclick={() => setUserEnabled(user.name, true)}>Enable</Button>
						<Button size="xs" variant="secondary" disabled={userBusy === user.name} onclick={() => setUserEnabled(user.name, false)}>Disable</Button>
						<Button size="xs" variant="secondary" onclick={() => openResetPassword(user.name)}>Reset password</Button>
						<Button size="xs" variant="destructive" onclick={() => deleteUser(user.name)}>Delete</Button>
					</div>
				</div>
				{#if pwUser === user.name}
					<div class="flex flex-wrap items-end gap-2 border-t border-border bg-muted/20 px-3 py-2">
						<div class="min-w-[10rem] flex-1">
							<label for="dc-pw-{user.name}" class="text-xs text-muted-foreground">New password {#if !pwNew && pwTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</label>
							<input
								id="dc-pw-{user.name}"
								type="password"
								bind:value={pwNew}
								class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm focus:outline-none focus:ring-2 focus:ring-ring {requiredFieldCls(!pwNew, pwTried)}"
							/>
						</div>
						<Button size="xs" onclick={resetPassword}>Save</Button>
						<Button size="xs" variant="secondary" onclick={() => { pwUser = null; pwNew = ''; pwTried = false; }}>Cancel</Button>
					</div>
				{/if}
			{/each}
		</div>
	{/if}

	<div class="mb-3">
		<Button size="sm" onclick={() => showAddUser = !showAddUser}>{showAddUser ? 'Cancel' : 'Add user'}</Button>
	</div>
	{#if showAddUser}
		<div class="mb-4 space-y-3 rounded-md border border-border p-3">
			<div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
				<div>
					<label for="dc-new-user-name" class="text-xs text-muted-foreground">Name {#if !newUserName.trim() && newUserTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</label>
					<input
						id="dc-new-user-name"
						type="text"
						bind:value={newUserName}
						placeholder="jdoe"
						class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm focus:outline-none focus:ring-2 focus:ring-ring {requiredFieldCls(!newUserName.trim(), newUserTried)}"
					/>
				</div>
				<div>
					<label for="dc-new-user-pw" class="text-xs text-muted-foreground">Password {#if !newUserPassword && newUserTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</label>
					<input
						id="dc-new-user-pw"
						type="password"
						bind:value={newUserPassword}
						class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm focus:outline-none focus:ring-2 focus:ring-ring {requiredFieldCls(!newUserPassword, newUserTried)}"
					/>
				</div>
				<div>
					<label for="dc-new-user-given" class="text-xs text-muted-foreground">Given name</label>
					<input id="dc-new-user-given" type="text" bind:value={newUserGiven} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm focus:outline-none focus:ring-2 focus:ring-ring" />
				</div>
				<div>
					<label for="dc-new-user-surname" class="text-xs text-muted-foreground">Surname</label>
					<input id="dc-new-user-surname" type="text" bind:value={newUserSurname} class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm focus:outline-none focus:ring-2 focus:ring-ring" />
				</div>
			</div>
			<!-- Stays enabled; createUser validates and triggers the amber decoration on a missing-field click. -->
			<Button size="sm" onclick={createUser}>Create</Button>
		</div>
	{/if}
{:else if activeTab === 'groups'}
	{#if dc.groups.length === 0}
		<p class="mb-3 text-sm text-muted-foreground">No groups.</p>
	{:else}
		<div class="mb-3 divide-y divide-border rounded-md border border-border">
			{#each dc.groups as group (group.name)}
				<div class="flex items-center justify-between gap-2 px-3 py-2 text-sm">
					<span class="font-mono">{group.name}</span>
					<Button size="xs" variant="destructive" onclick={() => deleteGroup(group.name)}>Delete</Button>
				</div>
			{/each}
		</div>
	{/if}

	<div class="mb-4 flex flex-wrap items-end gap-2">
		<div>
			<label for="dc-new-group" class="text-xs text-muted-foreground">New group {#if !newGroupName.trim() && newGroupTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</label>
			<input
				id="dc-new-group"
				type="text"
				bind:value={newGroupName}
				placeholder="engineering"
				class="mt-1 rounded-md border border-input bg-background px-2 py-1 text-sm focus:outline-none focus:ring-2 focus:ring-ring {requiredFieldCls(!newGroupName.trim(), newGroupTried)}"
			/>
		</div>
		<Button size="sm" onclick={createGroup}>Add group</Button>
	</div>

	<div class="rounded-md border border-border p-3">
		<h4 class="mb-2 text-xs font-semibold uppercase text-muted-foreground">Membership</h4>
		<div class="flex flex-wrap items-end gap-2">
			<div>
				<label for="dc-member-group" class="text-xs text-muted-foreground">Group</label>
				<select id="dc-member-group" bind:value={memberGroup} class="mt-1 h-8 rounded-md border border-input bg-transparent px-2 text-sm">
					<option value="">Select group…</option>
					{#each dc.groups as g (g.name)}
						<option value={g.name}>{g.name}</option>
					{/each}
				</select>
			</div>
			<div>
				<label for="dc-member-name" class="text-xs text-muted-foreground">Member (user or computer)</label>
				<input id="dc-member-name" type="text" bind:value={memberName} placeholder="jdoe" class="mt-1 h-8 rounded-md border border-input bg-background px-2 text-sm focus:outline-none focus:ring-2 focus:ring-ring" />
			</div>
			<Button size="xs" onclick={addMember} disabled={!memberGroup.trim() || !memberName.trim()}>Add</Button>
			<Button size="xs" variant="secondary" onclick={removeMember} disabled={!memberGroup.trim() || !memberName.trim()}>Remove</Button>
		</div>
	</div>
{:else}
	{#if dc.computers.length === 0}
		<p class="text-sm text-muted-foreground">No computers joined.</p>
	{:else}
		<div class="divide-y divide-border rounded-md border border-border">
			{#each dc.computers as computer (computer.name)}
				<div class="px-3 py-2 text-sm font-mono">{computer.name}</div>
			{/each}
		</div>
	{/if}
{/if}

<div class="my-6 border-t border-border"></div>

<!-- Back up domain -->
<h3 class="mb-2 text-sm font-semibold">Back up domain</h3>
<div class="mb-2 flex flex-wrap items-end gap-2">
	<div class="min-w-[16rem] flex-1">
		<label for="dc-backup-dest" class="text-xs text-muted-foreground">Destination</label>
		<input
			id="dc-backup-dest"
			type="text"
			bind:value={backupDest}
			placeholder="/fs/tank/dc-backups/2026-07-10"
			class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-ring"
		/>
	</div>
	<Button size="sm" disabled={!backupDest.trim() || backingUp} onclick={runBackup}>
		{backingUp ? 'Backing up…' : 'Back up now'}
	</Button>
</div>

<div class="my-6 border-t border-border"></div>

<!-- Danger zone -->
<h3 class="mb-1 text-sm font-semibold text-destructive">Danger zone</h3>
{#if !demoteOpen}
	<p class="mb-3 text-xs text-muted-foreground">Demoting destroys this domain — every user, group, and joined machine's trust.</p>
	<Button size="sm" variant="destructive" onclick={() => demoteOpen = true}>Demote domain</Button>
{:else}
	<div class="space-y-3 rounded-md border border-destructive/40 bg-destructive/10 p-3">
		<p class="text-xs text-destructive">
			Destroys the domain: every user, group, and joined machine's trust. A final backup is written to /fs first when a filesystem exists.
		</p>
		<div>
			<label for="dc-demote-realm" class="text-xs text-muted-foreground">Type the realm ({dc.status?.realm}) to confirm</label>
			<input
				id="dc-demote-realm"
				type="text"
				bind:value={demoteTyped}
				class="mt-1 w-full rounded-md border border-input bg-background px-2 py-1 text-sm font-mono focus:outline-none focus:ring-2 focus:ring-ring"
			/>
		</div>
		<div class="flex gap-2">
			<Button size="sm" variant="destructive" disabled={demoteTyped !== dc.status?.realm || demoting} onclick={doDemote}>
				{demoting ? 'Demoting…' : 'Demote'}
			</Button>
			<Button size="sm" variant="secondary" onclick={() => { demoteOpen = false; demoteTyped = ''; }}>Cancel</Button>
		</div>
	</div>
{/if}
