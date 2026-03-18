/** Shared reactive flag — set when a system restart is needed after a kernel/driver update.
 *  Persisted in localStorage so it survives page reloads until the user actually reboots. */
const STORAGE_KEY = 'nasty:reboot_required';

let _needed = $state(typeof localStorage !== 'undefined' && localStorage.getItem(STORAGE_KEY) === '1');

export const rebootState = {
	get needed() { return _needed; },
	set() {
		_needed = true;
		localStorage.setItem(STORAGE_KEY, '1');
	},
	clear() {
		_needed = false;
		localStorage.removeItem(STORAGE_KEY);
	},
};
