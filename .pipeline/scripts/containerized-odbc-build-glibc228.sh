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
# The manylinux_2_28 base ships a C/C++ toolchain and Python but no cargo and not
# the cmake / unixODBC dev headers we need, so install those (and rustup) here.
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

# openssl-devel on AlmaLinux 8 is 1.1.1, so the driver links libssl.so.1.1.
dnf install -y --allowerasing \
    gcc gcc-c++ make cmake unixODBC-devel openssl-devel

if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1090
source "$HOME/.cargo/env"

exec /workspace/mssql-odbc/tests/e2e/build_e2e.sh --release --out="$DROP_DIR"
