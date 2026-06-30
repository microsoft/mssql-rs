#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Preflight guard for the sandbox release flow.

Reads the base ``version`` from a pyproject.toml and fails (exit 1) when that
exact version is already published to the Azure Artifacts PyPI feed. This lets
the pipeline fail in seconds instead of building seven wheels and only hitting
the duplicate-version rejection at ``twine upload``.

The ``mssql-rs_Public`` feed allows anonymous reads, so the simple index URL is
passed in directly (no credentials). For convenience an embedded-credential URL
(``https://user:token@.../pypi/simple/``) is still accepted, as is a fallback to
the ``PIP_INDEX_URL`` environment variable.

Best-effort: if the feed cannot be reached or the index URL is missing, we WARN
and exit 0. The duplicate-version rejection at upload time remains the
authoritative guard, so we never block a release on a transient lookup failure.
"""

import base64
import re
import sys
import urllib.error
import urllib.request
from html.parser import HTMLParser
from urllib.parse import urlsplit, urlunsplit


def read_base_version(pyproject_path: str) -> str:
    with open(pyproject_path, encoding="utf-8") as f:
        text = f.read()
    m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    if not m:
        sys.exit(f"ERROR: could not read version from {pyproject_path}")
    return m.group(1).strip()


def normalize_name(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def normalize_version(version: str) -> str:
    # Light PEP 440 normalization: lowercase, drop a leading 'v', collapse the
    # SemVer '-dev' / '-rc' style separators maturin emits to PEP 440 form so a
    # filename version compares equal to the manifest version.
    v = version.strip().lower()
    if v.startswith("v"):
        v = v[1:]
    v = v.replace("-dev", ".dev").replace("-rc", "rc").replace("-alpha", "a").replace("-beta", "b")
    return v


class _AnchorParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.filenames: list[str] = []

    def handle_data(self, data: str) -> None:
        data = data.strip()
        if data.endswith((".whl", ".tar.gz", ".zip")):
            self.filenames.append(data)


def version_from_filename(filename: str) -> str | None:
    if filename.endswith(".whl"):
        parts = filename[:-4].split("-")
        return parts[1] if len(parts) >= 2 else None
    for ext in (".tar.gz", ".zip"):
        if filename.endswith(ext):
            stem = filename[: -len(ext)]
            bits = stem.rsplit("-", 1)
            return bits[1] if len(bits) == 2 else None
    return None


def fetch_simple_page(index_url: str, package: str) -> str | None:
    parts = urlsplit(index_url)
    auth_header = None
    netloc = parts.netloc
    if "@" in netloc:
        userinfo, host = netloc.rsplit("@", 1)
        netloc = host
        token = base64.b64encode(userinfo.encode("utf-8")).decode("ascii")
        auth_header = f"Basic {token}"

    base = urlunsplit((parts.scheme, netloc, parts.path, "", ""))
    if not base.endswith("/"):
        base += "/"
    url = f"{base}{normalize_name(package)}/"

    req = urllib.request.Request(url)
    if auth_header:
        req.add_header("Authorization", auth_header)
    req.add_header("Accept", "text/html")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return ""  # package not present at all -> no published versions
        print(f"WARNING: feed lookup failed (HTTP {e.code}); skipping preflight.")
        return None
    except (urllib.error.URLError, TimeoutError) as e:
        print(f"WARNING: feed unreachable ({e}); skipping preflight.")
        return None


def main(argv: list[str]) -> int:
    if len(argv) not in (3, 4):
        sys.exit(
            "usage: check-version-not-published.py <pyproject.toml> <package-name> [simple-index-url]"
        )
    pyproject_path, package = argv[1], argv[2]

    base = read_base_version(pyproject_path)
    target = normalize_version(base)
    print(f"Release preflight: checking feed for {package}=={base} (normalized {target})")

    import os

    index_url = (argv[3] if len(argv) == 4 else os.environ.get("PIP_INDEX_URL", "")).strip()
    if not index_url:
        print("WARNING: no simple index URL provided; skipping preflight (upload still guards).")
        return 0

    page = fetch_simple_page(index_url, package)
    if page is None:
        return 0  # best-effort: could not determine
    if page == "":
        print(f"OK: {package} has no published versions yet.")
        return 0

    parser = _AnchorParser()
    parser.feed(page)
    published = set()
    for fn in parser.filenames:
        v = version_from_filename(fn)
        if v:
            published.add(normalize_version(v))

    if target in published:
        print(
            f"ERROR: {package}=={base} is already published to the feed.\n"
            f"       Bump 'version' in {pyproject_path} before running a release.\n"
            f"       Azure Artifacts rejects re-uploading an existing version."
        )
        return 1

    print(f"OK: {package}=={base} is not on the feed; safe to release.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
