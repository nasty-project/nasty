import type { WrappedAudioBuffer } from 'mediabunny';
import type { MediaTrackProperties } from './media-properties';
import { batchPcmBuffers, PCM_BATCH_SECONDS } from './media-pcm';

export interface AudioDiagnostics {
	status: string;
	contextState: AudioContextState;
	contextSampleRate: number;
	pcmSampleRate: number;
	channels: number;
	playbackRate: number;
	targetBatchMs: number;
	averageBatchMs: number;
	queuedAheadMs: number;
	activeNodes: number;
	decodedBuffers: number;
	scheduledBatches: number;
	lateBatches: number;
	worstLateMs: number;
	trimmedLateMs: number;
	droppedBatches: number;
	possibleQueueGaps: number;
	queueGapMs: number;
	sourceDiscontinuities: number;
	averageReadDecodeWaitMs: number;
	worstReadDecodeWaitMs: number;
	baseLatencyMs: number;
	outputLatencyMs: number | null;
}

/** Native audio support is independent of Mediabunny's optional decoders. */
export function automaticDecodedTrack(tracks: MediaTrackProperties[], canPlayType: (type: string) => CanPlayTypeResult): number | null {
	const audio = tracks.filter(track => track.type === 'audio');
	const primary = audio.find(track => track.isDefault) ?? audio[0];
	if (!primary || !['ac3', 'eac3'].includes(primary.codec ?? '')) return null;
	const codec = primary.codecParameter ?? (primary.codec === 'ac3' ? 'ac-3' : 'ec-3');
	return canPlayType(`audio/mp4; codecs="${codec}"`) ? null : primary.id;
}

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
	private volume = 1;
	private channels = 2;
	private previousMuted: boolean;
	private events: Array<[string, EventListener]> = [];
	private queuedUntil: number | null = null;
	private counters = { decodedBuffers: 0, scheduledBatches: 0, scheduledSeconds: 0, lateBatches: 0, worstLateMs: 0, trimmedLateMs: 0, droppedBatches: 0, possibleQueueGaps: 0, queueGapMs: 0, sourceDiscontinuities: 0, readWaitMs: 0, worstReadWaitMs: 0 };
	private pcmSampleRate = 0;

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

	setVolume(volume: number) {
		this.volume = Math.max(0, Math.min(1, volume));
		// Web Audio's speaker downmix sums L + 0.707*C + 0.707*SL for
		// 5.1, or L + 0.5*SL for quad. Normalize those coefficient sums
		// so correlated full-scale channels cannot clip the stereo output.
		const headroom = this.channels === 6 ? 1 / (1 + Math.SQRT2) : this.channels === 4 ? 2 / 3 : 1;
		this.gain.gain.value = this.volume * headroom;
	}

	getDiagnostics(): AudioDiagnostics {
		const c = this.counters;
		return {
			status: this.disposed ? 'stopped' : this.player.seeking ? 'seeking' : this.player.paused ? 'paused' : 'playing',
			contextState: this.context.state, contextSampleRate: this.context.sampleRate, pcmSampleRate: this.pcmSampleRate,
			channels: this.channels, playbackRate: this.player.playbackRate, targetBatchMs: PCM_BATCH_SECONDS * 1000,
			averageBatchMs: c.scheduledBatches ? c.scheduledSeconds * 1000 / c.scheduledBatches : 0,
			queuedAheadMs: Math.max(0, (this.queuedUntil ?? this.context.currentTime) - this.context.currentTime) * 1000,
			activeNodes: this.nodes.size, decodedBuffers: c.decodedBuffers, scheduledBatches: c.scheduledBatches,
			lateBatches: c.lateBatches, worstLateMs: c.worstLateMs, trimmedLateMs: c.trimmedLateMs,
			droppedBatches: c.droppedBatches, possibleQueueGaps: c.possibleQueueGaps, queueGapMs: c.queueGapMs,
			sourceDiscontinuities: c.sourceDiscontinuities,
			averageReadDecodeWaitMs: c.decodedBuffers ? c.readWaitMs / c.decodedBuffers : 0, worstReadDecodeWaitMs: c.worstReadWaitMs,
			baseLatencyMs: (this.context.baseLatency ?? 0) * 1000,
			outputLatencyMs: Number.isFinite(this.context.outputLatency) ? this.context.outputLatency * 1000 : null
		};
	}

	private stop() {
		this.generation++;
		for (const node of this.nodes) {
			try { node.stop(); } catch { /* already ended */ }
			node.disconnect();
		}
		this.nodes.clear();
		this.queuedUntil = null;
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
					const batches = batchPcmBuffers(this.sink.buffers(start, start + 2), {
						start, end: start + 2, createBuffer: this.context.createBuffer.bind(this.context),
						isCurrent: () => this.current(generation),
						onRead: (waitMs, buffer) => {
							this.counters.decodedBuffers++;
							this.counters.readWaitMs += waitMs;
							this.counters.worstReadWaitMs = Math.max(this.counters.worstReadWaitMs, waitMs);
							this.pcmSampleRate = buffer.sampleRate;
						},
						onDiscontinuity: () => { this.counters.sourceDiscontinuities++; }
					});
					for await (const chunk of batches) {
						if (!this.current(generation)) break;
						yielded = true;
						while (chunk.timestamp - this.player.currentTime > 0.5 && this.current(generation)) {
							await new Promise(resolve => setTimeout(resolve, 25));
						}
						if (!this.current(generation)) break;
						const audioNow = this.context.currentTime;
						const mediaNow = mediaAnchor + (audioNow - audioAnchor) * rate;
						const latenessMs = Math.max(0, (mediaNow - chunk.timestamp) / rate) * 1000;
						this.counters.worstLateMs = Math.max(this.counters.worstLateMs, latenessMs);
						if (latenessMs > 5) this.counters.lateBatches++;
						const schedule = audioSchedule(chunk.timestamp, chunk.buffer.duration, mediaNow, rate);
						this.counters.trimmedLateMs += Math.min(chunk.buffer.duration, Math.max(0, mediaNow - chunk.timestamp)) * 1000;
						if (!schedule) { this.counters.droppedBatches++; continue; }
						// Window boundaries have already been clipped to exact PCM samples.
						const offset = schedule.offset;
						const duration = chunk.buffer.duration - offset;
						if (duration <= 0) continue;
						if (this.channels !== chunk.buffer.numberOfChannels) {
							this.channels = chunk.buffer.numberOfChannels;
							this.setVolume(this.volume);
						}
						const node = this.context.createBufferSource();
						node.buffer = chunk.buffer;
						node.playbackRate.value = rate;
						node.connect(this.gain);
						this.nodes.add(node);
						node.onended = () => { this.nodes.delete(node); node.disconnect(); };
						const when = audioAnchor + (chunk.timestamp + offset - mediaAnchor) / rate;
						if (this.queuedUntil != null && when - this.queuedUntil > 0.005) {
							this.counters.possibleQueueGaps++;
							this.counters.queueGapMs += (when - this.queuedUntil) * 1000;
						}
						node.start(when, offset, duration);
						this.queuedUntil = when + duration / rate;
						this.counters.scheduledBatches++;
						this.counters.scheduledSeconds += chunk.buffer.duration;
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
