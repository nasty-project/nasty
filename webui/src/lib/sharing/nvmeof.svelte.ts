/** NVMe-oF subsystem state + handlers.
 *
 * Same shape as the NFS / SMB / iSCSI modules. NVMe-oF subsystems
 * carry namespaces (block backstores), ports (transport+addr+svcid
 * listeners), and allowed-host NQNs — three nested collections, each
 * with add/remove handlers. */

import { getClient, registerSessionReset } from '$lib/client';
import { withToast } from '$lib/toast.svelte';
import { confirm } from '$lib/confirm.svelte';
import type { NvmeofSubsystem, Subvolume, ProtocolStatus } from '$lib/types';

function initialNvmeState() {
	return {
		subsystems: [] as NvmeofSubsystem[],
		loading: true,
		protocol: null as ProtocolStatus | null,
		nqnPrefix: 'nqn.2137-04.storage.nasty',
		blockSubvolumes: [] as Subvolume[],
		expanded: {} as Record<string, boolean>,
		addNsSubsys: '',
		addNsDevice: '',
		repairNsSubsys: '',
		repairNsId: 0,
		repairNsDevice: '',
		addPortSubsys: '',
		addPortTransport: 'tcp',
		addPortAddr: '0.0.0.0',
		addPortSvcId: 4420,
		addPortFamily: 'ipv4' as 'ipv4' | 'ipv6',
		addHostSubsys: '',
		addHostNqn: '',
		search: '',
		sortDir: 'asc' as 'asc' | 'desc',
	};
}

export const nvme = $state(initialNvmeState());

export function resetNvmeState() {
	Object.assign(nvme, initialNvmeState());
}

registerSessionReset(resetNvmeState);

export function nvmeToggleSort() {
	nvme.sortDir = nvme.sortDir === 'asc' ? 'desc' : 'asc';
}

export async function nvmeRefresh() {
	await withToast(async () => { nvme.subsystems = await getClient().call<NvmeofSubsystem[]>('share.nvmeof.list'); });
}

export async function nvmeLoadProtocol() {
	try {
		const all = await getClient().call<ProtocolStatus[]>('service.protocol.list');
		nvme.protocol = all.find(p => p.name === 'nvmeof') ?? null;
	} catch { /* ignore */ }
}

export async function nvmeLoadBaseNqn() {
	try {
		const config = await getClient().call<{ nqn_prefix: string }>('service.base_names.get');
		nvme.nqnPrefix = config.nqn_prefix.toLowerCase();
	} catch { /* keep the server default */ }
}

export async function nvmeLoadSubvolumes() {
	await withToast(async () => {
		const all = await getClient().call<Subvolume[]>('subvolume.list_all');
		nvme.blockSubvolumes = all.filter(s => s.subvolume_type === 'block' && s.block_device);
	});
}

export async function nvmeRemove(id: string) {
	if (!await confirm('Delete this NVMe-oF share?')) return;
	await withToast(() => getClient().call('share.nvmeof.delete', { id }), 'NVMe-oF share deleted');
	await nvmeRefresh();
}

export async function nvmeAddNamespace() {
	if (!nvme.addNsSubsys || !nvme.addNsDevice) return;
	await withToast(
		() => getClient().call('share.nvmeof.add_namespace', { subsystem_id: nvme.addNsSubsys, device_path: nvme.addNsDevice }),
		'Namespace added'
	);
	nvme.addNsSubsys = '';
	nvme.addNsDevice = '';
	await nvmeRefresh();
}

export async function nvmeRemoveNamespace(subsystemId: string, nsid: number) {
	if (!await confirm(`Remove namespace ${nsid}?`)) return;
	await withToast(
		() => getClient().call('share.nvmeof.remove_namespace', { subsystem_id: subsystemId, nsid }),
		'Namespace removed'
	);
	await nvmeRefresh();
}

export async function nvmeRepairNamespace() {
	if (!nvme.repairNsSubsys || !nvme.repairNsDevice) return;
	if (!await confirm(
		'Reconnect this namespace?',
		'Only select the original block subvolume. Choosing a different volume would expose its data through this subsystem.',
	)) return;
	const repaired = await withToast(
		() => getClient().call('share.nvmeof.repair_namespace', {
			subsystem_id: nvme.repairNsSubsys,
			nsid: nvme.repairNsId,
			device_path: nvme.repairNsDevice,
		}),
		'Namespace backing volume reconnected',
	);
	if (repaired !== undefined) {
		nvme.repairNsSubsys = '';
		nvme.repairNsDevice = '';
		await Promise.all([nvmeRefresh(), nvmeLoadProtocol()]);
	}
}

export async function nvmeAddPort() {
	if (!nvme.addPortSubsys) return;
	await withToast(
		() => getClient().call('share.nvmeof.add_port', {
			subsystem_id: nvme.addPortSubsys,
			transport: nvme.addPortTransport,
			addr: nvme.addPortAddr,
			service_id: nvme.addPortSvcId,
			addr_family: nvme.addPortFamily,
		}),
		'Port added'
	);
	nvme.addPortSubsys = '';
	nvme.addPortTransport = 'tcp';
	nvme.addPortAddr = '0.0.0.0';
	nvme.addPortSvcId = 4420;
	nvme.addPortFamily = 'ipv4';
	await nvmeRefresh();
}

export async function nvmeRemovePort(subsystemId: string, portId: number) {
	if (!await confirm(`Remove port ${portId}?`)) return;
	await withToast(
		() => getClient().call('share.nvmeof.remove_port', { subsystem_id: subsystemId, port_id: portId }),
		'Port removed'
	);
	await nvmeRefresh();
}

export async function nvmeAddHost() {
	if (!nvme.addHostSubsys || !nvme.addHostNqn) return;
	await withToast(
		() => getClient().call('share.nvmeof.add_host', { subsystem_id: nvme.addHostSubsys, host_nqn: nvme.addHostNqn }),
		'Allowed host added'
	);
	nvme.addHostSubsys = '';
	nvme.addHostNqn = '';
	await nvmeRefresh();
}

export async function nvmeRemoveHost(subsystemId: string, hostNqn: string) {
	if (!await confirm(`Remove access for ${hostNqn}?`)) return;
	await withToast(
		() => getClient().call('share.nvmeof.remove_host', { subsystem_id: subsystemId, host_nqn: hostNqn }),
		'Allowed host removed'
	);
	await nvmeRefresh();
}
