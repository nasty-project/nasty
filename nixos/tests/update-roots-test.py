"""Root inventory/deletion fixtures; never use the host's Nix store or state."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("update_roots", Path(__file__).parent.parent / "scripts/update-roots.py")
roots = importlib.util.module_from_spec(spec)
spec.loader.exec_module(roots)
OLD = "/nix/store/" + "a" * 32 + "-nixos-system-old"
CURRENT = "/nix/store/" + "b" * 32 + "-nixos-system-current"


class UpdateRootsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name) / "state"
        self.state.mkdir(mode=0o700)
        self.marker = Path(self.temp.name) / "recovery-marker"

    def tearDown(self):
        self.temp.cleanup()

    def link(self, name="update.a1B2c3", target=OLD):
        directory = self.state / name
        directory.mkdir(mode=0o700)
        (directory / "backup").mkdir()
        (directory / "backup/flake.lock").write_text("recovery evidence")
        link = directory / "result"
        link.symlink_to(target)
        return link

    def run_cleanup(self, active=lambda: False):
        return roots.remove_abandoned_roots(self.state, {CURRENT}, self.marker, active, os.getuid())

    def test_legacy_roots_removed_but_backups_and_protected_generations_preserved(self):
        old = self.link()
        current = self.link("update.abc123", CURRENT)
        self.assertEqual(self.run_cleanup(), 1)
        self.assertFalse(old.is_symlink())
        self.assertTrue(current.is_symlink())
        self.assertEqual((old.parent / "backup/flake.lock").read_text(), "recovery evidence")
        self.assertEqual(self.run_cleanup(), 0)

    def test_active_legacy_update_or_rollback_refuses_before_unlink(self):
        link = self.link()
        with self.assertRaises(RuntimeError):
            self.run_cleanup(lambda: True)
        self.assertTrue(link.is_symlink())

    def test_incomplete_rollback_marker_preserves_roots(self):
        link = self.link()
        self.marker.touch()
        with self.assertRaises(RuntimeError):
            self.run_cleanup()
        self.assertTrue(link.is_symlink())
        self.marker.unlink()
        self.marker.symlink_to("missing")
        with self.assertRaises(RuntimeError):
            self.run_cleanup()

    def test_changing_activity_refuses_entire_inventory(self):
        link = self.link()
        states = iter([False, True])
        with self.assertRaises(RuntimeError):
            self.run_cleanup(lambda: next(states))
        self.assertTrue(link.is_symlink())

    def test_untrusted_directories_paths_and_non_system_results_are_untouched(self):
        unsafe = self.link()
        unsafe.parent.chmod(0o777)
        unrelated = self.link("manual-result")
        foreign = self.link("update.123456", "/nix/store/" + "c" * 32 + "-user-data")
        self.state.joinpath("update.symlnk").symlink_to(unsafe.parent, target_is_directory=True)
        self.assertEqual(self.run_cleanup(), 0)
        for link in (unsafe, unrelated, foreign):
            self.assertTrue(link.is_symlink())

    def test_activity_query_failure_fails_closed(self):
        link = self.link()
        def failed():
            raise OSError("systemd unavailable")
        with self.assertRaises(OSError):
            self.run_cleanup(failed)
        self.assertTrue(link.is_symlink())


if __name__ == "__main__":
    unittest.main()
