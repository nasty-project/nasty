<script lang="ts">
	import { onMount } from 'svelte';
	import { mediaPreviewKind } from '$lib/public-share';
	import { formatBytes } from '$lib/format';
	import { readMediaProperties, type MediaProperties } from '$lib/media-properties';

	let { url, name, onclose }: { url: string; name: string; onclose: () => void } = $props();
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
	const kind = $derived(mediaPreviewKind(name));

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
		<button type="button" onclick={onclose} class="rounded-md border px-3 py-1 text-sm">Close preview</button>
	</div>
	<p class="text-xs text-muted-foreground">Prototype · Native playback; Mediabunny metadata and frame preview. Browser codec support varies.</p>
	{#if kind === 'video'}
		<!-- svelte-ignore a11y_media_has_caption -->
		<video bind:this={player} src={url} controls preload="metadata" playsinline onerror={() => { playbackError = true; seeking = false; }} onseeking={() => seeking = true} onseeked={() => seeking = false} class="max-h-96 w-full rounded bg-black"></video>
	{:else}
		<audio bind:this={player} src={url} controls preload="metadata" onerror={() => { playbackError = true; seeking = false; }} onseeking={() => seeking = true} onseeked={() => seeking = false} class="w-full"></audio>
	{/if}
	{#if playbackError}<p class="text-sm text-muted-foreground">This browser could not play the file, or preview access ended. You can still try downloading it.</p>{/if}
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
						<dt class="text-muted-foreground">Mediabunny decoding</dt><dd>{track.canDecode == null ? 'Unknown' : track.canDecode ? 'Available' : 'Unavailable in this browser'}</dd>
					</dl>
					{#if track.type === 'audio' && ['ac3', 'eac3', 'dts'].includes(track.codec ?? '')}
						<p class="mt-2 text-xs text-muted-foreground">{track.codec?.toUpperCase()} audio often needs an additional decoder. The native player may show video without sound.</p>
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
