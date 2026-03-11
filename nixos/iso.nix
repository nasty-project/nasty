{ config, pkgs, lib, nasty-middleware, nasty-webui, ... }:

let
  nastySrc = lib.cleanSource ./..;
in
{
  # Pre-built packages in the ISO's Nix store so nixos-install
  # can reuse them instead of recompiling from source.
  system.extraDependencies = [ nasty-middleware ]
    ++ lib.optional (nasty-webui != null) nasty-webui;

  # Bundle NASty source on the ISO for flake-based installation
  environment.etc."nasty-src".source = nastySrc;

  # ── Branding ──────────────────────────────────────────────
  isoImage.isoBaseName = lib.mkForce "nasty";
  isoImage.volumeID = "NASTY_INSTALLER";
  isoImage.appendToMenuLabel = " NASty Installer";

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";

  boot.supportedFilesystems = [ "bcachefs" ];

  environment.systemPackages = with pkgs; [
    bcachefs-tools
    parted
    gptfdisk
    nvme-cli
    util-linux
    e2fsprogs
    dosfstools
    nixos-install-tools
    git  # required for Nix flakes

    (writeShellScriptBin "nasty-install" ''
      set -euo pipefail

      echo "=== NASty NAS Guided Installer ==="
      echo ""

      # List available disks
      echo "Available disks:"
      lsblk -d -o NAME,SIZE,MODEL | grep -v loop
      echo ""

      read -p "Enter disk to install to (e.g., sda): " DISK
      DISK="/dev/$DISK"

      if [ ! -b "$DISK" ]; then
        echo "Error: $DISK is not a block device"
        exit 1
      fi

      # Get disk size in GiB for display
      DISK_SIZE_B=$(lsblk -b -d -n -o SIZE "$DISK")
      DISK_SIZE_G=$(( DISK_SIZE_B / 1073741824 ))

      echo ""
      echo "Disk: $DISK (''${DISK_SIZE_G} GiB)"
      echo ""
      echo "Partitioning mode:"
      echo "  1) Use entire disk for NASty OS (recommended if you have separate data disks)"
      echo "  2) Split disk: 8 GiB for OS, rest for data (single-disk setup)"
      echo ""
      read -p "Choose [1/2]: " PART_MODE

      if [ "$PART_MODE" != "1" ] && [ "$PART_MODE" != "2" ]; then
        echo "Invalid choice."
        exit 1
      fi

      if [ "$PART_MODE" = "2" ] && [ "$DISK_SIZE_G" -lt 16 ]; then
        echo "Error: disk too small for split mode (need at least 16 GiB, have ''${DISK_SIZE_G} GiB)"
        exit 1
      fi

      echo ""
      echo "WARNING: This will ERASE all data on $DISK"
      read -p "Continue? (yes/no): " CONFIRM
      if [ "$CONFIRM" != "yes" ]; then
        echo "Aborted."
        exit 0
      fi

      # Determine partition suffix style
      PSEP=""
      if [[ "$DISK" == *nvme* ]] || [[ "$DISK" == *mmcblk* ]]; then
        PSEP="p"
      fi

      echo ""
      echo "==> Partitioning $DISK..."
      if [ "$PART_MODE" = "1" ]; then
        parted -s "$DISK" -- \
          mklabel gpt \
          mkpart ESP fat32 1MiB 512MiB \
          set 1 esp on \
          mkpart root ext4 512MiB 100%
      else
        parted -s "$DISK" -- \
          mklabel gpt \
          mkpart ESP fat32 1MiB 512MiB \
          set 1 esp on \
          mkpart root ext4 512MiB 8GiB \
          mkpart data 8GiB 100%
      fi

      PART1="''${DISK}''${PSEP}1"
      PART2="''${DISK}''${PSEP}2"

      echo "==> Formatting partitions..."
      mkfs.fat -F32 "$PART1"
      mkfs.ext4 -F "$PART2"

      if [ "$PART_MODE" = "2" ]; then
        PART3="''${DISK}''${PSEP}3"
        echo "==> Formatting data partition as bcachefs..."
        bcachefs format --label=data "$PART3"
      fi

      echo "==> Mounting..."
      mount "$PART2" /mnt
      mkdir -p /mnt/boot
      mount "$PART1" /mnt/boot

      echo "==> Copying NASty source..."
      mkdir -p /mnt/etc
      cp -r /etc/nasty-src /mnt/etc/nixos
      chmod -R u+w /mnt/etc/nixos

      echo "==> Generating hardware configuration..."
      nixos-generate-config --root /mnt --dir /tmp/hw-config
      cp /tmp/hw-config/hardware-configuration.nix /mnt/etc/nixos/nixos/hardware-configuration.nix

      # Flakes require a git repo to resolve paths
      cd /mnt/etc/nixos
      git init -q
      git add .

      echo "==> Installing NASty..."
      echo "    (this may take a while on first install)"
      nixos-install --flake /mnt/etc/nixos/nixos#nasty --no-root-passwd

      echo ""
      echo "=== Installation complete! ==="
      echo ""
      echo "  The NASty WebUI will be available at https://<ip>/"
      echo "  Default login: admin / admin"
      echo ""
      if [ "$PART_MODE" = "2" ]; then
        echo "  Data partition: $PART3 (bcachefs, available for pool creation)"
        echo ""
      fi
      echo "  To reconfigure later:"
      echo "    nixos-rebuild switch --flake /etc/nixos/nixos#nasty"
      echo ""

      read -p "Set root password now? (yes/no): " SET_PW
      if [ "$SET_PW" = "yes" ]; then
        nixos-enter --root /mnt -c 'passwd'
      fi

      echo ""
      echo "Run 'reboot' when ready."
    '')
  ];

  # Enable networking for installation
  networking.wireless.enable = pkgs.lib.mkForce false;
  networking.useDHCP = pkgs.lib.mkForce true;

  # Installer welcome message
  services.getty.helpLine = ''

    ███╗   ██╗ █████╗ ███████╗████████╗██╗   ██╗
    ████╗  ██║██╔══██╗██╔════╝╚══██╔══╝╚██╗ ██╔╝
    ██╔██╗ ██║███████║███████╗   ██║    ╚████╔╝
    ██║╚██╗██║██╔══██║╚════██║   ██║     ╚██╔╝
    ██║ ╚████║██║  ██║███████║   ██║      ██║
    ╚═╝  ╚═══╝╚═╝  ╚═╝╚══════╝   ╚═╝      ╚═╝

    NASty NAS Installer

    Run the guided installer:  nasty-install

    The installer supports two modes:
      1) Entire disk for OS  (use separate disks for data)
      2) Split disk          (8 GiB OS + rest as bcachefs data)

    For manual installation, see the project documentation.

  '';
}
