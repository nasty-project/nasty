# Cloud disk image configuration for NASty.
# Uses the NixOS OCI image module which handles boot, serial console,
# filesystem layout, and cloud-init automatically.
#
# Build:
#   nix build .#packages.x86_64-linux.nasty-cloud-image
#
# The resulting image has:
#   - NASty engine + WebUI running on boot
#   - DHCP networking
#   - SSH enabled (key-based auth only)
#   - cloud-init for provider provisioning
#   - No pre-configured storage pool
#
# CI builds opt in to known credentials (root password "nasty", password SSH)
# by passing NASTY_CLOUD_TESTING_CREDS=1 in the build environment. Production
# images must never be built with this flag.

{ config, lib, pkgs, nasty-engine, nasty-webui ? null, nasty-version ? "dev", ... }:

let
  # Refuse to bake known-bad credentials unless the operator explicitly opts in
  # by setting NASTY_CLOUD_TESTING_CREDS=1 at build time.
  testingCreds = (builtins.getEnv "NASTY_CLOUD_TESTING_CREDS") == "1";
in
{
  networking.hostName = "nasty-cloud";

  # Default: SSH key-based root login, no password auth.
  # CI: opts in to a known root password via NASTY_CLOUD_TESTING_CREDS=1.
  users.users.root.initialPassword = lib.mkIf testingCreds "nasty";
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = if testingCreds then "yes" else "prohibit-password";
      PasswordAuthentication = testingCreds;
    };
  };
  networking.firewall.allowedTCPPorts = [ 22 ];

  warnings = lib.optional testingCreds
    "cloud.nix: NASTY_CLOUD_TESTING_CREDS=1 is set — image will ship root:nasty and password SSH. DO NOT publish this image.";

  services.nasty = {
    enable = true;
    engine = {
      package = nasty-engine;
      port = 2137;
      logLevel = "nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,nasty_apps=info,tower_http=info";
    };
    webui.package = nasty-webui;
    storage.mountBase = "/storage";
    nfs.enable = true;
    smb.enable = true;
    iscsi.enable = true;
    nvmeof.enable = true;
    # Build the NUT units so enabling the UPS protocol works (engine-
    # managed, only run when toggled on). Valid on cloud for remote-mode
    # monitoring of a network UPS. See #512.
    nut.enable = true;
  };

  # No mDNS/Avahi on cloud — no local network discovery needed
  services.avahi.enable = lib.mkForce false;

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";
  # Same boot-menu rename as appliance-base. Falls back to "dev"
  # when consumed standalone outside the flake's specialArgs.
  system.nixos.label = "v${nasty-version}";
  system.stateVersion = "24.11";
}
