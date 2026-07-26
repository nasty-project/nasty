import { beforeEach, describe, expect, test } from 'vitest';
import { resetClient } from './client';
import { domain } from './domain.svelte';
import { dc } from './dc.svelte';
import { rollbackState } from './rollbackState.svelte';
import { terminalStatus } from './terminalStatus.svelte';
import { confirm, confirmState } from './confirm.svelte';
import { confirmDangerous, confirmDangerousState } from './confirm-dangerous.svelte';
import { error as toastError, getToasts } from './toast.svelte';
import { unlockFs, unlockFsState } from './unlock-fs.svelte';
import { rdma } from './sharing/rdma.svelte';
import { nfs } from './sharing/nfs.svelte';
import { smb } from './sharing/smb.svelte';
import { iscsi } from './sharing/iscsi.svelte';
import { nvme } from './sharing/nvmeof.svelte';

beforeEach(() => resetClient());

describe('session state reset', () => {
	test('clears loaded stores and plaintext credentials without replacing proxies', () => {
		const identities = { domain, dc, rdma, nfs, smb, iscsi, nvme };
		domain.password = 'domain secret';
		dc.adminPassword = 'dc secret';
		rdma.loading = true;
		nfs.newHost = '192.0.2.1';
		smb.newName = 'private';
		iscsi.addAclPass = 'chap secret';
		nvme.addHostNqn = 'nqn.2026-01.test:host';
		rollbackState.set({ txnId: 'txn-1', revertAtUnix: 1234, riskReason: null });
		terminalStatus.set('connected');

		resetClient();

		expect(domain).toBe(identities.domain);
		expect(dc).toBe(identities.dc);
		expect(rdma).toBe(identities.rdma);
		expect(nfs).toBe(identities.nfs);
		expect(smb).toBe(identities.smb);
		expect(iscsi).toBe(identities.iscsi);
		expect(nvme).toBe(identities.nvme);
		expect(domain.password).toBe('');
		expect(dc.adminPassword).toBe('');
		expect(rdma.loading).toBe(false);
		expect(nfs.newHost).toBe('');
		expect(smb.newName).toBe('');
		expect(iscsi.addAclPass).toBe('');
		expect(nvme.addHostNqn).toBe('');
		expect(rollbackState.pending).toBeNull();
		expect(terminalStatus.value).toBe('idle');
	});

	test('settles dialogs and clears transient messages', async () => {
		const confirmation = confirm('Delete private data?');
		const dangerous = confirmDangerous('Destroy pool?', 'Type pool name', 'private');
		const unlock = unlockFs('private');
		toastError('private path failed');

		resetClient();

		await expect(confirmation).resolves.toBe(false);
		await expect(dangerous).resolves.toBe(false);
		await expect(unlock).resolves.toBe(false);
		expect(confirmState.open).toBe(false);
		expect(confirmDangerousState.open).toBe(false);
		expect(confirmDangerousState.expectedValue).toBe('');
		expect(unlockFsState.open).toBe(false);
		expect(unlockFsState.fsName).toBe('');
		expect(getToasts()).toEqual([]);
	});
});
