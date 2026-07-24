// Per-browser sidebar/chrome UI preferences, persisted in localStorage and
// shared reactively across components (same shape as theme.svelte.ts) so the
// sidebar and the Settings toggle stay in sync without a page reload.
const LOGO_HIDDEN_KEY = 'nasty:sidebar_logo_hidden';
const MENU_STYLE_KEY = 'nasty:menu_style';
const ICON_GROUP_KEY = 'nasty:icon_nav_group';

export type MenuStyle = 'classic' | 'icons' | 'launcher';

function createUiPrefs() {
	let logoHidden = $state<boolean>(
		typeof localStorage !== 'undefined' && localStorage.getItem(LOGO_HIDDEN_KEY) === '1'
	);
	const storedMenuStyle = typeof localStorage !== 'undefined' ? localStorage.getItem(MENU_STYLE_KEY) : null;
	let menuStyle = $state<MenuStyle>(storedMenuStyle === 'icons' || storedMenuStyle === 'launcher' ? storedMenuStyle : 'classic');
	let iconGroupId = $state<string | null>(
		typeof localStorage !== 'undefined' ? localStorage.getItem(ICON_GROUP_KEY) : null
	);

	return {
		get logoHidden() {
			return logoHidden;
		},
		setLogoHidden(v: boolean) {
			logoHidden = v;
			if (typeof localStorage !== 'undefined') {
				localStorage.setItem(LOGO_HIDDEN_KEY, v ? '1' : '0');
			}
		},
		get menuStyle() {
			return menuStyle;
		},
		setMenuStyle(style: MenuStyle) {
			menuStyle = style;
			if (typeof localStorage !== 'undefined') {
				localStorage.setItem(MENU_STYLE_KEY, style);
			}
		},
		get iconGroupId() {
			return iconGroupId;
		},
		setIconGroupId(id: string | null) {
			iconGroupId = id;
			if (typeof localStorage === 'undefined') return;
			if (id) localStorage.setItem(ICON_GROUP_KEY, id);
			else localStorage.removeItem(ICON_GROUP_KEY);
		},
	};
}

export const uiPrefs = createUiPrefs();
