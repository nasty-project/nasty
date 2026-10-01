import { expect, test, vi } from 'vitest';
import { batchPcmBuffers } from './media-pcm';
import type { WrappedAudioBuffer } from 'mediabunny';

function createBuffer(channels: number, length: number, sampleRate: number): AudioBuffer {
	const data = Array.from({ length: channels }, () => new Float32Array(length));
	return { numberOfChannels: channels, length, sampleRate, duration: length / sampleRate, getChannelData: (channel: number) => data[channel] } as AudioBuffer;
}

const collect = async (source: AsyncIterable<WrappedAudioBuffer>, start: number, end: number) => {
	const result: WrappedAudioBuffer[] = [];
	for await (const batch of batchPcmBuffers(source, { start, end, createBuffer, isCurrent: () => true })) result.push(batch);
	return result;
};

test('combines short stereo frames with bit-exact PCM across a fractional window boundary', async () => {
	const source = async function* () {
		for (let frame = 0; frame < 40; frame++) {
			const buffer = createBuffer(2, 1536, 48000);
			for (let channel = 0; channel < 2; channel++) {
				for (let index = 0; index < buffer.length; index++) buffer.getChannelData(channel)[index] = (frame * 1536 + index) * (channel ? -1 : 1);
			}
			yield { buffer, timestamp: frame * 0.032, duration: 0.032 };
		}
	};
	const boundary = 0.201937;
	const batches = [...await collect(source(), 0, boundary), ...await collect(source(), boundary, 1.28)];
	expect(batches.length).toBeLessThan(12);
	expect(batches.every(batch => batch.buffer.length <= 9216)).toBe(true);
	let frame = 0;
	for (const batch of batches) {
		expect(batch.timestamp).toBeCloseTo(frame / 48000, 10);
		for (let index = 0; index < batch.buffer.length; index++) {
			expect(batch.buffer.getChannelData(0)[index]).toBe(frame + index);
			expect(batch.buffer.getChannelData(1)[index]).toBe(-(frame + index));
		}
		frame += batch.buffer.length;
	}
	expect(frame).toBe(40 * 1536);
});

test('preserves timestamp gaps and channel/sample-rate changes instead of joining incompatible PCM', async () => {
	const source = async function* () {
		yield { buffer: createBuffer(2, 1536, 48000), timestamp: 0, duration: 0.032 };
		yield { buffer: createBuffer(2, 1536, 48000), timestamp: 0.064, duration: 0.032 };
		yield { buffer: createBuffer(1, 1411, 44100), timestamp: 0.096, duration: 1411 / 44100 };
	};
	const discontinuity = vi.fn();
	const batches: WrappedAudioBuffer[] = [];
	for await (const batch of batchPcmBuffers(source(), { start: 0, end: 1, createBuffer, isCurrent: () => true, onDiscontinuity: discontinuity })) batches.push(batch);
	expect(batches.map(batch => batch.timestamp)).toEqual([0, 0.064, 0.096]);
	expect(batches.map(batch => batch.buffer.numberOfChannels)).toEqual([2, 2, 1]);
	expect(discontinuity).toHaveBeenCalledTimes(2);
});

test('discards a partial batch when a read finishes after cancellation and closes the source iterator', async () => {
	let active = true;
	let release!: () => void;
	const wait = new Promise<void>(resolve => release = resolve);
	let closed = false;
	const source = async function* () {
		try {
			yield { buffer: createBuffer(2, 1536, 48000), timestamp: 0, duration: 0.032 };
			await wait;
			yield { buffer: createBuffer(2, 1536, 48000), timestamp: 0.032, duration: 0.032 };
		} finally { closed = true; }
	};
	const create = vi.fn(createBuffer);
	const iterator = batchPcmBuffers(source(), { start: 0, end: 1, createBuffer: create, isCurrent: () => active });
	const next = iterator.next();
	await new Promise(resolve => setTimeout(resolve, 0));
	active = false; release();
	expect((await next).done).toBe(true);
	expect(create).not.toHaveBeenCalled();
	expect(closed).toBe(true);
});
