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

    def test_ordering_a_published_prerelease_fails_cleanly(self):
        # A hand-published prerelease on the feed used to reach int() and give
        # a ValueError traceback rather than something anyone could act on.
        state = {
            "x86_64": {"version": "1.0.0-rc1", "current": False, "layout_stale": False},
            "arm64": {"version": "0.1.0", "current": False, "layout_stale": False},
        }
        with self.assertRaises(RuntimeError):
            resolve.decide(state)


class Organization(unittest.TestCase):
    def test_the_modern_collection_uri(self):
        self.assertEqual(resolve.organization("https://dev.azure.com/sqlclientdrivers/"), "sqlclientdrivers")

    def test_the_legacy_collection_uri(self):
        # This organization is one of these: taking the last path segment gives
        # the hostname, and the feed URL built from it 404s.
        self.assertEqual(
            resolve.organization("https://sqlclientdrivers.visualstudio.com/"),
            "sqlclientdrivers",
        )

    def test_a_missing_trailing_slash_is_fine(self):
        for uri in ("https://dev.azure.com/sqlclientdrivers",
                    "https://sqlclientdrivers.visualstudio.com"):
            self.assertEqual(resolve.organization(uri), "sqlclientdrivers", uri)

    def test_a_bare_organization_name_is_accepted(self):
        self.assertEqual(resolve.organization("sqlclientdrivers"), "sqlclientdrivers")

    def test_something_with_no_organization_in_it_is_an_error(self):
        with self.assertRaises(RuntimeError):
            resolve.organization("https://dev.azure.com/")


class Decision(unittest.TestCase):
    """Which architectures publish, and at what version."""

    def arch(self, version, current=True, layout_stale=False):
        return {"version": version, "current": current, "layout_stale": layout_stale}

    def both(self, **kwargs):
        return {"x86_64": self.arch(**kwargs), "arm64": self.arch(**kwargs)}

    def test_an_unchanged_feed_publishes_nothing(self):
        _, publish, reasons = resolve.decide(self.both(version="0.1.0"))
        self.assertEqual(publish, set())
        self.assertEqual(reasons, [])

    def test_an_upstream_bump_moves_every_architecture_together(self):
        version, publish, _ = resolve.decide(self.both(version="0.1.0", current=False))
        self.assertEqual(version, "0.1.1")
        self.assertEqual(publish, {"x86_64", "arm64"})

    def test_one_architecture_drifting_still_republishes_both(self):
        # A version has to mean the same build produced both, so the unchanged
        # architecture comes along rather than being left a version behind.
        state = {"x86_64": self.arch("0.1.0", current=False), "arm64": self.arch("0.1.0")}
        version, publish, _ = resolve.decide(state)
        self.assertEqual(version, "0.1.1")
        self.assertEqual(publish, {"x86_64", "arm64"})

    def test_a_layout_change_is_the_minor_bump(self):
        version, _, _ = resolve.decide(self.both(version="0.1.3", current=False, layout_stale=True))
        self.assertEqual(version, "0.2.0")

    def test_a_partial_publish_is_repaired_at_the_version_it_missed(self):
        # x86_64 uploaded 0.1.1 and arm64's upload failed. Bumping to 0.1.2
        # would strand 0.1.1 as x86_64-only forever, since packages are
        # immutable and a consumer pinning it would 404 on arm64.
        state = {
            "x86_64": self.arch("0.1.1"),
            "arm64": self.arch("0.1.0", current=False),
        }
        version, publish, reasons = resolve.decide(state)
        self.assertEqual(version, "0.1.1")
        self.assertEqual(publish, {"arm64"})
        self.assertIn("partial publish", reasons[0])

    def test_a_partial_FIRST_publish_is_repaired_too(self):
        # The laggard has no version at all rather than an older one. Treating
        # only "behind" as lagging left this to the normal path, which bumped
        # to 0.2.0 and stranded 0.1.0 as x86_64-only.
        state = {
            "x86_64": self.arch("0.1.0"),
            "arm64": self.arch(None, current=False, layout_stale=True),
        }
        version, publish, reasons = resolve.decide(state)
        self.assertEqual(version, "0.1.0")
        self.assertEqual(publish, {"arm64"})
        self.assertIn("partial publish", reasons[0])

    def test_a_laggard_is_not_dragged_onto_a_version_that_predates_upstream(self):
        # Both are behind upstream, so 0.1.1 is not what should ship; that is a
        # normal bump for both, not a repair.
        state = {
            "x86_64": self.arch("0.1.1", current=False),
            "arm64": self.arch("0.1.0", current=False),
        }
        version, publish, _ = resolve.decide(state)
        self.assertEqual(version, "0.1.2")
        self.assertEqual(publish, {"x86_64", "arm64"})

    def test_an_explicit_override_wins_over_a_repair(self):
        state = {
            "x86_64": self.arch("0.1.1"),
            "arm64": self.arch("0.1.0", current=False),
        }
        version, publish, _ = resolve.decide(state, override="2.0.0")
        self.assertEqual(version, "2.0.0")
        self.assertEqual(publish, {"x86_64", "arm64"})

    def test_a_first_publish_covers_every_architecture(self):
        version, publish, _ = resolve.decide(self.both(version=None, current=False, layout_stale=True))
        self.assertEqual(version, "0.1.0")
        self.assertEqual(publish, {"x86_64", "arm64"})

    def test_forcing_republishes_everything_unchanged(self):
        version, publish, reasons = resolve.decide(self.both(version="0.1.0"), force=True)
        self.assertEqual(version, "0.1.1")
        self.assertEqual(publish, {"x86_64", "arm64"})
        self.assertIn("forced", reasons[0])

    def test_an_explicit_version_publishes_even_when_nothing_changed(self):
        # Asking for a specific number and getting a run that publishes nothing
        # is a silent no-op; the request is itself the reason to publish.
        version, publish, reasons = resolve.decide(self.both(version="0.1.0"), override="1.0.0")
        self.assertEqual(version, "1.0.0")
        self.assertEqual(publish, {"x86_64", "arm64"})
        self.assertIn("1.0.0", reasons[0])


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
