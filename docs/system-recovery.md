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
| `/etc/machine-id` | Added by this foundation change | systemd validates the host credential key against the source identity. Old presets lacked this file; do not promise old snapshots are sufficient or overwrite the fresh identity during inspection. Existing profiles need explicit updating and a new backup. |
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
- Verify recovered material, demonstrate that neither the fresh credential key
  nor the recovered key with the fresh machine identity decrypts the old
  credential, and verify decryption with the recovered host key **and source
  machine ID** inside a private mount namespace. The fresh key and identity
  remain unchanged. The initial VM run exposed this machine-identity dependency;
  it motivated adding `/etc/machine-id` to the preset rather than dropping the
  decryption assertion.
- Assert fresh boot/network configuration is untouched and demonstrate that
  an SSH access file outside the preset was not recovered.

**This is backup-material qualification, not an appliance restore test.** It
uses the rustic CLI, not NASty's backup RPCs, has no running NASty engine and no
surviving data pool, and does not prove state import, authenticated login,
Samba identity recovery, TPM unsealing or restart safety. The original VM is
shut down cleanly; this is not a power-loss/backup-consistency test. No private
test keys or restored trees become Nix outputs or production artifacts.

## Next independently reviewable gates

### Private staging CLI (inspection only)

`nasty-recovery` is packaged on appliances independently of the engine. It
can retrieve an explicit encrypted-repository snapshot, or accept an
**already-extracted**, administrator-controlled tree whose top level contains
`etc/` and `var/`. It is not an implemented setup workflow. Never extract an
untrusted archive over live paths. Do not mount or initialize a pool just to
use this tool.

#### Retrieve directly from an existing repository

Independently obtain the repository address and password. `retrieve` needs a
**full 64-character snapshot ID**; it deliberately does not select `latest` or
accept an ambiguous ID prefix. List candidates with `snapshots` (below) and
choose one explicitly.

Create an absolute-path, root-owned regular password file, mode 0600, with
the repository password. Do not put the password on the command line. Create
a separate root-owned 0600 JSON options file, for example
`/run/recovery-options.json`:

```json
{
  "repository": "/path/to/existing/repository",
  "password_file": "/run/recovery-repository-password"
}
```

The only accepted keys are these two. Both files must be regular, private,
single-link files, with no symlink path components. Prefer a private parent
directory in `/run` for these independently supplied credentials. Do not put
them in the Nix store or a diagnostics bundle.

```console
sudo nasty-recovery snapshots --options-file /run/recovery-options.json
sudo nasty-recovery retrieve --options-file /run/recovery-options.json --snapshot <full_snapshot_id>
sudo nasty-recovery inspect <stage_id>
```

`snapshots` runs the same isolated rustic child (`snapshots --json`) and prints
only each snapshot's full ID, time, hostname and `missing_recovery_paths`: the
required recovery categories (NASty state, system configuration, TLS identity,
host credential key, source machine identity) whose paths no source path of
that snapshot covers. Tags, labels, source paths and other repository metadata
are not echoed; malformed times/hostnames are reported as `null`. It creates
no staging session and refuses output above 16 MiB or 10,000 snapshots. An
empty `missing_recovery_paths` means only that the snapshot's *declared*
sources cover those paths, not that the files exist or are valid; use
`retrieve` and `inspect` for that. The listing is not authenticated against an
expected appliance identity: hostname and time come from the repository.

The repository may be an existing absolute local directory or a
`rest:https://server/repository/` address using ordinary certificate
validation. REST Basic credentials, if needed, belong in the private JSON's
repository URL (percent-encode reserved characters), never in shell
arguments. Custom trust roots, S3, SFTP/rclone, insecure HTTP and arbitrary
backend options are not supported by this first retrieval path. Do not
disable TLS verification or initialize a replacement repository to bypass
refusal. Keep repositories outside the staging workspace.

The packaged, pinned rustic CLI decrypts/authenticates the selected snapshot
and streams an uncompressed tar dump. No `restore`, `init`, backup, repair or
prune command is invoked. Cache is disabled; inherited environment/profiles,
proxy settings, password commands, hooks and credential agents are not used.
Repository credentials are passed through a restricted environment/inherited
password descriptor, not process arguments or the staging journal. Child
stderr is suppressed because it can expose URLs or secrets. Errors therefore
do not reliably distinguish incorrect passwords, corrupt objects or backend
outages; the stage stays incomplete in all cases.

The child has a ten-minute wall deadline, 120 CPU-second limit, 512 MiB address
space limit and no regular-file writes. Large repositories may exceed these
limits even for small snapshots. These controls are bounds, not a sandbox or
a network-traffic quota. Only the selected snapshot's consumed objects are
authenticated; this is not a full repository scrub or proof that its contents
are safe, complete, consistent or from the intended appliance. Verify the
repository and snapshot choice independently.

Tar extraction accepts bounded regular files and directories only. Paths
cannot be absolute, traverse parents, contain ambiguous separators, duplicate
entries or conflict in type. Links/devices/FIFOs, sparse records and GNU/PAX
extended headers (including longname records) cause refusal of the entire
attempt. No backed-up permissions, owners, links, ACLs, xattrs or code are
installed. This validates the dump's entries, not the original filesystem's
types: rustic can represent a backed-up socket as an empty regular file, and
hardlink/ownership fidelity is not retained. Ordinary Nix `result` links will therefore block retrieval too;
there is no silent exclusion to make coverage appear complete. Failed or
interrupted attempts retain private evidence and are never marked completed.
Only a fully consumed stream and successful rustic exit publish a staged
session. Inspection reports `authenticated_repository` for this provenance,
but still always says `import_supported: false`.

#### Stage an already-extracted tree

With that tree available in a private directory on the fresh OS disk:

```console
sudo nasty-recovery stage --source /var/lib/recovery-input
sudo nasty-recovery inspect <stage_id>
```

Use the generated 32-character `stage_id` from the first JSON response.
`stage` copies, never moves or applies, into
`/var/lib/nasty-recovery/<stage_id>/tree`. The workspace must be on the root
filesystem, root-owned and mode 0700; the tool refuses unsafe existing
permissions instead of changing them. Copies use directories 0700/files 0600,
not backed-up ownership, modes, ACLs or executable permissions. Consequently
this is an inspection copy, **not an ownership-preserving data restore**.

The source must be quiescent on an ordinary local filesystem, under the
administrator's control. This is not a sandbox for an attacker-controlled
filesystem implementation or concurrent privileged writers. The tool uses
fd-relative traversal without following symlinks and refuses hardlinks,
special files, mount-device boundaries and overlapping source/workspace
trees. Entire staging fails on any unsafe entry, including ordinary Nix
`result` symlinks; it never dereferences, deletes or silently skips them.
Keep the original snapshot intact and review any separately prepared
regular-file inspection export. Do not remove links from a live configuration
to satisfy the tool. Entry, depth, per-file and aggregate-byte limits are
50,000, 24, 64 MiB and 1 GiB. Large app data inside the preset may exceed these
limits and is not automatically excluded. Copying checks available space before
starting and before data writes, leaving a best-effort 128 MiB free-space
margin. Concurrent disk users and inode exhaustion can still cause failure;
failed staging may occupy space. This is not a disk-space reservation.

`inspect` performs no intentional writes and reports fixed category names,
presence, bounded JSON-object checks, source machine-ID syntax and the
relationship of the saved version label to `/etc/nasty-version`. It does not
print backed-up paths, usernames, addresses, version strings, key material or
JSON values. Output still reveals coverage, sizes and a local stage ID; review
it before sharing. A matching version label does **not** prove schema or
import compatibility. Successful inspection exits zero even when blockers
are reported; `import_supported` is always false. Filesystem/input refusal or
incomplete staging exits nonzero.

No backup code/Nix is executed. Appliance-secret decryption, full repository
verification, manifest/schema validation, consistent identity export,
unknown-file classification and pool identity validation remain unverified.
Directories being present does not establish their completeness. The tool
cannot label any snapshot safe or ready for import.

The journal is separate from imported material. A stage is marked complete
only after copied file contents/directories are synced; interrupted or refused
stages remain incomplete and cannot be inspected as completed material.
There is no automatic resume, cleanup, apply, activation or reboot. Preserve
failed-stage evidence and use a new staging attempt. Staged contents are
secret-bearing: keep them private and review deliberate cleanup separately.
Inspection does not stop a running engine or inhibit consumers; because it
never installs the old state, it does not substitute for a recovery boot gate.

Unit tests exercise malformed/missing state, redaction, links/FIFOs, limits,
unsafe workspace/IDs and incomplete-stage refusal. The two-VM material test
also executes the packaged CLI on the fresh VM without an engine/data pool,
checks private permissions and verifies the fresh key, identity and management
configuration remain unchanged. Retrieval tests also exercise wrong password,
corrupt encrypted snapshot metadata, ambient-profile/environment isolation,
exact material hashes and unchanged repository file contents. Restricted tar
tests cover traversal, type conflicts, links/special files, extensions,
truncation/checksum errors, trailing data and resource limits. A Linux fake
producer verifies nonzero exit/timeout cannot publish a completed stage.
HTTPS REST interoperability is not qualified by the local-repository VM test.
This still does not qualify a production-state importer or power-loss recovery.

### Remaining gates

1. Capture a consistent, versioned recovery inventory, including explicit
   missing-state warnings and portable local account/Samba identity export.
   Never label old snapshots fully recoverable without inspecting their contents.
2. Extend retrieval and snapshot-listing qualification to remote repository
   transports; add versioned manifest/schema inspection before setup UI. Define
   explicit handling for excluded links/unsupported state without silent data
   loss. The current bounded retrieval remains inspection-only.
3. Add the durable recovery boot gate and a controlled apply procedure; test
   every interruption boundary before connecting it to the setup UI.
4. Upgrade the VM scenario to real NASty backup RPC → lost OS disk → fresh
   installation → import, with a surviving bcachefs disk and independent client.
   Verify login/secrets/identity, unchanged data hashes/ownership, no premature
   mount/export/autostart, missing keys, wrong/duplicate pools, and interrupted
   recovery. Do not weaken existing filesystem UUID checks to make it pass.
5. Publish the tested end-to-end operator runbook and setup wizard only once
   those gates pass. Report exclusions instead of claiming complete recovery.
