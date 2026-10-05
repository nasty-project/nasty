import { describe, expect, test } from 'vitest';
import { TlsDnsProviderForm } from './tls-dns-provider.svelte';

const providers = [{ code: 'cloudflare' }, { code: 'route53' }];

describe('TLS DNS provider form', () => {
	test('typing a custom code never changes the dropdown selection', () => {
		const form = new TlsDnsProviderForm();
		form.selection = 'other';
		for (const value of ['i', 'in', 'inwx', 'cloudflare', '']) {
			form.customCode = value;
			expect(form.selection).toBe('other');
			expect(form.provider).toBe(value);
		}
	});

	test('switching providers uses the selected code and retains custom text', () => {
		const form = new TlsDnsProviderForm();
		form.selection = 'other';
		form.customCode = '  inwx  ';
		expect(form.provider).toBe('inwx');
		form.selection = 'cloudflare';
		expect(form.provider).toBe('cloudflare');
		form.selection = 'other';
		expect(form.provider).toBe('inwx');
		expect(form.customCode).toBe('  inwx  ');
	});

	test('a saved custom code round-trips through Other on reload', () => {
		const form = new TlsDnsProviderForm();
		form.load('inwx', providers);
		expect(form.selection).toBe('other');
		expect(form.customCode).toBe('inwx');
		expect(form.provider).toBe('inwx');
		expect(form.validationError(true, 'dns')).toBeNull();
	});

	test('loading a known or unset provider resets stale custom text', () => {
		const form = new TlsDnsProviderForm();
		form.load('inwx', providers);
		form.load('cloudflare', providers);
		expect(form.selection).toBe('cloudflare');
		expect(form.customCode).toBe('');
		expect(form.provider).toBe('cloudflare');
		form.load('', providers);
		expect(form.selection).toBe('');
		expect(form.provider).toBe('');
		expect(form.validationError(true, 'dns')).toBe('Select a DNS provider.');
	});

	test.each(['', ' ', '\t\n'])('rejects blank custom code %j for enabled DNS-01', (code) => {
		const form = new TlsDnsProviderForm();
		form.load('cloudflare', providers);
		form.selection = 'other';
		form.customCode = code;
		expect(form.provider).toBe('');
		expect(form.validationError(true, 'dns')).toBe('Enter a DNS provider code.');
	});

	test('does not require a DNS provider when ACME is disabled or uses another challenge', () => {
		const form = new TlsDnsProviderForm();
		form.selection = 'other';
		expect(form.validationError(false, 'dns')).toBeNull();
		expect(form.validationError(true, 'http')).toBeNull();
		expect(form.validationError(true, 'tls-alpn')).toBeNull();
	});
});
