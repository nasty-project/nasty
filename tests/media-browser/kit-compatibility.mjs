// Production-bundle qualification with synthetic, loopback-only backend data.
// This tests client behavior, not real engine authorization or storage operations.
import assert from 'node:assert/strict';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { chromium, firefox } from 'playwright-core';
import { startServer } from './server.mjs';

const { values } = parseArgs({ options: {
  browser: { type: 'string', default: 'chromium' }, webui: { type: 'string' },
  report: { type: 'string' },
} });
assert(['chromium', 'firefox'].includes(values.browser));
const root = resolve(values.webui ?? fileURLToPath(new URL('../../webui/', import.meta.url)));
// Use the checkout's own Vite instance: Kit 3's builder hooks are instance-local.
const { build, createBuilder } = await import(pathToFileURL(`${root}/node_modules/vite/dist/node/index.js`).href);
const kitVersion = JSON.parse(await readFile(`${root}/node_modules/@sveltejs/kit/package.json`, 'utf8')).version;
const report = values.report ?? (values.webui ? 'baseline' : 'kit');
assert(/^[a-z0-9-]+$/.test(report));
const results = fileURLToPath(new URL(`./results/${report}-${values.browser}/`, import.meta.url));
await mkdir(results, { recursive: true });
const modules = new Set();
const previousDirectory = process.cwd();
process.chdir(root); // Kit 2's configuration loader uses cwd, unlike Kit 3.
const buildConfig = { root, plugins: [{ name: 'qualify-runed-imports', generateBundle(_options, bundle) {
  for (const chunk of Object.values(bundle)) {
    if (chunk.type === 'chunk') for (const id of Object.keys(chunk.modules)) modules.add(id);
  }
} }] };
// Kit 2 orchestrates its own builds in legacy hooks; Kit 3 uses buildApp.
if (Number(kitVersion.split('.')[0]) >= 3) await (await createBuilder(buildConfig)).buildApp();
else await build(buildConfig);
process.chdir(previousDirectory);
const runedModules = [...modules].filter(id => id.includes('/runed/'));
assert(runedModules.length > 0, 'qualification must actually include runed');
assert(!runedModules.some(id => /\/runed\/(?:dist|src)\/kit\/|use-search-params/.test(id)),
  'Kit-specific runed runtime unexpectedly entered the production module graph');
await writeFile(`${results}/runed-modules.json`, JSON.stringify(runedModules.sort(), null, 2));

const html = await readFile(`${root}/build/index.html`, 'utf8');
const server = await startServer({ buildDir: `${root}/build` });
const origin = `http://127.0.0.1:${server.port}`;
let browser;
try {
  browser = await ({ chromium, firefox }[values.browser]).launch({ headless: true });
} catch (error) {
  await server.close();
  throw error;
}
const outcomes = [];
const status = { level: 'healthy', headline: 'Fixture healthy', operations: [], critical_count: 0, warning_count: 0 };
const stats = {
  cpu: { count: 4, load_1: 1, load_5: 1, load_15: 1, temp_c: 40, freq_mhz: 2000, governor: 'fixture' },
  memory: { total_bytes: 8192, used_bytes: 4096, available_bytes: 4096, swap_total_bytes: 0, swap_used_bytes: 0, bcachefs_btree_cache_bytes: 0 },
  network: [{ name: 'fixture0', rx_bytes: 10000, tx_bytes: 5000, rx_packets: 10, tx_packets: 5, speed_mbps: 1000, up: true, addresses: [] }],
  disk_io: [{ name: 'fixture-disk', read_bytes: 10000, write_bytes: 5000, read_ios: 10, write_ios: 5, io_in_progress: 0 }],
};

async function scenario(name, role, run) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 1100 }, locale: 'en-US', timezoneId: 'UTC' });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  const errors = [], errorReads = [], expectedAuthErrors = [], unexpected = [], calls = [], requests = [];
  const historySamples = Array.from({ length: 8 }, (_, index) => ({
    ts: Date.now() - (8 - index) * 10000, in_rate: 20 + index, out_rate: 10 + index,
  }));
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => {
    if (message.type() !== 'error') return;
    // Browsers log the intentionally rejected guest auth probe as a resource
    // error. Only this exact fixture 401 is expected, not arbitrary JS errors.
    if (!message.args().length && /401/.test(message.text())
      && message.location().url === `${origin}/api/auth/check` && role === 'guest') {
      expectedAuthErrors.push(message.text()); return;
    }
    if (!message.args().length) { errors.push(message.text()); return; }
    errorReads.push(Promise.all(message.args().map(argument =>
      argument.evaluate(value => String(value?.message ?? value))
    )).then(parts => errors.push(parts.join(' '))).catch(() => errors.push(message.text())));
  });
  await context.addInitScript(() => {
    localStorage.setItem('nasty:ssh_password_auth_dismissed', '1');
    localStorage.setItem('nasty:system_recovery_backup_dismissed:v1', '1');
    localStorage.setItem('nasty:dashboard:v7', JSON.stringify({ version: 7, preset: 'custom', hiddenPresetTabs: [],
      activeViewId: 'browser', customViews: [{ id: 'browser', name: 'Browser qualification', density: 'comfortable',
        widgets: ['history', 'network', 'disk_io'].map((id, index) => ({ id, visible: true, width: 'full',
          presentation: 'standard', column: 0, row: index * 10, priority: index })) }] }));
  });
  await page.route('**/*', async route => {
    const request = route.request(), url = new URL(request.url());
    requests.push(`${request.method()} ${url.pathname}`);
    if (url.origin !== origin || request.method() !== 'GET') {
      unexpected.push(`${request.method()} ${url.href}`); await route.abort(); return;
    }
    const json = (body, code = 200) => route.fulfill({ status: code, contentType: 'application/json', body: JSON.stringify(body) });
    if (url.pathname === '/api/boot_status') return json({ overall: 'ready', phases: [], process_started_at_unix: 1, ready_at_ms: 1 });
    if (url.pathname === '/api/auth/check') return json({}, role === 'guest' ? 401 : 200);
    if (url.pathname === '/api/auth/oidc/available' || url.pathname === '/api/auth/webauthn/available') return json({ available: false });
    if (url.pathname === '/health') return json({ commit: 'fixture' });
    if (url.pathname === '/api/user/files/roots') return json([]);
    const share = /^\/api\/public\/share\/(alpha|beta)$/.exec(url.pathname);
    if (share) return json({ entries: [{ root: 0, name: `${share[1]}.txt`, is_dir: false, size: 42 }],
      password_required: false, unlocked: true, expires_at: null, media_preview_enabled: false });
    if (request.isNavigationRequest()) return route.fulfill({ contentType: 'text/html', body: html });
    if (url.pathname.startsWith('/_app/')) return route.continue();
    unexpected.push(`${request.method()} ${url.pathname}`);
    return json({ error: 'Unmocked fixture request' }, 404);
  });
  await page.routeWebSocket('**/*', ws => {
    if (ws.url() !== `${origin.replace('http:', 'ws:')}/ws` || role === 'guest') {
      unexpected.push(`WebSocket ${ws.url()}`); ws.close(); return;
    }
    ws.onMessage(raw => {
      const { id, method, params } = JSON.parse(String(raw));
      calls.push({ method, params });
      const answers = {
        'system.info': { hostname: 'fixture', version: 'fixture', uptime_seconds: 100, kernel: 'fixture', bcachefs_version: 'fixture',
          bcachefs_is_custom: false, timezone: 'UTC', ntp_synced: true },
        'system.settings.get': { clock_24h: true, temp_unit: 'celsius' },
        'system.status': status, 'system.health': { status: 'healthy', services: [] },
        'system.reboot_required': false, 'system.network.pending': [],
        'system.update.version': { current_version: 'fixture', update_available: false, status: 'up_to_date' },
        'system.stats': stats, 'fs.list': [], 'system.alerts': [], 'system.disks': [],
        'service.protocol.list': [], 'apps.status': { installed: false, running: false, pool: null },
        'auth.me': { username: 'fixture', role, groups: [], must_change_password: false },
      };
      let result;
      if (method === 'system.metrics.history') {
        const name = { cpu: 'cpu', mem: 'memory', net: 'fixture0', disk: 'fixture-disk' }[params.kind];
        result = [{ name, samples: historySamples }];
      } else if (Object.hasOwn(answers, method)) result = answers[method];
      else {
        unexpected.push(`RPC ${method}`);
        ws.send(JSON.stringify({ jsonrpc: '2.0', id, error: { code: -32601, message: 'Unmocked fixture RPC' } }));
        return;
      }
      ws.send(JSON.stringify({ jsonrpc: '2.0', id, result }));
    });
    ws.send(JSON.stringify({ authenticated: true, username: 'fixture', role }));
  });
  const outcome = { name, passed: false };
  outcomes.push(outcome);
  try {
    await run(page, calls, requests, historySamples);
    await Promise.all(errorReads);
    assert.deepEqual(unexpected, [], 'unexpected network/RPC access');
    assert.deepEqual(errors, [], 'browser errors');
    outcome.passed = true;
    console.log(`PASS ${values.browser}: ${name}`);
  } catch (error) {
    outcome.error = error.stack;
    await page.screenshot({ path: `${results}/${name}.png`, fullPage: true });
    await writeFile(`${results}/${name}.html`, await page.content());
    console.error(`FAIL ${values.browser}: ${name}: ${error.message}`);
  } finally {
    await Promise.all(errorReads);
    Object.assign(outcome, { errors, expectedAuthErrors, unexpected, calls, requests });
    await context.close();
  }
}

// Insert real links and click them: exercise Kit's router without importing
// private/generated client functions or simulating popstate manually.
async function navigate(page, path) {
  await page.evaluate(path => {
    const link = document.createElement('a'); link.href = path;
    link.id = 'qualification-link'; link.textContent = 'Qualification navigation';
    document.body.append(link);
  }, path);
  await page.locator('#qualification-link').click();
  await page.waitForURL(`${origin}${path}`);
  await page.locator('#qualification-link').evaluate(node => node.remove());
}

try {
  await scenario('launcher-history', 'admin', async page => {
    await page.goto(`${origin}/menu`);
    const group = page.locator('[data-launcher-group]').first();
    await group.waitFor();
    const id = await group.getAttribute('data-launcher-group');
    await page.evaluate(() => { window.qualificationDocument = 'same-document'; });
    await group.click();
    await page.waitForURL(`${origin}/menu?group=${encodeURIComponent(id)}`);
    const back = page.getByRole('button', { name: 'Back to launcher categories' });
    await back.waitFor();
    assert(await back.evaluate(node => document.activeElement === node), 'group entry should focus Back');
    await page.goBack();
    await page.waitForURL(`${origin}/menu`);
    await group.waitFor();
    await page.goForward();
    await back.waitFor();
    await back.click();
    await page.waitForURL(`${origin}/menu`);
    assert(await group.evaluate(node => document.activeElement === node), 'closing group should restore focus');
    assert.equal(await page.evaluate(() => window.qualificationDocument), 'same-document');
  });
  await scenario('charts-dialog', 'admin', async (page, calls) => {
    await page.goto(`${origin}/`);
    for (const subject of ['CPU usage history', 'Memory usage history', 'fixture0 network throughput history', 'fixture-disk disk throughput history']) {
      const chart = page.getByRole('img', { name: new RegExp(`^${subject}`) });
      await chart.waitFor();
      const path = chart.locator('svg path:visible').first();
      await path.waitFor();
      assert(!/NaN|Infinity/.test(await path.getAttribute('d')), 'chart geometry must be finite');
    }
    await page.getByRole('button', { name: 'Customize', exact: true }).click();
    const customize = page.getByRole('button', { name: 'Widgets & views', exact: true });
    await customize.click();
    const dialog = page.getByRole('dialog', { name: 'Customize dashboard' });
    await dialog.waitFor();
    assert(await dialog.evaluate(node => node.contains(document.activeElement)), 'dialog must capture focus');
    for (let index = 0; index < 12; index++) {
      await page.keyboard.press('Tab');
      assert(await dialog.evaluate(node => node.contains(document.activeElement)), 'Tab must stay inside dialog');
    }
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'hidden' });
    assert(await customize.evaluate(node => document.activeElement === node), 'dialog must restore trigger focus');
    assert(calls.filter(call => call.method === 'system.metrics.history').length >= 4);
  });
  await scenario('role-redirects', 'user', async page => {
    await page.goto(`${origin}/menu`);
    await page.waitForURL(`${origin}/portal`);
    await page.getByRole('button', { name: 'Files', exact: true }).waitFor();
    await navigate(page, '/menu');
    await page.waitForURL(`${origin}/portal`);
  });
  await scenario('login-labels', 'guest', async (page, calls) => {
    await page.goto(`${origin}/menu`);
    const username = page.getByLabel('Username', { exact: true });
    await username.fill('fixture');
    await page.getByText('Username', { exact: true }).click();
    assert(await username.evaluate(node => document.activeElement === node), 'bits-ui label must focus its input');
    await page.getByLabel('Password', { exact: true }).fill('not-submitted');
    assert.equal(calls.length, 0, 'unauthenticated page must not make RPCs');
  });
  await scenario('public-share-token', 'guest', async (page, calls, requests) => {
    await page.goto(`${origin}/share/alpha`);
    await page.getByText('alpha.txt', { exact: true }).waitFor();
    await page.evaluate(() => { window.qualificationDocument = 'same-document'; });
    await navigate(page, '/share/beta');
    await page.getByText('beta.txt', { exact: true }).waitFor();
    assert.equal(await page.getByText('alpha.txt', { exact: true }).count(), 0);
    await page.goBack();
    await page.getByText('alpha.txt', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.qualificationDocument), 'same-document');
    assert.equal(calls.length, 0, 'public share must not make authenticated RPCs');
    assert(!requests.some(request => request.includes('/api/auth/')), 'public share must not probe auth');
  });
  await scenario('chart-tooltips', 'admin', async (page, _calls, _requests, samples) => {
    await page.goto(`${origin}/`);
    for (const subject of ['CPU usage history', 'Memory usage history', 'fixture0 network throughput history', 'fixture-disk disk throughput history']) {
      const chart = page.getByRole('img', { name: new RegExp(`^${subject}`) });
      await chart.locator('svg path:visible').first().waitFor();
      await chart.scrollIntoViewIfNeeded();
      const box = await chart.locator('svg').first().boundingBox();
      assert(box);
      await page.mouse.move(box.x + box.width * 0.7, box.y + box.height * 0.4);
      await page.locator('.lc-tooltip-container:visible span.font-mono').first().waitFor();
      const label = await page.locator('.lc-tooltip-container:visible div.font-medium').first().textContent();
      const times = await page.evaluate(samples => samples.map(sample => new Date(sample.ts).toLocaleTimeString()), samples);
      assert(times.includes(label.trim()), `tooltip must display the selected sample time, got ${label}`);
      // Move off after the actual payload renders, not merely an empty container.
      await page.mouse.move(0, 0);
    }
  });
} finally {
  await writeFile(`${results}/summary.json`, JSON.stringify({ browser: values.browser, runedModules: runedModules.length, outcomes }, null, 2));
  await browser.close();
  await server.close();
}
if (outcomes.some(outcome => !outcome.passed)) process.exitCode = 1;
