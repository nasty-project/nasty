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
  };
}
