{ config, lib, pkgs, nasty-middleware ? null, nasty-webui ? null, nasty-version ? "dev", ... }:

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

    # Enable flakes for nixos-rebuild --flake
    nix.settings.experimental-features = [ "nix-command" "flakes" ];

    # Version file for update system
    environment.etc."nasty-version".text = nasty-version;

    # Kernel modules for iSCSI/NVMe-oF are NOT auto-loaded at boot.
    # They are loaded on demand by the middleware when the user enables
    # a protocol, keeping a clean default state on fresh installs.

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
      "d /var/lib/nasty 0751 root root -"
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
        "nasty-pool-mount.service"
        "nasty-block-restore.service"
        "nasty-protocol-restore.service"
      ] ++ lib.optional cfg.nfs.enable "nfs-server.service"
        ++ lib.optional cfg.smb.enable "smb.service"
        ++ lib.optional cfg.nvmeof.enable "nasty-nvmeof-restore.service";

      path = with pkgs; [
        bashInteractive  # bash for terminal
        util-linux       # lsblk, blkid, wipefs, mount, umount
        bcachefs-tools   # bcachefs
        smartmontools    # smartctl
        git              # for update check (git ls-remote)
      ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
        ++ lib.optionals cfg.smb.enable [ samba ]
        ++ lib.optionals cfg.iscsi.enable [ targetcli-fb ]
        ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ];

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
      after = [
        "sys-kernel-config.mount"
        "nasty-pool-mount.service"
        "nasty-block-restore.service"
      ];
      before = [ "nasty-middleware.service" ];

      # Script that reads our state file and recreates nvmet configfs entries
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-nvmeof-restore" ''
          set -euo pipefail

          # Check if NVMe-oF protocol is enabled by the user
          PROTO_STATE="/var/lib/nasty/protocols.json"
          if [ -f "$PROTO_STATE" ]; then
            NVMEOF_ENABLED=$(${pkgs.jq}/bin/jq -r '.nvmeof // false' "$PROTO_STATE" 2>/dev/null)
          else
            NVMEOF_ENABLED="false"
          fi

          if [ "$NVMEOF_ENABLED" != "true" ]; then
            echo "NVMe-oF protocol is disabled, skipping restore"
            exit 0
          fi

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
    # Re-mount bcachefs pools that were previously mounted (tracked by middleware)

    systemd.services.nasty-pool-mount = {
      description = "NASty pool auto-mount";
      wantedBy = [ "multi-user.target" ];
      after = [ "local-fs.target" ];
      before = [
        "nasty-middleware.service"
        "nasty-block-restore.service"
        "nfs-server.service"
        "smb.service"
      ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-pool-mount" ''
          set -euo pipefail
          MOUNT_BASE="${cfg.storage.mountBase}"
          STATE="/var/lib/nasty/pool-state.json"

          # If no state file, fall back to mounting all known pools
          if [ ! -f "$STATE" ]; then
            echo "No pool state file, discovering and mounting all bcachefs pools"
            (${pkgs.util-linux}/bin/blkid -t TYPE=bcachefs -o device 2>/dev/null || true) | while IFS= read -r dev; do
              for mp in "$MOUNT_BASE"/*/; do
                [ -d "$mp" ] || continue
                POOL_NAME=$(basename "$mp")
                if ! mountpoint -q "$mp" 2>/dev/null; then
                  echo "Auto-mounting pool $POOL_NAME ($dev) at $mp"
                  ${pkgs.bcachefs-tools}/bin/bcachefs mount "$dev" "$mp" || echo "  WARNING: failed to mount $dev"
                fi
              done
            done
            echo "Pool auto-mount complete (fallback)"
            exit 0
          fi

          # Read pool names from state file
          POOLS=$(${pkgs.jq}/bin/jq -r '.[]' "$STATE" 2>/dev/null) || exit 0

          # Build a map of UUID -> device list from blkid
          declare -A UUID_DEVS
          while IFS= read -r line; do
            [ -z "$line" ] && continue
            DEV=""
            UUID=""
            for kv in $line; do
              case "$kv" in
                DEVNAME=*) DEV="''${kv#DEVNAME=}" ;;
                UUID=*) UUID="''${kv#UUID=}" ;;
              esac
            done
            [ -z "$UUID" ] || [ -z "$DEV" ] && continue
            UUID_DEVS[$UUID]="''${UUID_DEVS[$UUID]:-}:$DEV"
          done < <(${pkgs.util-linux}/bin/blkid -t TYPE=bcachefs -o export 2>/dev/null || true)

          echo "$POOLS" | while IFS= read -r pool_name; do
            [ -z "$pool_name" ] && continue
            MP="$MOUNT_BASE/$pool_name"

            # Skip if already mounted
            if mountpoint -q "$MP" 2>/dev/null; then
              echo "Pool $pool_name already mounted at $MP"
              continue
            fi

            # Create mount point if needed
            mkdir -p "$MP"

            # Try each UUID's devices until we find one that mounts here
            MOUNTED=false
            for uuid in "''${!UUID_DEVS[@]}"; do
              DEVLIST="''${UUID_DEVS[$uuid]}"
              # Remove leading colon and use first device for mount
              DEVLIST="''${DEVLIST#:}"
              if ${pkgs.bcachefs-tools}/bin/bcachefs mount "$DEVLIST" "$MP" 2>/dev/null; then
                echo "Mounted pool $pool_name (UUID $uuid) at $MP"
                MOUNTED=true
                break
              fi
            done

            if [ "$MOUNTED" = false ]; then
              echo "WARNING: could not mount pool $pool_name"
            fi
          done

          echo "Pool auto-mount complete"
        '';
      };
    };

    # ── Block subvolume loop device restore ──────────────────────
    # Re-attach loop devices for block subvolumes after pools are mounted

    systemd.services.nasty-block-restore = {
      description = "NASty block subvolume loop device restore";
      wantedBy = [ "multi-user.target" ];
      after = [ "nasty-pool-mount.service" ];
      before = [
        "nasty-middleware.service"
        "nasty-nvmeof-restore.service"
      ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-block-restore" ''
          set -euo pipefail
          STATE="/var/lib/nasty/subvolumes.json"
          MOUNT_BASE="${cfg.storage.mountBase}"

          if [ ! -f "$STATE" ]; then
            echo "No subvolume state file, skipping block restore"
            exit 0
          fi

          # Find all block subvolumes from state file
          ${pkgs.jq}/bin/jq -r '.[] | select(.subvolume_type == "block") | "\(.pool) \(.name)"' "$STATE" 2>/dev/null | while IFS=' ' read -r pool name; do
            [ -z "$pool" ] || [ -z "$name" ] && continue

            IMG="$MOUNT_BASE/$pool/$name/vol.img"

            if [ ! -f "$IMG" ]; then
              echo "WARNING: block image $IMG not found for $pool/$name"
              continue
            fi

            # Check if already attached
            if ${pkgs.util-linux}/bin/losetup -j "$IMG" 2>/dev/null | grep -q "$IMG"; then
              echo "Loop device already attached for $pool/$name"
              continue
            fi

            LODEV=$(${pkgs.util-linux}/bin/losetup --find --show "$IMG" 2>/dev/null) || {
              echo "WARNING: failed to attach loop device for $pool/$name"
              continue
            }

            echo "Attached $LODEV for block subvolume $pool/$name"
          done

          echo "Block subvolume restore complete"
        '';
      };
    };

    # ── Protocol restore on boot ──────────────────────────────────
    # Start/stop protocol services based on saved state

    systemd.services.nasty-protocol-restore = {
      description = "NASty protocol service restore";
      wantedBy = [ "multi-user.target" ];
      after = [
        "nasty-pool-mount.service"
        "nasty-block-restore.service"
      ] ++ lib.optional cfg.nfs.enable "nfs-server.service"
        ++ lib.optional cfg.smb.enable "smb.service";
      before = [ "nasty-middleware.service" ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-protocol-restore" ''
          set -euo pipefail
          STATE="/var/lib/nasty/protocols.json"

          # Default: all protocols disabled on fresh install (no state file).
          # Read state and stop disabled services.
          for proto in nfs smb iscsi nvmeof; do
            if [ -f "$STATE" ]; then
              ENABLED=$(${pkgs.jq}/bin/jq -r ".$proto // false" "$STATE" 2>/dev/null)
            else
              ENABLED="false"
            fi

            if [ "$ENABLED" = "false" ]; then
              case "$proto" in
                nfs)
                  echo "Stopping NFS (disabled by user)"
                  systemctl stop nfs-server.service 2>/dev/null || true
                  ;;
                smb)
                  echo "Stopping SMB (disabled by user)"
                  systemctl stop smb.service nmb.service 2>/dev/null || true
                  ;;
                iscsi)
                  echo "iSCSI disabled by user (kernel modules will not be loaded)"
                  ;;
                nvmeof)
                  echo "NVMe-oF disabled by user (kernel modules will not be loaded)"
                  ;;
              esac
            else
              echo "Protocol $proto is enabled"
            fi
          done

          echo "Protocol restore complete"
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
