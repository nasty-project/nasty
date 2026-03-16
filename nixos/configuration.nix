{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [
    ./hardware-configuration.nix
  ];

  # Boot loader — UEFI with systemd-boot
  boot.loader.systemd-boot.enable = true;
  boot.loader.systemd-boot.configurationLimit = 10;
  boot.loader.efi.canTouchEfiVariables = true;

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
