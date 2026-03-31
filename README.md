<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="webui/src/lib/assets/nasty-white.svg" />
    <source media="(prefers-color-scheme: light)" srcset="webui/src/lib/assets/nasty.svg" />
    <img src="webui/src/lib/assets/nasty-white.svg" width="300" alt="NASty" />
  </picture>
</p>

<p align="center">
  <strong>A modern NAS appliance built on bcachefs.</strong><br>
  Designed for homelabs and small teams.
</p>

---

NASty is a self-contained NAS operating system that turns commodity hardware into a full-featured storage appliance. It combines bcachefs (the most exciting Linux filesystem in years) with NixOS (atomic updates, instant rollback) and a web-based management interface.

## Features

- **bcachefs filesystems** — compression, checksumming, erasure coding, tiering, encryption, O(1) snapshots
- **File sharing** — NFS, SMB, iSCSI, NVMe-oF — all managed from one UI
- **Web UI** — manage filesystems, subvolumes, snapshots, shares, disks, VMs, and more
- **Web terminal** — built-in shell access from the browser
- **Virtual machines** — QEMU/KVM with browser-based VNC console
- **Apps** — run containerized services on the appliance
- **Alerts** — configurable rules for filesystem usage, disk health, temperatures
- **Kubernetes integration** — CSI driver for dynamic volume provisioning across all 4 protocols
- **Atomic updates** — NixOS-based, with one-click rollback to any previous generation
- **File browser** — browse and manage files on your filesystems from the web UI

## Getting Started

Download the latest ISO from [Releases](../../releases) and boot it on your hardware. The installer walks you through disk selection and initial setup.

Pre-built QCOW2 cloud images (x86_64 and aarch64) are available in Releases for VM-based testing.

Default credentials: **admin** / **admin**

## Architecture

| Component | Technology |
|-----------|------------|
| Engine | Rust (tokio + axum), JSON-RPC 2.0 over WebSocket |
| Web UI | SvelteKit + TypeScript |
| OS | NixOS |
| Filesystem | bcachefs |

## Related Projects

| Repository | Description |
|------------|-------------|
| [nasty-csi](https://github.com/nasty-project/nasty-csi) | Kubernetes CSI driver |
| [nasty-chart](https://github.com/nasty-project/nasty-chart) | Helm chart for the CSI driver |
| [nasty-go](https://github.com/nasty-project/nasty-go) | Go client library for the NASty API |
| [nasty-plugin](https://github.com/nasty-project/nasty-plugin) | kubectl plugin (`kubectl nasty`) |

## FAQ

See [FAQ.md](FAQ.md) — covers why NASty exists, why bcachefs over ZFS, why NixOS, production readiness, and more.

## License

GPLv3
