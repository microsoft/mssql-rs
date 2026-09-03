#!/usr/bin/env python3
"""Embed the mssql-odbc native driver (mssqlodbc.{so,dylib,dll}) into the
mssql-py-core wheels so a single wheel carries both the PyO3 TDS core and the
Rust ODBC driver.

The driver is placed under ``mssql_py_core/libs/...`` using the exact layout
mssql-python's native resolver (``GetDriverPathForProviderCpp`` in
ddbc_bindings.cpp) reads for the ``mssql-odbc`` provider, rooted at the
mssql_py_core package directory:

    Linux glibc : libs/linux/glibc/<arch>/lib/mssqlodbc.so
    Linux musl  : libs/linux/musl/<arch>/lib/mssqlodbc.so
    macOS       : libs/macos/<arch>/lib/mssqlodbc.dylib
    Windows     : libs/windows/<winArch>/mssqlodbc.dll

where <arch> is ``x86_64`` or ``arm64`` (Linux/macOS) and <winArch> is ``x64``
or ``arm64`` (Windows). The driver is per platform+arch, so the same binary is
duplicated across the per-Python-version wheels for a given platform.

The staged --drivers-dir mirrors the ``libs/`` subtree exactly, e.g.

    <drivers-dir>/windows/x64/mssqlodbc.dll
    <drivers-dir>/linux/glibc/x86_64/lib/mssqlodbc.so
    <drivers-dir>/macos/arm64/lib/mssqlodbc.dylib

Wheels are rewritten in place with the stdlib ``zipfile`` (no third-party
dependency), regenerating the ``RECORD`` so the added files are listed.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import sys
import zipfile
from pathlib import Path

# The package directory that becomes the provider base dir at runtime
# (parent of mssql_py_core/__init__.py). Driver files land under
# <this>/libs/... to match GetOdbcLibsBaseDir()'s parent_path(__file__).
PACKAGE_DIR = "mssql_py_core"

# libs/-relative driver paths per platform+arch, mirrored under --drivers-dir.
_WIN_X64 = "windows/x64/mssqlodbc.dll"
_WIN_ARM64 = "windows/arm64/mssqlodbc.dll"
_GLIBC_X64 = "linux/glibc/x86_64/lib/mssqlodbc.so"
_GLIBC_ARM64 = "linux/glibc/arm64/lib/mssqlodbc.so"
_MUSL_X64 = "linux/musl/x86_64/lib/mssqlodbc.so"
_MUSL_ARM64 = "linux/musl/arm64/lib/mssqlodbc.so"
_MACOS_X64 = "macos/x86_64/lib/mssqlodbc.dylib"
_MACOS_ARM64 = "macos/arm64/lib/mssqlodbc.dylib"


def drivers_for_platform_tag(platform_tag: str) -> list[str]:
    """Return the libs/-relative driver path(s) to inject for a wheel whose
    platform tag is ``platform_tag``. Empty means the wheel is not one we
    embed a driver into (caller treats that as an error for our matrix)."""
    tag = platform_tag.lower()

    if "win_amd64" in tag:
        return [_WIN_X64]
    if "win_arm64" in tag:
        return [_WIN_ARM64]

    if "manylinux" in tag:
        if "x86_64" in tag:
            return [_GLIBC_X64]
        if "aarch64" in tag:
            return [_GLIBC_ARM64]
    if "musllinux" in tag:
        if "x86_64" in tag:
            return [_MUSL_X64]
        if "aarch64" in tag:
            return [_MUSL_ARM64]

    # Re-tagged macOS wheels are universal2 -> ship both arch slices.
    if "macosx" in tag and "universal2" in tag:
        return [_MACOS_X64, _MACOS_ARM64]

    return []


def platform_tag_of(wheel_name: str) -> str:
    """Extract the platform tag from a wheel filename. The wheel filename is
    ``<dist>-<ver>(-<build>)?-<python>-<abi>-<platform>.whl``; the platform tag
    is the final ``-``-separated field before ``.whl`` (compressed tag sets use
    ``.`` as an inner separator, which we keep)."""
    stem = wheel_name[:-len(".whl")] if wheel_name.endswith(".whl") else wheel_name
    return stem.rsplit("-", 1)[-1]


def _record_line(arcname: str, data: bytes) -> str:
    digest = hashlib.sha256(data).digest()
    encoded = base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")
    return f"{arcname},sha256={encoded},{len(data)}"


def inject_wheel(wheel: Path, drivers_dir: Path) -> None:
    platform_tag = platform_tag_of(wheel.name)
    rel_paths = drivers_for_platform_tag(platform_tag)
    if not rel_paths:
        raise SystemExit(
            f"ERROR: unrecognized platform tag '{platform_tag}' for wheel "
            f"'{wheel.name}'; no driver mapping."
        )

    additions: dict[str, bytes] = {}
    for rel in rel_paths:
        src = drivers_dir / rel
        if not src.is_file():
            raise SystemExit(
                f"ERROR: missing staged driver '{src}' required for wheel "
                f"'{wheel.name}'."
            )
        # Wheel archives always use forward slashes.
        additions[f"{PACKAGE_DIR}/libs/{rel}"] = src.read_bytes()

    with zipfile.ZipFile(wheel, "r") as zf:
        names = zf.namelist()
        entries = {name: zf.read(name) for name in names}

    record_name = next((n for n in names if n.endswith(".dist-info/RECORD")), None)
    if record_name is None:
        raise SystemExit(f"ERROR: wheel '{wheel.name}' has no dist-info/RECORD.")
    if not any(n.startswith(PACKAGE_DIR + "/") for n in names):
        raise SystemExit(
            f"ERROR: wheel '{wheel.name}' has no '{PACKAGE_DIR}/' package "
            f"directory to inject into."
        )

    entries.update(additions)

    # Regenerate RECORD: a hashed line for every file (skipping directory
    # entries and RECORD itself), then RECORD's own hash-less line last.
    record_lines = [
        _record_line(name, data)
        for name, data in entries.items()
        if name != record_name and not name.endswith("/")
    ]
    record_lines.append(f"{record_name},,")
    entries[record_name] = ("\n".join(record_lines) + "\n").encode("utf-8")

    tmp = wheel.with_name(wheel.name + ".tmp")
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    tmp.replace(wheel)

    for arcname in additions:
        print(f"  + {arcname}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheels-dir", required=True, type=Path,
                        help="Directory of mssql_py_core wheels to inject into (edited in place).")
    parser.add_argument("--drivers-dir", required=True, type=Path,
                        help="Directory mirroring the libs/ subtree of staged mssqlodbc drivers.")
    args = parser.parse_args()

    if not args.wheels_dir.is_dir():
        raise SystemExit(f"ERROR: --wheels-dir '{args.wheels_dir}' is not a directory.")
    if not args.drivers_dir.is_dir():
        raise SystemExit(f"ERROR: --drivers-dir '{args.drivers_dir}' is not a directory.")

    wheels = sorted(args.wheels_dir.glob("*.whl"))
    if not wheels:
        raise SystemExit(f"ERROR: no wheels found in '{args.wheels_dir}'.")

    for wheel in wheels:
        print(f"Injecting mssqlodbc into {wheel.name}")
        inject_wheel(wheel, args.drivers_dir)
    print(f"Done: injected driver into {len(wheels)} wheel(s).")

    return 0


if __name__ == "__main__":
    sys.exit(main())
