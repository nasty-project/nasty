# Lanzaboote 1.2 qualification

The appliance flake and the engine's **new Secure Boot enrollment** wrapper
input use [lanzaboote v1.2.0](https://github.com/nix-community/lanzaboote/releases/tag/v1.2.0).
The wrapper injector previously still used v1.0.0 despite the appliance pin
being v1.1.0; a unit regression now checks these defaults agree.
The enrollment injector preserves an already-declared wrapper input. However,
the existing legacy-wrapper migration re-renders the wrapper and injects the
current default when a Secure Boot overlay exists: that path will now use
v1.2.0. Operators with custom Measured Boot settings must review this before
requesting wrapper migration. This PR runs no live migration, enrolls no keys
and activates no boot configuration.

## Defaults and compatibility

- Secure Boot remains opt-in; ordinary appliances still use systemd-boot.
- When opted in, automatic key generation remains enabled, but automatic
  firmware enrollment remains disabled. PKI stays under `/var/lib/sbctl`.
- Measured Boot remains disabled; NASty's TPM sealing policy is unchanged.
  Ordinary Secure Boot retains the existing 20-generation configuration limit.
- Existing kexec restrictions remain. This update does not claim to fix
  firmware capsule updates or hardware-specific Secure Boot/TPM problems.

## Upstream Measured Boot caveat

[The upstream changelog](https://github.com/nix-community/lanzaboote/blob/v1.2.0/CHANGELOG.md)
describes retaining PCR4 policy coverage across a bootloader update. With
Measured Boot enabled, the maximum generation count drops from eight to four
because of [systemd #41526](https://github.com/systemd/systemd/issues/41526).
Upstream requires a reboot between consecutive bootloader updates. Do not
enable Measured Boot implicitly or shorten an operator's recovery history to
make a build pass. Any existing custom Measured Boot deployment needs separate
operator review, recovery keys and a planned reboot sequence.

## Qualification boundary

The dedicated workflow builds Secure Boot-enabled NASty configurations on
x86_64 and aarch64 without activation, checks opt-in defaults, and runs the
pinned upstream OVMF tests against NASty's stable nixpkgs: signed boot, test-key
generation/enrollment, tampered kernel/initrd rejection, and Measured Boot.
Only disposable VM keys, firmware variables, disks and TPM state are used.

These checks do not qualify physical firmware, an existing appliance's keys,
TPM-bound pool unlock, power-loss recovery or real-hardware upgrade/rollback.
Do not enroll keys, reseal storage, activate a system or reboot a live appliance
as part of dependency qualification.
