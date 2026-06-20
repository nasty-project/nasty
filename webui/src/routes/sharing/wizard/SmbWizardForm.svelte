<script lang="ts">
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import type { SmbGroup } from '$lib/types';
	import { smb } from '$lib/sharing/smb.svelte';
	import { requiredFieldCls } from '$lib/utils';

	interface Props {
		name: string;
		guestOk: boolean;
		readOnly: boolean;
		validUsers: string[];
		timeMachine: boolean;
		maxSizeGib: number | null;
	}
	let {
		name = $bindable(),
		guestOk = $bindable(),
		readOnly = $bindable(),
		validUsers = $bindable(),
		timeMachine = $bindable(),
		maxSizeGib = $bindable(),
	}: Props = $props();

	const client = getClient();

	// Inline user/group creation flow lives in the component because
	// these fields are only meaningful while the SMB step of the wizard
	// is open. Hoisting them onto the page would leak them across
	// protocol switches.
	let showInlineUserCreate = $state(false);
	let inlineUsername = $state('');
	let inlinePassword = $state('');
	let inlinePasswordConfirm = $state('');
	let inlineGroups: string[] = $state([]);
	let showInlineGroupCreate = $state(false);
	let inlineGroupName = $state('');

	// Per-form "tried" flags — defer amber required-field decoration
	// until each submit button is clicked at least once.
	let createUserTried = $state(false);
	let createGroupTried = $state(false);

	async function createInlineGroup() {
		if (!inlineGroupName.trim()) { createGroupTried = true; return; }
		createGroupTried = false;
		await withToast(
			() => client.call('smb.group.create', { name: inlineGroupName.trim() }),
			`Group "${inlineGroupName}" created`
		);
		smb.groups = await client.call<SmbGroup[]>('smb.group.list');
		inlineGroups = [...inlineGroups, inlineGroupName.trim()];
		inlineGroupName = '';
		showInlineGroupCreate = false;
	}

	async function createInlineUser() {
		if (!inlineUsername || !inlinePassword || !inlinePasswordConfirm) { createUserTried = true; return; }
		if (inlinePassword !== inlinePasswordConfirm) { createUserTried = true; return; }
		createUserTried = false;
		const ok = await withToast(
			() => client.call('smb.user.create', { username: inlineUsername, password: inlinePassword }),
			`User "${inlineUsername}" created`
		);
		if (ok !== undefined) {
			for (const g of inlineGroups) {
				await client.call('smb.group.add_member', { group: g, user: inlineUsername }).catch(() => {});
			}
			validUsers = [...validUsers, inlineUsername];
			smb.systemUsers = [...smb.systemUsers, { username: inlineUsername, uid: 0 }];
			showInlineUserCreate = false;
			inlineUsername = ''; inlinePassword = ''; inlinePasswordConfirm = ''; inlineGroups = [];
		}
	}
</script>

<div class="mb-4">
	<Label>Share Name</Label>
	<Input bind:value={name} placeholder="documents" class="mt-1" />
</div>
<div class="mb-4">
	<label class="flex items-center gap-2 text-sm cursor-pointer">
		<input
			type="checkbox"
			bind:checked={timeMachine}
			onchange={() => { if (timeMachine) { guestOk = false; readOnly = false; } }}
			class="rounded border-input" />
		Time Machine — macOS backup destination
	</label>
	{#if timeMachine}
		<div class="mt-2 ml-6 space-y-2">
			<div class="flex items-center gap-2 text-sm">
				<Label class="font-normal">Max size (GiB)</Label>
				<input
					type="number"
					min="1"
					placeholder="unlimited"
					value={maxSizeGib ?? ''}
					oninput={(e) => {
						const v = (e.target as HTMLInputElement).value;
						maxSizeGib = v === '' ? null : Number(v);
					}}
					class="h-8 w-32 rounded-md border border-input bg-transparent px-2 text-sm" />
			</div>
			<p class="text-xs text-muted-foreground">
				macOS thins old backups to stay under this. Pair it with a subvolume quota
				as a hard cap. Time Machine shares are authenticated and writable — add the
				one user who will back up below.
			</p>
		</div>
	{/if}
</div>
{#if !timeMachine}
	<div class="mb-4 flex gap-4">
		<label class="flex items-center gap-2 text-sm cursor-pointer">
			<input type="checkbox" bind:checked={guestOk} class="rounded border-input" />
			Allow guests
		</label>
		<label class="flex items-center gap-2 text-sm cursor-pointer">
			<input type="checkbox" bind:checked={readOnly} class="rounded border-input" />
			Read-only
		</label>
	</div>
{/if}
{#if !guestOk}
	<div class="mb-4">
		<Label>Allowed Users & Groups</Label>
		<p class="mt-1 mb-3 text-xs text-muted-foreground">Leave empty to allow all authenticated users.</p>

		{#if validUsers.length > 0}
			<div class="mb-3 rounded-md border border-green-500/30 bg-green-500/5 p-3">
				<p class="mb-2 text-[0.65rem] font-semibold uppercase tracking-wide text-green-400/70">Has access</p>
				<div class="flex flex-wrap gap-2">
					{#each validUsers as entry}
						<span class="flex items-center gap-1 rounded-md border border-green-500/30 bg-green-500/10 px-2 py-1 text-xs">
							{entry}
							<button class="ml-1 text-muted-foreground hover:text-destructive" onclick={() => { validUsers = validUsers.filter(u => u !== entry); }}>&times;</button>
						</span>
					{/each}
				</div>
			</div>
		{/if}

		{#if smb.systemUsers.some(u => !validUsers.includes(u.username)) || smb.groups.some(g => !validUsers.includes(`@${g.name}`))}
			<div class="mb-3 rounded-md border border-border p-3">
				<p class="mb-2 text-[0.65rem] font-semibold uppercase tracking-wide text-muted-foreground/70">Click to add</p>
				<div class="flex flex-wrap gap-2">
					{#each smb.systemUsers.filter(u => !validUsers.includes(u.username)) as user}
						<Button size="xs" variant="secondary" onclick={() => { validUsers = [...validUsers, user.username]; }}>
							{user.username}
						</Button>
					{/each}
					{#each smb.groups.filter(g => !validUsers.includes(`@${g.name}`)) as group}
						<Button size="xs" variant="secondary" class="text-blue-400" onclick={() => { validUsers = [...validUsers, `@${group.name}`]; }}>
							@{group.name}
						</Button>
					{/each}
				</div>
			</div>
		{/if}
		{#if showInlineUserCreate}
			<Card class="mt-3 max-w-md">
				<CardContent class="pt-4">
					<h3 class="mb-4 text-lg font-semibold">New System User</h3>
					{@const inlinePwMismatch = !!inlinePasswordConfirm && inlinePassword !== inlinePasswordConfirm}
					<div class="mb-4">
						<Label for="inline-username">Username {#if !inlineUsername && createUserTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
						<Input id="inline-username" bind:value={inlineUsername} placeholder="johndoe" autocomplete="off" class="mt-1 {requiredFieldCls(!inlineUsername, createUserTried)}" />
					</div>
					<div class="mb-4">
						<Label for="inline-password">Password {#if !inlinePassword && createUserTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
						<Input id="inline-password" type="password" bind:value={inlinePassword} autocomplete="new-password" class="mt-1 {requiredFieldCls(!inlinePassword, createUserTried)}" />
					</div>
					<div class="mb-4">
						<Label for="inline-password-confirm">Confirm Password {#if !inlinePasswordConfirm && createUserTried}<span class="text-xs font-normal text-amber-500">required</span>{/if}</Label>
						<Input id="inline-password-confirm" type="password" bind:value={inlinePasswordConfirm} autocomplete="new-password" class="mt-1 {requiredFieldCls(!inlinePasswordConfirm, createUserTried) || requiredFieldCls(inlinePwMismatch)}" />
						{#if inlinePwMismatch}
							<span class="mt-1 block text-xs text-destructive">Passwords do not match</span>
						{/if}
					</div>
					<div class="mb-4">
						<Label>Add to Groups</Label>
						<div class="mt-1 flex flex-wrap gap-2">
							{#each smb.groups as group}
								<label class="flex items-center gap-1.5 text-sm cursor-pointer rounded border border-border px-2 py-1 hover:bg-muted/30">
									<input type="checkbox" class="rounded border-input"
										onchange={(e) => {
											const checked = (e.target as HTMLInputElement).checked;
											if (checked) inlineGroups = [...inlineGroups, group.name];
											else inlineGroups = inlineGroups.filter(g => g !== group.name);
										}}
										checked={inlineGroups.includes(group.name)}
									/>
									{group.name}
								</label>
							{/each}
							{#if showInlineGroupCreate}
								<div class="flex items-center gap-1.5">
									<Input bind:value={inlineGroupName} placeholder="Group name" class="h-7 w-32 text-xs {requiredFieldCls(!inlineGroupName.trim(), createGroupTried)}" />
									<Button size="xs" onclick={createInlineGroup}>Create</Button>
									<Button size="xs" variant="secondary" onclick={() => { showInlineGroupCreate = false; createGroupTried = false; }}>Cancel</Button>
								</div>
							{:else}
								<Button size="sm" onclick={() => showInlineGroupCreate = true}>Create Group</Button>
							{/if}
						</div>
					</div>
					<div class="flex gap-2">
						<Button onclick={createInlineUser} disabled={inlinePwMismatch}>
							Create & Add
						</Button>
						<Button variant="secondary" onclick={() => { showInlineUserCreate = false; createUserTried = false; }}>Cancel</Button>
					</div>
				</CardContent>
			</Card>
		{:else}
			<div class="mt-2 flex gap-2">
				<Button size="sm" onclick={() => showInlineUserCreate = true}>Create System User</Button>
			</div>
		{/if}
	</div>
{/if}
