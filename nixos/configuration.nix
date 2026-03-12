{ config, pkgs, nasty-middleware, nasty-webui ? null, ... }:

{
  imports = [
    ./hardware-configuration.nix
    ./grub-device.nix
  ];

  # Boot loader — GRUB with hybrid BIOS + UEFI support
  # The installer writes grub-device.nix with the correct device path.
  boot.loader.grub = {
    enable = true;
    efiSupport = true;
    efiInstallAsRemovable = true;
  };
  boot.loader.efi.canTouchEfiVariables = false;

  networking.hostName = "nasty";

  # Enable the NASty module with all protocols
  services.nasty = {
    enable = true;

    middleware = {
      package = nasty-middleware;
      port = 2137;
      logLevel = "nasty_api=info,tower_http=info";
    };

    webui = {
      package = nasty-webui;
    };

    storage.mountBase = "/mnt/nasty";

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

  # Additional system configuration
  time.timeZone = "UTC";

  # Allow SSH for management
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = true;
    };
  };

  networking.firewall.allowedTCPPorts = [ 22 ];

  # Enable SMART monitoring
  services.smartd.enable = true;

  system.stateVersion = "24.11";
}
