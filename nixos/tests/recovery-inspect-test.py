"""Adversarial staging fixtures; no host recovery state, services or pools."""
import importlib.util
import json
import io
import os
from pathlib import Path
import tempfile
import unittest
import tarfile
import sys
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("recovery", Path(__file__).parent.parent / "scripts/recovery-inspect.py")
recovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recovery)


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.source = self.root / "source"
        self.source.mkdir()
        self.base = self.root / "workspace"
        self.fd = recovery.open_workspace(str(self.base), os.getuid())

    def tearDown(self):
        os.close(self.fd)
        self.temp.cleanup()

    def put(self, path, value):
        target = self.source / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(value)

    def stage(self):
        return recovery.stage(str(self.source), self.fd)

    def inspect(self, identifier):
        return recovery.inspect(identifier, self.fd, "v0.1.1", os.getuid())

    def test_private_copy_and_read_only_redacted_inspection(self):
        secret = "DO-NOT-DISPLAY-this-password"
        self.put("var/lib/nasty/auth.json", json.dumps({"users": [{"password_hash": secret}]}))
        self.put("var/lib/nasty/settings.json", json.dumps({"token": secret}))
        self.put("var/lib/nasty/version", "v0.1.1")
        self.put("etc/machine-id", "a" * 32)
        self.put("etc/nixos/custom.nix", 'throw "must never evaluate"')
        self.put("var/lib/caddy/key", secret)
        self.put("var/lib/systemd/credential.secret", secret)
        result = self.stage()
        self.assertEqual(result["status"], "staged")
        session = self.base / result["stage_id"]
        before = {p: p.read_bytes() for p in session.rglob("*") if p.is_file()}
        report = self.inspect(result["stage_id"])
        self.assertNotIn(secret, json.dumps(report))
        self.assertFalse(report["import_supported"])
        self.assertIn("system_import_not_implemented", report["blockers"])
        self.assertEqual(report["version_relation"], "matches_local_label")
        self.assertEqual(report["coverage"]["host_credential_key"], "present")
        self.assertEqual(before, {p: p.read_bytes() for p in session.rglob("*") if p.is_file()})
        for p in session.rglob("*"):
            self.assertEqual(p.stat().st_mode & 0o777, 0o700 if p.is_dir() else 0o600)

    def test_missing_and_malformed_material_is_not_recovery_ready(self):
        self.put("var/lib/nasty/auth.json", "secret is not JSON")
        self.put("var/lib/nasty/version", "unrecognized-password")
        self.put("etc/machine-id", "0" * 32)
        report = self.inspect(self.stage()["stage_id"])
        self.assertIn("missing_or_empty:host_credential_key", report["blockers"])
        self.assertIn("unvalidated_json:management_auth", report["blockers"])
        self.assertIn("invalid_source_machine_identity", report["blockers"])
        self.assertEqual(report["version_relation"], "unrecognized")
        self.assertNotIn("password", json.dumps(report))

    def test_symlink_file_directory_and_broken_link_are_refused(self):
        for target in (str(self.root), "/etc/passwd", "missing"):
            with self.subTest(target=target):
                link = self.source / "secret-name"
                link.symlink_to(target)
                result = self.stage()
                self.assertEqual(result["error"], "unsafe_entry")
                self.assertNotIn("secret-name", json.dumps(result))
                with self.assertRaises(recovery.Refusal):
                    self.inspect(result["stage_id"])
                link.unlink()

    def test_fifo_and_hardlinks_are_refused_without_opening_them(self):
        os.mkfifo(self.source / "pipe")
        self.assertEqual(self.stage()["error"], "unsafe_entry")
        (self.source / "pipe").unlink()
        self.put("one", "secret")
        os.link(self.source / "one", self.source / "two")
        self.assertEqual(self.stage()["error"], "unsafe_entry")

    def test_source_symlink_and_workspace_overlap_are_refused(self):
        alias = self.root / "alias"
        alias.symlink_to(self.source)
        with self.assertRaises(OSError):
            recovery.stage(str(alias), self.fd)
        for path in (self.base, self.root):
            with self.assertRaises(recovery.Refusal):
                recovery.stage(str(path), self.fd)
        with self.assertRaises(recovery.Refusal):
            recovery.stage(str(self.source / ".." / "source"), self.fd)

    def test_limits_leave_durable_incomplete_stage(self):
        self.put("large", "0123456789")
        for setting, value in (("MAX_FILE", 4), ("MAX_BYTES", 4), ("MAX_ENTRIES", 0), ("MAX_DEPTH", -1)):
            with self.subTest(setting=setting), patch.object(recovery, setting, value):
                result = self.stage()
                self.assertEqual(result["status"], "incomplete")
                state = json.loads((self.base / result["stage_id"] / "state.json").read_text())
                self.assertEqual(state["status"], "incomplete")

    def test_unsafe_stage_identifier_and_modified_stage_are_refused(self):
        for identifier in ("../source", "a" * 32 + "/..", "not-an-id"):
            with self.assertRaises(recovery.Refusal):
                self.inspect(identifier)
        result = self.stage()
        (self.base / result["stage_id"] / "tree" / "escape").symlink_to("/etc/passwd")
        with self.assertRaises(recovery.Refusal):
            self.inspect(result["stage_id"])

    def test_workspace_permissions_and_symlinks_fail_closed(self):
        self.base.chmod(0o755)
        with self.assertRaises(recovery.Refusal):
            recovery.open_workspace(str(self.base), os.getuid())
        self.base.chmod(0o700)
        alias = self.root / "workspace-alias"
        alias.symlink_to(self.base)
        with self.assertRaises(OSError):
            recovery.open_workspace(str(alias), os.getuid())

    def test_inspection_does_not_create_missing_workspace(self):
        missing = self.root / "absent"
        with self.assertRaises(FileNotFoundError):
            recovery.open_workspace(str(missing), os.getuid(), create=False)
        self.assertFalse(missing.exists())

    def test_interruption_cannot_publish_a_completed_stage(self):
        self.put("var/lib/nasty/settings.json", "{}")
        with patch.object(recovery, "walk", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                self.stage()
        sessions = list(self.base.iterdir())
        self.assertEqual(len(sessions), 1)
        self.assertEqual(json.loads((sessions[0] / "state.json").read_text())["status"], "incomplete")
        with self.assertRaises(recovery.Refusal):
            self.inspect(sessions[0].name)
        completed = self.stage()
        self.assertNotEqual(completed["stage_id"], sessions[0].name)
        self.assertTrue((sessions[0] / "tree").exists())

    def test_io_failure_does_not_leak_filename_or_publish_stage(self):
        with patch.object(recovery, "walk", side_effect=OSError("sensitive-filename")):
            result = self.stage()
        self.assertEqual(result["error"], "filesystem_error")
        self.assertNotIn("sensitive", json.dumps(result))
        with self.assertRaises(recovery.Refusal):
            self.inspect(result["stage_id"])

    def test_version_label_difference_is_only_a_compatibility_blocker(self):
        self.put("var/lib/nasty/version", "v9.9.9")
        report = self.inspect(self.stage()["stage_id"])
        self.assertEqual(report["version_relation"], "different_label")
        self.assertIn("version_compatibility_unverified", report["blockers"])
        self.assertNotIn("v9.9.9", json.dumps(report))

    def test_low_space_refused_before_creating_a_stage(self):
        with patch.object(recovery, "MIN_FREE_BYTES", 2 ** 80):
            with self.assertRaisesRegex(recovery.Refusal, "insufficient_staging_space"):
                self.stage()
        self.assertEqual(list(self.base.iterdir()), [])

    def archive(self, members):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w", format=tarfile.GNU_FORMAT) as archive:
            for name, kind, data in members:
                entry = tarfile.TarInfo(name)
                entry.type = kind
                entry.size = len(data) if kind == tarfile.REGTYPE else 0
                entry.mode = 0o7777
                entry.uid = 9999
                archive.addfile(entry, io.BytesIO(data))
        return output.getvalue()

    def extract(self, archive):
        destination = self.root / ("extract-" + recovery.uuid.uuid4().hex)
        destination.mkdir(mode=0o700)
        fd = os.open(destination, recovery.DIRECTORY)
        try:
            result = recovery.extract_tar(io.BytesIO(archive), fd)
            return destination, result
        finally:
            os.close(fd)

    def test_restricted_archive_copy_never_restores_permissions(self):
        archive = self.archive([
            (".", tarfile.DIRTYPE, b""), ("etc/", tarfile.DIRTYPE, b""),
            ("etc/machine-id", tarfile.REGTYPE, b"a" * 32),
            ("var/lib/nasty/settings.json", tarfile.REGTYPE, b"{}"),
        ])
        destination, result = self.extract(archive)
        self.assertEqual((destination / "etc/machine-id").read_bytes(), b"a" * 32)
        self.assertEqual(result["bytes"], 34)
        for path in destination.rglob("*"):
            self.assertEqual(path.stat().st_mode & 0o7777, 0o700 if path.is_dir() else 0o600)

    def test_archive_paths_cannot_escape(self):
        for name in ("/escape", "../escape", "etc/../../escape", "etc//file", "./etc/file", "etc\\file", "etc/secret\nfile"):
            with self.subTest(name=name), self.assertRaises(recovery.Refusal):
                self.extract(self.archive([(name, tarfile.REGTYPE, b"secret")]))
        self.assertFalse((self.root / "escape").exists())

    def test_archive_links_special_entries_and_extensions_are_refused(self):
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE,
                     tarfile.BLKTYPE, tarfile.XHDTYPE, tarfile.GNUTYPE_LONGNAME):
            with self.subTest(kind=kind), self.assertRaises(recovery.Refusal):
                self.extract(self.archive([("unsafe", kind, b"")]))

    def test_archive_duplicates_and_type_conflicts_are_refused(self):
        for members in (
            [("same", tarfile.REGTYPE, b"a"), ("same", tarfile.REGTYPE, b"b")],
            [("same", tarfile.REGTYPE, b"a"), ("same/child", tarfile.REGTYPE, b"b")],
            [("same/child", tarfile.REGTYPE, b"a"), ("same", tarfile.REGTYPE, b"b")],
        ):
            with self.subTest(members=members), self.assertRaises((recovery.Refusal, OSError)):
                self.extract(self.archive(members))

    def test_archive_truncation_corruption_trailing_data_and_limits(self):
        archive = self.archive([("fixture", tarfile.REGTYPE, b"abc")])
        for bad in (archive[:100], archive[:513], archive[:1024], b"X" + archive[1:], archive + b"secret"):
            with self.subTest(size=len(bad)), self.assertRaises(recovery.Refusal):
                self.extract(bad)
        for setting, value in (("MAX_FILE", 2), ("MAX_BYTES", 2), ("MAX_ENTRIES", 0), ("MAX_DEPTH", 0)):
            with patch.object(recovery, setting, value), self.assertRaises(recovery.Refusal):
                self.extract(archive)

    def options(self, repository=None):
        password = self.root / "password"
        password.write_text("independent-password")
        password.chmod(0o600)
        options = self.root / "options.json"
        options.write_text(json.dumps({"repository": repository or str(self.source), "password_file": str(password)}))
        options.chmod(0o600)
        return options, password

    def test_options_reject_ambient_config_and_command_backends(self):
        for repo in ("rclone:execute", "rest:http://server/", "s3:bucket", "relative/repo", "rest:https://host/?secret=x"):
            options, _ = self.options(repo)
            with self.subTest(repo=repo), self.assertRaises(recovery.Refusal):
                recovery.retrieval_options(str(options), os.getuid())
        options, password = self.options()
        password.chmod(0o644)
        with self.assertRaises(recovery.Refusal):
            recovery.retrieval_options(str(options), os.getuid())
        for snapshot in ("latest", "abc123", "a" * 64 + ":/", "../path"):
            with self.assertRaises(recovery.Refusal):
                recovery.retrieve(str(options), snapshot, self.fd, os.getuid())

    @unittest.skipUnless(sys.platform == "linux", "packaged subprocess uses Linux /proc fd paths")
    def test_subprocess_success_failure_timeout_and_environment_isolation(self):
        options, _ = self.options()
        archive = self.archive([("etc/machine-id", tarfile.REGTYPE, b"a" * 32)])
        executable = self.root / "fake-rustic"
        for outcome in ("success", "failure", "timeout"):
            executable.write_text(
                f"#!{sys.executable}\nimport os,sys,time\n"
                "assert 'RUSTIC_PASSWORD' not in os.environ\n"
                "assert 'RUSTIC_USE_PROFILE' not in os.environ\n"
                "assert 'SSH_AUTH_SOCK' not in os.environ\n"
                "assert open(os.environ['RUSTIC_PASSWORD_FILE']).read() == 'independent-password'\n"
                "assert sys.argv[1] == '--use-profile' and open(sys.argv[2]).read() == ''\n"
                "assert sys.argv[3:7] == ['--no-cache','dump','--archive','tar']\n"
                + ("time.sleep(30)\n" if outcome == "timeout" else f"sys.stdout.buffer.write({archive!r})\n")
                + ("sys.exit(1)\n" if outcome == "failure" else "")
            )
            executable.chmod(0o700)
            with patch.object(recovery, "RUSTIC", str(executable)), \
                    patch.object(recovery, "RETRIEVAL_SECONDS", 0.2 if outcome == "timeout" else 5), \
                    patch.dict(os.environ, {"RUSTIC_PASSWORD": "never-inherit", "RUSTIC_USE_PROFILE": "evil", "SSH_AUTH_SOCK": "socket"}):
                result = recovery.retrieve(str(options), "a" * 64, self.fd, os.getuid())
            if outcome == "success":
                self.assertEqual(result["status"], "staged")
                self.assertEqual(self.inspect(result["stage_id"])["retrieval"], "authenticated_repository")
            else:
                self.assertEqual(result["status"], "incomplete")
                with self.assertRaises(recovery.Refusal):
                    self.inspect(result["stage_id"])

    def snapshot(self, **fields):
        return {"id": "b" * 64, "time": "2026-10-16T20:34:42.123456789+02:00[Europe/Warsaw]",
                "hostname": "nasty", "paths": ["/var/lib/nasty", "/etc", "/var/lib/caddy",
                                               "/var/lib/systemd/credential.secret"], **fields}

    def test_snapshot_listing_reports_only_validated_fields_and_coverage(self):
        full = self.snapshot(tags=["secret-tag"], label="private-label", summary={"x": 1})
        partial = self.snapshot(id="c" * 64, paths=["/var/lib/nasty"])
        whole = self.snapshot(id="d" * 64, paths=["/"])
        hostile = self.snapshot(id="e" * 64, time="yesterday\n", hostname="evil\x1b[2J", paths=["relative"])
        for document in ([full, partial, whole, hostile],
                         [{"group_key": {"hostname": "nasty"}, "snapshots": [full, partial]},
                          {"group_key": {}, "snapshots": [whole, hostile]}]):
            listed = recovery.summarize_snapshots(document)
            self.assertEqual([set(s) for s in listed], [{"id", "time", "hostname", "missing_recovery_paths"}] * 4)
            self.assertEqual(listed[0]["time"], full["time"])
            self.assertEqual(listed[0]["missing_recovery_paths"], [])
            self.assertEqual(listed[1]["missing_recovery_paths"],
                             ["system_configuration", "tls_identity", "host_credential_key", "source_machine_identity"])
            self.assertEqual(listed[2]["missing_recovery_paths"], [])
            self.assertEqual((listed[3]["time"], listed[3]["hostname"]), (None, None))
            self.assertEqual(len(listed[3]["missing_recovery_paths"]), 5)
            self.assertNotIn("secret-tag", json.dumps(listed))
        # /etcetera is not an ancestor of /etc/nixos.
        self.assertIn("system_configuration",
                      recovery.summarize_snapshots([self.snapshot(paths=["/etcetera"])])[0]["missing_recovery_paths"])
        for document in ({}, [{"id": "B" * 64}], [{"id": "b" * 63}], [{"group_key": {}, "snapshots": {}}], ["x"]):
            with self.subTest(document=document), self.assertRaises(recovery.Refusal):
                recovery.summarize_snapshots(document)
        with patch.object(recovery, "MAX_SNAPSHOTS", 1), self.assertRaises(recovery.Refusal):
            recovery.summarize_snapshots([full, partial])

    @unittest.skipUnless(sys.platform == "linux", "packaged subprocess uses Linux /proc fd paths")
    def test_snapshot_listing_subprocess_isolation_failures_and_no_staging(self):
        options, _ = self.options()
        listing = json.dumps([self.snapshot()]).encode()
        executable = self.root / "fake-rustic"
        for outcome in ("success", "failure", "timeout", "oversize", "malformed"):
            output = {"oversize": b"[" + b" " * 64 + b"]", "malformed": b"[{"}.get(outcome, listing)
            executable.write_text(
                f"#!{sys.executable}\nimport os,sys,time\n"
                "assert 'RUSTIC_PASSWORD' not in os.environ\n"
                "assert 'SSH_AUTH_SOCK' not in os.environ\n"
                "assert open(os.environ['RUSTIC_PASSWORD_FILE']).read() == 'independent-password'\n"
                "assert sys.argv[1] == '--use-profile' and open(sys.argv[2]).read() == ''\n"
                "assert sys.argv[3:] == ['--no-cache','snapshots','--json']\n"
                + ("time.sleep(30)\n" if outcome == "timeout" else f"sys.stdout.buffer.write({output!r})\n")
                + ("sys.exit(1)\n" if outcome == "failure" else "")
            )
            executable.chmod(0o700)
            with patch.object(recovery, "RUSTIC", str(executable)), \
                    patch.object(recovery, "MAX_LISTING", 32 if outcome == "oversize" else 1024), \
                    patch.object(recovery, "RETRIEVAL_SECONDS", 0.2 if outcome == "timeout" else 5), \
                    patch.dict(os.environ, {"RUSTIC_PASSWORD": "never-inherit", "SSH_AUTH_SOCK": "socket"}):
                if outcome == "success":
                    result = recovery.snapshots(str(options), self.fd, os.getuid())
                    self.assertEqual([s["id"] for s in result["snapshots"]], ["b" * 64])
                else:
                    with self.subTest(outcome=outcome), self.assertRaises(recovery.Refusal):
                        recovery.snapshots(str(options), self.fd, os.getuid())
            # Listing never leaves a stage session or scratch directory behind.
            self.assertEqual(os.listdir(self.base), [])

if __name__ == "__main__":
    unittest.main()
