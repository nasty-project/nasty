# Run the pinned upstream tests with NASty's stable nixpkgs. The upstream
# fixture uses a newer image.repart.enable option; on 26.05, importing the
# repart module unconditionally enables image construction instead.
{ pkgs, lanzaboote, test }:

assert builtins.elem test [
  "basic"
  "auto-generate-enroll"
  "hash-mismatch-kernel-sb"
  "hash-mismatch-initrd-sb"
  "measured-boot"
];

pkgs.testers.runNixOSTest {
  imports = [ (lanzaboote.outPath + "/nix/tests/lanzaboote/${test}.nix") ];
  globalTimeout = 5 * 60;
  extraBaseModules = { lib, ... }: {
    imports = [ lanzaboote.nixosModules.lanzaboote ];
    # Accept only true: this shim cannot silently disable image construction.
    # No upstream test script, boot policy, partition or signature check changes.
    options.image.repart.enable = lib.mkOption {
      type = lib.types.enum [ true ];
      default = true;
      description = "Compatibility with upstream fixtures on always-enabled stable repart";
    };
    # v1.2 dropped these after upstream nixpkgs changed its firmware defaults.
    # Restore the v1.1 test hardware on stable: SB-capable OVMF with TPM support.
    config.virtualisation = {
      useSecureBoot = true;
      efi.OVMF = pkgs.OVMFFull.fd;
    };
  };
}
