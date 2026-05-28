<script lang="ts">
	import { Card, CardContent } from '$lib/components/ui/card';
	import { CircleHelp } from '@lucide/svelte';

	type Entry = { term: string; summary: string; detail?: string };

	const sections: { title: string; entries: Entry[] }[] = [
		{
			title: 'Getting Started',
			entries: [
				{
					term: 'Filesystem',
					summary: 'A storage pool built from one or more disks.',
					detail: 'In NASty, a filesystem is a bcachefs pool that spans one or more devices. All your data, subvolumes, and shares live inside a filesystem. You need at least one filesystem before you can store anything. Create one under Filesystems.',
				},
				{
					term: 'Subvolume',
					summary: 'An isolated directory or block device within a filesystem.',
					detail: 'Subvolumes are lightweight divisions of a filesystem. Each subvolume can have its own quota, compression, and tiering settings. There are two types: "filesystem" subvolumes (used for NFS/SMB file shares) and "block" subvolumes (used for iSCSI/NVMe-oF block storage). Think of them like folders with superpowers — they can be snapshotted, quota-limited, and independently managed.',
				},
				{
					term: 'Share',
					summary: 'A subvolume exported over the network so other machines can access it.',
					detail: 'A share makes a subvolume available to other computers on your network using a protocol like NFS or SMB. Without a share, data in a subvolume is only accessible locally on the NAS.',
				},
				{
					term: 'Snapshot',
					summary: 'A point-in-time copy of a subvolume.',
					detail: 'Snapshots are instant, space-efficient copies. They don\'t duplicate data — they share blocks with the original and only consume space as data diverges. Useful for backups and rollback.',
				},
			],
		},
		{
			title: 'Sharing Protocols',
			entries: [
				{
					term: 'NFS',
					summary: 'Network File System — the standard for Linux and macOS file sharing.',
					detail: 'Use NFS when your clients are Linux servers, Kubernetes nodes, or macOS workstations. It\'s fast, low-overhead, and widely supported. Best for: home labs, media servers, container storage, development environments.',
				},
				{
					term: 'SMB',
					summary: 'Server Message Block — the standard for Windows file sharing.',
					detail: 'Use SMB when your clients are Windows PCs or you need broad compatibility. Also works with macOS and Linux. Supports user authentication. Best for: Windows networks, mixed OS environments, desktop file access.',
				},
				{
					term: 'iSCSI',
					summary: 'Internet SCSI — presents a block device over the network.',
					detail: 'Use iSCSI when you need raw block storage — the client sees a disk, not files. Common for databases, VMs, and applications that need direct disk access. The client formats and mounts the block device itself. Best for: databases, virtual machines, applications needing consistent low-latency block I/O.',
				},
				{
					term: 'NVMe-oF',
					summary: 'NVMe over Fabrics — high-performance block storage over the network.',
					detail: 'Like iSCSI but faster — uses the NVMe protocol natively over the network. Requires NVMe-oF support on both ends. Best for: high-performance workloads, low-latency requirements, modern infrastructure.',
				},
			],
		},
		{
			title: 'Storage Concepts',
			entries: [
				{
					term: 'Quota',
					summary: 'A size limit on a subvolume.',
					detail: 'Quotas prevent a subvolume from consuming more than its allocated space. For block subvolumes (iSCSI/NVMe-oF), the quota defines the size of the virtual disk. For filesystem subvolumes (NFS/SMB), it\'s optional — without one, the subvolume can use all available space.',
				},
				{
					term: 'Replication',
					summary: 'Storing multiple copies of data across devices for redundancy.',
					detail: 'With 2x replication, every block is written to two different disks. If one disk fails, your data is still intact on the other. Higher replication means more safety but uses more space. Also called "mirroring" in traditional RAID terminology.',
				},
				{
					term: 'Compression',
					summary: 'Reducing data size on disk to save space.',
					detail: 'bcachefs supports transparent compression — data is compressed when written and decompressed when read. Options: lz4 (fast, moderate compression), zstd (good balance), gzip (maximum compression, slower). Compression is per-subvolume and can be changed at any time.',
				},
				{
					term: 'Tiering',
					summary: 'Automatically moving data between fast and slow storage.',
					detail: 'If your filesystem has both SSDs and HDDs, tiering writes new data to the fast tier (SSD) and moves cold data to the slow tier (HDD) in the background. This gives you SSD performance with HDD capacity. Configured via foreground/background/promote targets.',
				},
				{
					term: 'Scrub',
					summary: 'A background check that verifies all data checksums.',
					detail: 'Scrubbing reads every block and verifies its checksum to detect silent data corruption (bit rot). If replication is enabled, corrupted copies are automatically repaired from good ones. Run periodically — e.g., monthly.',
				},
				{
					term: 'Reconcile',
					summary: 'Background rebalancing of data across devices.',
					detail: 'Reconcile moves data between devices to maintain the desired layout — for example, after adding or removing a disk, or after changing tiering targets. It runs automatically when enabled.',
				},
				{
					term: 'Erasure Coding',
					summary: 'Parity-based redundancy that uses less space than mirroring.',
					detail: 'Instead of storing N full copies, erasure coding stores data plus parity blocks across multiple devices. Roughly: with replicas=2 and EC on, the layout is RAID-5-like (one parity); replicas=3 with EC is RAID-6-like (two parity). Usable capacity scales with (devices − parity) / devices, which is much better than 1/replicas mirroring once you have several disks. Trade-off: rebuilds and small writes are more expensive. Toggle when creating the filesystem.',
				},
				{
					term: 'Encryption',
					summary: 'At-rest encryption of every block written to the filesystem.',
					detail: 'bcachefs encrypts data and metadata with a key derived from a passphrase you provide at filesystem creation. The passphrase is required to unlock the filesystem at boot. Encryption is set at filesystem creation and cannot be added or removed later — pick it up front if you need it.',
				},
			],
		},
		{
			title: 'Disk Management',
			entries: [
				{
					term: 'Disk / Device',
					summary: 'A physical or virtual storage device (SSD, HDD, NVMe drive).',
					detail: 'NASty discovers all block devices in the system. Before a disk can be used in a filesystem, it may need to be wiped to remove existing partition tables or filesystem signatures.',
				},
				{
					term: 'Partition',
					summary: 'A section of a disk, divided at the hardware level.',
					detail: 'A single physical disk can be split into multiple partitions, each acting as a separate device. Most NAS setups use whole disks rather than partitions. Partitions are mainly relevant when a disk has an existing OS or data you want to preserve.',
				},
				{
					term: 'Wipe',
					summary: 'Erasing signatures and partition tables from a disk.',
					detail: 'Wiping removes filesystem signatures and partition tables so bcachefs can use the disk. This is destructive — all existing data on the disk is lost. Required when a disk was previously used by another system or filesystem.',
				},
				{
					term: 'Durability',
					summary: 'How reliable a device is considered for replication purposes.',
					detail: '0 = cache only (data is not durable), 1 = normal disk, 2 = hardware RAID or highly reliable storage. bcachefs uses this to decide where to place replicas — it won\'t put two replicas on devices with the same durability group.',
				},
			],
		},
		{
			title: 'System & Updates',
			entries: [
				{
					term: 'NixOS',
					summary: 'The Linux distribution that NASty runs on.',
					detail: 'NixOS is a declarative operating system — the entire system configuration is defined in code and rebuilt atomically. This means updates are safe and rollback is always possible. You don\'t need to know NixOS to use NASty, but it\'s why updates and rollbacks work so reliably.',
				},
				{
					term: 'Generation',
					summary: 'A snapshot of the entire system configuration.',
					detail: 'Every time NASty updates, NixOS creates a new generation — a complete, bootable system state. If an update causes problems, you can roll back to a previous generation from the Update page or the boot menu. Old generations can be garbage-collected to free disk space.',
				},
				{
					term: 'Firmware',
					summary: 'Low-level software embedded in hardware devices.',
					detail: 'Disk drives, network cards, and motherboards all have firmware. NASty can update firmware for supported devices through the fwupd service. Keeping firmware up to date improves stability and security.',
				},
			],
		},
		{
			title: 'Apps & Virtualization',
			entries: [
				{
					term: 'VM (Virtual Machine)',
					summary: 'A full computer emulated in software.',
					detail: 'VMs run a complete operating system with its own kernel, isolated from the host. Use VMs when you need a different OS (e.g., Windows), full isolation, or software that can\'t run in containers. Requires KVM support in the CPU.',
				},
				{
					term: 'App',
					summary: 'A self-contained application running in a Docker container.',
					detail: 'NASty\'s Apps page lets you deploy and manage Docker containers — pre-packaged applications like media servers, download managers, or home automation. Each app runs isolated from the host system.',
				},
				{
					term: 'Docker',
					summary: 'A container runtime for running isolated applications.',
					detail: 'Docker packages an application and all its dependencies into a container — a lightweight, portable unit that runs the same everywhere. Containers share the host kernel, making them much lighter than VMs.',
				},
				{
					term: 'Docker Compose',
					summary: 'A tool for defining multi-container applications.',
					detail: 'Some apps need multiple containers working together (e.g., a web app + database). Docker Compose defines these in a single YAML file, managing networking and dependencies between containers automatically.',
				},
				{
					term: 'allow_unsafe',
					summary: 'Per-app opt-in for privileged or host-impacting container options.',
					detail: 'NASty sandboxes app deploys by default — capabilities, host-path mounts, and other "escape hatch" options are stripped from compose files and rejected on simple installs. Set allow_unsafe on an app when you genuinely need things like privileged mode, host networking, or mounting /var. This is logged and visible on the app list so you remember which apps are running with extra trust.',
				},
				{
					term: 'Network Bridge',
					summary: 'A virtual L2 switch that lets VMs (and apps) share the host LAN.',
					detail: 'A bridge ties one or more host interfaces together so guests attached to it appear as ordinary devices on your physical network — they can pull DHCP from your router and be reached directly by IP. Configured under Network → Bridges; VMs select a bridge as their NIC backing instead of the default user-mode networking.',
				},
			],
		},
		{
			title: 'Networking & Services',
			entries: [
				{
					term: 'SSH',
					summary: 'Secure Shell — encrypted remote terminal access.',
					detail: 'SSH lets you connect to NASty\'s command line from another computer. Used for advanced administration, scripting, and debugging. Can be configured with password or key-based authentication.',
				},
				{
					term: 'Avahi (mDNS)',
					summary: 'Automatic network discovery — makes NASty findable by name.',
					detail: 'Avahi broadcasts NASty\'s hostname on the local network using mDNS (multicast DNS). This is why you can reach your NAS at nasty.local instead of memorizing an IP address. Works out of the box on macOS and most Linux desktops. Windows may need Bonjour installed.',
				},
				{
					term: 'SMART',
					summary: 'Self-Monitoring, Analysis and Reporting Technology for disks.',
					detail: 'SMART is built into every modern disk drive. It tracks health indicators like temperature, error counts, and hours of operation. NASty monitors SMART data and can alert you when a disk shows signs of failure — often before data loss occurs.',
				},
				{
					term: 'Terminal',
					summary: 'A command-line shell running directly on NASty.',
					detail: 'The built-in terminal gives you a bash shell on the NAS, accessible from the web UI. Useful for running bcachefs commands, inspecting logs, or anything the web UI doesn\'t cover. Commands like nasty-top are available here.',
				},
				{
					term: 'Caddy',
					summary: 'The reverse proxy and TLS terminator running in front of the engine.',
					detail: 'Caddy serves the web UI on port 443, terminates HTTPS, and proxies /api/* and /ws/* to the engine on 127.0.0.1:2137. Certificates come from either Let\'s Encrypt (ACME) when you\'ve set a real domain, or from Caddy\'s built-in "internal" CA when you haven\'t — that\'s the self-signed cert you\'ll see on nasty.local and on the box\'s IP addresses. The engine talks to Caddy through its admin API on 127.0.0.1:2019 to push per-app routes and TLS automation policies at runtime, so app installs and TLS settings changes apply without restarting anything. Replaced nginx in 0.0.8. Logs: journalctl -u caddy.',
				},
				{
					term: 'ACME / Let\'s Encrypt',
					summary: 'Automatic TLS certificates for the web UI.',
					detail: 'NASty can request a free, trusted TLS certificate for your hostname from Let\'s Encrypt and renew it automatically. Two challenge types are supported: TLS-ALPN (works when NASty is reachable on port 443 from the internet) and DNS-01 (works behind a NAT / on a private network, but needs API credentials for your DNS provider). Issuance is handled by Caddy. Configure under Settings → TLS.',
				},
				{
					term: 'Tailscale',
					summary: 'Mesh VPN for reaching NASty from anywhere.',
					detail: 'Tailscale builds a private network between your devices over WireGuard. Once you log in from NASty\'s Settings page, your NAS gets a stable Tailscale IP and a *.ts.net hostname reachable from any of your other Tailscale-enabled machines — phone, laptop, server — without exposing it to the public internet. Useful for offsite backups and remote access.',
				},
				{
					term: 'UPS / NUT',
					summary: 'Talks to a battery backup so NASty can shut down cleanly on power loss.',
					detail: 'NUT (Network UPS Tools) lets NASty read state from a USB- or network-attached UPS. When the UPS reports low battery, NASty shuts down gracefully so you don\'t lose data to a hard power-off. Optional — enable it under Services if you have a UPS connected.',
				},
			],
		},
		{
			title: 'Security & Access',
			entries: [
				{
					term: 'Access Control',
					summary: 'Managing who can log into and administer NASty.',
					detail: 'NASty supports local user accounts for the web UI and SMB shares. Access control settings let you manage passwords, permissions, and authentication methods.',
				},
				{
					term: 'Token',
					summary: 'A credential used for API authentication.',
					detail: 'API tokens let external programs (scripts, CSI drivers, automation tools) authenticate with NASty without using a username and password. Tokens can be created and revoked from the Access Control page.',
				},
				{
					term: 'API',
					summary: 'Application Programming Interface — how software talks to NASty.',
					detail: 'NASty\'s engine exposes a JSON-RPC 2.0 API over WebSocket. Everything the web UI does goes through this API, and you can use it directly for scripting and automation. Connect to ws://<nasty-ip>/ws/api with a valid token.',
				},
				{
					term: 'SSO / OIDC',
					summary: 'Sign in to NASty with an external identity provider.',
					detail: 'OpenID Connect lets you delegate web UI login to a provider like Authentik, Keycloak, Google, or any OIDC-compliant IdP. Users log in once at the provider and are redirected back. Configure under Access Control → Identity Provider; existing local accounts keep working alongside SSO.',
				},
				{
					term: 'Audit Log',
					summary: 'Append-only record of every action operators take on the box.',
					detail: 'Lives at /var/lib/nasty/audit.log (mode 0600) and is mirrored to journald with target "audit", so tampering with the file still leaves a trail. Every state-changing RPC the engine accepts is recorded with the username, client IP, method name, and a safelist-filtered parameter summary (secrets like passwords / API tokens / TLS DNS credentials never make it in). Logged in addition to mutations: every login attempt (success and failure), permission denials, terminal / VM-console / log-stream opens, and unsafe app deploys — anything an auditor would want to reconstruct after the fact. Read it via the audit.list RPC or the Logs page in the WebUI; rotated by logrotate at 10 MB.',
				},
				{
					term: 'WebAuthn / Passkey / Security Key',
					summary: 'A non-password authentication factor — Touch ID, YubiKey, Windows Hello, etc.',
					detail: 'WebAuthn is a browser standard for proving who you are without a password. The "credential" lives on a device you control: a hardware key (YubiKey, Solo 2, Trezor — sometimes called a security key), a platform authenticator (Touch ID on a Mac, Windows Hello on a PC), or a syncable passkey (iCloud Keychain, 1Password, Bitwarden). NASty supports them as a third login backend alongside local password and SSO. Each credential is bound to one origin (a hostname like nasty.local) — moving NASty to a different hostname silently invalidates registered credentials, and IP-based access can never use them by spec. Register and manage your own under Access Control → Tokens & Keys.',
				},
				{
					term: 'TPM2',
					summary: 'A small chip on the motherboard that holds secrets and measurements.',
					detail: 'The Trusted Platform Module v2.0 is a discrete security chip (or firmware-emulated equivalent like Intel PTT, AMD fTPM, swtpm on QEMU) that stores keys in tamper-resistant hardware and can release them only when system state matches what you sealed against. NASty uses it to "seal" the bcachefs encryption key so an encrypted filesystem can auto-unlock at boot without the operator typing a passphrase — but only when the box looks the way it did at seal time. Without a TPM2 chip the auto-unlock can\'t work; manual passphrase entry stays as a fallback.',
				},
				{
					term: 'PCR (Platform Configuration Register)',
					summary: 'TPM-internal registers that record what booted, in a way that can\'t be rewound or faked.',
					detail: 'PCRs are 24 (or 32) hash values inside the TPM that accumulate measurements during boot. Every component — firmware, bootloader, kernel, initrd, key databases — gets hashed and "extended" into one of these registers. Once a value is extended you can\'t set it back; the only way for a PCR to read a given value is for the boot chain to produce that exact value organically. NASty seals encryption keys against specific PCRs: PCR-7 covers the Secure Boot policy (which keys the firmware trusts), so the seal opens only when the firmware is still trusting NASty\'s keys. Future work extends to PCR-4 (the bootloader + kernel binaries themselves) so the seal binds the whole boot chain, not just the policy.',
				},
				{
					term: 'Secure Boot',
					summary: 'Firmware-level signature checking on the bootloader, kernel, and initrd.',
					detail: 'When Secure Boot is on, the UEFI firmware refuses to launch any boot artifact that isn\'t signed by a key in its trust database. NASty\'s SB integration uses lanzaboote to bundle the kernel + initrd + cmdline into a signed PE stub the firmware verifies before handing off control. Enrollment is a one-time per-box ceremony (BIOS Setup Mode → NASty\'s platform key gets installed → next reboot enforces). Once enrolled, every kernel and initrd update is auto-signed on rebuild; an attacker booting an unsigned rescue image (memtest, live USB) fails at the firmware stage. SB also strengthens TPM2 sealing — without it PCR-7 is constant across stock NixOS installs, so a sealed key would unseal anywhere. Highly experimental in NASty today; see the Hardware page.',
				},
				{
					term: 'Setup Mode',
					summary: 'A UEFI firmware state where it accepts new platform keys without an existing signing chain.',
					detail: 'A fresh-from-factory or "PK-cleared" UEFI is in Setup Mode: PK (Platform Key) is empty, and the firmware will accept any key enrolled by the operating system without a higher-trust signature. Once a PK is enrolled the firmware leaves Setup Mode and starts enforcing the full SB chain. NASty\'s enrollment ceremony requires the operator to reset firmware to Setup Mode (via BIOS — vendor-specific path documented in the wizard) so that on the next boot, systemd-boot\'s auto-enrollment can install NASty\'s keys without needing a Microsoft-signed bridge. After enrollment, firmware exits Setup Mode automatically.',
				},
				{
					term: 'Measured UKI',
					summary: 'A Unified Kernel Image whose load is recorded into a PCR.',
					detail: 'A UKI bundles the kernel, initrd, and command line into a single PE binary. When the firmware loads it and Secure Boot is on, the firmware records the binary\'s hash into PCR-4 — so a different kernel produces a different PCR-4 reading. lanzaboote produces measured UKIs on every NixOS rebuild; bootctl status reports "Measured UKI: yes" when this is active. This is what lets future work seal keys against PCR-4 to bind the entire boot chain (not just the SB policy in PCR-7).',
				},
				{
					term: 'lanzaboote',
					summary: 'The NixOS-native Secure Boot toolchain.',
					detail: 'lanzaboote (https://github.com/nix-community/lanzaboote) replaces systemd-boot\'s normal install with a flow that signs every kernel + initrd + UKI for the firmware to verify. NASty pins lanzaboote v1.0.0 as a flake input and ships sbctl alongside as the read-only inspector. Pin and key management live entirely inside the NASty install — operators don\'t pick a lanzaboote rev (the protocol with sd-stub and the install-hook contract are nasty-test-matrix dependent). See the experimental Secure Boot enrollment wizard on the Hardware page.',
				},
				{
					term: 'sbctl',
					summary: 'CLI tool for inspecting Secure Boot state — keys, signatures, enrollment status.',
					detail: 'NASty includes sbctl on the system path so operators can inspect SB state by hand (`sbctl status`, `sbctl verify`, `sbctl list-enrolled-keys`). The engine itself uses it as a read-only inspector — signing and key enrollment go through lanzaboote, never direct sbctl writes. Run it from a terminal if you want raw vendor / key-fingerprint data the WebUI doesn\'t surface.',
				},
			],
		},
		{
			title: 'Backup',
			entries: [
				{
					term: 'Backup Profile',
					summary: 'A reusable definition of what to back up, where, and how often.',
					detail: 'A profile bundles a set of source paths (subvolumes or filesystem dirs), a target (local, S3, SFTP, REST, or Backblaze B2), an encryption password, a schedule, and a retention policy. Backups are deduplicated and incremental — only changed blocks travel over the network.',
				},
				{
					term: 'Retention',
					summary: 'How many snapshots to keep, by age class.',
					detail: 'A retention policy says e.g. "keep the last 7 snapshots, plus 7 daily, 4 weekly, 6 monthly." After every backup, snapshots that don\'t match any class are pruned. Tune this per profile based on how much history you want versus storage cost at the target.',
				},
			],
		},
		{
			title: 'Which Protocol Should I Use?',
			entries: [
				{
					term: 'I want to share files with Windows PCs',
					summary: 'Use SMB.',
				},
				{
					term: 'I want to share files with Linux servers or containers',
					summary: 'Use NFS.',
				},
				{
					term: 'I want to serve a virtual disk for a VM or database',
					summary: 'Use iSCSI (compatible) or NVMe-oF (fastest).',
				},
				{
					term: 'I want Kubernetes persistent volumes',
					summary: 'Use NFS for ReadWriteMany, iSCSI or NVMe-oF for ReadWriteOnce.',
				},
				{
					term: 'I want to stream media (Plex, Jellyfin)',
					summary: 'Use NFS or SMB — either works, NFS has less overhead.',
				},
				{
					term: 'I\'m not sure',
					summary: 'Start with SMB — it works with everything.',
				},
			],
		},
	];

	let expandedTerm = $state<string | null>(null);

	function toggle(term: string) {
		expandedTerm = expandedTerm === term ? null : term;
	}
</script>

<div class="space-y-6">
	<div>
		<h1 class="text-2xl font-bold">Help & Glossary</h1>
		<p class="text-sm text-muted-foreground mt-0.5">Storage terms, protocols, and guidance for getting started with NASty.</p>
	</div>

	{#each sections as section}
		<div>
			<h2 class="mb-3 text-lg font-semibold">{section.title}</h2>
			<div class="space-y-1.5">
				{#each section.entries as entry}
					{@const hasDetail = !!entry.detail}
					<Card class="overflow-hidden">
						<button
							class="w-full text-left px-4 py-3 flex items-start gap-3 {hasDetail ? 'cursor-pointer hover:bg-accent/50' : 'cursor-default'} transition-colors"
							onclick={() => hasDetail && toggle(entry.term)}
						>
							<div class="flex-1 min-w-0">
								<span class="font-medium">{entry.term}</span>
								<span class="ml-2 text-sm text-muted-foreground">{entry.summary}</span>
							</div>
							{#if hasDetail}
								<span class="text-xs text-muted-foreground mt-1 shrink-0">{expandedTerm === entry.term ? '−' : '+'}</span>
							{/if}
						</button>
						{#if expandedTerm === entry.term && entry.detail}
							<div class="border-t border-border bg-secondary/20 px-4 py-3 text-sm leading-relaxed text-muted-foreground">
								{entry.detail}
							</div>
						{/if}
					</Card>
				{/each}
			</div>
		</div>
	{/each}
</div>
