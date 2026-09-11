#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build ONLY the mssql-odbc driver (no C++ gtest e2e) inside the same wheel-build
# container as its target wheel, so the driver's libc floor matches the wheel's
# by construction: glibc 2.28 or 2.34 for manylinux, musl 1.2 for musllinux.
# This replaces the ubuntu:22.04 / alpine:3.18 driver build on the injection
# path, where the newer host glibc could otherwise leak a symbol floor above the
# wheel's advertised one.
#
# Usage: MAX_GLIBC_VERSION=2.28 build-odbc-driver-only.sh <glibc|musl>
# Produces: $ODBC_DROP_DIR/build/mssqlodbc.so

set -eu

LIBC="${1:-glibc}"
DROP_DIR="${ODBC_DROP_DIR:-/workspace/odbc-drop}"
MAX_GLIBC_VERSION="${MAX_GLIBC_VERSION:-2.34}"

# Clean the shared drop as root inside the container; a prior pass may have left
# it root-owned, where a host-side rm can hit permission denied.
rm -rf "$DROP_DIR"

if [ "$LIBC" = "musl" ]; then
  # The musllinux wheel image ships Rust + openssl-dev but no C toolchain/bash.
  apk add --no-cache build-base bash >/dev/null
  # Dynamically link the musl CRT so the cdylib is a normal shared object.
  export RUSTFLAGS="-C target-feature=-crt-static"
fi

cd /workspace/mssql-odbc
cargo build --release
DRIVER="$(bash scripts/finalize-artifact.sh release)"

mkdir -p "$DROP_DIR/build"
cp -f "$DRIVER" "$DROP_DIR/build/mssqlodbc.so"

if [ "$LIBC" = "glibc" ]; then
  # Fail loudly if a base-image bump ever raises the driver's GLIBC floor above
  # the wheel it ships in. Capture readelf separately (sh has no pipefail) so a
  # failed or empty read is fatal instead of passing silently.
  syms="$(readelf -V "$DROP_DIR/build/mssqlodbc.so")" \
    || { echo "ERROR: readelf failed on the driver" >&2; exit 1; }
  max="$(printf '%s\n' "$syms" | grep -oE 'GLIBC_[0-9.]+' | sort -V | tail -1)"
  [ -n "$max" ] || { echo "ERROR: no GLIBC version needs found in the driver" >&2; exit 1; }
  echo "max required GLIBC: $max"
  ceiling="GLIBC_$MAX_GLIBC_VERSION"
  if [ "$(printf '%s\n%s\n' "$max" "$ceiling" | sort -V | tail -1)" != "$ceiling" ]; then
    echo "ERROR: $max exceeds the $ceiling floor" >&2
    exit 1
  fi
fi

echo "Staged driver: $DROP_DIR/build/mssqlodbc.so"
