# VM configuration for testing NASty in QEMU
# Build:   nix build .#nixosConfigurations.nasty-vm.config.system.build.vm
# Run:     ./result/bin/run-nasty-vm
# Access:  https://localhost:8443  (WebUI)
#          ssh -p 2222 root@localhost  (SSH, password: nasty)

{ config, pkgs, lib, ... }:

{
  # VM-specific settings
  virtualisation.vmVariant = {
    virtualisation = {
      memorySize = 2048;
      cores = 2;

      # Forward host ports to VM
      forwardPorts = [
        { from = "host"; host.port = 8443; guest.port = 443; }
        { from = "host"; host.port = 8080; guest.port = 80; }
        { from = "host"; host.port = 2222; guest.port = 22; }
        { from = "host"; host.port = 2137; guest.port = 2137; }
      ];

      # Create virtual disks for testing pool operations
      # These appear as /dev/vdb, /dev/vdc, /dev/vdd
      emptyDiskImages = [ 1024 1024 1024 ]; # 3x 1GB disks
    };
  };

  # Allow root login with password for VM testing
  users.users.root.initialPassword = "nasty";
  services.openssh.settings.PermitRootLogin = lib.mkForce "yes";
  services.openssh.settings.PasswordAuthentication = lib.mkForce true;

  # Helpful MOTD for VM testing
  users.motd = ''

    ███╗   ██╗ █████╗ ███████╗████████╗██╗   ██╗   ██╗   ██╗███╗   ███╗
    ████╗  ██║██╔══██╗██╔════╝╚══██╔══╝╚██╗ ██╔╝   ██║   ██║████╗ ████║
    ██╔██╗ ██║███████║███████╗   ██║    ╚████╔╝    ██║   ██║██╔████╔██║
    ██║╚██╗██║██╔══██║╚════██║   ██║     ╚██╔╝     ╚██╗ ██╔╝██║╚██╔╝██║
    ██║ ╚████║██║  ██║███████║   ██║      ██║       ╚████╔╝ ██║ ╚═╝ ██║
    ╚═╝  ╚═══╝╚═╝  ╚═╝╚══════╝   ╚═╝      ╚═╝        ╚═══╝  ╚═╝     ╚═╝

    NASty NAS Test VM

    WebUI:      https://localhost:8443  (from host)
    API:        https://localhost:8443/api/
    SSH:        ssh -p 2222 root@localhost

    Default credentials: admin / admin

    Virtual disks for testing:
      /dev/vdb  (1 GiB)
      /dev/vdc  (1 GiB)
      /dev/vdd  (1 GiB)

    Quick test:
      systemctl status nasty-engine
      journalctl -u nasty-engine -f
      curl -k https://localhost/health

  '';
}
