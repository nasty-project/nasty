# Backup Restore

Design for restoring rustic (restic-compatible) backups through the NASty
WebUI. Requested in [discussion #626](https://github.com/orgs/nasty-project/discussions/626)
(es5445): the backup system can run backups, check a repo, and list its
snapshots, but has **no restore operation** — so disaster recovery (a dead
box, a fresh install, appdata pulled back from S3) isn't possible without
shelling out to Backrest.

## Scope

**In scope (v1):**
- Restore a **whole snapshot** to a chosen destination under `/fs`.
- Works cross-instance for free: a repo created by another NASty is
  restored the same way — add a profile pointing at it (target + password,
  already supported), its snapshots list (already supported), restore.
- Tracked as a job (status + coarse progress %), like the existing backup
  operations.

**Out of scope (deferred, per the requester's own ranking):**
- Browsing and restoring individual files/folders within a snapshot.
- In-place restore back to a snapshot's original paths (v1 always restores
  to an operator-chosen destination).

## Principle: operations stay on `/fs`

Restore only writes into `/fs` (bcachefs-managed storage). This matches
where backup-worthy data already lives — including Docker **appdata**,
which is its own `/fs/<fs>/appdata` subvolume by design (#436), *not*
`/var/lib/nasty/apps-data` (that path is only a fallback for a box with no
filesystem configured). Restricting to `/fs` keeps the operation on the
managed substrate, matches the existing file-restore jail, and turns the
destination UX into a filesystem + subpath picker rather than a
root-writable free path.

## Components

All new surface mirrors the existing backup operations.

- **`BackupJobKind::Restore`** (`engine/nasty-backup/src/jobs.rs`) — added
  alongside `InitRepo`/`RunBackup`/`CheckRepo`; label `"restore"`. Restore
  is a tracked job the WebUI polls via `backup.jobs.get`, exactly like the
  others.
- **`BackupService::restore`** (`engine/nasty-backup/src/lib.rs`) — mirrors
  `run_backup`'s structure: resolve profile + password, take the
  single-running-op guard (`self.running`) so a restore can't race a backup
  on the same repo, then in `spawn_blocking` (rustic is sync):
  `make_repo → open → to_indexed → node_from_snapshot_path(snapshot_id) →
  LocalDestination::new(dest) → prepare_restore → restore`. Progress is
  driven through rustic_core's `ProgressBars` into the job's progress
  fraction.
- **Router arm `backup.restore`** (`engine/nasty-engine/src/router/backup.rs`)
  + registry entry (`registry/methods.rs`).
- **WebUI** (`webui/src/routes/backups/+page.svelte`) — a **Restore** action
  on each row of the snapshot list the page already renders, opening a
  dialog (filesystem picker + subpath + overwrite checkbox), then reusing
  the existing job-poll display with a progress bar.

## Request / API

`backup.restore` parameters:

| field | type | meaning |
|-------|------|---------|
| `id` | string | Backup profile id (identifies the repo + credentials). |
| `snapshot_id` | string | Snapshot to restore (from `backup.snapshots`). |
| `dest` | string | Absolute destination path; **must** resolve under `/fs`. |
| `allow_overwrite` | bool (default false) | Permit restoring into a non-empty destination. |

Returns a job id (the started `Restore` job), consistent with
`backup.repo.init` / `backup.run` returning jobs the client then polls.

## Destination validation (before the job starts)

Performed in the engine, up front, so the operator gets a clear error
instead of a half-run job:

1. **Under `/fs`.** Canonicalize the destination's existing ancestor and
   require the result to be under `/fs`. Reject `..` traversal and
   symlink-escape (resolve symlinks in the existing prefix). This is the
   same jail shape as the file-restore endpoint (`FILES_ROOT = "/fs"`).
2. **Filesystem exists.** The `/fs/<fs>` root of the destination must be an
   existing directory (a mounted NASty filesystem).
3. **Non-empty gate.** If the destination already exists and contains
   entries and `allow_overwrite` is false → reject with a clear message
   ("destination is not empty — enable 'overwrite existing files' to
   restore into it"). An empty or not-yet-existing destination proceeds
   regardless; it is created (`mkdir -p`) before the restore.

`allow_overwrite` only gates *entering* a non-empty directory. rustic's
restore then behaves normally: it creates missing files and overwrites
files whose content differs, and does **not** delete extra files already
present (no `--delete`). So a restore merges into the destination; it never
wipes unrelated data.

## Progress & job lifecycle

The `Restore` job moves `running → success | failed`, same as the others,
and carries:

- A **coarse progress fraction** (0.0–1.0), updated from rustic_core's
  `ProgressBars` (bytes restored / total). The WebUI shows a real progress
  bar; if a total isn't known yet, it shows an indeterminate state until
  the first update.
- On success, a short **summary** (files restored, bytes written) in the
  job result, mirroring `RunBackup`'s result shape.

## Error handling

- **Repo open / decrypt failure** (wrong password, unreachable target) →
  job fails with rustic's error surfaced.
- **Snapshot not found** → `node_from_snapshot_path` errors inside the job,
  which fails with a "snapshot not found" message. (The WebUI only offers
  `snapshot_id`s from the `backup.snapshots` list it already fetched, so an
  unknown id shouldn't reach here via the UI; no redundant pre-flight
  repo-open just to re-validate it.)
- **Invalid destination** (not under `/fs`, traversal, missing filesystem,
  non-empty without overwrite) → rejected up front (see validation).
- **Mid-restore failure** (network drop, disk full) → job fails; partial
  files may remain at the destination. This is documented, not cleaned up:
  rustic's restore is re-runnable (re-restoring overwrites/completes), so
  the recovery is "fix the cause and restore again." The engine does not
  attempt a partial-restore rollback.

## Testing

- **Unit** (pure / fs-fixture, matching the crate's existing style):
  destination validation — accepts a path under `/fs`, rejects `..`
  traversal, rejects a symlink that escapes `/fs`, rejects a missing
  filesystem root; the non-empty-without-overwrite gate; the overwrite
  flag threading.
- **Integration** (rustic + real FS — the crate already flags snapshot/repo
  paths as integration territory): a backup → restore → compare round-trip
  against a local repo, asserting the restored tree matches the source and
  that a coarse progress fraction reaches 1.0.
- **Real-world confirmation:** es5445's disaster-recovery scenario — a repo
  created by one instance, restored on another from S3 — is the acceptance
  test we can't fully reproduce in the lab; validate against a real S3 repo
  before calling it done.

## Follow-ups (explicitly not v1)

- File/folder-level browse-and-restore within a snapshot.
- In-place restore to original paths.
- These are additive: the same `restore` primitive (rustic
  `node_from_snapshot_path` accepts a `snapshot:/subpath`) extends to
  file-level restore later without reworking v1.
