"""Read-only maintenance status and assets. No engine, login, or repair API."""
import glob
import grp
import ipaddress
import json
import os
from pathlib import Path
import socket
import socketserver
import re
import subprocess
import sys
import time
from http.server import BaseHTTPRequestHandler

ACTIVE = Path('/run/nasty-maintenance')
FLAG = Path('/var/lib/nasty/maintenance')
RUNTIME = Path('/run/nasty-maintenance-web')
SOCKET = Path('/run/nasty-maintenance-http/status.sock')
LISTENERS = Path('/var/lib/nasty/webui-listeners.json')
RESTRICTIONS = Path('/var/lib/nasty/firewall-restrictions.json')
MOUNTINFO = Path('/proc/1/mountinfo')
BOOT_ID = Path('/proc/sys/kernel/random/boot_id')


def mount_records(text):
    def decode(value):
        for encoded, decoded in [('\\040', ' '), ('\\011', '\t'), ('\\012', '\n'), ('\\134', '\\')]:
            value = value.replace(encoded, decoded)
        return value
    records = []
    for line in text.splitlines():
        before, after = line.split(' - ', 1)
        fields, filesystem = before.split(), after.split()
        records.append((decode(fields[4]), filesystem[0], decode(filesystem[1])))
    return records


def data_mounts_present(records):
    root_source = next(source for target, _, source in records if target == '/')
    return any(target == '/fs' or target.startswith('/fs/') or
               (kind == 'bcachefs' and source != root_source)
               for target, kind, source in records)


def os_path(path, records):
    resolved = os.path.realpath(path)
    if resolved == '/fs' or resolved.startswith('/fs/'):
        return False
    root_source = next(source for target, _, source in records if target == '/')
    relevant = [(target, kind, source) for target, kind, source in records
                if resolved == target or resolved.startswith(target.rstrip('/') + '/')]
    _, kind, source = max(relevant, key=lambda record: len(record[0]))
    return kind != 'bcachefs' or source == root_source


def configured_ports():
    path = LISTENERS
    if path.exists():
        ports = json.loads(path.read_text())['confirmed']
    else:
        ports = {'https_port': int(os.environ['NASTY_WEBUI_HTTPS_PORT']),
                 'http_port': None if os.environ['NASTY_WEBUI_HTTP_PORT'] == 'disabled'
                 else int(os.environ['NASTY_WEBUI_HTTP_PORT'])}
    for value in [ports['https_port'], ports['http_port']]:
        if value is not None and (type(value) is not int or not 1 <= value <= 65535 or value in (2019, 2137)):
            raise ValueError('Invalid maintenance listener port')
    if ports['https_port'] is None or ports['https_port'] == ports['http_port']:
        raise ValueError('Maintenance requires distinct HTTPS and optional HTTP ports')
    return ports


def prepare():
    records = mount_records(MOUNTINFO.read_text())
    if not os_path(RUNTIME, records) or not os_path('/var/lib/nasty-maintenance-caddy', records):
        raise ValueError('Maintenance web state must be on the OS filesystem')
    ports = configured_ports()
    pairs = []
    cert, key = os.environ.get('NASTY_MAINTENANCE_CERT'), os.environ.get('NASTY_MAINTENANCE_KEY')
    if cert and key and os_path(cert, records) and os_path(key, records):
        pairs.append({'certificate': cert, 'key': key})
    cache = '/var/lib/caddy/.local/share/caddy/certificates'
    if os_path(cache, records):
        for cert in sorted(glob.glob(cache + '/*/*/*.crt'), key=lambda value: (Path(value).stem != 'nasty.local', value))[:64]:
            key = cert[:-4] + '.key'
            if os_path(cert, records) and os_path(key, records) and os.path.isfile(key):
                pairs.append({'certificate': cert, 'key': key})
    # Only the local internal CA may issue a fallback. Never start ACME,
    # restore app routes, or import the normal Caddy configuration.
    if pairs:
        pairs[0]['tags'] = ['maintenance-default']
    tls = {'certificates': {'load_files': pairs} if pairs else {'automate': ['nasty.local']},
           'automation': {'policies': [{'subjects': ['nasty.local'], 'issuers': [{'module': 'internal'}]}]}}
    headers = {'Cache-Control': ['no-store'], 'X-Content-Type-Options': ['nosniff'],
               'X-Frame-Options': ['DENY'], 'Referrer-Policy': ['no-referrer'],
               'Content-Security-Policy': ["default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"]}
    servers = {'maintenance': {
        'listen': [f":{ports['https_port']}"], 'automatic_https': {'disable': True},
        'tls_connection_policies': [{'default_sni': 'nasty.local', 'fallback_sni': 'nasty.local'}],
        'routes': [{'handle': [{'handler': 'headers', 'response': {'set': headers}},
                              {'handler': 'reverse_proxy', 'headers': {'request': {'delete': ['Cookie', 'Authorization']}},
                               'upstreams': [{'dial': 'unix//run/nasty-maintenance-http/status.sock'}]}]}]}}
    if pairs:
        servers['maintenance']['tls_connection_policies'][0]['certificate_selection'] = {'any_tag': ['maintenance-default']}
    if ports['http_port'] is not None:
        suffix = '' if ports['https_port'] == 443 else f":{ports['https_port']}"
        servers['redirect'] = {'listen': [f":{ports['http_port']}"], 'routes': [{'handle': [
            {'handler': 'map', 'source': '{http.request.host}', 'destinations': ['{maintenance.host}'],
             'mappings': [{'input_regexp': '^(.+:.*)$', 'outputs': ['[${1}]']}], 'defaults': ['{http.request.host}']},
            {'handler': 'static_response', 'status_code': 308,
             'headers': {'Location': ['https://{maintenance.host}' + suffix + '{http.request.uri}']}}]}]}
    config = {'admin': {'disabled': True, 'config': {'persist': False}},
              'apps': {'http': {'servers': servers}, 'tls': tls,
                       'pki': {'certificate_authorities': {'local': {'install_trust': False}}}}}
    RUNTIME.mkdir(mode=0o750, exist_ok=True)
    (RUNTIME / 'ports.json').write_text(json.dumps(ports))
    (RUNTIME / 'caddy.json').write_text(json.dumps(config))
    restrictions = RESTRICTIONS
    policy = json.loads(restrictions.read_text()) if restrictions.exists() else {}
    if not isinstance(policy, dict) or not isinstance(policy.get('services', {}), dict) or not isinstance(policy.get('interfaces', {}), dict):
        raise ValueError('Invalid WebUI firewall restrictions')
    sources = policy.get('services', {}).get('webui', [])
    interfaces = policy.get('interfaces', {}).get('webui', [])
    if not isinstance(sources, list) or not isinstance(interfaces, list) or any(not isinstance(source, str) for source in sources):
        raise ValueError('Invalid WebUI firewall restrictions')
    rules = []
    for interface in interfaces or [None]:
        if interface is not None and (not isinstance(interface, str) or not re.fullmatch(r'[a-zA-Z0-9_.@-]{1,15}', interface)):
            raise ValueError('Invalid WebUI firewall interface')
        for source in sources or [None]:
            qualifier = ''
            if source is not None:
                network = ipaddress.ip_network(source, strict=False)
                qualifier = f"{'ip6' if network.version == 6 else 'ip'} saddr {network} "
            if interface is not None:
                qualifier += f'iifname "{interface}" '
            for port in [ports['https_port'], ports['http_port']]:
                if port is not None:
                    rules.append(f'add rule inet nasty input {qualifier}tcp dport {port} accept')
    (RUNTIME / 'web.nft').write_text('\n'.join(rules) + '\n')
    write_status()
    for name in ('ports.json', 'caddy.json', 'web.nft', 'status.json'):
        os.chown(RUNTIME / name, 0, grp.getgrnam('caddy').gr_gid)
        os.chmod(RUNTIME / name, 0o640)


def ssh_ready(ports):
    try:
        result = subprocess.run(['systemctl', 'is-active', '--quiet', 'sshd.service'], timeout=2, check=False)
        if result.returncode != 0:
            return False
        for port in ports:
            for host in ('127.0.0.1', '::1'):
                try:
                    with socket.create_connection((host, port), timeout=0.5) as connection:
                        if connection.recv(256).startswith(b'SSH-'):
                            return True
                except OSError:
                    continue
    except (OSError, subprocess.TimeoutExpired):
        pass
    return False


def status():
    ports = json.loads(os.environ['NASTY_MAINTENANCE_SSH_PORTS'])
    mounts = 'unknown'
    try:
        mounts = 'mounted' if data_mounts_present(mount_records(MOUNTINFO.read_text())) else 'unmounted'
    except (OSError, ValueError, IndexError, StopIteration):
        pass
    return {'maintenance': ACTIVE.exists(), 'boot_id': BOOT_ID.read_text().strip(), 'observed_at': time.monotonic(),
            'ssh_ready': ssh_ready(ports), 'ssh_ports': ports, 'data_mounts': mounts,
            'exit_scheduled': not FLAG.exists()}


def write_status():
    temporary = RUNTIME / 'status.json.tmp'
    temporary.write_text(json.dumps(status()))
    os.chmod(temporary, 0o640)
    temporary.replace(RUNTIME / 'status.json')


def collect():
    while True:
        write_status()
        time.sleep(3)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        path = self.path.split('?', 1)[0]
        if not ACTIVE.exists():
            self.send_error(503)
            return
        if path == '/api/maintenance/status':
            try:
                snapshot = json.loads((RUNTIME / 'status.json').read_text())
                if not 0 <= time.monotonic() - snapshot['observed_at'] <= 10:
                    raise ValueError('Stale status')
            except (OSError, ValueError, KeyError):
                self.send_error(503)
                return
            snapshot.pop('observed_at')
            body, mime = json.dumps(snapshot).encode(), 'application/json'
        elif path == '/health':
            body, mime = b'{"status":"maintenance","maintenance":true}', 'application/json'
        elif path in ('/maintenance.js', '/maintenance.css'):
            body = (Path(os.environ['NASTY_MAINTENANCE_ASSETS']) / path[1:]).read_bytes()
            mime = 'text/javascript' if path.endswith('.js') else 'text/css'
        elif path.startswith(('/api/', '/ws')):
            self.send_error(404)
            return
        else:
            body = (Path(os.environ['NASTY_MAINTENANCE_ASSETS']) / 'index.html').read_bytes()
            mime = 'text/html; charset=utf-8'
        self.send_response(200)
        self.send_header('Content-Type', mime)
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.send_error(405)


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


def serve():
    path = SOCKET
    path.unlink(missing_ok=True)
    with Server(str(path), Handler) as server:
        os.chmod(path, 0o660)
        server.serve_forever()


if __name__ == '__main__':
    {'prepare': prepare, 'collect': collect, 'serve': serve}[sys.argv[1]]()
