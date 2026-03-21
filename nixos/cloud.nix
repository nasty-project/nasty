# Cloud disk image configuration for NASty.
# Uses the NixOS OCI image module which handles boot, serial console,
# filesystem layout, and cloud-init automatically.
#
# Build:
#   nix build .#nasty-cloud-image
#
# The resulting image has:
#   - NASty engine + WebUI running on boot
#   - admin / admin credentials
#   - DHCP networking
#   - SSH enabled (root login, password auth)
#   - cloud-init for provider provisioning
#   - No pre-configured storage pool
#
# This is a CI/testing artifact. Not intended for production deployment.

{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  networking.hostName = "nasty-cloud";

  # Known credentials for CI access
  users.users.root.initialPassword = "nasty";
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = true;
    };
  };
  networking.firewall.allowedTCPPorts = [ 22 ];

  services.nasty = {
    enable = true;
    engine = {
      package = nasty-engine;
      port = 2137;
      logLevel = "nasty_api=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info";
    };
    webui.package = nasty-webui;
    storage.mountBase = "/storage";
    nfs.enable = true;
    smb.enable = true;
    iscsi.enable = true;
    nvmeof.enable = true;
  };

  # Tell the update engine which flake config to rebuild.
  # aarch64 uses nasty-cloud-aarch64, x86_64 uses nasty-cloud.
  system.activationScripts.nasty-system-config = ''
    mkdir -p /var/lib/nasty
    CFG="nasty-cloud"
    [ "$(uname -m)" = "aarch64" ] && CFG="nasty-cloud-aarch64"
    echo "$CFG" > /var/lib/nasty/system-config
  '';

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";
  system.stateVersion = "24.11";
}
