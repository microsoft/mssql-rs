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

exec /workspace/mssql-odbc/tests/e2e/build_e2e.sh --release --out="$DROP_DIR"
