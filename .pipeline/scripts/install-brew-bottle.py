#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Install the newest Homebrew bottle of a formula that exists for this platform.

Homebrew publishes bottles as OCI artifacts on ghcr.io, readable anonymously.
When `brew install --force-bottle` fails because the *current* formula version
has no bottle for the running platform (docker 29.8.0 ships arm64 macOS and
Linux only), the previous version usually still does. This walks the registry
newest-first, finds a version bottled for this platform, and extracts its
binaries.

The bottle blob is content-addressed: the layer digest is its SHA-256, so the
download is verified against the digest the registry advertises rather than a
checksum vendored here that someone has to remember to bump.

Usage: install-brew-bottle.py <formula> <dest-bin-dir>
Prints the resolved version to stdout.
"""

import hashlib
import io
import json
import os
import platform
import re
import shutil
import sys
import tarfile
import time
import urllib.error
import urllib.request

REGISTRY = "https://ghcr.io"
REPO_PREFIX = "homebrew/core"

# Every extra version costs a registry round-trip, and the answer is always in
# the newest few. Bounds the no-match path so it reports failure while the
# caller's install budget is still intact instead of walking the whole tag list.
MAX_VERSIONS_SCANNED = 15

# Oldest to newest. A bottle built for an older macOS runs on a newer one, so a
# tag is usable when its index is <= the running OS, and higher is preferred.
MACOS_CODENAMES = [
    "el_capitan", "sierra", "high_sierra", "mojave", "catalina",
    "big_sur", "monterey", "ventura", "sonoma", "sequoia", "tahoe",
]
MACOS_CODENAMES_BY_MAJOR = {
    11: "big_sur", 12: "monterey", 13: "ventura", 14: "sonoma",
    15: "sequoia", 26: "tahoe",
}

TAG_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)(?:[._-](\d+))?$")


def _get(url, token=None, accept=None, binary=False, attempts=3):
    req = urllib.request.Request(url)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    if accept:
        req.add_header("Accept", accept)
    last = None
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                body = resp.read()
                return (body if binary else json.loads(body)), resp.headers
        except urllib.error.HTTPError as exc:
            # 4xx other than rate limiting is a permanent answer; retrying it
            # only burns the install budget.
            if 400 <= exc.code < 500 and exc.code not in (408, 429):
                raise RuntimeError(f"GET {url} failed: HTTP {exc.code}") from None
            last = exc
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
            last = exc
        if attempt < attempts - 1:
            time.sleep(5 * (attempt + 1))
    raise RuntimeError(f"GET {url} failed after {attempts} attempts: {last}")


def anonymous_token(formula):
    scope = f"repository:{REPO_PREFIX}/{formula}:pull"
    data, _ = _get(f"{REGISTRY}/token?service=ghcr.io&scope={scope}")
    return data["token"]


def list_versions(formula, token):
    url = f"{REGISTRY}/v2/{REPO_PREFIX}/{formula}/tags/list?n=100"
    tags = []
    seen = set()
    while url and url not in seen:
        seen.add(url)
        data, headers = _get(url, token=token)
        tags.extend(data.get("tags", []))
        link = headers.get("Link")
        match = re.search(r"<([^>]+)>", link) if link else None
        if not match:
            break
        # `Link` is relative on ghcr.io, but the spec permits an absolute URL.
        # `seen` stops a registry that keeps handing back the same page.
        target = match.group(1)
        url = target if target.startswith("http") else REGISTRY + target
    return tags


def version_key(tag):
    match = TAG_RE.match(tag)
    return tuple(int(part) for part in match.groups(default=0)) if match else None


def ref_parts(tag):
    """The `<base>` and `<revision>` a version tag's ref names are built from.

    Homebrew spells a formula revision as `29.7.2-1` in the registry tag but as
    `29.7.2.<platform>.1` in the ref names inside that tag's index, so the tag
    cannot be matched against a ref name directly.
    """
    match = TAG_RE.match(tag)
    return ".".join(match.group(1, 2, 3)), match.group(4)


def current_os_rank():
    override = os.environ.get("BOTTLE_MACOS_MAJOR_OVERRIDE")
    raw = override or platform.mac_ver()[0].split(".")[0]
    if not raw.isdigit():
        raise RuntimeError(f"cannot determine the macOS major version (got {raw!r})")
    major = int(raw)
    codename = MACOS_CODENAMES_BY_MAJOR.get(major)
    if codename is not None:
        return MACOS_CODENAMES.index(codename)
    if major > max(MACOS_CODENAMES_BY_MAJOR):
        # Newer than anything mapped: every published bottle predates it, so all
        # of them run here.
        return len(MACOS_CODENAMES) - 1
    # An unmapped major below the newest known release can't be ranked, and
    # guessing risks picking a bottle too new for the host to run.
    raise RuntimeError(f"unrecognized macOS major version {major}")


def platform_prefix():
    machine = os.environ.get("BOTTLE_ARCH_OVERRIDE") or platform.machine()
    return "arm64_" if machine in ("arm64", "aarch64") else ""


def usable_tag_rank(ref_name, base, revision, prefix, max_rank):
    """Rank of a bottle ref name for this platform, or None if unusable."""
    if not ref_name.startswith(f"{base}."):
        return None
    tag = ref_name[len(base) + 1:]
    if revision is not None:
        suffix = f".{revision}"
        if not tag.endswith(suffix):
            return None
        tag = tag[: -len(suffix)]
    if "linux" in tag:
        return None
    if prefix:
        if not tag.startswith(prefix):
            return None
        tag = tag[len(prefix):]
    elif tag.startswith("arm64_"):
        return None
    if tag not in MACOS_CODENAMES:
        return None
    rank = MACOS_CODENAMES.index(tag)
    return rank if rank <= max_rank else None


def find_bottle(formula, token):
    versions = [t for t in list_versions(formula, token) if version_key(t)]
    versions.sort(key=version_key, reverse=True)
    prefix = platform_prefix()
    max_rank = current_os_rank()

    for version in versions[:MAX_VERSIONS_SCANNED]:
        try:
            index, _ = _get(
                f"{REGISTRY}/v2/{REPO_PREFIX}/{formula}/manifests/{version}",
                token=token,
                accept="application/vnd.oci.image.index.v1+json",
            )
        except RuntimeError as exc:
            # A tag can be listed but unreadable (deleted, or an upload that
            # never completed). The next-newest version is just as good.
            print(f"##[warning]skipping {formula} {version}: {exc}", file=sys.stderr)
            continue
        base, revision = ref_parts(version)
        best = None
        for manifest in index.get("manifests", []):
            ref = manifest.get("annotations", {}).get("org.opencontainers.image.ref.name", "")
            rank = usable_tag_rank(ref, base, revision, prefix, max_rank)
            if rank is not None and (best is None or rank > best[0]):
                best = (rank, manifest["digest"], ref)
        if best:
            return version, best[1], best[2]
    raise RuntimeError(
        f"no {formula} bottle for this platform in the newest {MAX_VERSIONS_SCANNED} versions"
    )


def download_blob(formula, token, manifest_digest):
    manifest, _ = _get(
        f"{REGISTRY}/v2/{REPO_PREFIX}/{formula}/manifests/{manifest_digest}",
        token=token,
        accept="application/vnd.oci.image.manifest.v1+json",
    )
    layer = manifest["layers"][0]["digest"]
    blob, _ = _get(
        f"{REGISTRY}/v2/{REPO_PREFIX}/{formula}/blobs/{layer}",
        token=token,
        binary=True,
    )
    expected = layer.split(":", 1)[1]
    actual = hashlib.sha256(blob).hexdigest()
    if actual != expected:
        raise RuntimeError(f"bottle digest mismatch: expected {expected}, got {actual}")
    return blob


def extract_bin(blob, formula, version, dest_dir):
    os.makedirs(dest_dir, exist_ok=True)
    base, _ = ref_parts(version)
    wanted = f"{formula}/{base}/bin/"
    installed = []
    with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as tar:
        for member in tar.getmembers():
            # Only regular files directly under the formula's bin/, and never
            # a path that escapes it -- the archive is untrusted input.
            name = os.path.normpath(member.name).replace(os.sep, "/")
            if not member.isfile() or not name.startswith(wanted):
                continue
            if os.path.basename(name) != name[len(wanted):]:
                continue
            src = tar.extractfile(member)
            if src is None:
                continue
            target = os.path.join(dest_dir, os.path.basename(name))
            with open(target, "wb") as out:
                shutil.copyfileobj(src, out)
            os.chmod(target, 0o755)
            installed.append(os.path.basename(name))
    if not installed:
        raise RuntimeError(f"bottle for {formula} {version} contained no bin/ entries")
    return installed


def main():
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    formula, dest_dir = sys.argv[1], sys.argv[2]

    try:
        token = anonymous_token(formula)
        version, manifest_digest, ref = find_bottle(formula, token)
        blob = download_blob(formula, token, manifest_digest)
        installed = extract_bin(blob, formula, version, dest_dir)
    except RuntimeError as exc:
        print(f"##[error]{exc}")
        return 1

    print(f"{formula} {version} ({ref}) -> {dest_dir}: {', '.join(installed)}", file=sys.stderr)
    print(version)
    return 0


if __name__ == "__main__":
    sys.exit(main())
