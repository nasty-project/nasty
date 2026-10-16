export const CORE_RECOVERY_SOURCES = [
	{ path: '/var/lib/nasty', label: 'NASty settings and service definitions' },
	{ path: '/etc/nixos', label: 'installed system and hardware configuration' },
	{ path: '/var/lib/caddy', label: 'TLS certificates and local CA identity' },
	{ path: '/var/lib/systemd/credential.secret', label: 'host key for encrypted credentials' },
	{ path: '/etc/machine-id', label: 'source machine identity for host credential recovery' },
] as const;

export const CORE_RECOVERY_PATHS = CORE_RECOVERY_SOURCES.map(source => source.path);

export const SECURE_BOOT_RECOVERY_SOURCE = {
	path: '/var/lib/sbctl',
	label: 'Secure Boot private signing keys',
} as const;

export const RECOVERY_BACKUP_CHANGED_EVENT = 'nasty:recovery-backup-changed';
