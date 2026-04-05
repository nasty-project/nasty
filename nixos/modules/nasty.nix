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
        default = "nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,tower_http=info";
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
      echo "  Type 'debug'      to show advanced debugging (perf, oops)."
      echo "  Type 'benchmark'  to show storage benchmark commands."
      echo "  Type 'report'     to dump diagnostic info for bug reports."
      echo ""

      help()      { cat /etc/nasty/help-cheatsheet; }
      debug()     { cat /etc/nasty/debug-cheatsheet; }
      benchmark() { cat /etc/nasty/benchmark-cheatsheet; }
      report()    { nasty-report; }
      export -f help debug benchmark report
    '';

    environment.etc."nasty/help-cheatsheet".text = ''

      ╔══════════════════════════════════════════════════════╗
      ║               NASty Command Reference                ║
      ╚══════════════════════════════════════════════════════╝

       bcachefs — filesystem info
         bcachefs fs usage /fs/<filesystem>        space by type (btree, data, cached, parity …)
         bcachefs fs usage -h /fs/<filesystem>     human-readable sizes
         bcachefs show-super /dev/<disk>           dump superblock (UUID, features, devices)
         bcachefs device list /fs/<filesystem>      member devices with state and tier
         dmesg | grep -i bcachefs                  kernel messages

       bcachefs — live diagnostics (interactive, q to quit)
         bcachefs fs top /fs/<filesystem>           btree ops per process
         bcachefs fs timestats /fs/<filesystem>     op latency (min/max/mean/stddev/EWMA)

       bcachefs — device management
         bcachefs device add /fs/<filesystem> /dev/<disk>      add a device
         bcachefs device remove /fs/<filesystem> /dev/<disk>   remove a device (triggers rebalance)
         bcachefs device set-state failed /dev/<disk>         mark device failed
         bcachefs data rereplicate /fs/<filesystem>            rereplicate after device change

       bcachefs — subvolumes & snapshots
         bcachefs subvolume list /fs/<filesystem>
         bcachefs subvolume snapshot <src> <dst>

       I/O monitoring
         iotop -o
         iostat -x 1
         dool -dny 1
         # → type 'debug' for perf profiling and kernel oops symbolization
         # → type 'benchmark' for fio storage tests

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

       fio — storage tests  (replace <filesystem> with your filesystem name)
         # Sequential read — large block, measures throughput
         fio --name=seq-read \
             --ioengine=libaio --direct=1 --rw=read \
             --bs=1024k --iodepth=8 --numjobs=1 \
             --size=1g --runtime=30 \
             --filename=/fs/<filesystem>/fiotest

         # Sequential write
         fio --name=seq-write \
             --ioengine=libaio --direct=1 --rw=write \
             --bs=1024k --iodepth=8 --numjobs=1 \
             --size=1g --runtime=30 \
             --filename=/fs/<filesystem>/fiotest

         # Random read — small block, measures IOPS
         fio --name=rand-read \
             --ioengine=libaio --direct=1 --rw=randread \
             --bs=4k --iodepth=32 --numjobs=4 \
             --size=1g --runtime=30 \
             --filename=/fs/<filesystem>/fiotest

         # Random write
         fio --name=rand-write \
             --ioengine=libaio --direct=1 --rw=randwrite \
             --bs=4k --iodepth=32 --numjobs=4 \
             --size=1g --runtime=30 \
             --filename=/fs/<filesystem>/fiotest

         # Clean up test file afterwards
         rm -f /fs/<filesystem>/fiotest

       share results with devs
         fio ... | nc termbin.com 9999

    '';

    # Kernel modules for iSCSI/NVMe-oF are NOT auto-loaded at boot.
    # They are loaded on demand by the engine when the user enables
    # a protocol, keeping a clean default state on fresh installs.

    # ── Firmware updates (fwupd) ────────────────────────────────
    services.fwupd.enable = true;

    # ── k3s (app runtime, disabled by default) ─────────────────
    # k3s is installed but NOT started automatically. The engine starts it
    # via systemctl when the user enables apps from the WebUI.
    services.k3s = {
      enable = true;
      role = "server";
      extraFlags = builtins.toString [
        "--disable=traefik"         # NASty has its own nginx
        "--disable=servicelb"       # Use NodePort instead
        "--disable=metrics-server"  # Not needed for app workloads
        "--write-kubeconfig-mode=644"
        "--flannel-backend=host-gw" # host-gw avoids VXLAN overlay that hijacks default route
        "--node-name=nasty-node"    # Fixed name so hostname changes don't break k3s
      ];
    };
    # Prevent k3s from starting on boot — engine manages this.
    systemd.services.k3s.wantedBy = lib.mkForce [];

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
      iotop             # per-process I/O monitoring
      ethtool           # NIC speed, duplex, ring buffer tuning
      iperf3            # network throughput testing
      tcpdump           # packet capture for protocol debugging
      rsync             # file transfer and sync
      jq                # JSON parsing (used by engine scripts)
      htop
      python3           # scripting and quick data processing
      file              # file type identification
      tree              # directory structure visualization
      strace            # system call tracing for debugging
      dig               # DNS debugging
      nano              # quick file editing
      qemu              # QEMU/KVM for virtual machines
      pciutils          # lspci for passthrough device discovery
      k3s               # lightweight Kubernetes for app runtime (optional)
      kubernetes-helm   # Helm chart manager for app deployment
      kubectl           # Kubernetes CLI (also available via k3s kubectl)
      lego              # ACME client for Let's Encrypt certificates
      croc              # peer-to-peer file transfer for sending debug reports

      (writeShellScriptBin "nasty-report" ''
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
          bcachefs device list "$mp" 2>/dev/null | sed 's/^/    /' || true
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

        section "dmesg — bcachefs / storage errors (last 30)"
        dmesg --level=err,warn -T 2>/dev/null | grep -iE 'bcachefs|nvme|scsi|ata|disk|i/o error' | tail -30 | sed 's/^/  /' || echo "  (none)"

        echo ""
        echo "$SEP"
        echo "  Share full output:  report | nc termbin.com 9999"
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
      "d /var/lib/nasty/vms 0750 root root -"
      "f /var/lib/nasty/apps-proxy.conf 0644 root root - # empty = no app proxies"
      "d ${cfg.storage.mountBase} 0755 root root -"
      "d /etc/exports.d 0755 root root -"
      "d /etc/target 0750 root root -"
      "f /etc/samba/smb.nasty.conf 0644 root root -"
      "d /etc/samba/nasty.d 0755 root root -"
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
      };
    };

    # ── NASty Engine service ─────────────────────────────────

    systemd.services.nasty-engine = {
      description = "NASty Engine";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "nasty-metrics.service" ];
      wants = [ "nasty-metrics.service" ];

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
        curl             # for update check (GitHub API, TODO: remove when repo is public)
        qemu             # QEMU/KVM for virtual machines
        config.services.k3s.package  # k3s for apps runtime
        kubernetes-helm              # Helm for app deployment
        lego                         # ACME client for Let's Encrypt
      ] ++ lib.optionals cfg.nfs.enable [ nfs-utils ]
        ++ lib.optionals cfg.smb.enable [ samba shadow.out ]
        ++ lib.optionals cfg.iscsi.enable [ targetcli-fixed ]
        ++ lib.optionals cfg.nvmeof.enable [ nvme-cli ];

      environment = {
        RUST_LOG = cfg.engine.logLevel;
      };

      serviceConfig = {
        Type = "notify";
        ExecStart = "${cfg.engine.package}/bin/nasty-engine";
        Restart = "always";
        RestartSec = 5;
        StateDirectory = "nasty";

        # No filesystem sandboxing (ProtectSystem, ProtectHome, etc.) — any of
        # these create a private mount namespace, making filesystem mounts invisible
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

    # Disable rpcbind — not needed for NFSv4-only
    services.rpcbind.enable = lib.mkForce false;

    # ── Samba ──────────────────────────────────────────────────
    # Same approach: declare config but don't auto-start.

    services.samba = mkIf cfg.smb.enable {
      enable = true;
      settings = {
        global = {
          "server string" = "NASty NAS";
          "map to guest" = "Bad User";
          "guest account" = "nobody";
          "server min protocol" = "SMB2";
          # macOS Finder requires SMB signing as optional for guest access.
          "server signing" = "auto";
          # Include NASty-managed shares from the global section.
          # Must NOT be a separate [share] section — Samba merges the first
          # included share into its parent section, inheriting path/options.
          "include" = "/etc/samba/smb.nasty.conf";
        };
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
          proxy_set_header X-Real-IP $remote_addr;
          include /var/lib/nasty/apps-proxy.conf;
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
          extraConfig = ''
            proxy_read_timeout 28800s;
            proxy_send_timeout 28800s;
          '';
        };

        locations."/ws/vm/" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
          proxyWebsockets = true;
          priority = 400;
          extraConfig = ''
            proxy_read_timeout 28800s;
            proxy_send_timeout 28800s;
          '';
        };

        # Proxy API calls to engine
        locations."/api/" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
          extraConfig = ''
            client_max_body_size 10G;
            proxy_request_buffering off;
            proxy_read_timeout 3600s;
            proxy_send_timeout 3600s;
          '';
        };

        locations."/health" = {
          proxyPass = "http://127.0.0.1:${toString cfg.engine.port}";
        };

      };
    };

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
