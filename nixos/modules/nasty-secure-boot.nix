# Lanzaboote-using config for `services.nasty.secureBoot.enable`.
#
# This file lives separately from `nasty.nix` so its `boot.lanzaboote.*`
# references don't trip NixOS's option-existence validation when
# `nasty.nix` is imported into configurations that don't carry the
# lanzaboote module (notably the integration tests in `nixos/tests/`,
# which build a stripped-down system via `pkgs.nixosTest` without
# threading a lanzaboote specialArg through). `nasty.nix` imports this
# file via `lib.optionals lanzabooteAvailable [...]`, so it's only
# loaded when the lanzaboote NixOS module is also loaded — meaning the
# option paths referenced below are guaranteed to exist.
#
# The option itself (`services.nasty.secureBoot.enable`) is still
# declared in `nasty.nix`, since option declarations need to be
# present unconditionally so that mentioning the option in
# configurations is valid even on boxes where lanzaboote is absent.
{ config, lib, ... }:

let
  cfg = config.services.nasty;
  inherit (lib) mkIf;
in {
  config = mkIf cfg.secureBoot.enable {
    boot.lanzaboote.enable = true;
    boot.lanzaboote.pkiBundle = "/var/lib/sbctl";
    # First boot generates the PKI under pkiBundle if absent.
    # Operator (or PR #3's installer) still has to enroll PK/KEK/db
    # into firmware via a Setup-Mode visit — autoEnrollKeys stays
    # off here so flipping `secureBoot.enable` doesn't surprise an
    # operator with a firmware-state change.
    boot.lanzaboote.autoGenerateKeys.enable = true;

    # ── Defensive disables for known-broken paths under SB ──────
    #
    # kexec: lanzaboote-produced UKI stubs aren't PE-loadable by
    # `kexec --load` (upstream issue lanzaboote#143, open since
    # 2023). `systemctl kexec` and `kexec -e` would either fail
    # outright or load garbage and panic. The signed-kernel +
    # SB-on combination also typically enables kernel lockdown
    # which blocks the kexec_load syscall on its own, but pinning
    # the sysctl explicitly makes the disable visible in NixOS
    # config rather than relying on lockdown's runtime detection.
    # `kexec_load_disabled` is one-way (settable to 1, never back
    # to 0 without a reboot) — exactly the property we want.
    boot.kernel.sysctl."kernel.kexec_load_disabled" = 1;

    # fwupd intentionally NOT disabled. Listing devices / refreshing
    # metadata still works under SB; what's broken is the EFI-capsule
    # APPLY path (upstream lanzaboote#591). NASty's Firmware page
    # uses fwupdmgr for device enumeration and would go blank if
    # services.fwupd.enable were forced off here. Gating the apply-
    # update RPC on SB state is a separate WebUI concern — tracked
    # for a follow-up to this PR.
  };
}
