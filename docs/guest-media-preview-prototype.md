# Guest media preview prototype

Related: #558. Audio/video Preview buttons appear for an unlocked guest share
without a download limit. Supported filename types: MP4/M4V, MOV, WebM, MKV,
MP3, M4A, WAV, Ogg/OGA, FLAC and AAC.

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

The prototype is covered by range/parser and descriptor-boundary unit tests,
a generated PCM metadata test using Mediabunny, WebUI tests and build checks.
Real browser codec behavior and the deployed HTTP path still need the manual
acceptance checks above.
