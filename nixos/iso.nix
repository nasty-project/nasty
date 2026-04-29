{ config, pkgs, lib, nasty-engine, nasty-webui, nixpkgs, nasty-rootfs-toplevel ? null, installerSystemFlake, installerNastySource ? null, ... }:

let
  nasty-grub-theme = pkgs.runCommand "nasty-grub-theme" {
    nativeBuildInputs = [ pkgs.librsvg pkgs.imagemagick ];
  } ''
    nixos=${pkgs.nixos-grub2-theme}
    mkdir -p $out

    # Copy all theme chrome from nixos-grub2-theme (borders, fonts, icons, etc.)
    cp $nixos/*.png $nixos/*.pf2 $nixos/fonts.sh $out/
    cp -r $nixos/icons $out/
    # Files copied from the Nix store are read-only; make them writable so we
    # can overwrite background.png and logo.png with our own versions below.
    chmod -R u+w $out/

    # Replace background with a solid dark canvas (1920×1080).
    # PNG24: = 8-bit RGB — GRUB requires bit depth 8 or 16.
    magick -size 1920x1080 xc:'#0f1117' -type TrueColor -depth 8 PNG24:$out/background.png

    # Replace logo with the NASty logo (320×320, white SVG on transparent bg).
    # rsvg-convert outputs raw RGBA; re-encode as 16-bit RGBA (PNG64:) so GRUB
    # gets the full quality gradient/anti-aliasing instead of a lossy 8-bit palette.
    rsvg-convert -w 320 -h 320 \
      ${../webui/src/lib/assets/nasty-white.svg} \
      -o /tmp/nasty-logo-raw.png
    magick /tmp/nasty-logo-raw.png -type TrueColorAlpha -depth 16 PNG64:$out/logo.png

    # Rewrite theme.txt: bigger logo, dark transparent menu (no white box),
    # and better contrast item colors.
    cat > $out/theme.txt << 'EOF'
title-text: ""
title-font: "DejaVu Regular"
title-color: "#ffffff"

+ image {
  top = 4%
  height = 320
  width = 320
  left = 50%-160
  file = "logo.png"
}

desktop-image: "background.png"
message-font: "DejaVu Regular"
message-color: "#aaaaaa"
terminal-font: "Unifont Regular"
terminal-box: "terminal_*.png"

+ progress_bar {
  id = "__timeout__"
  top = 95%-32
  left  = 50%-25%
  height = 32
  width = 50%
  show_text = true
  text = "@TIMEOUT_NOTIFICATION_MIDDLE@"
  border_color = #5579C4
  bg_color = #1e2130
  fg_color = #5579C4
}

+ boot_menu {
  left = 50%-300
  width = 600
  top = 4%+320+4%
  height = 100%-4%-320-4%-4%-32-4%
  item_font = "DejaVu Regular"
  item_color = "#9aa0b4"
  item_height = 44
  item_icon_space = 14
  item_spacing = 4
  item_padding = 8
  selected_item_font = "DejaVu Regular"
  selected_item_color = "#ffffff"
  selected_item_pixmap_style = "select_*.png"
  icon_height = 32
  icon_width = 42
  scrollbar = false
}
EOF
  '';
in
{
  # Pre-built packages in the ISO's Nix store so nixos-install
  # can reuse them instead of recompiling from source.
  system.extraDependencies = [ nixpkgs nasty-engine ]
    ++ lib.optional (nasty-rootfs-toplevel != null) nasty-rootfs-toplevel
    ++ lib.optional (installerNastySource != null) installerNastySource
    ++ lib.optional (nasty-webui != null) nasty-webui;

  # Bundle the slim local system flake on the ISO. The installed appliance keeps
  # only a wrapper flake plus machine-local modules under /etc/nixos.
  environment.etc."nasty-system-flake".source = installerSystemFlake;
  environment.etc."nasty-source".source = lib.mkIf (installerNastySource != null) installerNastySource;

  # ── Branding ──────────────────────────────────────────────
  image.baseName = lib.mkForce "nasty";
  isoImage.volumeID = "NASTY_INSTALLER";
  isoImage.appendToMenuLabel = " NASty Installer";

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";

  boot.supportedFilesystems = [ "bcachefs" ];

  # Support both UEFI and legacy BIOS boot
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;
  isoImage.grubTheme = nasty-grub-theme;

  environment.systemPackages = with pkgs; [
    bcachefs-tools
    parted
    gptfdisk
    nvme-cli
    util-linux
    e2fsprogs
    dosfstools
    nixos-install-tools

    (writeShellScriptBin "nasty-install" ''
      set -euo pipefail

      if [ "$(id -u)" -ne 0 ]; then
        echo "Error: nasty-install must be run as root"
        exit 1
      fi

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
      echo "  2) Split disk: 30 GiB for OS, rest for data (single-disk setup)"
      echo ""
      read -p "Choose [1/2]: " PART_MODE

      if [ "$PART_MODE" != "1" ] && [ "$PART_MODE" != "2" ]; then
        echo "Invalid choice."
        exit 1
      fi

      if [ "$PART_MODE" = "2" ] && [ "$DISK_SIZE_G" -lt 50 ]; then
        echo ""
        echo "WARNING: Disk is only ''${DISK_SIZE_G} GiB. After 30 GiB for the OS,"
        echo "only $(( DISK_SIZE_G - 30 )) GiB will remain for data."
        echo "Consider using mode 1 with a separate data disk instead."
        read -p "Continue anyway? (yes/no): " SMALL_CONFIRM
        if [ "$SMALL_CONFIRM" != "yes" ]; then
          echo "Aborted."
          exit 0
        fi
      fi


      # ── Network configuration ─────────────────────────────────────
      echo ""
      echo "Network Configuration:"
      echo "  1) DHCP  (automatic — recommended for most setups)"
      echo "  2) Static IP"
      echo ""
      read -p "Choose [1/2, default 1]: " NET_MODE
      NET_MODE=''${NET_MODE:-1}
      if [ "$NET_MODE" != "1" ] && [ "$NET_MODE" != "2" ]; then
        NET_MODE=1
      fi

      if [ "$NET_MODE" = "2" ]; then
        NET_IFACE=$(${pkgs.iproute2}/bin/ip -4 route get 1.1.1.1 2>/dev/null \
          | awk '{for(i=1;i<=NF;i++) if($i=="dev") print $(i+1)}' \
          | head -1 || true)
        echo ""
        echo "  Detected interface: ''${NET_IFACE:-unknown}"
        echo ""
        read -p "  IP address    (e.g. 192.168.1.100): " NET_IP
        read -p "  Prefix length (e.g. 24 for /24):   " NET_PREFIX
        read -p "  Gateway       (e.g. 192.168.1.1):  " NET_GW
        read -p "  DNS servers, space-separated [1.1.1.1 8.8.8.8]: " NET_DNS
        NET_DNS=''${NET_DNS:-"1.1.1.1 8.8.8.8"}
        if [ -z "$NET_IP" ] || [ -z "$NET_PREFIX" ] || [ -z "$NET_GW" ]; then
          echo "Error: address, prefix length, and gateway are required for static IP."
          exit 1
        fi
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
          mkpart root ext4 512MiB 20GiB \
          mkpart data 20GiB 100%
      fi

      # Re-read partition table and wait for devices to settle
      partprobe "$DISK" 2>/dev/null || true
      udevadm settle --timeout=10
      sleep 1

      PART1="''${DISK}''${PSEP}1"
      PART2="''${DISK}''${PSEP}2"

      echo "==> Formatting partitions..."
      mkfs.fat -F32 "$PART1"
      mkfs.ext4 -F "$PART2"
      sync

      if [ "$PART_MODE" = "2" ]; then
        PART3="''${DISK}''${PSEP}3"
        echo "==> Data partition: $PART3 (left unformatted for filesystem creation via WebUI)"
      fi

      echo "==> Mounting..."
      mount -t ext4 "$PART2" /mnt
      mkdir -p /mnt/boot
      mount -t vfat "$PART1" /mnt/boot

      echo "==> Bootstrapping local system flake..."
      mkdir -p /mnt/etc/nixos
      cp /etc/nasty-system-flake/flake.lock /mnt/etc/nixos/flake.lock

      echo "==> Detecting local system identifier..."
      LOCAL_SYSTEM=$(nix --extra-experimental-features 'nix-command flakes' eval --impure --raw --expr builtins.currentSystem)
      if [ -z "$LOCAL_SYSTEM" ]; then
        echo "Error: failed to detect local system identifier"
        exit 1
      fi
      if [ ! -f /etc/nasty-source/nixos/system-flake/flake.nix.template ]; then
        echo "Error: missing /etc/nasty-source/nixos/system-flake/flake.nix.template"
        exit 1
      fi
      ${nasty-engine}/bin/nasty-engine bootstrap-system-flake \
        --dest-dir /mnt/etc/nixos \
        --template-file /etc/nasty-source/nixos/system-flake/flake.nix.template \
        --system "$LOCAL_SYSTEM" >/dev/null

      echo "==> Generating hardware configuration..."
      nixos-generate-config --root /mnt --dir /tmp/hw-config

      # Strip any /fs/ mount entries from the generated config.
      # Filesystem mounts are managed at runtime by the engine; if left in
      # hardware-configuration.nix they will block boot after filesystem destruction.
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
      ' /tmp/hw-config/hardware-configuration.nix > /tmp/hw-clean.nix \
        && mv /tmp/hw-clean.nix /tmp/hw-config/hardware-configuration.nix

      cp /tmp/hw-config/hardware-configuration.nix /mnt/etc/nixos/hardware-configuration.nix

      echo "==> Writing network configuration..."
      mkdir -p /mnt/var/lib/nasty
      if [ "$NET_MODE" = "2" ]; then
        NS_LIST=""
        for ns in $NET_DNS; do
          NS_LIST="$NS_LIST \"$ns\""
        done
        printf '%s\n' \
          '# Managed by NASty — edit via WebUI Settings > Network' \
          '{ ... }:' \
          '{' \
          '  networking.useDHCP = false;' \
          "  networking.interfaces.''${NET_IFACE}.ipv4.addresses = [{ address = \"''${NET_IP}\"; prefixLength = ''${NET_PREFIX}; }];" \
          "  networking.defaultGateway = \"''${NET_GW}\";" \
          "  networking.nameservers = [''${NS_LIST} ];" \
          '}' \
          > /mnt/etc/nixos/networking.nix
        # Write networking.json in new multi-interface format
        printf '%s\n' \
          '{' \
          '  "interfaces": [{' \
          "    \"name\": \"''${NET_IFACE}\"," \
          '    "enabled": true,' \
          '    "ipv4": {' \
          '      "method": "static",' \
          "      \"addresses\": [\"''${NET_IP}/''${NET_PREFIX}\"]," \
          "      \"gateway\": \"''${NET_GW}\"" \
          '    },' \
          '    "ipv6": { "method": "slaac", "addresses": [], "gateway": null },' \
          '    "mtu": null' \
          '  }],' \
          "  \"dns\": [$(echo "$NET_DNS" | sed 's/ /, /g; s/[^ ,][^ ,]*/\"&\"/g')]," \
          '  "bonds": [],' \
          '  "vlans": []' \
          '}' \
          > /mnt/var/lib/nasty/networking.json
      else
        printf '%s\n' \
          '# Managed by NASty — edit via WebUI Settings > Network' \
          '{ ... }:' \
          '{' \
          '  networking.useDHCP = true;' \
          '}' \
          > /mnt/etc/nixos/networking.nix
        # Write default DHCP networking.json
        printf '%s\n' \
          '{ "interfaces": [], "dns": [], "bonds": [], "vlans": [] }' \
          > /mnt/var/lib/nasty/networking.json
      fi

      # Empty SSH module — kept for backward compatibility with existing flakes
      # that import ./ssh.nix. SSH password auth is managed via sshd_override.conf.
      printf '%s\n' \
        '# Managed by NASty — SSH password auth is controlled via /var/lib/nasty/sshd_override.conf' \
        '{ ... }:' \
        '{' \
        '}' \
        > /mnt/etc/nixos/ssh.nix

      echo "==> Recording installed NASty version..."
      NASTY_REF=$(jq -r '.nodes["nasty"].original.ref // empty' /mnt/etc/nixos/flake.lock 2>/dev/null || true)
      NASTY_REV=$(jq -r '.nodes["nasty"].locked.rev // empty' /mnt/etc/nixos/flake.lock 2>/dev/null || true)
      case "$NASTY_REF" in
        v*|s*) echo "$NASTY_REF" > /mnt/var/lib/nasty/version ;;
        *) [ -n "$NASTY_REV" ] && echo "''${NASTY_REV:0:7}" > /mnt/var/lib/nasty/version || true ;;
      esac

      echo "==> Installing NASty..."
      echo "    (this may take a while on first install)"
      nixos-install --flake /mnt/etc/nixos#nasty --no-root-passwd

      # Detect IP address to show in post-install message
      NASTY_IP=$(${pkgs.iproute2}/bin/ip -4 route get 1.1.1.1 2>/dev/null | grep -oP 'src \K[^ ]+' || echo "<ip>")

      echo ""
      echo "=== Installation complete! ==="
      echo ""
      echo "  The NASty WebUI will be available at https://$NASTY_IP/"
      echo "  Default login: admin / admin"
      echo ""
      if [ "$PART_MODE" = "2" ]; then
        echo "  Data partition: $PART3 (create a filesystem via WebUI)"
        echo ""
      fi
      echo "  To reconfigure later:"
      echo "    nixos-rebuild switch --flake /etc/nixos#nasty"
      echo ""

      if [ ! -d /sys/firmware/efi ]; then
        echo ""
        echo -e "\033[1;31m  ╔═══════════════════════════════════════════════════════╗"
        echo -e "  ║                                                       ║"
        echo -e "  ║  WARNING: You booted in Legacy BIOS mode.             ║"
        echo -e "  ║  NASty requires UEFI to boot after installation.      ║"
        echo -e "  ║                                                       ║"
        echo -e "  ║  Before rebooting, switch to UEFI mode:               ║"
        echo -e "  ║    - Proxmox: change BIOS from SeaBIOS to OVMF        ║"
        echo -e "  ║    - Physical: enable UEFI boot in BIOS settings      ║"
        echo -e "  ║    - Other VMs: select UEFI/OVMF firmware             ║"
        echo -e "  ║                                                       ║"
        echo -e "  ╚═══════════════════════════════════════════════════════╝\033[0m"
        echo ""
      fi

      read -p "Set root password now? (yes/no): " SET_PW
      if [ "$SET_PW" = "yes" ]; then
        nixos-enter --root /mnt -c 'passwd'
      fi

      echo ""
      read -p "Press Enter to reboot or Ctrl+C for a shell... "
      reboot
    '')
  ];

  # Enable networking for installation
  networking.wireless.enable = pkgs.lib.mkForce false;
  networking.useDHCP = pkgs.lib.mkForce true;

  # Auto-launch the installer on tty1
  services.getty.autologinUser = lib.mkForce "root";
  programs.bash.loginShellInit = ''
    # Auto-launch installer on tty1 (only once)
    if [ "$(tty)" = "/dev/tty1" ] && [ ! -f /tmp/.nasty-installer-ran ]; then
      touch /tmp/.nasty-installer-ran
      nasty-install
    fi
  '';

  # Installer welcome message (for other ttys)
  services.getty.helpLine = ''

    \e[1;31m mm   m   mm    mmmm    m
     #"m  #   ##   #"   " mm#mm  m   m
     # #m #  #  #  "#mmm    #    "m m"
     #  # #  #mm#      "#   #     #m#
     #   ## #    # "mmm#"   "mm   "#
                                  m"
                                 ""\e[0m
    NASty NAS Installer

    Run the guided installer:  nasty-install

    The installer supports two modes:
      1) Entire disk for OS  (use separate disks for data)
      2) Split disk          (8 GiB OS + rest as bcachefs data)

    For manual installation, see the project documentation.

  '';
}
