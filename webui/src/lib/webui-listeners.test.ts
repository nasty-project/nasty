import { describe, it, expect } from 'vitest';
import { listenerUrl, onListener, validListenerPorts } from './webui-listeners';

describe('WebUI listener settings', () => {
	it('accepts alternative HTTPS and optional HTTP', () => {
		expect(validListenerPorts(8443, 8080)).toBe(true);
		expect(validListenerPorts(8443, null)).toBe(true);
	});
	it('rejects invalid, identical, and reserved ports', () => {
		for (const port of [0, -1, 65536, 1.5, NaN, 2019, 2137]) expect(validListenerPorts(port, null)).toBe(false);
		expect(validListenerPorts(8443, 8443)).toBe(false);
		expect(validListenerPorts(8443, 0)).toBe(false);
	});
	it('builds a bracketed IPv6 URL without retaining query tokens', () => {
		expect(listenerUrl('https://[2001:db8::1]/settings?token=secret#network', 8443)).toBe('https://[2001:db8::1]:8443/settings');
		expect(listenerUrl('http://nas.local:8080/settings', 443)).toBe('https://nas.local/settings');
	});
	it('only allows UI confirmation from the proposed HTTPS port', () => {
		expect(onListener('https://nas.local:8443/settings', 8443)).toBe(true);
		expect(onListener('https://nas.local/settings', 8443)).toBe(false);
		expect(onListener('http://nas.local:8443/settings', 8443)).toBe(false);
	});
});
