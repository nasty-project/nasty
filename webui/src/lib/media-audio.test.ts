import { expect, test, vi } from 'vitest';
import { audioSchedule, automaticDecodedTrack, MediaAudioSync } from './media-audio';
import type { MediaTrackProperties } from './media-properties';

function track(id: number, codec: string, isDefault = false): MediaTrackProperties {
	return { id, number: id, type: 'audio', codec, isDefault, codecParameter: null,
		internalCodec: null, name: null, language: null, bitrate: null, width: null,
		height: null, channels: 6, sampleRate: 48000, canDecode: true };
}

function pcmBuffer(channels: number, length: number, sampleRate: number): AudioBuffer {
	const data = Array.from({ length: channels }, () => new Float32Array(length));
	return { numberOfChannels: channels, length, sampleRate, duration: length / sampleRate, getChannelData: (channel: number) => data[channel] } as AudioBuffer;
}

function mediaPlayer(paused = false, rate = 1) {
	const player = Object.assign(new EventTarget(), { paused, seeking: false, readyState: 4, currentTime: 10, playbackRate: rate, muted: false, ended: false,
		pause: vi.fn(), play: vi.fn(async () => {}) });
	player.pause.mockImplementation(() => { if (!player.paused) { player.paused = true; player.dispatchEvent(new Event('pause')); } });
	player.play.mockImplementation(async () => { player.paused = false; player.dispatchEvent(new Event('play')); player.dispatchEvent(new Event('playing')); });
	return player;
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
	const player = mediaPlayer();
	const node = { buffer: null, playbackRate: { value: 1 }, connect: vi.fn(), disconnect: vi.fn(), start: vi.fn(), stop: vi.fn(), onended: null };
	const gain = { gain: { value: 1 }, connect: vi.fn(), disconnect: vi.fn() };
	const context = { currentTime: 5, resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, createBufferSource: () => node, destination: {} };
	const sink = { async *buffers() { yield { buffer: pcmBuffer(6, 24000, 48000), timestamp: 10, duration: 0.5 }; } };
	const error = vi.fn();
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, error);
	await vi.waitFor(() => expect(node.start).toHaveBeenCalledWith(5, 0, 0.192));
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
	const player = mediaPlayer();
	let deliver!: () => void;
	const wait = new Promise<void>(resolve => deliver = resolve);
	const create = vi.fn();
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const context = { currentTime: 5, resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, createBufferSource: create, destination: {} };
	const sink = { async *buffers() { await wait; yield { buffer: pcmBuffer(2, 24000, 48000), timestamp: 10, duration: 0.5 }; } };
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, vi.fn());
	await new Promise(resolve => setTimeout(resolve, 0));
	player.seeking = true;
	player.dispatchEvent(new Event('seeking'));
	deliver();
	await new Promise(resolve => setTimeout(resolve, 0));
	expect(create).not.toHaveBeenCalled();
	sync.dispose();
});

test('keeps a single decoder continuous despite rounded video-clock ticks', async () => {
	vi.useFakeTimers();
	let elapsed = 0;
	let mediaElapsed = 0;
	const player = mediaPlayer();
	Object.defineProperty(player, 'currentTime', { get: () => 10 + Math.floor(mediaElapsed * 10) / 10 });
	const scheduled: Array<{ time: number; offset: number; duration: number }> = [];
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const context = {
		get currentTime() { return 5 + elapsed; },
		resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, destination: {},
		createBufferSource: () => ({
			buffer: null, playbackRate: { value: 1 }, connect() {}, disconnect() {}, stop() {}, onended: null,
			start(time: number, offset: number, duration: number) { scheduled.push({ time, offset, duration }); }
		})
	};
	let opens = 0;
	const sink = {
		async *buffers(start = 10, end = Infinity) {
			opens++;
			// Like AudioBufferSink, include the frame that straddles start.
			for (let frame = Math.floor((start - 10) / 0.032); 10 + frame * 0.032 < end; frame++) {
				yield { buffer: pcmBuffer(2, 1536, 48000), timestamp: 10 + frame * 0.032, duration: 0.032 };
			}
		}
	};
	const error = vi.fn();
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, error);
	const clock = setInterval(() => { elapsed += 0.025; if (!player.paused) mediaElapsed += 0.025; }, 25);
	try {
		await vi.advanceTimersByTimeAsync(4500);
		expect(scheduled.length).toBeGreaterThan(20);
		expect(scheduled.length).toBeLessThan(35);
		for (let index = 1; index < scheduled.length; index++) {
			const previous = scheduled[index - 1];
			expect(scheduled[index].time).toBeCloseTo(previous.time + previous.duration, 8);
		}
		expect(scheduled.some(chunk => chunk.duration === 0.192)).toBe(true);
		expect(opens).toBe(1);
		expect(error).not.toHaveBeenCalled();
	} finally {
		sync.dispose();
		clearInterval(clock);
		await vi.runOnlyPendingTimersAsync();
		vi.useRealTimers();
	}
});

test('reports a sustained decoder stall without treating pause as an underrun', async () => {
	vi.useFakeTimers();
	let elapsed = 0;
	let mediaElapsed = 0;
	const player = mediaPlayer();
	Object.defineProperty(player, 'currentTime', { get: () => 10 + mediaElapsed });
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const context = {
		get currentTime() { return 5 + elapsed; }, sampleRate: 48000, state: 'running', baseLatency: 0.01,
		resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, destination: {},
		createBufferSource: () => ({ buffer: null, playbackRate: { value: 1 }, connect() {}, disconnect() {}, stop() {}, onended: null, start() {} })
	};
	const sink = { async *buffers(start = 10, end = Infinity) {
		for (let frame = Math.floor((start - 10) / 0.032); 10 + frame * 0.032 < end; frame++) {
			if (frame === 60) await new Promise(resolve => setTimeout(resolve, 1500));
			yield { buffer: pcmBuffer(2, 1536, 48000), timestamp: 10 + frame * 0.032, duration: 0.032 };
		}
	} };
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink, vi.fn());
	const clock = setInterval(() => { elapsed += 0.025; if (!player.paused) mediaElapsed += 0.025; }, 25);
	try {
		await vi.advanceTimersByTimeAsync(3000);
		const stats = sync.getDiagnostics();
		expect(stats.worstReadDecodeWaitMs).toBeGreaterThanOrEqual(1500);
		expect(stats.lateBatches).toBeGreaterThan(0);
		expect(stats.droppedBatches).toBeGreaterThan(0);
		expect(stats.possibleQueueGaps).toBeGreaterThan(0);
		expect(stats.trimmedLateMs).toBeGreaterThan(0);
		expect(stats.decodedBuffers).toBeGreaterThan(stats.scheduledBatches * 4);
		player.paused = true;
		player.dispatchEvent(new Event('pause'));
		expect(sync.getDiagnostics()).toMatchObject({ status: 'paused', activeNodes: 0, queuedAheadMs: 0, possibleQueueGaps: stats.possibleQueueGaps });
	} finally {
		sync.dispose(); clearInterval(clock); await vi.runOnlyPendingTimersAsync(); vi.useRealTimers();
	}
});

test.each([1, 1.75, 2])('maintains a bounded real-time scheduling margin at %sx with one decoder', async rate => {
	vi.useFakeTimers();
	let elapsed = 0, mediaElapsed = 0;
	const player = mediaPlayer(true, rate);
	Object.defineProperty(player, 'currentTime', { get: () => 10 + mediaElapsed });
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const context = {
		get currentTime() { return 5 + elapsed; }, sampleRate: 48000, state: 'running',
		resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, destination: {},
		createBufferSource: () => ({ buffer: null, playbackRate: { value: 1 }, connect() {}, disconnect() {}, stop() {}, onended: null, start() {} })
	};
	const buffers = vi.fn(async function* (start = 10) {
		for (let frame = 0; frame < 1500; frame++) {
			if (frame > 0 && frame % 60 === 0) await new Promise(resolve => setTimeout(resolve, 350));
			yield { buffer: pcmBuffer(2, 1536, 48000), timestamp: start + frame * 0.032, duration: 0.032 };
		}
	});
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, { buffers }, vi.fn());
	const clock = setInterval(() => { elapsed += 0.025; if (!player.paused) mediaElapsed += 0.025 * rate; }, 25);
	try {
		sync.play();
		for (let tick = 0; tick < 120; tick++) {
			await vi.advanceTimersByTimeAsync(100);
			const stats = sync.getDiagnostics();
			expect(stats.queuedAheadMs).toBeGreaterThan(450);
			expect(stats.queuedAheadMs).toBeLessThanOrEqual(1000 + 192 / rate + 1);
		}
		const stats = sync.getDiagnostics();
		expect(stats.prebufferedAheadMs).toBeGreaterThanOrEqual(500);
		expect(stats).toMatchObject({ decoderStarts: 1, lookAheadTargetMs: 1000, prebufferTargetMs: 500, lateBatches: 0, droppedBatches: 0, possibleQueueGaps: 0 });
		expect(buffers).toHaveBeenCalledTimes(1);
		expect(buffers).toHaveBeenCalledWith(10);
	} finally { sync.dispose(); clearInterval(clock); await vi.runOnlyPendingTimersAsync(); vi.useRealTimers(); }
});

test.each([false, true])('a slow prebuffer keeps video paused and respects cancellation=%s', async cancel => {
	vi.useFakeTimers();
	let elapsed = 0;
	const player = mediaPlayer(true);
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const startNode = vi.fn();
	const context = {
		get currentTime() { return 5 + elapsed; }, sampleRate: 48000, state: 'running',
		resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, destination: {},
		createBufferSource: () => ({ buffer: null, playbackRate: { value: 1 }, connect() {}, disconnect() {}, stop() {}, onended: null, start: startNode })
	};
	let closed = false;
	const sink = { async *buffers(start = 10) {
		try {
			for (let frame = 0; frame < 100; frame++) {
				if (frame === 2) await new Promise(resolve => setTimeout(resolve, 700));
				yield { buffer: pcmBuffer(2, 1536, 48000), timestamp: start + frame * 0.032, duration: 0.032 };
			}
		} finally { closed = true; }
	} };
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink, vi.fn());
	const clock = setInterval(() => { elapsed += 0.025; }, 25);
	try {
		sync.play();
		await vi.advanceTimersByTimeAsync(500);
		expect(player.paused).toBe(true);
		expect(player.play).not.toHaveBeenCalled();
		expect(startNode).not.toHaveBeenCalled();
		expect(sync.getDiagnostics().status).toBe('buffering');
		if (cancel) sync.pause();
		await vi.advanceTimersByTimeAsync(300);
		if (cancel) { expect(player.play).not.toHaveBeenCalled(); expect(startNode).not.toHaveBeenCalled(); expect(closed).toBe(true); }
		else { expect(player.play).toHaveBeenCalledOnce(); expect(startNode).toHaveBeenCalled(); expect(sync.getDiagnostics().lateBatches).toBe(0); }
	} finally { sync.dispose(); clearInterval(clock); await vi.runOnlyPendingTimersAsync(); vi.useRealTimers(); }
});

test('plays a short EOF tail and lets native play load video beyond metadata readiness', async () => {
	const player = mediaPlayer(true);
	player.readyState = 1;
	player.play.mockImplementation(async () => {
		player.paused = false; player.readyState = 4;
		player.dispatchEvent(new Event('play')); player.dispatchEvent(new Event('playing'));
	});
	const gain = { gain: { value: 1 }, connect() {}, disconnect() {} };
	const start = vi.fn();
	const context = { currentTime: 5, resume: async () => {}, createGain: () => gain, createBuffer: pcmBuffer, destination: {},
		createBufferSource: () => ({ buffer: null, playbackRate: { value: 1 }, connect() {}, disconnect() {}, stop() {}, onended: null, start }) };
	const sink = { async *buffers() { yield { buffer: pcmBuffer(2, 4608, 48000), timestamp: 10, duration: 0.096 }; } };
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink, vi.fn());
	try {
		sync.play();
		await vi.waitFor(() => expect(start).toHaveBeenCalledWith(5, 0, 0.096));
		expect(player.play).toHaveBeenCalledOnce();
		expect(sync.getDiagnostics().prebufferedAheadMs).toBe(96);
	} finally { sync.dispose(); }
});
