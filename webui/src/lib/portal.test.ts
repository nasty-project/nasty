import { describe, expect, test } from 'vitest';
import {
	joinPortalPath,
	parentPortalPath,
	portalBreadcrumbs,
	portalFilesUrl,
	portalPathSegments,
} from './portal';

describe('portal file helpers', () => {
	test('joins and ascends relative descriptor paths', () => {
		expect(joinPortalPath('', 'Projects')).toBe('Projects');
		expect(joinPortalPath('Projects', 'Q3 reports')).toBe('Projects/Q3 reports');
		expect(parentPortalPath('Projects/Q3 reports')).toBe('Projects');
		expect(parentPortalPath('Projects')).toBe('');
	});

	test('rejects absolute, traversal, and separator-bearing segments', () => {
		for (const path of ['/etc', '../etc', 'one/../two', 'one\\two']) {
			expect(() => portalPathSegments(path)).toThrow();
		}
		expect(() => joinPortalPath('safe', '../secret')).toThrow('Invalid portal path segment');
		expect(() => joinPortalPath('safe', 'one/two')).toThrow('Invalid portal path segment');
	});

	test('builds cumulative breadcrumbs without decoding names', () => {
		expect(portalBreadcrumbs('Team files/2026/Q3')).toEqual([
			{ label: 'Team files', path: 'Team files' },
			{ label: '2026', path: 'Team files/2026' },
			{ label: 'Q3', path: 'Team files/2026/Q3' },
		]);
	});

	test('encodes share IDs and relative paths in endpoint URLs', () => {
		const url = portalFilesUrl('content', 'share & one', 'Reports/Q3 #1.pdf');
		expect(url).toBe('/api/user/files/content?share=share+%26+one&path=Reports%2FQ3+%231.pdf');
	});
});
