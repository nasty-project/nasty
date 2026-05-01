{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [ ./binary-cache.nix ];

  # fwupd: disable the hourly metadata refresh timer — firmware checks are
  # triggered on-demand from the UI. fwupd itself stays D-Bus activated so
  # it starts when needed and exits after the idle timeout.
  systemd.timers.fwupd-refresh.enable = false;

  # Boot loader — systemd-boot (UEFI)
  boot.loader.systemd-boot.enable = true;
  boot.loader.systemd-boot.configurationLimit = 20;
  boot.loader.efi.canTouchEfiVariables = true;

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
      # Wait up to 30s for a non-link-local IP (DHCP may take a moment)
      for i in $(seq 1 15); do
        IP=$(ip -4 route get 1.1.1.1 2>/dev/null \
          | awk '{for(i=1;i<=NF;i++) if ($i=="src") {print $(i+1); exit}}')
        if [ -z "$IP" ]; then
          IP=$(ip -4 addr show \
            | awk '/inet / && !/127\./ {print $2}' | cut -d/ -f1 | head -1)
        fi
        # Got a real IP (not link-local 169.254.x.x)?
        if [ -n "$IP" ] && ! echo "$IP" | grep -q '^169\.254\.'; then
          break
        fi
        sleep 2
      done
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
      logLevel = "nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info";
    };

    webui = {
      package = nasty-webui;
    };

    storage.mountBase = "/fs";

    nfs.enable = true;
    smb.enable = true;
    iscsi.enable = true;
    nvmeof.enable = true;
    tailscale.enable = true;
  };

  # Branding
  system.nixos.distroName = "NASty";
  system.nixos.distroId = "nasty";

  # Useful tools
  environment.systemPackages = with pkgs; [ vim file binutils git fwupd rsync iotop-c btop ];

  # Allow SSH for management
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = false; # engine overrides at runtime via Include sshd_override.conf
    };
    extraConfig = ''
      Include /var/lib/nasty/sshd_override.conf
    '';
  };

  # SSH port is managed by the engine's dynamic nftables firewall.

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
