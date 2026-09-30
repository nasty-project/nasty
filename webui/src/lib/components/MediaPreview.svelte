<script lang="ts">
	import { onMount } from 'svelte';
	import { mediaPreviewKind } from '$lib/public-share';
	import { formatBytes } from '$lib/format';
	import { readMediaProperties, type MediaProperties, type MediaTrackProperties } from '$lib/media-properties';
	import { MediaAudioSync } from '$lib/media-audio';

	let { url, name, onclose, mediaKind }: { url: string; name: string; onclose?: () => void; mediaKind?: 'video' | 'audio' } = $props();
	let details = $state('Inspecting media with Mediabunny…');
	let inspectionError = $state('');
	let playbackError = $state(false);
	let thumbnail = $state<HTMLCanvasElement>();
	let hasThumbnail = $state(false);
	let properties = $state<MediaProperties | null>(null);
	let fileSize = $state<number | null>(null);
	let player = $state<HTMLMediaElement>();
	let seeking = $state(false);
	let nativeSupport = $state('Not checked');
	const kind = $derived(mediaKind ?? mediaPreviewKind(name));
	const ac3Tracks = $derived(properties?.tracks.filter(track => track.type === 'audio' && ['ac3', 'eac3'].includes(track.codec ?? '')) ?? []);
	let selectedAudioId = $state<number | null>(null);
	let audioLoading = $state(false);
	let decodedAudio = $state(false);
	let wasmDecoderLoaded = $state(false);
	let audioError = $state('');
	let audioVolume = $state(1);
	let audioSync: MediaAudioSync | undefined;
	let audioInput: import('mediabunny').Input | undefined;
	let audioContext: AudioContext | undefined;
	let audioGeneration = 0;

	function decoderStatus(track: MediaTrackProperties) {
		if (track.type === 'audio' && ['ac3', 'eac3'].includes(track.codec ?? '')) {
			if (decodedAudio && track.id === selectedAudioId) return 'Active (WASM decoder)';
			if (wasmDecoderLoaded) return 'Optional WASM decoder loaded';
			return track.canDecode ? 'Available' : 'Optional WASM decoder not loaded';
		}
		return track.canDecode == null ? 'Unknown' : track.canDecode ? 'Available' : 'Unavailable to Mediabunny';
	}

	function stopDecodedAudio() {
		audioGeneration++;
		audioSync?.dispose();
		audioSync = undefined;
		audioInput?.dispose();
		audioInput = undefined;
		void audioContext?.close().catch(() => {});
		audioContext = undefined;
		decodedAudio = false;
		audioLoading = false;
	}

	async function enableDecodedAudio() {
		if (!player || selectedAudioId == null) return;
		const wasPlaying = !player.paused;
		player.pause();
		stopDecodedAudio();
		const generation = audioGeneration;
		audioLoading = true;
		audioError = '';
		try {
			// Open/resume Web Audio in the button gesture, before fetching WASM.
			// Match the PCM rate: resampling each short source independently can
			// introduce high-frequency artifacts at its boundaries.
			const sampleRate = ac3Tracks.find(track => track.id === selectedAudioId)?.sampleRate;
			audioContext = new AudioContext(sampleRate ? { sampleRate } : undefined);
			await audioContext.resume();
			const [{ registerAc3Decoder }, m] = await Promise.all([import('@mediabunny/ac3'), import('$lib/media-readers')]);
			if (generation !== audioGeneration) return;
			registerAc3Decoder();
			wasmDecoderLoaded = true;
			audioInput = new m.Input({
				formats: [m.MP4, m.QTFF, m.MATROSKA, m.WEBM],
				source: new m.UrlSource(url, {
					maxCacheSize: 4 * 1024 * 1024,
					getRetryDelay: () => null,
					fetchFn: (request, init) => fetch(request, {
						...init, redirect: 'error',
						signal: AbortSignal.any([...(init?.signal ? [init.signal] : []), AbortSignal.timeout(15000)])
					})
				})
			});
			const track = (await audioInput.getAudioTracks()).find(track => track.id === selectedAudioId);
			if (generation !== audioGeneration) return;
			if (!track || !['ac3', 'eac3'].includes(await track.getCodec() ?? '') || !await track.canDecode()) {
				throw new Error('The selected AC-3/E-AC-3 track could not be decoded.');
			}
			if (generation !== audioGeneration) return;
			audioSync = new MediaAudioSync(player, audioContext, new m.AudioBufferSink(track), error => {
				stopDecodedAudio();
				audioError = error instanceof Error ? error.message : 'Audio decoding failed.';
			});
			audioSync.setVolume(audioVolume);
			decodedAudio = true;
			if (wasPlaying) await player.play();
		} catch (error) {
			if (generation === audioGeneration) {
				stopDecodedAudio();
				audioError = error instanceof Error ? error.message : 'Audio decoding failed.';
			}
		} finally {
			if (generation === audioGeneration) audioLoading = false;
		}
	}

	onMount(() => {
		let cancelled = false;
		const controller = new AbortController();
		let input: import('mediabunny').Input | undefined;
		// Metadata/frame probing is capped independently of native playback.
		// Never load the whole file merely to inspect a large video.
		let readBytes = 0;
		const deadline = setTimeout(() => {
			controller.abort();
			input?.dispose();
		}, 15000);
		void (async () => {
			try {
				const m = await import('$lib/media-readers');
				if (cancelled) return;
				input = new m.Input({
					formats: [m.MP4, m.QTFF, m.MATROSKA, m.WEBM, m.MP3, m.WAVE, m.OGG, m.FLAC, m.ADTS],
					source: new m.CustomSource({
						maxCacheSize: 4 * 1024 * 1024,
						getSize: async () => {
							const response = await fetch(url, { method: 'HEAD', signal: controller.signal, redirect: 'error' });
							if (!response.ok) throw new Error('Preview access expired or this file is unavailable.');
							const length = Number(response.headers.get('Content-Length'));
							if (!Number.isSafeInteger(length) || length <= 0) throw new Error('Media size is unavailable.');
							if (!cancelled) fileSize = length;
							return length;
						},
						read: async (start, end) => {
							if (end - start > 1024 * 1024 || readBytes + end - start > 16 * 1024 * 1024) {
								throw new Error('Media inspection exceeded the prototype read budget.');
							}
							readBytes += end - start;
							const response = await fetch(url, {
								headers: { Range: `bytes=${start}-${end - 1}` },
								signal: controller.signal, redirect: 'error'
							});
							if (response.status !== 206 || Number(response.headers.get('Content-Length')) !== end - start) {
								await response.body?.cancel();
								throw new Error('Media range access failed or the share expired.');
							}
							const bytes = new Uint8Array(await response.arrayBuffer());
							if (bytes.length !== end - start) throw new Error('Media changed while reading.');
							return bytes;
						}
					})
				});
				const nextProperties = await readMediaProperties(input);
				if (cancelled) return;
				properties = nextProperties;
				const audioTracks = nextProperties.tracks.filter(track => track.type === 'audio' && ['ac3', 'eac3'].includes(track.codec ?? ''));
				selectedAudioId = (audioTracks.find(track => track.isDefault) ?? audioTracks[0])?.id ?? null;
				const capability = player?.canPlayType(nextProperties.mimeType);
				nativeSupport = capability === 'probably' ? 'Probably' : capability === 'maybe' ? 'Maybe' : 'Not advertised by this browser';
				const video = await input.getPrimaryVideoTrack();
				if (cancelled) return;
				details = 'File properties read from container metadata.';
				if (video && await video.canDecode()) {
					const sink = new m.CanvasSink(video, { width: 480 });
					const frame = await sink.getCanvas(Math.max(0, await video.getFirstTimestamp()));
					if (!cancelled && frame && thumbnail) {
						thumbnail.width = frame.canvas.width;
						thumbnail.height = frame.canvas.height;
						thumbnail.getContext('2d')?.drawImage(frame.canvas, 0, 0);
						hasThumbnail = true;
					}
				}
			} catch (error) {
				if (!cancelled) {
					if (details.startsWith('Inspecting')) details = 'Media metadata unavailable.';
					inspectionError = error instanceof Error ? error.message : 'Media inspection unavailable.';
				}
			} finally {
				clearTimeout(deadline);
				input?.dispose();
			}
		})();
		return () => {
			stopDecodedAudio();
			cancelled = true;
			clearTimeout(deadline);
			controller.abort();
			input?.dispose();
		};
	});
</script>

<section class="my-4 space-y-3 rounded-lg border border-border p-4" aria-label="Media preview">
	<div class="flex items-center justify-between gap-3">
		<h2 class="min-w-0 truncate font-medium">{name}</h2>
		{#if onclose}<button type="button" onclick={onclose} class="rounded-md border px-3 py-1 text-sm">Close preview</button>{/if}
	</div>
	<p class="text-xs text-muted-foreground">Prototype · Native playback; Mediabunny metadata and frame preview. Browser codec support varies.</p>
	{#if kind === 'video'}
		<!-- svelte-ignore a11y_media_has_caption -->
		<video bind:this={player} src={url} controls preload="metadata" playsinline onerror={() => { playbackError = true; seeking = false; }} onseeking={() => seeking = true} onseeked={() => seeking = false} class="max-h-96 w-full rounded bg-black"></video>
	{:else}
		<audio bind:this={player} src={url} controls preload="metadata" onerror={() => { playbackError = true; seeking = false; }} onseeking={() => seeking = true} onseeked={() => seeking = false} class="w-full"></audio>
	{/if}
	{#if playbackError}<p class="text-sm text-muted-foreground">This browser could not play the file, or preview access ended. You can still try downloading it.</p>{/if}
	{#if ac3Tracks.length}
		<div class="space-y-2 rounded-md border border-border p-3 text-sm">
			<label for="decoded-audio-track" class="block font-medium">Browser-decoded AC-3 / E-AC-3 audio</label>
			<select id="decoded-audio-track" bind:value={selectedAudioId} disabled={audioLoading} onchange={() => { if (decodedAudio) void enableDecodedAudio(); }} class="w-full rounded-md border bg-background p-2">
				{#each ac3Tracks as track (track.id)}<option value={track.id}>Track {track.number} · {track.language ?? 'Unknown language'} · {track.codec?.toUpperCase()}{track.name ? ` · ${track.name}` : ''}</option>{/each}
			</select>
			<button type="button" disabled={audioLoading} onclick={() => { if (decodedAudio) stopDecodedAudio(); else void enableDecodedAudio(); }} class="rounded-md border px-3 py-1">{audioLoading ? 'Loading decoder…' : decodedAudio ? 'Use native audio' : 'Enable decoded sound'}</button>
			{#if decodedAudio}
				<label class="flex items-center gap-3">Decoded volume<input type="range" min="0" max="1" step="0.01" bind:value={audioVolume} oninput={event => audioSync?.setVolume(event.currentTarget.valueAsNumber)} /></label>
			{/if}
			<p class="text-xs text-muted-foreground">WASM audio decoding uses this device, not the NAS. Native sound stays muted while decoded audio is enabled; use Decoded volume. Stereo output, no Atmos passthrough. Video still requires native browser playback.</p>
			{#if audioError}<p role="alert" class="text-sm text-destructive">{audioError}</p>{/if}
		</div>
	{/if}
	<p class="text-xs text-muted-foreground">{details}</p>
	{#if seeking}<p role="status" class="text-sm text-muted-foreground">Seeking… The browser may need to fetch an index and decode from an earlier keyframe.</p>{/if}
	<details open class="rounded-md border border-border p-3 text-sm">
		<summary class="cursor-pointer font-medium">File properties</summary>
		<dl class="mt-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1">
			<dt class="text-muted-foreground">File</dt><dd class="break-all">{name}</dd>
			<dt class="text-muted-foreground">Size</dt><dd>{fileSize != null ? formatBytes(fileSize) : 'Unavailable'}</dd>
			{#if properties}
				<dt class="text-muted-foreground">Container</dt><dd>{properties.container}</dd>
				<dt class="text-muted-foreground">MIME type</dt><dd class="break-all">{properties.mimeType}</dd>
				<dt class="text-muted-foreground">Duration</dt><dd>{properties.duration != null ? `${properties.duration.toFixed(1)} seconds` : 'Not present in metadata'}</dd>
				<dt class="text-muted-foreground">Native container support</dt><dd>{nativeSupport}</dd>
			{/if}
		</dl>
		{#if properties}
			{#each properties.tracks as track (track.id)}
				<div class="mt-3 rounded border border-border p-3">
					<h3 class="font-medium">{track.type === 'audio' ? 'Audio' : 'Video'} track {track.number}{track.isDefault ? ' · Default' : ''}</h3>
					<dl class="mt-2 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1">
						<dt class="text-muted-foreground">Codec</dt><dd class="break-all">{track.codec ?? 'Unrecognized'}{track.internalCodec ? ` (${track.internalCodec})` : ''}</dd>
						{#if track.codecParameter}<dt class="text-muted-foreground">Codec identifier</dt><dd class="break-all">{track.codecParameter}</dd>{/if}
						{#if track.name}<dt class="text-muted-foreground">Title</dt><dd class="break-all">{track.name}</dd>{/if}
						{#if track.language && track.language !== 'und'}<dt class="text-muted-foreground">Language</dt><dd>{track.language}</dd>{/if}
						{#if track.width && track.height}<dt class="text-muted-foreground">Dimensions</dt><dd>{track.width} × {track.height}</dd>{/if}
						{#if track.channels}<dt class="text-muted-foreground">Channels</dt><dd>{track.channels}</dd>{/if}
						{#if track.sampleRate}<dt class="text-muted-foreground">Sample rate</dt><dd>{track.sampleRate} Hz</dd>{/if}
						{#if track.bitrate}<dt class="text-muted-foreground">Metadata bitrate</dt><dd>{Math.round(track.bitrate / 1000)} kbps</dd>{/if}
						<dt class="text-muted-foreground">Mediabunny decoding</dt><dd>{decoderStatus(track)}</dd>
					</dl>
					{#if track.type === 'audio' && ['ac3', 'eac3'].includes(track.codec ?? '')}
						<p class="mt-2 text-xs text-muted-foreground">Native playback may show video without sound. Use Enable decoded sound to try the optional AC-3 / E-AC-3 WASM decoder.</p>
					{:else if track.type === 'audio' && track.codec === 'dts'}
						<p class="mt-2 text-xs text-muted-foreground">No optional DTS decoder is included. Native playback may show video without sound.</p>
					{/if}
				</div>
			{/each}
			{#if properties.trackCount > properties.tracks.length}<p class="mt-2 text-xs text-muted-foreground">Showing the first {properties.tracks.length} of {properties.trackCount} audio/video tracks.</p>{/if}
			{#if !properties.tracks.some(track => track.type === 'audio')}<p class="mt-2 text-xs text-muted-foreground">No audio track found in the container.</p>{/if}
			<p class="mt-3 text-xs text-muted-foreground">Container support is only a browser capability hint. Mediabunny decoding and native player support are separate; these checks do not confirm which audio track the native player selected.</p>
		{/if}
	</details>
	<canvas bind:this={thumbnail} class:hidden={!hasThumbnail} class="max-h-48 max-w-full rounded" aria-label="Mediabunny decoded first frame"></canvas>
	{#if inspectionError}<p class="text-xs text-muted-foreground">{inspectionError}</p>{/if}
</section>
