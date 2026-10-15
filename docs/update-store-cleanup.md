# Update storage and abandoned GC roots

Build-space override moves **temporary build workspace**, not `/nix/store`.
Downloaded packages, build outputs and the new system closure still consume the
store filesystem, usually the OS root filesystem. Free space on a data pool does
not imply free store space. Updates check for at least 4 GiB of available store
headroom before staging/building; this is a minimum guard, not a closure-size
estimate or a guarantee that a large upgrade will fit.

`nasty-cleanup` retains its generation policy (last three by default;
`nasty-cleanup all` keeps the current profile generation only). It now also
removes abandoned indirect update roots left by older updaters or interrupted
builds. Otherwise those roots can pin old closures even after their profile
generations have been deleted.

## Safety boundaries

- Cleanup, the transactional updater, and `nasty-rebuild` share an exclusive
  `/run/lock/nasty-update.lock`. Cleanup/updater/rebuild refuse a busy lock;
  detached interrupted-update rollback acquires it before activation/cleanup.
- Cleanup also refuses active/starting/stopping legacy update, manual-rebuild,
  or rollback units, pending wrapper recovery, indeterminate systemd state,
  mismatched running/profile systems, or unsafe state-directory ownership/mode.
- It scans only root-owned, non-writable-by-others `update.XXXXXX` directories
  immediately under `/var/lib/nasty`. It unlinks only root-owned `result`
  symlinks with direct `/nix/store/<hash>-nixos-system-*` targets.
- Running, booted, surviving system-profile generations and `/etc/nixos/result`
  targets are protected. Arbitrary GC roots, unrelated directories/files,
  transaction directories, and rollback backups are not removed.
- Completed update/rollback cleanup explicitly unlinks its temporary result
  before workspace deletion. Failed rollback keeps its evidence/root; migration
  is blocked while the recovery marker remains.
- Garbage collection remains an explicit cleanup operation, not an automatic
  pre-update deletion. The ordinary Nix GC root rules still protect running
  systems/processes and retained profiles.

The helper inventories/checks before generation pruning, then re-inventories
surviving profiles and removes eligible roots before GC. `all` is still a
deliberate removal of non-current profile generations, not a promise to preserve
all rollback choices. Keep the default retention policy unless you explicitly
want that tradeoff.

If an update reports insufficient `/nix/store` space, run the appropriate cleanup
policy and inspect `df -h /nix/store /boot` before retrying. Do not manually erase
store directories. If the retained/current system plus the upgrade still cannot
fit, enlarge the OS/store filesystem rather than assuming build spillover fixes
installed-output space.

No disk resizing, live service activation, networking changes, or reboot is
performed by this fix. Deploying it remains a separate reviewed update.
