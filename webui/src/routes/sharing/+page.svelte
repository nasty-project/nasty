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
	import NfsWizardForm from './wizard/NfsWizardForm.svelte';
	import SmbWizardForm from './wizard/SmbWizardForm.svelte';
	import IscsiWizardForm from './wizard/IscsiWizardForm.svelte';
	import NvmeofWizardForm from './wizard/NvmeofWizardForm.svelte';
	import NfsWizardReview from './wizard/NfsWizardReview.svelte';
	import SmbWizardReview from './wizard/SmbWizardReview.svelte';
	import IscsiWizardReview from './wizard/IscsiWizardReview.svelte';
	import NvmeofWizardReview from './wizard/NvmeofWizardReview.svelte';
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

	type Tab = 'nfs' | 'smb' | 'iscsi' | 'nvmeof';

	// ── Share creation wizard ────────────────────────────
	let shareWizardStep: 0 | 1 | 2 | 3 | 4 = $state(0);
	let shareProtocol = $state<Tab>('smb');
	let shareSubvolume = $state('');
	// NFS access
	let shareNfsHost = $state('');
	let shareNfsOptions = $state('rw,sync,no_subtree_check');
	// SMB access
	let shareSmbName = $state('');
	let shareSmbGuestOk = $state(false);
	let shareSmbReadOnly = $state(false);
	let shareSmbValidUsers: string[] = $state([]);
	// SMB inline user/group creation moved into <SmbWizardForm>.
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

	// Whether the currently-chosen protocol stores its data on a block
	// subvolume (iSCSI, NVMe-oF) instead of a filesystem subvolume
	// (NFS, SMB). Used by the source-picker, the inline-subvolume
	// creator, and the suggested-quota copy. Single derived = no more
	// inline `shareProtocol === 'iscsi' || shareProtocol === 'nvmeof'`
	// expressions across the wizard.
	const isBlock = $derived(shareProtocol === 'iscsi' || shareProtocol === 'nvmeof');

	async function inlineCreateSubvolume() {
		if (!inlineSvName || !inlineSvFs) return;
		inlineSvCreating = true;
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

	const filteredShareSubvolumes = $derived(
		shareSubvolumes.filter(sv =>
			isBlock ? (sv.subvolume_type === 'block' && sv.block_device) : sv.subvolume_type === 'filesystem'
		)
	);

	// Per-protocol "create one share" hook. Each entry knows its own
	// API method + payload shape; createShare() just dispatches. This
	// replaces a 4-way if/else cascade with a single registry lookup.
	const protocolCreators: Record<
		'nfs' | 'smb' | 'iscsi' | 'nvmeof',
		(sv: Subvolume) => Promise<unknown>
	> = {
		nfs: (sv) => withToast(
			() => client.call('share.nfs.create', {
				path: sv.path,
				clients: [{ host: shareNfsHost || '*', options: shareNfsOptions }],
			}),
			'NFS share created',
		),
		smb: (sv) => withToast(
			() => client.call('share.smb.create', {
				name: shareSmbName || sv.name,
				path: sv.path,
				guest_ok: shareSmbGuestOk,
				read_only: shareSmbReadOnly,
				valid_users: shareSmbValidUsers,
			}),
			'SMB share created',
		),
		iscsi: (sv) => withToast(
			() => client.call('share.iscsi.create', {
				name: shareIscsiName || sv.name,
				device_path: sv.block_device,
			}),
			'iSCSI target created',
		),
		nvmeof: (sv) => withToast(
			() => client.call('share.nvmeof.create', {
				name: shareNvmeofName || sv.name,
				device_path: sv.block_device,
				addr: shareNvmeofAddr,
				port: parseInt(shareNvmeofPort) || 4420,
			}),
			'NVMe-oF subsystem created',
		),
	};

	async function createShare() {
		if (!shareSubvolume) return;
		const sv = shareSubvolumes.find(s => s.path === shareSubvolume || s.block_device === shareSubvolume);
		if (!sv) return;
		const ok = await protocolCreators[shareProtocol](sv);
		if (ok !== undefined) {
			shareWizardStep = 0;
			nfsRefresh(); smbRefresh(); iscsiRefresh(); nvmeRefresh();
		}
	}

	// ── Tab state ────────────────────────────────────────
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
						{#if isBlock}
							<option value={sv.block_device}>{sv.filesystem}/{sv.name} ({sv.block_device})</option>
						{:else}
							<option value={sv.path}>{sv.filesystem}/{sv.name} ({sv.path})</option>
						{/if}
					{/each}
				</select>
				{#if filteredShareSubvolumes.length === 0 && !showInlineCreate}
					<p class="mt-1 text-xs text-muted-foreground">
						No {isBlock ? 'block' : 'filesystem'} subvolumes available.
					</p>
					<Button size="sm" class="mt-2" onclick={() => { showInlineCreate = true; loadInlineFilesystems(); }}>Create Subvolume</Button>
				{:else if !showInlineCreate}
					<Button size="sm" class="mt-2" onclick={() => { showInlineCreate = true; loadInlineFilesystems(); }}>Create Subvolume</Button>
				{/if}
				{#if showInlineCreate}
					<div class="mt-3 rounded-lg border border-border bg-secondary/20 p-3 space-y-3">
						<p class="text-xs font-medium">New {isBlock ? 'block' : 'filesystem'} subvolume</p>
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
							<Label class="text-xs">Quota (GiB) <span class="text-muted-foreground font-normal">{isBlock ? '— required for block' : '— optional'}</span></Label>
							<Input bind:value={inlineSvQuota} type="number" placeholder="e.g. 100" class="mt-1 h-8 text-sm" />
						</div>
						<div class="flex gap-2">
							<Button size="sm" onclick={inlineCreateSubvolume}
								disabled={!inlineSvName || !inlineSvFs || inlineSvCreating || (isBlock && !inlineSvQuota)}>
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
				<NfsWizardForm bind:host={shareNfsHost} bind:options={shareNfsOptions} />
			{:else if shareProtocol === 'smb'}
				<SmbWizardForm
					bind:name={shareSmbName}
					bind:guestOk={shareSmbGuestOk}
					bind:readOnly={shareSmbReadOnly}
					bind:validUsers={shareSmbValidUsers}
				/>
			{:else if shareProtocol === 'iscsi'}
				<IscsiWizardForm bind:name={shareIscsiName} />
			{:else if shareProtocol === 'nvmeof'}
				<NvmeofWizardForm
					bind:name={shareNvmeofName}
					bind:addr={shareNvmeofAddr}
					bind:port={shareNvmeofPort}
				/>
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
					<NfsWizardReview host={shareNfsHost} options={shareNfsOptions} />
				{:else if shareProtocol === 'smb'}
					<SmbWizardReview
						name={shareSmbName}
						fallbackName={sv?.name ?? ''}
						guestOk={shareSmbGuestOk}
						readOnly={shareSmbReadOnly}
						validUsers={shareSmbValidUsers}
					/>
				{:else if shareProtocol === 'iscsi'}
					<IscsiWizardReview name={shareIscsiName} fallbackName={sv?.name ?? ''} />
				{:else if shareProtocol === 'nvmeof'}
					<NvmeofWizardReview
						name={shareNvmeofName}
						fallbackName={sv?.name ?? ''}
						addr={shareNvmeofAddr}
						port={shareNvmeofPort}
					/>
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
