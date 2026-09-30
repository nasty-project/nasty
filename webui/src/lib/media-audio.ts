import type { WrappedAudioBuffer } from 'mediabunny';

/** Schedule against the actual media clock, including offsets after a seek. */
export function audioSchedule(timestamp: number, duration: number, mediaTime: number, rate: number) {
	if (!Number.isFinite(rate) || rate <= 0) return null;
	const offset = Math.max(0, mediaTime - timestamp);
	if (offset >= duration) return null;
	return { delay: Math.max(0, (timestamp - mediaTime) / rate), offset };
}

interface BufferSink {
	buffers(start?: number, end?: number): AsyncGenerator<WrappedAudioBuffer, void, unknown>;
}

/** Native video remains the clock. Bound decoding to two-second windows and
 * discard every scheduled node on pause, buffering, seek, or rate changes. */
export class MediaAudioSync {
	private generation = 0;
	private disposed = false;
	private pump: Promise<void> = Promise.resolve();
	private nodes = new Set<AudioBufferSourceNode>();
	private gain: GainNode;
	private previousMuted: boolean;
	private events: Array<[string, EventListener]> = [];

	constructor(
		private player: HTMLMediaElement,
		private context: AudioContext,
		private sink: BufferSink,
		private onError: (error: unknown) => void
	) {
		this.gain = context.createGain();
		this.gain.channelCount = 2;
		this.gain.channelCountMode = 'explicit';
		this.gain.channelInterpretation = 'speakers';
		this.gain.connect(context.destination);
		this.previousMuted = player.muted;
		player.muted = true; // Never mix the native and decoded soundtracks.
		this.listen('volumechange', () => { if (!player.muted) player.muted = true; });
		for (const event of ['pause', 'waiting', 'seeking', 'ended', 'emptied']) this.listen(event, () => this.stop());
		for (const event of ['playing', 'seeked', 'ratechange']) this.listen(event, () => this.restart());
		this.restart();
	}

	private listen(event: string, callback: EventListener) {
		this.events.push([event, callback]);
		this.player.addEventListener(event, callback);
	}

	setVolume(volume: number) { this.gain.gain.value = Math.max(0, Math.min(1, volume)); }

	private stop() {
		this.generation++;
		for (const node of this.nodes) {
			try { node.stop(); } catch { /* already ended */ }
			node.disconnect();
		}
		this.nodes.clear();
	}

	private restart() {
		this.stop();
		const generation = this.generation;
		if (this.disposed || this.player.paused || this.player.seeking || this.player.readyState < 3) return;
		// Serialize decoder iterators; a superseded read cannot schedule stale audio.
		this.pump = this.pump.catch(() => {}).then(async () => {
			if (!this.current(generation)) return;
			try {
				await this.context.resume();
				// Read the native media clock once per playback generation. Sampling
				// it for every buffer creates gaps/overlaps (especially with Firefox's
				// rounded currentTime). Web Audio owns the continuous sample timeline.
				const mediaAnchor = this.player.currentTime;
				const audioAnchor = this.context.currentTime;
				const rate = this.player.playbackRate;
				let start = mediaAnchor;
				while (this.current(generation)) {
					let yielded = false;
					for await (const chunk of this.sink.buffers(start, start + 2)) {
						if (!this.current(generation)) break;
						yielded = true;
						while (chunk.timestamp - this.player.currentTime > 0.5 && this.current(generation)) {
							await new Promise(resolve => setTimeout(resolve, 25));
						}
						if (!this.current(generation)) break;
						const audioNow = this.context.currentTime;
						const mediaNow = mediaAnchor + (audioNow - audioAnchor) * rate;
						const schedule = audioSchedule(chunk.timestamp, chunk.buffer.duration, mediaNow, rate);
						if (!schedule) continue;
						// The sink can return the buffer straddling a window boundary.
						// Play only this window's slice, never duplicate its leading samples.
						const offset = Math.max(schedule.offset, start - chunk.timestamp);
						const duration = Math.min(chunk.buffer.duration, start + 2 - chunk.timestamp) - offset;
						if (duration <= 0) continue;
						const node = this.context.createBufferSource();
						node.buffer = chunk.buffer;
						node.playbackRate.value = rate;
						node.connect(this.gain);
						this.nodes.add(node);
						node.onended = () => { this.nodes.delete(node); node.disconnect(); };
						node.start(audioAnchor + (chunk.timestamp + offset - mediaAnchor) / rate, offset, duration);
					}
					// Keep consecutive window boundaries exact and prepare the next
					// window before this one ends, avoiding polling-sized audio gaps.
					while (yielded && this.player.currentTime < start + 1.5 && this.current(generation)) {
						await new Promise(resolve => setTimeout(resolve, 25));
					}
					if (!yielded) break;
					start += 2;
				}
			} catch (error) {
				if (this.current(generation)) { this.stop(); this.onError(error); }
			}
		});
	}

	private current(generation: number) {
		return !this.disposed && generation === this.generation && !this.player.paused && !this.player.seeking && this.player.readyState >= 3;
	}

	dispose() {
		this.disposed = true;
		this.stop();
		for (const [event, callback] of this.events) this.player.removeEventListener(event, callback);
		this.gain.disconnect();
		this.player.muted = this.previousMuted;
	}
}
