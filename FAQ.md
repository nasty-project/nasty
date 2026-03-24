# FAQ

## Why does NASty exist?

Because bcachefs deserves a proper NAS appliance, and nobody was building one.

bcachefs is the most exciting Linux filesystem in years — native COW snapshots, checksums, compression, tiering, erasure coding, and encryption, all in one filesystem that doesn't require you to learn a PhD-worth of ZFS terminology. But until NASty, using bcachefs for NAS meant SSH and CLI commands.

NASty wraps bcachefs in a real appliance: WebUI, 4-protocol sharing (NFS, SMB, iSCSI, NVMe-oF), Kubernetes CSI driver, and NixOS for atomic system updates with rollback.

## Why bcachefs instead of ZFS?

ZFS is battle-tested and great. We're not here to trash it. But:

- **bcachefs is GPL Linux.** No CDDL license friction. Kernel module builds are fully automated on NixOS — no manual DKMS steps.
- **Simpler model.** A "filesystem" is just a filesystem. Subvolumes are just directories. Snapshots are just snapshots. No datasets, zvols, pools-within-pools, or property inheritance trees.
- **Modern features out of the box.** Tiering (move cold data to slow disks automatically), erasure coding, and online filesystem repair — things that ZFS either doesn't have or requires third-party tools.
- **Active development.** Kent Overstreet is shipping features at a pace ZFS hasn't seen in years.

The tradeoff: bcachefs is younger and less proven. We're comfortable with that for a project that's explicitly exploring what's next.

## Why NixOS?

Because a NAS appliance should be a single atomic unit that you can update, roll back, and reproduce.

- **Atomic updates.** `nixos-rebuild switch` either succeeds completely or doesn't change anything. No "halfway upgraded" state.
- **Rollback.** Every update creates a new generation. Boot into the previous one if something breaks.
- **Reproducible.** The entire system is defined in code. Two machines with the same config are identical.
- **No package manager conflicts.** Nix handles all dependencies in isolation. No `apt upgrade` breaking your storage engine.

Traditional NAS distros (FreeNAS/TrueNAS, OpenMediaVault) use FreeBSD or Debian with mutable package management. NASty uses NixOS because a storage appliance should be the last thing that breaks during an update.

## Is this production-ready?

No. NASty is experimental and under active development. bcachefs itself is still maturing.

That said, NASty is probably the most thoroughly tested one-person NAS project you'll find:

- **132 end-to-end integration tests** across all 4 protocols (NFS, SMB, iSCSI, NVMe-oF) — snapshots, clones, volume expansion, crash simulation, concurrent operations, StatefulSets, access modes, adoption, and more
- **76 CSI sanity tests** verifying spec compliance
- Tests run against a real NASty instance on Oracle Cloud ARM (not mocks) with a real k3s cluster provisioning real volumes over real network protocols
- CI/CD pipeline builds, lints, tests, and publishes container images automatically
- Tested by an elite team consisting of me, myself, I, my alter ego, my evil twin, my inner child, my future self, my past self, my impostor syndrome, my caffeine-fueled persona, and a rubber duck named QA

Use it for homelabs, development, and learning. Not for storing your only copy of irreplaceable data. Yet.

## What's "vibecoded" mean?

NASty is developed with heavy AI assistance — architecture discussions, code generation, debugging, test analysis, and documentation are all done collaboratively with Claude. The human (Bartosz) makes the decisions; the AI does the heavy lifting.

This isn't a toy or a demo. It's a serious project built faster than a single developer could manage alone. The vibecoding approach lets one person build what would normally require a team — a full NAS appliance with engine, WebUI, CSI driver, NixOS integration, and CI/CD pipeline.

The same approach was used to build [tns-csi](https://github.com/fenio/tns-csi), a TrueNAS CSI driver that has an active userbase running it in production without issues. NASty's CSI driver evolved from that codebase.

## What protocols does NASty support?

- **NFS** — Network File System. Standard Linux/Unix file sharing.
- **SMB** — Server Message Block. Windows/macOS compatible file sharing.
- **iSCSI** — Internet SCSI. Block storage over TCP. Used by Kubernetes for persistent volumes.
- **NVMe-oF** — NVMe over Fabrics. High-performance block storage over TCP. The modern alternative to iSCSI.

All four protocols are managed through the same WebUI and API. The Kubernetes CSI driver supports all four.

## How does the Kubernetes integration work?

NASty includes a CSI (Container Storage Interface) driver that lets Kubernetes provision storage directly on NASty:

1. Admin creates a StorageClass pointing to NASty
2. User creates a PVC
3. CSI driver calls NASty's JSON-RPC API to create a subvolume and share
4. Kubernetes mounts the share into the pod

Supported features: dynamic provisioning, snapshots, clones, volume expansion, multiple access modes, and volume adoption (re-attaching volumes after cluster rebuild).

## How are snapshots and clones different from ZFS?

Simpler.

In bcachefs, a snapshot IS a subvolume. It's a first-class citizen, not a dependent child of its parent. Delete the parent — the snapshot survives. No "promote", no "detach", no dependency chains.

A clone is just a writable snapshot. One command: `bcachefs subvolume snapshot` (without `-r`). Instant, COW, fully independent. No clone modes, no send/receive for independence.

## Can I migrate from TrueNAS/ZFS?

Not directly. ZFS and bcachefs are different filesystems — there's no in-place migration. You'd need to:

1. Set up NASty on new hardware (or additional disks)
2. Copy data from TrueNAS to NASty via rsync, NFS, or SMB
3. Point your Kubernetes cluster at NASty

The CSI driver's volume adoption feature can help with step 3 — it can adopt existing subvolumes on NASty without re-creating PVCs from scratch.

## Where does the name come from?

**N**ixOS **A**ppliance for **S**torage, **T**iering, and... well, it's a backronym. The name came first. It's a NAS that's a bit nasty — experimental, opinionated, and not afraid to break things in pursuit of something better.
