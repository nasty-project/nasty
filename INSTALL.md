# Installation

## Standard Installation (ISO)

1. Download the latest ISO from [Releases](../../releases)
2. Write it to a USB stick: `sudo dd if=nasty-*.iso of=/dev/sdX bs=4M status=progress`
3. Boot from USB
4. Follow the installer prompts
5. Open the WebUI at `https://<nasty-ip>`
6. Default credentials: **admin** / **admin**

## Alternative Installation (from any Linux live environment)

If the NASty ISO doesn't boot on your hardware (some UEFI firmware is picky about NixOS ISOs), you can install from any Linux live environment — SystemRescueCD, Ubuntu live USB, Debian installer shell, etc.

### Requirements

- A working internet connection
- A Linux live environment with `curl` and `parted`
- Target disk (all data will be erased)

### Steps

Boot your live environment and get to a root shell, then:

```bash
# 1. Verify networking
ping -c1 github.com

# 2. Identify your target disk
lsblk -d
```

Pick your target disk. For a standard SATA/NVMe disk:

```bash
DISK=/dev/sda
PART1="${DISK}1"
PART2="${DISK}2"
PART3="${DISK}3"
```

For eMMC (e.g. ODROID H3):

```bash
DISK=/dev/mmcblk0
PART1="${DISK}p1"
PART2="${DISK}p2"
PART3="${DISK}p3"
```

Then proceed with the installation:

```bash
# 3. Partition
#    Option A: Dedicated NAS disk (EFI + root, no data partition — use separate disks for storage)
parted -s "$DISK" -- \
  mklabel gpt \
  mkpart ESP fat32 1MiB 512MiB \
  set 1 esp on \
  mkpart root ext4 512MiB 100%

#    Option B: Single disk (EFI + root + data partition for bcachefs)
parted -s "$DISK" -- \
  mklabel gpt \
  mkpart ESP fat32 1MiB 512MiB \
  set 1 esp on \
  mkpart root ext4 512MiB 20GiB \
  mkpart data 20GiB 100%

# 4. Format EFI and root partitions
mkfs.fat -F32 "$PART1"
mkfs.ext4 -F "$PART2"
# (data partition, if created, is left unformatted — create a bcachefs filesystem via the WebUI)

# 5. Mount
mount "$PART2" /mnt
mkdir -p /mnt/boot
mount "$PART1" /mnt/boot

# 6. Prepare Nix build users (required for Nix installation as root)
groupadd -r nixbld 2>/dev/null || true
for i in $(seq 1 10); do
  useradd -r -g nixbld -G nixbld -d /var/empty -s /sbin/nologin "nixbld$i" 2>/dev/null || true
done

# 7. Install Nix package manager
curl -L https://nixos.org/nix/install | sh -s -- --no-daemon --yes
. /root/.nix-profile/etc/profile.d/nix.sh

# 8. Enable flakes
mkdir -p ~/.config/nix
echo "experimental-features = nix-command flakes" > ~/.config/nix/nix.conf

# 9. Install tools
nix profile install nixpkgs#nixos-install-tools nixpkgs#git

# 10. Clone NASty directly into the target's /etc/nixos
git clone https://github.com/nasty-project/nasty.git /mnt/etc/nixos

# 11. Generate hardware configuration for your machine
nixos-generate-config --root /mnt --dir /tmp/hw-config

# 12. Copy it into the NASty flake
cp /tmp/hw-config/hardware-configuration.nix /mnt/etc/nixos/nixos/

# 13. Install NASty (this takes 10-30 minutes)
nixos-install --root /mnt \
  --flake /mnt/etc/nixos/nixos#nasty \
  --no-root-passwd

# 14. Set root password
nixos-enter --root /mnt -c 'echo "root:yourpassword" | /run/current-system/sw/bin/chpasswd'

# 15. Reboot (remove the USB stick)
reboot
```

After reboot, open `https://<nasty-ip>` and log in with **admin** / **admin**.

### Notes

- Step 2: make sure you pick the right disk — this will erase everything on it
- Step 3: use Option A if you have separate disks for storage (recommended). Use Option B for single-disk setups.
- Step 13: takes 10-30 minutes depending on your internet speed and hardware
- The data partition (if created) is intentionally left unformatted — create a bcachefs filesystem from the WebUI after first boot
