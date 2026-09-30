# Shared media preview prototype

Related: #558. Audio/video Preview buttons appear for an unlocked guest share
without a download limit. Supported filename types: MP4/M4V, MOV, WebM, MKV,
MP3, M4A, WAV, Ogg/OGA, FLAC and AAC.

The authenticated file-manager audio/video modal uses the same `MediaPreview`
component. Its `/api/files/content` endpoint also supports single byte ranges
and HEAD requests; existing authentication and path restrictions still apply.

The native browser player streams and seeks through a dedicated single-range
endpoint. Mediabunny is loaded on demand to read duration/codec metadata and,
where WebCodecs is available, decode a first-frame thumbnail. Media inspection
has a 15-second timeout, a 16 MiB total read budget, and a 4 MiB source cache.

The File properties panel shows file size/container/duration and up to 16
audio/video tracks, including codec and stored codec IDs, dimensions, channels,
sample rate, language/title, default-track metadata and metadata bitrate where
available. No full-file frame-rate or bitrate scan is performed. Native player
capability hints and Mediabunny decoder availability are reported separately;
neither proves which audio track the native player will select. AC-3/E-AC-3/DTS
audio can account for a video playing without sound, but these properties alone
cannot diagnose every silent-file case.

## Browser-decoded AC-3 / E-AC-3 audio

When inspection finds AC-3 or E-AC-3 tracks, select a track and click **Enable
decoded sound**. The lazily loaded `@mediabunny/ac3` extension decodes that track
using FFmpeg WASM in the browser. Native video remains the playback clock;
decoded audio follows pause, buffering, seek and playback-rate changes. Native
audio stays muted during decoded playback. Use **Decoded volume** to adjust or
silence the decoded soundtrack, and **Use native audio** to restore native sound.
Switching audio tracks briefly pauses playback while the new decoder opens.

Audio scheduling anchors the video and Web Audio clocks once on playback/resume
and seek/rate changes, then keeps decoded samples on a continuous Web Audio
timeline. Decode-window boundaries split a buffer without replaying samples.
The audio context requests the track's sample rate to avoid independently
resampling each short buffer. Quad and 5.1 stereo downmixes normalize the speaker
coefficient sums to reserve headroom; surround tracks may therefore sound quieter
at the same decoded-volume setting. Mono/stereo volume is not attenuated.
Properties distinguish an optional WASM decoder not yet loaded, a loaded decoder,
and the currently active WASM audio track; the initial capability probe alone
does not describe playback after the extension is enabled.

Decoding uses the client's CPU, not the NAS. Output is downmixed to stereo;
Atmos passthrough and DTS decoding are not supported. Native browser video and
container support are still required. Audio reads use a separate 4 MiB cache,
15-second request timeouts and bounded two-second decode windows; the metadata
inspection read budget does not apply to ongoing playback.

The application CSP permits WASM compilation (`script-src 'wasm-unsafe-eval'`)
and decoder workers (`worker-src 'self' blob:`). Deploy the WebUI and updated
Caddy configuration together. The separate sandbox policy on file responses
remains in effect.

`@mediabunny/server` is a different extension: it uses NodeAV/native FFmpeg in a
Node/Bun/Deno process, with broader codec support and optional hardware
acceleration. It is not used by this prototype; adding it would require a media
service alongside the Rust engine and its own deployment/resource management.

This is not a custom MKV player or a universal codec/transcoding solution.
An MKV can have readable metadata and a thumbnail but still fail native
playback. Download remains available. Images and text previews are not included.

## Access and accounting

- Every preview request validates the share token, expiry/revocation, and the
  password grant. It opens the file through the same retained-descriptor share
  boundary as downloads and retains a concurrency slot until the stream ends.
- Uncapped previews do not increment download counters. Capped shares reject
  the media endpoint entirely, even if the caller constructs its URL manually.
  Preview session/quota accounting is intentionally not implemented here.
- Only audio/video media types are served. HTML, SVG, text and playlists are
  rejected. Responses remain attachments with nosniff, no-store, and a sandbox
  CSP. Playlists cannot make Mediabunny fetch additional user-controlled URLs.
- Revocation prevents subsequent requests; bytes already delivered or buffered
  by a browser cannot be recalled.

## Manual acceptance checks

1. Share a folder containing a small MP4, MP3/WAV and an MKV. Use an uncapped
   share; open it in an incognito window and navigate into a nested folder.
2. Preview each file. Check audio/video controls, seek near the end of the MP4,
   and confirm media requests return 206 with correct Content-Range values.
3. Check metadata/thumbnail behavior and the fallback message for an unsupported
   native codec/container. Close/switch previews while inspection is pending.
4. Repeat with a password-protected share: preview must not work before unlock.
   Revoke/expire the share and verify new HEAD/range requests stop working.
5. Use a share with a download limit: no Preview button should appear, and a
   manually constructed media URL must return unavailable without serving data.
6. Try invalid roots, path traversal, HTML/SVG and multipart/invalid ranges.
   Confirm ordinary single-file downloads and ZIPs retain their previous behavior.
7. Preview an H.264 file with AC-3/E-AC-3 audio. Enable decoded sound; check
   lip-sync, pause/resume, repeated seeks, buffering and playback-rate changes.
   Switch tracks, change decoded volume, restore native sound, and close the
   preview during decoder startup. Repeat in Chrome and Firefox, including the
   authenticated file-manager modal.

The prototype is covered by range/parser and descriptor-boundary unit tests,
a generated PCM metadata test using Mediabunny, audio-clock/cancellation tests,
WebUI tests and build checks. A local production-build Chrome check with a
generated H.264/six-channel AC-3 and E-AC-3 MKVs verified nonzero decoded buffers,
playback across decode windows, pause, seek/resume, volume and playback-rate
changes, track switching and native-mute restoration under the application CSP.
Local Chrome and Firefox checks also measured continuous adjacent audio-buffer
scheduling through decode-window boundaries after fixing per-buffer clock jitter.
An 8 kHz six-channel E-AC-3 stress sample was compared with a native FFmpeg PCM
reference and rendered through offline Web Audio graphs in both browsers. The
matched-rate, normalized stereo graph produced no samples above full scale;
the unnormalized downmix clipped, and Chrome's per-buffer 48→44.1 kHz resampling
introduced additional boundary artifacts. These synthetic checks do not replace
listening to real material through the deployed player.
Long-running lip-sync and the deployed HTTP path still need the
manual acceptance checks above.
