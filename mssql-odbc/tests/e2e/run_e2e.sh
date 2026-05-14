#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the Rust ODBC driver, register it with a temporary odbcinst.ini,
# and run C++ gtest e2e tests against it via the unixODBC Driver Manager.
#
# For Unix-like platforms (Linux, macOS) that use unixODBC.
# For Windows, see run_e2e.ps1 (uses the Windows registry instead).
#
# Usage: ./run_e2e.sh [--release]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_TYPE="debug"
TMPDIR_INI=""

for arg in "$@"; do
    case $arg in
        --release) BUILD_TYPE="release" ;;
    esac
done

cleanup() {
    if [ -n "$TMPDIR_INI" ] && [ -d "$TMPDIR_INI" ]; then
        rm -rf "$TMPDIR_INI"
    fi
}
trap cleanup EXIT

echo "=== Building mssql-odbc ($BUILD_TYPE) ==="
cd "$ODBC_CRATE_DIR"
if [ "$BUILD_TYPE" = "release" ]; then
    cargo build --release
else
    cargo build
fi

# Determine shared library path
if [[ "$(uname -s)" == "Darwin" ]]; then
    DRIVER_PATH="$ODBC_CRATE_DIR/target/$BUILD_TYPE/libmsodbcsql18.dylib"
else
    DRIVER_PATH="$ODBC_CRATE_DIR/target/$BUILD_TYPE/libmsodbcsql18.so"
fi

if [ ! -f "$DRIVER_PATH" ]; then
    echo "Error: Driver not found at $DRIVER_PATH"
    exit 1
fi
echo "Driver: $DRIVER_PATH"

# Register the driver via a temporary odbcinst.ini so the Driver Manager
# can find it without system-wide installation.
TMPDIR_INI="$(mktemp -d)"
cat > "$TMPDIR_INI/odbcinst.ini" <<EOF
[ODBC Driver 18 for SQL Server]
Description=Microsoft ODBC Driver 18 for SQL Server (Rust)
Driver=$DRIVER_PATH
UsageCount=1
EOF
# No need to save/restore ODBCSYSINI — this export only affects the script's
# process and its children, not the parent shell.
export ODBCSYSINI="$TMPDIR_INI"
echo "ODBCSYSINI=$ODBCSYSINI"

echo ""
echo "=== Configuring e2e tests (CMake) ==="
cd "$SCRIPT_DIR"
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug

echo ""
echo "=== Building e2e tests ==="
cmake --build build -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"

echo ""
echo "=== Running e2e tests ==="
cd build
ctest --output-on-failure

echo ""
echo "=== e2e tests passed ==="
