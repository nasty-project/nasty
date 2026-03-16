{ config, lib, pkgs, nasty-engine ? null, nasty-webui ? null, nasty-version ? "dev", nasty-bcachefs-tools ? pkgs.bcachefs-tools, ... }:

let
  cfg = config.services.nasty;
  inherit (lib) mkEnableOption mkOption mkIf types;

  useSelfSigned = cfg.tls.selfSigned && cfg.tls.certFile == null && cfg.tls.keyFile == null;
  tlsCertFile = if cfg.tls.certFile != null then cfg.tls.certFile else "/var/lib/nasty/tls/cert.pem";
  tlsKeyFile = if cfg.tls.keyFile != null then cfg.tls.keyFile else "/var/lib/nasty/tls/key.pem";

  # targetcli-fb 3.0.2 passes `exclusive=` to rtslib-fb, but nixpkgs ships
  # rtslib-fb 2.2.3 which lacks that parameter.  Bump rtslib to 2.2.4+.
  rtslib-fb-latest = pkgs.python3Packages.rtslib-fb.overrideAttrs (old: rec {
    version = "2.2.4";
    src = pkgs.fetchPypi {
      pname = "rtslib_fb";
      inherit version;
      hash = "sha256-AITaplGnKxys0OqvFicl32m5kfUBz/6H4PZ+mSJKcmc=";
    };
  });
  targetcli-fixed = pkgs.targetcli-fb.override {
    python3Packages = pkgs.python3Packages // {
      rtslib-fb = rtslib-fb-latest;
    };
  };

  # ── Plymouth boot splash ────────────────────────────────────
  nasty-logo-png = pkgs.runCommand "nasty-logo.png" {
    nativeBuildInputs = [ pkgs.librsvg ];
  } ''
    rsvg-convert -w 300 -h 300 ${../../webui/src/lib/assets/nasty-white.svg} -o $out
  '';

  nasty-plymouth-theme = pkgs.stdenv.mkDerivation {
    name = "plymouth-theme-nasty";
    dontUnpack = true;
    installPhase = ''
      themeDir=$out/share/plymouth/themes/nasty
      mkdir -p "$themeDir"

      cp ${nasty-logo-png} "$themeDir/nasty.png"

      cat > "$themeDir/nasty.plymouth" << EOF
[Plymouth Theme]
Name=nasty
Description=NASty NAS System
ModuleName=script

[script]
ImageDir=$themeDir
ScriptFile=$themeDir/nasty.script
EOF

      cat > "$themeDir/nasty.script" << 'EOF'
Window.SetBackgroundTopColor(0.07, 0.07, 0.09);
Window.SetBackgroundBottomColor(0.07, 0.07, 0.09);

logo_image = Image("nasty.png");
logo_sprite = Sprite(logo_image);

# Position the logo in the refresh callback so Window dimensions are known.
fun refresh_callback() {
    logo_sprite.SetX(Window.GetWidth()  / 2 - logo_image.GetWidth()  / 2);
    logo_sprite.SetY(Window.GetHeight() / 2 - logo_image.GetHeight() / 2);
}
Plymouth.SetRefreshFunction(refresh_callback);
EOF
    '';
  };

in {
  options.services.nasty = {
    enable = mkEnableOption "NASty NAS management system";

    engine = {
      package = mkOption {
        type = types.package;
        default = nasty-engine;
        description = "NASty engine package";
      };

      port = mkOption {
        type = types.port;
        default = 2137;
        description = "WebSocket API port";
      };

      logLevel = mkOption {
        type = types.str;
        default = "nasty_api=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info";
        description = "RUST_LOG filter for engine";
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
        default = "/storage";
        description = "Base directory for pool mount points";
      };
    };

    # Protocol options control whether packages/firewall rules are available.
    # Actual service start/stop is managed by the engine via protocols.json.
    nfs.enable = mkEnableOption "NFS server for NASty shares" // { default = true; };
    smb.enable = mkEnableOption "Samba server for NASty shares" // { default = true; };
    iscsi.enable = mkEnableOption "iSCSI target (LIO) for NASty" // { default = true; };
    nvmeof.enable = mkEnableOption "NVMe-oF target for NASty" // { default = true; };
  };

  config = mkIf cfg.enable {

    # ── Required kernel support ────────────────────────────────
    # bcachefs kernel module + tools live in modules/bcachefs.nix

    # ── Boot splash ────────────────────────────────────────────
    boot.plymouth = {
      enable = true;
      theme = "nasty";
      themePackages = [ nasty-plymouth-theme ];
    };
    # Plymouth NixOS module adds "splash" automatically; we only add "quiet".
    boot.kernelParams = [ "quiet" ];
    boot.initrd.verbose = false;
    # Systemd in initrd: required for Plymouth to start early enough to
    # intercept boot messages. Without this Plymouth starts after systemd
    # is already printing to the console.
    boot.initrd.systemd.enable = true;
    # simpledrm uses the UEFI/OVMF EFI framebuffer (confirmed: system boots
    # via OVMF). Must be loaded in the initrd so Plymouth has a DRM device.
    boot.initrd.kernelModules = [ "simpledrm" ];

    # Enable flakes for nixos-rebuild --flake
    nix.settings.experimental-features = [ "nix-command" "flakes" ];

    # ── NTP ────────────────────────────────────────────────────
    services.timesyncd.enable = true;

    # Apply timezone saved in settings.json on every boot.
    # Runs before the engine so the correct timezone is set when it starts.
    systemd.services.nasty-apply-timezone = {
      description = "Apply NASty saved timezone";
      wantedBy = [ "multi-user.target" ];
      before = [ "nasty-engine.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-apply-timezone" ''
          SETTINGS=/var/lib/nasty/settings.json
          TZ="UTC"
          if [ -f "$SETTINGS" ]; then
            SAVED=$(${pkgs.jq}/bin/jq -r '.timezone // "UTC"' "$SETTINGS" 2>/dev/null)
            [ -n "$SAVED" ] && TZ="$SAVED"
          fi
          ${pkgs.systemd}/bin/timedatectl set-timezone "$TZ"
        '';
      };
    };

    # Version file for update system
    environment.etc."nasty-version".text = nasty-version;

    # Apply hostname saved in settings.json on every boot.
    systemd.services.nasty-apply-hostname = {
      description = "Apply NASty saved hostname";
      wantedBy = [ "multi-user.target" ];
      before = [ "nasty-engine.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "nasty-apply-hostname" ''
          SETTINGS=/var/lib/nasty/settings.json
          if [ -f "$SETTINGS" ]; then
            NAME=$(${pkgs.jq}/bin/jq -r '.hostname // ""' "$SETTINGS" 2>/dev/null)
            [ -n "$NAME" ] && ${pkgs.systemd}/bin/hostnamectl set-hostname "$NAME"
          fi
        '';
      };
    };

    # ── WebUI terminal welcome ──────────────────────────────────

    environment.etc."nasty/terminal-rc".text = ''
      # Source system-wide bashrc to get correct PATH for all tools
      [ -f /etc/bashrc ] && source /etc/bashrc

      echo ""
      echo "  Welcome to NASty!  |  $(hostname)  |  $(date '+%Y-%m-%d %H:%M %Z')"
      echo ""
      echo "  Type 'help'  to show the NASty command reference."
      echo "  Type 'debug' to dump diagnostic info for bug reports."
      echo ""

      help()  { cat /etc/nasty/debug-cheatsheet; }
      debug() { nasty-debug; }
      export -f help debug
    '';

    environment.etc."nasty/debug-cheatsheet".text = ''

      ╔══════════════════════════════════════════════════════╗
      ║               NASty Command Reference                ║
      ╚══════════════════════════════════════════════════════╝

       bcachefs — filesystem info
         bcachefs fs usage /storage/<pool>        space by type (btree, data, cached, parity …)
         bcachefs fs usage -h /storage/<pool>     human-readable sizes
         bcachefs show-super /dev/<disk>           dump superblock (UUID, features, devices)
         bcachefs device list /storage/<pool>      member devices with state and tier
         dmesg | grep -i bcachefs                  kernel messages

       bcachefs — live diagnostics (interactive, q to quit)
         bcachefs fs top /storage/<pool>           btree ops per process
         bcachefs fs timestats /storage/<pool>     op latency (min/max/mean/stddev/EWMA)

       bcachefs — device management
         bcachefs device add /storage/<pool> /dev/<disk>      add a device
         bcachefs device remove /storage/<pool> /dev/<disk>   remove a device (triggers rebalance)
         bcachefs device set-state failed /dev/<disk>         mark device failed
         bcachefs data rereplicate /storage/<pool>            rereplicate after device change

       bcachefs — subvolumes & snapshots
         bcachefs subvolume list /storage/<pool>
         bcachefs subvolume snapshot <src> <dst>

       I/O monitoring
         iotop -o
         iostat -x 1
         fio --name=seq-read    --ioengine=libaio --direct=1 --rw=read     --bs=1024k --iodepth=8  --numjobs=1 --size=1g --runtime=5 --filename=/storage/<pool>/fiotest
         fio --name=seq-write   --ioengine=libaio --direct=1 --rw=write    --bs=1024k --iodepth=8  --numjobs=1 --size=1g --runtime=5 --filename=/storage/<pool>/fiotest
         fio --name=rand-read   --ioengine=libaio --direct=1 --rw=randread --bs=4k    --iodepth=32 --numjobs=1 --size=1g --runtime=5 --filename=/storage/<pool>/fiotest
         fio --name=rand-write  --ioengine=libaio --direct=1 --rw=randwrite --bs=4k   --iodepth=32 --numjobs=1 --size=1g --runtime=5 --filename=/storage/<pool>/fiotest
         dool -dny 1

       perf profiling
         perf record -e 'bcachefs:*' -- sleep 5 && perf script
         perf record -g -p $(pgrep -f bcachefs) && perf report

       share findings with devs
         dmesg | nc termbin.com 9999
         perf script | nc termbin.com 9999
         journalctl -u nasty-engine | nc termbin.com 9999

    '';


    # Kernel modules for iSCSI/NVMe-oF are NOT auto-loaded at boot.
    # They are loaded on demand by the engine when the user enables
    # a protocol, keeping a clean default state on fresh installs.

    # ── System packages ────────────────────────────────────────

    environment.systemPackages = with pkgs; [
      util-linux        # lsblk, blkid, wipefs
      smartmontools     # smartctl for disk health
      htop

      (writeShellScriptBin "nasty-debug" ''
        set -euo pipefail

        SEP="─────────────────────────────────────────────────────"

        section() { echo ""; echo "$SEP"; echo "  $1"; echo "$SEP"; }

        echo ""
        echo "╔═════════════════════════════════════════════════════╗"
        echo "║              NASty Diagnostic Dump                  ║"
        echo "╚═════════════════════════════════════════════════════╝"
        echo "  $(date '+%Y-%m-%d %H:%M:%S %Z')  |  $(hostname)  |  NASty $(cat /etc/nasty-version 2>/dev/null || echo unknown)"

        section "System"
        echo "  OS:      $(nixos-version 2>/dev/null || echo unknown)"
        echo "  Kernel:  $(uname -r)"
        echo "  Uptime:  $(uptime -p)"
        echo "  Memory:  $(free -h | awk '/^Mem/ {print $3 " used / " $2 " total"}')"

        section "Block Devices"
        lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,MODEL 2>/dev/null || true

        section "bcachefs Pools"
        for mp in /storage/*/; do
          pool=$(basename "$mp")
          echo ""
          echo "  Pool: $pool  ($mp)"
          bcachefs fs usage -h "$mp" 2>/dev/null || echo "  (not mounted or error)"
          echo ""
          echo "  Devices:"
          bcachefs device list "$mp" 2>/dev/null | sed 's/^/    /' || true
        done
        if ! ls /storage/*/ >/dev/null 2>&1; then
          echo "  (no mounted pools)"
        fi

        section "Engine State — Protocols"
        cat /var/lib/nasty/protocols.json 2>/dev/null | ${pkgs.jq}/bin/jq . || echo "  (not found)"

        section "Engine State — Subvolumes"
        count=$(ls /var/lib/nasty/subvolumes/*.json 2>/dev/null | wc -l || echo 0)
        echo "  $count subvolume(s)"
        for f in /var/lib/nasty/subvolumes/*.json 2>/dev/null; do
          [ -f "$f" ] || continue
          ${pkgs.jq}/bin/jq -r '  "  • \(.name)  pool=\(.pool)  type=\(.subvolume_type)  \(if .volsize_bytes then "size=\(.volsize_bytes / 1048576 | floor)MiB" else "" end)"' "$f" 2>/dev/null || true
        done

        section "Engine State — Shares"
        for proto in nfs smb iscsi nvmeof; do
          count=$(ls /var/lib/nasty/shares/$proto/*.json 2>/dev/null | wc -l || echo 0)
          [ "$count" -gt 0 ] || continue
          echo "  $proto ($count share(s)):"
          for f in /var/lib/nasty/shares/$proto/*.json; do
            [ -f "$f" ] || continue
            ${pkgs.jq}/bin/jq -r '. | "    • \(.id[:8])  \(if .path then .path elif .nqn then .nqn elif .iqn then .iqn elif .name then .name else "" end)"' "$f" 2>/dev/null || true
          done
        done

        section "Active Mounts"
        mount | grep -E 'bcachefs|nfs|cifs|loop' | sed 's/^/  /' || echo "  (none)"

        section "Loop Devices"
        losetup -l 2>/dev/null | sed 's/^/  /' || echo "  (none)"

        section "Services"
        for svc in nasty-engine nfs-server samba-smbd target nvmet_tcp sshd; do
          state=$(systemctl is-active "$svc.service" 2>/dev/null || echo inactive)
          printf "  %-20s %s\n" "$svc" "$state"
        done

        section "Kernel Modules (storage/sharing)"
        lsmod | grep -E '^(bcachefs|nvmet|iscsi_target|target_core|nvme)' | awk '{printf "  %-30s %s\n", $1, $3}' || echo "  (none)"

        section "Recent Engine Logs (last 50 lines)"
        journalctl -u nasty-engine -n 50 --no-pager 2>/dev/null | sed 's/^/  /' || echo "  (unavailable)"

        section "dmesg — bcachefs / storage errors (last 30)"
        dmesg --level=err,warn -T 2>/dev/null | grep -iE 'bcachefs|nvme|scsi|ata|disk|i/o error' | tail -30 | sed 's/^/  /' || echo "  (none)"

        echo ""
        echo "$SEP"
        echo "  Share full output:  nasty-debug | nc termbin.com 9999"
        echo "$SEP"
        echo ""
      '')
      # bcachefs debugging
      perf               # perf record/report/script
      fio               # storage benchmarking
      iotop             # per-process I/O monitoring
      sysstat           # iostat, pidstat
      lsof              # open file handles
      strace            # syscall tracing
      dool              # system resource stats (dstat successor)
      netcat-gnu        # share output with devs: cmd | nc termbin.com 9999
      psmisc            # fuser, killall
      pciutils          # lspci for hardware identification
    ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
      ++ lib.optionals cfg.smb.enable [ samba ]
      ++ lib.optionals cfg.iscsi.enable [ targetcli-fixed ]
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
      "d /etc/target 0750 root root -"
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

    # ── NASty Engine service ─────────────────────────────────

    systemd.services.nasty-engine = {
      description = "NASty Engine";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      path = with pkgs; [
        bashInteractive  # bash for terminal
        util-linux       # lsblk, blkid, wipefs, mount, umount
        nasty-bcachefs-tools  # bcachefs
        smartmontools    # smartctl
        iproute2         # ip (for network addresses)
        kmod             # modprobe (for iSCSI/NVMe-oF kernel modules)
        systemd          # systemctl, journalctl (for update status)
        nixos-rebuild-ng # nixos-rebuild (for system updates)
        git              # for update check (git ls-remote)
        curl             # for update check (GitHub API, TODO: remove when repo is public)
      ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
        ++ lib.optionals cfg.smb.enable [ samba ]
        ++ lib.optionals cfg.iscsi.enable [ targetcli-fixed ]
        ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ];

      environment = {
        RUST_LOG = cfg.engine.logLevel;
      };

      serviceConfig = {
        Type = "notify";
        ExecStart = "${cfg.engine.package}/bin/nasty-api";
        Restart = "always";
        RestartSec = 5;
        StateDirectory = "nasty";

        # No filesystem sandboxing (ProtectSystem, ProtectHome, etc.) — any of
        # these create a private mount namespace, making pool mounts invisible
        # to NFS/SMB/iSCSI services.  The engine is a privileged system manager;
        # security is enforced at the API authentication layer.
        NoNewPrivileges = false;  # needs root for mount/format operations
      };
    };

    # ── NFS server ─────────────────────────────────────────────
    # NFS service is NOT auto-started by NixOS — the engine manages it.
    # We still declare the server config so nfsd is available when started.

    services.nfs.server = mkIf cfg.nfs.enable {
      enable = true;
      # Prevent NixOS from auto-starting nfs-server
      # The engine handles start/stop via protocol management
    };

    # NFSv4 only — simpler, needs only port 2049 (no rpcbind/portmapper)
    services.nfs.settings = mkIf cfg.nfs.enable {
      nfsd.vers2 = false;
      nfsd.vers3 = false;
      nfsd.vers4 = true;
      nfsd."vers4.1" = true;
      nfsd."vers4.2" = true;
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

    # Prevent Samba from auto-starting at boot. NixOS enables samba.target in
    # multi-user.target, which then pulls in all three daemons via samba.target.wants.
    # Override the target's wantedBy to break that chain; the engine starts Samba
    # on demand when the user enables the protocol.
    systemd.targets.samba.wantedBy = mkIf cfg.smb.enable (lib.mkForce []);

    # ── iSCSI / LIO ───────────────────────────────────────────
    # target.service: restore LIO config from /etc/target/saveconfig.json.
    # Not started at boot — the nasty-engine starts it on demand after
    # loading kernel modules and patching device paths.
    systemd.services.target = mkIf cfg.iscsi.enable {
      description = "LIO iSCSI target restore";
      path = [ targetcli-fixed ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.bash}/bin/bash -c 'test -f /etc/target/saveconfig.json && ${targetcli-fixed}/bin/targetcli restoreconfig /etc/target/saveconfig.json || true'";
        ExecStop = "${targetcli-fixed}/bin/targetcli clearconfig confirm=True";
      };
    };

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

        # Proxy WebSocket to engine
        locations."/ws" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
          proxyWebsockets = true;
          priority = 500;
        };

        locations."/ws/terminal" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
          proxyWebsockets = true;
          priority = 400;
        };

        # Proxy API calls to engine
        locations."/api/" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
        };

        locations."/health" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
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
