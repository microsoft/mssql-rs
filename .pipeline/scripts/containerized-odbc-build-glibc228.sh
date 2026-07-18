#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the mssql-odbc driver (libmsodbcsql18.so) and the C++ gtest e2e binaries
# on a glibc-2.28 base (PyPA manylinux_2_28 = AlmaLinux 8), then stage them into
# a drop directory the surrounding pipeline job publishes as an artifact. Later
# the RHEL 8 test stage downloads the drop and reruns the prebuilt binaries via
# run_e2e.sh --skip-build.
#
# Why a separate track: the driver dynamically links libssl/libcrypto. AlmaLinux
# 8 ships glibc 2.28 and OpenSSL 1.1 (libssl.so.1.1), matching RHEL 8 / UBI 8, so
# a binary built here loads on RHEL 8 (the modern glibc 2.35 / OpenSSL 3 track
# does not).
#
# The manylinux_2_28 rust build image (python-build/manylinux_2_28_*_rust) ships
# cargo plus the cmake / unixODBC dev headers we need pre-installed, so this
# script just cleans the shared trees and runs the e2e build.
#
# Env:
#   ODBC_DROP_DIR   Drop directory to stage into (default: /workspace/odbc-drop).

set -euo pipefail

DROP_DIR="${ODBC_DROP_DIR:-/workspace/odbc-drop}"

# The glibc / musl / glibc-2.28 tracks build sequentially in the same job,
# sharing the in-place CMake build tree (tests/e2e/build) and this drop dir.
# Both end up root-owned by the container, so clean them here (as root) to avoid
# cross-toolchain CMake cache reuse and host-side permission errors.
rm -rf "$DROP_DIR" /workspace/mssql-odbc/tests/e2e/build

# cargo is baked into the image at /root/.cargo; source it in case PATH was reset.
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
fi

# Build into a target dir dedicated to this track. The modern-glibc track builds
# first in the same job and shares the workspace, and it uses the same host
# triple (x86_64-unknown-linux-gnu), so cargo's fingerprints match and it would
# otherwise reuse the modern track's host build scripts (e.g. openssl-sys's
# build-script-main). Those are linked against the modern image's glibc (2.34+)
# and fail to run on this glibc-2.28 base. A separate target dir forces cargo to
# recompile the build scripts here under glibc 2.28. build_e2e.sh resolves the
# driver via `cargo metadata`, which honors CARGO_TARGET_DIR.
export CARGO_TARGET_DIR=/workspace/target-glibc228
rm -rf "$CARGO_TARGET_DIR"

exec /workspace/mssql-odbc/tests/e2e/build_e2e.sh --release --out="$DROP_DIR"
