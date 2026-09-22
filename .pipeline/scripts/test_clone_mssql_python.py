# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Offline checkout tests using a real local Git remote, not a mocked fetch."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HERE = Path(__file__).resolve().parent


class RevisionCheckout(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.remote = self.root / "upstream"
        self.remote.mkdir()
        self.env = {
            **os.environ,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(self.root / "gitconfig"),
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_AUTHOR_NAME": "Checkout test",
            "GIT_AUTHOR_EMAIL": "checkout@example.invalid",
            "GIT_COMMITTER_NAME": "Checkout test",
            "GIT_COMMITTER_EMAIL": "checkout@example.invalid",
            "BUILD_REPOSITORY_PROVIDER": "TfsGit",
            "SYSTEM_PULLREQUEST_PULLREQUESTID": "123",
            "MSSQL_PYTHON_BRANCH": "main",
        }
        self.git(self.root, "config", "--global",
                 f"url.{self.remote.as_uri()}.insteadOf",
                 "https://github.com/microsoft/mssql-python.git")
        self.git(self.remote, "init", "--quiet", "--initial-branch=main")
        self.pin = self.commit("approved")
        self.newer = self.commit("newer")
        pipeline = self.root / ".pipeline"
        (pipeline / "scripts").mkdir(parents=True)
        self.pin_file = pipeline / "mssql-python-revision.txt"
        self.pin_file.write_text(self.pin + "\n", encoding="ascii")
        self.script = pipeline / "scripts" / "clone-mssql-python.sh"
        self.script.write_text(
            (HERE / "clone-mssql-python.sh").read_text(encoding="utf-8"),
            encoding="utf-8", newline="\n",
        )

    def git(self, cwd, *args):
        return subprocess.run(
            ["git", *args], cwd=cwd, env=self.env, check=True,
            capture_output=True, text=True,
        ).stdout.strip()

    def commit(self, content):
        (self.remote / "content.txt").write_text(content, encoding="ascii")
        self.git(self.remote, "add", "content.txt")
        self.git(self.remote, "commit", "--quiet", "-m", content)
        return self.git(self.remote, "rev-parse", "HEAD")

    def checkout(self, destination="checkout"):
        return subprocess.run(
            ["bash", ".pipeline/scripts/clone-mssql-python.sh"],
            cwd=self.root,
            env={**self.env, "MSSQL_PYTHON_CLONE_DIR": destination},
            capture_output=True, text=True,
        )

    def assert_pinned(self, destination):
        result = self.checkout(destination)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        checkout = self.root / destination
        self.assertEqual(self.git(checkout, "rev-parse", "HEAD"), self.pin)
        self.assertEqual(self.git(checkout, "rev-parse", "--abbrev-ref", "HEAD"), "HEAD")
        self.assertEqual(self.git(checkout, "rev-list", "--count", "HEAD"), "1")
        self.assertEqual((checkout / "content.txt").read_text(), "approved")
        self.assertIn(f"requested revision: {self.pin}", result.stdout)
        self.assertIn(f"mssql-python HEAD: {self.pin}", result.stdout)
        self.assertNotIn("##[error]", result.stdout + result.stderr)

    def test_exact_old_commit_is_repeatable_after_main_moves(self):
        self.assertNotEqual(self.pin, self.newer)
        self.assert_pinned("first checkout")
        latest = self.commit("main advanced again")
        self.assertNotEqual(latest, self.newer)
        self.assertEqual(self.git(self.remote, "rev-parse", "main"), latest)
        self.assert_pinned("second checkout")

    def test_main_tracks_latest_commit_on_each_checkout(self):
        self.pin_file.write_text("main\n", encoding="ascii")
        for destination in ("first checkout", "second checkout"):
            latest = self.commit(destination)
            result = self.checkout(destination)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            checkout = self.root / destination
            self.assertEqual(self.git(checkout, "rev-parse", "HEAD"), latest)
            self.assertEqual(self.git(checkout, "rev-parse", "--abbrev-ref", "HEAD"), "HEAD")
            self.assertEqual(self.git(checkout, "rev-list", "--count", "HEAD"), "1")
            self.assertEqual((checkout / "content.txt").read_text(), destination)
            self.assertIn("requested revision: main", result.stdout)
            self.assertIn(f"mssql-python HEAD: {latest}", result.stdout)

    def test_repository_defaults_to_main(self):
        self.assertEqual(
            (HERE.parent / "mssql-python-revision.txt").read_text().strip(), "main"
        )

    def test_missing_main_never_falls_back_to_another_branch(self):
        self.pin_file.write_text("main\n", encoding="ascii")
        self.git(self.remote, "branch", "-m", "different-default")
        result = self.checkout()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cannot fetch mssql-python revision main", result.stderr)
        self.assertFalse((self.root / "checkout" / "content.txt").exists())

    def test_existing_checkout_is_not_modified(self):
        self.assert_pinned("checkout")
        marker = self.root / "checkout" / "local-work.txt"
        marker.write_text("keep", encoding="ascii")
        result = self.checkout()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(marker.read_text(), "keep")
        self.assertEqual(self.git(marker.parent, "rev-parse", "HEAD"), self.pin)

    def test_missing_pin_fails_before_creating_destination(self):
        self.pin_file.unlink()
        result = self.checkout()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Missing mssql-python revision", result.stderr)
        self.assertFalse((self.root / "checkout").exists())

    def test_invalid_pin_fails_before_creating_destination(self):
        for pin in ("", "develop", self.pin[:7], "g" * 40, self.pin.upper(),
                    f"{self.pin}\n{self.newer}", f" {self.pin}"):
            with self.subTest(pin=pin):
                self.pin_file.write_text(pin, encoding="ascii")
                result = self.checkout()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("40-character commit SHA", result.stderr)
                self.assertFalse((self.root / "checkout").exists())

    def test_unavailable_commit_never_falls_back_to_main(self):
        self.pin_file.write_text("0" * 40 + "\n", encoding="ascii")
        result = self.checkout()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cannot fetch mssql-python revision", result.stderr)
        self.assertFalse((self.root / "checkout" / "content.txt").exists())

    def test_unavailable_remote_fails_without_fallback(self):
        self.remote.rename(self.root / "unavailable-upstream")
        result = self.checkout()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cannot fetch mssql-python revision", result.stderr)
        self.assertFalse((self.root / "checkout" / "content.txt").exists())


if __name__ == "__main__":
    unittest.main()
