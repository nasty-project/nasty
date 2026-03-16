{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [
    ./hardware-configuration.nix
    ./networking.nix
  ];

  # Boot loader — UEFI with systemd-boot
  boot.loader.systemd-boot.enable = true;
  boot.loader.systemd-boot.configurationLimit = 10;
  boot.loader.efi.canTouchEfiVariables = true;

  # NASty background in the systemd-boot generation picker.
  # systemd-boot v253+ reads `background` from loader.conf and displays
  # the BMP as a full-screen backdrop behind the boot menu.
  boot.loader.systemd-boot.extraFiles."EFI/loader/nasty-bg.bmp" =
    let
      # SVG → PNG (1920×1080, dark canvas with centred logo) → BMP
      # BMP is the format systemd-boot requires for its background.
      nasty-boot-bg = pkgs.runCommand "nasty-boot-bg.bmp" {
        nativeBuildInputs = [ pkgs.librsvg pkgs.imagemagick ];
      } ''
        # Render the white SVG logo at 320×320
        rsvg-convert -w 320 -h 320 \
          ${../webui/src/lib/assets/nasty-white.svg} \
          -o /tmp/logo.png

        # Compose: dark background + centred logo
        magick \
          -size 1920x1080 xc:'#0f1117' \
          /tmp/logo.png -gravity center -composite \
          -type TrueColor -depth 8 \
          BMP3:/tmp/nasty-bg.bmp

        cp /tmp/nasty-bg.bmp $out
      '';
    in nasty-boot-bg;

  boot.loader.systemd-boot.extraConfig = ''
    background \EFI\loader\nasty-bg.bmp
  '';

  # Restrict /boot (EFI) partition to root-only access.
  # nixos-generate-config defaults to fmask=0022 (world-readable), which causes
  # systemd-boot to warn that the random seed file is a security hole.
  fileSystems."/boot".options = lib.mkForce [ "fmask=0077" "dmask=0077" ];

  networking.hostName = "nasty";

  # Enable the NASty module with all protocols
  services.nasty = {
    enable = true;

    engine = {
      package = nasty-engine;
      port = 2137;
      logLevel = "nasty_api=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info";
    };

    webui = {
      package = nasty-webui;
    };

    storage.mountBase = "/storage";

    nfs.enable = true;
    smb.enable = true;
    iscsi.enable = true;
    nvmeof.enable = true;
  };

  # Branding
  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";

  # Useful tools
  environment.systemPackages = with pkgs; [ vim ];

  # Allow SSH for management
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = true;
    };
  };

  networking.firewall.allowedTCPPorts = [ 22 ];

  services.avahi = {
    enable = true;
    nssmdns4 = true;
    publish = {
      enable = true;
      addresses = true;
      workstation = true;
    };
  };

  # Enable SMART monitoring; skip silently on VMs (no SMART-capable devices)
  services.smartd.enable = true;
  systemd.services.smartd.unitConfig.ConditionVirtualization = "no";

  system.stateVersion = "24.11";
}
