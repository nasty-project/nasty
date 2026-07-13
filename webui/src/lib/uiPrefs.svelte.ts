// Per-browser sidebar/chrome UI preferences, persisted in localStorage and
// shared reactively across components (same shape as theme.svelte.ts) so the
// sidebar and the Settings toggle stay in sync without a page reload.
const LOGO_HIDDEN_KEY = 'nasty:sidebar_logo_hidden';

function createUiPrefs() {
	let logoHidden = $state<boolean>(
		typeof localStorage !== 'undefined' && localStorage.getItem(LOGO_HIDDEN_KEY) === '1'
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
	};
}

export const uiPrefs = createUiPrefs();
