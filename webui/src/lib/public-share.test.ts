import { describe, expect, test } from 'vitest';
import {
	joinSharePath,
	normalizeShareDownloadLimit,
	parentSharePath,
	shareBreadcrumbs,
	shareBrowseUrl,
	shareDownloadUrl,
	shareZipUrl,
	mediaPreviewKind,
	shareMediaUrl,
	singleVideoPreview,
	type PublicShareMeta
} from './public-share';

describe('public share navigation', () => {
	test('normalizes numeric download limits from number inputs', () => {
		expect(normalizeShareDownloadLimit(undefined)).toBeNull();
		expect(normalizeShareDownloadLimit(5)).toBe(5);
		expect(() => normalizeShareDownloadLimit(0)).toThrow('positive whole number');
		expect(() => normalizeShareDownloadLimit(2.5)).toThrow('positive whole number');
	});

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

	test('preview links encode the same root and path boundary as downloads', () => {
		expect(shareMediaUrl('abc', 2, 'Music/live & loud.mp3')).toBe('/api/public/share/abc/media?root=2&path=Music%2Flive+%26+loud.mp3');
		expect(mediaPreviewKind('clip.MKV')).toBe('video');
		expect(mediaPreviewKind('song.mp3')).toBe('audio');
		for (const name of ['page.html', 'picture.svg', 'remote.m3u8', 'file.mp4.html']) {
			expect(mediaPreviewKind(name)).toBeNull();
		}
	});

	test('opens a single eligible video only after unlock and when preview access is enabled', () => {
		const meta: PublicShareMeta = { entries: [{ root: 2, name: 'movie.mp4', is_dir: false, size: 100 }], password_required: false, unlocked: true, expires_at: null, media_preview_enabled: true };
		expect(singleVideoPreview(meta)).toEqual(meta.entries[0]);
		expect(singleVideoPreview({ ...meta, media_preview_enabled: false })).toBeNull();
		expect(singleVideoPreview({ ...meta, password_required: true, unlocked: false })).toBeNull();
		expect(singleVideoPreview({ ...meta, entries: [...meta.entries, { root: 3, name: 'other.mp4', is_dir: false, size: 100 }] })).toBeNull();
		for (const entry of [{ ...meta.entries[0], is_dir: true }, { ...meta.entries[0], name: 'notes.txt' }, { ...meta.entries[0], name: 'music.mp3' }]) {
			expect(singleVideoPreview({ ...meta, entries: [entry] })).toBeNull();
		}
	});
});
