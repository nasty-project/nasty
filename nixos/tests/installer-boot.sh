#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=nixos/installer-boot.sh
source "$(dirname "$0")/../installer-boot.sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

export NASTY_EFI_DIR="$tmp/efi"
test "$(nasty_select_boot_mode auto x86_64-linux)" = bios
test "$(nasty_select_boot_mode bios x86_64-linux)" = bios
if nasty_select_boot_mode uefi x86_64-linux > /dev/null 2>&1; then
  echo 'UEFI request unexpectedly accepted on BIOS live system' >&2
  exit 1
fi
if nasty_select_boot_mode auto aarch64-linux > /dev/null 2>&1; then
  echo 'BIOS request unexpectedly accepted on aarch64' >&2
  exit 1
fi
# U-Boot: live system booted via extlinux, installed system boots via UEFI.
test "$(nasty_select_boot_mode uefi aarch64-linux 2>/dev/null)" = uefi
nasty_write_boot_module "$tmp/uefi-novars.nix" uefi /dev/unused
grep -q 'systemd-boot.enable = true' "$tmp/uefi-novars.nix"
grep -q 'canTouchEfiVariables = lib.mkForce false' "$tmp/uefi-novars.nix"

mkdir "$NASTY_EFI_DIR"
test "$(nasty_select_boot_mode auto x86_64-linux)" = uefi
test "$(nasty_select_boot_mode uefi aarch64-linux)" = uefi
if nasty_select_boot_mode bios x86_64-linux > /dev/null 2>&1; then
  echo 'BIOS request unexpectedly accepted on UEFI live system' >&2
  exit 1
fi
if nasty_select_boot_mode invalid x86_64-linux > /dev/null 2>&1; then
  echo 'Invalid boot mode unexpectedly accepted' >&2
  exit 1
fi

nasty_write_boot_module "$tmp/uefi.nix" uefi /dev/unused
if grep -q 'grub.device' "$tmp/uefi.nix"; then
  echo 'UEFI module unexpectedly enables GRUB' >&2
  exit 1
fi
grep -q 'systemd-boot.enable = true' "$tmp/uefi.nix"
nasty_write_boot_module "$tmp/bios.nix" bios /dev/unused
grep -q 'grub.device = "/dev/unused"' "$tmp/bios.nix"
grep -q 'systemd-boot.enable = lib.mkForce false' "$tmp/bios.nix"
if nasty_write_boot_module "$tmp/invalid.nix" bios '/dev/quote"' > /dev/null 2>&1; then
  echo 'Unsafe GRUB disk path unexpectedly accepted' >&2
  exit 1
fi

# Exercise all layouts without touching a disk; CI additionally runs parted
# against an isolated loop device to validate the actual GPT flags.
parted() { PARTED_ARGS="$*"; }
for layout in whole split; do
  nasty_partition_disk /dev/test bios "$layout"
  [[ "$PARTED_ARGS" == *'set 1 bios_grub on'* ]]
  [[ "$PARTED_ARGS" == *'mkpart root ext4 3MiB'* ]]
  nasty_partition_disk /dev/test uefi "$layout"
  [[ "$PARTED_ARGS" == *'set 1 esp on'* ]]
  [[ "$PARTED_ARGS" == *'mkpart root ext4 512MiB'* ]]
done
unset -f parted

if command -v nix-instantiate >/dev/null 2>&1; then
  nix-instantiate --parse "$tmp/uefi.nix" > /dev/null
  nix-instantiate --parse "$tmp/bios.nix" > /dev/null
  nix-instantiate --parse "$tmp/uefi-novars.nix" > /dev/null
fi

if command -v nix >/dev/null 2>&1; then
  # Existing appliances have no nasty-installer-boot.nix; template's optional import
  # must leave their UEFI loader unchanged.
  nix eval --impure --json --expr '
    let config = (builtins.getFlake (toString ./.)).nixosConfigurations.nasty-vm.config;
    in {
      grub = config.boot.loader.grub.enable;
      systemdBoot = config.boot.loader.systemd-boot.enable;
      efiVariables = config.boot.loader.efi.canTouchEfiVariables;
    }
  ' | grep -q '"efiVariables":true,"grub":false,"systemdBoot":true'

  NASTY_BOOT_TEST_MODULE="$tmp/bios.nix" nix eval --impure --json --expr '
    let
      flake = builtins.getFlake (toString ./.);
      system = flake.nixosConfigurations.nasty-vm.extendModules {
        modules = [ (import (builtins.getEnv "NASTY_BOOT_TEST_MODULE")) ];
      };
    in {
      grub = system.config.boot.loader.grub.enable;
      device = system.config.boot.loader.grub.device;
      systemdBoot = system.config.boot.loader.systemd-boot.enable;
      efiVariables = system.config.boot.loader.efi.canTouchEfiVariables;
    }
  ' | grep -q '"grub":true.*"systemdBoot":false'

  NASTY_BOOT_TEST_MODULE="$tmp/bios.nix" nix eval --impure --json --expr '
    let
      flake = builtins.getFlake (toString ./.);
      system = flake.nixosConfigurations.nasty-vm.extendModules {
        modules = [
          (import (builtins.getEnv "NASTY_BOOT_TEST_MODULE"))
          ({ ... }: { services.nasty.secureBoot.enable = true; })
        ];
      };
    in builtins.any (assertion: !assertion.assertion && builtins.match ".*legacy BIOS.*" assertion.message != null) system.config.assertions
  ' | grep -qx true

  NASTY_BOOT_TEST_MODULE="$tmp/uefi.nix" nix eval --impure --json --expr '
    let
      flake = builtins.getFlake (toString ./.);
      system = flake.nixosConfigurations.nasty-vm.extendModules {
        modules = [ (import (builtins.getEnv "NASTY_BOOT_TEST_MODULE")) ];
      };
    in {
      grub = system.config.boot.loader.grub.enable;
      systemdBoot = system.config.boot.loader.systemd-boot.enable;
      efiVariables = system.config.boot.loader.efi.canTouchEfiVariables;
    }
  ' | grep -q '"efiVariables":true,"grub":false,"systemdBoot":true'

  NASTY_BOOT_TEST_MODULE="$tmp/uefi-novars.nix" nix eval --impure --json --expr '
    let
      flake = builtins.getFlake (toString ./.);
      system = flake.nixosConfigurations.nasty-vm.extendModules {
        modules = [ (import (builtins.getEnv "NASTY_BOOT_TEST_MODULE")) ];
      };
    in {
      grub = system.config.boot.loader.grub.enable;
      systemdBoot = system.config.boot.loader.systemd-boot.enable;
      efiVariables = system.config.boot.loader.efi.canTouchEfiVariables;
    }
  ' | grep -q '"efiVariables":false,"grub":false,"systemdBoot":true'

  # A UEFI install may later enable lanzaboote; its forced systemd-boot
  # disable must still override the installer module's normal true.
  NASTY_BOOT_TEST_MODULE="$tmp/uefi.nix" nix eval --impure --json --expr '
    let
      flake = builtins.getFlake (toString ./.);
      system = flake.nixosConfigurations.nasty-vm.extendModules {
        modules = [
          (import (builtins.getEnv "NASTY_BOOT_TEST_MODULE"))
          ({ ... }: { services.nasty.secureBoot.enable = true; })
        ];
      };
    in {
      systemdBoot = system.config.boot.loader.systemd-boot.enable;
      lanzaboote = system.config.boot.lanzaboote.enable;
    }
  ' | grep -q '"lanzaboote":true,"systemdBoot":false'
fi
