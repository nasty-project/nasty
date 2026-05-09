import { describe, expect, it } from 'vitest';
import { cToF, formatTemp, tempUnitLabel } from './temperature.svelte';

describe('cToF', () => {
	it('converts the canonical reference points', () => {
		// Sanity — verifies the formula isn't reversed.
		expect(cToF(0)).toBe(32);
		expect(cToF(100)).toBe(212);
		expect(cToF(-40)).toBe(-40);
	});

	it('handles non-integer inputs', () => {
		expect(cToF(36.6)).toBeCloseTo(97.88, 2);
	});
});

describe('formatTemp', () => {
	it('renders Celsius with the degree symbol when unit is celsius', () => {
		expect(formatTemp(45, 'celsius')).toBe('45°C');
	});

	it('rounds and converts to Fahrenheit when unit is fahrenheit', () => {
		// 55°C ≈ 131°F. The disks page uses 55°C as the red threshold —
		// the user should see "131°F" there if they switched units.
		expect(formatTemp(55, 'fahrenheit')).toBe('131°F');
	});

	it('rounds rather than truncating', () => {
		// 45.6°C → 114.08°F → "114°F" (round to nearest, not floor).
		expect(formatTemp(45.6, 'fahrenheit')).toBe('114°F');
	});

	it('returns null for null inputs so callers can render their own placeholder', () => {
		expect(formatTemp(null, 'celsius')).toBeNull();
		expect(formatTemp(null, 'fahrenheit')).toBeNull();
	});
});

describe('tempUnitLabel', () => {
	it('returns the bare degree label for headers/metric names', () => {
		expect(tempUnitLabel('celsius')).toBe('°C');
		expect(tempUnitLabel('fahrenheit')).toBe('°F');
	});
});
