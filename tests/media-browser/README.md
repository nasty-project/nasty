# Automated media playback checks

Runs the **built production guest player**, its actual Mediabunny WASM decoder,
and native video playback against generated H.264 + six-channel AC-3/E-AC-3
fixtures. The local server supplies synthetic unlocked-share metadata and byte
ranges under the deployed CSP; it is not an authenticated Rust-engine test.

The suite covers 1×/1.75×/2× steady playback, bounded queued audio, unexpected
decoder starts, late/dropped batches, queue gaps, pause/resume, seek, mute and
preview close. JSON artifacts include sampled diagnostics, native video events
(`waiting`, `stalled`, `pause`, `seeking`, etc.), native buffer ahead, dropped
video frames and browser version. Failures also save screenshots.

## Desktop

From the repository root, with Node 22+ and FFmpeg installed:

```sh
npm ci --prefix webui --legacy-peer-deps
npm run build --prefix webui
npm ci --prefix tests/media-browser
npm --prefix tests/media-browser run test:unit
npm exec --prefix tests/media-browser -- playwright-core install chromium firefox
npm --prefix tests/media-browser run fixtures
npm --prefix tests/media-browser test -- --browser=chromium --container=mp4
npm --prefix tests/media-browser test -- --browser=firefox --container=mkv
```

Use `--main-thread-stall-ms=350` for a main-thread pause. For controlled network
latency use `--scenario=network-jitter --delay-ms=150 --expect-video-buffering`:
this permits only decoder restarts associated with observed native `waiting`
events and requires playback to recover. The default steady scenario rejects
unexpected restarts. `--seconds=30` makes each steady sample longer;
fixtures default to 90 seconds. Use `CHROME_BIN` for an installed Chromium binary
or `HEADED=1` for a visible desktop browser. Fixtures and results stay in this
directory's ignored `artifacts/` and `results/` directories.

## Real Android Firefox

Desktop mobile emulation does **not** run GeckoView or Android's media/audio
backend. This runner uses Mozilla's supported Android geckodriver capabilities:

<https://developer.mozilla.org/en-US/docs/Web/WebDriver/Reference/Capabilities/firefoxOptions#android>

Use an Android emulator or a dedicated test device with USB debugging, `adb`,
Firefox installed, and geckodriver 0.37.1+ on the host. Complete Firefox's first-run
screens if necessary. geckodriver starts a temporary automation profile and may
restart the Firefox app, so use a test device rather than a friend's daily browser.

```sh
adb devices
adb -s emulator-5554 shell am set-debug-app --persistent org.mozilla.firefox
adb -s emulator-5554 reverse tcp:8787 tcp:8787
geckodriver --host 127.0.0.1 --port 4444 --android-storage=sdcard
```

In another terminal, from the repository root:

```sh
ANDROID_SERIAL=emulator-5554 npm --prefix tests/media-browser run test:android -- --container=mp4 --seconds=15
```

After testing:

```sh
adb -s emulator-5554 reverse --remove tcp:8787
adb -s emulator-5554 shell am clear-debug-app
```

`WEBDRIVER_URL` selects another geckodriver endpoint; `ANDROID_FIREFOX_PACKAGE`
selects beta/nightly. `MEDIA_BROWSER_ORIGIN` may override the loopback URL when
using a remote device. Always use fixtures/local test infrastructure, not a live
share token. `--browser=stock-firefox` uses the same WebDriver adapter on desktop.

## CI and coverage

The Media browser regression workflow runs desktop Chromium/Firefox for media
changes. Its manual Android job boots an Android 16/API 36 emulator and installs
an official Firefox APK (version selectable, default 152.0). Artifacts are saved
even on failure. The Android path needs verification on that runner; local SDK,
Java, emulator, and `adb` availability are prerequisites for local execution.

Tests deliberately do not fake native codec support, Web Audio clocks or video
buffering events. Unsupported native containers or broken browser startup fail
rather than being marked as passed. Chromium CI uses MP4 and desktop Firefox CI
uses MKV; Android uses MP4, matching the reported shared videos. A local Chromium
AC-3/MKV 2× run reproduced native `waiting` and an extra decoder start around a
range refill, even without injected network delay. The strict runner reports
that as a failure; use `--container=mkv` to investigate that additional case.

Emulator timing/GPU/audio differs from physical hardware. Zero late/gap counters
does not prove the speakers rendered glitch-free audio; these checks detect
delivery, scheduling and lifecycle regressions. A short physical-device check
can still be needed for a final fix, but repeated basic testing need not fall on
friends.

For an optional 4K/60 fixture stress run, generate fixtures with
`MEDIA_VIDEO_SOURCE='testsrc2=size=3840x2160:rate=60' MEDIA_H264_LEVEL=5.2`.
This is substantially heavier to encode/decode than the default 360p/30 fixture.
