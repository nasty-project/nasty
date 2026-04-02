{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [
    ./tls.nix
  ];

  boot.isContainer = true;

  networking.hostName = "nasty-rootfs";

  services.nasty = {
    enable = true;

    engine = {
      package = nasty-engine;
      port = 2137;
      logLevel = "nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info";
    };

    webui.package = nasty-webui;
    storage.mountBase = "/fs";
    nfs.enable = true;
    smb.enable = true;
    iscsi.enable = true;
    nvmeof.enable = true;
  };

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = true;
    };
  };

  networking.firewall.allowedTCPPorts = [ 22 ];

  # Container/rootfs artifacts should rebuild this config if the appliance
  # update engine is ever used inside them.
  system.activationScripts.nasty-system-config = ''
    mkdir -p /var/lib/nasty
    CFG="nasty-rootfs"
    [ "$(uname -m)" = "aarch64" ] && CFG="nasty-rootfs-aarch64"
    echo "$CFG" > /var/lib/nasty/system-config
  '';

  # Avahi is not generally useful in container-style rootfs artifacts.
  services.avahi.enable = lib.mkForce false;

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";
  system.stateVersion = "24.11";
}
