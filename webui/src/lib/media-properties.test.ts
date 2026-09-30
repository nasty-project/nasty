import { expect, test } from 'vitest';
import type { Input } from 'mediabunny';
import { readMediaProperties } from './media-properties';

test('keeps stored codec and track metadata when decoding/probing is unavailable', async () => {
	const unavailable = async () => { throw new Error('No browser decoder'); };
	const audio = {
		id: 7, number: 2,
		isVideoTrack: () => false, isAudioTrack: () => true,
		getCodec: async () => null,
		getCodecParameterString: unavailable,
		getInternalCodecId: async () => 'A_DTS',
		getName: async () => 'Original soundtrack',
		getLanguageCode: async () => 'eng',
		getDisposition: async () => ({ default: true }),
		getBitrate: async () => null,
		getNumberOfChannels: async () => 6,
		getSampleRate: async () => 48000,
		canDecode: unavailable
	};
	const input = {
		getFormat: async () => ({ name: 'Matroska', mimeType: 'video/x-matroska' }),
		getTracks: async () => [audio],
		getDurationFromMetadata: unavailable
	} as unknown as Input;
	const properties = await readMediaProperties(input);
	expect(properties.duration).toBeNull();
	expect(properties.tracks[0]).toMatchObject({
		codec: null, internalCodec: 'A_DTS', channels: 6, sampleRate: 48000,
		name: 'Original soundtrack', language: 'eng', isDefault: true, canDecode: null
	});
});
