{ config, pkgs, nasty-middleware, nasty-webui ? null, ... }:

{
  imports = [
    ./hardware-configuration.nix
  ];

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

  # Additional system configuration
  time.timeZone = "UTC";

  # Allow SSH for management
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  networking.firewall.allowedTCPPorts = [ 22 ];

  # Enable SMART monitoring
  services.smartd.enable = true;

  system.stateVersion = "24.11";
}
