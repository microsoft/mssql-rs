# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Offline tests for the Homebrew bottle installer's selection rules.

This script only runs when a macOS agent has no bottle for the current formula
version, so a mistake in it stays invisible until CI is already degraded. The
parts worth testing are the ones that silently pick the wrong artifact rather
than crashing: Homebrew spelling a revision as `29.7.2-1` in the registry tag
but `29.7.2.<platform>.1` in the ref names inside it, a bottle newer than the
running macOS being accepted, an arm64 bottle being taken on Intel, and a tar
member escaping the formula's bin/ directory.

Everything here is synthetic - no network and no macOS required.

Run with ``python -m unittest discover .pipeline/scripts``.
"""

from __future__ import annotations

import importlib.util
import io
import os
import tarfile
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
INSTALLER = REPO_ROOT / ".pipeline" / "scripts" / "install-brew-bottle.py"


def _load():
    spec = importlib.util.spec_from_file_location("install_brew_bottle", INSTALLER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


bottle = _load()

SEQUOIA = bottle.MACOS_CODENAMES.index("sequoia")

# Ref names as ghcr.io actually annotates them, captured from
# homebrew/core/docker. The revision index is the case the tag itself cannot be
# matched against.
PLAIN_REFS = [
    "29.7.2.arm64_linux", "29.7.2.arm64_sequoia", "29.7.2.arm64_sonoma",
    "29.7.2.arm64_tahoe", "29.7.2.sonoma", "29.7.2.x86_64_linux",
]
REVISION_REFS = [
    "29.7.2.arm64_linux.1", "29.7.2.arm64_sequoia.1", "29.7.2.arm64_sonoma.1",
    "29.7.2.arm64_tahoe.1", "29.7.2.sonoma.1", "29.7.2.x86_64_linux.1",
]


def best_ref(refs, tag, prefix, max_rank):
    """Mirror of find_bottle's inner selection, minus the registry."""
    base, revision = bottle.ref_parts(tag)
    ranked = [
        (bottle.usable_tag_rank(ref, base, revision, prefix, max_rank), ref)
        for ref in refs
    ]
    usable = [(rank, ref) for rank, ref in ranked if rank is not None]
    return max(usable)[1] if usable else None


class VersionOrdering(unittest.TestCase):
    def test_revision_sorts_above_its_base(self):
        self.assertGreater(bottle.version_key("29.7.2-1"), bottle.version_key("29.7.2"))
        self.assertGreater(bottle.version_key("29.8.0"), bottle.version_key("29.7.2-1"))

    def test_non_version_tags_are_rejected(self):
        for tag in ("latest", "29.7", "29.7.2-beta"):
            self.assertIsNone(bottle.version_key(tag), tag)

    def test_ref_parts_splits_the_revision_off_the_tag(self):
        self.assertEqual(bottle.ref_parts("29.7.2-1"), ("29.7.2", "1"))
        self.assertEqual(bottle.ref_parts("29.7.2"), ("29.7.2", None))


class BottleSelection(unittest.TestCase):
    def test_revision_index_is_matched_by_its_base_and_revision(self):
        # The tag is `29.7.2-1` but the refs read `29.7.2.sonoma.1`; matching the
        # tag against the ref name directly would discard the whole version.
        self.assertEqual(best_ref(REVISION_REFS, "29.7.2-1", "", SEQUOIA), "29.7.2.sonoma.1")

    def test_revision_refs_are_not_accepted_for_the_unrevised_tag(self):
        self.assertIsNone(best_ref(REVISION_REFS, "29.7.2", "", SEQUOIA))

    def test_intel_never_takes_an_arm64_bottle(self):
        self.assertEqual(best_ref(PLAIN_REFS, "29.7.2", "", SEQUOIA), "29.7.2.sonoma")
        arm_only = ["29.8.0.arm64_sequoia", "29.8.0.arm64_tahoe", "29.8.0.x86_64_linux"]
        self.assertIsNone(best_ref(arm_only, "29.8.0", "", SEQUOIA))

    def test_arm64_prefers_the_newest_compatible_codename(self):
        self.assertEqual(
            best_ref(PLAIN_REFS, "29.7.2", "arm64_", SEQUOIA), "29.7.2.arm64_sequoia"
        )

    def test_bottle_newer_than_the_host_is_rejected(self):
        sonoma = bottle.MACOS_CODENAMES.index("sonoma")
        self.assertEqual(best_ref(PLAIN_REFS, "29.7.2", "arm64_", sonoma), "29.7.2.arm64_sonoma")
        tahoe_only = ["29.7.2.arm64_tahoe"]
        self.assertIsNone(best_ref(tahoe_only, "29.7.2", "arm64_", sonoma))

    def test_linux_bottles_are_never_selected(self):
        linux_only = ["29.7.2.arm64_linux", "29.7.2.x86_64_linux"]
        for prefix in ("", "arm64_"):
            self.assertIsNone(best_ref(linux_only, "29.7.2", prefix, SEQUOIA), prefix)

    def test_all_platform_bottles_are_not_selected(self):
        # `:all` bottles carry a `.all` ref. Out of scope for the CLIs this
        # installs, and taking one blindly is worse than reporting no match.
        self.assertIsNone(best_ref(["1.2.3.all"], "1.2.3", "", SEQUOIA))


class HostRanking(unittest.TestCase):
    def setUp(self):
        self.addCleanup(os.environ.pop, "BOTTLE_MACOS_MAJOR_OVERRIDE", None)

    def rank_for(self, major):
        os.environ["BOTTLE_MACOS_MAJOR_OVERRIDE"] = major
        return bottle.current_os_rank()

    def test_known_majors_map_to_their_codename(self):
        self.assertEqual(self.rank_for("15"), bottle.MACOS_CODENAMES.index("sequoia"))
        self.assertEqual(self.rank_for("11"), bottle.MACOS_CODENAMES.index("big_sur"))

    def test_major_above_the_newest_known_accepts_anything(self):
        self.assertEqual(self.rank_for("27"), len(bottle.MACOS_CODENAMES) - 1)

    def test_unmapped_major_below_the_newest_is_an_error(self):
        # Falling forward here would hand a macOS 26 bottle to an older host.
        with self.assertRaises(RuntimeError):
            self.rank_for("16")

    def test_unparsable_version_is_an_error(self):
        with self.assertRaises(RuntimeError):
            self.rank_for("not-a-version")


def make_bottle(entries):
    """A gzipped tar of `(name, is_file)` members, as the registry serves them."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for name, is_file in entries:
            if is_file:
                payload = b"#!/bin/sh\n"
                info = tarfile.TarInfo(name)
                info.size = len(payload)
                tar.addfile(info, io.BytesIO(payload))
            else:
                info = tarfile.TarInfo(name)
                info.type = tarfile.SYMTYPE
                info.linkname = "../elsewhere"
                tar.addfile(info)
    return buf.getvalue()


class Extraction(unittest.TestCase):
    def extract(self, entries, version="29.7.2"):
        dest = tempfile.mkdtemp()
        installed = bottle.extract_bin(make_bottle(entries), "docker", version, dest)
        return installed, sorted(os.listdir(dest))

    def test_only_regular_files_directly_under_bin_are_installed(self):
        installed, on_disk = self.extract([
            ("docker/29.7.2/bin/docker", True),
            ("docker/29.7.2/bin/nested/helper", True),
            ("docker/29.7.2/README.md", True),
            ("docker/29.7.2/libexec/internal", True),
            ("docker/29.7.2/bin/link", False),
        ])
        self.assertEqual(installed, ["docker"])
        self.assertEqual(on_disk, ["docker"])

    def test_revision_tag_reads_the_unrevised_cellar_directory(self):
        # Homebrew keeps the bottle root at the plain version even for a revision.
        installed, _ = self.extract([("docker/29.7.2/bin/docker", True)], version="29.7.2-1")
        self.assertEqual(installed, ["docker"])

    def test_traversal_outside_the_formula_is_refused(self):
        with self.assertRaises(RuntimeError):
            self.extract([("docker/29.7.2/bin/../../../../tmp/evil", True)])

    def test_a_bottle_without_bin_entries_is_an_error(self):
        with self.assertRaises(RuntimeError):
            self.extract([("docker/29.7.2/README.md", True)])


if __name__ == "__main__":
    unittest.main()
