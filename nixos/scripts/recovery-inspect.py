"""Private recovery staging and read-only inventory. No import/apply operation."""

import argparse
import json
import os
import re
import stat
import signal
import subprocess
import sys
import threading
import resource
from urllib.parse import urlsplit
import uuid


BASE = "/var/lib/nasty-recovery"
MAX_ENTRIES = 50_000
MAX_BYTES = 1024 * 1024 * 1024
MAX_FILE = 64 * 1024 * 1024
MAX_DEPTH = 24
MAX_JSON = 2 * 1024 * 1024
MIN_FREE_BYTES = 128 * 1024 * 1024
RUSTIC = "@rustic@"
CA_BUNDLE = "@ca_bundle@"
RETRIEVAL_SECONDS = 600
MAX_SNAPSHOTS = 10_000
MAX_LISTING = 16 * 1024 * 1024
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
            "retrieval": ("authenticated_repository" if state.get("retrieval") == "rustic_authenticated_dump"
                          else "unverified_source"),
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


def private_input(path, uid=0):
    parent, name = os.path.split(path)
    directory = open_directory(parent)
    try:
        fd = os.open(name, REGULAR, dir_fd=directory)
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != uid
                or stat.S_IMODE(info.st_mode) != 0o600 or info.st_size > 16 * 1024):
            os.close(fd)
            raise Refusal("credentials_not_private")
        return fd
    finally:
        os.close(directory)


def retrieval_options(path, uid=0):
    with os.fdopen(private_input(path, uid), "rb") as stream:
        options = json.loads(stream.read(16 * 1024 + 1))
    if not isinstance(options, dict) or set(options) != {"repository", "password_file"}:
        raise Refusal("invalid_repository_options")
    repo = options["repository"]
    password = options["password_file"]
    if not isinstance(repo, str) or not isinstance(password, str) or len(repo) > 4096:
        raise Refusal("invalid_repository_options")
    if repo.startswith("/"):
        # Local repository must already exist; never initialize it.
        fd = open_directory(repo)
        os.close(fd)
    elif repo.startswith("rest:https://"):
        url = urlsplit(repo[5:])
        if not url.hostname or url.fragment or url.query:
            raise Refusal("invalid_repository_options")
    else:
        # In particular, no rclone subprocesses, arbitrary backend options,
        # insecure HTTP, profiles, hooks or password commands.
        raise Refusal("unsupported_repository_backend")
    if any(ord(c) < 32 for c in repo):
        raise Refusal("invalid_repository_options")
    return repo, private_input(password, uid)


def exact(stream, count):
    data = bytearray()
    while len(data) < count:
        chunk = stream.read(count - len(data))
        if not chunk:
            raise Refusal("truncated_archive")
        data.extend(chunk)
    return bytes(data)


def tar_number(raw):
    text = raw.rstrip(b"\0 ").lstrip(b" ")
    if not text or any(c not in b"01234567" for c in text):
        raise Refusal("invalid_archive_header")
    return int(text, 8)


def extract_tar(stream, root):
    """Restricted uncompressed tar; never use extractall or restore metadata."""
    seen = set()
    budget = {"entries": 0, "bytes": 0}
    created = set()
    while True:
        header = exact(stream, 512)
        if header == bytes(512):
            if exact(stream, 512) != bytes(512):
                raise Refusal("invalid_archive_end")
            # Permit only a bounded number of zero padding records. Drain to
            # EOF so a failed producer cannot masquerade as a complete dump.
            padding = 0
            while chunk := stream.read(4096):
                padding += len(chunk)
                if padding > 64 * 1024 or any(chunk):
                    raise Refusal("invalid_archive_end")
            break
        if tar_number(header[148:156]) != sum(header[:148]) + 8 * 32 + sum(header[156:]):
            raise Refusal("invalid_archive_checksum")
        if header[257:263] not in (b"ustar\0", b"ustar "):
            raise Refusal("unsupported_archive_header")
        kind = header[156:157]
        if kind not in (b"0", b"\0", b"5"):
            # Reject GNU longnames/PAX too: their metadata is not implicitly
            # trusted or allocated before checking limits.
            raise Refusal("unsafe_archive_entry")
        size = tar_number(header[124:136])
        if size > MAX_FILE or (kind == b"5" and size != 0):
            raise Refusal("file_size_limit")
        raw = header[:100].split(b"\0", 1)[0]
        if header[257:263] == b"ustar\0":
            prefix = header[345:500].split(b"\0", 1)[0]
            if prefix:
                raw = prefix + b"/" + raw
        name = raw.decode("utf-8", errors="strict")
        if kind == b"5" and name.endswith("/"):
            name = name[:-1]
        # Rustic may include a directory record for the snapshot root.
        if name in ("", ".") and kind == b"5":
            parts = []
        else:
            parts = name.split("/")
            if (not name or any(p in ("", ".", "..") or len(p.encode()) > 255 for p in parts)
                    or name.startswith("/") or "\\" in name or any(ord(c) < 32 for c in name)):
                raise Refusal("unsafe_archive_path")
        if len(parts) > MAX_DEPTH:
            raise Refusal("depth_limit")
        if name in seen:
            raise Refusal("duplicate_archive_entry")
        seen.add(name)
        if len(seen) > MAX_ENTRIES:
            raise Refusal("entry_limit")
        budget["bytes"] += size
        if budget["bytes"] > MAX_BYTES:
            raise Refusal("byte_limit")
        directory = os.dup(root)
        try:
            directories = parts if kind == b"5" else parts[:-1]
            for index, part in enumerate(directories):
                path = "/".join(parts[:index + 1])
                if path not in created:
                    os.mkdir(part, 0o700, dir_fd=directory)
                    os.fsync(directory)
                    created.add(path)
                    if len(created) > MAX_ENTRIES:
                        raise Refusal("entry_limit")
                child = os.open(part, DIRECTORY, dir_fd=directory)
                os.close(directory)
                directory = child
            if kind != b"5":
                check_space(directory, size)
                target = os.open(parts[-1], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o600, dir_fd=directory)
                try:
                    remaining = size
                    while remaining:
                        chunk = exact(stream, min(remaining, 64 * 1024))
                        check_space(directory, len(chunk))
                        view = memoryview(chunk)
                        while view:
                            view = view[os.write(target, view):]
                        remaining -= len(chunk)
                    os.fsync(target)
                finally:
                    os.close(target)
                created.add(name)
                if len(created) > MAX_ENTRIES:
                    raise Refusal("entry_limit")
                if size % 512:
                    exact(stream, 512 - size % 512)
            os.fsync(directory)
        finally:
            os.close(directory)
    budget["entries"] = len(created)
    os.fsync(root)
    return budget


def child_limits():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024 * 1024,) * 2)
    resource.setrlimit(resource.RLIMIT_CPU, (120, 120))
    resource.setrlimit(resource.RLIMIT_FSIZE, (0, 0))


def spawn_rustic(arguments, repository, password, scratch):
    """Start the pinned rustic CLI with no ambient profile, hooks or credentials."""
    # An explicit absolute empty profile prevents ambient /etc/rustic,
    # HOME or working-directory profiles (and their hooks) being loaded.
    profile = os.open("empty.toml", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=scratch)
    os.close(profile)
    env = {"PATH": "/nonexistent", "HOME": f"/proc/self/fd/{scratch}",
           "RUSTIC_REPOSITORY": repository,
           "RUSTIC_PASSWORD_FILE": f"/proc/self/fd/{password}",
           "SSL_CERT_FILE": CA_BUNDLE, "LANG": "C.UTF-8"}
    process = subprocess.Popen(
        [RUSTIC, "--use-profile", f"/proc/self/fd/{scratch}/empty.toml", "--no-cache", *arguments],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        env=env, cwd="/", pass_fds=(password, scratch), start_new_session=True,
        preexec_fn=child_limits,
    )
    def terminate():
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    timer = threading.Timer(RETRIEVAL_SECONDS, terminate)
    timer.daemon = True
    timer.start()
    return process, timer


def reap(process, timer):
    if timer is not None:
        timer.cancel()
    if process is not None:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.wait()
        process.stdout.close()


def retrieve(options_path, snapshot, workspace, uid=0):
    if not re.fullmatch(r"[a-f0-9]{64}", snapshot):
        raise Refusal("full_snapshot_id_required")
    repository, password = retrieval_options(options_path, uid)
    identifier = uuid.uuid4().hex
    session = tree = process = timer = None
    try:
        if repository.startswith("/"):
            repository_fd = open_directory(repository)
            try:
                if ancestor(repository_fd, workspace) or ancestor(workspace, repository_fd):
                    raise Refusal("repository_overlaps_workspace")
            finally:
                os.close(repository_fd)
        check_space(workspace)
        os.mkdir(identifier, 0o700, dir_fd=workspace)
        os.fsync(workspace)
        session = os.open(identifier, DIRECTORY, dir_fd=workspace)
        save_state(session, {"schema": 1, "status": "incomplete"})
        os.mkdir("tree", 0o700, dir_fd=session)
        tree = os.open("tree", DIRECTORY, dir_fd=session)
        try:
            process, timer = spawn_rustic(["dump", "--archive", "tar", snapshot], repository, password, session)
            budget = extract_tar(process.stdout, tree)
            if process.wait() != 0:
                raise Refusal("repository_read_failed")
            timer.cancel()
            save_state(session, {"schema": 1, "status": "staged", "retrieval": "rustic_authenticated_dump",
                                 "snapshot_id": snapshot, **budget})
            return {"stage_id": identifier, "status": "staged"}
        except (Refusal, OSError, ValueError, subprocess.SubprocessError) as error:
            code = str(error) if isinstance(error, Refusal) else "repository_read_failed"
            save_state(session, {"schema": 1, "status": "incomplete", "error": code})
            return {"stage_id": identifier, "status": "incomplete", "error": code}
    finally:
        reap(process, timer)
        os.close(password)
        if tree is not None:
            os.close(tree)
        if session is not None:
            os.close(session)


def recovery_paths(paths):
    """Required recovery categories not covered by any snapshot source path."""
    missing = []
    for name, (path, kind) in SOURCES.items():
        if kind == "optional-directory":
            continue
        wanted = "/" + path
        if not any(p == "/" or wanted == p or wanted.startswith(p.rstrip("/") + "/") for p in paths):
            missing.append(name)
    return missing


def summarize_snapshots(document):
    """Fixed, validated fields only; never echo arbitrary repository metadata."""
    if not isinstance(document, list):
        raise Refusal("invalid_snapshot_listing")
    snapshots = []
    for item in document:
        # rustic emits a plain list when ungrouped, else {group_key, snapshots}.
        if isinstance(item, dict) and "group_key" in item:
            group = item.get("snapshots")
            if not isinstance(group, list):
                raise Refusal("invalid_snapshot_listing")
            snapshots.extend(group)
        else:
            snapshots.append(item)
    if len(snapshots) > MAX_SNAPSHOTS:
        raise Refusal("snapshot_limit")
    result = []
    for snap in snapshots:
        if not isinstance(snap, dict) or not isinstance(snap.get("id"), str) \
                or not re.fullmatch(r"[a-f0-9]{64}", snap["id"]):
            raise Refusal("invalid_snapshot_listing")
        time = snap.get("time")
        if not (isinstance(time, str) and re.fullmatch(
                r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})(\[[A-Za-z0-9_/+-]{1,64}\])?",
                time)):
            time = None
        host = snap.get("hostname")
        if not (isinstance(host, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.-]{0,252}", host)):
            host = None
        paths = snap.get("paths")
        if not isinstance(paths, list) or not all(
                isinstance(p, str) and p.startswith("/") and len(p) <= 4096 for p in paths):
            paths = []
        result.append({"id": snap["id"], "time": time, "hostname": host,
                       "missing_recovery_paths": recovery_paths(paths)})
    return result


def snapshots(options_path, workspace, uid=0):
    """List snapshots without touching staging or the repository."""
    repository, password = retrieval_options(options_path, uid)
    # Transient private scratch for the empty profile; not a stage session.
    scratch_name = ".list-" + uuid.uuid4().hex
    scratch = process = timer = None
    try:
        os.mkdir(scratch_name, 0o700, dir_fd=workspace)
        scratch = os.open(scratch_name, DIRECTORY, dir_fd=workspace)
        try:
            process, timer = spawn_rustic(["snapshots", "--json"], repository, password, scratch)
            output = process.stdout.read(MAX_LISTING + 1)
            if len(output) > MAX_LISTING:
                raise Refusal("listing_limit")
            if process.wait() != 0:
                raise Refusal("repository_read_failed")
            timer.cancel()
            document = json.loads(output)
        except (OSError, ValueError, RecursionError, subprocess.SubprocessError):
            raise Refusal("repository_read_failed")
        return {"schema": 1, "status": "listed", "snapshots": summarize_snapshots(document)}
    finally:
        reap(process, timer)
        os.close(password)
        if scratch is not None:
            try:
                os.unlink("empty.toml", dir_fd=scratch)
            except FileNotFoundError:
                pass
            os.close(scratch)
            os.rmdir(scratch_name, dir_fd=workspace)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("stage", help="copy an already-extracted recovery tree into private staging")
    create.add_argument("--source", required=True, help="absolute directory containing etc/ and var/")
    report = commands.add_parser("inspect", help="read-only, redacted inventory of a completed stage")
    report.add_argument("stage_id")
    fetch = commands.add_parser("retrieve", help="retrieve an explicit snapshot into private staging")
    fetch.add_argument("--options-file", required=True, help="absolute root-owned 0600 JSON options file")
    fetch.add_argument("--snapshot", required=True, help="full 64-character snapshot ID; no latest or prefixes")
    listing = commands.add_parser("snapshots", help="list repository snapshots and their recovery-path coverage")
    listing.add_argument("--options-file", required=True, help="absolute root-owned 0600 JSON options file")
    args = parser.parse_args()
    if os.geteuid() != 0:
        print(json.dumps({"error": "root_required"}))
        return 1
    workspace = None
    try:
        workspace = open_workspace(create=args.command != "inspect")
        if args.command == "stage":
            result = stage(args.source, workspace)
        elif args.command == "retrieve":
            result = retrieve(args.options_file, args.snapshot, workspace)
        elif args.command == "snapshots":
            result = snapshots(args.options_file, workspace)
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
