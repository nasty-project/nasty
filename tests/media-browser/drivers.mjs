import { chromium, firefox } from 'playwright-core';

export async function desktopDriver(browserName) {
  const browser = await (browserName === 'firefox' ? firefox : chromium).launch({ headless: process.env.HEADED !== '1',
    ...(browserName === 'chromium' && process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {}) });
  const page = await browser.newPage({ viewport: { width: 960, height: 720 } });
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  return {
    errors,
    navigate: url => page.goto(url, { waitUntil: 'domcontentloaded' }),
    evaluate: (fn, arg) => page.evaluate(fn, arg),
    click: selector => page.locator(selector).click(),
    clickText: (tag, text) => page.locator(tag).filter({ hasText: text }).click(),
    screenshot: file => page.screenshot({ path: file, fullPage: true }),
    close: () => browser.close()
  };
}

// W3C WebDriver speaks to stock Firefox, including GeckoView on Android.
// Playwright's desktop Firefox binary is not used for Android sessions.
export async function webdriverDriver({ android = false } = {}) {
  const endpoint = process.env.WEBDRIVER_URL ?? 'http://127.0.0.1:4444';
  const deadline = Date.now() + 30000;
  let ready = false;
  while (Date.now() < deadline) {
    try { const response = await fetch(`${endpoint}/status`, { signal: AbortSignal.timeout(1000) }); if (response.ok) { ready = true; break; } } catch {}
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  if (!ready) throw new Error('geckodriver is not reachable; start it on WEBDRIVER_URL before running this suite');
  async function command(path, body, method = body === undefined ? 'GET' : 'POST') {
    const response = await fetch(`${endpoint}${path}`, { method,
      ...(body === undefined ? {} : { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }),
      signal: AbortSignal.timeout(120000) });
    const { value } = await response.json();
    if (!response.ok || value?.error) throw new Error(`WebDriver ${path}: ${value?.message ?? response.status}`);
    return value;
  }
  const options = android ? {
    androidPackage: process.env.ANDROID_FIREFOX_PACKAGE ?? 'org.mozilla.firefox',
    ...(process.env.ANDROID_SERIAL ? { androidDeviceSerial: process.env.ANDROID_SERIAL } : {})
  } : { args: process.env.HEADED === '1' ? [] : ['-headless'],
    ...(process.env.FIREFOX_BIN ? { binary: process.env.FIREFOX_BIN } : {}) };
  const { sessionId, capabilities } = await command('/session', { capabilities: { alwaysMatch: {
    browserName: 'firefox', pageLoadStrategy: 'eager', 'moz:firefoxOptions': options
  } } });
  const root = `/session/${sessionId}`;
  const evaluate = (fn, arg) => command(`${root}/execute/sync`, { script: `return (${fn.toString()})(arguments[0]);`, args: [arg ?? null] });
  async function click(using, value) {
    const element = await command(`${root}/element`, { using, value });
    await command(`${root}/element/${element['element-6066-11e4-a52e-4f735466cecf']}/click`, {});
  }
  await command(`${root}/timeouts`, { implicit: 0, pageLoad: 60000, script: 30000 });
  return {
    errors: [], capabilities,
    navigate: url => command(`${root}/url`, { url }), evaluate,
    click: selector => click('css selector', selector),
    clickText: (tag, text) => click('xpath', `//${tag}[contains(normalize-space(.), '${text}')]`),
    screenshot: async file => {
      const { writeFile } = await import('node:fs/promises');
      await writeFile(file, Buffer.from(await command(`${root}/screenshot`), 'base64'));
    },
    close: () => command(root, undefined, 'DELETE')
  };
}
