import { describe, expect, test } from 'vitest';
import {
	joinSharePath,
	parentSharePath,
	shareBreadcrumbs,
	shareBrowseUrl,
	shareDownloadUrl,
	shareZipUrl
} from './public-share';

describe('public share navigation', () => {
	test('joins paths and walks to parents', () => {
		expect(joinSharePath('', 'Photos')).toBe('Photos');
		expect(joinSharePath('Photos/2026', 'July')).toBe('Photos/2026/July');
		expect(parentSharePath('Photos/2026/July')).toBe('Photos/2026');
		expect(parentSharePath('Photos')).toBe('');
		expect(parentSharePath('')).toBe('');
	});

	test('builds cumulative breadcrumbs', () => {
		expect(shareBreadcrumbs('Media', 'Photos/2026')).toEqual([
			{ label: 'Media', path: '' },
			{ label: 'Photos', path: 'Photos' },
			{ label: '2026', path: 'Photos/2026' }
		]);
	});

	test('encodes tokens and query paths', () => {
		expect(shareBrowseUrl('token/value', 2, 'Family Photos/July #1')).toBe(
			'/api/public/share/token%2Fvalue/browse?root=2&path=Family+Photos%2FJuly+%231'
		);
		expect(shareDownloadUrl('abc', 0, 'reports/Q2 & Q3.pdf')).toBe(
			'/api/public/share/abc/download?root=0&path=reports%2FQ2+%26+Q3.pdf'
		);
		expect(shareZipUrl('abc')).toBe('/api/public/share/abc/zip');
	});
});
