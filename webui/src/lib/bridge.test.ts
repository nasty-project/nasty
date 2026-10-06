import { describe, expect, it } from 'vitest';
import { bridgeIpv4 } from './network';

describe('bridge creation IPv4', () => {
	it('keeps inheritance free of stale static fields', () => {
		expect(bridgeIpv4('inherit', '10.10.30.1/24', '10.10.30.254')).toEqual({ method: 'inherit', addresses: [], gateway: null });
	});
	it('creates an internal static subnet without changing the default gateway', () => {
		expect(bridgeIpv4('static', ' 10.10.30.1/24 ', ' ')).toEqual({ method: 'static', addresses: ['10.10.30.1/24'], gateway: null });
	});
	it('supports an explicit gateway for a LAN-connected bridge', () => {
		expect(bridgeIpv4('static', '10.10.20.100/28', ' 10.10.20.97 ').gateway).toBe('10.10.20.97');
	});
});
