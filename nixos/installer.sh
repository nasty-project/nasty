#!/usr/bin/env bash

set -euo pipefail

TARGET_ROOT=/mnt
DISK=""
PART_MODE=""
BOOT_MODE_REQUESTED=auto
ASSUME_YES=0
DRY_RUN=0
NO_REBOOT=0
STAGE_DIR=""
ROOT_MOUNTED=0
BOOT_MOUNTED=0

usage() {
  cat <<'EOF'
Usage: nasty-install [options]

Install NASty from a Linux live environment.

Options:
  --disk PATH       Target whole disk, for example /dev/nvme0n1
  --mode MODE       "whole" or "split"
  --boot-mode MODE  "auto" (default), "uefi", or "bios"; on aarch64, "uefi"
                    also works from a non-UEFI live system (U-Boot)
  --yes             Skip the final destructive confirmation
  --dry-run         Validate the host, disk, release lock, and evaluation only
  --no-reboot       Return to the live shell after installation
  -h, --help        Show this help

The split layout creates a 50 GiB OS partition and leaves partition 3
unformatted for later use through the NASty WebUI.
EOF
}

die() {
  echo "Error: $*" >&2
  exit 1
}

cleanup() {
  set +e
  if [ "$BOOT_MOUNTED" -eq 1 ] && mountpoint -q "$TARGET_ROOT/boot"; then
    umount "$TARGET_ROOT/boot"
  fi
  if [ "$ROOT_MOUNTED" -eq 1 ] && mountpoint -q "$TARGET_ROOT"; then
    umount "$TARGET_ROOT"
  fi
  if [ -n "$STAGE_DIR" ] && [ -d "$STAGE_DIR" ]; then
    rm -rf "$STAGE_DIR"
  fi
}

trap cleanup EXIT INT TERM

while [ "$#" -gt 0 ]; do
  case "$1" in
    --disk)
      [ "$#" -ge 2 ] || die "--disk requires a path"
      DISK=$2
      shift 2
      ;;
    --mode)
      [ "$#" -ge 2 ] || die "--mode requires whole or split"
      PART_MODE=$2
      shift 2
      ;;
    --boot-mode)
      [ "$#" -ge 2 ] || die "--boot-mode requires auto, uefi, or bios"
      BOOT_MODE_REQUESTED=$2
      shift 2
      ;;
    --yes)
      ASSUME_YES=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --no-reboot)
      NO_REBOOT=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

[ "$(id -u)" -eq 0 ] || die "nasty-install must be run as root"
mountpoint -q "$TARGET_ROOT" && die "$TARGET_ROOT is already a mount point"
mountpoint -q "$TARGET_ROOT/boot" && die "$TARGET_ROOT/boot is already a mount point"

case "$(uname -m)" in
  x86_64) LOCAL_SYSTEM=x86_64-linux ;;
  aarch64) LOCAL_SYSTEM=aarch64-linux ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac
[ "$LOCAL_SYSTEM" = "$NASTY_INSTALL_SYSTEM" ] \
  || die "installer is for $NASTY_INSTALL_SYSTEM, but this host is $LOCAL_SYSTEM"

: "${NASTY_INSTALL_BOOT_HELPER:?installer boot helper is missing}"
# Never allow a caller-supplied test path to override real firmware detection
# during a destructive install.
[ "$DRY_RUN" -eq 1 ] || unset NASTY_EFI_DIR
# shellcheck source=nixos/installer-boot.sh
source "$NASTY_INSTALL_BOOT_HELPER"
BOOT_MODE=$(nasty_select_boot_mode "$BOOT_MODE_REQUESTED" "$LOCAL_SYSTEM") \
  || die "cannot install in the requested firmware mode"

shopt -s nullglob
secure_boot_vars=(/sys/firmware/efi/efivars/SecureBoot-*)
if [ "${#secure_boot_vars[@]}" -gt 0 ]; then
  secure_boot=$(od -An -t u1 -j 4 -N 1 "${secure_boot_vars[0]}" | tr -d ' ')
  [ "$secure_boot" != 1 ] || die "disable Secure Boot for the initial installation"
fi
shopt -u nullglob

list_disks() {
  echo "Available writable whole disks:"
  while read -r path size type ro; do
    [ "$type" = disk ] || continue
    [ "$ro" = 0 ] || continue
    model=$(lsblk -dn -o MODEL "$path" | xargs)
    serial=$(lsblk -dn -o SERIAL "$path" | xargs)
    printf '  %-20s %-8s  %s%s\n' \
      "$path" "$size" "${model:-unknown model}" "${serial:+ [$serial]}"
  done < <(lsblk -bdpno NAME,SIZE,TYPE,RO | numfmt --field=2 --to=iec-i --suffix=B)
}

echo "=== NASty live-system installer ==="
echo "Boot mode: $BOOT_MODE (detected from the live environment)"
if [ "$BOOT_MODE" = bios ]; then
  echo "Legacy BIOS uses GRUB and a GPT BIOS-boot partition; UEFI Secure Boot is unavailable."
else
  echo "UEFI uses systemd-boot and a FAT32 EFI partition."
fi
echo
list_disks
echo

if [ -z "$DISK" ]; then
  read -r -p "Target whole disk (for example /dev/nvme0n1): " DISK
fi

[ -b "$DISK" ] || die "$DISK is not a block device"
DISK=$(readlink -f "$DISK")
DISK_TYPE=$(lsblk -dn -o TYPE "$DISK")
if [ "$DISK_TYPE" != disk ]; then
  [ "${NASTY_INSTALL_ALLOW_LOOP:-0}" = 1 ] && [ "$DISK_TYPE" = loop ] \
    || die "$DISK is not a whole disk"
fi
[ "$(lsblk -dn -o RO "$DISK")" = 0 ] || die "$DISK is read-only"

DISK_SIZE_B=$(lsblk -bdn -o SIZE "$DISK")
DISK_SIZE_G=$((DISK_SIZE_B / 1073741824))

mounted=$(lsblk --json -p -o MOUNTPOINTS "$DISK" \
  | jq -r '[.. | objects | .mountpoints? // empty | .[]? | select(. != null)] | length')
[ "$mounted" = 0 ] || die "$DISK or one of its partitions is mounted or active swap"

unsupported_children=$(lsblk -nrpo TYPE "$DISK" \
  | awk 'NR > 1 && $1 != "part" { print; found=1 } END { exit found ? 0 : 1 }' || true)
[ -z "$unsupported_children" ] \
  || die "$DISK has active stacked devices: $(echo "$unsupported_children" | tr '\n' ' ')"

if [ -z "$PART_MODE" ]; then
  echo "Partitioning mode:"
  echo "  1) whole - use the disk for the OS; use separate data disks"
  echo "  2) split - 50 GiB OS; leave the remainder unformatted"
  read -r -p "Choose [1/2]: " choice
  case "$choice" in
    1) PART_MODE="whole" ;;
    2) PART_MODE="split" ;;
    *) die "invalid partitioning mode" ;;
  esac
fi

case "$PART_MODE" in
  whole)
    [ "$DISK_SIZE_B" -ge 17179869184 ] \
      || die "whole-disk installation requires at least 16 GiB"
    ;;
  split)
    [ "$DISK_SIZE_B" -ge 55834574848 ] \
      || die "split installation requires at least 52 GiB"
    if [ "$DISK_SIZE_G" -lt 70 ]; then
      echo "Warning: only about $((DISK_SIZE_G - 50)) GiB will remain for data."
    fi
    ;;
  *) die "--mode must be whole or split" ;;
esac

DISK_IDENTITY=$(lsblk --json -b -d -o PATH,TYPE,SIZE,RO,SERIAL,WWN "$DISK" \
  | jq -c '.blockdevices[0] | {path,type,size,ro,serial,wwn}')

echo
echo "Selected disk:"
lsblk -d -o NAME,SIZE,MODEL,SERIAL,WWN,TRAN "$DISK"
echo "Boot mode: $BOOT_MODE"
if [ "$BOOT_MODE" = bios ]; then
  echo "Partition layout: 2 MiB BIOS-boot, ext4 root, optional unformatted data."
else
  echo "Partition layout: 511 MiB EFI, ext4 root, optional unformatted data."
fi
echo
echo "Existing signatures:"
wipefs "$DISK" || true
while read -r child; do
  wipefs "$child" || true
done < <(lsblk -nrpo NAME,TYPE "$DISK" | awk '$2 == "part" { print $1 }')

echo
echo "==> Resolving and evaluating the NASty release before disk changes..."
STAGE_DIR=$(mktemp -d -t nasty-install.XXXXXX)
cp "$NASTY_SYSTEM_FLAKE/networking.nix" "$STAGE_DIR/"
cp "$NASTY_SYSTEM_FLAKE/flake.nix" "$STAGE_DIR/"
nasty_write_boot_module "$STAGE_DIR/nasty-installer-boot.nix" "$BOOT_MODE" "$DISK" \
  || die "could not configure the bootloader"
cat > "$STAGE_DIR/hardware-configuration.nix" <<'EOF'
# Evaluation-only placeholder. The installer replaces this after partitioning.
{ ... }:
{
  fileSystems."/" = {
    device = "/dev/disk/by-label/NASTY_ROOT";
    fsType = "ext4";
  };
EOF
if [ "$BOOT_MODE" = uefi ]; then
  # The evaluation stub must match the filesystem layout that will be
  # generated after partitioning; BIOS uses /boot on the ext4 root.
  cat >> "$STAGE_DIR/hardware-configuration.nix" <<'EOF'
  fileSystems."/boot" = {
    device = "/dev/disk/by-label/NASTY_EFI";
    fsType = "vfat";
  };
EOF
fi
printf '}\n' >> "$STAGE_DIR/hardware-configuration.nix"
nix --extra-experimental-features 'nix-command flakes' flake lock "$STAGE_DIR"
nix --extra-experimental-features 'nix-command flakes' eval --raw \
  "$STAGE_DIR#nixosConfigurations.nasty.config.system.build.toplevel.drvPath" >/dev/null

if [ "$DRY_RUN" -eq 1 ]; then
  echo "Dry run passed. No disk changes were made."
  exit 0
fi

if [ "$ASSUME_YES" -ne 1 ]; then
  echo
  echo "WARNING: this will permanently erase $DISK."
  read -r -p "Type ERASE $DISK to continue: " confirmation
  [ "$confirmation" = "ERASE $DISK" ] || die "confirmation did not match"
fi

CURRENT_IDENTITY=$(lsblk --json -b -d -o PATH,TYPE,SIZE,RO,SERIAL,WWN "$DISK" \
  | jq -c '.blockdevices[0] | {path,type,size,ro,serial,wwn}')
[ "$CURRENT_IDENTITY" = "$DISK_IDENTITY" ] || die "disk identity changed before erase"

echo "==> Erasing old partition and filesystem signatures..."
while read -r child; do
  wipefs --all --force "$child"
done < <(lsblk -nrpo NAME,TYPE "$DISK" | awk '$2 == "part" { print $1 }')
wipefs --all --force "$DISK"
sgdisk --zap-all "$DISK"

echo "==> Partitioning $DISK..."
nasty_partition_disk "$DISK" "$BOOT_MODE" "$PART_MODE"

partprobe "$DISK" 2>/dev/null || true
udevadm settle --timeout=10

partition_path() {
  local part_number=$1
  local path=""
  local attempt
  for ((attempt = 1; attempt <= 20; attempt++)); do
    path=$(lsblk -nrpo NAME,PARTN "$DISK" \
      | awk -v part="$part_number" '$2 == part { print $1; exit }')
    if [ -n "$path" ] && [ -b "$path" ]; then
      printf '%s\n' "$path"
      return 0
    fi
    sleep 1
  done
  return 1
}

PART1=$(partition_path 1) || die "boot partition did not appear"
PART2=$(partition_path 2) || die "root partition did not appear"

echo "==> Formatting filesystem partitions..."
if [ "$BOOT_MODE" = uefi ]; then
  mkfs.fat -F32 -n NASTY_EFI "$PART1"
fi
mkfs.ext4 -F -m 1 -L NASTY_ROOT "$PART2"

echo "==> Mounting target filesystems..."
mkdir -p "$TARGET_ROOT"
mountpoint -q "$TARGET_ROOT" && die "$TARGET_ROOT is already a mount point"
mount -t ext4 "$PART2" "$TARGET_ROOT"
ROOT_MOUNTED=1
mkdir -p "$TARGET_ROOT/boot"
if [ "$BOOT_MODE" = uefi ]; then
  mount -t vfat "$PART1" "$TARGET_ROOT/boot"
  BOOT_MOUNTED=1
fi

echo "==> Installing the machine-local system wrapper..."
mkdir -p "$TARGET_ROOT/etc/nixos"
cp -a "$STAGE_DIR/." "$TARGET_ROOT/etc/nixos/"

echo "==> Generating hardware configuration..."
HW_DIR=$(mktemp -d -t nasty-hardware.XXXXXX)
nixos-generate-config --root "$TARGET_ROOT" --dir "$HW_DIR"
awk '
  /fileSystems\."\/fs\// { skip=1; depth=0 }
  skip {
    for (i=1; i<=length($0); i++) {
      c = substr($0, i, 1)
      if (c == "{") depth++
      if (c == "}") { depth--; if (depth <= 0) { skip=0; break } }
    }
    next
  }
  !skip
' "$HW_DIR/hardware-configuration.nix" \
  > "$TARGET_ROOT/etc/nixos/hardware-configuration.nix"
rm -rf "$HW_DIR"

cat > "$TARGET_ROOT/etc/nixos/networking.nix" <<'EOF'
# Managed by NASty - edit via WebUI Settings > Network
{ ... }:
{
  networking.useDHCP = true;
}
EOF

mkdir -p "$TARGET_ROOT/var/lib/nasty"
cat > "$TARGET_ROOT/var/lib/nasty/networking.json" <<'EOF'
{ "interfaces": [], "dns": [], "bonds": [], "vlans": [] }
EOF

NASTY_REF=$(jq -r '.nodes["nasty"].original.ref // empty' \
  "$TARGET_ROOT/etc/nixos/flake.lock")
NASTY_REV=$(jq -r '.nodes["nasty"].locked.rev // empty' \
  "$TARGET_ROOT/etc/nixos/flake.lock")
case "$NASTY_REF" in
  v*|s*) printf '%s\n' "$NASTY_REF" > "$TARGET_ROOT/var/lib/nasty/version" ;;
  *) [ -z "$NASTY_REV" ] || printf '%s\n' "${NASTY_REV:0:7}" > "$TARGET_ROOT/var/lib/nasty/version" ;;
esac

echo "==> Installing NASty..."
nixos-install --root "$TARGET_ROOT" \
  --flake "$TARGET_ROOT/etc/nixos#nasty" \
  --no-root-passwd

echo
echo "Installation complete. Default WebUI login: admin / admin"
echo "Installed boot mode: $BOOT_MODE"
if [ "$PART_MODE" = split ]; then
  PART3=$(partition_path 3) || die "data partition did not appear"
  echo "Unformatted data partition: $PART3"
fi

if [ -t 0 ]; then
  read -r -p "Set a root password now? [y/N]: " set_password
  if [[ "$set_password" =~ ^[Yy]$ ]]; then
    nixos-enter --root "$TARGET_ROOT" -c passwd
  fi
fi

cleanup
trap - EXIT INT TERM

if [ "$NO_REBOOT" -eq 1 ]; then
  echo "Target filesystems unmounted. Remove the live medium before rebooting."
else
  echo "Remove the live medium when the machine restarts."
  reboot
fi
