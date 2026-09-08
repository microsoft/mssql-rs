#!/usr/bin/env python3
"""Tests for the toolchain publish/version decision.

Run: python3 -m unittest test_resolve_toolchain_version
"""

from __future__ import annotations

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(
        os.path.splitext(name)[0].replace("-", "_"), os.path.join(HERE, name)
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


resolve = load("resolve-toolchain-version.py")
builder = load("build-macos-docker-toolchain.py")


def components(docker="29.7.2-1", colima="0.10.3", lima="2.2.0", digest="sha256:aa"):
    return {
        "docker": {"version": docker, "bottle_digest": digest},
        "colima": {"version": colima, "bottle_digest": digest},
        "lima": {"version": lima, "bottle_digest": digest},
    }


class Identity(unittest.TestCase):
    def test_same_inputs_give_the_same_digest(self):
        self.assertEqual(
            builder.payload_identity(components(), 14),
            builder.payload_identity(components(), 14),
        )

    def test_an_upstream_bump_changes_it(self):
        self.assertNotEqual(
            builder.payload_identity(components(), 14),
            builder.payload_identity(components(docker="29.8.0"), 14),
        )

    def test_a_rebuilt_bottle_changes_it_even_at_the_same_version(self):
        # Homebrew can republish a version; the digest is what actually ships.
        self.assertNotEqual(
            builder.payload_identity(components(digest="sha256:aa"), 14),
            builder.payload_identity(components(digest="sha256:bb"), 14),
        )

    def test_the_macos_floor_changes_it(self):
        self.assertNotEqual(
            builder.payload_identity(components(), 14),
            builder.payload_identity(components(), 15),
        )

    def test_the_payload_layout_changes_it(self):
        # Without this the metadata/ move would have looked like a no-op and
        # the scheduled run would have skipped publishing the fix.
        self.assertNotEqual(
            builder.payload_identity(components(), 14, payload_format=1),
            builder.payload_identity(components(), 14, payload_format=2),
        )

    def test_extra_manifest_fields_are_ignored(self):
        # resolve-toolchain-version.py computes this from registry metadata and
        # has no file counts; the producer must agree with it anyway.
        enriched = components()
        for entry in enriched.values():
            entry["files"] = 42
            entry["bottle_tag"] = "29.7.2.sonoma.1"
        self.assertEqual(
            builder.payload_identity(components(), 14),
            builder.payload_identity(enriched, 14),
        )


class PublishedDescription(unittest.TestCase):
    def parse(self, description):
        return resolve.describe_published({"version": "1.2.3", "packageDescription": description})

    def manifest(self, **overrides):
        m = {
            "arch": "x86_64",
            "macos_major_floor": 14,
            "payload_format": builder.PAYLOAD_FORMAT,
            "identity": "0c9acc130659bfcb",
            "components": components(),
        }
        m.update(overrides)
        return m

    def test_publishing_and_reading_back_agree(self):
        # The whole skip-if-unchanged scheme rests on this round-trip: publish
        # writes the description, the next run parses it. Drift between the two
        # would silently republish forever.
        m = self.manifest()
        _, layout, identity = self.parse(resolve.format_description(m))
        self.assertEqual(layout, m["payload_format"])
        self.assertEqual(identity, m["identity"])

    def test_the_description_leads_with_something_a_human_can_read(self):
        described = resolve.format_description(self.manifest())
        self.assertTrue(described.startswith("docker 29.7.2-1, colima 0.10.3, lima 2.2.0"))
        self.assertIn("for macOS 14+ x86_64", described)

    def test_markers_are_read_back_out(self):
        version, layout, identity = self.parse(
            "docker 29.7.2-1, colima 0.10.3, lima 2.2.0 for macOS 14+ x86_64 "
            "[layout:2 id:0c9acc130659bfcb]"
        )
        self.assertEqual((version, layout, identity), ("1.2.3", 2, "0c9acc130659bfcb"))

    def test_a_package_published_before_the_markers_existed(self):
        # 0.0.1 shipped with a prose-only description.
        _, layout, identity = self.parse("docker CLI, colima, lima and colima guest image")
        self.assertIsNone(layout)
        self.assertIsNone(identity)

    def test_a_missing_package_is_not_an_error(self):
        self.assertEqual(resolve.describe_published(None), (None, None, None))

    def test_a_missing_description_is_not_an_error(self):
        self.assertEqual(self.parse(None), ("1.2.3", None, None))


class NextVersion(unittest.TestCase):
    def test_an_upstream_bump_moves_the_patch(self):
        self.assertEqual(resolve.next_version("0.1.0", layout_changed=False), "0.1.1")

    def test_a_layout_change_moves_the_minor_and_resets_the_patch(self):
        self.assertEqual(resolve.next_version("0.1.7", layout_changed=True), "0.2.0")

    def test_the_first_publish_starts_at_a_minor(self):
        self.assertEqual(resolve.next_version(None, layout_changed=True), "0.1.0")

    def test_a_version_it_cannot_reason_about_is_an_error(self):
        with self.assertRaises(RuntimeError):
            resolve.next_version("1.0.0-beta", layout_changed=False)


class RequestedVersion(unittest.TestCase):
    def test_the_sentinel_means_derive_it(self):
        # The pipeline passes 'auto' rather than '' because the run panel
        # refuses to queue with a blank string parameter.
        self.assertIsNone(resolve.requested_version("auto"))

    def test_the_sentinel_is_not_case_or_space_sensitive(self):
        for raw in (" auto ", "AUTO", "Auto"):
            self.assertIsNone(resolve.requested_version(raw), raw)

    def test_an_empty_value_still_means_derive_it(self):
        for raw in ("", "   ", None):
            self.assertIsNone(resolve.requested_version(raw), repr(raw))

    def test_an_explicit_version_is_taken_as_given(self):
        self.assertEqual(resolve.requested_version(" 1.0.0 "), "1.0.0")

    def test_a_typo_is_refused_rather_than_published(self):
        # Packages are immutable, so a bad override burns the number.
        for raw in ("1.0", "v1.0.0", "1.0.0-rc1", "latest"):
            with self.assertRaises(RuntimeError, msg=raw):
                resolve.requested_version(raw)


if __name__ == "__main__":
    unittest.main()
