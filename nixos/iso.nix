{ config, pkgs, lib, nasty-engine, nasty-webui, ... }:

let
  nastySrc = lib.cleanSource ./..;

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
  system.extraDependencies = [ nasty-engine ]
    ++ lib.optional (nasty-webui != null) nasty-webui;

  # Bundle NASty source on the ISO for flake-based installation
  environment.etc."nasty-src".source = nastySrc;

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
    git  # required for Nix flakes

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
        echo "==> Data partition: $PART3 (left unformatted for filesystem creation via WebUI)"
      fi

      echo "==> Mounting..."
      mount "$PART2" /mnt
      mkdir -p /mnt/boot
      mount "$PART1" /mnt/boot

      echo "==> Copying NASty source..."
      mkdir -p /mnt/etc/nixos
      for dir in engine webui nixos; do
        cp -rL --no-preserve=mode /etc/nasty-src/$dir /mnt/etc/nixos/
      done

      # TODO: Remove once repo is public — copy GitHub token for update support
      if [ -f /etc/nasty-src/nixos/github-token ]; then
        echo "==> Installing GitHub token for updates..."
        mkdir -p /mnt/var/lib/nasty
        cp /etc/nasty-src/nixos/github-token /mnt/var/lib/nasty/github-token
        chmod 600 /mnt/var/lib/nasty/github-token
      fi

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

      cp /tmp/hw-config/hardware-configuration.nix /mnt/etc/nixos/nixos/hardware-configuration.nix

      echo "==> Writing network configuration..."
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
          > /mnt/etc/nixos/nixos/networking.nix
        # Also write networking.json so WebUI shows the configured values
        printf '%s\n' \
          '{' \
          '  "dhcp": false,' \
          "  \"interface\": \"''${NET_IFACE}\"," \
          "  \"address\": \"''${NET_IP}\"," \
          "  \"prefix_length\": ''${NET_PREFIX}," \
          "  \"gateway\": \"''${NET_GW}\"," \
          "  \"nameservers\": [$(echo "$NET_DNS" | sed 's/ /, /g; s/[^ ,][^ ,]*/\"&\"/g')]" \
          '}' \
          > /mnt/var/lib/nasty/networking.json
      else
        printf '%s\n' \
          '# Managed by NASty — edit via WebUI Settings > Network' \
          '{ ... }:' \
          '{' \
          '  networking.useDHCP = true;' \
          '}' \
          > /mnt/etc/nixos/nixos/networking.nix
      fi

      # Flakes require a git repo to resolve paths.
      # Sparse checkout ensures future updates don't materialize dev-only files
      # (tests/, CLAUDE.md, build-iso.sh) that have no place on an appliance.
      cd /mnt/etc/nixos
      git init -q
      git remote add origin https://github.com/nasty-project/nasty.git
      git sparse-checkout init --cone
      git sparse-checkout set engine webui nixos
      git add .

      echo "==> Installing NASty..."
      echo "    (this may take a while on first install)"
      nixos-install --flake /mnt/etc/nixos/nixos#nasty --no-root-passwd

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
      echo "    nixos-rebuild switch --flake /etc/nixos/nixos#nasty"
      echo ""

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
