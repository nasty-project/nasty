import { beforeEach, describe, expect, test } from 'vitest';
import { parseMenuStyle, uiPrefs } from './uiPrefs.svelte';

beforeEach(() => {
	uiPrefs.setLogoHidden(false);
	uiPrefs.setMenuStyle('classic');
	uiPrefs.setIconGroupId(null);
	localStorage.clear();
});

describe('uiPrefs', () => {
	test('defaults fresh and invalid preferences to icons', () => {
		expect(parseMenuStyle(null)).toBe('icons');
		expect(parseMenuStyle('unknown')).toBe('icons');
	});

	test('preserves every explicit menu preference', () => {
		expect(parseMenuStyle('classic')).toBe('classic');
		expect(parseMenuStyle('icons')).toBe('icons');
		expect(parseMenuStyle('launcher')).toBe('launcher');
	});

	test('persists the selected menu presentation', () => {
		uiPrefs.setMenuStyle('icons');
		expect(uiPrefs.menuStyle).toBe('icons');
		expect(localStorage.getItem('nasty:menu_style')).toBe('icons');

		uiPrefs.setMenuStyle('classic');
		expect(uiPrefs.menuStyle).toBe('classic');
		expect(localStorage.getItem('nasty:menu_style')).toBe('classic');

		uiPrefs.setMenuStyle('launcher');
		expect(uiPrefs.menuStyle).toBe('launcher');
		expect(localStorage.getItem('nasty:menu_style')).toBe('launcher');
	});

	test('keeps logo visibility behavior independent', () => {
		uiPrefs.setMenuStyle('icons');
		uiPrefs.setLogoHidden(true);
		expect(uiPrefs.logoHidden).toBe(true);
		expect(uiPrefs.menuStyle).toBe('icons');
	});

	test('persists and clears the selected icon category', () => {
		uiPrefs.setIconGroupId('storage');
		expect(uiPrefs.iconGroupId).toBe('storage');
		expect(localStorage.getItem('nasty:icon_nav_group')).toBe('storage');

		uiPrefs.setIconGroupId(null);
		expect(uiPrefs.iconGroupId).toBeNull();
		expect(localStorage.getItem('nasty:icon_nav_group')).toBeNull();
	});
});
