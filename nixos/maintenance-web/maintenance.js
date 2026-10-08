const byId = id => document.getElementById(id);
let lastBoot = null;
function sshCommand() {
  const username = byId('username').value.trim();
  const valid = !username || /^[a-z_][a-z0-9_.-]*\$?$/i.test(username);
  const port = byId('ssh-port').value;
  const host = window.location.hostname.replace(/^\[|\]$/g, '');
  const destination = `'${`${username || 'YOUR_OS_USER'}@${host}`.replace(/'/g, "'\\''")}'`;
  byId('ssh-command').textContent = !port ? 'SSH is not configured. Use console access.' : valid ? `ssh -p ${port} ${destination}` : 'Enter a valid local OS username.';
  byId('copy-ssh').disabled = !valid || !lastBoot || !port;
}
byId('username').addEventListener('input', sshCommand);
byId('ssh-port').addEventListener('change', sshCommand);
byId('copy-ssh').addEventListener('click', () => copy(byId('copy-ssh'), byId('ssh-command').textContent));
document.querySelectorAll('[data-copy]').forEach(button => button.addEventListener('click', () => copy(button, button.dataset.copy)));
async function copy(button, text) {
  try { await navigator.clipboard.writeText(text); button.textContent = 'Copied'; }
  catch { button.textContent = 'Select and copy the command'; }
  setTimeout(() => { button.textContent = 'Copy command'; }, 2500);
}
async function poll() {
  try {
    const response = await fetch('/api/maintenance/status', { cache: 'no-store', credentials: 'omit', signal: AbortSignal.timeout(4000) });
    if (!response.ok) throw new Error('Unavailable');
    const status = await response.json();
    if (status.maintenance !== true || !status.boot_id) throw new Error('Not maintenance');
    byId('connection').textContent = 'Maintenance mode is active';
    byId('connection').className = 'ready';
    byId('ssh-status').textContent = status.ssh_ready ? 'SSH service ready — you can try logging in' : 'SSH service is not ready yet — use the console if this persists';
    byId('ssh-status').className = status.ssh_ready ? 'ready' : 'warning';
    byId('mount-status').textContent = status.data_mounts === 'unmounted' ? 'No data mounts detected — verify your target pool before repair' : status.data_mounts === 'mounted' ? 'Data mounts are present — do not repair mounted pools' : 'Data mount status could not be verified — check at the console or over SSH';
    byId('mount-status').className = status.data_mounts === 'unmounted' ? 'ready' : 'warning';
    byId('exit-status').textContent = status.exit_scheduled ? 'Normal startup is scheduled for the next reboot. Maintenance guards remain active until then.' : '';
    if (lastBoot !== status.boot_id) {
      lastBoot = status.boot_id;
      byId('ssh-port').replaceChildren(...status.ssh_ports.map(port => { const option = document.createElement('option'); option.value = String(port); option.textContent = String(port); return option; }));
    }
    sshCommand();
  } catch {
    byId('connection').textContent = 'Waiting for the NAS — it may be rebooting';
    byId('connection').className = 'warning';
    byId('ssh-status').textContent = 'SSH readiness is currently unknown';
    byId('mount-status').textContent = 'Data mount status is currently unknown';
    byId('exit-status').textContent = '';
    byId('ssh-status').className = byId('mount-status').className = 'warning';
    try {
      const response = await fetch('/health', { cache: 'no-store', credentials: 'omit', signal: AbortSignal.timeout(4000) });
      const health = await response.json();
      if (response.ok && health.status === 'ok' && health.maintenance !== true) {
        byId('dashboard').hidden = false;
        window.location.replace('/');
        return;
      }
    } catch { /* Keep waiting; do not claim the NAS is ready. */ }
  }
  setTimeout(poll, 3000);
}
poll();
