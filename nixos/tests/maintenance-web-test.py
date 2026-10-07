import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('maintenance_web', Path(__file__).parents[1] / 'maintenance-web.py')
web = importlib.util.module_from_spec(spec)
spec.loader.exec_module(web)

ROOT = '1 0 8:1 / / rw - ext4 /dev/root rw\n'


class MaintenanceWebTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        for name in ('RUNTIME', 'ACTIVE', 'FLAG', 'LISTENERS', 'RESTRICTIONS', 'MOUNTINFO', 'BOOT_ID', 'SOCKET'):
            replacement = root / name
            self.enterContext(patch.object(web, name, replacement))
        web.RUNTIME.mkdir()
        web.MOUNTINFO.write_text(ROOT)
        web.BOOT_ID.write_text('test-boot-id')
        web.ACTIVE.touch()
        web.FLAG.touch()
        web.SOCKET = root / 'http.sock'
        self.enterContext(patch.dict(os.environ, {
            'NASTY_WEBUI_HTTPS_PORT': '8443', 'NASTY_WEBUI_HTTP_PORT': '8080',
            'NASTY_MAINTENANCE_SSH_PORTS': '[22]', 'NASTY_MAINTENANCE_CERT': '', 'NASTY_MAINTENANCE_KEY': '',
            'NASTY_MAINTENANCE_ASSETS': str(Path(__file__).parents[1] / 'maintenance-web')
        }))

    def test_actual_mounts_not_the_maintenance_flag_determine_readiness(self):
        self.assertFalse(web.data_mounts_present(web.mount_records(ROOT)))
        for mount in ['2 1 8:2 / /fs/pool rw - xfs /dev/data rw\n', '2 1 8:2 / /elsewhere rw - bcachefs UUID=data rw\n']:
            self.assertTrue(web.data_mounts_present(web.mount_records(ROOT + mount)))
        self.assertFalse(web.os_path('/fs/cert.key', web.mount_records(ROOT)))

    def test_non_root_bcachefs_paths_are_not_certificate_sources(self):
        records = web.mount_records(ROOT + f'2 1 8:2 / {os.path.realpath("/var/lib/caddy")} rw - bcachefs UUID=data rw\n')
        self.assertFalse(web.os_path('/var/lib/caddy/certificate.key', records))

    def test_status_unknown_when_mount_table_cannot_be_read(self):
        web.MOUNTINFO.unlink()
        with patch.object(web, 'ssh_ready', return_value=False):
            value = web.status()
        self.assertEqual(value['data_mounts'], 'unknown')
        self.assertTrue(value['maintenance'])
        self.assertFalse(value['ssh_ready'])
        self.assertNotIn('pools', value)

    def test_exit_is_only_scheduled_not_implicitly_performed(self):
        web.FLAG.unlink()
        with patch.object(web, 'ssh_ready', return_value=True):
            value = web.status()
        self.assertTrue(value['exit_scheduled'])
        self.assertTrue(value['maintenance'])

    def test_confirmed_ports_win_over_unconfirmed_candidate(self):
        web.LISTENERS.write_text(json.dumps({'confirmed': {'https_port': 9443, 'http_port': None}, 'pending': {'ports': {'https_port': 9999}}}))
        self.assertEqual(web.configured_ports(), {'https_port': 9443, 'http_port': None})
        for value in (0, 65536, 2019, 2137, True, '443'):
            web.LISTENERS.write_text(json.dumps({'confirmed': {'https_port': value, 'http_port': None}}))
            with self.assertRaises(ValueError):
                web.configured_ports()

    def test_preparation_preserves_source_restrictions_and_disables_normal_caddy_features(self):
        web.RESTRICTIONS.write_text(json.dumps({'services': {'webui': ['192.0.2.0/24', '2001:db8::/32']}, 'interfaces': {'webui': ['eth0']}}))
        with patch.object(web.glob, 'glob', return_value=[]), patch.object(web.os, 'chown'), patch.object(web.grp, 'getgrnam', return_value=types.SimpleNamespace(gr_gid=os.getgid())), patch.object(web, 'ssh_ready', return_value=True):
            web.prepare()
        config = json.loads((web.RUNTIME / 'caddy.json').read_text())
        self.assertTrue(config['admin']['disabled'])
        self.assertFalse(config['admin']['config']['persist'])
        self.assertEqual(config['apps']['http']['servers']['maintenance']['listen'], [':8443'])
        issuer = config['apps']['tls']['automation']['policies'][0]['issuers'][0]
        self.assertEqual(issuer['module'], 'internal')
        self.assertNotIn('2137', json.dumps(config))
        rules = (web.RUNTIME / 'web.nft').read_text()
        self.assertIn('ip6 saddr 2001:db8::/32 iifname "eth0" tcp dport 8443', rules)
        self.assertIn('ip saddr 192.0.2.0/24 iifname "eth0" tcp dport 8080', rules)
        self.assertNotIn('tcp dport 443 ', rules)

    def test_confirmed_custom_https_only_listener_and_cached_certificate(self):
        web.LISTENERS.write_text(json.dumps({'confirmed': {'https_port': 9443, 'http_port': None}}))
        certificate = web.RUNTIME / 'nasty.local.crt'
        certificate.touch()
        certificate.with_suffix('.key').touch()
        with patch.object(web.glob, 'glob', return_value=[str(certificate)]), patch.object(web.os, 'chown'), patch.object(web.grp, 'getgrnam', return_value=types.SimpleNamespace(gr_gid=os.getgid())), patch.object(web, 'ssh_ready', return_value=True):
            web.prepare()
        config = json.loads((web.RUNTIME / 'caddy.json').read_text())
        servers = config['apps']['http']['servers']
        self.assertEqual(list(servers), ['maintenance'])
        self.assertEqual(servers['maintenance']['listen'], [':9443'])
        self.assertNotIn('automate', config['apps']['tls']['certificates'])
        self.assertEqual(config['apps']['tls']['certificates']['load_files'][0]['tags'], ['maintenance-default'])
        proxy = servers['maintenance']['routes'][0]['handle'][1]
        self.assertEqual(proxy['headers']['request']['delete'], ['Cookie', 'Authorization'])

    def test_malformed_firewall_policy_fails_closed(self):
        for policy in ({'services': {'webui': '192.0.2.1'}}, {'interfaces': {'webui': ['eth0" accept']}}):
            web.RESTRICTIONS.write_text(json.dumps(policy))
            with patch.object(web.glob, 'glob', return_value=[]), self.assertRaises(ValueError):
                web.prepare()
            self.assertFalse((web.RUNTIME / 'web.nft').exists())

    def test_ssh_banner_and_running_service_are_both_required(self):
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            port = listener.getsockname()[1]
            def banner():
                with listener.accept()[0] as connection:
                    connection.sendall(b'SSH-2.0-test\r\n')
            thread = threading.Thread(target=banner)
            thread.start()
            with patch.object(web.subprocess, 'run', return_value=types.SimpleNamespace(returncode=0)):
                self.assertTrue(web.ssh_ready([port]))
            thread.join(timeout=2)
            with patch.object(web.subprocess, 'run', return_value=types.SimpleNamespace(returncode=1)):
                self.assertFalse(web.ssh_ready([port]))

    def test_http_surface_is_read_only_and_rejects_stale_status(self):
        (web.RUNTIME / 'status.json').write_text(json.dumps({'maintenance': True, 'observed_at': time.monotonic(), 'boot_id': 'test'}))
        with web.Server(str(web.SOCKET), web.Handler) as server:
            thread = threading.Thread(target=server.serve_forever)
            thread.start()
            try:
                def request(method, path):
                    with socket.socket(socket.AF_UNIX) as client:
                        client.connect(str(web.SOCKET))
                        client.sendall(f'{method} {path} HTTP/1.0\r\nHost: localhost\r\n\r\n'.encode())
                        result = b''
                        while data := client.recv(65536):
                            result += data
                        return result
                self.assertIn(b'200 OK', request('GET', '/api/maintenance/status'))
                self.assertIn(b'405 Method Not Allowed', request('POST', '/api/maintenance/exit'))
                self.assertIn(b'404 Not Found', request('GET', '/api/login'))
                self.assertIn(b'No repairs run automatically', request('GET', '/settings'))
                (web.RUNTIME / 'status.json').write_text(json.dumps({'observed_at': time.monotonic() - 11}))
                self.assertIn(b'503 Service Unavailable', request('GET', '/api/maintenance/status'))
            finally:
                server.shutdown()
                thread.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
