import { test } from 'node:test';
import assert from 'node:assert/strict';
import { byteRange } from './server.mjs';
import { assertSteady } from './assertions.mjs';
import http from 'node:http';
import { webdriverDriver } from './drivers.mjs';

test('range fixture server clips valid ranges and rejects malformed/out-of-bounds ranges', () => {
  assert.deepEqual(byteRange(undefined, 100), [0, 99]);
  assert.deepEqual(byteRange('bytes=20-', 100), [20, 99]);
  assert.deepEqual(byteRange('bytes=5-500', 100), [5, 99]);
  for (const header of ['bytes=100-', 'bytes=10-5', 'bytes=0-2,5-8', 'wat', 'bytes=99999999999999999-']) assert.equal(byteRange(header, 100), null);
});

test('network recovery never excuses unexplained decoder restarts or audio lateness', () => {
  const baseline = { video: { time: 1, errorCode: null }, pageErrors: [], events: [], diagnostics: { audio: {
    status: 'playing', decoderStarts: 1, lateBatches: 0, droppedBatches: 0, possibleQueueGaps: 0,
    sourceDiscontinuities: 0, lookAheadTargetMs: 1000, queuedAheadMs: 1000
  } } };
  const recovered = structuredClone(baseline); recovered.video.time = 2; recovered.diagnostics.audio.decoderStarts = 2;
  assert.throws(() => assertSteady(baseline, recovered, true), /restarted/);
  recovered.events.push({ type: 'waiting', readyState: 2 });
  assert.doesNotThrow(() => assertSteady(baseline, recovered, true));
  assert.throws(() => assertSteady(baseline, recovered, false), /restarted/);
  recovered.diagnostics.audio.lateBatches = 1;
  assert.throws(() => assertSteady(baseline, recovered, true), /lateBatches/);
  recovered.diagnostics.audio.status = 'buffering'; recovered.pageErrors.push('boom');
  assert.throws(() => assertSteady(baseline, recovered, true), /Page error/);
});

test('Android runner uses real Android Firefox capabilities and disposes the WebDriver session', async () => {
  const requests = [];
  const server = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    requests.push({ path: req.url, method: req.method, body: body ? JSON.parse(body) : null });
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ value: req.url === '/session' ? { sessionId: 'test-session', capabilities: { platformName: 'android' } } : req.url === '/status' ? { ready: true } : null }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const oldUrl = process.env.WEBDRIVER_URL, oldSerial = process.env.ANDROID_SERIAL;
  process.env.WEBDRIVER_URL = `http://127.0.0.1:${server.address().port}`;
  process.env.ANDROID_SERIAL = 'emulator-5554';
  try {
    const driver = await webdriverDriver({ android: true });
    await driver.close();
    const caps = requests.find(request => request.path === '/session').body.capabilities.alwaysMatch;
    assert.equal(caps.browserName, 'firefox');
    assert.equal(caps['moz:firefoxOptions'].androidPackage, 'org.mozilla.firefox');
    assert.equal(caps['moz:firefoxOptions'].androidDeviceSerial, 'emulator-5554');
    assert.equal(caps['moz:firefoxOptions'].args, undefined);
    assert(requests.some(request => request.path === '/session/test-session' && request.method === 'DELETE'));
  } finally {
    if (oldUrl === undefined) delete process.env.WEBDRIVER_URL; else process.env.WEBDRIVER_URL = oldUrl;
    if (oldSerial === undefined) delete process.env.ANDROID_SERIAL; else process.env.ANDROID_SERIAL = oldSerial;
    await new Promise(resolve => server.close(resolve));
  }
});
