# Atomic subvolume publication (draft API)

The opt-in methods `subvolume.create_atomic`, `subvolume.clone_atomic` and
`snapshot.clone_atomic` accept their existing create/clone parameters plus an
`initial_properties` string map. Separate method names are intentional: clients
must not fall back to legacy methods when an older appliance rejects them.

## Contract

- Destinations are top-level names. Existing VM/nested-volume operations continue
  to use their existing APIs; they are not silently converted.
- A durable reservation and request journal are created under
  `<pool-root>/.nasty/provisioning/<sha256(destination-name)>` before any storage
  command. The subvolume is built as `volume` beneath that private directory.
- Backing-image initialization, internal owner metadata, clone provenance and
  initial user properties are finished before the ready/publication markers.
- Atomic clones replace inherited user properties, including the source's CSI
  identity and presentation IDs. Internal appliance metadata is preserved.
- File/directory durability barriers precede Linux `renameat2(RENAME_NOREPLACE)`
  publication, followed by directory barriers. Existing destinations are never
  replaced, reformatted or relabeled.
- A retry may return a published destination only when its trusted publication
  marker, requested properties, ownership, type/capacity (create) or source
  provenance (clone) match. Legacy objects without the marker are not adopted.
- Interrupted reservations are never automatically resumed, adopted or deleted.
  Legacy create/clone methods also refuse to recreate an absent destination with
  a pending reservation. Only a completed publication removes its own journal.

Property limits: at most 64 keys / 64 KiB combined key/value bytes; each key is
nonempty, at most 240 bytes and contains no NUL; each value is at most 4096 bytes.
`nasty.*` keys remain reserved for appliance metadata.

## Namespace prerequisite

The mount root must be engine-owned and must not let untrusted share users rename
root-owned children: no group/other write permission, or sticky-bit protection.
The internal directories must be engine-owned real directories without untrusted
write permission. Unsafe roots/symlinks are rejected. The API does **not** chmod a
live pool root or reconfigure a share to achieve this prerequisite.

## Interrupted operations

After an error or lost response, query the final destination first. If it is not
published, preserve the private reservation. An administrator must explicitly
review its journal, original ownership/retention policy, source provenance,
backing image and any attached loop device. Do not bulk-label or delete objects
to silence a failure. There is no automatic recovery/repair endpoint in this
change. A successful publication followed by a journal-cleanup/durability error
can return an error even though the initialized final destination exists.

## Validation boundary

Unit tests cover property/name validation, persistent interruption reservations,
missing-ready-marker rejection, symlink/writable-parent refusal, and no-replace
rename races on ordinary Linux fixture directories.
An explicitly enabled isolated trusted-xattr fixture also verifies that
destination properties/retention survive publication, inherited source identity
is removed, internal owner metadata is preserved and the successful journal is
removed. The disposable appliance smoke test now exercises filesystem/block
create and both clone paths through RPC, but that full VM test has not yet been
run locally for this change (the regular smoke has since passed GitHub CI).
The appliance VM also runs a test-only crash harness: command shims pause a real
block create before formatting and both clone types after the kernel copy but
before destination metadata is written; a preload interposer pauses each create/
clone immediately before and after no-replace rename. The harness kills the
entire engine service cgroup, restarts it, and checks visibility, property/retain
preservation, no mutation replay and source data/metadata. Hooks are installed
only in the disposable test VM, never in production binaries or API methods.
These are deterministic engine-process-loss tests, not sudden VM/power cuts.
None of these fixture checks proves bcachefs
power-loss behavior or loop-device identity after a staged backing file is
renamed. The API must remain draft until isolated bcachefs filesystem/block
create, subvolume/snapshot clone, metadata/quota preservation and crash-boundary
tests pass. SDK and CSI adoption are separate dependent PRs; this change alone
does not close the CSI post-create identity-write window.
