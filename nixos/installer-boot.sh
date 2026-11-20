#!/usr/bin/env bash
# Shared by the ISO and packaged live-system installers.

nasty_detect_boot_mode() {
  if [ -d "${NASTY_EFI_DIR:-/sys/firmware/efi}" ]; then
    printf '%s\n' uefi
  else
    printf '%s\n' bios
  fi
}

nasty_select_boot_mode() {
  local requested=$1 system=$2 detected
  detected=$(nasty_detect_boot_mode)
  case "$requested" in
    auto) requested=$detected ;;
    bios|uefi) ;;
    *) printf 'invalid boot mode: %s (expected auto, uefi, or bios)\n' "$requested" >&2; return 1 ;;
  esac
  if [ "$requested" = uefi ] && [ "$detected" = bios ] && [ "$system" = aarch64-linux ]; then
    # U-Boot often starts the live system via extlinux yet offers UEFI to the
    # installed one; such firmware cannot store EFI variables.
    printf 'warning: the live environment did not boot via UEFI; installing for UEFI firmware without EFI boot variables (for example U-Boot), using the fallback EFI/BOOT loader\n' >&2
  elif [ "$requested" != "$detected" ]; then
    printf 'requested %s installation, but the live environment booted in %s mode; reboot the installer in the desired mode\n' "$requested" "$detected" >&2
    return 1
  fi
  if [ "$requested" = bios ] && [ "$system" != x86_64-linux ]; then
    printf 'legacy BIOS installation is supported only on x86_64-linux\n' >&2
    return 1
  fi
  printf '%s\n' "$requested"
}

nasty_partition_disk() {
  local disk=$1 mode=$2 layout=$3
  case "$mode:$layout" in
    bios:whole)
      parted -s "$disk" -- \
        mklabel gpt \
        mkpart BIOS-BOOT 1MiB 3MiB \
        set 1 bios_grub on \
        mkpart root ext4 3MiB 100%
      ;;
    bios:split)
      parted -s "$disk" -- \
        mklabel gpt \
        mkpart BIOS-BOOT 1MiB 3MiB \
        set 1 bios_grub on \
        mkpart root ext4 3MiB 50GiB \
        mkpart data 50GiB 100%
      ;;
    uefi:whole)
      parted -s "$disk" -- \
        mklabel gpt \
        mkpart ESP fat32 1MiB 512MiB \
        set 1 esp on \
        mkpart root ext4 512MiB 100%
      ;;
    uefi:split)
      parted -s "$disk" -- \
        mklabel gpt \
        mkpart ESP fat32 1MiB 512MiB \
        set 1 esp on \
        mkpart root ext4 512MiB 50GiB \
        mkpart data 50GiB 100%
      ;;
    *) printf 'invalid partition layout: %s/%s\n' "$mode" "$layout" >&2; return 1 ;;
  esac
}

# Prefer an identity-stable disk path for GRUB installation on subsequent
# updates. Serial/model-based by-id paths can collide on virtual disks.
nasty_grub_disk_path() {
  local disk=$1 dir link resolved
  for dir in /dev/disk/by-id /dev/disk/by-path; do
    [ -d "$dir" ] || continue
    for link in "$dir"/*; do
      [ -L "$link" ] || continue
      case "$link" in
        *-part[0-9]*) continue ;;
      esac
      if [ "$dir" = /dev/disk/by-id ]; then
        case "${link##*/}" in
          wwn-*|nvme-eui.*|scsi-3*) ;;
          *) continue ;;
        esac
      fi
      resolved=$(readlink -f "$link") || continue
      if [ "$resolved" = "$disk" ]; then
        printf '%s\n' "$link"
        return 0
      fi
    done
  done
  printf 'Warning: no stable disk alias for %s; GRUB updates will use this /dev path, which can change if disks are reordered\n' "$disk" >&2
  printf '%s\n' "$disk"
}

nasty_write_boot_module() {
  local dest=$1 mode=$2 disk=$3 grub_disk
  if [ "$mode" = uefi ] && [ "$(nasty_detect_boot_mode)" = uefi ]; then
    cat > "$dest" <<'EOF'
# Installer-selected boot mode. Preserved by NASty updates.
{ ... }: {
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
}
EOF
    return
  fi
  if [ "$mode" = uefi ]; then
    cat > "$dest" <<'EOF'
# Installer-selected boot mode. Preserved by NASty updates.
# UEFI firmware without EFI variables (for example U-Boot): systemd-boot is
# found through the fallback EFI/BOOT loader path.
{ lib, ... }: {
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = lib.mkForce false;
}
EOF
    return
  fi
  grub_disk=$(nasty_grub_disk_path "$disk")
  # Only allow path characters safe to interpolate into a Nix string.
  if [[ ! "$grub_disk" =~ ^/dev/[a-zA-Z0-9_./:+-]+$ ]]; then
    printf 'unsupported GRUB disk path: %s\n' "$grub_disk" >&2
    return 1
  fi
  cat > "$dest" <<EOF
# Installer-selected boot mode. Preserved by NASty updates.
{ config, lib, ... }: {
  assertions = [{
    assertion = !config.services.nasty.secureBoot.enable;
    message = "UEFI Secure Boot is unavailable on a legacy BIOS installation";
  }];
  boot.loader.systemd-boot.enable = lib.mkForce false;
  boot.loader.systemd-boot.memtest86.enable = lib.mkForce false;
  boot.loader.efi.canTouchEfiVariables = lib.mkForce false;
  boot.loader.grub.enable = true;
  boot.loader.grub.device = "$grub_disk";
  boot.loader.grub.configurationLimit = 20;
}
EOF
}
