/** Shared reactive counter — increment to trigger a sysInfo reload in the layout. */
let _count = $state(0);
let _timers: ReturnType<typeof setTimeout>[] = [];

export const sysInfoRefresh = {
	get count() { return _count; },
	trigger() { _count++; },
	/**
	 * Re-fetch sysInfo now, then again over the next several seconds.
	 *
	 * A single trigger after a version switch can lose a race: the fetch
	 * may land before the rebuild finishes rewriting flake.lock, or fail
	 * transiently while the box is activating the new generation. The
	 * layout swallows that miss silently, so the top-bar chip stays stale
	 * until a manual reload. Spacing a few retries lets a later one pick
	 * up the settled value without the user hitting refresh.
	 */
	triggerReconcile() {
		for (const t of _timers) clearTimeout(t);
		_timers = [];
		_count++;
		for (const delay of [1500, 4000, 8000]) {
			_timers.push(setTimeout(() => { _count++; }, delay));
		}
	},
};
