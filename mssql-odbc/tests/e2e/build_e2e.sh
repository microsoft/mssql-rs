#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the Rust ODBC driver and the C++ gtest e2e binaries, then stage them
# into a drop directory so they can be published as a CI artifact and rerun
# later on other machines/containers via `run_e2e.sh --skip-build`.
#
# This is the build half of the split e2e flow; run_e2e.sh is the run half.
#
# Usage:
#   ./build_e2e.sh [--release] [--out=DIR]
#
# --release   Build the driver in release mode (default: debug).
# --out=DIR   Stage the driver .so and the CMake build/ tree into DIR for
#             artifact publishing. The build always happens in-place at
#             tests/e2e/build regardless of --out.
#
# The CMake build/ tree embeds ABSOLUTE paths to the test executables in
# CTestTestfile.cmake. To rerun the prebuilt binaries, the consumer must place
# build/ back at the SAME absolute path it was built at (in CI both the build
# and test jobs mount the repo at /workspace, so the paths line up).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"

BUILD_TYPE="debug"
OUT_DIR=""

for arg in "$@"; do
    case "$arg" in
        --release) BUILD_TYPE="release" ;;
        --out=*) OUT_DIR="${arg#--out=}" ;;
        -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# ----------------------------------------------------------------------------
# Resolve the driver shared-library filename for this platform.
# ----------------------------------------------------------------------------
if [[ "$(uname -s)" == "Darwin" ]]; then
    DRIVER_FILE="libmsodbcsql18.dylib"
else
    DRIVER_FILE="libmsodbcsql18.so"
fi

# ----------------------------------------------------------------------------
# Step 1: Build the Rust driver.
# ----------------------------------------------------------------------------
echo "=== Building mssql-odbc driver ($BUILD_TYPE) ==="
(
    cd "$ODBC_CRATE_DIR"
    if [ "$BUILD_TYPE" = "release" ]; then
        cargo build --release
    else
        cargo build
    fi
)

# Resolve the Cargo target dir (workspace-level for a workspace member) without
# depending on python3/jq, which the musl (Alpine) build image doesn't ship.
TARGET_DIR="$(cd "$ODBC_CRATE_DIR" \
    && cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | grep -o '"target_directory":"[^"]*"' | head -n1 \
    | sed 's/^"target_directory":"//; s/"$//' || true)"
if [ -z "$TARGET_DIR" ]; then
    # Fallback: workspace-root target first, then a standalone crate build.
    for cand in "$ODBC_CRATE_DIR/../target" "$ODBC_CRATE_DIR/target"; do
        if [ -f "$cand/$BUILD_TYPE/$DRIVER_FILE" ]; then TARGET_DIR="$cand"; break; fi
    done
fi
DRIVER_PATH="$TARGET_DIR/$BUILD_TYPE/$DRIVER_FILE"

if [ ! -f "$DRIVER_PATH" ]; then
    echo "Error: Rust driver not found at $DRIVER_PATH" >&2
    exit 1
fi
echo "Driver: $DRIVER_PATH"

# ----------------------------------------------------------------------------
# Step 2: Configure + build the C++ gtest e2e binaries.
# ----------------------------------------------------------------------------
echo ""
echo "=== Configuring e2e tests (CMake) ==="
cmake -S "$SCRIPT_DIR" -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Debug

echo ""
echo "=== Building e2e tests ==="
cmake --build "$BUILD_DIR" \
    -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"

# ----------------------------------------------------------------------------
# Step 3: Stage into the drop directory (optional).
# ----------------------------------------------------------------------------
if [ -n "$OUT_DIR" ]; then
    echo ""
    echo "=== Staging drop into $OUT_DIR ==="
    # Copy the driver into the build tree so the whole drop is a single,
    # path-consistent directory. run_e2e.sh --skip-build resolves the driver
    # from build/<name> automatically.
    cp "$DRIVER_PATH" "$BUILD_DIR/$DRIVER_FILE"
    mkdir -p "$OUT_DIR"
    rm -rf "$OUT_DIR/build"
    cp -r "$BUILD_DIR" "$OUT_DIR/build"
    echo "Staged build/ (with $DRIVER_FILE) into $OUT_DIR"
fi

echo ""
echo "=== e2e build complete ==="
