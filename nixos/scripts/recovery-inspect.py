"""Private recovery staging and read-only inventory. No import/apply operation."""

import argparse
import json
import os
import re
import stat
import sys
import uuid


BASE = "/var/lib/nasty-recovery"
MAX_ENTRIES = 50_000
MAX_BYTES = 1024 * 1024 * 1024
MAX_FILE = 64 * 1024 * 1024
MAX_DEPTH = 24
MAX_JSON = 2 * 1024 * 1024
MIN_FREE_BYTES = 128 * 1024 * 1024
DIRECTORY = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
REGULAR = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
SOURCES = {
    "nasty_state": ("var/lib/nasty", "directory"),
    "system_configuration": ("etc/nixos", "directory"),
    "tls_identity": ("var/lib/caddy", "directory"),
    "host_credential_key": ("var/lib/systemd/credential.secret", "file"),
    "source_machine_identity": ("etc/machine-id", "file"),
    "secure_boot_keys": ("var/lib/sbctl", "optional-directory"),
}


class Refusal(Exception):
    """Only fixed, non-secret error codes reach command output."""


def open_directory(path):
    """Open every absolute path component without following symlinks."""
    if not path.startswith("/") or any(p in (".", "..") for p in path.split("/")):
        raise Refusal("invalid_directory_path")
    fd = os.open("/", DIRECTORY)
    try:
        for part in filter(None, path.split("/")):
            child = os.open(part, DIRECTORY, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def private_directory(fd, uid):
    info = os.fstat(fd)
    if info.st_uid != uid or stat.S_IMODE(info.st_mode) != 0o700:
        raise Refusal("workspace_not_private")


def open_workspace(path=BASE, uid=0, create=True):
    # /var/lib must already exist; do not create or traverse arbitrary parents.
    parent, name = os.path.split(path)
    parent_fd = open_directory(parent)
    try:
        if create:
            try:
                os.mkdir(name, 0o700, dir_fd=parent_fd)
                os.fsync(parent_fd)
            except FileExistsError:
                pass
        fd = os.open(name, DIRECTORY, dir_fd=parent_fd)
    finally:
        os.close(parent_fd)
    try:
        private_directory(fd, uid)
        # Staging must not need a data pool (or live on a separate mounted pool).
        if uid == 0 and os.fstat(fd).st_dev != os.stat("/").st_dev:
            raise Refusal("workspace_not_on_root_filesystem")
        return fd
    except BaseException:
        os.close(fd)
        raise


def save_state(fd, state):
    out = os.open("state.new", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
    with os.fdopen(out, "w") as stream:
        json.dump(state, stream)
        stream.flush()
        os.fsync(stream.fileno())
    os.rename("state.new", "state.json", src_dir_fd=fd, dst_dir_fd=fd)
    os.fsync(fd)


def check_space(fd, needed=0):
    space = os.fstatvfs(fd)
    if space.f_bavail * space.f_frsize < MIN_FREE_BYTES + needed:
        raise Refusal("insufficient_staging_space")


def walk(source, destination=None, budget=None, depth=0, device=None):
    """Bounded fd-relative traversal. Inspection uses the same safety checks."""
    if budget is None:
        budget = {"entries": 0, "bytes": 0}
    if depth > MAX_DEPTH:
        raise Refusal("depth_limit")
    if device is None:
        device = os.fstat(source).st_dev
    if os.fstat(source).st_dev != device:
        raise Refusal("mount_boundary")
    with os.scandir(source) as entries:
        for entry in entries:
            budget["entries"] += 1
            if budget["entries"] > MAX_ENTRIES:
                raise Refusal("entry_limit")
            before = os.stat(entry.name, dir_fd=source, follow_symlinks=False)
            if before.st_dev != device:
                raise Refusal("mount_boundary")
            if stat.S_ISDIR(before.st_mode):
                child = os.open(entry.name, DIRECTORY, dir_fd=source)
                target = None
                try:
                    if os.fstat(child).st_ino != before.st_ino:
                        raise Refusal("source_changed")
                    if destination is not None:
                        os.mkdir(entry.name, 0o700, dir_fd=destination)
                        target = os.open(entry.name, DIRECTORY, dir_fd=destination)
                    walk(child, target, budget, depth + 1, device)
                finally:
                    os.close(child)
                    if target is not None:
                        os.close(target)
            elif stat.S_ISREG(before.st_mode) and before.st_nlink == 1:
                if before.st_size > MAX_FILE:
                    raise Refusal("file_size_limit")
                opened = os.open(entry.name, REGULAR, dir_fd=source)
                try:
                    actual = os.fstat(opened)
                    if (actual.st_dev, actual.st_ino, actual.st_mode, actual.st_nlink) != (
                        before.st_dev, before.st_ino, before.st_mode, 1
                    ):
                        raise Refusal("source_changed")
                    target = None
                    try:
                        if destination is not None:
                            target = os.open(entry.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                             0o600, dir_fd=destination)
                        count = 0
                        while chunk := os.read(opened, 64 * 1024):
                            count += len(chunk)
                            budget["bytes"] += len(chunk)
                            if count > MAX_FILE or budget["bytes"] > MAX_BYTES:
                                raise Refusal("byte_limit")
                            if target is not None:
                                check_space(destination, len(chunk))
                                view = memoryview(chunk)
                                while view:
                                    view = view[os.write(target, view):]
                        after = os.fstat(opened)
                        if (after.st_size, after.st_mtime_ns, after.st_ctime_ns) != (
                            actual.st_size, actual.st_mtime_ns, actual.st_ctime_ns
                        ) or count != actual.st_size:
                            raise Refusal("source_changed")
                        if target is not None:
                            os.fsync(target)
                    finally:
                        if target is not None:
                            os.close(target)
                finally:
                    os.close(opened)
            else:
                raise Refusal("unsafe_entry")
    if destination is not None:
        os.fsync(destination)
    return budget


def ancestor(parent, child):
    wanted = os.fstat(parent)
    fd = os.dup(child)
    try:
        while True:
            here = os.fstat(fd)
            if (here.st_dev, here.st_ino) == (wanted.st_dev, wanted.st_ino):
                return True
            above = os.open("..", DIRECTORY, dir_fd=fd)
            info = os.fstat(above)
            if (here.st_dev, here.st_ino) == (info.st_dev, info.st_ino):
                os.close(above)
                return False
            os.close(fd)
            fd = above
    finally:
        os.close(fd)


def stage(source_path, workspace):
    source = open_directory(source_path)
    identifier = uuid.uuid4().hex
    session = tree = None
    try:
        if ancestor(source, workspace) or ancestor(workspace, source):
            raise Refusal("source_overlaps_workspace")
        check_space(workspace)
        os.mkdir(identifier, 0o700, dir_fd=workspace)
        os.fsync(workspace)
        session = os.open(identifier, DIRECTORY, dir_fd=workspace)
        save_state(session, {"schema": 1, "status": "incomplete"})
        os.mkdir("tree", 0o700, dir_fd=session)
        tree = os.open("tree", DIRECTORY, dir_fd=session)
        try:
            budget = walk(source, tree)
        except (Refusal, OSError) as error:
            # Preserve partial evidence, never publish it as completed staging.
            code = str(error) if isinstance(error, Refusal) else "filesystem_error"
            save_state(session, {"schema": 1, "status": "incomplete", "error": code})
            return {"stage_id": identifier, "status": "incomplete", "error": code}
        save_state(session, {"schema": 1, "status": "staged", **budget})
        return {"stage_id": identifier, "status": "staged"}
    finally:
        os.close(source)
        if tree is not None:
            os.close(tree)
        if session is not None:
            os.close(session)


def read_beneath(root, path, maximum=MAX_JSON):
    parts = path.split("/")
    directory = os.dup(root)
    try:
        for part in parts[:-1]:
            child = os.open(part, DIRECTORY, dir_fd=directory)
            os.close(directory)
            directory = child
        fd = os.open(parts[-1], REGULAR, dir_fd=directory)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > maximum:
                raise Refusal("invalid_metadata_file")
            content = stream.read(maximum + 1)
            if len(content) > maximum:
                raise Refusal("metadata_limit")
            return content
    finally:
        os.close(directory)


def presence(root, path, directory):
    try:
        if directory:
            fd = os.dup(root)
            try:
                for part in path.split("/"):
                    child = os.open(part, DIRECTORY, dir_fd=fd)
                    os.close(fd)
                    fd = child
            finally:
                os.close(fd)
        else:
            if not read_beneath(root, path, MAX_FILE):
                return "empty"
        return "present"
    except FileNotFoundError:
        return "missing"


def inspect(identifier, workspace, local_version=None, uid=0):
    if not re.fullmatch(r"[a-f0-9]{32}", identifier):
        raise Refusal("invalid_stage_id")
    session = os.open(identifier, DIRECTORY, dir_fd=workspace)
    tree = None
    try:
        private_directory(session, uid)
        state = json.loads(read_beneath(session, "state.json", 4096))
        if not isinstance(state, dict) or state.get("schema") != 1 or state.get("status") != "staged":
            raise Refusal("stage_incomplete")
        tree = os.open("tree", DIRECTORY, dir_fd=session)
        private_directory(tree, uid)
        budget = walk(tree)
        coverage = {name: presence(tree, path, kind != "file") for name, (path, kind) in SOURCES.items()}
        blockers = ["system_import_not_implemented", "snapshot_consistency_unverified"]
        for name, (_, kind) in SOURCES.items():
            if kind != "optional-directory" and coverage[name] != "present":
                blockers.append(f"missing_or_empty:{name}")
        for path, code in (("var/lib/nasty/auth.json", "management_auth"),
                           ("var/lib/nasty/settings.json", "settings")):
            try:
                if not isinstance(json.loads(read_beneath(tree, path)), dict):
                    raise ValueError()
            except (OSError, ValueError, RecursionError, Refusal):
                blockers.append(f"unvalidated_json:{code}")
        try:
            machine = read_beneath(tree, "etc/machine-id", 128).strip()
            if not re.fullmatch(rb"[0-9a-f]{32}", machine) or machine == b"0" * 32:
                blockers.append("invalid_source_machine_identity")
        except (OSError, Refusal):
            blockers.append("invalid_source_machine_identity")
        relation = "unavailable"
        try:
            version = read_beneath(tree, "var/lib/nasty/version", 128).decode().strip()
            if not re.fullmatch(r"v?\d+\.\d+\.\d+|[a-f0-9]{7,40}", version):
                relation = "unrecognized"
            elif local_version:
                relation = "matches_local_label" if version == local_version.strip() else "different_label"
        except (OSError, Refusal, UnicodeError):
            pass
        if relation != "matches_local_label":
            blockers.append("version_compatibility_unverified")
        return {
            "schema": 1, "stage_id": identifier, "status": "inspected",
            "import_supported": False, "coverage": coverage, "version_relation": relation,
            "blockers": blockers,
            "unverified": ["state_schemas", "credential_decryption", "storage_identity", "unix_samba_identity",
                           "ad_recovery", "hardware_compatibility", "unknown_and_transient_state"],
            **budget,
        }
    finally:
        if tree is not None:
            os.close(tree)
        os.close(session)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("stage", help="copy an already-extracted recovery tree into private staging")
    create.add_argument("--source", required=True, help="absolute directory containing etc/ and var/")
    report = commands.add_parser("inspect", help="read-only, redacted inventory of a completed stage")
    report.add_argument("stage_id")
    args = parser.parse_args()
    if os.geteuid() != 0:
        print(json.dumps({"error": "root_required"}))
        return 1
    workspace = None
    try:
        workspace = open_workspace(create=args.command == "stage")
        if args.command == "stage":
            result = stage(args.source, workspace)
        else:
            try:
                with open("/etc/nasty-version", encoding="utf-8") as stream:
                    local_version = stream.read(129)
            except OSError:
                local_version = None
            result = inspect(args.stage_id, workspace, local_version)
        print(json.dumps(result, sort_keys=True))
        return 0 if result["status"] != "incomplete" else 1
    except (Refusal, OSError, ValueError, RecursionError):
        # Do not serialize exceptions: paths, filenames or JSON may contain secrets.
        error = sys.exc_info()[1]
        print(json.dumps({"error": str(error) if isinstance(error, Refusal) else "inspection_refused"}))
        return 1
    finally:
        if workspace is not None:
            os.close(workspace)


if __name__ == "__main__":
    raise SystemExit(main())
