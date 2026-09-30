import { expect, test } from 'vitest';
import { Input, CustomSource, WAVE } from 'mediabunny';
import { readMediaProperties } from './media-properties';

test('Mediabunny can inspect PCM metadata through bounded range reads', async () => {
	const buffer = new ArrayBuffer(44 + 16000);
	const view = new DataView(buffer);
	const text = (offset: number, value: string) => {
		for (let i = 0; i < value.length; i++) view.setUint8(offset + i, value.charCodeAt(i));
	};
	text(0, 'RIFF'); view.setUint32(4, buffer.byteLength - 8, true);
	text(8, 'WAVE'); text(12, 'fmt '); view.setUint32(16, 16, true);
	view.setUint16(20, 1, true); view.setUint16(22, 1, true);
	view.setUint32(24, 8000, true); view.setUint32(28, 16000, true);
	view.setUint16(32, 2, true); view.setUint16(34, 16, true);
	text(36, 'data'); view.setUint32(40, 16000, true);
	const reads: number[] = [];
	const input = new Input({
		formats: [WAVE],
		source: new CustomSource({
			getSize: () => buffer.byteLength,
			read: (start, end) => {
				reads.push(end - start);
				return new Uint8Array(buffer.slice(start, end));
			}
		})
	});
	try {
		expect(await input.getDurationFromMetadata()).toBe(1);
		expect(await (await input.getPrimaryAudioTrack())?.getCodec()).toBe('pcm-s16');
		const properties = await readMediaProperties(input);
		expect(properties.container).toBe(WAVE.name);
		expect(properties.duration).toBe(1);
		expect(properties.trackCount).toBe(1);
		expect(properties.tracks[0]).toMatchObject({
			type: 'audio', codec: 'pcm-s16', internalCodec: '1', channels: 1, sampleRate: 8000,
			width: null, height: null
		});
		expect(reads.length).toBeGreaterThan(0);
		expect(Math.max(...reads)).toBeLessThanOrEqual(buffer.byteLength);
	} finally {
		input.dispose();
	}
});
