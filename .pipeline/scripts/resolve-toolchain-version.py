#!/usr/bin/env python3
"""Decide whether the macOS docker toolchain needs republishing, and at what version.

The payload is a derived artifact: its bytes are a function of the docker,
colima and lima bottles it repacks, the macOS floor those bottles are selected
for, and the layout this repo packs them into. None of that is a judgement
call, so neither is the version -- this resolves that function and compares the
result with what is already on the feed.

Nothing is downloaded to do it. Bottle versions and digests come from registry
metadata, and the published side is read back out of the package description,
where publishing records it as "[layout:N id:...]". So the decision costs a
handful of HTTP round-trips rather than the 430 MB the payload weighs.

Publishes only when something actually changed. A scheduled run that finds
upstream unmoved exits without publishing, which keeps the version history a
record of upstream movement instead of a record of the schedule firing.

Version:
  layout unchanged  patch bump   (an upstream docker/colima/lima release)
  layout changed    minor bump   (consumers resolve paths into the tree)
  no package yet    0.1.0
An explicit --version X.Y.Z overrides all of it; 'auto' derives it.

Emits Azure Pipelines output variables: shouldPublish, packageVersion, and
identity_<arch> for the publish step to record.

Usage: resolve-toolchain-version.py --org <url> --feed <project/feed> \
           --arch x86_64 --arch arm64 --macos-major 14 [--version X.Y.Z] [--force]
Reads SYSTEM_ACCESSTOKEN from the environment.
"""

import argparse
import importlib.util
import json
import os
import re
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
PACKAGE_PREFIX = "macos-docker-toolchain-"
API_VERSION = "7.1-preview.1"
# The pipeline passes this rather than an empty string: the run panel refuses to
# queue with a blank string parameter.
DERIVE = "auto"


def load(name):
    path = os.path.join(HERE, name)
    spec = importlib.util.spec_from_file_location(
        os.path.splitext(name)[0].replace("-", "_"), path
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def resolve_components(bottle, arch, macos_major):
    """Bottle versions and digests for one architecture, without fetching blobs."""
    os.environ["BOTTLE_ARCH_OVERRIDE"] = arch
    os.environ["BOTTLE_MACOS_MAJOR_OVERRIDE"] = str(macos_major)
    components = {}
    for formula in ("docker", "colima", "lima"):
        token = bottle.anonymous_token(formula)
        version, manifest_digest, _ = bottle.find_bottle(formula, token)
        components[formula] = {"version": version, "bottle_digest": manifest_digest}
    return components


def published_packages(org_url, project, feed, token):
    org = org_url.rstrip("/").rsplit("/", 1)[-1]
    url = (
        f"https://feeds.dev.azure.com/{org}/{project}/_apis/packaging/Feeds/{feed}"
        f"/packages?protocolType=upack&includeDescription=true&api-version={API_VERSION}"
    )
    request = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            body = json.load(response)
    except urllib.error.HTTPError as exc:
        raise RuntimeError(f"listing feed {project}/{feed} failed: {exc.code} {exc.reason}") from None
    except urllib.error.URLError as exc:
        raise RuntimeError(f"listing feed {project}/{feed} failed: {exc}") from None

    published = {}
    for package in body.get("value", []):
        versions = package.get("versions") or []
        if versions:
            published[package["name"]] = versions[0]
    return published


def describe_published(entry):
    """(version, layout, identity) for a published package; layout/identity may be None."""
    if entry is None:
        return None, None, None
    description = entry.get("packageDescription") or ""
    layout = re.search(r"\blayout:(\d+)\b", description)
    identity = re.search(r"\bid:([0-9a-f]+)\b", description)
    return (
        entry.get("version"),
        int(layout.group(1)) if layout else None,
        identity.group(1) if identity else None,
    )


def format_description(manifest):
    """The published description, which the next run parses back with describe_published.

    Prose first so the feed UI says something useful, then the machine-readable
    markers. Changing this format without changing the parser above strands
    every published package as "unknown", forcing a needless republish.
    """
    versions = ", ".join(
        f"{name} {manifest['components'][name]['version']}"
        for name in ("docker", "colima", "lima")
    )
    return (
        f"{versions} for macOS {manifest['macos_major_floor']}+ {manifest['arch']} "
        f"[layout:{manifest['payload_format']} id:{manifest['identity']}]"
    )


def requested_version(raw):
    """The explicit override, or None to derive one.

    Checked before the comparison runs so a typo fails immediately rather than
    after the network work -- and, since packages are immutable, before it can
    burn a version number.
    """
    value = (raw or "").strip()
    if not value or value.lower() == DERIVE:
        return None
    if not re.match(r"^\d+\.\d+\.\d+$", value):
        raise RuntimeError(
            f"version override {value!r} is not X.Y.Z; use '{DERIVE}' to derive it"
        )
    return value


def next_version(latest, layout_changed):
    if latest is None:
        return "0.1.0"
    parsed = re.match(r"^(\d+)\.(\d+)\.(\d+)$", latest)
    if not parsed:
        raise RuntimeError(
            f"published version {latest!r} is not X.Y.Z, so the next one cannot be "
            f"derived; pass an explicit --version"
        )
    major, minor, patch = (int(part) for part in parsed.groups())
    if layout_changed:
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def emit(name, value):
    print(f"##vso[task.setvariable variable={name};isOutput=true]{value}")


def describe_payload(manifest_path, expect_identity):
    with open(manifest_path) as handle:
        manifest = json.load(handle)
    # The build resolved upstream independently of the decision. If they
    # disagree, a release landed between the two jobs and the description
    # would misdescribe what is in the package.
    if expect_identity and manifest["identity"] != expect_identity:
        raise RuntimeError(
            f"payload identity {manifest['identity']} does not match the resolved "
            f"{expect_identity}; upstream moved mid-run"
        )
    description = format_description(manifest)
    print(f"description: {description}")
    print(f"##vso[task.setvariable variable=packageDescription]{description}")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--org", help="e.g. https://dev.azure.com/sqlclientdrivers")
    parser.add_argument("--feed", help="<project>/<feed>")
    parser.add_argument("--arch", action="append", choices=["x86_64", "arm64"])
    parser.add_argument("--macos-major", type=int)
    parser.add_argument(
        "--version",
        default=DERIVE,
        help=f"X.Y.Z to force a number, or '{DERIVE}' (default) to derive it",
    )
    parser.add_argument("--force", action="store_true", help="publish even if nothing changed")
    parser.add_argument(
        "--describe",
        metavar="MANIFEST",
        help="emit the publish description for a built payload and exit",
    )
    parser.add_argument(
        "--expect-identity",
        help="with --describe, fail unless the built payload has this identity",
    )
    args = parser.parse_args()

    if args.describe:
        return describe_payload(args.describe, args.expect_identity)

    missing = [
        flag
        for flag, value in (("--org", args.org), ("--feed", args.feed),
                            ("--arch", args.arch), ("--macos-major", args.macos_major))
        if not value
    ]
    if missing:
        raise RuntimeError(f"missing required arguments: {', '.join(missing)}")

    if "/" not in args.feed:
        raise RuntimeError(f"--feed must be <project>/<feed>, got {args.feed!r}")
    project, feed = args.feed.split("/", 1)

    override = requested_version(args.version)

    token = os.environ.get("SYSTEM_ACCESSTOKEN", "")
    if not token:
        raise RuntimeError("SYSTEM_ACCESSTOKEN is empty; the decide step needs it to read the feed")

    builder = load("build-macos-docker-toolchain.py")
    bottle = load("install-brew-bottle.py")
    published = published_packages(args.org, project, feed, token)

    changed = []
    latest_versions = []
    layout_changed = False

    for arch in args.arch:
        package = PACKAGE_PREFIX + arch
        components = resolve_components(bottle, arch, args.macos_major)
        identity = builder.payload_identity(components, args.macos_major)
        emit(f"identity_{arch}", identity)

        version, layout, published_identity = describe_published(published.get(package))
        summary = ", ".join(f"{n} {components[n]['version']}" for n in sorted(components))
        print(f"\n{package}")
        print(f"  upstream now : {summary} (layout {builder.PAYLOAD_FORMAT}, id {identity})")

        if version is None:
            print("  published    : nothing yet")
            changed.append(f"{arch}: not published yet")
            layout_changed = True
            continue

        print(f"  published    : {version} (layout {layout}, id {published_identity})")
        latest_versions.append(version)
        if layout != builder.PAYLOAD_FORMAT:
            layout_changed = True
            changed.append(f"{arch}: payload layout {layout} -> {builder.PAYLOAD_FORMAT}")
        if published_identity != identity:
            changed.append(f"{arch}: contents differ from {version}")

    # One decision for every architecture, so a given version means the same
    # build produced all of them -- worth more than skipping the odd republish.
    should_publish = bool(changed) or args.force
    version = override or next_version(
        max(latest_versions, default=None, key=lambda v: [int(p) for p in v.split(".")]),
        layout_changed,
    )

    print("\n" + "=" * 62)
    if changed:
        print("republishing because:")
        for reason in changed:
            print(f"  - {reason}")
    elif args.force:
        print("nothing changed upstream, but a publish was forced")
    else:
        print("nothing changed upstream; leaving the feed alone")
    if should_publish:
        print(f"version: {version}" + (" (explicit override)" if override else ""))

    emit("shouldPublish", "true" if should_publish else "false")
    emit("packageVersion", version)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as exc:
        print(f"##[error]{exc}")
        sys.exit(1)
