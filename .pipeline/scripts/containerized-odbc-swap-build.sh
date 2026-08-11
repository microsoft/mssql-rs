#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build just the mssql-odbc cdylib (libmsodbcsql18.so) inside the Linux build
# container and stage it into a drop directory, for the job that swaps it in for
# the driver mssql-python bundles (see swap-mssql-python-odbc-driver.sh).
#
# This is deliberately narrower than containerized-odbc-build.sh: that script
# also builds the C++ gtest e2e binaries and so needs cmake and the unixODBC dev
# headers. Here only the Rust cdylib is wanted, which needs neither.
#
# Env:
#   ODBC_DROP_DIR     Drop directory to stage into (default: /workspace/odbc-swap-drop).
#   CARGO_TARGET_DIR  Cargo target directory, if the job overrides it.

set -euo pipefail

source ~/.cargo/env

DROP_DIR="${ODBC_DROP_DIR:-/workspace/odbc-swap-drop}"

# The container runs as root, so anything it wrote on a previous run is
# root-owned on the agent and cannot be cleaned host-side without sudo.
rm -rf "$DROP_DIR"
mkdir -p "$DROP_DIR"

cargo build --release -p mssql-odbc

# Honour CARGO_TARGET_DIR the way the sibling build jobs in this stage set it, so
# adding it here later cannot break the copy after a full release build.
TARGET_DIR="${CARGO_TARGET_DIR:-/workspace/target}"

cp "$TARGET_DIR/release/libmsodbcsql18.so" "$DROP_DIR/"
ls -la "$DROP_DIR"
