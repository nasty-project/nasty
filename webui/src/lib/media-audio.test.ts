import { expect, test, vi } from 'vitest';
import { audioSchedule, MediaAudioSync } from './media-audio';

test('maps media timestamps to audio time without replaying samples before a seek', () => {
	expect(audioSchedule(10, 1, 9.5, 1)).toEqual({ delay: 0.5, offset: 0 });
	expect(audioSchedule(10, 1, 10.25, 1)).toEqual({ delay: 0, offset: 0.25 });
	expect(audioSchedule(10, 1, 9, 2)).toEqual({ delay: 0.5, offset: 0 });
	expect(audioSchedule(10, 1, 11, 1)).toBeNull();
	expect(audioSchedule(10, 1, 10, 0)).toBeNull();
});

test('pause cancels scheduled sound and disposal restores the native mute state', async () => {
	const player = Object.assign(new EventTarget(), { paused: false, seeking: false, readyState: 4, currentTime: 10, playbackRate: 1, muted: false });
	const node = { buffer: null, playbackRate: { value: 1 }, connect: vi.fn(), disconnect: vi.fn(), start: vi.fn(), stop: vi.fn(), onended: null };
	const gain = { gain: { value: 1 }, connect: vi.fn(), disconnect: vi.fn() };
	const context = { currentTime: 5, resume: async () => {}, createGain: () => gain, createBufferSource: () => node, destination: {} };
	const sink = { async *buffers() { yield { buffer: { duration: 0.5 }, timestamp: 10, duration: 0.5 }; } };
	const error = vi.fn();
	const sync = new MediaAudioSync(player as unknown as HTMLMediaElement, context as unknown as AudioContext, sink as never, error);
	await vi.waitFor(() => expect(node.start).toHaveBeenCalledWith(5, 0, 0.5));
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
