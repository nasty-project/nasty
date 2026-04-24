# FAQ

## Why does NASty exist?

Because bcachefs deserves a proper NAS appliance, and nobody was building one.

bcachefs is arguably the most interesting Linux filesystem in years. But using it for NAS meant CLI-only. NASty wraps it in an appliance with a web UI, NFS/SMB/iSCSI/NVMe-oF sharing, a Kubernetes CSI driver, and NixOS for atomic updates.

## Why bcachefs instead of ZFS?

ZFS is battle-tested and great. I'm not here to trash it. But:

- **bcachefs is GPL.** Fits Linux perfectly. And with NASty you don't even need to know what DKMS is.
- **Simpler model.** A "filesystem" is just a filesystem. Subvolumes are just directories. Snapshots are just snapshots. No datasets, zvols, pools-within-pools, or property inheritance trees.
- **Modern features out of the box.** Tiering (move cold data to slow disks automatically), erasure coding, and online filesystem repair — things that ZFS either doesn't have or requires third-party tools.
- **Active development.** Kent Overstreet is shipping features at a pace ZFS hasn't seen in years.

The tradeoff: bcachefs is younger and less proven. I'm comfortable with that for a project that's explicitly exploring what's next.

## Why NixOS?

Because a NAS appliance should be a single atomic unit that you can update, roll back, and reproduce.

- **Atomic updates.** `nixos-rebuild switch` either succeeds completely or doesn't change anything. No "halfway upgraded" state.
- **Rollback.** Every update creates a new generation. Boot into the previous one if something breaks.
- **Reproducible.** The entire system is defined in code. Two machines with the same config are identical.
- **No package manager conflicts.** Nix handles all dependencies in isolation. No `pacman -Syu` breaking your storage engine at 2am.

Traditional NAS distros (FreeNAS/TrueNAS, OpenMediaVault) use FreeBSD or Debian with mutable package management. NASty uses NixOS because a storage appliance should be the last thing that breaks during an update.

## Is this production-ready?

No. NASty is experimental and under active development. bcachefs itself is still maturing.

That said, NASty is probably the most thoroughly tested one-person NAS project you'll find:

- **170 Kubernetes E2E tests** — real cluster provisioning real volumes over all four network protocols, including snapshots, clones, and scale tests
- **362 CSI driver unit tests** — covering node staging, volume lifecycle, health monitoring, and recovery
- **76 CSI sanity tests** — spec compliance verification
- **Integration test suite** — exercising the engine API across all protocols, snapshots, clones, and data integrity
- CI/CD pipeline builds, lints, tests, and publishes container images automatically

Use it for homelabs, development, and learning. Not for storing your only copy of irreplaceable data. Yet.

## What protocols does NASty support?

- **NFS** — Network File System. Standard Linux/Unix file sharing.
- **SMB** — Server Message Block. Windows/macOS file sharing over the network.
- **iSCSI** — Internet SCSI. Block storage over TCP. Used by Kubernetes for persistent volumes.
- **NVMe-oF** — NVMe over Fabrics. High-performance block storage over TCP. The modern alternative to iSCSI.

All four protocols are managed through the same WebUI and API. The Kubernetes CSI driver supports all four.

## How are snapshots and clones different from ZFS?

Simpler.

In bcachefs, a snapshot IS a subvolume. It's a first-class citizen, not a dependent child of its parent. Delete the parent — the snapshot survives. No "promote", no "detach", no dependency chains.

A clone is just a writable snapshot. One command: `bcachefs subvolume snapshot` (without `-r`). Instant, COW, fully independent. No clone modes, no send/receive for independence.

## What about VMs and Apps?

They work, but both are early-stage.

VMs use QEMU/KVM with a noVNC console in the browser. You can create a VM, boot an ISO, and use it. It won't replace Proxmox, but it handles simple workloads.

Apps run on Docker. You can deploy single containers or full Compose stacks from the web UI. The management interface is basic.

Both features are under active development. Contributions in these areas would have outsized impact.

## How can I help?

Try it. Break it. Tell me what sucks. Open issues. Send patches. Or just use it and let the telemetry tell me you exist — that alone is motivating.

NASty is a small project and always looking for contributors. Whether you're into Rust, SvelteKit, NixOS, Kubernetes, bcachefs, or just want a NAS that doesn't feel like it was designed in 2005 — there's something here for you.

The best way to start:
- **Use it** — install on spare hardware, play with it, find the rough edges
- **File issues** — even "this confused me" is valuable feedback
- **Join the conversation** — bcachefs IRC on OFTC (`#bcachefs`) or [Matrix](https://matrix.to/#/#_oftc_%23bcache:matrix.org)
- **Contribute code** — pick an issue, send a PR, or just improve something that bothers you

No contribution is too small. Documentation fixes, typo corrections, better error messages — it all counts.

## Where does the name come from?

NAS + ty. It's the only English word with "NAS" in it that I could think of. Maybe there are others. I don't care. Just a NAS that's a bit nasty.
