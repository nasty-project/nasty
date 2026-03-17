{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [
    ./hardware-configuration.nix
    ./networking.nix
  ];

  # Boot loader — UEFI with systemd-boot
  boot.loader.systemd-boot.enable = true;
  boot.loader.systemd-boot.configurationLimit = 10;
  boot.loader.systemd-boot.consoleMode = "auto";
  boot.loader.efi.canTouchEfiVariables = true;

  # Restrict /boot (EFI) partition to root-only access.
  # nixos-generate-config defaults to fmask=0022 (world-readable), which causes
  # systemd-boot to warn that the random seed file is a security hole.
  fileSystems."/boot".options = lib.mkForce [ "fmask=0077" "dmask=0077" ];

  networking.hostName = "nasty";

  # Dynamic TTY banner: a oneshot service writes /run/nasty-issue with the
  # current IP (via 'ip route get') before getty starts on tty1.
  # We use ip route get instead of agetty's \4 escape because \4 can resolve
  # to the wrong interface (e.g. systemd-resolved's 127.0.0.2).
  systemd.services.nasty-tty-banner = {
    description = "NASty TTY login banner";
    wantedBy = [ "getty@tty1.service" ];
    before = [ "getty@tty1.service" ];
    wants = [ "network-online.target" ];
    after = [ "network-online.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    path = [ pkgs.iproute2 pkgs.gawk pkgs.coreutils ];
    script = ''
      # Try routing-based detection first (most accurate)
      IP=$(ip -4 route get 1.1.1.1 2>/dev/null \
        | awk '{for(i=1;i<=NF;i++) if ($i=="src") {print $(i+1); exit}}')
      # Fallback: first non-loopback address on any interface
      if [ -z "$IP" ]; then
        IP=$(ip -4 addr show \
          | awk '/inet / && !/127\./ {print $2}' | cut -d/ -f1 | head -1)
      fi
      IP=''${IP:-"(not yet assigned)"}
      printf '\n  NASty -- Storage with attitude\n\n  WebUI:   https://%s\n  Login:   admin / admin\n\n' \
        "$IP" > /run/nasty-issue
    '';
  };

  services.getty.helpLine = lib.mkForce "";
  services.getty.extraArgs = [ "--issue-file" "/run/nasty-issue" ];

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
  environment.systemPackages = with pkgs; [ vim file binutils git ];

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
