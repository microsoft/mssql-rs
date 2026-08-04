#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Split debug info out of the native extension inside an already-built wheel.

Writes a standalone ``.debug`` file (locatable by GNU build-id) next to a copy
of the stripped ``.so`` in the symbols directory, and rewrites the wheel so the
shipped extension is ``--strip-debug``'d.

The wheel is rewritten entry-by-entry rather than unpacked and repacked, so the
filename, member order, timestamps, compression and Unix permission bits are all
preserved. Only the ``.so`` payload and its ``RECORD`` line change.

Usage: split-wheel-debuginfo.py <wheel> <symbols-output-dir>
"""

from __future__ import annotations

import base64
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile

BUILD_ID_RE = re.compile(r"Build ID:\s*([0-9a-f]+)", re.IGNORECASE)


def fail(message: str) -> None:
    print(f"ERROR: {message}", file=sys.stderr)
    sys.exit(1)


def run(*args: str) -> str:
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode != 0:
        fail(f"{args[0]} failed: {result.stderr.strip()}")
    return result.stdout


def build_id(path: str) -> str:
    match = BUILD_ID_RE.search(run("readelf", "-n", path))
    return match.group(1) if match else ""


def record_hash(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return "sha256=" + digest.decode("ascii")


def rewrite_record(record: bytes, so_name: str, so_bytes: bytes) -> bytes:
    lines = record.decode("utf-8").splitlines()
    replacement = f"{so_name},{record_hash(so_bytes)},{len(so_bytes)}"
    for index, line in enumerate(lines):
        if line.split(",", 1)[0] == so_name:
            lines[index] = replacement
            break
    else:
        fail(f"{so_name} has no RECORD entry")
    return ("\n".join(lines) + "\n").encode("utf-8")


def main(wheel_path: str, symbols_dir: str) -> None:
    with zipfile.ZipFile(wheel_path) as archive:
        entries = archive.infolist()
        payloads = {entry.filename: archive.read(entry) for entry in entries}

    extensions = [
        name
        for name in payloads
        if name.endswith(".so") and os.path.basename(name).startswith("mssql_py_core")
    ]
    if len(extensions) != 1:
        fail(f"expected exactly one mssql_py_core*.so in the wheel, found {extensions}")
    so_name = extensions[0]

    records = [name for name in payloads if name.endswith(".dist-info/RECORD")]
    if len(records) != 1:
        fail(f"expected exactly one .dist-info/RECORD, found {records}")
    record_name = records[0]

    os.makedirs(symbols_dir, exist_ok=True)
    workdir = tempfile.mkdtemp()
    try:
        base = os.path.basename(so_name)
        staged_so = os.path.join(workdir, base)
        with open(staged_so, "wb") as handle:
            handle.write(payloads[so_name])

        original_build_id = build_id(staged_so)
        if not original_build_id:
            fail(f"{base} has no GNU build-id; the symbol server cannot index it")

        debug_path = os.path.join(symbols_dir, base + ".debug")
        run("objcopy", "--only-keep-debug", staged_so, debug_path)
        run("objcopy", "--strip-debug", staged_so)
        run("objcopy", f"--add-gnu-debuglink={debug_path}", staged_so)

        for label, path in (("debug file", debug_path), ("stripped .so", staged_so)):
            if build_id(path) != original_build_id:
                fail(f"build-id of the {label} does not match the original binary")

        with open(staged_so, "rb") as handle:
            stripped = handle.read()
        shutil.copy2(staged_so, os.path.join(symbols_dir, base))

        payloads[so_name] = stripped
        payloads[record_name] = rewrite_record(payloads[record_name], so_name, stripped)

        # Copy every entry verbatim apart from the two payloads that changed.
        rewritten = wheel_path + ".tmp"
        with zipfile.ZipFile(rewritten, "w") as archive:
            for entry in entries:
                info = zipfile.ZipInfo(entry.filename, date_time=entry.date_time)
                info.compress_type = entry.compress_type
                info.external_attr = entry.external_attr
                info.internal_attr = entry.internal_attr
                info.create_system = entry.create_system
                archive.writestr(info, payloads[entry.filename])
        os.replace(rewritten, wheel_path)

        print(
            f"Split debug info from {base} (build-id {original_build_id}): "
            f"{debug_path} ({os.path.getsize(debug_path) // 1024} KB), "
            f"shipped .so now {len(stripped) // 1024} KB"
        )
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        fail("usage: split-wheel-debuginfo.py <wheel> <symbols-output-dir>")
    main(sys.argv[1], sys.argv[2])
