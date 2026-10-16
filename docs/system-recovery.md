# System recovery: boot-disk replacement

Tracking: [#997](https://github.com/nasty-project/nasty/issues/997).

**Status: foundation, not a supported system-restore command or completed
runbook.** Existing backup restore extracts snapshots into managed storage;
it does not import a system recovery snapshot into a fresh installation.
Do not copy the staged tree over `/` and start the engine.

## First supported scenario to build

Replace the OS disk on the same machine, with surviving data disks and an
independently accessible encrypted recovery repository. Preserve the fresh
installation's boot-device configuration and working management access.
Recover reviewed settings and credentials, then explicitly reconcile existing
pools by immutable identity before bringing consumers online.

Different hardware, cross-version state conversion, hosted AD restoration,
Secure Boot key enrollment and TPM resealing are separate recovery paths.
Even on the same machine, changed firmware/PCR state can prevent TPM unsealing.

## Preparation operators can do today

1. Create a **NASty System Recovery** backup and check that it succeeds.
   A selected source list alone does not establish a consistent, complete or
   decryptable snapshot. Keep the backup outside the OS disk's failure domain.
2. Keep the repository address, password, remote access credentials and any
   private CA material needed to reach it independently of the appliance.
   Restoring `backups.json` cannot bootstrap access to the repository containing
   that very file. Keep filesystem recovery passphrases/keys separately too.
3. Back up app data and VM disk images separately. Use the Directory panel's
   transaction-safe backup for a hosted AD domain and back up its output.
4. Record the installed NASty version, management access requirements and
   pool UUIDs. Treat the inventory and all recovery material as sensitive.

After OS disk failure, install only to the intended replacement boot disk.
Do not format surviving data disks, recreate pools, run automatic repairs or
enroll firmware keys to get past a recovery error. Stop at staging/review
until a qualified import procedure exists. Seek administrator assistance for
manual recovery; this document is not authorization to perform live actions.

## Recovery-state inventory

The current preset is defined in `webui/src/lib/recoveryBackup.ts`.
Its coverage is **not** equivalent to recovering every service or identity.

| Material | Current preset | Recovery treatment required |
| --- | --- | --- |
| `/var/lib/nasty/auth.json` | Included | Management users/password hashes/passkeys; distinct from Unix/Samba users. Preserve a verified fresh recovery administrator until restored login works; do not restore active sessions. |
| `/var/lib/nasty/settings.json`, notifications, backup profiles and other settings | Included | Version/schema validation and credential-decryption checks; apply selected categories, not a directory overlay. |
| `/var/lib/nasty/keys` | Included | Highly sensitive pool-key material; may include TPM-bound blobs. Never assume it can be decrypted after reinstall. |
| `/var/lib/nasty/fs-state.json`, mount/share/block-export state | Included | Reconcile pool UUIDs and subvolume IDs against surviving storage; reject missing, duplicate or ambiguous identities. No path/name-only adoption, formatting or automatic repair. |
| `/var/lib/nasty/vms`, app definitions | Included | Definitions do not recover external VM disks, app data, Docker named volumes or images. Autostart must remain inhibited until reviewed dependencies are available. |
| `/var/lib/nasty/apps-data` fallback | Included if present within the selected directory | Can hold application data when no pool was configured; identify explicitly, do not confuse with ordinary `/fs` appdata. Live application/database consistency is not guaranteed. |
| Networking/firewall/listener/Tailscale state under `/var/lib/nasty` | Included | Preserve fresh management access and routing by default. Old interface names, addresses, route acceptance and listeners require explicit review. Tailscale identity must not run concurrently on two machines. |
| Maintenance flags, pending network rollback, enrollment progress, jobs, update scripts, metrics and diagnostics | May be included | Operational/transient state is not a set of instructions to replay. Classify it; never resume interrupted mutation/enrollment/update jobs automatically. |
| `/etc/nixos` | Included | Keep fresh hardware/boot/ESP configuration. Treat custom Nix and old wrappers as executable input, not trusted settings. Do not evaluate or activate backed-up Nix during inspection. |
| `/var/lib/caddy` | Included | TLS private keys and local CA identity; validate ownership and planned names before enabling listeners. Paths configured outside this directory need separate coverage. |
| `/var/lib/systemd/credential.secret` | Included | Needed for host-only systemd credentials. TPM+host credentials also require a usable original TPM/policy. Do not replace the fresh host key during inspection. |
| `/var/lib/sbctl` | Optional, when present | Private signing keys; back up securely, but never auto-enroll firmware keys or restore enrollment progress. |
| `/root/.ssh/authorized_keys`, SSH host keys under `/etc/ssh` | Not covered by the core preset | Explicit recovery decision needed. Preserve fresh SSH access; restoring server identity/access lists must be reviewed. |
| Local Unix account/group IDs and Samba password/SID databases | Not covered by the core preset | `SmbService::create_user` uses `useradd` and `smbpasswd`; share JSON does not recreate these identities. Need consistent export and collision checks, preserving UID/GID/SID ownership. Do not blindly import `/etc/passwd`, `/etc/shadow` or live Samba databases. |
| AD member trust/keytab and hosted Samba AD databases | Not covered completely | Domain JSON is not the trust database. Review rejoin for member mode; use transaction-safe domain backup and a separate restore procedure for hosted AD. |
| `/fs` datasets, VM disks and appdata | Not included just by selecting system recovery | Retain surviving pools or restore separate data backups. Do not create replacement pools over surviving disks. |

Source anchors: `engine/nasty-engine/src/auth.rs`, `main.rs`,
`engine/nasty-common/src/secrets.rs`, `engine/nasty-storage/src/filesystem.rs`,
`engine/nasty-sharing/src/smb.rs`, and `engine/nasty-system/src/{domain,dc}.rs`.
This is an initial code inventory; unlisted/custom state must be reported as
unclassified, not silently treated as safe to import.

## Required recovery flow (not implemented yet)

1. **Recovery boot:** durable, explicit recovery mode before old state is
   loaded. Pools, exports, VM/app autostart, scheduled work and old VPN identity
   stay offline. Preserve the fresh management network and SSH. Existing
   maintenance mode is a starting point, not the full solution: it also stops
   the engine/UI and may retain the configured Tailscale transport.
2. **Retrieve and stage:** accept independently supplied repository credentials
   without initializing or modifying the repository. Extract into a private
   OS-disk staging area; do not require a mounted/initialized data pool. Current
   `backup.restore` only accepts `/fs`, so it cannot provide this bootstrap.
3. **Inspect:** authenticate the backup, bound resource use, reject unsafe
   archive paths/special files/symlink escapes and unsupported versions. Report
   missing/unknown state and decryption failures without displaying secrets.
   No backed-up Nix evaluation, script execution or commands from metadata.
4. **Plan:** show categories, identity conflicts, fresh configuration retained,
   missing data and credentials needing operator input. Fail closed on storage
   ambiguity; no repair, automatic adoption or destructive retries.
5. **Apply explicitly:** implement durable checkpoints and a separate recovery
   journal outside the imported state. Interrupted apply stays in recovery mode
   with evidence intact; never pretend a multi-file import is atomic. Preserve
   the fresh configuration/credential identity needed for recovery or rollback.
6. **Validate and release:** prove administrator access and secret usability;
   explicitly authorize pool reconnection, then consumers. Exit recovery only
   by operator action. Activation/reboot and firmware/key changes need their own
   review; no implicit post-import reboot or reseal.

## Executable first step

`nix build .#checks.x86_64-linux.recovery-material -L`

`nixos/tests/recovery-material.nix` uses two minimal, disposable NixOS VMs:

- Create synthetic settings/old hardware configuration, a real temporary TLS
  certificate and a host-only systemd credential in the original VM.
- Back up the core preset paths with the packaged rustic CLI and export only
  the encrypted repository, not the original password file or loose host key.
- Shut down the original VM. Start a separate fresh VM with its own host key,
  management route table and hardware configuration; no old boot disk is
  attached to it.
- Reject a wrong repository password, then retrieve the snapshot into private
  staging with independently supplied credentials.
- Verify recovered material, demonstrate the fresh credential key cannot
  decrypt the old credential, and verify decryption with the recovered host key
  inside a private mount namespace. The fresh key remains unchanged.
- Assert fresh boot/network configuration is untouched and demonstrate that
  an SSH access file outside the preset was not recovered.

**This is backup-material qualification, not an appliance restore test.** It
uses the rustic CLI, not NASty's backup RPCs, has no running NASty engine and no
surviving data pool, and does not prove state import, authenticated login,
Samba identity recovery, TPM unsealing or restart safety. The original VM is
shut down cleanly; this is not a power-loss/backup-consistency test. No private
test keys or restored trees become Nix outputs or production artifacts.

## Next independently reviewable gates

1. Capture a consistent, versioned recovery inventory, including explicit
   missing-state warnings and portable local account/Samba identity export.
   Never label old snapshots fully recoverable without inspecting their contents.
2. Add private staging and read-only inspection with adversarial-input tests,
   available without restoring old engine state or mounting a pool.
3. Add the durable recovery boot gate and a controlled apply procedure; test
   every interruption boundary before connecting it to the setup UI.
4. Upgrade the VM scenario to real NASty backup RPC → lost OS disk → fresh
   installation → import, with a surviving bcachefs disk and independent client.
   Verify login/secrets/identity, unchanged data hashes/ownership, no premature
   mount/export/autostart, missing keys, wrong/duplicate pools, and interrupted
   recovery. Do not weaken existing filesystem UUID checks to make it pass.
5. Publish the tested end-to-end operator runbook and setup wizard only once
   those gates pass. Report exclusions instead of claiming complete recovery.
