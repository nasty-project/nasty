/** Shared reactive counter — increment to trigger a sysInfo reload in the layout. */
let _count = $state(0);

export const sysInfoRefresh = {
	get count() { return _count; },
	trigger() { _count++; },
};
