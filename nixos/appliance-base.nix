{ config, lib, pkgs, nasty-engine, nasty-webui ? null, nasty-version ? "dev", ... }:

{
  imports = [ ./binary-cache.nix ];

  # fwupd: disable the hourly metadata refresh timer — firmware checks are
  # triggered on-demand from the UI. fwupd itself stays D-Bus activated so
  # it starts when needed and exits after the idle timeout.
  systemd.timers.fwupd-refresh.enable = false;

  # Boot loader — systemd-boot (UEFI)
  boot.loader.systemd-boot.enable = true;
  boot.loader.systemd-boot.configurationLimit = 20;
  # Memtest86+ EFI binary added as a sibling menu entry. ~1 MB on
  # the ESP, no closure-size impact on boxes that never boot into
  # it — but when an operator needs it (flaky RAM, ECC errors, post-
  # hardware-change sanity check) it's right there in the boot
  # menu instead of requiring a USB stick.
  #
  # Gated on x86 because nixpkgs's memtest86+ package declares
  # `meta.platforms = [ "x86_64-linux" "i686-linux" ]` only — and
  # the systemd-boot module does NOT silently skip platforms it
  # can't satisfy, despite earlier comments here implying it would.
  # Setting `memtest86.enable = true` on aarch64 triggers a hard
  # eval failure ("Refusing to evaluate package 'memtest86+-8.00'
  # because it is not available on the requested hostPlatform").
  # The `pkgs.stdenv.hostPlatform.isx86` check makes the option
  # value depend on architecture: true on x86_64 / i686, false on
  # aarch64 et al.
  #
  # Note for future Secure Boot work: memtest86+ isn't signed by
  # NASty's keys, so under SB it will refuse to launch. The
  # systemd-boot module keeps the entry visible but the boot
  # attempt fails; that's acceptable since SB-protected boxes are
  # the ones where memtest's "unsigned-but-trusted" model doesn't
  # fit anyway.
  boot.loader.systemd-boot.memtest86.enable = pkgs.stdenv.hostPlatform.isx86;
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
      for i in $(seq 1 5); do
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
      logLevel = "nasty_engine=info,nasty_storage=info,nasty_sharing=info,nasty_snapshot=info,nasty_system=info,nasty_apps=info,tower_http=info";
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
  # Boot menu entries read as `NASty v<release>` instead of the
  # default `NASty <release>.<commit-date>.<commit-hash>` nixpkgs
  # blob. Falls back to "dev" when this file is consumed standalone
  # (no specialArgs from the flake) — typical for nixos-rebuild
  # tests that import the module without the wrapper.
  system.nixos.label = "v${nasty-version}";

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
  # smartd's default `-a` tracks attribute changes by *normalized* value, so
  # Temperature_Celsius (194) and Airflow_Temperature_Cel (190) hit the journal
  # as a 0–253 health value — e.g. "194 Temperature_Celsius changed from 119 to
  # 120" — which reads like 120 °C and alarms operators who think their disks
  # are cooking (#424). The real temperature is the attribute's *raw* value
  # (~30 °C). `-r 194 -r 190` appends that raw value to the change lines so the
  # actual temperature is visible (e.g. "... [Raw 30]"). NASty's own disk-temp
  # alerting already reads smartctl's raw °C, so this only de-confuses the log.
  #
  # `-r` (report raw alongside the normalized value) — deliberately NOT `-R`,
  # which would *trigger* a journal line on every raw change, i.e. spam one
  # entry per 1 °C of drift.
  services.smartd.defaults.monitored = "-a -r 194 -r 190";
  systemd.services.smartd.unitConfig.ConditionVirtualization = "no";

  system.stateVersion = "24.11";
}
