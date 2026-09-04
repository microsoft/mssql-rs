#!/usr/bin/env python3
"""Assemble the macOS docker toolchain payload for one architecture.

Collects everything a hosted macOS agent needs to run containers, so the job
itself touches nothing but our own feed:

  bin/     docker, colima, limactl and lima's helpers, from Homebrew bottles
  image/   the Ubuntu guest disk image colima boots
  manifest.json  what went in, where it came from, and its digests

The guest image reference is not hard-coded. colima embeds a table of
"<arch> <runtime> <url> <sha512> <filename>" for the image release it expects,
so the image and the checksum to verify it against are read out of the very
binary being packaged. colima 0.10.3 wants colima-core v0.10.4, which any
hand-maintained mapping would get wrong.

Runs on Linux: nothing here is executed, only fetched, checked and repacked.

Usage: build-macos-docker-toolchain.py --arch x86_64|arm64 --out <dir>
"""

import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import struct
import sys
import time
import urllib.error
import urllib.request

FORMULAE = ("docker", "colima", "lima")
ARCH_TO_COLIMA = {"x86_64": "amd64", "arm64": "arm64"}
MACHO_MAGIC_64 = 0xFEEDFACF
MACHO_CPU_TYPE = {"x86_64": 0x01000007, "arm64": 0x0100000C}
# Bottles built for an older macOS run on newer hosts, so target the oldest
# release we might schedule on and stay compatible with everything above it.
DEFAULT_MACOS_MAJOR = 14

HERE = os.path.dirname(os.path.abspath(__file__))


def load_bottle_helper():
    path = os.path.join(HERE, "install-brew-bottle.py")
    spec = importlib.util.spec_from_file_location("install_brew_bottle", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def verify_macho(path, arch):
    """Guard against packaging the wrong slice: this cross-builds on Linux."""
    with open(path, "rb") as handle:
        header = handle.read(8)
    if len(header) < 8:
        raise RuntimeError(f"{path}: too short to be a Mach-O binary")
    magic, cpu_type = struct.unpack("<II", header)
    if magic != MACHO_MAGIC_64:
        raise RuntimeError(f"{path}: not a 64-bit Mach-O binary (magic {magic:#x})")
    if cpu_type != MACHO_CPU_TYPE[arch]:
        wrong = next((k for k, v in MACHO_CPU_TYPE.items() if v == cpu_type), hex(cpu_type))
        raise RuntimeError(f"{path}: built for {wrong}, expected {arch}")


def image_entry(colima_binary, arch):
    """Read colima's embedded (arch, runtime) -> image URL + sha512 table."""
    with open(colima_binary, "rb") as handle:
        blob = handle.read()
    pattern = (
        rb"(" + ARCH_TO_COLIMA[arch].encode() + rb")\s+docker\s+"
        rb"(https://\S+\.raw\.gz)\s+([0-9a-f]{128})\s+(\S+\.raw\.gz)"
    )
    match = re.search(pattern, blob)
    if not match:
        raise RuntimeError(
            f"colima binary has no docker image entry for {arch}; "
            "the embedded image table may have changed format"
        )
    return {
        "url": match.group(2).decode(),
        "sha512": match.group(3).decode(),
        "filename": match.group(4).decode(),
    }


def download_verified(url, sha512, dest, attempts=3):
    """Stream to disk, hashing as we go -- the image is ~350 MB."""
    for attempt in range(attempts):
        digest = hashlib.sha512()
        try:
            with urllib.request.urlopen(url, timeout=300) as resp, open(dest, "wb") as out:
                while True:
                    chunk = resp.read(1 << 20)
                    if not chunk:
                        break
                    digest.update(chunk)
                    out.write(chunk)
        except (urllib.error.URLError, TimeoutError) as exc:
            if attempt == attempts - 1:
                raise RuntimeError(f"downloading {url} failed: {exc}") from None
            time.sleep(10 * (attempt + 1))
            continue

        actual = digest.hexdigest()
        if actual == sha512:
            return os.path.getsize(dest)
        raise RuntimeError(
            f"{url}: sha512 mismatch against colima's own manifest\n"
            f"  expected {sha512}\n  actual   {actual}"
        )
    raise RuntimeError(f"downloading {url} failed after {attempts} attempts")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", required=True, choices=sorted(ARCH_TO_COLIMA))
    parser.add_argument("--out", required=True)
    parser.add_argument(
        "--macos-major",
        type=int,
        default=DEFAULT_MACOS_MAJOR,
        help="oldest macOS major version the payload must run on (default: %(default)s)",
    )
    args = parser.parse_args()

    helper = load_bottle_helper()
    os.environ["BOTTLE_ARCH_OVERRIDE"] = args.arch
    os.environ["BOTTLE_MACOS_MAJOR_OVERRIDE"] = str(args.macos_major)

    bin_dir = os.path.join(args.out, "bin")
    image_dir = os.path.join(args.out, "image")
    if os.path.exists(args.out):
        shutil.rmtree(args.out)
    os.makedirs(bin_dir)
    os.makedirs(image_dir)

    components = {}
    for formula in FORMULAE:
        token = helper.anonymous_token(formula)
        version, manifest_digest, ref = helper.find_bottle(formula, token)
        blob = helper.download_blob(formula, token, manifest_digest)
        # Whole prefix, not just bin/: limactl reaches for ../share/lima and
        # ../libexec/lima, so a bin-only copy cannot boot a VM.
        extracted = helper.extract_prefix(blob, formula, version, args.out)
        components[formula] = {
            "version": version,
            "bottle_tag": ref,
            "bottle_digest": manifest_digest,
            "files": len(extracted),
        }
        print(f"{formula:8s} {version:10s} {ref:24s} -> {len(extracted)} files")

    for name in ("docker", "colima", "limactl"):
        verify_macho(os.path.join(bin_dir, name), args.arch)
    print(f"verified {args.arch} Mach-O binaries")

    # limactl resolves the guest agent relative to its own location, so its
    # absence is a boot failure on the agent rather than a build failure here.
    guest_agent = os.path.join(args.out, "share", "lima")
    agents = [f for f in os.listdir(guest_agent) if f.startswith("lima-guestagent")] \
        if os.path.isdir(guest_agent) else []
    if not agents:
        raise RuntimeError("payload has no share/lima/lima-guestagent.*; lima could not boot a VM")
    print(f"guest agent present: {', '.join(agents)}")

    entry = image_entry(os.path.join(bin_dir, "colima"), args.arch)
    image_path = os.path.join(image_dir, entry["filename"])
    size = download_verified(entry["url"], entry["sha512"], image_path)
    print(f"image    {entry['filename']} ({size / 1e6:.0f} MB) sha512 verified")

    manifest = {
        "arch": args.arch,
        "macos_major_floor": args.macos_major,
        "components": components,
        "image": {
            **entry,
            "size": size,
            # colima looks the image up in its cache by sha256 of the URL, so the
            # consumer can seed it without colima ever reaching the network.
            "cache_filename": hashlib.sha256(entry["url"].encode()).hexdigest(),
        },
    }
    with open(os.path.join(args.out, "manifest.json"), "w") as out:
        json.dump(manifest, out, indent=2, sort_keys=True)

    versions = "-".join(f"{name}{components[name]['version']}" for name in FORMULAE)
    print(f"\npayload ready: {args.out}")
    print(f"  contents: {versions}, image {entry['url'].split('/')[-2]}")
    print(f"  cache filename: {manifest['image']['cache_filename']}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as exc:
        print(f"##[error]{exc}")
        sys.exit(1)
