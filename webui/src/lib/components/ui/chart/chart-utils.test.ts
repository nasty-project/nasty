import { describe, expect, it } from 'vitest';
import { getTooltipLabelValue, type ChartConfig, type TooltipPayload } from './chart-utils';

const config: ChartConfig = {
	in: { label: 'Read', color: 'blue' },
	out: { label: 'Write', color: 'green' },
};
const series: TooltipPayload[] = [
	{ key: 'in', label: 'Read', value: 42 },
	{ key: 'out', label: 'Write', value: 21 },
];

describe('tooltip label selection', () => {
	it('passes the raw sample Date rather than a series name to the time formatter', () => {
		const time = new Date('2026-10-15T12:34:56Z');
		const value = getTooltipLabelValue(config, series, { time, in: 42, out: 21 }, 'time');
		expect(value).toBe(time);
		expect((value as Date).toLocaleTimeString('en-US', { timeZone: 'UTC' })).toBe('12:34:56 PM');
	});

	it('also uses the shared datum for a single series', () => {
		const time = new Date('2026-10-15T12:34:56Z');
		expect(getTooltipLabelValue(config, series.slice(0, 1), { time }, 'time')).toBe(time);
	});

	it('preserves default series labels and explicit configured labels', () => {
		expect(getTooltipLabelValue(config, series, {})).toBe('Read');
		expect(getTooltipLabelValue(config, series, {}, undefined, 'out')).toBe('Write');
		expect(getTooltipLabelValue(config, series, {}, undefined, 'Custom')).toBe('Custom');
	});

	it('does not pass a series name to a datum formatter when the field is absent', () => {
		for (const data of [null, undefined, {}, 'Read', Object.create({ time: new Date() })]) {
			expect(getTooltipLabelValue(config, series, data, 'time')).toBeUndefined();
		}
		expect(getTooltipLabelValue(config, [], { time: new Date() }, 'time')).toBeUndefined();
	});
});
