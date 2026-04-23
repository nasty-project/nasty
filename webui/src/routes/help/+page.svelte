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
