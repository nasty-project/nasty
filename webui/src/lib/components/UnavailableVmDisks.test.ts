import { describe, expect, it } from 'vitest';
import { render } from 'svelte/server';
import UnavailableVmDisks from './UnavailableVmDisks.svelte';

describe('unavailable VM disks disclosure', () => {
	it('is collapsed by default with readable names and complete technical details', () => {
		const result = render(UnavailableVmDisks, { props: { candidates: [{
			subvolume: { name: 'pvc-long-uuid', filesystem: 'tank', block_device: '/dev/loop1',
				subvolume_type: 'block', path: '/fs/tank/pvc-long-uuid', used_bytes: null, quota_bytes: null,
				compression: null, comments: null, volsize_bytes: 1073741824, snapshots: [], owner: null,
				parent: null, direct_io: false, properties: {
				'nasty-csi:pvc_namespace': 'db', 'nasty-csi:pvc_name': 'postgres-3'
			} },
			consumers: ['Kubernetes CSI volume (PVC db/postgres-3); reserved even when not mounted', "iSCSI target 'long-iqn'"]
		}] } });
		expect(result.body).toContain('Unavailable disks');
		expect(result.body).toContain('db/postgres-3');
		expect(result.body).toContain('tank/pvc-long-uuid');
		expect(result.body).toContain('long-iqn');
		expect(result.body).toContain('/dev/loop1');
		expect(result.body).not.toMatch(/<details[^>]*\sopen(?:\s|=|>)/);
		const summaries = [...result.body.matchAll(/<summary\b[^>]*>([\s\S]*?)<\/summary>/g)].map(match => match[1]);
		expect(summaries).toHaveLength(2);
		expect(summaries[1]).not.toContain('pvc-long-uuid');
		expect(summaries[1]).not.toContain('long-iqn');
	});
	it('does not render an empty disclosure', () => {
		expect(render(UnavailableVmDisks, { props: { candidates: [] } }).body).not.toContain('<details');
	});
});
