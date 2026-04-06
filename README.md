<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="webui/src/lib/assets/nasty-white.svg" />
    <source media="(prefers-color-scheme: light)" srcset="webui/src/lib/assets/nasty.svg" />
    <img src="webui/src/lib/assets/nasty-white.svg" width="300" alt="NASty" />
  </picture>
</p>

<p align="center">
  <strong>A NAS appliance built on bcachefs.</strong>
</p>

---

NASty is a NAS operating system built on NixOS and bcachefs. It turns commodity hardware into a storage appliance serving NFS, SMB, iSCSI, and NVMe-oF — managed from a single web UI, updated atomically, and rolled back when things go sideways.

## Features

- **bcachefs** — compression, checksumming, erasure coding, tiering, encryption, O(1) snapshots
- **File sharing** — NFS, SMB 
- **Block storage** — iSCSI, NVMe-oF
- **Web UI** — manage filesystems, subvolumes, snapshots, shares, disks, VMs, and more
- **Web terminal** — built-in shell access from the browser
- **Virtual machines** — QEMU/KVM with VNC console (experimental)
- **Apps** — k3s-based container runtime (experimental)
- **Alerts** — configurable rules for filesystem usage, disk health, temperatures
- **Let's Encrypt** — automatic TLS certificates
- **Kubernetes integration** — CSI driver for dynamic volume provisioning across all 4 protocols
- **Atomic updates** — NixOS-based, with one-click rollback to any previous generation
- **File browser** — browse and manage files from the web UI

## Screenshots

<p align="center">
  <img src="images/dashboard.jpg" width="800" alt="Dashboard — system overview with CPU, memory, storage, and network stats" />
</p>
<p align="center"><em>Dashboard</em></p>

<p align="center">
  <img src="images/filesystems.jpg" width="800" alt="Filesystems — bcachefs filesystem with 3 devices, scrub status, and per-device actions" />
</p>
<p align="center"><em>Filesystems</em></p>

<p align="center">
  <img src="images/subvolumes.jpg" width="800" alt="Subvolumes — list with snapshots, block devices, and clone relationships" />
</p>
<p align="center"><em>Subvolumes</em></p>

<p align="center">
  <img src="images/sharing.jpg" width="800" alt="Sharing — iSCSI targets with portals, LUNs, and ACLs" />
</p>
<p align="center"><em>Sharing</em></p>

<p align="center">
  <img src="images/update.jpg" width="800" alt="Update — NixOS atomic updates with flavor selection and build progress" />
</p>
<p align="center"><em>Updates</em></p>

<p align="center">
  <img src="images/terminal.jpg" width="800" alt="Terminal — built-in web shell with bcachefs tools" />
</p>
<p align="center"><em>Terminal</em></p>

## Getting Started

1. Download the latest ISO from [Releases](../../releases)
2. Boot it on your hardware — the installer lets you pick a disk and press Enter
3. Open the WebUI at `https://<nasty-ip>`
4. Default credentials: **admin** / **admin**

ISO won't boot? Some UEFI firmware doesn't like NixOS ISOs. See [INSTALL.md](INSTALL.md) for an alternative installation method from any Linux live environment.

## Update Flavors

NASty has three update flavors:

| Flavor | What you get | Description |
|--------|-------------|-------------|
| **Mild** | Tagged stable releases (`v0.0.1`) | Stable releases |
| **Spicy** | Pre-release builds (`s0.0.1`) | Pre-release builds with newer features |
| **Nasty** | Latest commit on main | Bleeding edge, no guarantees |

Switch flavors from **Settings → Update → Flavor** in the WebUI.

## Architecture

| Component | Technology | Why |
|-----------|------------|-----|
| Engine | Rust | Async runtime, handles all storage and system operations |
| Web UI | SvelteKit + TypeScript | Reactive UI with real-time WebSocket updates |
| OS | NixOS | Atomic updates, rollback, reproducible system config |
| Filesystem | bcachefs | Checksumming, compression, tiering, snapshots, erasure coding |
| API | JSON-RPC 2.0 over WebSocket | Persistent connection, bidirectional, low overhead |

## Project Structure

```
engine/         Rust workspace — storage, sharing, system management
webui/          SvelteKit web interface
nixos/          NixOS modules and ISO configuration
```

The full ecosystem (CSI driver, Helm chart, kubectl plugin, and more) lives at [github.com/nasty-project](https://github.com/nasty-project).

## FAQ

See [FAQ.md](FAQ.md) for common questions about bcachefs, NixOS, and project status.

## Telemetry

NASty collects anonymous usage data (drive count and storage capacity). Disable anytime from **Settings → Telemetry**. Details: [nasty-telemetry](https://github.com/nasty-project/nasty-telemetry).

## License

GPLv3
