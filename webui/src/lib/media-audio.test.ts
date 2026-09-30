import { expect, test, vi } from 'vitest';
import { audioSchedule, automaticDecodedTrack, MediaAudioSync } from './media-audio';
import type { MediaTrackProperties } from './media-properties';

function track(id: number, codec: string, isDefault = false): MediaTrackProperties {
	return { id, number: id, type: 'audio', codec, isDefault, codecParameter: null,
		internalCodec: null, name: null, language: null, bitrate: null, width: null,
		height: null, channels: 6, sampleRate: 48000, canDecode: true };
}

test('defaults to decoded E-AC-3 when native audio is unsupported, even if WASM was registered earlier', () => {
	const native = vi.fn(() => '' as CanPlayTypeResult);
	expect(automaticDecodedTrack([track(1, 'ac3'), track(2, 'eac3', true)], native)).toBe(2);
	expect(native).toHaveBeenCalledWith('audio/mp4; codecs="ec-3"');
});

test('retains native playback for a supported codec or an AAC default with an AC-3 alternative', () => {
	expect(automaticDecodedTrack([track(1, 'eac3', true)], () => 'probably')).toBeNull();
	expect(automaticDecodedTrack([track(1, 'ac3', true)], () => 'maybe')).toBeNull();
	const native = vi.fn(() => '' as CanPlayTypeResult);
	expect(automaticDecodedTrack([track(1, 'ac3'), track(2, 'aac', true)], native)).toBeNull();
	expect(native).not.toHaveBeenCalled();
});

test('uses the first audio track when no default is declared and ignores absent or unsupported fallback codecs', () => {
	expect(automaticDecodedTrack([track(1, 'ac3'), track(2, 'eac3')], () => '')).toBe(1);
	expect(automaticDecodedTrack([], () => '')).toBeNull();
	expect(automaticDecodedTrack([track(1, 'dts', true)], () => '')).toBeNull();
});

test('maps media timestamps to audio time without replaying samples before a seek', () => {
	expect(audioSchedule(10, 1, 9.5, 1)).toEqual({ delay: 0.5, offset: 0 });
	expect(audioSchedule(10, 1, 10.25, 1)).toEqual({ delay: 0, offset: 0.25 });
	expect(audioSchedule(10, 1, 9, 2)).toEqual({ delay: 0.5, offset: 0 });
	expect(audioSchedule(10, 1, 11, 1)).toBeNull();
	expect(audioSchedule(10, 1, 10, 0)).toBeNull();
});

test('surround playback preserves volume headroom, pause cancels sound, and disposal restores native mute', async () => {
	const player = Object.assign(new EventTarget(), { paused: false, seeking: false, readyState: 4, currentTime: 10, playbackRate: 1, muted: false });
	const node = { buffer: null, playbackRate: { value: 1 }, connect: vi.fn(), disconnect: vi.fn(), start: vi.fn(), stop: vi.fn(), onended: null };
	const gain = { gain: { value: 1 }, connect: vi.fn(), disconnect: vi.fn() };
	const context = { currentTime: 5, resume: async () => {}, createGain: () => gain, createBufferSource: () => node, destination: {} };
	const sink = { async *buffers() { yield { buffer: { duration: 0.5, numberOfChannels: 6 }, timestamp: 10, duration: 0.5 }; } };
	const error = vi.fn();
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, error);
	await vi.waitFor(() => expect(node.start).toHaveBeenCalledWith(5, 0, 0.5));
	const fullGain = gain.gain.value;
	expect(fullGain).toBeGreaterThan(0);
	expect(fullGain).toBeLessThan(0.5);
	sync.setVolume(0.5);
	expect(gain.gain.value).toBe(fullGain / 2);
	sync.setVolume(0);
	expect(gain.gain.value).toBe(0);
	sync.setVolume(1);
	expect(gain.gain.value).toBe(fullGain);
	expect(player.muted).toBe(true);
	player.paused = true;
	player.dispatchEvent(new Event('pause'));
	expect(node.stop).toHaveBeenCalledOnce();
	sync.dispose();
	expect(player.muted).toBe(false);
	expect(gain.disconnect).toHaveBeenCalledOnce();
	expect(error).not.toHaveBeenCalled();
});

test('a read completed after seeking cannot schedule stale audio', async () => {
	const player = Object.assign(new EventTarget(), { paused: false, seeking: false, readyState: 4, currentTime: 10, playbackRate: 1, muted: false });
	let deliver!: () => void;
	const wait = new Promise<void>(resolve => deliver = resolve);
	const create = vi.fn();
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const context = { currentTime: 5, resume: async () => {}, createGain: () => gain, createBufferSource: create, destination: {} };
	const sink = { async *buffers() { await wait; yield { buffer: { duration: 0.5 }, timestamp: 10, duration: 0.5 }; } };
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, vi.fn());
	await new Promise(resolve => setTimeout(resolve, 0));
	player.seeking = true;
	player.dispatchEvent(new Event('seeking'));
	deliver();
	await new Promise(resolve => setTimeout(resolve, 0));
	expect(create).not.toHaveBeenCalled();
	sync.dispose();
});

test('keeps PCM continuous across rounded video-clock ticks and overlapping decode windows', async () => {
	vi.useFakeTimers();
	let elapsed = 0;
	const player = Object.assign(new EventTarget(), { paused: false, seeking: false, readyState: 4, playbackRate: 1, muted: false });
	Object.defineProperty(player, 'currentTime', { get: () => 10 + Math.floor(elapsed * 10) / 10 });
	const scheduled: Array<{ time: number; offset: number; duration: number }> = [];
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const context = {
		get currentTime() { return 5 + elapsed; },
		resume: async () => {}, createGain: () => gain, destination: {},
		createBufferSource: () => ({
			buffer: null, playbackRate: { value: 1 }, connect() {}, disconnect() {}, stop() {}, onended: null,
			start(time: number, offset: number, duration: number) { scheduled.push({ time, offset, duration }); }
		})
	};
	const sink = {
		async *buffers(start = 10, end = Infinity) {
			// Like AudioBufferSink, include the frame that straddles start.
			for (let frame = Math.floor((start - 10) / 0.032); 10 + frame * 0.032 < end; frame++) {
				yield { buffer: { duration: 0.032 }, timestamp: 10 + frame * 0.032, duration: 0.032 };
			}
		}
	};
	const error = vi.fn();
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, error);
	const clock = setInterval(() => { elapsed += 0.025; }, 25);
	try {
		await vi.advanceTimersByTimeAsync(4500);
		expect(scheduled.length).toBeGreaterThan(120);
		for (let index = 1; index < scheduled.length; index++) {
			const previous = scheduled[index - 1];
			expect(scheduled[index].time).toBeCloseTo(previous.time + previous.duration, 8);
		}
		expect(scheduled.some(chunk => chunk.offset > 0 && chunk.duration < 0.032)).toBe(true);
		expect(error).not.toHaveBeenCalled();
	} finally {
		sync.dispose();
		clearInterval(clock);
		await vi.runOnlyPendingTimersAsync();
		vi.useRealTimers();
	}
});
