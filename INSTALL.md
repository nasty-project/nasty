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
lsblk
DISK=/dev/sda  # change to your target disk

# 3. Partition: EFI (512M) + root (20G) + data (rest)
parted -s "$DISK" -- \
  mklabel gpt \
  mkpart ESP fat32 1MiB 512MiB \
  set 1 esp on \
  mkpart root ext4 512MiB 20GiB \
  mkpart data 20GiB 100%

# 4. Format EFI and root partitions
mkfs.fat -F32 "${DISK}1"
mkfs.ext4 -F "${DISK}2"
# (data partition is left unformatted — create a bcachefs filesystem via the WebUI)

# 5. Mount
mount "${DISK}2" /mnt
mkdir -p /mnt/boot
mount "${DISK}1" /mnt/boot

# 6. Install Nix package manager
curl -L https://nixos.org/nix/install | sh -s -- --no-daemon
. ~/.nix-profile/etc/profile.d/nix.sh

# 7. Enable flakes
mkdir -p ~/.config/nix
echo "experimental-features = nix-command flakes" > ~/.config/nix/nix.conf

# 8. Install tools
nix profile install nixpkgs#nixos-install-tools nixpkgs#git

# 9. Clone NASty
git clone https://github.com/nasty-project/nasty.git /tmp/nasty

# 10. Generate hardware configuration for your machine
nixos-generate-config --root /mnt

# 11. Copy it into the NASty flake
cp /mnt/etc/nixos/hardware-configuration.nix /tmp/nasty/nixos/

# 12. Install NASty (this takes a while — downloads and builds the full system)
nixos-install --root /mnt \
  --flake /tmp/nasty/nixos#nasty \
  --no-root-passwd

# 13. Set root password
nixos-enter --root /mnt -c 'passwd root'

# 14. Done — reboot into NASty
reboot
```

After reboot, open `https://<nasty-ip>` and log in with **admin** / **admin**.

### Notes

- Step 2: make sure you pick the right disk — this will erase everything on it
- Step 3: if your root disk is small (<40GB), adjust the 20GiB root partition size
- Step 12: takes 10-30 minutes depending on your internet speed and hardware
- The data partition (`${DISK}3`) is intentionally left unformatted — create a bcachefs filesystem from the WebUI after first boot
