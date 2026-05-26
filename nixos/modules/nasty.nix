args@{ config, lib, pkgs, nasty-engine ? null, nasty-webui ? null, nasty-version ? "dev", nasty-bcachefs-tools ? pkgs.bcachefs-tools, ... }:

let
  cfg = config.services.nasty;
  inherit (lib) mkEnableOption mkOption mkIf types;
  nastySystemFlakeSnapshot = args.nastySystemFlakeSnapshot or null;

  # Secure Boot integration is opt-in per box. The wrapper flake at
  # /etc/nixos/flake.nix passes the lanzaboote input through as a
  # specialArg when it has the input declared; pre-#324-era wrappers
  # don't, in which case lanzaboote stays null and any attempt to set
  # `services.nasty.secureBoot.enable = true` trips a clear assertion
  # (rather than failing with a cryptic "option boot.lanzaboote.enable
  # does not exist" message). The fix path is to re-render the wrapper
  # by running any upgrade once on the new engine.
  lanzaboote = args.lanzaboote or null;
  lanzabooteAvailable = lanzaboote != null;

  # When the operator supplies a cert + key (cfg.tls.certFile/keyFile),
  # Caddy serves those for the :443 catch-all. Otherwise we use Caddy's
  # `tls internal` directive — Caddy's "Local Authority" issues a
  # self-signed cert into its own state dir (`/var/lib/caddy/...`) and
  # auto-renews it.
  #
  # The internal-CA leaf TTL is overridden to 7 days via the engine's
  # `apps.tls.automation` admin-API push (see `build_tls_automation_json`
  # in engine/nasty-apps/src/caddy.rs) — Caddyfile's long-form
  # `tls { issuer internal { lifetime ... } }` creates a second
  # automation policy for `nasty.local` and conflicts with the one
  # auto-generated from the site address, so we keep the Caddyfile in
  # its short-form `tls internal` shape and lift the lifetime override
  # into the engine's runtime PATCH instead.
  useUserCert = cfg.tls.certFile != null && cfg.tls.keyFile != null;
  caddyTlsDirective =
    if useUserCert
    then "tls ${cfg.tls.certFile} ${cfg.tls.keyFile}"
    else "tls internal";

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
  # Import the lanzaboote NixOS module — and the small sub-module
  # that uses its options — only when the wrapper passed lanzaboote
  # through. Splitting the lanzaboote-using config into a separate
  # file (`./nasty-secure-boot.nix`) keeps `boot.lanzaboote.*`
  # references out of this file's evaluation, so configurations
  # that import `nasty.nix` without threading lanzaboote (notably
  # the integration tests in `nixos/tests/`) don't trip
  # option-existence validation. On older wrappers without the
  # input, this is just `[]` — the option below stays unflippable
  # and the assertion catches anyone who flips it anyway.
  imports = lib.optionals lanzabooteAvailable [
    lanzaboote.nixosModules.lanzaboote
    ./nasty-secure-boot.nix
  ];

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
        default = "nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,nasty_apps=info,tower_http=info";
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
        default = "/fs";
        description = "Base directory for filesystem mount points";
      };
    };

    # Protocol options control whether packages/firewall rules are available.
    # Actual service start/stop is managed by the engine via protocols.json.
    nfs.enable = mkEnableOption "NFS server for NASty shares" // { default = true; };
    smb.enable = mkEnableOption "Samba server for NASty shares" // { default = true; };
    iscsi.enable = mkEnableOption "iSCSI target (LIO) for NASty" // { default = true; };
    nvmeof.enable = mkEnableOption "NVMe-oF target for NASty" // { default = true; };
    nut.enable = mkEnableOption "NUT (Network UPS Tools) for NASty";

    # VPN — not enabled by default (requires Tailscale auth key)
    tailscale.enable = mkEnableOption "Tailscale VPN for NASty";

    # ── Secure Boot (opt-in, per box) ──────────────────────────
    # Lanzaboote-backed UEFI Secure Boot. Off by default everywhere;
    # operators flip it on per-box only after their firmware is in
    # Setup Mode and the on-box `sbctl enroll-keys` ceremony has
    # been arranged. Without SB on, NASty's TPM2 sealing of bcachefs
    # keys is bound to PCR-7 — which on a stock NixOS install is
    # essentially a constant (PCR-7 measures Secure Boot policy, and
    # "SB disabled" is the same value on every box). The seal looks
    # right but a box-theft attacker can boot any OS and still
    # unseal. With SB on, PCR-7 binds to NASty-owned firmware keys,
    # so an attacker can't satisfy the policy without booting the
    # signed NASty stub.
    #
    # See `docs/adr/0001-secure-boot-via-lanzaboote.md` for the full
    # picture; PR #2 (enrollment ceremony) is the operator-facing
    # half of the workflow.
    secureBoot.enable = mkEnableOption "lanzaboote-backed UEFI Secure Boot (opt-in, per box)";
  };

  config = lib.mkMerge [

    # ── Secure Boot: bits that touch stock NixOS options only ───
    # The lanzaboote-specific settings (`boot.lanzaboote.*`) live in
    # `./nasty-secure-boot.nix`, which is only imported when
    # lanzaboote was actually passed through — so they don't trip
    # option-existence validation in test configurations.
    # `boot.loader.systemd-boot.enable` is a stock NixOS option and
    # is always safe to set here.
    (mkIf (cfg.enable && cfg.secureBoot.enable && lanzabooteAvailable) {
      # Lanzaboote installs systemd-boot itself (signed); the NixOS
      # `boot.loader.systemd-boot.enable` option conflicts with that
      # install path, so it must be forced off when lanzaboote takes
      # over the loader.
      boot.loader.systemd-boot.enable = lib.mkForce false;
    })

    # ── Catch operator-error: SB enabled on a wrapper that doesn't
    # carry the lanzaboote input. Pre-this-PR wrappers will hit this
    # if someone flips the knob; the fix is to run any upgrade once
    # so the new engine re-renders the wrapper template with the
    # `lanzaboote.url` input added.
    (mkIf (cfg.enable && cfg.secureBoot.enable && !lanzabooteAvailable) {
      assertions = [
        {
          assertion = false;
          message = ''
            services.nasty.secureBoot.enable = true requires the wrapper
            flake at /etc/nixos/flake.nix to declare the lanzaboote input.
            Your wrapper is out of date — run any upgrade once on the new
            NASty engine to re-render it (the new template includes the
            lanzaboote input), then this option will work.
          '';
        }
      ];
    })

    (mkIf cfg.enable {

    # ── Required kernel support ────────────────────────────────
    # bcachefs kernel module + tools live in modules/bcachefs.nix

    # ── Boot splash ────────────────────────────────────────────
    boot.plymouth = {
      enable = true;
      theme = "nasty";
      themePackages = [ nasty-plymouth-theme ];
    };
    # Plymouth NixOS module adds "splash" automatically; we only add "quiet".
    # IOMMU enabled for VFIO passthrough (VM feature). Harmless when unused.
    boot.kernelParams = [ "quiet" "intel_iommu=on" "amd_iommu=on" "iommu=pt" ];

    # VFIO modules for PCI passthrough (loaded on demand, not at boot).
    boot.kernelModules = [ "vfio-pci" "vfio" "vfio_iommu_type1" ];
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

    # ── Nix garbage collection ─────────────────────────────────
    # Automatic weekly cleanup of old generations and unreferenced store paths.
    # configurationLimit in boot.loader caps boot menu entries separately.
    nix.gc = {
      automatic = true;
      dates = "weekly";
      options = "--delete-older-than 30d";
    };

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

    # OVMF UEFI firmware for QEMU virtual machines
    environment.etc."nasty/ovmf/OVMF_CODE.fd".source = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd";
    environment.etc."nasty/ovmf/OVMF_VARS.fd".source = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd";

    # qemu-bridge-helper attaches a VM's tap to a bridge (e.g. br0) — needs
    # CAP_NET_ADMIN, which NixOS does not give it by default. Without this
    # wrapper, `qemu-system-* -netdev bridge,br=...` fails for VMs configured
    # in bridge mode. The allow-list is `allow all` because the only user
    # that ever invokes the helper on a NASty appliance is nasty-engine
    # itself (running as root via the systemd unit) — so a static config
    # avoids having the engine rewrite /etc/qemu/bridge.conf on every
    # bridge change in the WebUI.
    security.wrappers.qemu-bridge-helper = {
      source = "${pkgs.qemu}/libexec/qemu-bridge-helper";
      capabilities = "cap_net_admin+ep";
      owner = "root";
      group = "root";
    };
    environment.etc."qemu/bridge.conf".text = "allow all\n";

    # Keep a generation-owned copy of the managed wrapper flake in /etc so the
    # exact flake used to build the active generation is readable from
    # /run/current-system/etc/nasty-system-flake and can be restored on boot.
    environment.etc."nasty-system-flake" = mkIf (nastySystemFlakeSnapshot != null) {
      source = nastySystemFlakeSnapshot;
    };

    systemd.services.recover-generation-flake = mkIf (nastySystemFlakeSnapshot != null) {
      description = "Recover /etc/nixos flake files from the active system generation";
      wantedBy = [ "multi-user.target" ];
      before = [ "nasty-engine.service" ];
      after = [ "local-fs.target" ];
      aliases = [ "nasty-restore-system-flake.service" ];
      restartTriggers = [ nastySystemFlakeSnapshot ];
      unitConfig.ConditionPathExists = "/etc/nasty-system-flake/flake.nix";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "recover-generation-flake" ''
          set -euo pipefail

          ${pkgs.coreutils}/bin/install -d -m 0755 /etc/nixos

          sync_file() {
            local name="$1"
            local src="/etc/nasty-system-flake/$name"
            local dst="/etc/nixos/$name"

            if [ ! -e "$src" ]; then
              return 0
            fi

            if [ -e "$dst" ] && ${pkgs.diffutils}/bin/cmp -s "$src" "$dst"; then
              return 0
            fi

            ${pkgs.coreutils}/bin/install -m 0644 -T "$src" "$dst"
          }

          sync_file flake.nix
          sync_file flake.lock
        '';
      };
    };

    # systemd starts the recovery unit on boot; activation starts it again so a
    # freshly switched generation re-applies its own flake snapshot immediately.
    system.activationScripts.recover-generation-flake = mkIf (nastySystemFlakeSnapshot != null) ''
      if [ -d /run/systemd/system ]; then
        ${pkgs.systemd}/bin/systemctl start recover-generation-flake.service >/dev/null 2>&1 || true
      fi
    '';

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
            [ -n "$NAME" ] && echo "$NAME" > /proc/sys/kernel/hostname || true
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
      echo "  Type 'help'       to show bcachefs command reference."
      echo "  Type 'system'     to show NASty system commands."
      echo "  Type 'debug'      to show advanced debugging (perf, oops)."
      echo "  Type 'benchmark'  to show storage benchmark commands."
      echo "  Type 'report'     to dump diagnostic info for bug reports."
      echo ""

      help()      { cat /etc/nasty/help-cheatsheet; }
      system()    { cat /etc/nasty/system-cheatsheet; }
      debug()     { cat /etc/nasty/debug-cheatsheet; }
      benchmark() { cat /etc/nasty/benchmark-cheatsheet; }
      report()    { nasty-report; }
      export -f help system debug benchmark report
    '';

    environment.etc."nasty/help-cheatsheet".text = ''

      ╔══════════════════════════════════════════════════════╗
      ║               NASty Command Reference                ║
      ╚══════════════════════════════════════════════════════╝

       Examples use /fs/first — replace if your filesystem differs.

       bcachefs — filesystem info
         bcachefs fs usage /fs/first        space by type (btree, data, cached, parity …)
         bcachefs fs usage -h /fs/first     human-readable sizes
         bcachefs show-super /dev/<disk>           dump superblock (UUID, features, devices)
         bcachefs fs usage /fs/first           member devices with state, usage, and data types
         dmesg | grep -i bcachefs                  kernel messages

       bcachefs — live diagnostics (interactive, q to quit)
         nasty-top                                  per-device IO, latency, time stats, tuning advisor
         bcachefs fs top /fs/first           btree ops per process
         bcachefs fs timestats /fs/first     op latency (min/max/mean/stddev/EWMA)

       bcachefs — device management
         bcachefs device add /fs/first /dev/<disk>      add a device
         bcachefs device remove /fs/first /dev/<disk>   remove a device (triggers rebalance)
         bcachefs device set-state failed /dev/<disk>         mark device failed
         bcachefs device evacuate /fs/first /dev/<disk>  move data off a device before removal

       bcachefs — extended attributes
         getfattr -R -d -m "^bcachefs\\." /fs/first    list all bcachefs xattrs (compression, replicas, etc.)
         getfattr -d -m "^bcachefs\\." /fs/first/mydir  xattrs on a specific file or directory

       bcachefs — subvolumes & snapshots
         bcachefs subvolume list /fs/first
         bcachefs subvolume snapshot <src> <dst>

       I/O monitoring
         iotop-c -o
         iostat -x 1
         dool -dny 1
         btop                                       interactive CPU/mem/disk/net dashboard
         # → type 'debug' for perf profiling and kernel oops symbolization
         # → type 'benchmark' for fio storage tests

       rustic — backup (restic-compatible, deduplicating, encrypted)
         rustic -r /path/to/repo -p <password> init              initialize a new backup repo
         rustic -r /path/to/repo -p <password> backup /fs/first  back up a filesystem
         rustic -r /path/to/repo -p <password> snapshots         list snapshots
         rustic -r /path/to/repo -p <password> restore <id> /restore/target  restore a snapshot
         rustic -r /path/to/repo -p <password> forget --prune \
           --keep-daily 7 --keep-weekly 4 --keep-monthly 6       prune old snapshots
         rustic -r /path/to/repo -p <password> check             verify repo integrity
         # Managed via WebUI → Backups, or configure via API

    '';

    environment.etc."nasty/debug-cheatsheet".text = ''

      ╔══════════════════════════════════════════════════════╗
      ║             NASty Advanced Debugging                 ║
      ╚══════════════════════════════════════════════════════╝

       perf profiling
         perf top                                                  live per-symbol CPU usage (all processes)
         perf top -p $(pgrep -f bcachefs)                         live CPU usage scoped to bcachefs process
         perf record -e 'bcachefs:*' -- sleep 5 && perf script    capture bcachefs tracepoints
         perf record -g -p $(pgrep -f bcachefs) && perf report    call-graph profile of bcachefs process

       trace-cmd — kernel ftrace frontend
         trace-cmd list -e 'bcachefs:*'                            list available bcachefs tracepoints
         trace-cmd record -e 'bcachefs:*' sleep 5                  capture 5s of bcachefs events
         trace-cmd report                                          show captured trace (reads trace.dat)
         trace-cmd record -e block:block_rq_complete sleep 5       trace block I/O completions
         trace-cmd record -p function_graph -g bch2_write sleep 3  function call graph for bch2_write
         trace-cmd stream -e 'bcachefs:*' | tee logfile.txt      live-stream bcachefs events to terminal + file
         trace-cmd stream -e bcachefs:reconcile_data             watch reconcile progress in real time

       kernel oops symbolization (bcachefs crash)
         # From an oops line like: RIP: 0010:bch2_btree_node_get+0x8d/0x5f0 [bcachefs]
         faddr2line bch2_btree_node_get+0x8d/0x5f0
         # To look at raw disassembly around the fault:
         objdump -d $(find /run/current-system/kernel-modules -name "bcachefs.ko*" | head -1) | grep -A 20 "<bch2_btree_node_get>"
         # Capture full oops for the bcachefs devs:
         dmesg | grep -A 50 "RIP:" | nc termbin.com 9999

       bcachefs module: debug symbols
         # Check if the loaded .ko has DWARF debug info (needed for faddr2line source lines)
         xz -dc $(modinfo bcachefs -F filename) | file -    # look for "with debug_info" in output
         # Quick yes/no:
         xz -dc $(modinfo bcachefs -F filename) | file - | grep -q debug_info && echo "YES" || echo "NO"

       bcachefs module: debug checks (CONFIG_BCACHEFS_DEBUG)
         # Debug-only module params (journal_seq_verify, inject_invalid_keys, etc.)
         # are only compiled in when CONFIG_BCACHEFS_DEBUG is set.
         # /sys/module/ reflects the loaded module; modinfo reads the .ko on disk.
         # Loaded module (survives DKMS rebuild until reboot):
         test -e /sys/module/bcachefs/parameters/journal_seq_verify && echo "YES" || echo "NO"
         # On-disk module (what will be loaded after reboot):
         modinfo bcachefs -F parm | grep -q journal_seq_verify && echo "YES" || echo "NO"

       share findings with devs
         cat /var/lib/nasty/bcachefs-switch.log       # bcachefs version switch history
         dmesg | nc termbin.com 9999
         perf script | nc termbin.com 9999
         journalctl -u nasty-engine | nc termbin.com 9999
         journalctl -u nasty-bcachefs-switch | nc termbin.com 9999

    '';


    environment.etc."nasty/benchmark-cheatsheet".text = ''

      ╔══════════════════════════════════════════════════════╗
      ║            NASty Benchmark Reference                 ║
      ╚══════════════════════════════════════════════════════╝

       fio — storage tests  (examples use /fs/first — replace if your filesystem differs)
         # Sequential read — large block, measures throughput
         fio --name=seq-read \
             --ioengine=libaio --direct=1 --rw=read \
             --bs=1024k --iodepth=8 --numjobs=1 \
             --size=1g --runtime=30 \
             --filename=/fs/first/fiotest

         # Sequential write
         fio --name=seq-write \
             --ioengine=libaio --direct=1 --rw=write \
             --bs=1024k --iodepth=8 --numjobs=1 \
             --size=1g --runtime=30 \
             --filename=/fs/first/fiotest

         # Random read — small block, measures IOPS
         fio --name=rand-read \
             --ioengine=libaio --direct=1 --rw=randread \
             --bs=4k --iodepth=32 --numjobs=4 \
             --size=1g --runtime=30 \
             --filename=/fs/first/fiotest

         # Random write
         fio --name=rand-write \
             --ioengine=libaio --direct=1 --rw=randwrite \
             --bs=4k --iodepth=32 --numjobs=4 \
             --size=1g --runtime=30 \
             --filename=/fs/first/fiotest

         # Clean up test file afterwards
         rm -f /fs/first/fiotest

       share results with devs
         fio ... | nc termbin.com 9999

    '';

    environment.etc."nasty/system-cheatsheet".text = ''

      ╔══════════════════════════════════════════════════════╗
      ║             NASty System Commands                    ║
      ╚══════════════════════════════════════════════════════╝

       nasty — built-in tools
         nasty-top                                  live IO, latency, tuning advisor
         nasty-report                               dump diagnostic info for bug reports
         nasty-cleanup                              remove old generations + garbage collect
         nasty-rebuild                              force rebuild from current /etc/nixos

       NixOS — system management
         nixos-rebuild switch --flake /etc/nixos#nasty     rebuild and activate
         nixos-rebuild boot --flake /etc/nixos#nasty       rebuild, activate on next boot
         nix-env --list-generations -p /nix/var/nix/profiles/system   list generations
         nix-env --delete-generations +3 -p /nix/var/nix/profiles/system   keep last 3

       NixOS — flake management
         nix flake update nasty --flake /etc/nixos         pull latest NASty
         nix flake metadata /etc/nixos                     show current input revisions

       disk space
         df -h /                                           root partition usage
         nix-collect-garbage                               remove unreferenced store paths
         nix-collect-garbage -d                             also delete old generations
         du -sh /nix/store | sort -h | tail -20            biggest store paths

       services
         systemctl status nasty-engine                     engine status
         systemctl restart nasty-engine                    restart engine
         journalctl -u nasty-engine -f                     follow engine logs
         journalctl -u nasty-engine --since "1h ago"       last hour of logs

       networking
         ip addr                                           show all interfaces
         ip route                                          show routes
         ss -tlnp                                          listening ports

    '';

    # Kernel modules for iSCSI/NVMe-oF are NOT auto-loaded at boot.
    # They are loaded on demand by the engine when the user enables
    # a protocol, keeping a clean default state on fresh installs.

    # ── Firmware updates (fwupd) ────────────────────────────────
    services.fwupd.enable = true;

    # ── Docker (app runtime, disabled by default) ──────────────
    # Docker is installed but NOT started automatically. The engine starts it
    # via systemctl when the user enables apps from the WebUI.
    virtualisation.docker = {
      enable = true;
      autoPrune.enable = true;
    };
    # Prevent Docker from starting on boot — engine starts it on demand.
    systemd.services.docker.wantedBy = lib.mkForce [];
    systemd.sockets.docker.wantedBy = lib.mkForce [];

    # ── System packages ────────────────────────────────────────

    environment.systemPackages = with pkgs; [
      util-linux        # lsblk, blkid, wipefs
      parted            # partition management (parted, partprobe)
      gptfdisk          # GPT partition tools (sgdisk)
      cloud-utils       # growpart for expanding partitions
      smartmontools     # smartctl for disk health
      nvme-cli          # NVMe drive health, SMART, firmware
      hdparm            # HDD spin-down, drive parameters
      lm_sensors        # CPU/drive temperature monitoring
      lsof              # open file debugging ("device busy")
      keyutils          # keyctl — used by the engine to lock encrypted FSes
      dmidecode         # DMI tables — engine reads baseboard/BIOS/memory for /system/hardware
      usbutils          # lsusb — engine reads USB device tree for /system/hardware
      iotop-c           # per-process I/O monitoring
      ethtool           # NIC speed, duplex, ring buffer tuning
      iperf3            # network throughput testing
      tcpdump           # packet capture for protocol debugging
      rsync             # file transfer and sync
      jq                # JSON parsing (used by engine scripts)
      htop
      python3           # scripting and quick data processing
      uv                # fast Python package manager (uv + uvx — `uvx <tool>` for one-shots)
      file              # file type identification
      tree              # directory structure visualization
      eza               # modern ls replacement (colors, git, tree)
      strace            # system call tracing for debugging
      trace-cmd         # ftrace frontend for kernel tracing
      dig               # DNS debugging
      openssl           # cert inspection (x509 -text), TLS handshake debugging (s_client)
      nano              # quick file editing
      qemu              # QEMU/KVM for virtual machines
      pciutils          # lspci for passthrough device discovery
      tpm2-tools        # tpm2_getcap, tpm2_pcrread, tpm2_unseal — engine reads vendor info via tpm2_getcap, operators use the rest for TPM debugging
      sbctl             # Secure Boot key + signing-state inspector. Used by operators directly (`sbctl status`, `sbctl verify`, `sbctl list-enrolled-keys`); the lanzaboote module also drives it via the `autoGenerateKeys` install hook to create `/var/lib/sbctl` keys on first boot. NASty itself only ever reads — signing/enrollment writes go through lanzaboote, never direct sbctl calls.
      docker-compose    # Docker Compose for multi-container apps
      croc              # peer-to-peer file transfer for sending debug reports
      rustic             # deduplicating encrypted backups (restic-compatible)
      restic-rest-server # REST API server for receiving backups from other machines
      (pkgs.rustPlatform.buildRustPackage {
        pname = "nasty-top";
        version = "0.0.5";
        src = pkgs.fetchFromGitHub {
          owner = "nasty-project";
          repo = "nasty-top";
          rev = "v0.0.5";
          hash = "sha256-8oefkH5hRIYYuECdmVUsbCtqnTJxC3IgwGoPQppmTYc=";
        };
        cargoHash = "sha256-d1gVWeQMLx03/qE62qY4Yk+Xqa/bWfdpIg8sjV4XX50=";
        meta.mainProgram = "nasty-top";
      })

      (writeShellScriptBin "nasty-cleanup" ''
        set -euo pipefail
        echo "==> Removing old NixOS generations (keeping last 3)..."
        nix-env --delete-generations +3 -p /nix/var/nix/profiles/system 2>/dev/null || true
        echo "==> Running garbage collection..."
        nix-collect-garbage 2>&1
        # Re-sync /boot to match the surviving generations: GC removed
        # their store paths but systemd-boot's entries (and the kernel +
        # initrd copies under /boot) stick around until we rebuild the
        # bootloader.  Without this the /boot-full alert keeps firing
        # even after nix-collect-garbage reclaimed everything in /.
        echo "==> Re-syncing bootloader entries (/boot cleanup)..."
        /run/current-system/bin/switch-to-configuration boot
        echo "==> Done."
        df -h / /boot
      '')

      (writeShellScriptBin "nasty-rebuild" ''
        set -euo pipefail
        echo "==> Rebuilding NASty from /etc/nixos..."
        nixos-rebuild switch --flake /etc/nixos#nasty
        NASTY_REV=$(${pkgs.jq}/bin/jq -r '.nodes["nasty"].locked.rev // empty' /etc/nixos/flake.lock 2>/dev/null || true)
        [ -n "$NASTY_REV" ] && echo "''${NASTY_REV:0:7}" > /var/lib/nasty/version
        echo "==> Done. Running: $(cat /var/lib/nasty/version 2>/dev/null || echo unknown)"
      '')

      # `nasty-sync` — the engine-bypass recovery + manual update
      # CLI. Four modes:
      #
      #   nasty-sync                Bump nasty to current main HEAD,
      #                             rebuild. bcachefs-tools untouched.
      #   nasty-sync -b [<ref>]     Bump nasty AND bcachefs-tools.
      #                             Without <ref>: bcachefs adopts the
      #                             rev nasty's new main HEAD declares
      #                             (atomic dev-build bundle).
      #                             With <ref>: bcachefs pinned to
      #                             <ref> (e.g. v1.37.2 to downgrade
      #                             when a newer rev regressed).
      #   nasty-sync -r             Recovery: canonicalize the wrapper
      #                             back to `bcachefs-tools.url = ...`
      #                             shape using the rev currently in
      #                             flake.lock (fixes follows-shape AND
      #                             corrupted-ref states), then update
      #                             nasty + rebuild.
      #   nasty-sync -s             Read-only: show current state.
      #
      # Why this exists at all: engine-side regressions in
      # `system.version.switch` / `system.update.apply` can strand a
      # box on the very RPC path that would deploy the fix (the old
      # running engine is the one that deploys the new one). The CLI
      # is safe to run anytime — it's the same `nix flake update` +
      # `nixos-rebuild switch` dance an operator would type by hand,
      # just packaged so they don't have to remember it.
      #
      # WebUI-terminal protection: when invoked from the engine's PTY
      # (detected via $NASTY_WEBUI_TERMINAL set by terminal.rs), all
      # rebuild work is detached into a systemd-run transient unit
      # (`nasty-sync-rebuild`) so the rebuild survives the engine
      # restart that nixos-rebuild triggers as part of activation.
      # Without this detach, the WebUI terminal's bash dies when the
      # engine restarts mid-rebuild → SIGHUP → nixos-rebuild dies
      # half-applied → wrapper partly-written → box stranded worse
      # than before nasty-sync ran. Operators invoking from a real
      # SSH session (no NASTY_WEBUI_TERMINAL env) get inline output
      # like any normal CLI tool.
      (writeShellScriptBin "nasty-sync" ''
        set -euo pipefail

        JQ=${pkgs.jq}/bin/jq

        usage() {
          cat <<HELP
        nasty-sync — CLI update / recovery tool for NASty

        Usage:
          nasty-sync             Bump nasty to current ref's HEAD and rebuild
                                 (bcachefs-tools untouched).
          nasty-sync -n <ref>    Switch nasty.url's ref to <ref> (a branch
                                 like \"main\", a tag like \"v0.0.9\", or
                                 a PR branch name), then bump + rebuild.
                                 Useful for testing PR branches, switching
                                 channels, or rolling back to a tag.
          nasty-sync -b          Bump nasty AND bcachefs-tools together —
                                 bcachefs adopts whatever rev nasty's new
                                 HEAD declares (atomic bundle).
          nasty-sync -b <ref>    Bump nasty; pin bcachefs-tools to <ref>
                                 (e.g. v1.37.2 for a downgrade), rebuild.
          nasty-sync -r          Recovery: canonicalize the wrapper back to
                                 \`bcachefs-tools.url\` shape using the rev
                                 in flake.lock (handles follows-shape AND
                                 corrupted-ref states), then update nasty
                                 + rebuild. Use this when the WebUI Update
                                 page is broken or the wrapper got
                                 corrupted by a half-applied upgrade.
          nasty-sync -s          Show current state (read-only).
          nasty-sync -h          This message.

        \`-n\` composes with the other modes: \`nasty-sync -n <ref> -b\` or
        \`nasty-sync -n <ref> -r\` first rewrites nasty.url, then runs the
        requested mode. \`-n\` followed by another mode flag in either
        order is fine — getopts handles the parse.

        When invoked from the WebUI terminal, the rebuild is auto-detached
        to a systemd transient unit (\`nasty-sync-rebuild.service\`) so it
        survives the engine restart that nixos-rebuild triggers. Follow
        live: \`journalctl -fu nasty-sync-rebuild\`. From a real SSH
        session, the rebuild runs inline.
        HELP
        }

        mode=update
        nasty_ref=""
        while getopts ":brshn:" opt; do
          case "$opt" in
            b) mode=update-with-bcachefs ;;
            r) mode=rescue ;;
            s) mode=show ;;
            n) nasty_ref="$OPTARG" ;;
            h) usage; exit 0 ;;
            :) echo "nasty-sync: option -''${OPTARG} requires a value (e.g. -n main)" >&2; usage >&2; exit 2 ;;
            \?) echo "nasty-sync: unknown option -''${OPTARG}" >&2; usage >&2; exit 2 ;;
          esac
        done
        shift $((OPTIND - 1))

        # -s never touches state; everything else writes /etc/nixos
        # and runs nixos-rebuild, so demands root + a wrapper flake.
        if [ "$mode" != show ]; then
          if [ "$(id -u)" -ne 0 ]; then
            echo "nasty-sync: $mode mode must run as root (writes to /etc/nixos, runs nixos-rebuild)" >&2
            exit 1
          fi
          if [ ! -f /etc/nixos/flake.nix ]; then
            echo "nasty-sync: /etc/nixos/flake.nix not found — not a NASty box, or the wrapper flake is missing" >&2
            exit 1
          fi
        fi
        if [ -n "$nasty_ref" ] && [ "$mode" = show ]; then
          echo "nasty-sync: -n is for modifying the wrapper; combine it with the default mode, -b, or -r (not -s)" >&2
          exit 2
        fi

        # When `-n <ref>` was passed: rewrite nasty.url's ref segment
        # in /etc/nixos/flake.nix BEFORE doing any update / rebuild
        # work. The rewrite preserves the github:nasty-project/nasty/
        # path prefix (so this only retargets within the canonical
        # nasty repo — for forks, edit by hand). The downstream
        # `nix flake update nasty` step then fetches the new ref.
        #
        # `#` as sed delimiter to dodge collision with the `/` inside
        # `github:nasty-project/nasty/...`. Branch names with
        # special regex chars aren't supported — keep it
        # github-branch-name-clean (alphanumeric + dash + underscore
        # + slash + dot).
        if [ -n "$nasty_ref" ]; then
          # Reject inputs with shell-meaningful characters that could
          # break out of the sed quoting. Branch / tag names don't
          # need any of these.
          case "$nasty_ref" in
            *[\"\$\\\`\|\&\;\<\>\(\)\{\}\[\]\*\?]*)
              echo "nasty-sync: ref '$nasty_ref' contains characters not allowed in a git ref" >&2
              exit 2
              ;;
          esac
          if ! grep -qE '^\s*nasty\.url\s*=\s*"github:nasty-project/nasty/' /etc/nixos/flake.nix; then
            echo "nasty-sync: /etc/nixos/flake.nix doesn't declare nasty.url with the canonical github:nasty-project/nasty/<ref> shape — refusing to rewrite (operator is on a fork; edit by hand)" >&2
            exit 1
          fi
          echo "==> Switching nasty.url ref to: $nasty_ref"
          sed -i -E "s#^(\s*nasty\.url\s*=\s*\")github:nasty-project/nasty/[^\"]+(\")#\1github:nasty-project/nasty/$nasty_ref\2#" /etc/nixos/flake.nix
        fi

        # Run the actual rebuild work — either inline (from a real SSH
        # session) or detached as a systemd transient unit (from the
        # WebUI terminal, so the rebuild survives the engine restart
        # that nixos-rebuild's activation triggers).
        #
        # Takes the rebuild shell command as a single argument string;
        # uses a transient unit name unique to this nasty-sync mode so
        # rescue runs don't collide with update-with-bcachefs runs.
        run_rebuild() {
          local script="$1"
          local unit_name="$2"
          if [ -n "''${NASTY_WEBUI_TERMINAL:-}" ]; then
            echo ""
            echo "==> WebUI terminal detected — detaching rebuild to systemd unit '$unit_name'"
            echo "==> The rebuild will outlive this shell. Follow live with:"
            echo "      journalctl -fu $unit_name"
            echo "==> Check status with:"
            echo "      systemctl status $unit_name"
            echo ""
            # --collect: clean the unit up after it exits.
            # --no-block: return immediately so the WebUI terminal
            #   isn't waiting for completion (it'd die mid-rebuild
            #   anyway when nasty-engine restarts during activation).
            # --setenv PATH: pass through the calling shell's PATH so
            #   the unit can find nixos-rebuild and friends.
            systemd-run \
              --unit "$unit_name" \
              --collect \
              --no-block \
              --description "nasty-sync detached rebuild" \
              --setenv "PATH=$PATH" \
              -- bash -c "$script"
            echo "==> Started. Detaching."
          else
            bash -c "$script"
          fi
        }

        # Write the version stamp file after a successful nasty update.
        # Factored out so the inline + detached paths use the same
        # post-rebuild bookkeeping.
        stamp_nasty_version() {
          local rev
          rev=$("$JQ" -r '.nodes.nasty.locked.rev // empty' /etc/nixos/flake.lock 2>/dev/null || true)
          if [ -n "$rev" ]; then
            echo "''${rev:0:7}" > /var/lib/nasty/version
          fi
        }

        case "$mode" in
          show)
            echo "── NASty state ──────────────────────────────────────"
            if [ -f /etc/nixos/flake.lock ]; then
              NASTY_REV=$("$JQ" -r '.nodes.nasty.locked.rev // "?"' /etc/nixos/flake.lock 2>/dev/null || echo "?")
              NASTY_REF=$("$JQ" -r '.nodes.nasty.original.ref // "main"' /etc/nixos/flake.lock 2>/dev/null || echo "?")
              BCACHEFS_REV=$("$JQ" -r '.nodes["bcachefs-tools"].locked.rev // "?"' /etc/nixos/flake.lock 2>/dev/null || echo "?")
              BCACHEFS_TAG=$("$JQ" -r '.nodes["bcachefs-tools"].original.ref // "?"' /etc/nixos/flake.lock 2>/dev/null || echo "?")
              printf "  nasty:          %s (tracking: %s)\n" "''${NASTY_REV:0:12}" "$NASTY_REF"
              printf "  bcachefs-tools: %s (%s)\n" "$BCACHEFS_TAG" "''${BCACHEFS_REV:0:12}"
            else
              echo "  /etc/nixos/flake.lock missing — can't read pinned revs"
            fi
            ENGINE_VER=$(cat /etc/nasty-version 2>/dev/null || echo "?")
            printf "  engine:         %s\n" "$ENGINE_VER"
            RUNNING_BCACHEFS=$(bcachefs --field version 2>/dev/null | head -1 || echo "?")
            printf "  bcachefs kernel module: %s\n" "$RUNNING_BCACHEFS"
            if [ "$RUNNING_BCACHEFS" != "?" ] && [ "$BCACHEFS_TAG" != "?" ]; then
              BCACHEFS_TAG_BARE=''${BCACHEFS_TAG#v}
              if [ "$RUNNING_BCACHEFS" = "$BCACHEFS_TAG_BARE" ]; then
                echo "  → kernel module matches pinned bcachefs-tools."
              else
                echo "  → mismatch: kernel module $RUNNING_BCACHEFS vs pinned $BCACHEFS_TAG_BARE (reboot pending?)"
              fi
            fi
            ENGINE_STATE=$(systemctl is-active nasty-engine 2>/dev/null || echo "unknown")
            printf "  engine service: %s\n" "$ENGINE_STATE"
            ;;

          update)
            echo "==> Bumping nasty input to current main HEAD..."
            cd /etc/nixos
            nix flake update nasty
            run_rebuild "set -euo pipefail; cd /etc/nixos; echo '==> Rebuilding system...'; nixos-rebuild switch --flake /etc/nixos; systemctl is-active --quiet nasty-engine || systemctl start nasty-engine; echo '==> Done.'" "nasty-sync-rebuild"
            # Inline-path bookkeeping. Detached path can't update this
            # in real time; the engine startup hook will reconcile.
            if [ -z "''${NASTY_WEBUI_TERMINAL:-}" ]; then
              stamp_nasty_version
              echo ""
              echo "==> Done. Now on: $(cat /var/lib/nasty/version 2>/dev/null || echo unknown)"
            fi
            ;;

          update-with-bcachefs)
            cd /etc/nixos
            echo "==> Bumping nasty input to current main HEAD..."
            nix flake update nasty
            # If a positional <ref> was passed, use it. Otherwise pull
            # the bcachefs ref the new nasty HEAD declares — read it
            # directly out of the freshly-fetched nasty source in the
            # Nix store rather than hitting GitHub again.
            if [ -n "''${1:-}" ]; then
              BCACHEFS_REF="$1"
              echo "==> Pinning bcachefs-tools to user-specified ref: $BCACHEFS_REF"
            else
              NASTY_SRC=$(nix flake metadata --json /etc/nixos | "$JQ" -r '.locks.nodes.nasty.locked.path // empty' 2>/dev/null || echo "")
              if [ -z "$NASTY_SRC" ]; then
                # `path` isn't populated for github: inputs; resolve via the eval path instead.
                NASTY_SRC=$(nix eval --raw /etc/nixos#inputs.nasty.outPath 2>/dev/null || echo "")
              fi
              if [ -z "$NASTY_SRC" ] || [ ! -f "$NASTY_SRC/flake.nix" ]; then
                echo "nasty-sync: couldn't locate nasty's flake.nix in /nix/store to read its bcachefs-tools pin" >&2
                echo "nasty-sync: re-run with an explicit ref, e.g. nasty-sync -b v1.38.3" >&2
                exit 1
              fi
              BCACHEFS_REF=$(grep -oE 'bcachefs-tools\.url = "github:koverstreet/bcachefs-tools/[^"]+"' "$NASTY_SRC/flake.nix" | sed -E 's#.*/([^"]+)"#\1#' | head -1)
              if [ -z "$BCACHEFS_REF" ]; then
                echo "nasty-sync: couldn't extract bcachefs-tools ref from nasty's flake.nix" >&2
                exit 1
              fi
              echo "==> Adopting bcachefs-tools ref from nasty's main HEAD: $BCACHEFS_REF"
            fi
            # Rewrite the wrapper's bcachefs-tools.(url|follows) line.
            # `#` delimiter on sed to avoid colliding with the `|` in
            # the (follows|url) alternation.
            if grep -qE '^\s*bcachefs-tools\.(follows|url)\s*=' /etc/nixos/flake.nix; then
              sed -i -E "s#^(\s*)bcachefs-tools\.(follows|url)\s*=\s*\"[^\"]*\"#\1bcachefs-tools.url = \"github:koverstreet/bcachefs-tools/$BCACHEFS_REF\"#" /etc/nixos/flake.nix
            else
              echo "nasty-sync: /etc/nixos/flake.nix has no bcachefs-tools declaration to rewrite" >&2
              exit 1
            fi
            echo "==> Re-resolving lock for bcachefs-tools..."
            nix flake lock
            run_rebuild "set -euo pipefail; cd /etc/nixos; echo '==> Rebuilding system...'; nixos-rebuild switch --flake /etc/nixos; systemctl is-active --quiet nasty-engine || systemctl start nasty-engine; echo '==> Done.'" "nasty-sync-rebuild"
            if [ -z "''${NASTY_WEBUI_TERMINAL:-}" ]; then
              stamp_nasty_version
              echo ""
              echo "==> Done. nasty: $(cat /var/lib/nasty/version 2>/dev/null || echo unknown), bcachefs-tools: $BCACHEFS_REF"
            fi
            ;;

          rescue)
            cd /etc/nixos
            # Source of truth for the bcachefs ref to pin: flake.lock.
            # The lock survives wrapper corruption because Nix wrote it
            # before the bug fired and never writes it during a failed
            # update. Read original.ref (the tag string) here.
            BCACHEFS_REF=$("$JQ" -r '.nodes["bcachefs-tools"].original.ref // empty' /etc/nixos/flake.lock 2>/dev/null || echo "")
            if [ -z "$BCACHEFS_REF" ] || [ "$BCACHEFS_REF" = "null" ]; then
              echo "nasty-sync: can't determine bcachefs-tools ref from /etc/nixos/flake.lock" >&2
              echo "nasty-sync: try -b <ref> with an explicit version (e.g. nasty-sync -b v1.38.3)" >&2
              exit 1
            fi
            echo "==> Recovery: pinning bcachefs-tools to $BCACHEFS_REF (read from flake.lock)"
            # Same rewrite as update-with-bcachefs: handles
            # follows-shape, url-shape, AND a corrupted .url where
            # the ref segment got mangled by a previous half-applied
            # update.
            if grep -qE '^\s*bcachefs-tools\.(follows|url)\s*=' /etc/nixos/flake.nix; then
              sed -i -E "s#^(\s*)bcachefs-tools\.(follows|url)\s*=\s*\"[^\"]*\"#\1bcachefs-tools.url = \"github:koverstreet/bcachefs-tools/$BCACHEFS_REF\"#" /etc/nixos/flake.nix
            else
              echo "nasty-sync: /etc/nixos/flake.nix has no bcachefs-tools declaration to rewrite" >&2
              exit 1
            fi
            echo "==> Bumping nasty input to current main HEAD..."
            nix flake update nasty
            run_rebuild "set -euo pipefail; cd /etc/nixos; echo '==> Rebuilding system...'; nixos-rebuild switch --flake /etc/nixos; systemctl is-active --quiet nasty-engine || systemctl start nasty-engine; echo '==> Done.'" "nasty-sync-rebuild"
            if [ -z "''${NASTY_WEBUI_TERMINAL:-}" ]; then
              stamp_nasty_version
              echo ""
              echo "==> Recovered. nasty: $(cat /var/lib/nasty/version 2>/dev/null || echo unknown), bcachefs-tools: $BCACHEFS_REF"
            fi
            ;;
        esac
      '')

      (writeShellScriptBin "nasty-report" ''
        # `set -e` is intentionally OFF: this script is meant to be run on
        # broken systems where individual commands will fail. We want each
        # section to attempt independently and produce as much diagnostic
        # info as possible even when half the box is down. `pipefail` is
        # also off — the `tee` at the end shouldn't fail-mask any
        # diagnostic command upstream of it.
        set -u

        SEP="─────────────────────────────────────────────────────"

        section() { echo ""; echo "$SEP"; echo "  $1"; echo "$SEP"; }

        # Persist every report under /var/lib/nasty/reports/. Two reasons:
        # 1) When the WebUI is dead, the operator may roll back to a
        #    working generation and want to send us what they had on the
        #    broken one. /var/lib survives nixos-rebuild rollbacks (it's
        #    state, not config) so the report survives with it.
        # 2) Capturing the same machine state across multiple rebuild
        #    attempts gives us a delta to look at.
        # Rotate aggressively (keep last 10) so a wedged box doesn't fill
        # the disk with reports.
        REPORTS_DIR="/var/lib/nasty/reports"
        mkdir -p "$REPORTS_DIR" 2>/dev/null
        TIMESTAMP=$(date '+%Y%m%d-%H%M%S')
        REPORT_FILE="$REPORTS_DIR/nasty-report-$TIMESTAMP.txt"
        ls -1t "$REPORTS_DIR"/nasty-report-*.txt 2>/dev/null | tail -n +11 | xargs -r rm -f

        # Tee everything to both stdout (the operator's terminal) and the
        # persisted file. `exec` redirects the rest of the script.
        exec > >(tee "$REPORT_FILE") 2>&1

        echo ""
        echo "╔═════════════════════════════════════════════════════╗"
        echo "║              NASty Diagnostic Dump                  ║"
        echo "╚═════════════════════════════════════════════════════╝"
        echo "  $(date '+%Y-%m-%d %H:%M:%S %Z')  |  $(hostname)  |  NASty $(cat /etc/nasty-version 2>/dev/null || echo unknown)"
        echo "  Saved to: $REPORT_FILE"

        section "System"
        echo "  OS:      $(nixos-version 2>/dev/null || echo unknown)"
        echo "  Kernel:  $(uname -r)"
        echo "  Uptime:  $(awk '{s=int($1); d=int(s/86400); h=int((s%86400)/3600); m=int((s%3600)/60); if(d>0) printf "%dd %dh %dm\n",d,h,m; else if(h>0) printf "%dh %dm\n",h,m; else printf "%dm\n",m}' /proc/uptime)"
        echo "  Memory:  $(free -h | awk '/^Mem/ {print $3 " used / " $2 " total"}')"

        section "Block Devices"
        lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,MODEL 2>/dev/null || true

        section "bcachefs Filesystems"
        for mp in /fs/*/; do
          fs=$(basename "$mp")
          echo ""
          echo "  Filesystem: $fs  ($mp)"
          bcachefs fs usage -h "$mp" 2>/dev/null || echo "  (not mounted or error)"
          echo ""
          echo "  Devices:"
          bcachefs fs usage "$mp" 2>/dev/null | head -20 | sed 's/^/    /' || true
        done
        if ! ls /fs/*/ >/dev/null 2>&1; then
          echo "  (no mounted filesystems)"
        fi

        section "Engine State — Protocols"
        cat /var/lib/nasty/protocols.json 2>/dev/null | ${pkgs.jq}/bin/jq . || echo "  (not found)"

        section "Engine State — Subvolumes"
        count=$(find /var/lib/nasty/subvolumes -maxdepth 1 -name '*.json' 2>/dev/null | wc -l)
        echo "  $count subvolume(s)"
        for f in /var/lib/nasty/subvolumes/*.json; do
          [ -f "$f" ] || continue
          ${pkgs.jq}/bin/jq -r '  "  • \(.name)  filesystem=\(.filesystem)  type=\(.subvolume_type)  \(if .volsize_bytes then "size=\(.volsize_bytes / 1048576 | floor)MiB" else "" end)"' "$f" 2>/dev/null || true
        done

        section "Engine State — Shares"
        for proto in nfs smb iscsi nvmeof; do
          count=$(find /var/lib/nasty/shares/$proto -maxdepth 1 -name '*.json' 2>/dev/null | wc -l)
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
          state=$(systemctl is-active "$svc.service" 2>/dev/null || true)
          printf "  %-20s %s\n" "$svc" "$state"
        done

        section "Kernel Modules (storage/sharing)"
        lsmod | grep -E '^(bcachefs|nvmet|iscsi_target|target_core|nvme)' | awk '{printf "  %-30s %s\n", $1, $3}' || echo "  (none)"

        section "Recent Engine Logs (last 50 lines)"
        journalctl -u nasty-engine -n 50 --no-pager 2>/dev/null | sed 's/^/  /' || echo "  (unavailable)"

        section "Reverse Proxy — Caddy"
        # Service state first: covers the "caddy.service could not be
        # found" case (build evaluated falsy / module skipped) by
        # distinguishing "not installed" from "installed but failed".
        if systemctl list-unit-files caddy.service >/dev/null 2>&1; then
          echo "  Unit:       installed"
          state=$(systemctl is-active caddy.service 2>/dev/null || true)
          enabled=$(systemctl is-enabled caddy.service 2>/dev/null || true)
          echo "  Active:     $state"
          echo "  Enabled:    $enabled"
        else
          echo "  Unit:       NOT INSTALLED (services.caddy probably evaluated to absent)"
        fi
        echo ""
        echo "  Listening sockets on :80 / :443 / :2019 (Caddy admin API):"
        ss -tlnp 2>/dev/null | awk '/:80\s|:443\s|:2019\s/ {print "    " $0}' || echo "    (ss unavailable)"
        echo ""
        # Caddy's admin API is on 127.0.0.1:2019; pulling the runtime
        # config tells us what TLS automation policies / app routes are
        # actually live, vs. what we think we pushed. Short timeout so
        # this doesn't hang on broken systems.
        echo "  Admin-API runtime config (apps.tls + apps.http servers):"
        if curl -fsS --max-time 3 http://127.0.0.1:2019/config/apps 2>/dev/null \
          | ${pkgs.jq}/bin/jq '{tls: .tls, servers: (.http.servers // {} | map_values({listen, route_count: (.routes|length), tls_policy_count: ((.tls_connection_policies // [])|length)}))}' 2>/dev/null \
          | sed 's/^/    /'; then
          :
        else
          echo "    (admin API not reachable — caddy may not be running)"
        fi
        echo ""
        echo "  Issued certs on disk:"
        find /var/lib/caddy/.local/share/caddy/certificates -name '*.crt' 2>/dev/null \
          | sed 's|^|    |' || echo "    (none)"
        echo ""
        echo "  Recent Caddy logs (last 30 lines, level=warn or error):"
        journalctl -u caddy -n 200 --no-pager 2>/dev/null \
          | grep -iE '"level":"(warn|error)"|^Aug |^May |^Apr |^Jan |^Feb |^Mar |^Jun |^Jul |^Sep |^Oct |^Nov |^Dec ' \
          | tail -30 \
          | sed 's/^/    /' || echo "    (unavailable)"

        section "Networking"
        echo "  Interfaces:"
        ip -brief addr show 2>/dev/null | sed 's/^/    /' || echo "    (ip unavailable)"
        echo ""
        echo "  Routes:"
        ip route show 2>/dev/null | sed 's/^/    /' || true
        echo ""
        echo "  /etc/resolv.conf:"
        sed 's/^/    /' /etc/resolv.conf 2>/dev/null || echo "    (missing)"
        echo ""
        # systemd-resolved is the box's actual stub; resolv.conf above
        # may just point at 127.0.0.53. Surface the upstream resolvers
        # resolved is using.
        if command -v resolvectl >/dev/null 2>&1; then
          echo "  resolvectl status (DNS servers / search domains):"
          resolvectl status 2>/dev/null | grep -E 'DNS Servers|DNS Domain|Current DNS' | sed 's/^/    /' || true
        fi
        echo ""
        # The two DNS resolvers our DNS-01 propagation check uses by
        # default. If they're unreachable, the upgrade migration's cert
        # issuance will appear to hang for 30+ seconds.
        echo "  Public-DNS reachability (1.1.1.1, 8.8.8.8, port 53):"
        for r in 1.1.1.1 8.8.8.8; do
          if timeout 2 bash -c "echo > /dev/tcp/$r/53" 2>/dev/null; then
            echo "    $r:53  reachable"
          else
            echo "    $r:53  UNREACHABLE"
          fi
        done
        echo ""
        echo "  Firewall (nftables) summary:"
        nft list ruleset 2>/dev/null | head -40 | sed 's/^/    /' || echo "    (nft unavailable)"

        section "System Updates — Flake + Generations"
        echo "  Current NixOS generation:"
        readlink /run/current-system 2>/dev/null | sed 's/^/    /'
        echo ""
        echo "  Last 5 boot generations:"
        # nix-env -p shows the bootloader's view; nixos-rebuild list-generations is more
        # human-readable but only exists in newer NixOS. Fall back gracefully.
        if command -v nixos-rebuild >/dev/null 2>&1 && nixos-rebuild list-generations 2>/dev/null | head -6 | sed 's/^/    /'; then
          :
        else
          nix-env --list-generations --profile /nix/var/nix/profiles/system 2>/dev/null \
            | tail -5 | sed 's/^/    /' || echo "    (unavailable)"
        fi
        echo ""
        echo "  /etc/nixos/flake.lock (nixpkgs + nasty pins):"
        if [ -f /etc/nixos/flake.lock ]; then
          ${pkgs.jq}/bin/jq -r '
            (.nodes.nixpkgs.locked // {}) as $np
            | (.nodes.nasty.locked // {}) as $n
            | (.nodes."bcachefs-tools".locked // {}) as $b
            | "    nixpkgs:        ref=\($np.ref // "?")  rev=\($np.rev // "?" | .[0:12])  lastModified=\($np.lastModified // "?")",
              "    nasty:          ref=\($n.ref // "?")  rev=\($n.rev // "?" | .[0:12])  lastModified=\($n.lastModified // "?")",
              "    bcachefs-tools: ref=\($b.ref // "?")  rev=\($b.rev // "?" | .[0:12])  lastModified=\($b.lastModified // "?")"
          ' /etc/nixos/flake.lock 2>/dev/null || echo "    (parse failed)"
        else
          echo "    (no /etc/nixos/flake.lock — not running from a flake?)"
        fi
        echo ""
        echo "  Recent nixos-rebuild attempts (journal, last 40 lines):"
        # nixos-rebuild logs as a transient unit; journalctl -u nixos-rebuild won't
        # work, but we can grep the journal for the activation script's stdout.
        journalctl --since '2 hours ago' --no-pager 2>/dev/null \
          | grep -E 'nixos-rebuild|activating the configuration|switching to system configuration|error: builder for|hash mismatch|sha256-' \
          | tail -40 \
          | sed 's/^/    /' || echo "    (none)"

        section "dmesg — bcachefs / storage errors (last 30)"
        dmesg --level=err,warn -T 2>/dev/null | grep -iE 'bcachefs|nvme|scsi|ata|disk|i/o error' | tail -30 | sed 's/^/  /' || echo "  (none)"

        echo ""
        echo "$SEP"
        echo "  Report saved to: $REPORT_FILE"
        echo "  Older reports:   $REPORTS_DIR/  (keeps last 10)"
        echo "  Share output:    report | nc termbin.com 9999"
        echo "  Share saved:     cat $REPORT_FILE | nc termbin.com 9999"
        echo "$SEP"
        echo ""
      '')
      # debugging & diagnostics
      perf               # perf record/report/script
      fio               # storage benchmarking
      iotop-c           # per-process I/O monitoring
      sysstat           # iostat, pidstat
      lsof              # open file handles
      strace            # syscall tracing
      dool              # system resource stats (dstat successor)
      netcat-gnu        # share output with devs: cmd | nc termbin.com 9999
      psmisc            # fuser, killall
      pciutils          # lspci for hardware identification
      websocat          # WebSocket CLI client (test engine API)
      tcpdump           # network packet capture
      nmap              # network scanning and port discovery

      # kernel crash symbolization
      binutils          # addr2line, nm, objdump, readelf

      (writeShellScriptBin "faddr2line" ''
        # Resolve a kernel function+offset (from a kernel oops) to a source line.
        #
        # Usage: faddr2line FUNC+OFFSET[/SIZE] [MODULE.ko]
        #
        # If MODULE is not given, bcachefs.ko is located automatically.
        # Requires debug symbols in the .ko; see README for how to enable them.
        #
        # Example (from a kernel oops):
        #   faddr2line bch2_btree_node_get+0x8d/0x5f0

        set -euo pipefail

        usage() {
          echo "Usage: faddr2line FUNC+OFFSET[/SIZE] [MODULE.ko]" >&2
          echo "Example: faddr2line bch2_btree_node_get+0x8d/0x5f0" >&2
          exit 1
        }

        [ $# -lt 1 ] && usage

        SPEC="$1"
        FUNC="''${SPEC%%+*}"
        REST="''${SPEC#*+}"
        OFFSET_STR="''${REST%%/*}"

        # Resolve hex or decimal offset to an integer
        OFFSET=$(printf "%d" "$OFFSET_STR" 2>/dev/null || { echo "Error: bad offset '$OFFSET_STR'" >&2; exit 1; })

        if [ $# -ge 2 ]; then
          MODULE="$2"
        else
          # Auto-locate bcachefs.ko (may be compressed)
          MODULE=$(find \
            /run/current-system/kernel-modules \
            /lib/modules \
            -type f \( -name "bcachefs.ko" -o -name "bcachefs.ko.xz" -o -name "bcachefs.ko.zst" \) \
            2>/dev/null | head -1 || true)
          if [ -z "$MODULE" ]; then
            echo "Error: bcachefs.ko not found — pass the path as the second argument." >&2
            exit 1
          fi
        fi

        # Decompress .ko if needed
        TMPKO=""
        case "$MODULE" in
          *.ko.xz)
            TMPKO=$(mktemp /tmp/kdbg-XXXXXX.ko)
            xz -d -c "$MODULE" > "$TMPKO"
            MODULE="$TMPKO"
            ;;
          *.ko.zst)
            TMPKO=$(mktemp /tmp/kdbg-XXXXXX.ko)
            ${pkgs.zstd}/bin/zstd -d -c "$MODULE" > "$TMPKO"
            MODULE="$TMPKO"
            ;;
        esac
        trap '[ -n "$TMPKO" ] && rm -f "$TMPKO"' EXIT

        # Find the symbol in the module
        SYM_LINE=$(${pkgs.binutils}/bin/nm "$MODULE" 2>/dev/null | awk -v f="$FUNC" '$3 == f {print; exit}')
        if [ -z "$SYM_LINE" ]; then
          echo "Error: symbol '$FUNC' not found in $MODULE" >&2
          echo "Nearby symbols (grep):" >&2
          ${pkgs.binutils}/bin/nm "$MODULE" 2>/dev/null | grep -i "$FUNC" | head -10 >&2 || true
          exit 1
        fi

        SYM_ADDR_HEX=$(echo "$SYM_LINE" | awk '{print $1}')
        SYM_ADDR=$(printf "%d" "0x$SYM_ADDR_HEX")
        TARGET=$(printf "0x%x" $(( SYM_ADDR + OFFSET )))

        echo "  module:  $MODULE"
        echo "  symbol:  $FUNC @ 0x$SYM_ADDR_HEX"
        echo "  offset:  $OFFSET_STR  →  address $TARGET"
        echo ""

        RESULT=$(${pkgs.binutils}/bin/addr2line -i -f -p -e "$MODULE" "$TARGET" 2>&1)
        echo "$RESULT"

        if echo "$RESULT" | grep -q "??"; then
          echo ""
          echo "Note: '??' means the .ko has no DWARF debug symbols (stripped)."
          echo "To get source lines, rebuild bcachefs with debug info enabled."
        fi
      '')
    ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
      ++ lib.optionals cfg.smb.enable [ samba ]
      ++ lib.optionals cfg.iscsi.enable [ targetcli-fixed ]
      ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ]
      ++ lib.optionals cfg.nut.enable [ pkgs.nut ]
      ++ lib.optionals cfg.tailscale.enable [ tailscale ];

    # ── State directory ────────────────────────────────────────

    systemd.tmpfiles.rules = [
      "d /var/lib/nasty 0751 root root -"
      # acme.env feeds DNS-01 provider creds into Caddy via
      # EnvironmentFile. Seeded empty so Caddy can start before the
      # engine has had a chance to write it.
      # /var/lib/nasty/reports/ holds rotated nasty-report dumps.
      # Surviving boot-generation rollbacks is the point — operator
      # generates a report on a broken system, rolls back to a working
      # one, then `cat`s the saved report to share. tmpfiles creates
      # the dir; the script rotates within it (keep last 10).
      "d /var/lib/nasty/reports 0750 root root -"
      "d /var/lib/nasty/caddy 0750 root caddy -"
      "f /var/lib/nasty/caddy/acme.env 0640 root caddy -"
      "d /var/lib/nasty/subvolumes 0750 root root -"
      "d /var/lib/nasty/shares 0750 root root -"
      "d /var/lib/nasty/shares/nfs 0750 root root -"
      "d /var/lib/nasty/shares/smb 0750 root root -"
      "d /var/lib/nasty/shares/iscsi 0750 root root -"
      "d /var/lib/nasty/shares/nvmeof 0750 root root -"
      "d /var/lib/nasty/vms 0750 root root -"
      # No apps-proxy tmpfile: apps ingress lives in Caddy's admin-API
      # config, not on disk. The engine PATCHes
      # `/config/apps/http/servers/.../routes` directly on install /
      # remove.
      "C /var/lib/nasty/sshd_override.conf 0644 root root - ${pkgs.writeText "sshd-default" "PasswordAuthentication yes\n"}"
      "d ${cfg.storage.mountBase} 0755 root root -"
      "d /etc/exports.d 0755 root root -"
      "d /etc/target 0750 root root -"
      "f /etc/samba/smb.nasty.conf 0644 root root -"
      "d /etc/samba/nasty.d 0755 root root -"
      "d /var/lib/nasty/nut 0750 root root -"
      "d /var/state/ups 0750 root root -"
    ];

    # ── NASty Metrics service ────────────────────────────────

    systemd.services.nasty-metrics = {
      description = "NASty Metrics Collector";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      path = with pkgs; [
        smartmontools         # smartctl for disk health
        iproute2              # ip -j addr show
        nasty-bcachefs-tools  # bcachefs fs usage
        util-linux            # lsblk
        pciutils              # lspci for controller name resolution
      ];

      environment = {
        RUST_LOG = "nasty_metrics=info";
      };

      serviceConfig = {
        Type = "notify";
        ExecStart = "${cfg.engine.package}/bin/nasty-metrics";
        Restart = "always";
        RestartSec = 5;
        StateDirectory = "nasty";

        # nasty-metrics is a read-only system inspector + Prometheus
        # endpoint. Unlike nasty-engine it does NOT manage mounts /
        # services / kernel tunables, so we can apply most of the
        # service-hardening set without breaking what it does.
        #
        # The directives we intentionally leave off:
        #   - ProtectKernelLogs: it shells out to `dmesg` for kernel
        #     error metrics. Setting this would block /dev/kmsg.
        #   - PrivateDevices:    smartctl needs /dev/sd*, /dev/nvme*
        #     to read SMART data. PrivateDevices hides all of /dev.
        #   - ProcSubset:        the whole *point* of metrics is to
        #     read /proc/loadavg, /proc/meminfo, /proc/stat,
        #     /proc/net/dev, /proc/diskstats. ProcSubset=pid hides
        #     everything except /proc/<pid>/* — leaving the service
        #     up but reading zeros for every CPU / memory / network
        #     stat. ProtectProc=invisible stays on because it only
        #     hides other users' processes (a separate concern).
        #   - SystemCallFilter:  the original
        #     `@system-service ~@privileged ~@resources` set fatally
        #     SIGSYS'd the service in <50ms at startup. The hot path
        #     was SQLite's WAL init calling `fchown()` (which is in
        #     @privileged); the runtime / tokio paths were close
        #     behind. Re-add as additive-only (`@system-service`
        #     alone, no `~@` subtractors) if you want it back.
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        LockPersonality = true;
        NoNewPrivileges = true;
        RestrictSUIDSGID = true;
        KeyringMode = "private";
        # AF_NETLINK for `ip -j addr show`; AF_INET/INET6 for the
        # Prometheus HTTP endpoint on :2138.
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" "AF_NETLINK" ];
        SystemCallArchitectures = "native";
        UMask = "0077";
      };
    };

    # ── NASty Engine service ─────────────────────────────────

    systemd.services.nasty-engine = {
      description = "NASty Engine";
      wantedBy = [ "multi-user.target" ];
      # caddy.service ordering: the engine pushes per-app ingress
      # routes via Caddy's admin API at startup (and on every
      # install/remove).  `after` keeps boot ordering clean; `wants`
      # (vs `requires`) means a broken Caddy doesn't block the engine
      # from running — the admin-API client has retry/backoff and
      # logs a warn! if it can't reach :2019.
      after = [ "network.target" "nasty-metrics.service" "caddy.service" ];
      wants = [ "nasty-metrics.service" "caddy.service" ];

      path = with pkgs; [
        bashInteractive  # bash for terminal
        util-linux       # lsblk, blkid, wipefs, mount, umount
        gptfdisk         # sgdisk (free space detection, partition creation)
        parted           # partprobe (re-read partition table)
        nasty-bcachefs-tools  # bcachefs
        config.nasty.linuxquota  # setproject, setquota, repquota (bcachefs project quotas)
        iproute2         # ip (for network config detection)
        kmod             # modprobe (for iSCSI/NVMe-oF kernel modules)
        systemd          # systemctl, journalctl (for update status)
        nixos-rebuild-ng # nixos-rebuild (for system updates)
        nix              # nix flake lock (for bcachefs-tools version switching)
        git              # for update check (git ls-remote)
        qemu             # QEMU/KVM for virtual machines
        # Decompressors for the VM disk-import flow: HAOS ships as
        # *.qcow2.xz, OPNsense as *.img.bz2, lots of cloud images as
        # *.qcow2.gz. The engine shells out to whichever matches the
        # uploaded file's wrapper before handing the inner image to
        # qemu-img convert.
        xz
        gzip
        bzip2
        docker                       # Docker for apps runtime
        docker-compose               # Docker Compose for multi-container apps
        fwupd                        # fwupdmgr for firmware updates
        curl                         # HTTP debugging
        rsync                        # file sync
        procps                       # sysctl (vm.dirty_* tuning)
        nftables                     # nft (dynamic firewall rules)
        getent                       # getent (user/group lookups)
        pciutils                     # lspci — PCI passthrough enumeration + hardware page
        usbutils                     # lsusb — USB enumeration for hardware page + VM USB passthrough
        dmidecode                    # DMI tables for /system/hardware (BIOS, baseboard, memory)
        tpm2-tools                   # tpm2_getcap / tpm2_create / tpm2_unseal — Hardware page chip info + the PCR-7 seal/unseal flow for the bcachefs encryption key (#102)
        systemd                      # bootctl — Secure Boot + Measured UKI state for the Hardware page. systemd is PID 1 already; this just puts its bin/ on the engine's path so the engine's restricted PATH can find bootctl without an absolute store path.
        sbctl                        # SB enrollment + signing-state checks for PR #2's WebUI ceremony (`sbctl verify`, `sbctl list-enrolled-keys`). Read paths only from the engine; writes go through lanzaboote's install hooks, never direct sbctl-from-engine calls.
        keyutils                     # keyctl — fs.lock revokes the bcachefs unlock key from the kernel session keyring; without this the call fails with "No such file or directory" even though every other tool we shell out to is here
      ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
        ++ lib.optionals cfg.smb.enable [ samba shadow.out ]
        ++ lib.optionals cfg.iscsi.enable [ targetcli-fixed ]
        ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ]
        ++ lib.optionals cfg.nut.enable [ pkgs.nut ]
        ++ lib.optionals cfg.tailscale.enable [ tailscale ];

      environment = {
        RUST_LOG = cfg.engine.logLevel;
        # Pin the TPM2 TCTI to /dev/tpmrm0. Without this every tpm2-tools
        # invocation (Hardware page vendor probe, the seal/unseal flow
        # for #102) prints a stderr stanza about failing to dlopen
        # libtss2-tcti-tabrmd.so.0 — the userspace TPM resource-manager
        # daemon NixOS doesn't ship — before falling through to the
        # in-kernel RM. Setting TPM2TOOLS_TCTI cuts straight to /dev/tpmrm0
        # and keeps the journal readable.
        TPM2TOOLS_TCTI = "device:/dev/tpmrm0";
      };

      serviceConfig = {
        Type = "notify";
        ExecStart = "${cfg.engine.package}/bin/nasty-engine";
        Restart = "always";
        RestartSec = 5;
        StateDirectory = "nasty";

        # The engine is a privileged system manager and cannot run under a
        # private mount namespace — that would hide its mounts from
        # NFS/SMB/iSCSI. So ProtectSystem, ProtectHome, PrivateTmp,
        # ProtectKernelTunables, and friends stay off.
        #
        # What we *can* tighten is per-process state (prctl flags), syscall
        # filters (seccomp), and address families. None of these need a
        # private namespace, so they don't break mount visibility.

        # Prevent privilege gain via setuid binaries spawned by the engine.
        # The engine itself runs as root, so this doesn't drop privileges —
        # it just stops a compromised engine from launching, say, a setuid
        # ping or a misconfigured suid debug helper.
        NoNewPrivileges = true;
        # Lock the personality(2) bits — block selinux-style ABI flips.
        LockPersonality = true;
        # Reject chmod that would set the setuid/setgid bits on any file
        # the engine creates. The engine writes configs, not setuid bins.
        RestrictSUIDSGID = true;
        # Block adjtimex/clock_settime — the engine never sets the clock.
        ProtectClock = true;
        # Block escalation to SCHED_FIFO/SCHED_RR.
        RestrictRealtime = true;
        # Use a private kernel session keyring so the engine cannot read
        # secrets stashed in another unit's keyring (and vice-versa).
        KeyringMode = "private";
        # Allow only the address families the engine actually uses:
        #   AF_UNIX   — docker/QMP/libvirt sockets
        #   AF_INET/6 — outbound HTTPS (Caddy ACME, telemetry, registries)
        #   AF_NETLINK — nft, ip, mount/umount kernel chatter
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" "AF_NETLINK" ];
        # Block the 32-bit syscall ABI on x86_64. NixOS doesn't ship
        # 32-bit binaries on the appliance image, and SystemCallArchitectures
        # applies recursively to descendants — but container processes
        # run under dockerd (a separate unit), not under the engine, so
        # this only constrains the engine + its direct subprocesses
        # (docker CLI, qemu-system, smartctl, bcachefs, …) all of which
        # are 64-bit-only.
        SystemCallArchitectures = "native";
        # Default-tight file creation: any file the engine writes
        # without an explicit chmod gets owner-only access. State
        # files (audit log, settings.json, oidc client_secret) already
        # set 0o600 explicitly; this is belt-and-braces for any future
        # writer that forgets to.
        UMask = "0077";
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

    # Disable rpcbind — not needed for NFSv4-only
    services.rpcbind.enable = lib.mkForce false;

    # ── Samba ──────────────────────────────────────────────────
    # Same approach: declare config but don't auto-start.

    services.samba = mkIf cfg.smb.enable {
      enable = true;
      settings = {
        global = {
          "server string" = "NASty NAS";
          # Default-deny on guest. Per-share `guest ok = yes` is still allowed,
          # but a misconfigured share (missing `valid users`, typo in ACL) no
          # longer silently downgrades to anonymous read.
          "map to guest" = "Never";
          "guest account" = "nobody";
          "server min protocol" = "SMB2";
          # macOS Finder requires SMB signing as optional for guest access.
          "server signing" = "auto";
          # Include NASty-managed shares and tuning from the global section.
          # The nasty-tuning.conf include must use "include" NOT "config file" —
          # Samba's "config file" directive replaces the entire config, which
          # prevents any subsequent directives (like our share includes) from
          # being processed. NixOS sorts keys alphabetically, so "config file"
          # would be emitted before "include", breaking share loading entirely.
          "include" = "/etc/samba/smb.nasty.conf";
        };
      };
    };

    # Ensure the SMB tuning config exists (empty) so Samba doesn't fail on startup
    # before the engine has written any tuning settings. No [global] header needed —
    # this file is included from within the [global] section of smb.nasty.conf.
    environment.etc."samba/nasty-tuning.conf" = mkIf cfg.smb.enable {
      text = "# Engine-managed tuning — written by nasty-engine TuningService\n";
      mode = "0644";
    };

    # Prevent Samba from auto-starting at boot. NixOS enables samba.target in
    # multi-user.target, which then pulls in all three daemons via samba.target.wants.
    # Override the target's wantedBy to break that chain; the engine starts Samba
    # on demand when the user enables the protocol.
    systemd.targets.samba.wantedBy = mkIf cfg.smb.enable (lib.mkForce []);

    # ── SMB network discovery ──────────────────────────────────
    # NASty was invisible to file-manager network browsers — TrueNAS,
    # Synology et al. show up because they advertise SMB over multiple
    # discovery surfaces. Cover the three that matter for modern OSes:
    #
    # - mDNS `_smb._tcp` via Avahi  → macOS Finder, GNOME Files, KDE
    # - mDNS `_device-info._tcp`    → Finder shows the rack-server icon
    # - WS-Discovery (UDP 3702)     → Windows 10/11 Explorer
    #
    # All gated on the build-time SMB switch. The engine starts/stops
    # samba-wsdd alongside samba-smbd/nmbd via the protocol service
    # list (see engine/nasty-system/src/protocol.rs::Protocol::Smb).
    services.avahi.extraServiceFiles = mkIf cfg.smb.enable {
      smb = ''
        <?xml version="1.0" standalone='no'?>
        <!DOCTYPE service-group SYSTEM "avahi-service.dtd">
        <service-group>
          <name replace-wildcards="yes">%h</name>
          <service>
            <type>_smb._tcp</type>
            <port>445</port>
          </service>
        </service-group>
      '';
      # `model=Xserve` is the rack-mount NAS icon Finder uses for
      # TrueNAS, Synology, etc. `port=0` because this entry is just
      # metadata — there's no service on the other end.
      device-info = ''
        <?xml version="1.0" standalone='no'?>
        <!DOCTYPE service-group SYSTEM "avahi-service.dtd">
        <service-group>
          <name replace-wildcards="yes">%h</name>
          <service>
            <type>_device-info._tcp</type>
            <port>0</port>
            <txt-record>model=Xserve</txt-record>
          </service>
        </service-group>
      '';
    };

    # WSDD daemon (Web Services Dynamic Discovery) for Windows 10/11
    # Explorer browsing — Win dropped NetBIOS/SMB1 browse master in
    # favour of WS-Discovery, which TrueNAS et al. ship by default.
    # NixOS upstream enables the unit at boot via `wantedBy`; override
    # so it follows the SMB protocol toggle managed by the engine.
    services.samba-wsdd.enable = mkIf cfg.smb.enable true;
    systemd.services.samba-wsdd.wantedBy = mkIf cfg.smb.enable (lib.mkForce []);

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

    # ── NUT (Network UPS Tools) ─────────────────────────────────
    # Custom systemd services that read config from /var/lib/nasty/nut/.
    # Not started at boot — the engine manages lifecycle via protocol toggle.

    systemd.services.nut-driver = mkIf cfg.nut.enable {
      description = "NUT UPS driver";
      after = [ "local-fs.target" ];
      serviceConfig = {
        Type = "forking";
        ExecStart = "${pkgs.nut}/bin/upsdrvctl -u root start";
        ExecStop = "${pkgs.nut}/bin/upsdrvctl stop";
        Environment = "NUT_CONFPATH=/var/lib/nasty/nut";
      };
      wantedBy = lib.mkForce [];
    };

    systemd.services.nut-server = mkIf cfg.nut.enable {
      description = "NUT UPS data server (upsd)";
      after = [ "nut-driver.service" ];
      requires = [ "nut-driver.service" ];
      serviceConfig = {
        Type = "forking";
        ExecStart = "${pkgs.nut}/sbin/upsd -u root";
        ExecStop = "${pkgs.nut}/sbin/upsd -c stop";
        Environment = "NUT_CONFPATH=/var/lib/nasty/nut";
      };
      wantedBy = lib.mkForce [];
    };

    systemd.services.nut-monitor = mkIf cfg.nut.enable {
      description = "NUT UPS monitor (upsmon)";
      after = [ "nut-server.service" ];
      requires = [ "nut-server.service" ];
      serviceConfig = {
        Type = "forking";
        ExecStart = "${pkgs.nut}/sbin/upsmon -u root";
        ExecStop = "${pkgs.nut}/sbin/upsmon -c stop";
        Environment = "NUT_CONFPATH=/var/lib/nasty/nut";
      };
      wantedBy = lib.mkForce [];
    };

    # ── Backup REST server (receives backups from other NASties) ─
    systemd.services.nasty-rest-server = {
      description = "restic REST server for NASty backups";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = pkgs.writeShellScript "nasty-rest-server-start" ''
          PATH_FILE="/var/lib/nasty/rest-server-path"
          if [ -f "$PATH_FILE" ]; then
            REPO_PATH=$(cat "$PATH_FILE")
          else
            REPO_PATH="/var/lib/nasty/rest-server"
          fi
          mkdir -p "$REPO_PATH"
          exec ${pkgs.restic-rest-server}/bin/rest-server --listen :8000 --path "$REPO_PATH" --no-auth
        '';
        StateDirectory = "nasty/rest-server";
        Restart = "on-failure";
        RestartSec = "5s";
      };
      wantedBy = lib.mkForce [];  # engine starts on demand
    };

    # ── WebUI via Caddy ────────────────────────────────────────

    # ── Reverse proxy: Caddy ───────────────────────────────────
    #
    # Caddy serves the WebUI, terminates TLS, and proxies the
    # engine RPC + API.  Per-app `/apps/<name>/` ingress lives in
    # Caddy's admin-API config: the engine talks to
    # http://127.0.0.1:2019/config/... on install / remove and the
    # routes apply immediately, no file rewrite, no reload.
    # Caddy is configured as a single named snippet (`nasty_webui_routes`)
    # that holds every reverse-proxy / file-server / websocket / header
    # rule, plus a static `:<port>` vhost that mounts the snippet with the
    # self-signed (or user-supplied) cert.
    #
    # TLS automation (Let's Encrypt / ZeroSSL / staging) is pushed through
    # the same admin API as ingress routes: on startup and after every
    # settings/ingress mutation, the engine PUTs `apps.tls.automation`
    # with a policy per managed hostname (main domain + each app
    # subdomain). Caddy then issues + renews each cert, and SNI matching
    # serves the right one. The static-cert `:<port>` block below stays
    # as the IP / unknown-SNI fallback. No `vhosts.conf` file, no
    # `systemctl reload caddy` on every settings change.
    services.caddy = mkIf (cfg.webui.package != null) {
      enable = true;
      # Rebuild Caddy with DNS-01 plugins compiled in. Stock `pkgs.caddy`
      # only supports HTTP-01 / TLS-ALPN-01 (no DNS providers), so users
      # whose ACME challenge type is "dns" need a custom build.
      #
      # Provider list is intentionally short — the four below cover the
      # vast majority of selfhost / indie-NAS users, and each plugin
      # we add lengthens the Caddy build (xcaddy compiles them in via a
      # fresh `go build`). To add a provider, append to `plugins`, run
      # `nix build .#nixosConfigurations.nasty-vm.config.services.caddy.package`,
      # and update `hash` from the resulting "got: sha256-…" message.
      package = pkgs.caddy.withPlugins {
        plugins = [
          # Public-DNS heavyweights — Cloudflare alone covers the
          # majority of selfhost users.
          "github.com/caddy-dns/cloudflare@v0.2.4"
          "github.com/caddy-dns/duckdns@v0.5.0"
          # Cloud / hosting providers commonly running NASty boxes.
          "github.com/caddy-dns/route53@v1.6.2"
          "github.com/caddy-dns/hetzner/v2@v2.0.0"
          "github.com/caddy-dns/linode@v0.8.0"
          # Indie domain registrars with first-class API support.
          "github.com/caddy-dns/porkbun@v0.3.1"
          "github.com/caddy-dns/namecheap@v1.0.0"
          # Niche but commonly requested: deSEC (open-source DNS host)
          # and RFC 2136 (any DNS server speaking standards-compliant
          # dynamic update — BIND, Knot, PowerDNS, etc.).
          "github.com/caddy-dns/desec@v1.1.0"
          "github.com/caddy-dns/rfc2136@v1.0.0"
        ];
        hash = "sha256-xwtaYTcoX0ZfAdfNiJG9b3zZrwH9aVhwJoxdDtgtQKU=";
      };
      globalConfig = ''
        # auto_https stays ON so Caddy generates the per-hostname
        # `tls_connection_policies` entries that route a given SNI to
        # the right managed cert. The engine still drives issuance
        # explicitly through `apps.tls.automation` PUTs over the admin
        # API — auto_https only adds the listener-level glue.
        #
        # `disable_redirects` because we already declare an explicit
        # `:<httpPort> { redir ... }` block below; without this, Caddy
        # tries to add its own redirect server on :80 and the two
        # collide.
        auto_https disable_redirects

        # SNI substitution rules. Both pair with the
        # `nasty.local:443 { tls internal }` block below so every
        # otherwise-unmatchable connection ends up served by the
        # internal-CA cert instead of TLS-handshake-failing.
        #
        # `default_sni` only fires when the ClientHello has NO SNI at
        # all — the direct-IP-literal case (`curl https://10.x.x.x/`).
        # Without it, a port-only `:443 { tls internal }` listener has
        # no hostname for the internal CA to bind to and IP-direct
        # connections get `tlsv1 alert internal error` (originally
        # caught by appliance-smoke CI).
        #
        # `fallback_sni` covers the other case: ClientHello DOES send
        # an SNI but no `automation.policies` entry has it in
        # `subjects` (typical for tailnet hostnames in the CSI E2E
        # rig — the QEMU VM curls the box at its `*.ts.net` MagicDNS
        # name, which we deliberately don't put on the cert). Without
        # it, Caddy has no policy to apply, doesn't issue on-demand
        # (we don't want arbitrary-SNI cert issuance), and emits
        # `internal_error`. With it, the unknown SNI is rewritten to
        # `nasty.local`, the internal-CA policy matches, and the
        # internal cert is served.
        default_sni nasty.local
        fallback_sni nasty.local

        # Don't try to install Caddy's internal-CA root into the local
        # OS / Java / NSS trust stores. On NixOS those paths are
        # read-only via the Nix store, so Caddy emits three startup
        # log lines on every boot:
        #
        #   define JAVA_HOME environment variable to use the Java trust
        #   warning: "certutil" is not available, install "certutil" …
        #   error: failed to install root certificate … failed to
        #     execute tee: exit status 1
        #
        # None of them affect serving — TLS handshakes work, the cert
        # is valid — but they make the boot log look broken and send
        # operators chasing a phantom. Operators who want the root in
        # their client's trust store grab it via the WebUI's
        # "Download CA Root" button on the TLS page.
        #
        # Note: the equivalent JSON field is `apps.pki.cas.<id>.install_trust`
        # but Caddy's Caddyfile parser doesn't expose that per-CA; the
        # global `skip_install_trust` directive below is the Caddyfile
        # path to the same outcome (applies to every CA, which is what
        # we want here — there's only the one local CA).
        skip_install_trust
      '';
      extraConfig = ''
        (nasty_webui_routes) {
          root * ${cfg.webui.package}/share/nasty-webui

          # Security headers on every response.  CSP keeps
          # 'unsafe-inline' for now because SvelteKit emits inline
          # bootstrap + theme-detection scripts in index.html;
          # nonces / hashes are a follow-up.
          header {
            Strict-Transport-Security "max-age=31536000; includeSubDomains"
            X-Content-Type-Options "nosniff"
            X-Frame-Options "DENY"
            Referrer-Policy "same-origin"
            Content-Security-Policy "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' blob:; connect-src 'self' ws: wss:; frame-src 'self' blob:; frame-ancestors 'none'; base-uri 'self'; form-action 'self'; object-src 'none'"
          }

          # File-content preview: same engine endpoint, but a much
          # tighter sandbox CSP and SAMEORIGIN frame-ancestors so a
          # previewed HTML/SVG can't run scripts in the WebUI's
          # origin.  Block order matters — this exact-path handle
          # must come before the broader /api/* one.
          handle /api/files/content {
            header {
              Content-Security-Policy "sandbox; default-src 'none'; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'unsafe-inline'; frame-ancestors 'self'"
              X-Frame-Options "SAMEORIGIN"
            }
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 3600s
                write_timeout 3600s
              }
            }
          }

          # Long-running websockets — terminal, VM console (vnc /
          # serial), apps-deploy streaming, log-stream viewer, VM
          # disk-import upload.  8h read/write timeouts so an idle
          # session doesn't get axed.  Caddy's reverse_proxy detects
          # Upgrade headers automatically — no explicit websocket
          # directive needed.  All of these need X-Real-IP set so
          # the engine's IP-bound session validation matches what it
          # saw on the /api/login that issued the token (see /ws
          # comment below).
          #
          # `stream_close_delay 30m` keeps each handler instance
          # alive for 30 min after a Caddy config reload, so the
          # admin-API mutations the engine performs for app ingress
          # (PUT/DELETE routes on every install/remove) don't kill
          # every active WS in the WebUI.  Caddy's reload model is
          # all-or-nothing — there's no in-place route patch — so
          # this is the supported mitigation until upstream
          # caddyserver/caddy#7222 (`stream_detached`) lands.
          #
          # The final `/ws/*` block is a catch-all for any future
          # engine WS route that might be added without a
          # corresponding nasty.nix update — without it, a new
          # `/ws/foo` route would silently 404 through the SPA
          # fallback.  Caddy evaluates routes in declaration order,
          # so the specific handlers above still win for their paths;
          # this one only catches what nothing else claimed.
          handle /ws/terminal {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 28800s
                write_timeout 28800s
              }
              stream_close_delay 30m
            }
          }
          handle /ws/vm/* {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 28800s
                write_timeout 28800s
              }
              stream_close_delay 30m
            }
          }
          handle /ws/apps/deploy {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 28800s
                write_timeout 28800s
              }
              stream_close_delay 30m
            }
          }
          handle /ws/system/logs {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 28800s
                write_timeout 28800s
              }
              stream_close_delay 30m
            }
          }

          # Main engine RPC websocket.  Must propagate X-Real-IP so
          # the engine's IP-bound session validation sees the same
          # client address it saw on the /api/login that issued the
          # token — without this, login through Caddy succeeds and the
          # immediate /ws upgrade through Caddy fails as "invalid
          # token" because the engine sees Caddy's loopback address
          # instead of the user's real IP.
          handle /ws {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 28800s
                write_timeout 28800s
              }
              stream_close_delay 30m
            }
          }

          # Catch-all for future `/ws/*` routes the engine grows
          # without us updating this block.  Same long timeouts as the
          # explicit websocket handlers above.
          handle /ws/* {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 28800s
                write_timeout 28800s
              }
              stream_close_delay 30m
            }
          }

          # /api/* — generic engine API.  3600s timeout +
          # request-body streaming for large uploads.
          handle /api/* {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port} {
              header_up X-Real-IP {remote_host}
              transport http {
                read_timeout 3600s
                write_timeout 3600s
              }
            }
          }

          handle /health {
            reverse_proxy 127.0.0.1:${toString cfg.engine.port}
          }

          # Static WebUI with SPA fallback.  Anything not matched
          # above falls through here.
          handle {
            try_files {path} {path}/ /index.html
            file_server
          }
        }

        # HTTP -> HTTPS redirect, port-only (works for IP and hostname).
        :${toString cfg.webui.httpPort} {
          redir https://{host}{uri} permanent
        }

        # Fallback site bound to `nasty.local` AND the port-only
        # catch-all on :${toString cfg.webui.port}. The hostname is
        # what `tls internal` issues the self-signed cert against (a
        # port-only listener alone has no hostname for the internal CA
        # to bind to). The port-only address makes the same site catch
        # any TLS connection on this listener — combined with the
        # `default_sni nasty.local` global directive, every SNI-less
        # or unmatched-SNI connection ends up here and gets served the
        # `nasty.local` cert.
        #
        # When ACME is enabled, the engine pushes
        # `tls.automation.policies` per managed hostname via the admin
        # API; `auto_https` adds per-SNI connection policies in front
        # of this fallback so SNI=<managed-host> wins. Both share
        # routes through the named snippet above.
        #
        # User-supplied cert + key still honoured via
        # `cfg.tls.certFile/keyFile`; we pick that path instead of
        # `tls internal` when both are set.
        nasty.local, :${toString cfg.webui.port} {
          ${caddyTlsDirective}
          import nasty_webui_routes
        }
      '';
    };

    # Caddy reads DNS-01 provider creds (when the user picks DNS
    # challenge) from this EnvironmentFile. The `-` prefix means
    # "ignore if missing" so the unit still starts on a fresh box
    # before the engine has written anything.
    systemd.services.caddy.serviceConfig.EnvironmentFile =
      mkIf (cfg.webui.package != null) "-/var/lib/nasty/caddy/acme.env";

    # ── Journald ───────────────────────────────────────────────
    services.journald.extraConfig = ''
      SystemMaxUse=200M
      MaxRetentionSec=7day
    '';

    # ── Log rotation ──────────────────────────────────────────
    services.logrotate.settings.nasty = {
      files = "/var/lib/nasty/audit.log";
      rotate = 10;
      size = "10M";
      compress = true;
      missingok = true;
      copytruncate = true;  # don't rename — engine holds the file open
    };

    # ── Tailscale VPN ─────────────────────────────────────────
    # Custom systemd service instead of the stock NixOS services.tailscale module.
    # The engine manages tailscale up/down imperatively from /var/lib/nasty/tailscale.json.
    # Service is NOT auto-started — the engine starts it when Tailscale is enabled.
    systemd.services.nasty-tailscale = mkIf cfg.tailscale.enable {
      description = "NASty Tailscale VPN";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = []; # Engine manages lifecycle via systemctl start/stop
      serviceConfig = {
        ExecStart = "${pkgs.tailscale}/bin/tailscaled --state=/var/lib/nasty/tailscale/tailscaled.state --socket=/run/tailscale/tailscaled.sock";
        RuntimeDirectory = "tailscale";
        StateDirectory = "nasty/tailscale";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    # ── Firewall ───────────────────────────────────────────────
    # Disable NixOS's static iptables firewall — the engine manages
    # nftables rules dynamically via `table inet nasty`.
    networking.firewall.enable = false;
    networking.nftables.enable = true;

    # ── Networking backend (NetworkManager) ───────────────────
    # As of v0.0.7 NASty manages the host network via NetworkManager
    # rather than scripted networking + dhcpcd. The engine writes NM
    # connection profiles directly via DBus; nixos-rebuild no longer
    # plays a role in network changes after the cutover migration.
    # See docs/network-architecture.md for the rationale.
    #
    # The engine still writes /etc/nixos/networking.nix on every apply
    # (for the `import ./networking.nix` chain in configuration.nix),
    # but its content is force-overridden here so any leftover legacy
    # declarations from before the upgrade can't fight NM.
    networking.networkmanager.enable = true;
    networking.useDHCP = lib.mkForce false;
    networking.useNetworkd = lib.mkForce false;
    networking.bridges = lib.mkForce { };
    networking.bonds = lib.mkForce { };
    networking.vlans = lib.mkForce { };
    networking.interfaces = lib.mkForce { };
    services.resolved.enable = true;

    # NM ownership boundary: the engine's `nasty-*` connections are
    # ours; everything else (Docker bridges, libvirt taps, container
    # veth ends, WireGuard / Tailscale tunnels, k8s CNI) is run by
    # other services and must not be touched. Glob match is the
    # discriminator until phase 4 makes it more sophisticated.
    networking.networkmanager.unmanaged = [
      "interface-name:docker*"
      "interface-name:br-*"
      "interface-name:veth*"
      "interface-name:vnet*"
      "interface-name:tap*"
      "interface-name:wg*"
      "interface-name:tailscale*"
      "interface-name:cni*"
      "interface-name:flannel*"
    ];
    })
  ];
}
