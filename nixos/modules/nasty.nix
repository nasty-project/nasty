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

    # Protocol options control whether packages/firewall rules are available.
    # Actual service start/stop is managed by the middleware via protocols.json.
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
      "d /var/lib/nasty/subvolumes 0750 root root -"
      "d /var/lib/nasty/shares 0750 root root -"
      "d /var/lib/nasty/shares/nfs 0750 root root -"
      "d /var/lib/nasty/shares/smb 0750 root root -"
      "d /var/lib/nasty/shares/iscsi 0750 root root -"
      "d /var/lib/nasty/shares/nvmeof 0750 root root -"
      "d ${cfg.storage.mountBase} 0755 root root -"
      "d /etc/exports.d 0755 root root -"
      "f /etc/samba/smb.nasty.conf 0644 root root -"
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
      after = [ "network.target" ];

      path = with pkgs; [
        bashInteractive  # bash for terminal
        util-linux       # lsblk, blkid, wipefs, mount, umount
        bcachefs-tools   # bcachefs
        smartmontools    # smartctl
        iproute2         # ip (for network addresses)
        kmod             # modprobe (for iSCSI/NVMe-oF kernel modules)
        systemd          # systemctl, journalctl (for update status)
        nixos-rebuild-ng # nixos-rebuild (for system updates)
        git              # for update check (git ls-remote)
        curl             # for update check (GitHub API, TODO: remove when repo is public)
      ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
        ++ lib.optionals cfg.smb.enable [ samba ]
        ++ lib.optionals cfg.iscsi.enable [ targetcli-fb ]
        ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ];

      environment = {
        RUST_LOG = cfg.middleware.logLevel;
      };

      serviceConfig = {
        Type = "notify";
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

    # ── NFS server ─────────────────────────────────────────────
    # NFS service is NOT auto-started by NixOS — the middleware manages it.
    # We still declare the server config so nfsd is available when started.

    services.nfs.server = mkIf cfg.nfs.enable {
      enable = true;
      # Prevent NixOS from auto-starting nfs-server
      # The middleware / protocol-restore service handles start/stop
    };

    systemd.services.nfs-server.wantedBy = mkIf cfg.nfs.enable (lib.mkForce []);

    # ── Samba ──────────────────────────────────────────────────
    # Same approach: declare config but don't auto-start.

    services.samba = mkIf cfg.smb.enable {
      enable = true;
      settings.global = {
        "server string" = "NASty NAS";
        "map to guest" = "Bad User";
        "include" = "/etc/samba/smb.nasty.conf";
      };
    };

    systemd.services.smb.wantedBy = mkIf cfg.smb.enable (lib.mkForce []);
    systemd.services.nmb.wantedBy = mkIf cfg.smb.enable (lib.mkForce []);

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
