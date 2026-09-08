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
import urllib.parse
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


def organization(collection_uri):
    """Organization name out of either form of System.CollectionUri.

    Older organizations -- this one included -- still hand agents
    https://<org>.visualstudio.com/ rather than https://dev.azure.com/<org>/,
    so taking the last path segment yields a hostname and a feed URL that 404s.
    """
    parsed = urllib.parse.urlparse(collection_uri.strip().rstrip("/"))
    host = parsed.netloc or parsed.path
    if host.endswith(".visualstudio.com"):
        return host.split(".", 1)[0]
    segments = [segment for segment in parsed.path.split("/") if segment]
    if not segments:
        raise RuntimeError(f"cannot read an organization out of {collection_uri!r}")
    return segments[-1]


def published_packages(org_url, project, feed, token):
    # Filtered by name rather than listing the feed: the response is paginated,
    # and a feed with enough packages would push these off the first page and
    # make them look unpublished. Only the two names can match this prefix.
    url = (
        f"https://feeds.dev.azure.com/{organization(org_url)}/{project}"
        f"/_apis/packaging/Feeds/{feed}/packages"
        f"?protocolType=upack&packageNameQuery={PACKAGE_PREFIX}"
        f"&includeDescription=true&api-version={API_VERSION}"
    )
    request = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            body = json.load(response)
    except urllib.error.HTTPError as exc:
        # The URL matters more than the feed name here: a 404 is equally what
        # you get from a malformed organization and from an identity that
        # cannot see the feed.
        raise RuntimeError(f"GET {url} failed: {exc.code} {exc.reason}") from None
    except urllib.error.URLError as exc:
        raise RuntimeError(f"GET {url} failed: {exc}") from None

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


def version_key(version):
    return [int(part) for part in version.split(".")]


def decide(state, override=None, force=False):
    """(version, architectures to publish, reasons) from each architecture's state.

    `state[arch]` is the published version (None if absent), whether that
    published package already matches upstream, and whether its layout is stale.

    A version is meant to mean the same build produced every architecture, so
    the normal path publishes all of them together at a version that exists for
    none. Publishing is not atomic across packages though: a failure after the
    first upload leaves one architecture behind at a version the other already
    has. That is repaired rather than papered over -- the laggard is published
    at the version the others reached, which is legal precisely because it does
    not exist for that package yet.
    """
    versions = {arch: s["version"] for arch, s in state.items()}
    behind = [arch for arch, v in versions.items() if v is not None]
    leader = max((versions[a] for a in behind), default=None, key=version_key)
    lagging = {arch for arch, v in versions.items() if v is not None and v != leader}

    # Repair first: with a partial publish outstanding, a laggard's contents
    # differing from *its* version is the symptom, not a reason to bump past the
    # version it is missing. Only sound while the leader is itself current --
    # otherwise that version predates upstream and nobody should join it.
    if lagging and not override and all(state[a]["current"] for a in state if a not in lagging):
        return leader, lagging, [
            f"partial publish: {', '.join(sorted(lagging))} never reached {leader}",
        ]

    reasons = []
    for arch in sorted(state):
        if versions[arch] is None:
            reasons.append(f"- {arch}: not published yet")
        elif not state[arch]["current"]:
            reasons.append(f"- {arch}: differs from {versions[arch]}")

    if not reasons and not force:
        return next_version(leader, layout_changed=False), set(), []
    if not reasons:
        reasons = ["nothing changed upstream, but a publish was forced"]
    else:
        reasons.insert(0, "republishing because:")

    # A version nobody holds yet, so every architecture can take it. A stale or
    # absent layout is the minor bump: consumers resolving paths into the tree
    # may have to move with it, where a component bump leaves the tree alone.
    layout_changed = any(s["layout_stale"] for s in state.values())
    return override or next_version(leader, layout_changed), set(state), reasons


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

    state = {}
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
        else:
            print(f"  published    : {version} (layout {layout}, id {published_identity})")
        state[arch] = {
            "version": version,
            "layout_stale": version is None or layout != builder.PAYLOAD_FORMAT,
            "current": version is not None
            and layout == builder.PAYLOAD_FORMAT
            and published_identity == identity,
        }

    version, publish_for, reasons = decide(state, override, args.force)

    print("\n" + "=" * 62)
    for reason in reasons:
        print(f"  {reason}")
    if publish_for:
        print(f"publishing {', '.join(sorted(publish_for))} as {version}"
              + (" (explicit override)" if override else ""))
    else:
        print("nothing changed upstream; leaving the feed alone")

    for arch in args.arch:
        emit(f"publish_{arch}", "true" if arch in publish_for else "false")
    emit("shouldPublish", "true" if publish_for else "false")
    emit("packageVersion", version)
    return 0

    emit("shouldPublish", "true" if should_publish else "false")
    emit("packageVersion", version)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as exc:
        print(f"##[error]{exc}")
        sys.exit(1)
