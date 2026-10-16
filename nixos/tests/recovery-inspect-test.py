"""Adversarial staging fixtures; no host recovery state, services or pools."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
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


if __name__ == "__main__":
    unittest.main()
