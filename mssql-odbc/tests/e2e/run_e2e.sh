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
# Usage:
#   ./run_e2e.sh [--release] [--verbose] [--retries=N]
#                [--compare-with-msodbcsql] [--msodbcsql-ini=PATH]
#
# Default: runs the e2e suite against mssql-odbc only.
#
# --retries=N reruns each failing test up to N extra times (ctest
# --repeat until-pass:N+1). A test that passes on any attempt counts as a
# pass; the suite only fails if a test still fails after all retries.
#
# With --compare-with-msodbcsql, the script reruns the same suite against
# the Microsoft C++ driver registered in --msodbcsql-ini (default
# /etc/odbcinst.ini) and prints a parity table. The script exits 0 only
# if BOTH runs pass.
#
# Both INIs must register the driver under the same section name:
#   [ODBC Driver 18 for SQL Server]
#
# Rust driver logs are controlled by MSSQL_TDS_TRACE and
# MSSQL_TDS_TRACE_LEVEL.
# In --verbose mode, this script defaults to:
#   MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL=warn,msodbcsql18=debug
# unless they are already set in the environment.
#
# Examples:
#   ./run_e2e.sh --verbose
#   ./run_e2e.sh --compare-with-msodbcsql
#   ./run_e2e.sh --compare-with-msodbcsql --msodbcsql-ini=/opt/msodbcsql/odbcinst.ini

set -euo pipefail

# ----------------------------------------------------------------------------
# Globals
# ----------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"
DRIVER_SECTION="ODBC Driver 18 for SQL Server"

BUILD_TYPE="debug"
VERBOSE=0
COMPARE=0
RETRIES=0
MSODBCSQL_INI="/etc/odbcinst.ini"

CTEST_ARGS=(--output-on-failure)
RUST_INI_DIR=""    # tempdir holding our generated odbcinst.ini
RUST_DRIVER_PATH=""

# ----------------------------------------------------------------------------
# CLI parsing / help
# ----------------------------------------------------------------------------
usage() {
    sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
}

parse_args() {
    for arg in "$@"; do
        case "$arg" in
            --release) BUILD_TYPE="release" ;;
            --verbose)
                VERBOSE=1
                CTEST_ARGS=(-V --output-on-failure)
                ;;
            --compare-with-msodbcsql) COMPARE=1 ;;
            --retries=*) RETRIES="${arg#--retries=}" ;;
            --msodbcsql-ini=*) MSODBCSQL_INI="${arg#--msodbcsql-ini=}" ;;
            -h|--help) usage; exit 0 ;;
            *) echo "Unknown argument: $arg" >&2; usage >&2; exit 2 ;;
        esac
    done
}

# ----------------------------------------------------------------------------
# Cleanup
# ----------------------------------------------------------------------------
cleanup() {
    if [ -n "$RUST_INI_DIR" ] && [ -d "$RUST_INI_DIR" ]; then
        rm -rf "$RUST_INI_DIR"
    fi
}
trap cleanup EXIT

# ----------------------------------------------------------------------------
# Step 1: Configure tracing for verbose mode
# ----------------------------------------------------------------------------
setup_tracing() {
    if [ "$VERBOSE" -eq 1 ]; then
        export MSSQL_TDS_TRACE="${MSSQL_TDS_TRACE:-true}"
        export MSSQL_TDS_TRACE_LEVEL="${MSSQL_TDS_TRACE_LEVEL:-warn,msodbcsql18=debug}"
        echo "Verbose: MSSQL_TDS_TRACE=$MSSQL_TDS_TRACE MSSQL_TDS_TRACE_LEVEL=$MSSQL_TDS_TRACE_LEVEL"
    fi
}

# ----------------------------------------------------------------------------
# Step 2: Build the Rust driver and resolve its shared library path
# ----------------------------------------------------------------------------
build_rust_driver() {
    echo "=== Building mssql-odbc ($BUILD_TYPE) ==="
    (
        cd "$ODBC_CRATE_DIR"
        if [ "$BUILD_TYPE" = "release" ]; then
            cargo build --release
        else
            cargo build
        fi
    )

    # Cargo builds into the workspace root's target/ directory, which may
    # differ from the crate-local directory. Use `cargo metadata` to resolve.
    local target_dir
    target_dir="$(cd "$ODBC_CRATE_DIR" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
        | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null \
        || echo "$ODBC_CRATE_DIR/target")"

    if [[ "$(uname -s)" == "Darwin" ]]; then
        RUST_DRIVER_PATH="$target_dir/$BUILD_TYPE/libmsodbcsql18.dylib"
    else
        RUST_DRIVER_PATH="$target_dir/$BUILD_TYPE/libmsodbcsql18.so"
    fi

    if [ ! -f "$RUST_DRIVER_PATH" ]; then
        echo "Error: Rust driver not found at $RUST_DRIVER_PATH" >&2
        exit 1
    fi
    echo "Rust driver: $RUST_DRIVER_PATH"
}

# ----------------------------------------------------------------------------
# Step 3: Register the Rust driver in a temporary odbcinst.ini
# ----------------------------------------------------------------------------
register_rust_driver() {
    RUST_INI_DIR="$(mktemp -d)"
    cat > "$RUST_INI_DIR/odbcinst.ini" <<EOF
[$DRIVER_SECTION]
Description=Microsoft ODBC Driver 18 for SQL Server (Rust)
Driver=$RUST_DRIVER_PATH
UsageCount=1
EOF
    echo "Rust driver registered at: $RUST_INI_DIR/odbcinst.ini"
}

# ----------------------------------------------------------------------------
# Step 4: Validate the msodbcsql odbcinst.ini for the comparison run
# ----------------------------------------------------------------------------
validate_msodbcsql_ini() {
    if [ ! -f "$MSODBCSQL_INI" ]; then
        echo "Error: msodbcsql odbcinst.ini not found: $MSODBCSQL_INI" >&2
        echo "  Pass --msodbcsql-ini=PATH or install the C++ driver." >&2
        exit 1
    fi
    if ! grep -qE "^\[$DRIVER_SECTION\]" "$MSODBCSQL_INI"; then
        echo "Error: $MSODBCSQL_INI does not contain a [$DRIVER_SECTION] section." >&2
        echo "  Both INIs must register the driver under the same name." >&2
        exit 1
    fi
    # Resolve to absolute path so subprocesses see the same file regardless of cwd.
    MSODBCSQL_INI="$(cd "$(dirname "$MSODBCSQL_INI")" && pwd)/$(basename "$MSODBCSQL_INI")"
    echo "msodbcsql ini: $MSODBCSQL_INI"
}

# ----------------------------------------------------------------------------
# Step 5: Auto-detect dev SQL Server credentials (localhost:1433, sa)
# ----------------------------------------------------------------------------
setup_dev_sql_env() {
    if [ -n "${ODBC_TEST_SERVER:-}" ]; then
        return
    fi
    if ! { (echo >/dev/tcp/localhost/1433) 2>/dev/null || nc -z localhost 1433 2>/dev/null; }; then
        return
    fi

    export ODBC_TEST_SERVER="localhost"
    export ODBC_TEST_UID="${ODBC_TEST_UID:-sa}"
    export ODBC_TEST_TRUST_CERT="${ODBC_TEST_TRUST_CERT:-Yes}"

    if [ -z "${ODBC_TEST_PWD:-}" ]; then
        if [ -n "${SQL_PASSWORD:-}" ]; then
            export ODBC_TEST_PWD="$SQL_PASSWORD"
        elif [ -f "$ODBC_CRATE_DIR/../mssql-tds/.env" ]; then
            local _pwd
            _pwd=$(grep -m1 '^SQL_PASSWORD=' "$ODBC_CRATE_DIR/../mssql-tds/.env" | cut -d= -f2-)
            if [ -n "$_pwd" ]; then
                export ODBC_TEST_PWD="$_pwd"
            fi
        fi
    fi

    if [ -n "${ODBC_TEST_PWD:-}" ]; then
        echo "Auto-detected dev SQL Server at localhost:1433 (sa login)"
    else
        echo "Warning: SQL Server detected on localhost:1433 but no password found."
        echo "  Set SQL_PASSWORD, ODBC_TEST_PWD, or run dev/dev-launchsql.sh first."
    fi
}

# ----------------------------------------------------------------------------
# Step 6: Configure + build the C++ test binaries (once, shared by both runs)
# ----------------------------------------------------------------------------
configure_and_build_tests() {
    echo ""
    echo "=== Configuring e2e tests (CMake) ==="
    # Linux/macOS default to platform TCHAR mode unless explicitly overridden.
    cmake -S "$SCRIPT_DIR" -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Debug \
        -DODBC_E2E_FORCE_UNICODE="${ODBC_E2E_FORCE_UNICODE:-OFF}"

    echo ""
    echo "=== Building e2e tests ==="
    cmake --build "$BUILD_DIR" \
        -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
}

# ----------------------------------------------------------------------------
# Step 7: Run ctest with a given driver pointed at by ODBCSYSINI.
#   $1 = label (printed in headers)
#   $2 = ODBCSYSINI directory
#   $3 = absolute path to JUnit XML output
# Returns ctest's exit code (does not abort the script).
# ----------------------------------------------------------------------------
run_tests() {
    local label="$1" ini_dir="$2" junit_out="$3"
    echo ""
    echo "=== Running e2e tests against $label ==="
    echo "ODBCSYSINI=$ini_dir"

    local rc=0
    (
        cd "$BUILD_DIR"
        # ODBC_TEST_TARGET tells tests which driver implementation this leg runs
        # against ("mssql-odbc" or "msodbcsql") so mssql-odbc-specific tests can
        # SKIP_IF_COMPARING_MSODBCSQL() on the reference-driver leg.
        ODBC_TEST_TARGET="$label" ODBCSYSINI="$ini_dir" ctest "${CTEST_ARGS[@]}" --output-junit "$junit_out"
    ) || rc=$?
    return $rc
}

# ----------------------------------------------------------------------------
# Step 8: Parse two JUnit XMLs and print a parity table.
#   $1 = mssql-odbc JUnit XML
#   $2 = msodbcsql  JUnit XML
# Always returns 0 — exit code is decided by the caller.
# ----------------------------------------------------------------------------
print_parity_report() {
    local rust_xml="$1" ms_xml="$2"
    python3 - "$rust_xml" "$ms_xml" <<'PY'
import sys, xml.etree.ElementTree as ET

def load(path):
    """Returns {test_name: 'PASS'|'FAIL'|'MISSING'}."""
    out = {}
    try:
        root = ET.parse(path).getroot()
    except (ET.ParseError, FileNotFoundError):
        return out
    # ctest --output-junit emits <testsuite><testcase name="..."> ...
    for tc in root.iter("testcase"):
        name = tc.get("name") or "<unnamed>"
        failed = any(child.tag in ("failure", "error") for child in tc)
        out[name] = "FAIL" if failed else "PASS"
    return out

rust = load(sys.argv[1])
ms   = load(sys.argv[2])
names = sorted(set(rust) | set(ms))

def verdict(r, m):
    if r == "PASS" and m == "PASS": return ("parity",          "ok")
    if r == "FAIL" and m == "PASS": return ("FIX mssql-odbc",  "bug")
    if r == "PASS" and m == "FAIL": return ("mssql-odbc bug, but test hides it (msodbcsql fails)", "warn")
    if r == "FAIL" and m == "FAIL": return ("test bug (both fail)",       "warn")
    return ("missing run", "warn")

w = max((len(n) for n in names), default=4)
print()
print("=== Parity report (mssql-odbc vs msodbcsql) ===")
print(f"{'Test'.ljust(w)}  {'mssql-odbc':<10}  {'msodbcsql':<10}  Verdict")
print(f"{'-'*w}  {'-'*10}  {'-'*10}  {'-'*30}")
counts = {"ok":0, "bug":0, "warn":0}
for n in names:
    r = rust.get(n, "MISSING")
    m = ms.get(n, "MISSING")
    v, kind = verdict(r, m)
    counts[kind] += 1
    print(f"{n.ljust(w)}  {r:<10}  {m:<10}  {v}")
print()
print(f"Summary: {counts['ok']} parity, {counts['bug']} mssql-odbc bug(s), {counts['warn']} test issue(s)")
PY
}

# ----------------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------------
main() {
    parse_args "$@"
    if [ "$RETRIES" -gt 0 ] 2>/dev/null; then
        # until-pass:N runs a failing test up to N times total, so N retries = N+1.
        CTEST_ARGS+=(--repeat "until-pass:$((RETRIES + 1))")
        echo "Retries enabled: each failing test reruns up to $RETRIES time(s)."
    fi
    setup_tracing
    setup_dev_sql_env
    build_rust_driver
    register_rust_driver
    if [ "$COMPARE" -eq 1 ]; then
        validate_msodbcsql_ini
    fi
    configure_and_build_tests

    local rust_junit="$BUILD_DIR/junit-mssql-odbc.xml"
    local ms_junit="$BUILD_DIR/junit-msodbcsql.xml"
    local rust_rc=0 ms_rc=0

    run_tests "mssql-odbc" "$RUST_INI_DIR" "$rust_junit" || rust_rc=$?

    if [ "$COMPARE" -eq 0 ]; then
        if [ "$rust_rc" -ne 0 ]; then
            echo "=== e2e tests FAILED (mssql-odbc) ==="
            exit "$rust_rc"
        fi
        echo ""
        echo "=== e2e tests passed ==="
        exit 0
    fi

    # Comparison mode: also run against the C++ driver, then print the table.
    local ms_ini_dir
    ms_ini_dir="$(dirname "$MSODBCSQL_INI")"
    run_tests "msodbcsql"  "$ms_ini_dir"  "$ms_junit" || ms_rc=$?

    print_parity_report "$rust_junit" "$ms_junit"

    if [ "$rust_rc" -eq 0 ] && [ "$ms_rc" -eq 0 ]; then
        echo "=== Both runs passed ==="
        exit 0
    fi
    echo "=== Parity check FAILED (mssql-odbc rc=$rust_rc, msodbcsql rc=$ms_rc) ==="
    exit 1
}

main "$@"
