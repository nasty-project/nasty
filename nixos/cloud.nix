# Cloud/CI disk image configuration for NASty.
# Produces a bootable QCOW2 image suitable for upload to cloud providers.
#
# Build:
#   nix build .#nasty-cloud-image
#
# The resulting image has:
#   - NASty engine + WebUI running on boot
#   - admin / admin credentials
#   - DHCP networking
#   - SSH enabled (root login, password auth)
#   - No pre-configured storage pool — create one via WebUI/API against /dev/vdb etc.
#
# This is a CI/testing artifact. Not intended for production deployment.

{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  # virtio drivers so the cloud VM can see its disks and network
  boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" "virtio_net" "virtio_scsi" ];

  networking.hostName = "nasty-cloud";
  networking.useDHCP = true;

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

  # cloud-init for cloud provider provisioning (hostname, SSH keys, etc.)
  services.cloud-init.enable = true;

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";
  system.stateVersion = "24.11";
}
