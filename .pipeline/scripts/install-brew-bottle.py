#!/usr/bin/env python3
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
import json
import os
import platform
import re
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

REGISTRY = "https://ghcr.io"
REPO_PREFIX = "homebrew/core"

# Oldest to newest. A bottle built for an older macOS runs on a newer one, so a
# tag is usable when its index is <= the running OS, and higher is preferred.
MACOS_CODENAMES = [
    "el_capitan", "sierra", "high_sierra", "mojave", "catalina",
    "big_sur", "monterey", "ventura", "sonoma", "sequoia", "tahoe",
]
MACOS_CODENAMES_BY_MAJOR = {
    12: "monterey", 13: "ventura", 14: "sonoma", 15: "sequoia", 26: "tahoe",
}


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
    while url:
        data, headers = _get(url, token=token)
        tags.extend(data.get("tags", []))
        link = headers.get("Link")
        match = re.search(r"<([^>]+)>", link) if link else None
        url = REGISTRY + match.group(1) if match else None
    return tags


def version_key(tag):
    match = re.match(r"^(\d+)\.(\d+)\.(\d+)(?:[._-](\d+))?$", tag)
    return tuple(int(part) for part in match.groups(default=0)) if match else None


def current_os_rank():
    override = os.environ.get("BOTTLE_MACOS_MAJOR_OVERRIDE")
    major = int(override or platform.mac_ver()[0].split(".")[0] or 0)
    codename = MACOS_CODENAMES_BY_MAJOR.get(major)
    if codename is None:
        # Unknown/newer macOS: accept the newest tag rather than nothing.
        return len(MACOS_CODENAMES) - 1
    return MACOS_CODENAMES.index(codename)


def platform_prefix():
    machine = os.environ.get("BOTTLE_ARCH_OVERRIDE") or platform.machine()
    return "arm64_" if machine in ("arm64", "aarch64") else ""


def usable_tag_rank(ref_name, version, prefix, max_rank):
    """Rank of a bottle tag for this platform, or None if unusable."""
    if not ref_name.startswith(f"{version}."):
        return None
    tag = ref_name[len(version) + 1:]
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

    for version in versions:
        index, _ = _get(
            f"{REGISTRY}/v2/{REPO_PREFIX}/{formula}/manifests/{version}",
            token=token,
            accept="application/vnd.oci.image.index.v1+json",
        )
        best = None
        for manifest in index.get("manifests", []):
            ref = manifest.get("annotations", {}).get("org.opencontainers.image.ref.name", "")
            rank = usable_tag_rank(ref, version, prefix, max_rank)
            if rank is not None and (best is None or rank > best[0]):
                best = (rank, manifest["digest"], ref)
        if best:
            return version, best[1], best[2]
    raise RuntimeError(f"no {formula} bottle found for this platform")


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


def _members(tar, root):
    """Files under root/, rejecting anything whose path escapes it."""
    for member in tar.getmembers():
        if not (member.isfile() or member.issym()):
            continue
        name = os.path.normpath(member.name)
        if not name.startswith(root) or ".." in name.split("/"):
            continue
        yield member, name[len(root):]


def extract_bin(blob, formula, version, dest_dir):
    """Extract just the executables, flattened into dest_dir."""
    os.makedirs(dest_dir, exist_ok=True)
    root = f"{formula}/{version}/bin/"
    installed = []
    with tempfile.NamedTemporaryFile(suffix=".tar.gz") as tmp:
        tmp.write(blob)
        tmp.flush()
        with tarfile.open(tmp.name, "r:gz") as tar:
            for member, relative in _members(tar, root):
                if "/" in relative or not member.isfile():
                    continue
                src = tar.extractfile(member)
                if src is None:
                    continue
                target = os.path.join(dest_dir, relative)
                with open(target, "wb") as out:
                    shutil.copyfileobj(src, out)
                os.chmod(target, 0o755)
                installed.append(relative)
    if not installed:
        raise RuntimeError(f"bottle for {formula} {version} contained no bin/ entries")
    return installed


def extract_prefix(blob, formula, version, dest_root):
    """Extract the whole install prefix, merging into dest_root.

    lima is not self-contained in bin/: limactl reaches for
    ../share/lima/lima-guestagent.* and ../libexec/lima/*, so a bin-only copy
    produces a lima that cannot boot a VM.
    """
    root = f"{formula}/{version}/"
    extracted = []
    with tempfile.NamedTemporaryFile(suffix=".tar.gz") as tmp:
        tmp.write(blob)
        tmp.flush()
        with tarfile.open(tmp.name, "r:gz") as tar:
            for member, relative in _members(tar, root):
                if relative.startswith(".brew/"):
                    continue
                target = os.path.join(dest_root, relative)
                os.makedirs(os.path.dirname(target), exist_ok=True)
                if member.issym():
                    link = os.path.normpath(os.path.join(os.path.dirname(relative), member.linkname))
                    if link.startswith("..") or os.path.isabs(member.linkname):
                        continue
                    if os.path.lexists(target):
                        os.unlink(target)
                    os.symlink(member.linkname, target)
                else:
                    src = tar.extractfile(member)
                    if src is None:
                        continue
                    with open(target, "wb") as out:
                        shutil.copyfileobj(src, out)
                    os.chmod(target, member.mode or 0o644)
                extracted.append(relative)
    if not extracted:
        raise RuntimeError(f"bottle for {formula} {version} extracted nothing")
    return extracted


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
