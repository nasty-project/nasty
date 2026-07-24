import {
	Activity,
	Archive,
	Bell,
	Box,
	CircuitBoard,
	Cpu,
	Database,
	Flame,
	FolderOpen,
	Globe,
	Grid3X3,
	HardDrive,
	Layers,
	LayoutDashboard,
	Lock,
	Monitor,
	Network,
	RefreshCw,
	ScrollText,
	Server,
	Settings,
	Share2,
	Shield,
	ShieldCheck,
	Terminal,
	Wrench
} from '@lucide/svelte';

export type NavIcon = typeof LayoutDashboard;
export type NavMode = 'full' | 'common';

export interface NavItem {
	kind: 'item';
	id: string;
	href: string;
	label: string;
	icon: NavIcon;
	keywords: string[];
	commonRank?: number;
	requires?: 'kvm';
}

export interface NavGroup {
	kind: 'group';
	id: string;
	label: string;
	icon: NavIcon;
	children: NavItem[];
}

export type NavEntry = NavItem | NavGroup;

export interface NavigationContext {
	kvmAvailable: boolean;
}

export const NAVIGATION_CONTEXT = Symbol('navigation');

const item = (
	id: string,
	href: string,
	label: string,
	icon: NavIcon,
	keywords: string[],
	options: Pick<NavItem, 'commonRank' | 'requires'> = {}
): NavItem => ({ kind: 'item', id, href, label, icon, keywords, ...options });

const NAVIGATION: NavEntry[] = [
	item('dashboard', '/', 'Dashboard', LayoutDashboard, ['dashboard', 'overview', 'stats', 'cpu', 'memory', 'load', 'charts', 'home'], { commonRank: 0 }),
	{
		kind: 'group',
		id: 'storage',
		label: 'Storage',
		icon: Database,
		children: [
			item('filesystems', '/filesystems', 'Filesystems', Database, ['filesystem', 'pool', 'bcachefs', 'format', 'mount', 'unmount', 'create', 'encryption', 'replicas', 'erasure', 'tiering', 'compression', 'scrub', 'fsck', 'check', 'repair', 'balance', 'rebuild', 'defrag', 'add disk', 'remove disk', 'tpm', 'tpm2', 'seal', 'unlock', 'key', 'quota'], { commonRank: 1 }),
			item('subvolumes', '/subvolumes', 'Subvolumes', Layers, ['subvolume', 'snapshot', 'quota', 'block device', 'dataset', 'clone', 'send', 'receive', 'replication', 'volume', 'zvol', 'lun', 'namespace'], { commonRank: 2 }),
			item('disks', '/disks', 'Disks', HardDrive, ['disk', 'drive', 'smart', 'health', 'temperature', 'ssd', 'hdd', 'sas', 'nvme', 'topology', 'device', 'wipe', 'format', 'partition', 'serial', 'model', 'firmware', 'pcie', 'endurance', 'wear', 'scan'], { commonRank: 3 }),
			item('operations', '/operations', 'Operations', Activity, ['operation', 'scrub', 'reconcile', 'balance', 'copygc', 'evacuate', 'progress', 'activity', 'job']),
			item('files', '/files', 'Files', FolderOpen, ['file', 'browser', 'folder', 'directory', 'upload', 'download', 'rename', 'move', 'copy', 'permissions'], { commonRank: 4 })
		]
	},
	item('sharing', '/sharing', 'Sharing', Share2, ['share', 'nfs', 'smb', 'samba', 'cifs', 'iscsi', 'nvmeof', 'nvme-of', 'export', 'target', 'lun', 'acl', 'chap', 'portal', 'subsystem', 'nqn', 'iqn', 'client', 'username']),
	{
		kind: 'group',
		id: 'protection',
		label: 'Protection',
		icon: Shield,
		children: [
			item('backups', '/backups', 'Backups', Archive, ['backup', 'restore', 'rustic', 'restic', 'snapshot', 'retention', 'schedule', 'offsite', 'encrypt', 'cron', 's3', 'b2', 'sftp', 'rest', 'repository', 'repo', 'init', 'prune', 'check', 'cacert', 'ca cert', 'password'], { commonRank: 7 }),
			item('alerts', '/alerts', 'Alerts', Bell, ['alert', 'notification', 'warning', 'critical', 'rule', 'threshold', 'monitor', 'webhook', 'email', 'telegram', 'discord', 'notify', 'severity', 'acknowledge', 'resolve']),
			item('firewall', '/firewall', 'Firewall', Flame, ['firewall', 'port', 'nftables', 'restrict', 'ip', 'interface', 'security', 'network', 'rule', 'block', 'allow', 'tcp', 'udp']),
			item('tls', '/tls', 'TLS', Lock, ['tls', 'ssl', 'certificate', 'cert', 'https', 'encrypt', 'letsencrypt', 'acme', 'dns', 'cloudflare', 'domain', 'ca', 'caddy', 'internal', 'self-signed', 'selfsigned', 'staging', 'renew', 'root']),
			item('ingress', '/ingress', 'Ingress', Network, ['ingress', 'reverse', 'proxy', 'route', 'caddy', 'subdomain', 'host', 'path', 'apps', 'domain', 'https', 'redirect']),
			item('vpn', '/vpn', 'VPN', Globe, ['vpn', 'tailscale', 'remote', 'access', 'tunnel', 'wireguard', 'connect', 'auth key', 'exit node', 'subnet', 'peer'])
		]
	},
	{
		kind: 'group',
		id: 'compute',
		label: 'Compute',
		icon: Cpu,
		children: [
			item('vms', '/vms', 'VMs', Monitor, ['vm', 'virtual', 'machine', 'qemu', 'kvm', 'cpu', 'vnc', 'passthrough', 'iso', 'cdrom', 'disk', 'memory', 'ram', 'boot', 'uefi', 'ovmf', 'spice', 'bridge', 'console'], { commonRank: 6, requires: 'kvm' }),
			item('apps', '/apps', 'Apps', Box, ['app', 'docker', 'container', 'compose', 'install', 'image', 'port', 'volume', 'env', 'pull', 'logs', 'restart', 'stop', 'start'], { commonRank: 5 })
		]
	},
	item('terminal', '/terminal', 'Terminal', Terminal, ['terminal', 'shell', 'ssh', 'console', 'command', 'bash', 'web', 'tty']),
	{
		kind: 'group',
		id: 'system',
		label: 'System',
		icon: Wrench,
		children: [
			item('services', '/services', 'Services', Server, ['service', 'protocol', 'nfs', 'smb', 'iscsi', 'smart', 'avahi', 'mdns', 'enable', 'disable', 'rest server', 'backup server', 'receiver', 'htpasswd', 'docker', 'container', 'runtime', 'ups', 'nut', 'battery', 'power', 'shutdown', 'uninterruptible']),
			item('hardware', '/hardware', 'Hardware', CircuitBoard, ['hardware', 'pci', 'iommu', 'group', 'passthrough', 'vfio', 'gpu', 'device', 'driver', 'lspci', 'tpm', 'tpm2', 'secure boot', 'secureboot', 'cpu', 'memory', 'ram', 'dmi', 'bios', 'firmware', 'motherboard', 'mainboard', 'usb', 'nic']),
			item('logs', '/logs', 'Logs', ScrollText, ['log', 'journal', 'systemd', 'debug', 'error', 'follow', 'stream', 'filter', 'level', 'tail', 'kernel', 'dmesg'], { commonRank: 8 }),
			item('update', '/update', 'Update', RefreshCw, ['update', 'upgrade', 'version', 'release', 'nixos', 'rebuild', 'generation', 'nasty', 'nixpkgs', 'bcachefs', 'flake', 'lock', 'rollback', 'pin']),
			item('users', '/users', 'Access Control', ShieldCheck, ['user', 'password', 'role', 'admin', 'group', 'permission', 'token', 'api', 'access', 'auth', 'login', 'security key', 'webauthn', 'passkey', 'yubikey', 'touch id', 'windows hello', 'authenticator', 'fido', '2fa', 'mfa', 'sso', 'oidc', 'single sign-on', 'provider']),
			item('settings', '/settings', 'Settings', Settings, ['setting', 'hostname', 'timezone', 'clock', 'directory', 'active directory', 'domain', 'ad', 'network', 'ip', 'dhcp', 'dns', 'bond', 'vlan', 'bridge', 'static', 'gateway', 'route', 'mtu', 'notification', 'email', 'smtp', 'telegram', 'webhook', 'tuning', 'nfs threads', 'metrics', 'prometheus', 'telemetry', 'log level', 'theme', 'dark', 'light', 'appearance', 'custom nix', 'custom.nix', 'nixos', 'package', 'systemd'])
		]
	}
];

export const LAUNCHER_NAV_ITEM = item('launcher', '/menu', 'Launcher', Grid3X3, ['launcher', 'menu', 'navigation', 'pages']);

export function isNavGroup(entry: NavEntry): entry is NavGroup {
	return entry.kind === 'group';
}

function isVisible(item: NavItem, context: NavigationContext): boolean {
	return item.requires !== 'kvm' || context.kvmAvailable;
}

export function resolveNavigation(context: NavigationContext): NavEntry[] {
	return NAVIGATION.map((entry) => {
		if (!isNavGroup(entry)) return entry;
		return { ...entry, children: entry.children.filter((child) => isVisible(child, context)) };
	}).filter((entry) => isNavGroup(entry) ? entry.children.length > 0 : isVisible(entry, context));
}

export function flattenNavigation(entries: NavEntry[]): NavItem[] {
	return entries.flatMap((entry) => isNavGroup(entry) ? entry.children : [entry]);
}

export function commonNavigation(entries: NavEntry[]): NavItem[] {
	return flattenNavigation(entries)
		.filter((entry) => entry.commonRank !== undefined)
		.sort((a, b) => (a.commonRank ?? 0) - (b.commonRank ?? 0));
}

export function navigationForMode(entries: NavEntry[], mode: NavMode): NavEntry[] {
	return mode === 'common' ? commonNavigation(entries) : entries;
}

export function pathMatches(path: string, href: string): boolean {
	return path === href || (href !== '/' && path.startsWith(href));
}

export function currentNavigationItem(path: string, entries: NavEntry[]): NavItem {
	if (path === LAUNCHER_NAV_ITEM.href) return LAUNCHER_NAV_ITEM;
	const items = flattenNavigation(entries);
	return [...items].sort((a, b) => b.href.length - a.href.length)
		.find((entry) => pathMatches(path, entry.href)) ?? items[0];
}

export function activeNavigationGroup(path: string, entries: NavEntry[]): string | null {
	return entries.find((entry) => isNavGroup(entry) && entry.children.some((child) => pathMatches(path, child.href)))?.id ?? null;
}

export function searchNavigation(entries: NavEntry[], query: string): Set<string> {
	const normalized = query.trim().toLowerCase();
	if (!normalized) return new Set();

	const matches = new Set<string>();
	for (const entry of entries) {
		if (isNavGroup(entry)) {
			const groupMatches = entry.label.toLowerCase().includes(normalized);
			for (const child of entry.children) {
				if (groupMatches || itemMatches(child, normalized)) matches.add(child.href);
			}
		} else if (itemMatches(entry, normalized)) {
			matches.add(entry.href);
		}
	}
	return matches;
}

function itemMatches(item: NavItem, query: string): boolean {
	return item.label.toLowerCase().includes(query) || item.keywords.some((keyword) => keyword.includes(query));
}
