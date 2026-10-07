{ config, lib, pkgs, utils, ... }:

let
  flag = "/var/lib/nasty/maintenance";
  active = "/run/nasty-maintenance";
  command = pkgs.writeShellApplication {
    name = "nasty-maintenance";
    runtimeInputs = [ pkgs.coreutils pkgs.util-linux pkgs.systemd ];
    text = builtins.readFile ../maintenance.sh;
  };
  # Engine-owned mounts, jobs, VMs and exports cannot be restored without the
  # engine. Guard their systemd entry points too, including socket activation.
  consumers = [
    "nasty-engine.service" "nasty-metrics.service" "caddy.service"
    "nfs-server.service" "nfs-mountd.service" "nfs-idmapd.service"
    "samba-smbd.service" "samba-nmbd.service" "samba-winbindd.service"
    "samba-wsdd.service" "samba-dc.service"
    "docker.service" "docker.socket" "docker-prune.service" "docker-prune.timer"
    "containerd.service" "containerd.socket"
    "target.service" "nvmet_tcp.service" "nasty-rest-server.service"
    "libvirtd.service" "libvirtd.socket" "libvirtd-ro.socket" "libvirtd-admin.socket"
    "smartd.service" "nasty-watchdog.service"
  ];
  dataMounts = lib.filter (path: path == "/fs" || lib.hasPrefix "/fs/" path)
    (builtins.attrNames config.fileSystems);
  mountUnits = lib.concatMap (path: [
    "${utils.escapeSystemdPath path}.mount"
    "${utils.escapeSystemdPath path}.automount"
  ]) dataMounts;
  guard = ''
    [Unit]
    ConditionPathExists=!${flag}
    ConditionPathExists=!${active}
  '';
in {
  config = lib.mkIf config.services.nasty.enable {
    environment.systemPackages = [ command ];
    systemd.services.nasty-engine.path = [ command ];

    # Generated drop-ins preserve NixOS's full unit definitions and do not
    # invent services for disabled protocols. Both flags are checked at each
    # start; daemon-reload cannot clear the latched mode after exit is queued.
    systemd.generators.nasty-maintenance = pkgs.writeShellScript "nasty-maintenance-generator" ''
      set -eu
      for unit in ${lib.escapeShellArgs (consumers ++ mountUnits)}; do
        ${pkgs.coreutils}/bin/mkdir -p "$1/$unit.d"
        ${pkgs.coreutils}/bin/cat > "$1/$unit.d/90-nasty-maintenance.conf" <<'EOF'
      ${guard}
      EOF
      done
      for unit in ${lib.escapeShellArgs consumers}; do
        ${pkgs.coreutils}/bin/cat >> "$1/$unit.d/90-nasty-maintenance.conf" <<'EOF'
      Requires=nasty-maintenance-state.service
      After=nasty-maintenance-state.service
      EOF
      done
    '';

    # Latch this boot's mode. Removing the durable flag must not permit storage
    # services to start midway through a repair, even after daemon-reload.
    systemd.services.nasty-maintenance-state = {
      description = "Latch NASty storage maintenance mode for this boot";
      wantedBy = [ "sysinit.target" ];
      after = [ "local-fs.target" ];
      before = [ "sysinit.target" ];
      unitConfig.DefaultDependencies = false;
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      script = ''
        if [ -e ${flag} ]; then
          ${pkgs.coreutils}/bin/touch ${active}
        fi
      '';
    };

    # The usual fail-closed firewall relies on the engine to open SSH. In
    # maintenance, open only SSH and the existing Tailscale transport instead.
    systemd.services.nasty-maintenance-access = {
      description = "Keep SSH and configured Tailscale available during maintenance";
      wantedBy = [ "multi-user.target" ];
      requires = [ "nasty-maintenance-state.service" "nftables.service" ];
      after = [ "nasty-maintenance-state.service" "nftables.service" "network-online.target" ];
      wants = [ "network-online.target" ] ++ lib.optional config.services.openssh.enable "sshd.service";
      partOf = [ "nftables.service" ];
      unitConfig.ConditionPathExists = active;
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      path = [ pkgs.nftables pkgs.systemd pkgs.jq ];
      script = ''
        ${lib.optionalString config.services.openssh.enable ''
          nft add rule inet nasty input tcp dport '{ ${lib.concatMapStringsSep ", " toString config.services.openssh.ports} }' accept
        ''}
        ${lib.optionalString config.services.nasty.tailscale.enable ''
          if [ -f /var/lib/nasty/tailscale.json ] && jq -e '.enabled == true' /var/lib/nasty/tailscale.json >/dev/null; then
            nft add rule inet nasty input udp dport 41641 accept
            systemctl start nasty-tailscale.service
          fi
        ''}
      '';
    };

    environment.interactiveShellInit = lib.mkAfter ''
      if [ -e ${active} ]; then
        echo "NASty STORAGE MAINTENANCE: data pools and consumers are not started automatically."
        echo "Verify pools are unmounted before repair. Run 'sudo nasty-maintenance exit' to reboot normally."
      fi
    '';
  };
}
