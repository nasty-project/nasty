{ config, lib, pkgs, utils, ... }:

let
  flag = "/var/lib/nasty/maintenance";
  active = "/run/nasty-maintenance";
  webEnabled = config.services.nasty.webui.package != null;
  webScript = pkgs.writeText "maintenance-web.py" (builtins.readFile ../maintenance-web.py);
  webEnvironment = {
    NASTY_WEBUI_HTTPS_PORT = toString config.services.nasty.webui.port;
    NASTY_WEBUI_HTTP_PORT = if config.services.nasty.webui.httpPort == null then "disabled" else toString config.services.nasty.webui.httpPort;
    NASTY_MAINTENANCE_CERT = if config.services.nasty.tls.certFile == null then "" else toString config.services.nasty.tls.certFile;
    NASTY_MAINTENANCE_KEY = if config.services.nasty.tls.keyFile == null then "" else toString config.services.nasty.tls.keyFile;
    NASTY_MAINTENANCE_SSH_PORTS = builtins.toJSON (if config.services.openssh.enable then config.services.openssh.ports else []);
    NASTY_MAINTENANCE_ASSETS = toString ../maintenance-web;
  };
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
    # maintenance, open SSH and the existing Tailscale transport independently
    # of the optional landing page, so a web failure cannot take SSH away.
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

    # Root only collects a small, non-secret status snapshot. It does not parse
    # HTTP requests. Public HTTP handling is in a separate unprivileged unit.
    systemd.services.nasty-maintenance-web = lib.mkIf webEnabled {
      description = "NASty read-only maintenance readiness page";
      wantedBy = [ "multi-user.target" ];
      requires = [ "nasty-maintenance-state.service" "nftables.service" ];
      after = [ "nasty-maintenance-state.service" "nasty-maintenance-access.service" "nftables.service" ];
      partOf = [ "nftables.service" ];
      unitConfig.ConditionPathExists = active;
      environment = webEnvironment;
      path = [ pkgs.systemd ];
      serviceConfig = {
        Type = "exec";
        Group = "caddy";
        RuntimeDirectory = "nasty-maintenance-web";
        RuntimeDirectoryMode = "0750";
        UMask = "0027";
        ExecStartPre = "+${pkgs.python3}/bin/python3 ${webScript} prepare";
        ExecStart = "${pkgs.python3}/bin/python3 ${webScript} collect";
        ExecStartPost = "+${pkgs.nftables}/bin/nft --file /run/nasty-maintenance-web/web.nft";
        NoNewPrivileges = true;
        CapabilityBoundingSet = "";
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" "AF_NETLINK" ];
      };
    };

    systemd.services.nasty-maintenance-http = lib.mkIf webEnabled {
      description = "NASty unprivileged maintenance page backend";
      wantedBy = [ "multi-user.target" ];
      requires = [ "nasty-maintenance-web.service" ];
      after = [ "nasty-maintenance-web.service" ];
      partOf = [ "nftables.service" ];
      unitConfig.ConditionPathExists = active;
      environment = webEnvironment;
      serviceConfig = {
        Type = "exec";
        User = "caddy";
        Group = "caddy";
        RuntimeDirectory = "nasty-maintenance-http";
        RuntimeDirectoryMode = "0750";
        ExecStart = "${pkgs.python3}/bin/python3 ${webScript} serve";
        NoNewPrivileges = true;
        CapabilityBoundingSet = "";
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
      };
    };

    # This is NOT caddy.service: no app routes, engine proxy, admin API,
    # DNS-provider environment, or public ACME automation are restored.
    systemd.services.nasty-maintenance-caddy = lib.mkIf webEnabled {
      description = "NASty maintenance-only HTTPS listener";
      wantedBy = [ "multi-user.target" ];
      requires = [ "nasty-maintenance-http.service" ];
      after = [ "nasty-maintenance-http.service" ];
      partOf = [ "nftables.service" ];
      unitConfig.ConditionPathExists = active;
      environment.HOME = "/var/lib/nasty-maintenance-caddy";
      serviceConfig = {
        Type = "notify";
        User = "caddy";
        Group = "caddy";
        StateDirectory = "nasty-maintenance-caddy";
        StateDirectoryMode = "0700";
        ExecStart = "${config.services.caddy.package}/bin/caddy run --config /run/nasty-maintenance-web/caddy.json";
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
      };
    };

    environment.interactiveShellInit = lib.mkAfter ''
      if [ -e ${active} ]; then
        echo "NASty STORAGE MAINTENANCE: data pools and consumers are not started automatically."
        echo "Verify pools are unmounted before repair. Run 'sudo nasty-maintenance exit' to reboot normally."
      fi
    '';
  };
}
