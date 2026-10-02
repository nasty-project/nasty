import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { startServer } from './server.mjs';
import { desktopDriver, webdriverDriver } from './drivers.mjs';
import { assertSteady } from './assertions.mjs';

const { values } = parseArgs({ options: {
  browser: { type: 'string', default: 'chromium' }, container: { type: 'string', default: 'mp4' },
  codecs: { type: 'string', default: 'ac3,eac3' }, rates: { type: 'string', default: '1,1.75,2' },
  seconds: { type: 'string', default: '12' }, 'delay-ms': { type: 'string', default: '0' },
  'main-thread-stall-ms': { type: 'string', default: '0' }, port: { type: 'string', default: '8787' },
  'expect-video-buffering': { type: 'boolean', default: false }, scenario: { type: 'string', default: 'steady' }
} });
if (!['chromium', 'firefox', 'stock-firefox', 'android-firefox'].includes(values.browser)) throw new Error('Unknown browser');
if (!['mp4', 'mkv'].includes(values.container)) throw new Error('container must be mp4 or mkv');
const codecs = values.codecs.split(','), rates = values.rates.split(',').map(Number);
if (codecs.some(codec => !['ac3', 'eac3'].includes(codec)) || rates.some(rate => ![1, 1.75, 2].includes(rate))) throw new Error('Unsupported codec/rate');
const seconds = Number(values.seconds), delayMs = Number(values['delay-ms']), stallMs = Number(values['main-thread-stall-ms']);
if (![seconds, delayMs, stallMs].every(Number.isFinite) || seconds < 3 || seconds > 40 || delayMs < 0 || stallMs < 0 || stallMs > 500) throw new Error('Invalid duration/delay');
if (!/^[a-z0-9-]+$/.test(values.scenario)) throw new Error('Invalid scenario name');
const results = resolve(fileURLToPath(new URL('./results/', import.meta.url)), values.browser, values.scenario);
await mkdir(results, { recursive: true });
const android = values.browser === 'android-firefox';
const server = await startServer({ port: Number(values.port), delayMs });
// adb reverse tcp:8787 tcp:8787 makes loopback work on an emulator/device too.
const origin = process.env.MEDIA_BROWSER_ORIGIN ?? `http://127.0.0.1:${server.port}`;
let driver;
let startupError;
const outcomes = [];

async function until(predicate, arg, timeout = 60000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await driver.evaluate(predicate, arg)) return;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error('Timed out waiting for media state');
}

const report = () => driver.evaluate(() => {
  const text = document.querySelector('[aria-label="Audio diagnostic report"]')?.textContent;
  const video = document.querySelector('video');
  const quality = video?.getVideoPlaybackQuality?.();
  let bufferedAhead = 0;
  for (let index = 0; video && index < video.buffered.length; index++) {
    if (video.currentTime >= video.buffered.start(index) && video.currentTime <= video.buffered.end(index)) bufferedAhead = video.buffered.end(index) - video.currentTime;
  }
  return { diagnostics: text ? JSON.parse(text) : null,
    video: video ? { time: video.currentTime, paused: video.paused, readyState: video.readyState,
      bufferedAheadSeconds: bufferedAhead, width: video.videoWidth, height: video.videoHeight,
      droppedFrames: quality?.droppedVideoFrames ?? null, totalFrames: quality?.totalVideoFrames ?? null,
      errorCode: video.error?.code ?? null } : null,
    events: window.mediaRegressionEvents ?? [], pageErrors: window.mediaRegressionErrors ?? [],
    browser: navigator.userAgent };
});

try {
  driver = android || values.browser === 'stock-firefox' ? await webdriverDriver({ android }) : await desktopDriver(values.browser);
  for (const codec of codecs) for (const rate of rates) {
    const name = `${codec}-${values.container}-${rate}x`;
    const outcome = { name, passed: false, samples: [] };
    outcomes.push(outcome);
    try {
      await driver.navigate(`${origin}/share/${codec}-${values.container}`);
      await until(() => !!document.querySelector('video') && [...document.querySelectorAll('summary')].some(node => node.textContent.includes('Audio options')));
      await driver.evaluate(() => {
        window.mediaRegressionEvents = []; window.mediaRegressionErrors = [];
        const video = document.querySelector('video');
        for (const type of ['play', 'playing', 'pause', 'waiting', 'stalled', 'seeking', 'seeked', 'ratechange', 'ended', 'error']) {
          video.addEventListener(type, () => { window.mediaRegressionEvents.push({ type, atMs: performance.now(), mediaTime: video.currentTime, readyState: video.readyState }); });
        }
        window.addEventListener('error', event => window.mediaRegressionErrors.push(event.message));
        window.addEventListener('unhandledrejection', () => window.mediaRegressionErrors.push('Unhandled rejection'));
      });
      await driver.clickText('summary', 'Audio options');
      await until(() => [...document.querySelectorAll('button')].some(node => !node.disabled && ['Enable decoded sound', 'Use native audio'].includes(node.textContent.trim())));
      const native = await driver.evaluate(() => [...document.querySelectorAll('button')].some(node => node.textContent.trim() === 'Enable decoded sound'));
      if (native) await driver.clickText('button', 'Enable decoded sound');
      await until(() => [...document.querySelectorAll('button')].some(node => !node.disabled && node.textContent.trim() === 'Use native audio'));
      await driver.clickText('summary', 'Decoded audio diagnostics');
      assert.equal(await driver.evaluate(() => document.querySelector('video').paused), true);
      await driver.click('button[aria-label="Play video"]');
      await until(() => { const pre = document.querySelector('[aria-label="Audio diagnostic report"]'); return pre && JSON.parse(pre.textContent).audio.status === 'playing' && document.querySelector('video').currentTime > 0.5; });
      await driver.evaluate(rate => {
        const select = document.querySelector('select[aria-label="Playback speed"]');
        select.value = String(rate); select.dispatchEvent(new Event('change', { bubbles: true }));
      }, rate);
      await until(rate => { const pre = document.querySelector('[aria-label="Audio diagnostic report"]'); const audio = pre && JSON.parse(pre.textContent).audio; return audio?.status === 'playing' && audio.playbackRate === rate; }, rate);
      await new Promise(resolve => setTimeout(resolve, 700));
      const baseline = await report(); outcome.samples.push(baseline);
      assert(baseline.diagnostics.audio.prebufferedAheadMs >= 500);
      const untilTime = Date.now() + seconds * 1000;
      let stalled = false;
      while (Date.now() < untilTime) {
        await new Promise(resolve => setTimeout(resolve, 500));
        if (stallMs && !stalled) {
          await driver.evaluate(ms => { const end = performance.now() + ms; while (performance.now() < end) {} }, stallMs);
          stalled = true;
        }
        const sample = await report(); outcome.samples.push(sample); assertSteady(baseline, sample, values['expect-video-buffering']);
      }
      await until(() => { const text = document.querySelector('[aria-label="Audio diagnostic report"]')?.textContent; return text && JSON.parse(text).audio.status === 'playing'; });
      const recovered = await report(); outcome.samples.push(recovered); assertSteady(baseline, recovered, values['expect-video-buffering']);
      // Pause/resume and seek use real controls and must create only deliberate periods.
      await driver.click('button[aria-label="Pause"]');
      await until(() => document.querySelector('video').paused);
      await driver.click('button[aria-label="Play"]');
      await until(() => document.querySelector('[aria-label="Audio diagnostic report"]') && JSON.parse(document.querySelector('[aria-label="Audio diagnostic report"]').textContent).audio.status === 'playing');
      await driver.evaluate(() => {
        const seek = document.querySelector('input[aria-label="Seek"]'); seek.value = '5'; seek.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await until(() => !document.querySelector('video').paused && document.querySelector('video').currentTime >= 5 && document.querySelector('video').currentTime < 8);
      outcome.samples.push(await report());
      await driver.click('button[aria-label="Mute"]');
      await driver.click('button[aria-label="Unmute"]');
      await driver.clickText('button', 'Close preview');
      await until(() => !document.querySelector('video'));
      assert.equal(driver.errors.length, 0, driver.errors.join('\n'));
      outcome.passed = true;
      console.log(`PASS ${values.browser} ${name}`);
    } catch (error) {
      outcome.error = error.message;
      try { outcome.samples.push(await report()); await driver.screenshot(resolve(results, `${name}.png`)); } catch {}
      console.error(`FAIL ${values.browser} ${name}: ${error.message}`);
    } finally { await writeFile(resolve(results, `${name}.json`), JSON.stringify(outcome, null, 2)); }
  }
} catch (error) {
  startupError = error.message;
  throw error;
} finally {
  await writeFile(resolve(results, 'summary.json'), JSON.stringify({ browser: values.browser, android,
    capabilities: driver?.capabilities, startupError, delayMs, stallMs, seconds, expectVideoBuffering: values['expect-video-buffering'], outcomes: outcomes.map(({ name, passed, error }) => ({ name, passed, error })) }, null, 2));
  try { await driver?.close(); } finally { await server.close(); }
}
if (!outcomes.length || outcomes.some(result => !result.passed)) process.exitCode = 1;
