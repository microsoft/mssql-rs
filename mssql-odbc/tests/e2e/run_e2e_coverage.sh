#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Measure Rust code coverage of the mssql-odbc driver (the msodbcsql18 cdylib)
# as exercised by the C++ gtest e2e suite.
#
# cargo llvm-cov nextest cannot be used here: the gtest binaries are separate
# C++ processes that dlopen our cdylib through the unixODBC Driver Manager, so
# coverage must be collected from an externally-loaded, instrumented library.
# This uses cargo-llvm-cov's external-process (show-env) support:
#
#   1. cargo llvm-cov clean --workspace
#   2. eval "$(cargo llvm-cov show-env --export-prefix)"  # instrument-coverage env
#   3. cargo build -p mssql-odbc                          # instrumented cdylib
#   4. LLVM_PROFILE_FILE=<abs>/%p-%m.profraw so each forked gtest writes its own
#   5. register the instrumented .so + run the gtest suite via CTest
#   6. cargo llvm-cov report --package mssql-odbc         # scoped to the crate
#
# For Unix-like platforms (Linux, macOS) that use unixODBC. Connected tests
# only run against a live SQL Server (auto-detected on localhost:1433, or set
# ODBC_TEST_SERVER/ODBC_TEST_UID/ODBC_TEST_PWD); without one, most of the suite
# skips and the measured coverage is correspondingly low.
#
# Usage:
#   ./run_e2e_coverage.sh [--release] [--verbose] [--retries=N] [--html]
#
# Outputs (under the workspace target/ directory):
#   odbc-e2e-cobertura.xml        Cobertura report (mssql-odbc only)
#   odbc-e2e-coverage-summary.txt llvm-cov text summary
#   e2e-coverage-html/            HTML report (only with --html)

set -euo pipefail

# ----------------------------------------------------------------------------
# Globals
# ----------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
REPO_ROOT="$(cd "$ODBC_CRATE_DIR/.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"
DRIVER_SECTION="ODBC Driver 18 for SQL Server"

BUILD_TYPE="debug"
VERBOSE=0
RETRIES=0
EMIT_HTML=0

CTEST_ARGS=(--output-on-failure)
RUST_INI_DIR=""       # tempdir holding our generated odbcinst.ini
RUST_DRIVER_PATH=""
TARGET_DIR=""         # workspace target dir (from cargo llvm-cov show-env)

# ----------------------------------------------------------------------------
# CLI parsing / help
# ----------------------------------------------------------------------------
usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

parse_args() {
    for arg in "$@"; do
        case "$arg" in
            --release) BUILD_TYPE="release" ;;
            --verbose)
                VERBOSE=1
                CTEST_ARGS=(-V --output-on-failure)
                ;;
            --retries=*) RETRIES="${arg#--retries=}" ;;
            --html) EMIT_HTML=1 ;;
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
# Step 2: Auto-detect dev SQL Server credentials (localhost:1433, sa)
# ----------------------------------------------------------------------------
setup_dev_sql_env() {
    if [ -n "${ODBC_TEST_SERVER:-}" ]; then
        return
    fi
    if ! { (echo >/dev/tcp/localhost/1433) 2>/dev/null || nc -z localhost 1433 2>/dev/null; }; then
        echo "Warning: no SQL Server on localhost:1433 and ODBC_TEST_SERVER unset."
        echo "  Connected tests will skip; coverage will reflect the offline paths only."
        echo "  Run dev/dev-launchsql.sh (or set ODBC_TEST_*) for meaningful coverage."
        return
    fi

    export ODBC_TEST_SERVER="localhost"
    export ODBC_TEST_UID="${ODBC_TEST_UID:-sa}"
    export ODBC_TEST_TRUST_CERT="${ODBC_TEST_TRUST_CERT:-Yes}"

    if [ -z "${ODBC_TEST_PWD:-}" ]; then
        if [ -n "${SQL_PASSWORD:-}" ]; then
            export ODBC_TEST_PWD="$SQL_PASSWORD"
        elif [ -f "$REPO_ROOT/mssql-tds/.env" ]; then
            local _pwd
            _pwd=$(grep -m1 '^SQL_PASSWORD=' "$REPO_ROOT/mssql-tds/.env" | cut -d= -f2-)
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
# Step 3: Reset counters and export the instrument-coverage environment.
# show-env sets RUSTC_WRAPPER, the profile target dir, and a default
# LLVM_PROFILE_FILE. We override the latter with a %p-%m pattern so each of the
# many forked gtest processes writes a distinct .profraw. The files must land in
# the target-dir root: `cargo llvm-cov report` scans it non-recursively.
# ----------------------------------------------------------------------------
setup_coverage_env() {
    echo "=== Resetting coverage state (cargo llvm-cov clean) ==="
    (cd "$REPO_ROOT" && cargo llvm-cov clean --workspace)

    echo "=== Exporting instrument-coverage environment ==="
    # shellcheck disable=SC1090
    eval "$(cd "$REPO_ROOT" && cargo llvm-cov show-env --export-prefix)"

    TARGET_DIR="${CARGO_LLVM_COV_TARGET_DIR:-$REPO_ROOT/target}"
    rm -f "$TARGET_DIR"/e2e-*.profraw
    export LLVM_PROFILE_FILE="$TARGET_DIR/e2e-%p-%m.profraw"
    echo "LLVM_PROFILE_FILE=$LLVM_PROFILE_FILE"
}

# ----------------------------------------------------------------------------
# Step 4: Build the instrumented Rust driver and resolve its shared library.
# ----------------------------------------------------------------------------
build_rust_driver() {
    echo "=== Building instrumented mssql-odbc ($BUILD_TYPE) ==="
    (
        cd "$REPO_ROOT"
        if [ "$BUILD_TYPE" = "release" ]; then
            cargo build --release -p mssql-odbc
        else
            cargo build -p mssql-odbc
        fi
    )

    if [[ "$(uname -s)" == "Darwin" ]]; then
        RUST_DRIVER_PATH="$TARGET_DIR/$BUILD_TYPE/libmsodbcsql18.dylib"
    else
        RUST_DRIVER_PATH="$TARGET_DIR/$BUILD_TYPE/libmsodbcsql18.so"
    fi

    if [ ! -f "$RUST_DRIVER_PATH" ]; then
        echo "Error: Rust driver not found at $RUST_DRIVER_PATH" >&2
        exit 1
    fi
    echo "Instrumented driver: $RUST_DRIVER_PATH"
}

# ----------------------------------------------------------------------------
# Step 5: Register the instrumented driver in a temporary odbcinst.ini
# ----------------------------------------------------------------------------
register_rust_driver() {
    RUST_INI_DIR="$(mktemp -d)"
    cat > "$RUST_INI_DIR/odbcinst.ini" <<EOF
[$DRIVER_SECTION]
Description=Microsoft ODBC Driver 18 for SQL Server (Rust, instrumented)
Driver=$RUST_DRIVER_PATH
UsageCount=1
EOF
    echo "Instrumented driver registered at: $RUST_INI_DIR/odbcinst.ini"
}

# ----------------------------------------------------------------------------
# Step 6: Configure + build the C++ test binaries
# ----------------------------------------------------------------------------
configure_and_build_tests() {
    echo ""
    echo "=== Configuring e2e tests (CMake) ==="
    cmake -S "$SCRIPT_DIR" -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Debug

    echo ""
    echo "=== Building e2e tests ==="
    cmake --build "$BUILD_DIR" \
        -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
}

# ----------------------------------------------------------------------------
# Step 7: Run the gtest suite via CTest against the instrumented driver.
# LLVM_PROFILE_FILE is already exported, so every forked gtest process emits a
# .profraw. Returns ctest's exit code (does not abort the script).
# ----------------------------------------------------------------------------
run_tests() {
    local junit_out="$BUILD_DIR/junit-mssql-odbc.xml"
    echo ""
    echo "=== Running e2e tests against instrumented mssql-odbc ==="
    echo "ODBCSYSINI=$RUST_INI_DIR"

    local rc=0
    (
        cd "$BUILD_DIR"
        ODBCSYSINI="$RUST_INI_DIR" ctest "${CTEST_ARGS[@]}" --output-junit "$junit_out"
    ) || rc=$?
    return $rc
}

# ----------------------------------------------------------------------------
# Step 8: Generate the mssql-odbc coverage report from the collected profraw.
# ----------------------------------------------------------------------------
generate_report() {
    local cobertura="$TARGET_DIR/odbc-e2e-cobertura.xml"
    local summary="$TARGET_DIR/odbc-e2e-coverage-summary.txt"

    local profcount
    profcount=$(find "$TARGET_DIR" -maxdepth 1 -name 'e2e-*.profraw' 2>/dev/null | wc -l)
    echo ""
    echo "=== Generating coverage report ($profcount profraw files) ==="
    if [ "$profcount" -eq 0 ]; then
        echo "Error: no .profraw files were produced. Did the instrumented driver load?" >&2
        exit 1
    fi

    (cd "$REPO_ROOT" && cargo llvm-cov report --package mssql-odbc \
        --cobertura --output-path "$cobertura")

    (cd "$REPO_ROOT" && cargo llvm-cov report --package mssql-odbc \
        --summary-only 2>/dev/null | tee "$summary")

    if [ "$EMIT_HTML" -eq 1 ]; then
        (cd "$REPO_ROOT" && cargo llvm-cov report --package mssql-odbc \
            --html --output-dir "$TARGET_DIR/e2e-coverage-html")
        echo "HTML report: $TARGET_DIR/e2e-coverage-html/index.html"
    fi

    echo ""
    echo "Cobertura: $cobertura"
    echo "Summary:   $summary"
}

# ----------------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------------
main() {
    parse_args "$@"
    if [ "$RETRIES" -gt 0 ] 2>/dev/null; then
        CTEST_ARGS+=(--repeat "until-pass:$((RETRIES + 1))")
        echo "Retries enabled: each failing test reruns up to $RETRIES time(s)."
    fi
    setup_tracing
    setup_dev_sql_env
    setup_coverage_env
    build_rust_driver
    register_rust_driver
    configure_and_build_tests

    local rc=0
    run_tests || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "Warning: some e2e tests failed (rc=$rc); reporting coverage anyway." >&2
    fi

    generate_report
    echo ""
    echo "=== e2e coverage run complete ==="
    exit "$rc"
}

main "$@"
