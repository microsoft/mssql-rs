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
# Usage: ./run_e2e.sh [--release] [--verbose]
#
# Rust driver logs are controlled by MSSQL_TDS_TRACE and
# MSSQL_TDS_TRACE_LEVEL.
# In --verbose mode, this script defaults to:
#   MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL=warn,msodbcsql18=debug
# unless they are already set in the environment.
#
# Examples:
#   ./run_e2e.sh --verbose
#   MSSQL_TDS_TRACE_LEVEL=warn,msodbcsql18=trace ./run_e2e.sh --verbose

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_TYPE="debug"
TMPDIR_INI=""
CTEST_ARGS=(--output-on-failure)
VERBOSE=0

for arg in "$@"; do
    case $arg in
        --release) BUILD_TYPE="release" ;;
        --verbose)
            VERBOSE=1
            CTEST_ARGS=(-V --output-on-failure)
            ;;
    esac
done

if [ "$VERBOSE" -eq 1 ]; then
    export MSSQL_TDS_TRACE="${MSSQL_TDS_TRACE:-true}"
    export MSSQL_TDS_TRACE_LEVEL="${MSSQL_TDS_TRACE_LEVEL:-warn,msodbcsql18=debug}"
    echo "Verbose mode: MSSQL_TDS_TRACE=$MSSQL_TDS_TRACE MSSQL_TDS_TRACE_LEVEL=$MSSQL_TDS_TRACE_LEVEL"
fi

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

# Determine shared library path.
# Cargo builds into the workspace root's target/ directory, which may differ
# from the crate-local directory.  Use `cargo metadata` to resolve it.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null \
    || echo "$ODBC_CRATE_DIR/target")"

if [[ "$(uname -s)" == "Darwin" ]]; then
    DRIVER_PATH="$TARGET_DIR/$BUILD_TYPE/libmsodbcsql18.dylib"
else
    DRIVER_PATH="$TARGET_DIR/$BUILD_TYPE/libmsodbcsql18.so"
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

# ---------------------------------------------------------------------------
# Auto-detect dev SQL Server launched by dev/dev-launchsql.sh
# If ODBC_TEST_SERVER is not set, check if localhost:1433 is reachable and
# pull credentials from the dev environment (SQL_PASSWORD or mssql-tds/.env).
# ---------------------------------------------------------------------------
if [ -z "$ODBC_TEST_SERVER" ]; then
    # Check if SQL Server is listening on localhost:1433
    if (echo >/dev/tcp/localhost/1433) 2>/dev/null || nc -z localhost 1433 2>/dev/null; then
        export ODBC_TEST_SERVER="localhost"
        export ODBC_TEST_UID="${ODBC_TEST_UID:-sa}"
        export ODBC_TEST_TRUST_CERT="${ODBC_TEST_TRUST_CERT:-Yes}"

        # Resolve password: prefer ODBC_TEST_PWD > SQL_PASSWORD > mssql-tds/.env
        if [ -z "$ODBC_TEST_PWD" ]; then
            if [ -n "$SQL_PASSWORD" ]; then
                export ODBC_TEST_PWD="$SQL_PASSWORD"
            elif [ -f "$ODBC_CRATE_DIR/../mssql-tds/.env" ]; then
                _pwd=$(grep -m1 '^SQL_PASSWORD=' "$ODBC_CRATE_DIR/../mssql-tds/.env" | cut -d= -f2-)
                if [ -n "$_pwd" ]; then
                    export ODBC_TEST_PWD="$_pwd"
                fi
            fi
        fi

        if [ -n "$ODBC_TEST_PWD" ]; then
            echo "Auto-detected dev SQL Server at localhost:1433 (sa login)"
        else
            echo "Warning: SQL Server detected on localhost:1433 but no password found."
            echo "  Set SQL_PASSWORD, ODBC_TEST_PWD, or run dev/dev-launchsql.sh first."
        fi
    fi
fi

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
ctest "${CTEST_ARGS[@]}"

echo ""
echo "=== e2e tests passed ==="
