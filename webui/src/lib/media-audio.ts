import type { WrappedAudioBuffer } from 'mediabunny';
import type { MediaTrackProperties } from './media-properties';
import { batchPcmBuffers, PCM_BATCH_SECONDS } from './media-pcm';

const LOOK_AHEAD_SECONDS = 1;
const PREBUFFER_SECONDS = 0.5;

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
	lookAheadTargetMs: number;
	prebufferTargetMs: number;
	prebufferedAheadMs: number;
	decoderStarts: number;
	clockDriftMs: number | null;
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

/** One backpressured decoder per playback period. Prebuffer before starting
 * video, then schedule against Web Audio with a real-time look-ahead budget. */
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
	private wanted = false;
	private buffering = false;
	private internalPauses = 0;
	private startingNative = false;
	private prebufferedSeconds = 0;
	private decoderStarts = 0;
	private mapping: { media: number; audio: number; rate: number } | null = null;

	constructor(
		private player: HTMLMediaElement,
		private context: AudioContext,
		private sink: BufferSink,
		private onError: (error: unknown) => void,
		private onBuffering: (buffering: boolean) => void = () => {}
	) {
		this.gain = context.createGain();
		this.gain.channelCount = 2;
		this.gain.channelCountMode = 'explicit';
		this.gain.channelInterpretation = 'speakers';
		this.gain.connect(context.destination);
		this.previousMuted = player.muted;
		player.muted = true; // Never mix the native and decoded soundtracks.
		this.listen('volumechange', () => { if (!player.muted) player.muted = true; });
		this.listen('pause', () => {
			if (this.internalPauses) { this.internalPauses--; return; }
			if (this.player.paused) this.pause();
		});
		this.listen('play', () => { if (!this.player.paused && !this.startingNative && !this.wanted) this.play(); });
		this.listen('waiting', () => { if (this.player.readyState < 3 && this.wanted && !this.startingNative && !this.buffering) this.restart(); });
		this.listen('seeking', () => { this.stop(); this.holdPlayer(); });
		for (const event of ['seeked', 'ratechange']) this.listen(event, () => { if (this.wanted) this.restart(); });
		for (const event of ['ended', 'emptied']) this.listen(event, () => this.pause());
		if (!player.paused) this.play();
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

	play() {
		if (this.disposed || this.wanted) return;
		this.wanted = true;
		if (this.player.ended) this.player.currentTime = 0;
		this.restart();
	}

	pause() {
		this.wanted = false;
		this.stop();
		this.holdPlayer();
	}

	private holdPlayer() {
		if (!this.player.paused) { this.internalPauses++; this.player.pause(); }
	}

	private setBuffering(value: boolean) {
		if (this.buffering === value) return;
		this.buffering = value;
		this.onBuffering(value);
	}

	getDiagnostics(): AudioDiagnostics {
		const c = this.counters;
		return {
			status: this.disposed ? 'stopped' : this.buffering ? 'buffering' : this.player.seeking ? 'seeking' : this.player.paused ? 'paused' : 'playing',
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
			outputLatencyMs: Number.isFinite(this.context.outputLatency) ? this.context.outputLatency * 1000 : null,
			lookAheadTargetMs: LOOK_AHEAD_SECONDS * 1000, prebufferTargetMs: PREBUFFER_SECONDS * 1000,
			prebufferedAheadMs: this.prebufferedSeconds * 1000, decoderStarts: this.decoderStarts,
			clockDriftMs: this.mapping && !this.player.paused ? (this.player.currentTime - (this.mapping.media + (this.context.currentTime - this.mapping.audio) * this.mapping.rate)) / this.mapping.rate * 1000 : null
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
		this.mapping = null;
		this.prebufferedSeconds = 0;
		this.setBuffering(false);
	}

	private restart() {
		this.stop();
		this.holdPlayer();
		const generation = this.generation;
		if (!this.valid(generation) || this.player.seeking) return;
		this.setBuffering(true);
		// Serialize decoder iterators; a superseded read cannot schedule stale audio.
		this.pump = this.pump.catch(() => {}).then(async () => {
			if (!this.valid(generation)) return;
			let batches: AsyncGenerator<WrappedAudioBuffer> | undefined;
			try {
				await this.context.resume();
				if (!this.valid(generation)) return;
				const start = this.player.currentTime;
				const rate = this.player.playbackRate;
				this.decoderStarts++;
				batches = batchPcmBuffers(this.sink.buffers(start), {
					start, end: Infinity, createBuffer: this.context.createBuffer.bind(this.context),
					isCurrent: () => this.valid(generation),
					onRead: (waitMs, buffer) => {
						this.counters.decodedBuffers++;
						this.counters.readWaitMs += waitMs;
						this.counters.worstReadWaitMs = Math.max(this.counters.worstReadWaitMs, waitMs);
						this.pcmSampleRate = buffer.sampleRate;
					},
					onDiscontinuity: () => { this.counters.sourceDiscontinuities++; }
				});
				const prebuffer: WrappedAudioBuffer[] = [];
				while (this.valid(generation) && this.prebufferedSeconds < PREBUFFER_SECONDS) {
					const next = await batches.next();
					if (!this.valid(generation)) return;
					if (next.done) break;
					prebuffer.push(next.value);
					// Count playable samples, not a timestamp span that might contain gaps.
					this.prebufferedSeconds += next.value.buffer.duration / rate;
				}
				if (!prebuffer.length) { this.pause(); return; }
				while (this.valid(generation) && this.player.seeking) await new Promise(resolve => setTimeout(resolve, 25));
				if (!this.valid(generation)) return;
				this.startingNative = true;
				try { await this.player.play(); } finally { this.startingNative = false; }
				if (!this.valid(generation)) return;
				// Anchor only after both data paths are ready. Fetch/decode preparation
				// cannot consume the first words while video runs ahead silently.
				const mediaAnchor = this.player.currentTime;
				const audioAnchor = this.context.currentTime;
				this.mapping = { media: mediaAnchor, audio: audioAnchor, rate };
				this.setBuffering(false);
				const scheduleBatch = async (chunk: WrappedAudioBuffer) => {
					while (audioAnchor + (chunk.timestamp - mediaAnchor) / rate - this.context.currentTime > LOOK_AHEAD_SECONDS && this.current(generation)) {
						await new Promise(resolve => setTimeout(resolve, 25));
					}
					if (!this.current(generation)) return;
					const audioNow = this.context.currentTime;
					const mediaNow = mediaAnchor + (audioNow - audioAnchor) * rate;
					const latenessMs = Math.max(0, (mediaNow - chunk.timestamp) / rate) * 1000;
					this.counters.worstLateMs = Math.max(this.counters.worstLateMs, latenessMs);
					if (latenessMs > 5) this.counters.lateBatches++;
					const schedule = audioSchedule(chunk.timestamp, chunk.buffer.duration, mediaNow, rate);
					this.counters.trimmedLateMs += Math.min(chunk.buffer.duration, Math.max(0, mediaNow - chunk.timestamp)) * 1000;
					if (!schedule) { this.counters.droppedBatches++; return; }
					const offset = schedule.offset;
					const duration = chunk.buffer.duration - offset;
					if (duration <= 0) return;
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
				};
				for (const chunk of prebuffer) { if (!this.current(generation)) return; await scheduleBatch(chunk); }
				while (this.current(generation)) {
					const next = await batches.next();
					if (!this.current(generation) || next.done) break;
					await scheduleBatch(next.value);
				}
			} catch (error) {
				if (this.valid(generation)) { this.pause(); this.onError(error); }
			} finally {
				try { await batches?.return(undefined); }
				catch (error) { if (this.valid(generation)) { this.pause(); this.onError(error); } }
			}
		});
	}

	private current(generation: number) {
		return this.valid(generation) && !this.player.paused && !this.player.seeking && this.player.readyState >= 3;
	}

	private valid(generation: number) {
		return !this.disposed && this.wanted && generation === this.generation;
	}

	dispose() {
		this.disposed = true;
		this.stop();
		for (const [event, callback] of this.events) this.player.removeEventListener(event, callback);
		this.gain.disconnect();
		this.player.muted = this.previousMuted;
	}
}
