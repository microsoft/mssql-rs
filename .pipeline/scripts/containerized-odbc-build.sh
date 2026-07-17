#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the mssql-odbc driver (libmsodbcsql18.so) and the C++ gtest e2e binaries
# inside a Linux build container, then stage them into a drop directory that the
# surrounding pipeline job publishes as an artifact. Later test stages download
# the drop and rerun the prebuilt binaries via run_e2e.sh --skip-build.
#
# The Ubuntu build image (containers/Dockerfile.Ubuntu.Build) ships gcc/g++/make/
# git/cargo but not cmake or the unixODBC dev headers the C++ Driver Manager
# links against, so install those before building.
#
# Env:
#   ODBC_DROP_DIR   Drop directory to stage into (default: /workspace/odbc-drop).

set -euo pipefail

source ~/.cargo/env

DROP_DIR="${ODBC_DROP_DIR:-/workspace/odbc-drop}"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends cmake unixodbc-dev
rm -rf /var/lib/apt/lists/*

exec /workspace/mssql-odbc/tests/e2e/build_e2e.sh --release --out="$DROP_DIR"
