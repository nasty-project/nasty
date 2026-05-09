/** Temperature display helper. Internal storage and alert thresholds are
 * always Celsius — this module only converts at render time so a user
 * preference for Fahrenheit never leaks into stored data. */

import type { TempUnit } from './types';

let _unit = $state<TempUnit>('celsius');

export const tempUnit = {
	get current(): TempUnit {
		return _unit;
	},
	set(unit: TempUnit) {
		_unit = unit;
	},
};

export function cToF(c: number): number {
	return c * 9 / 5 + 32;
}

/** Render a Celsius value in the active unit, with the degree symbol.
 * Pass `null` for missing readings — caller decides whether to render
 * "—" or hide the row entirely. */
export function formatTemp(c: number | null, unit: TempUnit = _unit): string | null {
	if (c == null) return null;
	if (unit === 'fahrenheit') return `${Math.round(cToF(c))}°F`;
	return `${Math.round(c)}°C`;
}

/** Just the unit's degree label, without a value — for column headers
 * and metric labels like "Disk Temperature (°C)". */
export function tempUnitLabel(unit: TempUnit = _unit): string {
	return unit === 'fahrenheit' ? '°F' : '°C';
}
