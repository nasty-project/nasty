"""Remove only abandoned update result links; caller holds nasty-update.lock.

Never remove transaction directories/backups, arbitrary GC roots, or profile
generations. Refuse indeterminate activity/recovery state before unlinking.
"""
import os
from pathlib import Path
import re
import stat
import subprocess
import sys


def remove_abandoned_roots(state, protected, marker, units_active, owner=0, check_only=False):
    if marker.exists() or marker.is_symlink():
        raise RuntimeError("Update recovery is pending; preserve its roots and backups")
    if units_active():
        raise RuntimeError("An update, rebuild or rollback is active; cleanup refused")
    metadata = state.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != owner or metadata.st_mode & 0o022:
        raise RuntimeError("Unsafe update-state directory; cleanup refused")
    candidates = []
    for directory in sorted(state.iterdir()):
        if not re.fullmatch(r"update\.[A-Za-z0-9]{6}", directory.name):
            continue
        metadata = directory.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != owner or metadata.st_mode & 0o022:
            continue
        link = directory / "result"
        try:
            metadata = link.lstat()
        except FileNotFoundError:
            continue
        if not stat.S_ISLNK(metadata.st_mode) or metadata.st_uid != owner:
            continue
        target = os.readlink(link)
        if not re.fullmatch(r"/nix/store/[0-9a-df-np-sv-z]{32}-nixos-system-[^/]+", target):
            continue
        if target not in protected:
            candidates.append((link, target))
    # Do not change even one root if the safety gate fails mid-inventory.
    if marker.exists() or marker.is_symlink() or units_active():
        raise RuntimeError("Update/recovery state changed; cleanup refused")
    if check_only:
        return 0
    for link, target in candidates:
        if link.is_symlink() and os.readlink(link) == target:
            link.unlink()
            print(f"Removed abandoned update result link: {link}")
    return len(candidates)


def active_units():
    output = subprocess.run([
        "systemctl", "list-units", "--all", "--plain", "--no-legend", "--no-pager",
        "--state=active,activating,deactivating,reloading", "nasty-update*", "nasty-manual-rebuild*",
    ], check=True, capture_output=True, text=True)
    return bool(output.stdout.strip())


def main():
    if sys.argv[1:] not in ([], ["--check"]):
        raise RuntimeError("Only --check is supported")
    if os.geteuid() != 0:
        raise RuntimeError("Run nasty-cleanup as root")
    paths = [Path("/run/current-system"), Path("/run/booted-system"),
             Path("/nix/var/nix/profiles/system"), Path("/etc/nixos/result")]
    paths += list(Path("/nix/var/nix/profiles").glob("system-*-link"))
    if any(not path.is_symlink() or not path.exists() for path in paths[:3]):
        raise RuntimeError("Cannot establish running/booted/profile systems; cleanup refused")
    if paths[0].resolve() != paths[2].resolve():
        raise RuntimeError("Running system differs from profile; cleanup refused")
    protected = {str(path.resolve()) for path in paths if path.is_symlink()}
    count = remove_abandoned_roots(Path("/var/lib/nasty"), protected,
                                   Path("/run/nasty-wrapper-update-in-progress"), active_units,
                                   check_only=sys.argv[1:] == ["--check"])
    print(f"Removed {count} abandoned update result links; preserved system/profile roots and backups")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        raise SystemExit(f"Update-root cleanup refused: {error}") from error
