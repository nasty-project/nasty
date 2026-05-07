import { describe, expect, test } from 'vitest';
import {
	estimateUsableBytes,
	formatBytes,
	formatPercent,
	formatUptime,
	makeBytesFormatter
} from './format';

const GIB = 1024 ** 3;

describe('formatBytes', () => {
	test('zero is the literal "0 B" — not "0.0 B" or "0 KiB"', () => {
		expect(formatBytes(0)).toBe('0 B');
	});

	test('values below 1 KiB use B with no decimal', () => {
		expect(formatBytes(512)).toBe('512 B');
		expect(formatBytes(1023)).toBe('1023 B');
	});

	test('values above 1 KiB use one decimal place', () => {
		expect(formatBytes(1024)).toBe('1.0 KiB');
		expect(formatBytes(1536)).toBe('1.5 KiB');
		expect(formatBytes(1024 * 1024)).toBe('1.0 MiB');
		expect(formatBytes(1024 * 1024 * 1024)).toBe('1.0 GiB');
		expect(formatBytes(1024 ** 4)).toBe('1.0 TiB');
		expect(formatBytes(1024 ** 5)).toBe('1.0 PiB');
	});

	test('uses binary (1024) units, not SI (1000)', () => {
		// 1500 bytes → 1.5 KiB, not 1.5 KB.
		expect(formatBytes(1500)).toBe('1.5 KiB');
	});
});

describe('makeBytesFormatter', () => {
	test('locks the unit so all axis ticks render in the same unit', () => {
		const fmt = makeBytesFormatter(1024 * 1024 * 100); // 100 MiB
		// Every tick is in MiB, even small ones — the chart axis stays consistent.
		expect(fmt(0)).toBe('0.0 MiB');
		expect(fmt(1024)).toBe('0.0 MiB');
		expect(fmt(1024 * 1024)).toBe('1.0 MiB');
		expect(fmt(1024 * 1024 * 50)).toBe('50.0 MiB');
	});

	test('zero maxBytes falls back to bytes (no NaN unit)', () => {
		const fmt = makeBytesFormatter(0);
		expect(fmt(0)).toBe('0 B');
		expect(fmt(42)).toBe('42 B');
	});
});

describe('formatUptime', () => {
	test('under an hour shows minutes only', () => {
		expect(formatUptime(0)).toBe('0m');
		expect(formatUptime(59)).toBe('0m');
		expect(formatUptime(60)).toBe('1m');
		expect(formatUptime(59 * 60)).toBe('59m');
	});

	test('under a day shows hours and minutes', () => {
		expect(formatUptime(3600)).toBe('1h 0m');
		expect(formatUptime(3600 + 90)).toBe('1h 1m');
		expect(formatUptime(23 * 3600 + 59 * 60)).toBe('23h 59m');
	});

	test('one day or more shows days, hours, minutes', () => {
		expect(formatUptime(86400)).toBe('1d 0h 0m');
		expect(formatUptime(86400 + 3600 + 60)).toBe('1d 1h 1m');
		expect(formatUptime(7 * 86400 + 12 * 3600 + 30 * 60)).toBe('7d 12h 30m');
	});
});

describe('estimateUsableBytes', () => {
	test('mirror divides raw by replicas — the issue #16 case', () => {
		// 2 × 480 GiB mirror should report ~480 GiB usable (raw is 960).
		const usable = estimateUsableBytes(960 * GIB, 2, 2, false);
		expect(usable).toBe(480 * GIB);
	});

	test('replicas=1 with no EC is identity', () => {
		expect(estimateUsableBytes(500 * GIB, 1, 1, false)).toBe(500 * GIB);
	});

	test('mirror with replicas=3 is rawTotal / 3', () => {
		const usable = estimateUsableBytes(900 * GIB, 3, 3, false);
		expect(usable).toBe(300 * GIB);
	});

	test('erasure code RAID-5 (replicas=2) keeps (n-1)/n of raw', () => {
		// 4 × 480 GiB with RAID-5 → 3/4 × 1920 = 1440 GiB usable.
		const raw = 4 * 480 * GIB;
		expect(estimateUsableBytes(raw, 4, 2, true)).toBe(1440 * GIB);
	});

	test('erasure code RAID-6 (replicas=3) keeps (n-2)/n of raw', () => {
		// 5 × 480 GiB with RAID-6 → 3/5 × 2400 = 1440 GiB usable.
		const raw = 5 * 480 * GIB;
		expect(estimateUsableBytes(raw, 5, 3, true)).toBe(1440 * GIB);
	});

	test('zero / negative inputs return 0 (no NaN, no surprises)', () => {
		expect(estimateUsableBytes(0, 2, 2, false)).toBe(0);
		expect(estimateUsableBytes(-1, 2, 2, false)).toBe(0);
		expect(estimateUsableBytes(100, 0, 2, false)).toBe(0);
		expect(estimateUsableBytes(100, 2, 0, false)).toBe(0);
	});

	test('EC with too few devices to satisfy parity returns 0', () => {
		// RAID-5 needs at least 2 devices (1 data + 1 parity); 1 device → 0.
		expect(estimateUsableBytes(100, 1, 2, true)).toBe(0);
		// RAID-6 needs 3+ (1 data + 2 parity); 2 devices → 0.
		expect(estimateUsableBytes(100, 2, 3, true)).toBe(0);
	});

	test('EC with replicas=1 returns 0 (no parity, EC nonsense)', () => {
		expect(estimateUsableBytes(100, 5, 1, true)).toBe(0);
	});
});

describe('formatPercent', () => {
	test('zero total is "0%" — not NaN, not Infinity', () => {
		expect(formatPercent(0, 0)).toBe('0%');
		expect(formatPercent(5, 0)).toBe('0%');
	});

	test('non-zero ratios render with one decimal', () => {
		expect(formatPercent(50, 100)).toBe('50.0%');
		expect(formatPercent(33, 100)).toBe('33.0%');
		expect(formatPercent(1, 3)).toBe('33.3%');
	});

	test('full and over-full ratios render correctly', () => {
		expect(formatPercent(100, 100)).toBe('100.0%');
		expect(formatPercent(150, 100)).toBe('150.0%');
	});
});
