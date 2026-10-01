import type { WrappedAudioBuffer } from 'mediabunny';

// Six typical 32 ms AC-3 frames; keep allocation and decode-ahead bounded.
export const PCM_BATCH_SECONDS = 0.192;

interface BatchOptions {
	start: number;
	end: number;
	createBuffer: (channels: number, frames: number, sampleRate: number) => AudioBuffer;
	isCurrent: () => boolean;
	onRead?: (waitMs: number, buffer: AudioBuffer) => void;
	onDiscontinuity?: () => void;
}

/** Copy contiguous PCM into larger sources without padding gaps, mixing formats,
 * or duplicating the sample that straddles a decode-window boundary. */
export async function* batchPcmBuffers(source: AsyncIterable<WrappedAudioBuffer>, options: BatchOptions): AsyncGenerator<WrappedAudioBuffer> {
	let pieces: Array<{ buffer: AudioBuffer; offset: number; length: number }> = [];
	let frames = 0;
	let timestamp = 0;
	let sampleRate = 0;
	let channels = 0;
	let previousEnd: number | null = null;
	const flush = (): WrappedAudioBuffer => {
		const buffer = options.createBuffer(channels, frames, sampleRate);
		for (let channel = 0; channel < channels; channel++) {
			const output = buffer.getChannelData(channel);
			let offset = 0;
			for (const piece of pieces) {
				output.set(piece.buffer.getChannelData(channel).subarray(piece.offset, piece.offset + piece.length), offset);
				offset += piece.length;
			}
		}
		const result = { buffer, timestamp, duration: frames / sampleRate };
		pieces = [];
		frames = 0;
		return result;
	};
	const iterator = source[Symbol.asyncIterator]();
	try {
		while (options.isCurrent()) {
			const before = performance.now();
			const result = await iterator.next();
			if (!options.isCurrent()) return;
			if (result.done) break;
			const { buffer, timestamp: sourceTime } = result.value;
			options.onRead?.(performance.now() - before, buffer);
			const rate = buffer.sampleRate;
			let offset = Math.max(0, Math.min(buffer.length, Math.round((options.start - sourceTime) * rate)));
			const end = Math.max(offset, Math.min(buffer.length, Math.round((options.end - sourceTime) * rate)));
			if (offset === end) continue;
			if (previousEnd != null && (sampleRate !== rate || channels !== buffer.numberOfChannels || Math.abs(sourceTime + offset / rate - previousEnd) > 1 / rate)) {
				options.onDiscontinuity?.();
				if (frames) yield flush();
			}
			previousEnd = sourceTime + end / rate;
			while (offset < end && options.isCurrent()) {
				if (!frames) { timestamp = sourceTime + offset / rate; sampleRate = rate; channels = buffer.numberOfChannels; }
				const target = Math.max(1, Math.round(sampleRate * PCM_BATCH_SECONDS));
				const length = Math.min(end - offset, target - frames);
				pieces.push({ buffer, offset, length });
				frames += length;
				offset += length;
				if (frames === target) yield flush();
			}
		}
		if (frames && options.isCurrent()) yield flush();
	} finally {
		await iterator.return?.();
	}
}
