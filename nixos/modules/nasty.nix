{ config, lib, pkgs, nasty-middleware ? null, nasty-webui ? null, ... }:

let
  cfg = config.services.nasty;
  inherit (lib) mkEnableOption mkOption mkIf types;

  useSelfSigned = cfg.tls.selfSigned && cfg.tls.certFile == null && cfg.tls.keyFile == null;
  tlsCertFile = if cfg.tls.certFile != null then cfg.tls.certFile else "/var/lib/nasty/tls/cert.pem";
  tlsKeyFile = if cfg.tls.keyFile != null then cfg.tls.keyFile else "/var/lib/nasty/tls/key.pem";
in {
  options.services.nasty = {
    enable = mkEnableOption "NASty NAS management system";

    middleware = {
      package = mkOption {
        type = types.package;
        default = nasty-middleware;
        description = "NASty middleware package";
      };

      port = mkOption {
        type = types.port;
        default = 2137;
        description = "WebSocket API port";
      };

      logLevel = mkOption {
        type = types.str;
        default = "nasty_api=info";
        description = "RUST_LOG filter for middleware";
      };
    };

    webui = {
      package = mkOption {
        type = types.nullOr types.package;
        default = nasty-webui;
        description = "NASty WebUI package (static files)";
      };

      port = mkOption {
        type = types.port;
        default = 443;
        description = "WebUI HTTPS port";
      };

      httpPort = mkOption {
        type = types.port;
        default = 80;
        description = "HTTP port (redirects to HTTPS)";
      };
    };

    tls = {
      selfSigned = mkOption {
        type = types.bool;
        default = true;
        description = "Generate a self-signed TLS certificate if no cert/key files are provided";
      };

      certFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "Path to TLS certificate file. If null and selfSigned is true, a self-signed cert is generated.";
      };

      keyFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "Path to TLS private key file. If null and selfSigned is true, a self-signed key is generated.";
      };
    };

    storage = {
      mountBase = mkOption {
        type = types.str;
        default = "/mnt/nasty";
        description = "Base directory for pool mount points";
      };
    };

    nfs.enable = mkEnableOption "NFS server for NASty shares" // { default = true; };
    smb.enable = mkEnableOption "Samba server for NASty shares" // { default = true; };
    iscsi.enable = mkEnableOption "iSCSI target (LIO) for NASty" // { default = true; };
    nvmeof.enable = mkEnableOption "NVMe-oF target for NASty" // { default = true; };
  };

  config = mkIf cfg.enable {

    # ── Required kernel support ────────────────────────────────

    boot.supportedFilesystems = [ "bcachefs" ];

    boot.kernelModules = lib.flatten [
      (lib.optional cfg.iscsi.enable "target_core_mod")
      (lib.optional cfg.iscsi.enable "iscsi_target_mod")
      (lib.optional cfg.nvmeof.enable "nvmet")
      (lib.optional cfg.nvmeof.enable "nvmet-tcp")
    ];

    # ── System packages ────────────────────────────────────────

    environment.systemPackages = with pkgs; [
      bcachefs-tools
      util-linux   # lsblk, blkid, wipefs
      smartmontools  # smartctl for disk health
    ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
      ++ lib.optionals cfg.smb.enable [ samba ]
      ++ lib.optionals cfg.iscsi.enable [ targetcli-fb ]
      ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ];

    # ── State directory ────────────────────────────────────────

    systemd.tmpfiles.rules = [
      "d /var/lib/nasty 0750 root root -"
      "d /var/lib/nasty/tls 0750 root nginx -"
      "d ${cfg.storage.mountBase} 0755 root root -"
      "d /etc/exports.d 0755 root root -"
    ];

    # ── Self-signed TLS certificate ───────────────────────────

    systemd.services.nasty-selfsigned-cert = mkIf useSelfSigned {
      description = "Generate NASty self-signed TLS certificate";
      wantedBy = [ "multi-user.target" ];
      before = [ "nginx.service" ];
      requiredBy = [ "nginx.service" ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-selfsigned-cert" ''
          set -euo pipefail
          CERT="${tlsCertFile}"
          KEY="${tlsKeyFile}"

          if [ -f "$CERT" ] && [ -f "$KEY" ]; then
            echo "TLS certificate already exists, skipping generation"
            exit 0
          fi

          echo "Generating self-signed TLS certificate for NASty..."
          ${pkgs.openssl}/bin/openssl req -x509 -newkey ec \
            -pkeyopt ec_paramgen_curve:prime256v1 \
            -keyout "$KEY" -out "$CERT" \
            -days 3650 -nodes \
            -subj "/CN=nasty.local/O=NASty NAS" \
            -addext "subjectAltName=DNS:nasty.local,DNS:localhost,IP:127.0.0.1"

          chmod 640 "$KEY"
          chown root:nginx "$KEY"
          chmod 644 "$CERT"

          echo "Self-signed certificate generated at $CERT"
        '';
      };
    };

    # ── NASty Middleware service ─────────────────────────────────

    systemd.services.nasty-middleware = {
      description = "NASty Middleware";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
      ] ++ lib.optional cfg.nfs.enable "nfs-server.service"
        ++ lib.optional cfg.smb.enable "smb.service"
        ++ lib.optional cfg.nvmeof.enable "nasty-nvmeof-restore.service";

      environment = {
        RUST_LOG = cfg.middleware.logLevel;
      };

      serviceConfig = {
        ExecStart = "${cfg.middleware.package}/bin/nasty-api";
        Restart = "always";
        RestartSec = 5;
        StateDirectory = "nasty";

        # Security hardening
        ProtectHome = true;
        NoNewPrivileges = false;  # needs root for mount/format operations
        ProtectSystem = "full";
        ReadWritePaths = [
          "/var/lib/nasty"
          cfg.storage.mountBase
          "/etc/exports.d"
          "/etc/samba"
          "/sys/kernel/config"
        ];
      };
    };

    # ── NVMe-oF state restore on boot ─────────────────────────
    # nvmet configfs is volatile — we replay state from our JSON file

    systemd.services.nasty-nvmeof-restore = mkIf cfg.nvmeof.enable {
      description = "NASty NVMe-oF target restore";
      wantedBy = [ "multi-user.target" ];
      after = [ "sys-kernel-config.mount" ];
      before = [ "nasty-middleware.service" ];

      # Script that reads our state file and recreates nvmet configfs entries
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-nvmeof-restore" ''
          set -euo pipefail
          STATE="/var/lib/nasty/nvmeof-targets.json"

          if [ ! -f "$STATE" ]; then
            echo "No NVMe-oF state file, skipping restore"
            exit 0
          fi

          NVMET="/sys/kernel/config/nvmet"

          # Ensure nvmet module is loaded
          modprobe nvmet
          modprobe nvmet-tcp 2>/dev/null || true

          # Parse state file with jq and recreate entries
          SUBSYSTEMS=$(${pkgs.jq}/bin/jq -r '.subsystems[]' "$STATE" 2>/dev/null) || exit 0

          echo "$SUBSYSTEMS" | ${pkgs.jq}/bin/jq -c '.' | while IFS= read -r subsys; do
            NQN=$(echo "$subsys" | ${pkgs.jq}/bin/jq -r '.nqn')
            ALLOW_ANY=$(echo "$subsys" | ${pkgs.jq}/bin/jq -r '.allow_any_host')

            echo "Restoring NVMe-oF subsystem: $NQN"

            # Create subsystem
            mkdir -p "$NVMET/subsystems/$NQN"

            if [ "$ALLOW_ANY" = "true" ]; then
              echo 1 > "$NVMET/subsystems/$NQN/attr_allow_any_host"
            else
              echo 0 > "$NVMET/subsystems/$NQN/attr_allow_any_host"
            fi

            # Restore namespaces
            echo "$subsys" | ${pkgs.jq}/bin/jq -c '.namespaces[]' 2>/dev/null | while IFS= read -r ns; do
              NSID=$(echo "$ns" | ${pkgs.jq}/bin/jq -r '.nsid')
              DEV=$(echo "$ns" | ${pkgs.jq}/bin/jq -r '.device_path')
              ENABLED=$(echo "$ns" | ${pkgs.jq}/bin/jq -r '.enabled')

              if [ -e "$DEV" ]; then
                mkdir -p "$NVMET/subsystems/$NQN/namespaces/$NSID"
                echo "$DEV" > "$NVMET/subsystems/$NQN/namespaces/$NSID/device_path"
                if [ "$ENABLED" = "true" ]; then
                  echo 1 > "$NVMET/subsystems/$NQN/namespaces/$NSID/enable"
                fi
                echo "  Restored namespace $NSID -> $DEV"
              else
                echo "  WARNING: device $DEV not found, skipping namespace $NSID"
              fi
            done

            # Restore allowed hosts
            echo "$subsys" | ${pkgs.jq}/bin/jq -r '.allowed_hosts[]' 2>/dev/null | while IFS= read -r host_nqn; do
              mkdir -p "$NVMET/hosts/$host_nqn"
              ln -sf "$NVMET/hosts/$host_nqn" "$NVMET/subsystems/$NQN/allowed_hosts/$host_nqn" 2>/dev/null || true
            done

            # Restore ports
            echo "$subsys" | ${pkgs.jq}/bin/jq -c '.ports[]' 2>/dev/null | while IFS= read -r port; do
              PORT_ID=$(echo "$port" | ${pkgs.jq}/bin/jq -r '.port_id')
              TRTYPE=$(echo "$port" | ${pkgs.jq}/bin/jq -r '.transport')
              TRADDR=$(echo "$port" | ${pkgs.jq}/bin/jq -r '.addr')
              TRSVCID=$(echo "$port" | ${pkgs.jq}/bin/jq -r '.service_id')
              ADRFAM=$(echo "$port" | ${pkgs.jq}/bin/jq -r '.addr_family')

              mkdir -p "$NVMET/ports/$PORT_ID"
              echo "$TRTYPE" > "$NVMET/ports/$PORT_ID/addr_trtype"
              echo "$TRADDR" > "$NVMET/ports/$PORT_ID/addr_traddr"
              echo "$TRSVCID" > "$NVMET/ports/$PORT_ID/addr_trsvcid"
              echo "$ADRFAM" > "$NVMET/ports/$PORT_ID/addr_adrfam"

              ln -sf "$NVMET/subsystems/$NQN" "$NVMET/ports/$PORT_ID/subsystems/$NQN" 2>/dev/null || true
              echo "  Restored port $PORT_ID ($TRTYPE $TRADDR:$TRSVCID)"
            done
          done

          echo "NVMe-oF target restore complete"
        '';
      };
    };

    # ── Pool auto-mount on boot ────────────────────────────────
    # Re-mount bcachefs pools that were previously mounted

    systemd.services.nasty-pool-mount = {
      description = "NASty pool auto-mount";
      wantedBy = [ "multi-user.target" ];
      after = [ "local-fs.target" ];
      before = [
        "nasty-middleware.service"
        "nfs-server.service"
      ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-pool-mount" ''
          set -euo pipefail
          MOUNT_BASE="${cfg.storage.mountBase}"

          # Find bcachefs devices using blkid
          ${pkgs.util-linux}/bin/blkid -t TYPE=bcachefs -o device 2>/dev/null | while IFS= read -r dev; do
            # Get the UUID
            UUID=$(${pkgs.util-linux}/bin/blkid -s UUID -o value "$dev" 2>/dev/null) || continue

            # Check if any mount point exists for this device under our base
            for mp in "$MOUNT_BASE"/*/; do
              [ -d "$mp" ] || continue
              POOL_NAME=$(basename "$mp")

              # Try mounting if not already mounted
              if ! mountpoint -q "$mp" 2>/dev/null; then
                echo "Auto-mounting pool $POOL_NAME ($dev) at $mp"
                ${pkgs.bcachefs-tools}/bin/bcachefs mount "$dev" "$mp" || echo "  WARNING: failed to mount $dev"
              fi
            done
          done

          echo "Pool auto-mount complete"
        '';
      };
    };

    # ── NFS server ─────────────────────────────────────────────

    services.nfs.server = mkIf cfg.nfs.enable {
      enable = true;
      # nfsd will pick up /etc/exports.d/*.exports automatically
    };

    # ── Samba ──────────────────────────────────────────────────

    services.samba = mkIf cfg.smb.enable {
      enable = true;
      settings.global = {
        "server string" = "NASty NAS";
        "map to guest" = "Bad User";
        "include" = "/etc/samba/smb.nasty.conf";
      };
    };

    # ── iSCSI / LIO ───────────────────────────────────────────
    # kernel modules loaded via boot.kernelModules above
    # targetcli auto-restores from /etc/target/saveconfig.json on boot

    # ── WebUI via nginx ────────────────────────────────────────

    services.nginx = mkIf (cfg.webui.package != null) {
      enable = true;

      # Recommended TLS settings
      recommendedTlsSettings = true;
      recommendedProxySettings = true;

      virtualHosts."nasty" = {
        listen = [
          { addr = "0.0.0.0"; port = cfg.webui.httpPort; }
          { addr = "0.0.0.0"; port = cfg.webui.port; ssl = true; }
        ];
        forceSSL = true;
        root = "${cfg.webui.package}/share/nasty-webui";
        sslCertificate = tlsCertFile;
        sslCertificateKey = tlsKeyFile;

        extraConfig = ''
          add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;
        '';

        locations."/" = {
          tryFiles = "$uri $uri/ /index.html";
        };

        # Proxy WebSocket to middleware
        locations."/ws" = {
          proxyPass = "http://127.0.0.1:${toString cfg.middleware.port}";
          proxyWebsockets = true;
          priority = 500;
        };

        locations."/ws/terminal" = {
          proxyPass = "http://127.0.0.1:${toString cfg.middleware.port}";
          proxyWebsockets = true;
          priority = 400;
        };

        # Proxy API calls to middleware
        locations."/api/" = {
          proxyPass = "http://127.0.0.1:${toString cfg.middleware.port}";
        };

        locations."/health" = {
          proxyPass = "http://127.0.0.1:${toString cfg.middleware.port}";
        };
      };
    };

    # ── Firewall ───────────────────────────────────────────────

    networking.firewall.allowedTCPPorts = lib.flatten [
      [ cfg.webui.port cfg.webui.httpPort ]
      (lib.optional cfg.nfs.enable 2049)
      (lib.optional cfg.iscsi.enable 3260)
      (lib.optionals cfg.smb.enable [ 445 139 ])
      (lib.optional cfg.nvmeof.enable 4420)
    ];
  };
}
