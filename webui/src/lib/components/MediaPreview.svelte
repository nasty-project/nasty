<script lang="ts">
	import { onMount } from 'svelte';
	import { mediaPreviewKind } from '$lib/public-share';
	import { formatBytes } from '$lib/format';
	import { readMediaProperties, type MediaProperties, type MediaTrackProperties } from '$lib/media-properties';
	import { automaticDecodedTrack, MediaAudioSync, type AudioDiagnostics } from '$lib/media-audio';
	import { Play, Pause, Volume2, VolumeX, Maximize, Minimize } from '@lucide/svelte';

	let { url, name, onclose, mediaKind }: { url: string; name: string; onclose?: () => void; mediaKind?: 'video' | 'audio' } = $props();
	let details = $state('Inspecting media with Mediabunny…');
	let inspectionError = $state('');
	let playbackError = $state(false);
	let poster = $state<string>();
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
	let soundMuted = $state(false);
	let playerFrame = $state<HTMLDivElement>();
	let paused = $state(true);
	let hasPlayed = $state(false);
	let position = $state(0);
	let playerDuration = $state(0);
	let playbackRate = $state(1);
	let fullscreen = $state(false);
	const duration = $derived(playerDuration || properties?.duration || 0);
	let audioSync: MediaAudioSync | undefined;
	let audioInput: import('mediabunny').Input | undefined;
	let audioContext: AudioContext | undefined;
	let audioGeneration = 0;
	let diagnosticsOpen = $state(false);
	let audioDiagnostics = $state<AudioDiagnostics | null>(null);
	let networkDiagnostics = $state({ requests: 0, failures: 0, cancelled: 0, inFlight: 0, averageHeadersMs: 0, worstHeadersMs: 0, lastStatus: 0 });
	let diagnosticCopyMessage = $state('');
	let networkStats = { requests: 0, failures: 0, cancelled: 0, inFlight: 0, totalHeadersMs: 0, worstHeadersMs: 0, lastStatus: 0 };

	function updateDiagnostics() {
		if (audioSync) audioDiagnostics = audioSync.getDiagnostics();
		const { totalHeadersMs, ...stats } = networkStats;
		networkDiagnostics = { ...stats, averageHeadersMs: totalHeadersMs / Math.max(1, stats.requests - stats.inFlight) };
	}

	$effect(() => {
		const active = decodedAudio;
		if (!diagnosticsOpen) return;
		updateDiagnostics();
		if (!active) return;
		// Poll only while the panel is visible, not on every decoded frame.
		const timer = setInterval(updateDiagnostics, 500);
		return () => clearInterval(timer);
	});

	function diagnosticReport() {
		return JSON.stringify({ browser: navigator.userAgent, codec: ac3Tracks.find(track => track.id === selectedAudioId)?.codec, audio: audioDiagnostics, network: networkDiagnostics }, null, 2);
	}

	async function copyDiagnostics() {
		updateDiagnostics();
		try { await navigator.clipboard.writeText(diagnosticReport()); diagnosticCopyMessage = 'Copied'; }
		catch { diagnosticCopyMessage = 'Select the report below to copy it.'; }
	}

	function timeLabel(time: number) {
		const seconds = Math.max(0, Math.floor(Number.isFinite(time) ? time : 0));
		const hours = Math.floor(seconds / 3600);
		const minutes = Math.floor(seconds / 60) % 60;
		return `${hours ? `${hours}:${String(minutes).padStart(2, '0')}` : Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
	}

	async function togglePlayback() {
		if (!player || audioLoading) return;
		if (!player.paused) { player.pause(); return; }
		resumeDecodedAudio();
		try { await player.play(); } catch { playbackError = true; }
	}

	function setVolume(volume: number) {
		audioVolume = Math.max(0, Math.min(1, volume));
		if (player) player.volume = audioVolume;
		audioSync?.setVolume(soundMuted ? 0 : audioVolume);
	}

	function toggleMute() {
		soundMuted = !soundMuted;
		if (player && !decodedAudio) player.muted = soundMuted;
		audioSync?.setVolume(soundMuted ? 0 : audioVolume);
	}

	function seek(time: number) {
		if (player && duration > 0) player.currentTime = Math.max(0, Math.min(duration, time));
	}

	async function toggleFullscreen() {
		try {
			if (document.fullscreenElement === playerFrame) await document.exitFullscreen();
			else await playerFrame?.requestFullscreen();
		} catch { /* The browser may not expose the Fullscreen API. */ }
	}

	function playerKey(event: KeyboardEvent) {
		if (event.ctrlKey || event.metaKey || event.altKey) return;
		if (event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement) return;
		if (event.target instanceof HTMLButtonElement && (event.code === 'Space' || event.key === 'Enter')) return;
		if (event.code === 'Space' || event.key === 'k') { event.preventDefault(); void togglePlayback(); }
		else if (event.key === 'm') { event.preventDefault(); toggleMute(); }
		else if (event.key === 'f' && kind === 'video') { event.preventDefault(); void toggleFullscreen(); }
		else if (event.key === 'ArrowLeft') { event.preventDefault(); seek(position - 5); }
		else if (event.key === 'ArrowRight') { event.preventDefault(); seek(position + 5); }
	}

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
		updateDiagnostics();
		audioSync = undefined;
		audioInput?.dispose();
		audioInput = undefined;
		void audioContext?.close().catch(() => {});
		audioContext = undefined;
		decodedAudio = false;
		audioLoading = false;
		if (player) player.muted = soundMuted;
	}

	function resumeDecodedAudio() {
		if (!audioContext) return;
		const generation = audioGeneration;
		void audioContext.resume().catch(error => {
			if (generation === audioGeneration) audioError = error instanceof Error ? error.message : 'Audio could not start.';
		});
	}

	async function enableDecodedAudio(automatic = false) {
		if (!player || selectedAudioId == null) return;
		const wasPlaying = !player.paused;
		player.pause();
		stopDecodedAudio();
		const generation = audioGeneration;
		audioLoading = true;
		audioError = '';
		audioDiagnostics = null;
		diagnosticCopyMessage = '';
		const requests = { requests: 0, failures: 0, cancelled: 0, inFlight: 0, totalHeadersMs: 0, worstHeadersMs: 0, lastStatus: 0 };
		networkStats = requests;
		try {
			// Open/resume Web Audio in the button gesture, before fetching WASM.
			// Match the PCM rate: resampling each short source independently can
			// introduce high-frequency artifacts at its boundaries.
			const sampleRate = ac3Tracks.find(track => track.id === selectedAudioId)?.sampleRate;
			audioContext = new AudioContext(sampleRate ? { sampleRate } : undefined);
			// Automatic preparation must not wait indefinitely for an autoplay
			// grant. Native Play/control gestures resume the prepared context.
			if (automatic) resumeDecodedAudio();
			else await audioContext.resume();
			const [{ registerAc3Decoder }, m] = await Promise.all([import('@mediabunny/ac3'), import('$lib/media-readers')]);
			if (generation !== audioGeneration) return;
			registerAc3Decoder();
			wasmDecoderLoaded = true;
			audioInput = new m.Input({
				formats: [m.MP4, m.QTFF, m.MATROSKA, m.WEBM],
				source: new m.UrlSource(url, {
					maxCacheSize: 4 * 1024 * 1024,
					getRetryDelay: () => null,
					fetchFn: async (request, init) => {
						const before = performance.now();
						requests.requests++; requests.inFlight++;
						try {
							const response = await fetch(request, {
								...init, redirect: 'error',
								signal: AbortSignal.any([...(init?.signal ? [init.signal] : []), AbortSignal.timeout(15000)])
							});
							requests.lastStatus = response.status;
							if (!response.ok) requests.failures++;
							return response;
						} catch (error) {
							if (error instanceof DOMException && error.name === 'AbortError') requests.cancelled++;
							else requests.failures++;
							throw error;
						} finally {
							const elapsed = performance.now() - before;
							requests.inFlight--; requests.totalHeadersMs += elapsed;
							requests.worstHeadersMs = Math.max(requests.worstHeadersMs, elapsed);
						}
					}
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
			audioSync.setVolume(soundMuted ? 0 : audioVolume);
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
		// Resume while a control gesture is active, including native controls.
		const mountedPlayer = player;
		const mountedFrame = playerFrame;
		mountedFrame?.addEventListener('keydown', playerKey);
		const updatePlayer = () => {
			if (!mountedPlayer) return;
			paused = mountedPlayer.paused;
			position = mountedPlayer.currentTime;
			playerDuration = Number.isFinite(mountedPlayer.duration) ? mountedPlayer.duration : 0;
			playbackRate = mountedPlayer.playbackRate;
			if (!paused) hasPlayed = true;
		};
		const updateVolume = () => {
			if (!mountedPlayer) return;
			audioVolume = mountedPlayer.volume;
			if (!decodedAudio) soundMuted = mountedPlayer.muted;
			audioSync?.setVolume(soundMuted ? 0 : audioVolume);
		};
		const updateFullscreen = () => { fullscreen = document.fullscreenElement === playerFrame; };
		const playbackEvents = ['loadedmetadata', 'durationchange', 'timeupdate', 'play', 'pause', 'ended', 'ratechange'];
		for (const event of playbackEvents) mountedPlayer?.addEventListener(event, updatePlayer);
		mountedPlayer?.addEventListener('volumechange', updateVolume);
		document.addEventListener('fullscreenchange', updateFullscreen);
		updatePlayer();
		updateVolume();
		mountedPlayer?.addEventListener('pointerdown', resumeDecodedAudio, true);
		mountedPlayer?.addEventListener('keydown', resumeDecodedAudio, true);
		mountedPlayer?.addEventListener('play', resumeDecodedAudio);
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
				if (player) {
					const automaticTrack = automaticDecodedTrack(nextProperties.tracks, type => player!.canPlayType(type));
					if (automaticTrack != null) {
						selectedAudioId = automaticTrack;
						void enableDecodedAudio(true);
					}
				}
				const video = await input.getPrimaryVideoTrack();
				if (cancelled) return;
				details = 'File properties read from container metadata.';
				if (video && await video.canDecode()) {
					const sink = new m.CanvasSink(video, { width: 480 });
					const frame = await sink.getCanvas(Math.max(0, await video.getFirstTimestamp()));
					if (!cancelled && frame) {
						const canvas = document.createElement('canvas');
						canvas.width = frame.canvas.width;
						canvas.height = frame.canvas.height;
						canvas.getContext('2d')?.drawImage(frame.canvas, 0, 0);
						poster = canvas.toDataURL('image/jpeg', 0.8);
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
			mountedFrame?.removeEventListener('keydown', playerKey);
			mountedPlayer?.pause();
			for (const event of playbackEvents) mountedPlayer?.removeEventListener(event, updatePlayer);
			mountedPlayer?.removeEventListener('volumechange', updateVolume);
			document.removeEventListener('fullscreenchange', updateFullscreen);
			mountedPlayer?.removeEventListener('pointerdown', resumeDecodedAudio, true);
			mountedPlayer?.removeEventListener('keydown', resumeDecodedAudio, true);
			mountedPlayer?.removeEventListener('play', resumeDecodedAudio);
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
	<div bind:this={playerFrame} class="media-player overflow-hidden rounded bg-black text-white" role="group" aria-label="Media player">
		{#if kind === 'video'}
			<div class="media-picture relative">
				<!-- svelte-ignore a11y_media_has_caption -->
				<video bind:this={player} src={url} {poster} preload="metadata" playsinline onerror={() => { playbackError = true; seeking = false; }} onseeking={() => seeking = true} onseeked={() => seeking = false} class="aspect-video max-h-96 w-full object-contain"></video>
				<button type="button" aria-label={!hasPlayed ? 'Play video' : paused ? 'Resume video' : 'Pause video'} disabled={audioLoading} onclick={() => void togglePlayback()} class="absolute inset-0 flex items-center justify-center disabled:opacity-60">
					{#if paused}<span class="flex size-16 items-center justify-center rounded-full bg-black/70 shadow-lg"><Play size={32} fill="currentColor" /></span>{/if}
				</button>
			</div>
		{:else}
			<audio bind:this={player} src={url} preload="metadata" onerror={() => { playbackError = true; seeking = false; }} onseeking={() => seeking = true} onseeked={() => seeking = false}></audio>
		{/if}
		{#if hasPlayed || kind === 'audio'}
			<div class="flex shrink-0 items-center gap-2 bg-black/90 px-3 py-2 text-xs sm:gap-3" aria-label="Playback controls">
				<button type="button" aria-label={paused ? 'Play' : 'Pause'} disabled={audioLoading} onclick={() => void togglePlayback()} class="shrink-0 rounded p-1 hover:bg-white/20">{#if paused}<Play size={20} />{:else}<Pause size={20} />{/if}</button>
				<input type="range" aria-label="Seek" min="0" max={Math.max(duration, 1)} step="0.1" value={position} disabled={!duration} oninput={event => seek(event.currentTarget.valueAsNumber)} class="min-w-0 flex-1 accent-sky-400" />
				<span class="shrink-0 tabular-nums">{timeLabel(position)}<span class="hidden sm:inline"> / {timeLabel(duration)}</span></span>
				<select aria-label="Playback speed" value={playbackRate} onchange={event => { if (player) player.playbackRate = Number(event.currentTarget.value); }} class="w-12 shrink-0 rounded bg-black text-white">
					{#each [0.25, 0.5, 0.75, 1, 1.25, 1.5, 1.75, 2] as rate}<option value={rate}>{rate}×</option>{/each}
				</select>
				<button type="button" aria-label={soundMuted ? 'Unmute' : 'Mute'} onclick={toggleMute} class="shrink-0 rounded p-1 hover:bg-white/20">{#if soundMuted || audioVolume === 0}<VolumeX size={20} />{:else}<Volume2 size={20} />{/if}</button>
				<input type="range" aria-label="Volume" min="0" max="1" step="0.01" value={audioVolume} oninput={event => setVolume(event.currentTarget.valueAsNumber)} class="w-12 shrink-0 accent-sky-400 sm:w-20" />
				{#if kind === 'video'}<button type="button" aria-label={fullscreen ? 'Exit fullscreen' : 'Enter fullscreen'} onclick={() => void toggleFullscreen()} class="shrink-0 rounded p-1 hover:bg-white/20">{#if fullscreen}<Minimize size={20} />{:else}<Maximize size={20} />{/if}</button>{/if}
			</div>
		{/if}
	</div>
	{#if playbackError}<p class="text-sm text-muted-foreground">This browser could not play the file, or preview access ended. You can still try downloading it.</p>{/if}
	{#if ac3Tracks.length}
		<details class="space-y-2 rounded-md border border-border p-3 text-sm">
			<summary class="cursor-pointer font-medium">Audio options · {audioLoading ? 'Loading decoder…' : decodedAudio ? 'Decoded audio' : 'Native audio'}</summary>
			<label for="decoded-audio-track" class="block font-medium">Decoded audio track</label>
			<select id="decoded-audio-track" bind:value={selectedAudioId} disabled={audioLoading} onchange={() => { if (decodedAudio) void enableDecodedAudio(); }} class="w-full rounded-md border bg-background p-2">
				{#each ac3Tracks as track (track.id)}<option value={track.id}>Track {track.number} · {track.language ?? 'Unknown language'} · {track.codec?.toUpperCase()}{track.name ? ` · ${track.name}` : ''}</option>{/each}
			</select>
			<button type="button" disabled={audioLoading} onclick={() => { if (decodedAudio) stopDecodedAudio(); else void enableDecodedAudio(); }} class="rounded-md border px-3 py-1">{audioLoading ? 'Loading decoder…' : decodedAudio ? 'Use native audio' : 'Enable decoded sound'}</button>
			<p class="text-xs text-muted-foreground">AC-3 / E-AC-3 decoding runs on this device. Player mute and volume control either audio mode, including fullscreen. Stereo output, no Atmos passthrough. Video still requires native browser playback.</p>
		</details>
	{/if}
	{#if audioError}<p role="alert" class="text-sm text-destructive">{audioError}</p>{/if}
	{#if ac3Tracks.length}
		<details class="rounded-md border border-border p-3 text-sm" ontoggle={event => diagnosticsOpen = event.currentTarget.open}>
			<summary class="cursor-pointer font-medium">Decoded audio diagnostics</summary>
			{#if audioDiagnostics}
				<dl class="mt-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-xs">
					<dt>Status / context</dt><dd>{audioDiagnostics.status} / {audioDiagnostics.contextState}</dd>
					<dt>PCM / context rate</dt><dd>{audioDiagnostics.pcmSampleRate} / {audioDiagnostics.contextSampleRate} Hz · {audioDiagnostics.channels} channels</dd>
					<dt>PCM batch target / average</dt><dd>{audioDiagnostics.targetBatchMs.toFixed(0)} / {audioDiagnostics.averageBatchMs.toFixed(1)} ms</dd>
					<dt>Audio queued ahead</dt><dd>{audioDiagnostics.queuedAheadMs.toFixed(1)} ms · {audioDiagnostics.activeNodes} nodes</dd>
					<dt>Decoded frames / scheduled batches</dt><dd>{audioDiagnostics.decodedBuffers} / {audioDiagnostics.scheduledBatches}</dd>
					<dt>Late batches (&gt;5 ms) / worst</dt><dd>{audioDiagnostics.lateBatches} / {audioDiagnostics.worstLateMs.toFixed(1)} ms</dd>
					<dt>Trimmed late PCM / dropped batches</dt><dd>{audioDiagnostics.trimmedLateMs.toFixed(1)} ms / {audioDiagnostics.droppedBatches}</dd>
					<dt>Possible queue gaps / total</dt><dd>{audioDiagnostics.possibleQueueGaps} / {audioDiagnostics.queueGapMs.toFixed(1)} ms</dd>
					<dt>Source discontinuities</dt><dd>{audioDiagnostics.sourceDiscontinuities}</dd>
					<dt>Read/decode wait average / worst</dt><dd>{audioDiagnostics.averageReadDecodeWaitMs.toFixed(1)} / {audioDiagnostics.worstReadDecodeWaitMs.toFixed(1)} ms</dd>
					<dt>Base / output latency</dt><dd>{audioDiagnostics.baseLatencyMs.toFixed(1)} / {audioDiagnostics.outputLatencyMs?.toFixed(1) ?? 'Unknown'} ms</dd>
					<dt>HTTP requests / failures / cancelled</dt><dd>{networkDiagnostics.requests} / {networkDiagnostics.failures} / {networkDiagnostics.cancelled}</dd>
					<dt>HTTP headers average / worst</dt><dd>{networkDiagnostics.averageHeadersMs.toFixed(1)} / {networkDiagnostics.worstHeadersMs.toFixed(1)} ms</dd>
				</dl>
				<p class="mt-3 text-xs text-muted-foreground">Counters cover this decoded session, including startup and seeks. Queue gaps are scheduling estimates, not measured speaker underruns. Read/decode wait includes demuxing, network body reads and WASM decoding; HTTP timing measures headers only.</p>
				<button type="button" onclick={() => void copyDiagnostics()} class="mt-3 rounded-md border px-3 py-1 text-xs">Copy diagnostics</button>
				{#if diagnosticCopyMessage}<p role="status" class="mt-1 text-xs">{diagnosticCopyMessage}</p>{/if}
				<pre class="mt-2 max-h-48 overflow-auto whitespace-pre-wrap break-all text-xs" aria-label="Audio diagnostic report">{diagnosticReport()}</pre>
			{:else}<p class="mt-3 text-xs text-muted-foreground">Enable decoded sound in Audio options to collect diagnostics.</p>{/if}
		</details>
	{/if}
	<p class="text-xs text-muted-foreground">{details}</p>
	{#if seeking}<p role="status" class="text-sm text-muted-foreground">Seeking… The browser may need to fetch an index and decode from an earlier keyframe.</p>{/if}
	<details class="rounded-md border border-border p-3 text-sm">
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
	{#if inspectionError}<p class="text-xs text-muted-foreground">{inspectionError}</p>{/if}
</section>

<style>
	.media-player:fullscreen { display: flex; flex-direction: column; width: 100%; height: 100%; border-radius: 0; }
	.media-player:fullscreen .media-picture { flex: 1; min-height: 0; }
	.media-player:fullscreen video { width: 100%; height: 100%; max-height: none; }
</style>
