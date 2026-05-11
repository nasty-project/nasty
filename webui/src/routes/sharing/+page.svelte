<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import type { Subvolume, ProtocolStatus } from '$lib/types';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Card, CardContent } from '$lib/components/ui/card';
	import SortTh from '$lib/components/SortTh.svelte';
	import { AlertTriangle } from '@lucide/svelte';
	import NfsPanel from './NfsPanel.svelte';
	import SmbPanel from './SmbPanel.svelte';
	import IscsiPanel from './IscsiPanel.svelte';
	import NvmeofPanel from './NvmeofPanel.svelte';
	import {
		nfs,
		nfsRefresh,
		nfsLoadProtocol,
	} from '$lib/sharing/nfs.svelte';
	import {
		smb,
		smbRefresh,
		smbLoadProtocol,
		smbEnsureSystemUsers,
	} from '$lib/sharing/smb.svelte';
	import {
		iscsi,
		iscsiRefresh,
		iscsiLoadProtocol,
	} from '$lib/sharing/iscsi.svelte';
	import {
		nvme,
		nvmeRefresh,
		nvmeLoadProtocol,
	} from '$lib/sharing/nvmeof.svelte';

	// ── Share creation wizard ────────────────────────────
	let shareWizardStep: 0 | 1 | 2 | 3 | 4 = $state(0);
	let shareProtocol: Tab = $state('smb');
	let shareSubvolume = $state('');
	// NFS access
	let shareNfsHost = $state('');
	let shareNfsOptions = $state('rw,sync,no_subtree_check');
	// SMB access
	let shareSmbName = $state('');
	let shareSmbGuestOk = $state(false);
	let shareSmbReadOnly = $state(false);
	let shareSmbValidUsers: string[] = $state([]);
	let showInlineUserCreate = $state(false);
	let inlineUsername = $state('');
	let inlinePassword = $state('');
	let inlinePasswordConfirm = $state('');
	let inlineGroups: string[] = $state([]);
	let showInlineGroupCreate = $state(false);
	let inlineGroupName = $state('');
	// iSCSI access
	let shareIscsiName = $state('');
	// NVMe-oF access
	let shareNvmeofName = $state('');
	let shareNvmeofAddr = $state('0.0.0.0');
	let shareNvmeofPort = $state('4420');

	let shareSubvolumes: Subvolume[] = $state([]);

	// Inline subvolume creation within share wizard
	let showInlineCreate = $state(false);
	let inlineSvName = $state('');
	let inlineSvQuota = $state('');
	let inlineSvCreating = $state(false);
	let inlineSvFilesystems: string[] = $state([]);
	let inlineSvFs = $state('');

	async function loadInlineFilesystems() {
		try {
			const fsList = await client.call<{ name: string; mounted: boolean }[]>('fs.list');
			inlineSvFilesystems = fsList.filter(f => f.mounted).map(f => f.name);
			if (inlineSvFilesystems.length > 0 && !inlineSvFs) inlineSvFs = inlineSvFilesystems[0];
		} catch { inlineSvFilesystems = []; }
	}

	async function inlineCreateSubvolume() {
		if (!inlineSvName || !inlineSvFs) return;
		inlineSvCreating = true;
		const isBlock = shareProtocol === 'iscsi' || shareProtocol === 'nvmeof';
		const params: Record<string, unknown> = {
			filesystem: inlineSvFs,
			name: inlineSvName,
			subvolume_type: isBlock ? 'block' : 'filesystem',
		};
		if (inlineSvQuota) params.volsize_bytes = parseFloat(inlineSvQuota) * 1073741824;
		try {
			await withToast(
				() => client.call('subvolume.create', params),
				'Subvolume created'
			);
			await loadShareSubvolumes();
			// Auto-select the newly created subvolume
			const created = shareSubvolumes.find(sv => sv.name === inlineSvName && sv.filesystem === inlineSvFs);
			if (created) {
				shareSubvolume = isBlock ? (created.block_device ?? '') : created.path;
			}
			showInlineCreate = false;
			inlineSvName = '';
			inlineSvQuota = '';
		} finally {
			inlineSvCreating = false;
		}
	}

	function openShareWizard() {
		shareWizardStep = 1;
		shareProtocol = activeTab;
		shareSubvolume = '';
		shareNfsHost = ''; shareNfsOptions = 'rw,sync,no_subtree_check';
		shareSmbName = ''; shareSmbGuestOk = false; shareSmbReadOnly = false; shareSmbValidUsers = [];
		shareIscsiName = ''; shareNvmeofName = '';
		shareNvmeofAddr = '0.0.0.0'; shareNvmeofPort = '4420';
		showInlineCreate = false;
		inlineSvName = '';
		inlineSvQuota = '';
	}

	async function loadShareSubvolumes() {
		try {
			const all = await client.call<Subvolume[]>('subvolume.list_all');
			shareSubvolumes = all;
		} catch { shareSubvolumes = []; }
	}

	$effect(() => { if (shareWizardStep > 0) loadShareSubvolumes(); });

	const filteredShareSubvolumes = $derived.by(() => {
		const isBlock = shareProtocol === 'iscsi' || shareProtocol === 'nvmeof';
		return shareSubvolumes.filter(sv =>
			isBlock ? (sv.subvolume_type === 'block' && sv.block_device) : sv.subvolume_type === 'filesystem'
		);
	});

	async function createShare() {
		if (!shareSubvolume) return;
		const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume);
		if (!sv) return;

		let ok;
		if (shareProtocol === 'nfs') {
			ok = await withToast(
				() => client.call('share.nfs.create', {
					path: sv.path,
					clients: [{ host: shareNfsHost || '*', options: shareNfsOptions }],
				}),
				'NFS share created'
			);
		} else if (shareProtocol === 'smb') {
			ok = await withToast(
				() => client.call('share.smb.create', {
					name: shareSmbName || sv.name,
					path: sv.path,
					guest_ok: shareSmbGuestOk,
					read_only: shareSmbReadOnly,
					valid_users: shareSmbValidUsers,
				}),
				'SMB share created'
			);
		} else if (shareProtocol === 'iscsi') {
			ok = await withToast(
				() => client.call('share.iscsi.create', {
					name: shareIscsiName || sv.name,
					device_path: sv.block_device,
				}),
				'iSCSI target created'
			);
		} else if (shareProtocol === 'nvmeof') {
			ok = await withToast(
				() => client.call('share.nvmeof.create', {
					name: shareNvmeofName || sv.name,
					device_path: sv.block_device,
					addr: shareNvmeofAddr,
					port: parseInt(shareNvmeofPort) || 4420,
				}),
				'NVMe-oF subsystem created'
			);
		}
		if (ok !== undefined) {
			shareWizardStep = 0;
			nfsRefresh(); smbRefresh(); iscsiRefresh(); nvmeRefresh();
		}
	}

	// ── Tab state ────────────────────────────────────────
	type Tab = 'nfs' | 'smb' | 'iscsi' | 'nvmeof';
	const TABS: { key: Tab; label: string; hash: string }[] = [
		{ key: 'smb',    label: 'SMB',     hash: '#smb' },
		{ key: 'nfs',    label: 'NFS',     hash: '#nfs' },
		{ key: 'iscsi',  label: 'iSCSI',   hash: '#iscsi' },
		{ key: 'nvmeof', label: 'NVMe-oF', hash: '#nvmeof' },
	];

	function tabFromHash(): Tab {
		if (typeof window === 'undefined') return 'smb';
		const h = window.location.hash.replace('#', '');
		if (TABS.some(t => t.key === h)) return h as Tab;
		return 'smb';
	}

	let activeTab: Tab = $state(tabFromHash());

	function switchTab(tab: Tab) {
		activeTab = tab;
		// Sync wizard protocol with active tab
		if (shareWizardStep > 0) {
			shareProtocol = tab;
		}
		window.location.hash = tab;
	}

	const client = getClient();

	// ── NFS state ──
	// All NFS state + handlers live in lib/sharing/nfs.svelte.ts and
	// the <NfsPanel> component. We import only what the cross-protocol
	// wizard, onMount, and event broadcasts need to reach into:
	// `nfs.protocol` (for the wizard's "Protocol" step / toggle button),
	// `nfsRefresh()` (triggered after wizard create + on share.nfs
	// events), and `nfsLoadProtocol()` (triggered on protocol events).

	// ── SMB state ────────────────────────────────────────
	// SMB state + handlers live in lib/sharing/smb.svelte.ts and the
	// <SmbPanel> component. The page imports only what the cross-
	// protocol wizard, onMount, and event broadcasts need: `smb.protocol`
	// for the protocol toggle button, `smb.systemUsers` (and the
	// `smbEnsureSystemUsers()` lazy-loader) for the wizard's valid-users
	// picker, and the refresh/loadProtocol entry points.

	// ── iSCSI state ──────────────────────────────────────
	// iSCSI state + handlers live in lib/sharing/iscsi.svelte.ts and the
	// <IscsiPanel> component. The page imports only `iscsi.protocol` for
	// the wizard / toggle-button heading, and the refresh/loadProtocol
	// entry points for onMount + event broadcasts.

	// ── NVMe-oF state ───────────────────────────────────
	// NVMe-oF state + handlers live in lib/sharing/nvmeof.svelte.ts and
	// the <NvmeofPanel> component. The page imports only `nvme.protocol`
	// for the wizard / toggle button, and the refresh/loadProtocol entry
	// points for onMount + event broadcasts.

	async function toggleProtocol(name: string, currentlyEnabled: boolean) {
		const action = currentlyEnabled ? 'disable' : 'enable';
		await withToast(
			() => client.call(`service.protocol.${action}`, { name }),
			`${name} ${action}d`
		);
	}

	// ── Events & lifecycle ───────────────────────────────
	function handleEvent(_: string, params: unknown) {
		const p = params as { collection?: string };
		if (p?.collection === 'share.nfs') nfsRefresh();
		if (p?.collection === 'share.smb') smbRefresh();
		if (p?.collection === 'share.iscsi') iscsiRefresh();
		if (p?.collection === 'share.nvmeof') nvmeRefresh();
		if (p?.collection === 'protocol') {
			nfsLoadProtocol();
			smbLoadProtocol();
			iscsiLoadProtocol();
			nvmeLoadProtocol();
		}
	}

	onMount(async () => {
		client.onEvent(handleEvent);
		await Promise.all([
			nfsRefresh().then(() => { nfs.loading = false; }),
			smbRefresh().then(() => { smb.loading = false; }),
			iscsiRefresh().then(() => { iscsi.loading = false; }),
			nvmeRefresh().then(() => { nvme.loading = false; }),
			nfsLoadProtocol(),
			smbLoadProtocol(),
			iscsiLoadProtocol(),
			nvmeLoadProtocol(),
		]);
	});

	onDestroy(() => client.offEvent(handleEvent));
</script>

{#if shareWizardStep !== 0}
	<Card class="mb-6 max-w-2xl">
		<CardContent class="pt-6">
			<div class="mb-6 flex items-center gap-0">
				{#each [['1', 'Protocol'], ['2', 'Source'], ['3', 'Access'], ['4', 'Review']] as [num, label], i}
					<div class="flex items-center">
						<div class="flex items-center gap-2">
							<div class="flex h-6 w-6 items-center justify-center rounded-full text-xs font-semibold
								{shareWizardStep > i + 1 ? 'bg-primary text-primary-foreground' :
								 shareWizardStep === i + 1 ? 'bg-primary text-primary-foreground ring-2 ring-primary ring-offset-2 ring-offset-background' :
								 'bg-secondary text-muted-foreground'}">
								{num}
							</div>
							<span class="text-xs {shareWizardStep === i + 1 ? 'text-foreground font-medium' : 'text-muted-foreground'}">{label}</span>
						</div>
						{#if i < 3}
							<div class="mx-3 h-px w-8 bg-border"></div>
						{/if}
					</div>
				{/each}
			</div>

			<!-- Step 1: Protocol -->
			{#if shareWizardStep === 1}
			{@const selectedProto = ({ nfs: nfs.protocol, smb: smb.protocol, iscsi: iscsi.protocol, nvmeof: nvme.protocol })[shareProtocol]}
			<div class="mb-4">
				<Label>Protocol</Label>
				<select bind:value={shareProtocol} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="smb">SMB — Windows/Samba File Sharing</option>
					<option value="nfs">NFS — Network File System</option>
					<option value="iscsi">iSCSI — Block Storage over TCP</option>
					<option value="nvmeof">NVMe-oF — NVMe over Fabrics (TCP)</option>
				</select>
			</div>
			{#if selectedProto && !selectedProto.enabled}
				<div class="mb-4 flex items-center gap-3 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2.5">
					<AlertTriangle size={16} class="shrink-0 text-amber-500" />
					<span class="flex-1 text-sm">{selectedProto.display_name} service is not enabled.</span>
					<Button size="xs" onclick={async () => { await toggleProtocol(shareProtocol === 'nvmeof' ? 'nvmeof' : shareProtocol, false); await ({ nfs: nfsLoadProtocol, smb: smbLoadProtocol, iscsi: iscsiLoadProtocol, nvmeof: nvmeLoadProtocol })[shareProtocol](); }}>
						Enable
					</Button>
				</div>
			{/if}
			<div class="flex gap-2">
				<Button size="sm" onclick={() => shareWizardStep = 2} disabled={selectedProto != null && !selectedProto.enabled}>Next: Source →</Button>
			</div>

			<!-- Step 2: Source -->
			{:else if shareWizardStep === 2}
			<div class="mb-4">
				<Label>Subvolume</Label>
				<select bind:value={shareSubvolume} class="mt-1 h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm">
					<option value="">Select a subvolume...</option>
					{#each filteredShareSubvolumes as sv}
						{#if shareProtocol === 'iscsi' || shareProtocol === 'nvmeof'}
							<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
						{:else}
							<option value={sv.path}>{sv.filesystem}/{sv.name} ({sv.path})</option>
						{/if}
					{/each}
				</select>
				{#if filteredShareSubvolumes.length === 0 && !showInlineCreate}
					<p class="mt-1 text-xs text-muted-foreground">
						No {shareProtocol === 'iscsi' || shareProtocol === 'nvmeof' ? 'block' : 'filesystem'} subvolumes available.
					</p>
					<Button size="sm" class="mt-2" onclick={() => { showInlineCreate = true; loadInlineFilesystems(); }}>Create Subvolume</Button>
				{:else if !showInlineCreate}
					<Button size="sm" class="mt-2" onclick={() => { showInlineCreate = true; loadInlineFilesystems(); }}>Create Subvolume</Button>
				{/if}
				{#if showInlineCreate}
					<div class="mt-3 rounded-lg border border-border bg-secondary/20 p-3 space-y-3">
						<p class="text-xs font-medium">New {shareProtocol === 'iscsi' || shareProtocol === 'nvmeof' ? 'block' : 'filesystem'} subvolume</p>
						{#if inlineSvFilesystems.length > 1}
							<div>
								<Label class="text-xs">Filesystem</Label>
								<select bind:value={inlineSvFs} class="mt-1 h-8 w-full rounded-md border border-input bg-transparent px-2 text-sm">
									{#each inlineSvFilesystems as fs}
										<option value={fs}>{fs}</option>
									{/each}
								</select>
							</div>
						{/if}
						<div>
							<Label class="text-xs">Name</Label>
							<Input bind:value={inlineSvName} placeholder="e.g. media" class="mt-1 h-8 text-sm" />
						</div>
						<div>
							<Label class="text-xs">Quota (GiB) <span class="text-muted-foreground font-normal">{shareProtocol === 'iscsi' || shareProtocol === 'nvmeof' ? '— required for block' : '— optional'}</span></Label>
							<Input bind:value={inlineSvQuota} type="number" placeholder="e.g. 100" class="mt-1 h-8 text-sm" />
						</div>
						<div class="flex gap-2">
							<Button size="sm" onclick={inlineCreateSubvolume}
								disabled={!inlineSvName || !inlineSvFs || inlineSvCreating || ((shareProtocol === 'iscsi' || shareProtocol === 'nvmeof') && !inlineSvQuota)}>
								{inlineSvCreating ? 'Creating...' : 'Create'}
							</Button>
							<Button variant="secondary" size="sm" onclick={() => showInlineCreate = false}>Cancel</Button>
						</div>
					</div>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => shareWizardStep = 1}>← Back</Button>
				<Button size="sm" onclick={() => {
					// Auto-fill names from subvolume
					const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume);
					if (sv) {
						if (shareProtocol === 'smb' && !shareSmbName) shareSmbName = sv.name;
						if (shareProtocol === 'iscsi' && !shareIscsiName) shareIscsiName = sv.name;
						if (shareProtocol === 'nvmeof' && !shareNvmeofName) shareNvmeofName = sv.name;
					}
					shareWizardStep = 3;
					if (shareProtocol === 'smb') {
						smbEnsureSystemUsers();
					}
				}} disabled={!shareSubvolume}>Next: Access →</Button>
			</div>

			<!-- Step 3: Access (protocol-specific) -->
			{:else if shareWizardStep === 3}
			{#if shareProtocol === 'nfs'}
				<div class="mb-4">
					<Label>Allowed Network</Label>
					<Input bind:value={shareNfsHost} placeholder="192.168.1.0/24 or * for any" class="mt-1" />
				</div>
				<div class="mb-4">
					<Label>Export Options</Label>
					<Input bind:value={shareNfsOptions} class="mt-1" />
				</div>
			{:else if shareProtocol === 'smb'}
				<div class="mb-4">
					<Label>Share Name</Label>
					<Input bind:value={shareSmbName} placeholder="documents" class="mt-1" />
				</div>
				<div class="mb-4 flex gap-4">
					<label class="flex items-center gap-2 text-sm cursor-pointer">
						<input type="checkbox" bind:checked={shareSmbGuestOk} class="rounded border-input" />
						Allow guests
					</label>
					<label class="flex items-center gap-2 text-sm cursor-pointer">
						<input type="checkbox" bind:checked={shareSmbReadOnly} class="rounded border-input" />
						Read-only
					</label>
				</div>
				{#if !shareSmbGuestOk}
					<div class="mb-4">
						<Label>Allowed Users & Groups</Label>
						<p class="mt-1 mb-3 text-xs text-muted-foreground">Leave empty to allow all authenticated users.</p>

						{#if shareSmbValidUsers.length > 0}
							<div class="mb-3 rounded-md border border-green-500/30 bg-green-500/5 p-3">
								<p class="mb-2 text-[0.65rem] font-semibold uppercase tracking-wide text-green-400/70">Has access</p>
								<div class="flex flex-wrap gap-2">
									{#each shareSmbValidUsers as entry}
										<span class="flex items-center gap-1 rounded-md border border-green-500/30 bg-green-500/10 px-2 py-1 text-xs">
											{entry}
											<button class="ml-1 text-muted-foreground hover:text-destructive" onclick={() => { shareSmbValidUsers = shareSmbValidUsers.filter(u => u !== entry); }}>&times;</button>
										</span>
									{/each}
								</div>
							</div>
						{/if}

						{#if smb.systemUsers.some(u => !shareSmbValidUsers.includes(u.username)) || smb.groups.some(g => !shareSmbValidUsers.includes(`@${g.name}`))}
							<div class="mb-3 rounded-md border border-border p-3">
								<p class="mb-2 text-[0.65rem] font-semibold uppercase tracking-wide text-muted-foreground/70">Click to add</p>
								<div class="flex flex-wrap gap-2">
									{#each smb.systemUsers.filter(u => !shareSmbValidUsers.includes(u.username)) as user}
										<Button size="xs" variant="secondary" onclick={() => { shareSmbValidUsers = [...shareSmbValidUsers, user.username]; }}>
											{user.username}
										</Button>
									{/each}
									{#each smb.groups.filter(g => !shareSmbValidUsers.includes(`@${g.name}`)) as group}
										<Button size="xs" variant="secondary" class="text-blue-400" onclick={() => { shareSmbValidUsers = [...shareSmbValidUsers, `@${group.name}`]; }}>
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
									<div class="mb-4">
										<Label for="inline-username">Username</Label>
										<Input id="inline-username" bind:value={inlineUsername} placeholder="johndoe" autocomplete="off" class="mt-1" />
									</div>
									<div class="mb-4">
										<Label for="inline-password">Password</Label>
										<Input id="inline-password" type="password" bind:value={inlinePassword} autocomplete="new-password" class="mt-1" />
									</div>
									<div class="mb-4">
										<Label for="inline-password-confirm">Confirm Password</Label>
										<Input id="inline-password-confirm" type="password" bind:value={inlinePasswordConfirm} autocomplete="new-password" class="mt-1" />
										{#if inlinePasswordConfirm && inlinePassword !== inlinePasswordConfirm}
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
													<Input bind:value={inlineGroupName} placeholder="Group name" class="h-7 w-32 text-xs" />
													<Button size="xs" disabled={!inlineGroupName.trim()} onclick={async () => {
														await withToast(() => client.call('smb.group.create', { name: inlineGroupName.trim() }), `Group "${inlineGroupName}" created`);
														smb.groups = await client.call('smb.group.list');
														inlineGroups = [...inlineGroups, inlineGroupName.trim()];
														inlineGroupName = '';
														showInlineGroupCreate = false;
													}}>Create</Button>
													<Button size="xs" variant="secondary" onclick={() => showInlineGroupCreate = false}>Cancel</Button>
												</div>
											{:else}
												<Button size="sm" onclick={() => showInlineGroupCreate = true}>Create Group</Button>
											{/if}
										</div>
									</div>
									<div class="flex gap-2">
										<Button onclick={async () => {
											const ok = await withToast(
												() => client.call('smb.user.create', { username: inlineUsername, password: inlinePassword }),
												`User "${inlineUsername}" created`
											);
											if (ok !== undefined) {
												for (const g of inlineGroups) {
													await client.call('smb.group.add_member', { group: g, user: inlineUsername }).catch(() => {});
												}
												shareSmbValidUsers = [...shareSmbValidUsers, inlineUsername];
												smb.systemUsers = [...smb.systemUsers, { username: inlineUsername, uid: 0 }];
												showInlineUserCreate = false;
												inlineUsername = ''; inlinePassword = ''; inlinePasswordConfirm = ''; inlineGroups = [];
											}
										}} disabled={!inlineUsername || !inlinePassword || inlinePassword !== inlinePasswordConfirm}>
											Create & Add
										</Button>
										<Button variant="secondary" onclick={() => { showInlineUserCreate = false; }}>Cancel</Button>
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
			{:else if shareProtocol === 'iscsi'}
				<div class="mb-4">
					<Label>Target Name</Label>
					<Input bind:value={shareIscsiName} placeholder="dbserver" class="mt-1" />
					<p class="mt-1 text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{shareIscsiName || '...'}</p>
				</div>
			{:else if shareProtocol === 'nvmeof'}
				<div class="mb-4">
					<Label>Subsystem Name</Label>
					<Input bind:value={shareNvmeofName} placeholder="storage-vol" class="mt-1" />
				</div>
				<div class="grid grid-cols-2 gap-4 mb-4">
					<div>
						<Label>Listen Address</Label>
						<Input bind:value={shareNvmeofAddr} placeholder="0.0.0.0" class="mt-1" />
					</div>
					<div>
						<Label>Port</Label>
						<Input bind:value={shareNvmeofPort} placeholder="4420" class="mt-1" />
					</div>
				</div>
			{/if}
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => shareWizardStep = 2}>← Back</Button>
				<Button size="sm" onclick={() => shareWizardStep = 4}>Next: Review →</Button>
			</div>

			<!-- Step 4: Review -->
			{:else if shareWizardStep === 4}
			{@const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume)}
			<div class="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
				<span class="text-muted-foreground">Protocol</span>
				<span class="uppercase">{shareProtocol}</span>
				<span class="text-muted-foreground">Source</span>
				<span class="font-mono text-xs">{sv ? `${sv.filesystem}/${sv.name}` : shareSubvolume}</span>
				{#if shareProtocol === 'nfs'}
					<span class="text-muted-foreground">Allowed</span>
					<span>{shareNfsHost || '*'}</span>
					<span class="text-muted-foreground">Options</span>
					<span class="text-xs">{shareNfsOptions}</span>
				{:else if shareProtocol === 'smb'}
					<span class="text-muted-foreground">Share Name</span>
					<span>{shareSmbName || sv?.name}</span>
					{#if shareSmbGuestOk}<span class="text-muted-foreground">Guests</span><span>Allowed</span>{/if}
					{#if shareSmbReadOnly}<span class="text-muted-foreground">Access</span><span>Read-only</span>{/if}
					<span class="text-muted-foreground">Allowed Users</span>
					<span>{shareSmbValidUsers.length > 0 ? shareSmbValidUsers.join(', ') : 'All authenticated users'}</span>
				{:else if shareProtocol === 'iscsi'}
					<span class="text-muted-foreground">Target</span>
					<span class="font-mono text-xs">iqn.2137-01.com.nasty:{shareIscsiName || sv?.name}</span>
				{:else if shareProtocol === 'nvmeof'}
					<span class="text-muted-foreground">Subsystem</span>
					<span>{shareNvmeofName || sv?.name}</span>
					<span class="text-muted-foreground">Listen</span>
					<span>{shareNvmeofAddr}:{shareNvmeofPort}</span>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button variant="secondary" size="sm" onclick={() => shareWizardStep = 3}>← Back</Button>
				<Button size="sm" onclick={createShare}>Create Share</Button>
			</div>
			{/if}
		</CardContent>
	</Card>
{/if}

<!-- Tab bar with inline status -->
<div class="mb-6 flex items-center border-b border-border">
	{#each TABS as tab}
		{@const proto = ({ nfs: nfs.protocol, smb: smb.protocol, iscsi: iscsi.protocol, nvmeof: nvme.protocol })[tab.key]}
		{@const count = ({ nfs: nfs.shares.length, smb: smb.shares.length, iscsi: iscsi.targets.length, nvmeof: nvme.subsystems.length })[tab.key]}
		<button
			onclick={() => switchTab(tab.key)}
			class="flex items-center gap-2 px-4 py-2 text-sm font-medium transition-colors {activeTab === tab.key
				? 'border-b-2 border-primary text-foreground'
				: 'text-muted-foreground hover:text-foreground'}"
		>
			{tab.label}
			{#if proto}
				<span class="inline-block h-1.5 w-1.5 rounded-full {proto.running ? 'bg-green-500' : 'bg-muted-foreground/40'}"></span>
			{/if}
			{#if count > 0}
				<span class="text-[0.65rem] text-muted-foreground">{count}</span>
			{/if}
		</button>
	{/each}
</div>

<!-- Create Share button + wizard -->
<div class="mb-4">
	<Button size="sm" onclick={() => shareWizardStep === 0 ? openShareWizard() : (shareWizardStep = 0)}>
		{shareWizardStep !== 0 ? 'Cancel' : 'Create Share'}
	</Button>
</div>

<!-- ════════════════════════════════════════════════════ NFS ════════════════════════════════════════════════════ -->
{#if activeTab === 'nfs'}
	<NfsPanel />


<!-- ════════════════════════════════════════════════════ SMB ════════════════════════════════════════════════════ -->
{:else if activeTab === 'smb'}
	<SmbPanel />


<!-- ════════════════════════════════════════════════════ iSCSI ════════════════════════════════════════════════════ -->
{:else if activeTab === 'iscsi'}
	<IscsiPanel />


<!-- ════════════════════════════════════════════════════ NVMe-oF ════════════════════════════════════════════════════ -->
{:else if activeTab === 'nvmeof'}
	<NvmeofPanel />

{/if}
