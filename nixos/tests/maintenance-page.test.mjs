import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../maintenance-web/maintenance.js', import.meta.url), 'utf8');
const ready = { maintenance: true, boot_id: 'boot', ssh_ready: true, ssh_ports: [2222], data_mounts: 'unmounted', exit_scheduled: false };

async function page(statuses, health = { status: 'maintenance' }) {
  const elements = new Map();
  const element = () => ({ value: '', textContent: '', disabled: false, hidden: true, handlers: {},
    addEventListener(event, handler) { this.handlers[event] = handler; },
    replaceChildren(...options) { this.value = options[0]?.value ?? ''; }
  });
  const get = id => { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); };
  get('ssh-port').value = '22';
  const timers = [], redirects = [], requests = [];
  const context = {
    document: { getElementById: get, querySelectorAll: () => [], createElement: element },
    window: { location: { hostname: '[2001:db8::1]', replace: url => redirects.push(url) } },
    navigator: { clipboard: { writeText: async () => {} } },
    AbortSignal: { timeout: () => undefined },
    setTimeout: callback => timers.push(callback),
    fetch: async (url, options) => {
      requests.push({ url, options });
      const status = url === '/api/maintenance/status' ? statuses.shift() : health;
      return { ok: status !== null, json: async () => status };
    }
  };
  await vm.runInNewContext(source, context);
  return { get, timers, redirects, requests };
}

test('shows verified readiness and copyable configured-port IPv6 SSH instructions', async () => {
  const { get, requests } = await page([ready]);
  assert.match(get('ssh-status').textContent, /SSH service ready/);
  assert.match(get('mount-status').textContent, /verify your target pool/);
  assert.equal(get('ssh-command').textContent, "ssh -p 2222 'YOUR_OS_USER@2001:db8::1'");
  assert.equal(requests[0].options.credentials, 'omit');
  get('username').value = 'root';
  get('username').handlers.input();
  assert.equal(get('ssh-command').textContent, "ssh -p 2222 'root@2001:db8::1'");
  get('username').value = 'root; reboot';
  get('username').handlers.input();
  assert.equal(get('copy-ssh').disabled, true);
});

test('never reports unknown observations as ready or redirects while still in maintenance', async () => {
  const { get, timers, redirects } = await page([{ ...ready, ssh_ready: false, data_mounts: 'unknown' }, null]);
  assert.equal(get('ssh-status').className, 'warning');
  assert.match(get('mount-status').textContent, /could not be verified/);
  await timers.shift()();
  assert.match(get('ssh-status').textContent, /unknown/);
  assert.deepEqual(redirects, []);
});

test('keeps exit-scheduled mode active and only returns when the normal engine is healthy', async () => {
  const { get, timers, redirects } = await page([{ ...ready, exit_scheduled: true }, null], { status: 'ok' });
  assert.match(get('exit-status').textContent, /guards remain active/);
  await timers.shift()();
  assert.deepEqual(redirects, ['/']);
});

test('does not offer a malformed command when SSH is unconfigured', async () => {
  const { get } = await page([{ ...ready, ssh_ports: [], ssh_ready: false }]);
  assert.equal(get('copy-ssh').disabled, true);
  assert.match(get('ssh-command').textContent, /not configured/);
});
