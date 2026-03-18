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
  # GRUB is used instead of Limine here because disk image builds run in a
  # sandbox without real EFI hardware — Limine's installer requires efibootmgr
  # which cannot register boot entries without actual EFI variables.
  boot.loader.grub = {
    enable = true;
    device = "nodev";
    efiSupport = true;
    efiInstallAsRemovable = true;
  };
  boot.loader.efi.canTouchEfiVariables = false;

  # virtio drivers so the cloud VM can see its disks and network
  boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" "virtio_net" "virtio_scsi" ];

  # Root filesystem on /dev/vda (first virtio disk — the image itself)
  fileSystems."/" = {
    device = "/dev/vda";
    fsType = "ext4";
  };

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

  # Disk image size — enough for NixOS + NASty closure
  virtualisation.diskSize = 8 * 1024; # 8 GiB

  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";
  system.stateVersion = "24.11";
}
