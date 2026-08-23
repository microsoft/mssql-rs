#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the mssql-odbc driver (libmsodbcsql18.so) and the C++ gtest e2e binaries
# inside the Alpine (musl) build container, then stage them into a drop
# directory that the surrounding pipeline job publishes as an artifact. Later
# musl test stages download the drop and rerun the prebuilt binaries via
# run_e2e.sh --skip-build.
#
# The Alpine build image ships rust/cargo but not the C/C++ toolchain, cmake, or
# the unixODBC dev headers the C++ Driver Manager links against, so install them.
#
# Rust static-links crt on musl by default, but the driver is a cdylib loaded by
# unixODBC via dlopen and links libssl dynamically, so build with
# -C target-feature=-crt-static (matching the mssql-tds musl build).
#
# Env:
#   ODBC_DROP_DIR   Drop directory to stage into (default: /workspace/odbc-drop).

set -e

export PATH="$HOME/.cargo/bin:$PATH"

DROP_DIR="${ODBC_DROP_DIR:-/workspace/odbc-drop}"

# The glibc / musl / glibc-2.28 tracks build sequentially in the same job,
# sharing the in-place CMake build tree (tests/e2e/build) and this drop dir.
# Both end up root-owned by the container, so clean them here (as root) to avoid
# cross-toolchain CMake cache reuse and host-side permission errors.
rm -rf "$DROP_DIR" /workspace/mssql-odbc/tests/e2e/build

# bash is required by build_e2e.sh; build-base gives gcc/g++/make; git is
# needed by the e2e CMake FetchContent(googletest) step (the glibc build images
# ship git already, Alpine does not).
apk add --no-cache bash build-base cmake unixodbc-dev openssl-dev git

export RUSTFLAGS="-C target-feature=-crt-static"

exec /workspace/mssql-odbc/tests/e2e/build_e2e.sh --release --out="$DROP_DIR"
