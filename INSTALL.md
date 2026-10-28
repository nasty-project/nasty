# Installation

## Standard Installation (ISO)

1. Download the latest ISO from [Releases](../../releases)
2. Write it to a USB stick: `sudo dd if=nasty-*.iso of=/dev/sdX bs=4M status=progress`
3. Boot from USB
4. Follow the installer prompts
5. Open the WebUI at `https://<nasty-ip>`
6. Default credentials: **admin** / **admin**

## Alternative Installation (from any Linux live environment)

If the NASty ISO does not boot on your hardware, install from a current
SystemRescue, Ubuntu, Debian, or other Linux live environment. The packaged
installer uses the same machine-local wrapper and release inputs as the NASty
ISO. It supports SATA, NVMe, virtio, and eMMC whole disks without manually
constructing partition names.

Boot the live environment in the mode you want to install: UEFI on either
supported architecture, or legacy BIOS on x86_64. Secure Boot must be disabled
for the initial UEFI installation. BIOS installs do not support UEFI Secure Boot.

### Requirements

- A working internet connection
- A 64-bit x86_64 or aarch64 Linux live environment
- Root access, `curl`, user-management tools, and a writable `/nix`
- Target disk (all data will be erased)

### Steps

Boot the live environment and open a root shell. Check its firmware mode and
identify the target whole disk:

```bash
if test -d /sys/firmware/efi; then echo UEFI; else echo BIOS; fi
lsblk -dp -o NAME,SIZE,MODEL,SERIAL,TRAN,TYPE,MOUNTPOINTS
```

Install the Nix package manager if the live system does not already provide it.
The official single-user installer needs the standard `nixbld` build users when
it is run as root:

```bash
groupadd -r nixbld 2>/dev/null || true
for i in $(seq 1 10); do
  useradd -r -g nixbld -G nixbld -d /var/empty \
    -s "$(command -v nologin || echo /bin/false)" "nixbld$i" 2>/dev/null || true
done
curl -L https://nixos.org/nix/install | sh -s -- --no-daemon --yes
. /root/.nix-profile/etc/profile.d/nix.sh
```

The helper installs in UEFI mode or, on x86_64, legacy BIOS mode (without Secure
Boot). The boot mode is determined by how you boot the live system;
`--boot-mode auto` is the default, and an explicit `uefi` or `bios` must match.

Start with a dry run. It checks the boot mode, Secure Boot, target-disk safety, release
resolution, the wrapper lock, and NixOS evaluation without modifying the disk:

```bash
INSTALLER_REF=v0.1.2
DISK=/dev/nvme0n1

nix --extra-experimental-features 'nix-command flakes' run \
  "github:nasty-project/nasty/${INSTALLER_REF}#installer" -- \
  --disk "$DISK" --mode whole --dry-run
```

If the dry run succeeds, repeat without `--dry-run`:

```bash
nix --extra-experimental-features 'nix-command flakes' run \
  "github:nasty-project/nasty/${INSTALLER_REF}#installer" -- \
  --disk "$DISK" --mode whole
```

Use `--mode split` to create a 50 GiB OS partition and leave the remainder as
an unformatted third partition. The installer uses DHCP for the installed
system; configure static networking from the WebUI after first boot.

After reboot, open `https://<nasty-ip>` and log in with **admin** / **admin**.
The first login requires changing that password.

### Notes

- Prefer `--mode whole` with separate data disks.
- The exact target path must be confirmed before erasure. Mounted, read-only,
  active-swap, and stacked LVM/RAID/crypt devices are rejected.
- Installation usually takes 10-30 minutes, depending on network and hardware.
- In split mode, partition 3 is intentionally left unformatted. Create the
  bcachefs filesystem from the WebUI after first boot.
- BIOS installation uses a small GPT BIOS-boot partition and GRUB; UEFI uses a
  FAT32 EFI partition and systemd-boot. The installer saves its chosen mode in
  `/etc/nixos/nasty-installer-boot.nix` so updates preserve the bootloader. To change modes,
  reinstall rather than changing a WebUI setting.
- The initial installation requires network access to GitHub, the Nix cache,
  and `nasty.cachix.org`.
