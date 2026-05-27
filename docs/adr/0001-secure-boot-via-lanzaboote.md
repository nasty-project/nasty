# ADR 0001 — Secure Boot via lanzaboote

- **Status:** Accepted, staged rollout. Opt-in per box from day one.
- **Date:** 2026-05-25
- **Deciders:** @fenio
- **Supersedes:** —
- **Superseded by:** —

## Context

NASty seals bcachefs encryption keys to the host TPM2 under a **PCR-7-only** policy (see `engine/nasty-common/src/tpm.rs`, `PCR_SELECTION: &str = "sha256:7"`). PCR-7 measures Secure Boot policy — PK, KEK, db, dbx, and the `SecureBoot` variable. On a stock NixOS install with Secure Boot disabled, **PCR-7 is effectively constant across every NASty box in the fleet**, so an attacker who pulls a drive can boot it on any other TPM-equipped box and the unseal succeeds. The seal looks correct but doesn't actually defend against the threat model it suggests.

PR #323 surfaced this gap in the Hardware page (Secure Boot pill via `bootctl status`), but the page is read-only — there is no path today to actually enable SB or to bind seals to a measured boot chain.

The remaining question for "TPM2 implementation complete" is: **what gets us from PCR-7-only-theatre to genuine measured-boot-backed sealing on a NixOS appliance fleet?**

## Investigated option: lanzaboote

[nix-community/lanzaboote](https://github.com/nix-community/lanzaboote) is the de-facto NixOS path to Secure Boot. It replaces the default systemd-boot installer with a Rust UEFI stub that embeds SHA-256 hashes of the kernel, initrd, and cmdline — the firmware verifies the stub, the stub verifies everything else. systemd-boot remains as the menu front-end. v1.0.0 was tagged 2025-12-10; prior versions sat at 0.4.x for several years.

### Architecture summary

| Concern | Lanzaboote answer |
|---|---|
| What gets signed | Per-generation Lanzaboote stub (PE binary), systemd-bootx64.efi |
| What gets measured | PCR-4 transitively covers stub + kernel + initrd + cmdline (stub carries their hashes); PCR-7 covers SB policy |
| PCRs available via module | `{0,1,2,3,4,7}` (5,6,8–15 explicitly forbidden by enum) |
| Boot menu | Still systemd-boot, signed and installed by lzbt |
| ESP layout | One Lanzaboote stub per NixOS generation; kernel/initrd deduplicated across generations |
| MOK / shim | Not used. Lanzaboote enrolls PK/KEK/db directly. Requires firmware Setup Mode for the one-time enrollment. |
| Coexistence with `boot.loader.systemd-boot.enable` | Replacement at module level (must `lib.mkForce false` it); systemd-boot binary is still present at firmware level |

### NixOS module surface (selected)

Full option set in `nix/modules/lanzaboote.nix` upstream. The load-bearing ones for NASty:

- `boot.lanzaboote.enable` — master switch.
- `boot.lanzaboote.pkiBundle` — directory containing PKI; **must not be a Nix store path**. Default `/var/lib/sbctl` (current docs) or legacy `/etc/secureboot`.
- `boot.lanzaboote.autoGenerateKeys.enable` — runs `sbctl create-keys` on first boot if the keystore doesn't exist. **New in 1.0.0.** Makes the install declarative.
- `boot.lanzaboote.autoEnrollKeys.{enable, autoReboot, includeMicrosoftKeys, allowBrickingMyMachine}` — stages `{PK,KEK,db}.auth` at `${ESP}/loader/keys/auto/`; systemd-boot enrolls into firmware on next boot via `secure-boot-enroll = force`. **New in 1.0.0.** **Still requires the firmware to already be in Setup Mode** — there is no escape from the one-time BIOS step. `includeMicrosoftKeys` defaults to `true` and the module asserts you can only disable it with `allowBrickingMyMachine = true` (option ROMs on NICs and RAID controllers are typically Microsoft-signed).
- `boot.lanzaboote.measuredBoot.{enable, pcrs, pcrlockDirectory, pcrlockPolicy, autoCryptenroll.*}` — wires up `systemd-pcrlock`. **Currently on master, not yet in a release.**
- `boot.lanzaboote.allowUnsigned` — needed for the "first boot has no keys yet" transition state.
- `boot.lanzaboote.configurationLimit` — capped at **8** when measuredBoot is on (assertion in module; upstream systemd issue 41526). Regression vs. NASty's current long rollback history.

### Key management & enrollment

Keys live unencrypted on the root filesystem under the `pkiBundle` path. The lanzaboote docs are explicit: "Lanzaboote cannot keep your keys secure. You need to do this yourself, e.g. by using full disk encryption." For a NAS that boots before its data drives are unlocked, this means **the platform keys are a per-box secret stored on the system partition**, not on the encrypted dataset.

Enrollment paths:
1. **`sbctl enroll-keys --microsoft`** — manual operator path; requires firmware in Setup Mode first.
2. **`autoEnrollKeys`** — declarative; stages key files, systemd-boot enrolls on next boot; **still requires Setup Mode**.
3. **MOK / shim** — not supported; lanzaboote doesn't use a shim.

Disaster recovery on lost keys: disable SB in firmware, boot a rescue medium, regenerate, re-enroll. The keystore is as critical as the dataset master key.

### PCR impact

Lanzaboote's documented recommendation is `pcrs = [ 0 4 7 ]`. PCR-4 is the one that actually matters — it covers the bootloader and stub, and because the stub carries the kernel/initrd/cmdline hashes, **PCR-4 transitively binds the entire boot chain after firmware**. PCR-7 covers SB policy, not the running binaries; SB alone doesn't fix the "drive moves to another box" attack, but SB *plus* PCR-4 sealing does.

**The migration teeth:** PCR-7 changes the moment PK/KEK/db are enrolled into firmware. Every existing NASty seal — every TPM-bound bcachefs filesystem in the fleet — will fail to unseal on the next boot after SB is enabled. **This is unavoidable** and must be designed for, not worked around.

PCR-11 (kernel measurement) and PCR-15 (system identity, the [oddlama defense](https://oddlama.org/blog/bypassing-disk-encryption-with-tpm2-unlock/)) are **not available via lanzaboote's module** today. They would have to be handled outside lanzaboote.

### Migration story for fleet appliances

Enabling lanzaboote at the Nix layer is reversible by toggling the flag and rebuilding. The point of no return is firmware enrollment — once PK is in firmware, rollback requires entering BIOS to disable SB, then disabling the Nix options. Always a per-box manual visit.

**Per-box manual step is irreducible**: every operator must enter Setup Mode in BIOS once. There is no zero-touch path. On server boards this typically means an IPMI iKVM session.

The cheapest first migration ("Option A"):
1. WebUI: opt into lanzaboote → NASty rebuilds with a signed ESP; SB stays off in firmware.
2. WebUI prompts: enter BIOS / IPMI, put firmware in Setup Mode, save & reboot.
3. NASty stages auto-enroll keys; reboot triggers firmware enrollment.
4. WebUI prompts: enter bcachefs passphrase one last time; engine re-seals every dataset blob under the new PCR-7.
5. Operator confirms `bootctl status` shows `Secure Boot: enabled (user)` + `Measured UKI: yes`.

Three reboots minimum. One passphrase prompt. The result is still PCR-7-only sealing — but PCR-7 now binds to *this box's* NASty-specific keyring, which kills the trivial drive-pull attack. **Option B** (real PCR-4 measured boot via `systemd-pcrlock`) is the v2 target.

## Risks identified

Ranked by how much they should worry us, descending:

1. **Server-board hardware compat is a void.** Lanzaboote docs name-check Lenovo ThinkPads and Framework laptops. The issue tracker, blogs, and NixOS Discourse have **no signal** for Supermicro, ASRock Rack, Tyan, or Gigabyte server boards — NASty's actual target hardware. Speculation: should work because SB is part of the UEFI spec, but no empirical base exists. **Single largest project risk.**
2. **PCR-7 prediction drift on AMD** ([lanzaboote#584](https://github.com/nix-community/lanzaboote/issues/584), May 2026). Mixed Intel/AMD fleets will see unattended TPM unlock fail on some boxes. Fix is upstream in systemd, not yet released.
3. **kexec broken under lanzaboote** ([lanzaboote#143](https://github.com/nix-community/lanzaboote/issues/143), open since 2023). If NASty's upgrade fast-path ever calls `systemctl kexec`, this is a regression we need to verify or work around.
4. **`fwupd` interaction broken** ([lanzaboote#591](https://github.com/nix-community/lanzaboote/issues/591), open May 2026). Firmware updates under SB don't pick up the historical env-var workaround. NASty users who run `fwupdmgr` will hit this.
5. **dbx updates change PCR-7.** Every firmware-pushed dbx update via `fwupd` will invalidate any PCR-7-only seal. `systemd-pcrlock` is supposed to handle this — depends on the prediction path working, which is #2 above.
6. **ESP space exhaustion.** Lanzaboote copies kernel + initrd per generation; `configurationLimit ≤ 8` under measuredBoot. NASty's installer doesn't currently size the ESP for this; needs validation or a doc requirement.
7. **ESP corruption after power loss is unrecoverable without a rescue medium with SB disabled.** A box in a customer's basement that loses power mid-`nixos-rebuild boot` may need an in-person visit. Documented in lanzaboote's troubleshooting page.
8. **No prior art for NixOS appliance + lanzaboote at fleet scale.** Closest references are personal workstations. NASty would be charting new territory.
9. **Project's own warning persists at 1.0.0.** "Secure Boot for NixOS is still in development and has some sharp edges. We only recommend setting up Secure Boot to NixOS users that are comfortable using recovery tools." Tag is `1.0.0`; tone is not.
10. **Auto-enroll without Microsoft keys can soft-brick.** Option ROMs are typically Microsoft-signed; without those keys, boot stalls. `includeMicrosoftKeys = true` is mandatory unless operator opts into `allowBrickingMyMachine`. Easy to get right but worth calling out.

## Decision

Pursue lanzaboote as the path to real measured-boot binding on NASty, **as an opt-in per-box feature from day one** — same pattern TPM2 sealing already follows. Boxes that physically can't do SB (BIOS/legacy, OVMF without SB, firmware in a bricked state) just don't get opted in; the Hardware-card states added in PR #323 already distinguish `Unsupported` from `Disabled` from `Unknown` cleanly so the operator sees the signal they need.

No fleet-wide validation gate, no readiness-probe PR. Validation happens organically as Bartosz implements the toggle on his own boxes (`.74` is the natural first target); the operator on any other box validates by flipping the toggle and watching what happens.

## Recommended sequencing

Two PRs:

1. **`nasty.secureBoot.enable` — opt-in lanzaboote module.** Imports lanzaboote 1.0.0 with `pkiBundle = /var/lib/sbctl`, `autoGenerateKeys.enable = true`, `autoEnrollKeys` **off**, and `boot.loader.systemd-boot.enable = lib.mkForce false`. Fold the seal-format-versioning audit inline — a small `policy_kind` enum on `SealedBlob` before any field semantics shift so future PCR-policy changes can be discriminated. Default off across the fleet; opt-in per box. Rollback while keys aren't yet enrolled is one Nix rebuild — no firmware visit needed at this stage.

2. **WebUI enrollment ceremony.** Once #1 boots cleanly, the operator UX: hardware-specific BIOS hints, Setup-Mode confirmation gate, auto-enroll flip, post-enroll passphrase prompt, re-seal-in-place under the new PCR-7. This is the per-box ceremony for moving from "signed ESP, SB off" to "SB enforcing with NASty-owned keys + re-sealed bcachefs blobs."

**Deferred (post-launch, not v1):** PCR-4 + `systemd-pcrlock` measured boot. Wait for [lanzaboote#584](https://github.com/nix-community/lanzaboote/issues/584) (PCR-7 prediction drift on AMD) and pcrlock to graduate out of experimental upstream. The engine would also need to move off `tpm2-tools` toward `systemd-cryptenroll` for this — bigger refactor than #1 + #2 combined.

## Consequences

### If we go ahead with the staged path

- **Positive**: existing TPM-bound boxes keep working untouched during steps 1–4. Operators see SB readiness in the UI without commitment. The migration is opt-in per-box. The first real-attack improvement (PCR-7 bound to NASty-specific keys, drive-pull defense) lands at step 5.
- **Cost**: per-box one-time BIOS / IPMI visit forever; a passphrase prompt during the SB activation window; a rollback-history regression to 8 generations if measured boot is ever turned on; new failure modes around fwupd, kexec, and AMD PCR-7 prediction drift.
- **Upgrade-path invariant**: must always leave a clean Nix-level path to disable lanzaboote and rebuild (per the project invariant). Firmware-level rollback requires the BIOS visit; that's unavoidable but should be documented in WebUI when an operator opts in.

### If we don't

- **PCR-7-only sealing remains theatre.** The Hardware page Secure Boot pill keeps showing "Disabled · not enforcing" or "Unsupported" on every box. TPM-bound bcachefs filesystems continue to be portable across hosts; the security claim of "TPM bound" is misleading vs. the actual threat model.
- **No path to lanzaboote.** Until measured boot exists, even adding PCR-15 / PCR-11 defenses (the oddlama approach) is harder because there's no measured boot chain to extend from.

### Things we deliberately defer

- **PCR-15 / volume-key extension** ([oddlama](https://oddlama.org/blog/bypassing-disk-encryption-with-tpm2-unlock/)). Not blocked by lanzaboote, but more useful once SB is in place. Engine extends PCR-15 with a bcachefs-volume-key derivative after unlock; future unseals require that extended value. Real defense-in-depth.
- **systemd-pcrlock**: the upstream-recommended way to handle PCRs that change across boots (kernel updates, dbx updates). Currently experimental in systemd, currently `Unreleased` in lanzaboote. Re-evaluate when both stabilize.
- **`autoEnrollKeys` as a NASty default.** Even after step 5, operator-explicit enrollment is the conservative posture. Defaulting auto-enroll across the fleet would expand the soft-brick blast radius for any quirky-firmware box.

## Open questions

- **What to do about kexec.** If NASty ever relies on kexec for fast upgrades, document the regression in the SB toggle's WebUI copy or detect-and-warn at opt-in time.
- **ESP sizing.** Current installer default is sufficient for systemd-boot; under lanzaboote + 8 generations it may not be. PR #1 should add a pre-flight `df /boot` check at opt-in time and refuse to enable if there isn't headroom.
- **Where does the keystore back up?** `/var/lib/sbctl` is per-box and unencrypted on the system partition. Operator workflow: include it in the existing "back up your master key" guidance, or build a separate flow? Probably worth surfacing in the enrollment-ceremony WebUI copy.
- **AMD PCR-7 prediction drift** ([lanzaboote#584](https://github.com/nix-community/lanzaboote/issues/584)) — material for any AMD-based opt-in. Acceptable for v1 because re-seal happens at enrollment time (we read the actual post-enroll PCR-7 value, not the predicted one), but blocks the deferred PCR-4 + pcrlock work.

## Sources

Upstream:
- [nix-community/lanzaboote (repo)](https://github.com/nix-community/lanzaboote)
- [Module source (master)](https://github.com/nix-community/lanzaboote/blob/master/nix/modules/lanzaboote.nix)
- [v1.0.0 release](https://github.com/nix-community/lanzaboote/releases/tag/v1.0.0)
- [Docs index](https://nix-community.github.io/lanzaboote/)
- [Prepare your system](https://nix-community.github.io/lanzaboote/getting-started/prepare-your-system.html)
- [Enable Secure Boot](https://nix-community.github.io/lanzaboote/getting-started/enable-secure-boot.html)
- [Disable Secure Boot](https://nix-community.github.io/lanzaboote/how-to-guides/disable-secure-boot.html)
- [Automatically Generate Keys](https://nix-community.github.io/lanzaboote/how-to-guides/automatically-generate-keys.html)
- [Automatically Enroll Keys](https://nix-community.github.io/lanzaboote/how-to-guides/automatically-enroll-keys.html)
- [Enable Measured Boot](https://nix-community.github.io/lanzaboote/how-to-guides/enable-measured-boot.html)
- [Measured Boot explanation](https://nix-community.github.io/lanzaboote/explanation/measured-boot.html)
- [Automatic Provisioning](https://nix-community.github.io/lanzaboote/explanation/automatic-provisioning.html)
- [Troubleshooting](https://nix-community.github.io/lanzaboote/explanation/troubleshooting.html)
- [Lanzaboote on NixOS Wiki](https://wiki.nixos.org/wiki/Lanzaboote)

Upstream issues called out above:
- [#143 — kexec broken under lanzaboote](https://github.com/nix-community/lanzaboote/issues/143)
- [#394 — Specialisations indistinguishable in menu](https://github.com/nix-community/lanzaboote/issues/394)
- [#584 — PCR-7 prediction drift on AMD](https://github.com/nix-community/lanzaboote/issues/584)
- [#591 — fwupd no longer respects FWUPD_EFIAPPDIR](https://github.com/nix-community/lanzaboote/issues/591)
- [#594 — PCR-7 issues in KVM/QEMU](https://github.com/nix-community/lanzaboote/issues/594)
- [#596 — PCR-11 support](https://github.com/nix-community/lanzaboote/issues/596)

Third-party:
- [oddlama — Bypassing disk encryption with TPM2 unlock](https://oddlama.org/blog/bypassing-disk-encryption-with-tpm2-unlock/) — argument for PCR-15 / volume-key extension.
- [Haseeb Majid — lanzaboote + TPM + impermanence walkthrough (2025-12-31)](https://haseebmajid.dev/posts/2025-12-31-how-to-setup-a-new-pc-with-lanzaboote-tpm-decryption-sops-nix-impermanence-nixos-anywhere/)
- [jnsgr.uk — Secure Boot & TPM-backed FDE on NixOS](https://jnsgr.uk/2024/04/nixos-secure-boot-tpm-fde/)
- [Discourse — migrating from lanzaboote to limine](https://discourse.nixos.org/t/migrating-from-lanzaboote-to-limine-secure-boot/77534)
- [UAPI.7 — Linux TPM PCR registry](https://uapi-group.org/specifications/specs/linux_tpm_pcr_registry/)

NASty in-tree references:
- `engine/nasty-common/src/tpm.rs` — current PCR-7 seal/unseal implementation.
- `engine/nasty-common/src/secure_boot.rs` — `bootctl status` reader (added in PR #323).
