<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import type { NvmeofSubsystem } from '$lib/types';

	let subsystems: NvmeofSubsystem[] = $state([]);
	let showCreate = $state(false);
	let loading = $state(true);

	let newName = $state('');

	// Add namespace
	let nsSubsys = $state<string | null>(null);
	let nsDevicePath = $state('');

	// Add port
	let portSubsys = $state<string | null>(null);
	let portTransport = $state('tcp');
	let portAddr = $state('0.0.0.0');
	let portSvcId = $state(4420);

	const client = getClient();

	onMount(async () => {
		await refresh();
		loading = false;
	});

	async function refresh() {
		await withToast(async () => {
			subsystems = await client.call<NvmeofSubsystem[]>('share.nvmeof.list');
		});
	}

	async function create() {
		if (!newName) return;
		const ok = await withToast(
			() => client.call('share.nvmeof.create', { name: newName }),
			'NVMe-oF subsystem created'
		);
		if (ok !== undefined) {
			showCreate = false;
			newName = '';
			await refresh();
		}
	}

	async function remove(id: string) {
		if (!confirm('Delete this NVMe-oF subsystem?')) return;
		await withToast(
			() => client.call('share.nvmeof.delete', { id }),
			'NVMe-oF subsystem deleted'
		);
		await refresh();
	}

	async function addNamespace() {
		if (!nsSubsys || !nsDevicePath) return;
		const ok = await withToast(
			() => client.call('share.nvmeof.add_namespace', {
				subsystem_id: nsSubsys,
				device_path: nsDevicePath,
			}),
			'Namespace added'
		);
		if (ok !== undefined) {
			nsSubsys = null;
			nsDevicePath = '';
			await refresh();
		}
	}

	async function removeNamespace(subsysId: string, nsid: number) {
		if (!confirm(`Remove namespace ${nsid}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_namespace', { subsystem_id: subsysId, nsid }),
			'Namespace removed'
		);
		await refresh();
	}

	async function addPort() {
		if (!portSubsys) return;
		const ok = await withToast(
			() => client.call('share.nvmeof.add_port', {
				subsystem_id: portSubsys,
				transport: portTransport,
				addr: portAddr,
				service_id: portSvcId,
			}),
			'Port added'
		);
		if (ok !== undefined) {
			portSubsys = null;
			await refresh();
		}
	}

	async function removePort(subsysId: string, portId: number) {
		if (!confirm(`Remove port ${portId}?`)) return;
		await withToast(
			() => client.call('share.nvmeof.remove_port', { subsystem_id: subsysId, port_id: portId }),
			'Port removed'
		);
		await refresh();
	}
</script>

<h1>NVMe-oF Subsystems</h1>

<div class="toolbar">
	<button onclick={() => showCreate = !showCreate}>
		{showCreate ? 'Cancel' : 'Create Subsystem'}
	</button>
</div>

{#if showCreate}
	<div class="form-card">
		<h3>New NVMe-oF Subsystem</h3>
		<div class="field">
			<label for="nvme-name">Name</label>
			<input id="nvme-name" bind:value={newName} placeholder="faststore" />
			<span class="hint">NQN: nqn.2024-01.com.nasty:{newName || '...'}</span>
		</div>
		<button onclick={create} disabled={!newName}>Create</button>
	</div>
{/if}

{#if loading}
	<p>Loading...</p>
{:else if subsystems.length === 0}
	<p class="muted">No NVMe-oF subsystems configured.</p>
{:else}
	{#each subsystems as subsys}
		<div class="card">
			<div class="card-header">
				<div>
					<strong class="mono">{subsys.nqn}</strong>
					<div class="muted">
						{subsys.allow_any_host ? 'Any host allowed' : `${subsys.allowed_hosts.length} allowed host(s)`}
					</div>
				</div>
				<div class="actions">
					<button class="secondary" onclick={() => { nsSubsys = subsys.id; nsDevicePath = ''; }}>Add Namespace</button>
					<button class="secondary" onclick={() => { portSubsys = subsys.id; portAddr = '0.0.0.0'; portSvcId = 4420; }}>Add Port</button>
					<button class="danger" onclick={() => remove(subsys.id)}>Delete</button>
				</div>
			</div>

			<div class="section">
				<h4>Namespaces</h4>
				{#if subsys.namespaces.length === 0}
					<span class="muted">No namespaces</span>
				{:else}
					<table class="inner-table">
						<thead><tr><th>NSID</th><th>Device</th><th>Status</th><th></th></tr></thead>
						<tbody>
							{#each subsys.namespaces as ns}
								<tr>
									<td>{ns.nsid}</td>
									<td class="mono">{ns.device_path}</td>
									<td>
										<span class="badge" class:enabled={ns.enabled}>{ns.enabled ? 'Enabled' : 'Disabled'}</span>
									</td>
									<td><button class="danger small" onclick={() => removeNamespace(subsys.id, ns.nsid)}>Remove</button></td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			</div>

			<div class="section">
				<h4>Ports</h4>
				{#if subsys.ports.length === 0}
					<span class="muted">No ports (not listening)</span>
				{:else}
					{#each subsys.ports as port}
						<div class="port-row">
							<span class="tag">{port.transport.toUpperCase()}</span>
							<span class="mono">{port.addr}:{port.service_id}</span>
							<span class="muted">({port.addr_family})</span>
							<button class="danger small" onclick={() => removePort(subsys.id, port.port_id)}>Remove</button>
						</div>
					{/each}
				{/if}
			</div>

			{#if subsys.allowed_hosts.length > 0}
				<div class="section">
					<h4>Allowed Hosts</h4>
					{#each subsys.allowed_hosts as host}
						<div class="mono">{host}</div>
					{/each}
				</div>
			{/if}
		</div>
	{/each}
{/if}

{#if nsSubsys}
	<div class="modal-overlay" role="presentation" onclick={() => nsSubsys = null} onkeydown={(e) => { if (e.key === 'Escape') nsSubsys = null; }}>
		<div class="modal" role="dialog" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<h3>Add Namespace</h3>
			<div class="field">
				<label for="ns-device">Device Path</label>
				<input id="ns-device" bind:value={nsDevicePath} placeholder="/dev/nvme0n1" />
			</div>
			<div class="modal-actions">
				<button onclick={addNamespace} disabled={!nsDevicePath}>Add</button>
				<button class="secondary" onclick={() => nsSubsys = null}>Cancel</button>
			</div>
		</div>
	</div>
{/if}

{#if portSubsys}
	<div class="modal-overlay" role="presentation" onclick={() => portSubsys = null} onkeydown={(e) => { if (e.key === 'Escape') portSubsys = null; }}>
		<div class="modal" role="dialog" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
			<h3>Add Port</h3>
			<div class="field">
				<label for="port-transport">Transport</label>
				<select id="port-transport" bind:value={portTransport}>
					<option value="tcp">TCP</option>
					<option value="rdma">RDMA</option>
				</select>
			</div>
			<div class="field">
				<label for="port-addr">Address</label>
				<input id="port-addr" bind:value={portAddr} />
			</div>
			<div class="field">
				<label for="port-svcid">Port</label>
				<input id="port-svcid" type="number" bind:value={portSvcId} />
			</div>
			<div class="modal-actions">
				<button onclick={addPort}>Add</button>
				<button class="secondary" onclick={() => portSubsys = null}>Cancel</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.toolbar { margin: 1rem 0; }
	.form-card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; max-width: 450px; }
	.form-card h3 { margin: 0 0 1rem; }
	.field { margin-bottom: 1rem; }
	.field label { display: block; margin-bottom: 0.25rem; color: #9ca3af; font-size: 0.875rem; }
	.field input, .field select { width: 100%; box-sizing: border-box; }
	.hint { font-size: 0.75rem; color: #6b7280; }
	.mono { font-family: monospace; font-size: 0.85rem; }
	.muted { color: #6b7280; font-size: 0.8rem; }
	.tag { display: inline-block; background: #1e2130; padding: 0.15rem 0.4rem; border-radius: 3px; font-size: 0.75rem; }
	.badge { padding: 0.2rem 0.5rem; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
	.badge.enabled { background: #064e3b; color: #4ade80; }
	.card { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.25rem; margin-bottom: 1rem; }
	.card-header { display: flex; justify-content: space-between; align-items: start; margin-bottom: 1rem; }
	.section { margin-top: 0.75rem; }
	.section h4 { margin: 0 0 0.4rem; color: #9ca3af; font-size: 0.75rem; text-transform: uppercase; }
	.actions { display: flex; gap: 0.5rem; }
	.port-row { display: flex; align-items: center; gap: 0.5rem; margin: 0.25rem 0; }
	.inner-table { margin-top: 0.25rem; }
	:global(button.small) { padding: 0.2rem 0.5rem; font-size: 0.75rem; }
	.modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.6); display: flex; align-items: center; justify-content: center; z-index: 100; }
	.modal { background: #161926; border: 1px solid #2d3348; border-radius: 8px; padding: 1.5rem; min-width: 380px; }
	.modal h3 { margin: 0 0 1rem; }
	.modal-actions { display: flex; gap: 0.5rem; }
</style>
