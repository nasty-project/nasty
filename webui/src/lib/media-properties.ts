import type { Input, InputTrack } from 'mediabunny';

export interface MediaTrackProperties {
	id: number;
	number: number;
	type: 'video' | 'audio';
	codec: string | null;
	codecParameter: string | null;
	internalCodec: string | null;
	name: string | null;
	language: string | null;
	isDefault: boolean | null;
	bitrate: number | null;
	width: number | null;
	height: number | null;
	channels: number | null;
	sampleRate: number | null;
	canDecode: boolean | null;
}

export interface MediaProperties {
	container: string;
	mimeType: string;
	duration: number | null;
	trackCount: number;
	tracks: MediaTrackProperties[];
}

// Some containers expose only a subset of these fields. Missing metadata or
// an unavailable WebCodecs probe must not hide the remaining file properties.
async function optional<T>(get: () => Promise<T>): Promise<T | null> {
	try { return await get(); } catch { return null; }
}

export function codecIdLabel(id: string | number | Uint8Array | null): string | null {
	if (id == null) return null;
	if (id instanceof Uint8Array) return Array.from(id.slice(0, 16), byte => byte.toString(16).padStart(2, '0')).join(' ');
	return String(id);
}

async function trackProperties(track: InputTrack): Promise<MediaTrackProperties> {
	return {
		id: track.id,
		number: track.number,
		type: track.isVideoTrack() ? 'video' : 'audio',
		codec: await optional(() => track.getCodec()),
		codecParameter: await optional(() => track.getCodecParameterString()),
		internalCodec: codecIdLabel(await optional(() => track.getInternalCodecId())),
		name: await optional(() => track.getName()),
		language: await optional(() => track.getLanguageCode()),
		isDefault: (await optional(() => track.getDisposition()))?.default ?? null,
		bitrate: await optional(() => track.getBitrate()),
		width: track.isVideoTrack() ? await optional(() => track.getDisplayWidth()) : null,
		height: track.isVideoTrack() ? await optional(() => track.getDisplayHeight()) : null,
		channels: track.isAudioTrack() ? await optional(() => track.getNumberOfChannels()) : null,
		sampleRate: track.isAudioTrack() ? await optional(() => track.getSampleRate()) : null,
		canDecode: await optional(() => track.canDecode())
	};
}

export async function readMediaProperties(input: Input): Promise<MediaProperties> {
	const format = await input.getFormat();
	const tracks = (await input.getTracks()).filter(track => track.isVideoTrack() || track.isAudioTrack());
	const summaries: MediaTrackProperties[] = [];
	// No packet scans or full-file frame-rate/bitrate calculations here. Keep
	// probing within the preview's existing time/read budget and limit huge track lists.
	for (const track of tracks.slice(0, 16)) summaries.push(await trackProperties(track));
	return {
		container: format.name,
		mimeType: format.mimeType,
		duration: await optional(() => input.getDurationFromMetadata()),
		trackCount: tracks.length,
		tracks: summaries
	};
}
