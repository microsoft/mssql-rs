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
#   ./run_e2e.sh [--release] [--verbose] [--retries=N] [--coverage[=OUTPUT]]
#                [--compare-with-msodbcsql] [--msodbcsql-ini=PATH]
#                [--skip-build] [--driver=PATH]
#
# Default: runs the e2e suite against mssql-odbc only.
#
# --skip-build reuses a driver and CMake `build/` produced earlier by
# build_e2e.sh (no cargo/cmake needed — only the unixODBC runtime). Combine
# with --driver=PATH to point at the prebuilt mssql-odbc.so. This is how
# CI runs prebuilt binaries across distro containers.
#
# --retries=N reruns each failing test up to N extra times (ctest
# --repeat until-pass:N+1). A test that passes on any attempt counts as a
# pass; the suite only fails if a test still fails after all retries.
#
# --coverage[=OUTPUT] builds the Rust driver with LLVM source-based coverage
# instrumentation so the driver code exercised by the C++ tests (which load the
# .so through the Driver Manager as separate processes) is measured. A Cobertura
# report for mssql-tds + mssql-odbc is written to OUTPUT (default
# <repo>/target/cobertura-odbc-e2e.xml). Same mechanism as dev/test-python.sh
# --coverage; everything runs through cargo-llvm-cov so the LLVM version that
# reads the .profraw matches the rustc that produced the instrumented .so.
#
# With --compare-with-msodbcsql, the script reruns the same suite against
# the Microsoft C++ driver registered in --msodbcsql-ini (default
# /etc/odbcinst.ini) and prints a parity table. The script exits 0 only if both
# runs pass AND every test reaches the same verdict in both legs.
#
# The two drivers register under distinct names, so they can be installed side
# by side:
#   Rust:      [ODBC Driver 18 for SQL Server (Rust)]
#   reference: [ODBC Driver 18 for SQL Server]
# Each leg selects its driver via ODBC_TEST_DRIVER, so the same test binaries
# run unchanged against both. Setting ODBC_TEST_CONNSTR overrides the whole
# connection string and would pin both legs to one driver; the script rejects
# that in comparison mode.
#
# Rust driver logs are controlled by MSSQL_TDS_TRACE and
# MSSQL_TDS_TRACE_LEVEL.
# In --verbose mode, this script defaults to:
#   MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL=warn,mssqlodbc=debug
# unless they are already set in the environment.
#
# Examples:
#   ./run_e2e.sh --verbose
#   ./run_e2e.sh --compare-with-msodbcsql
#   ./run_e2e.sh --compare-with-msodbcsql --msodbcsql-ini=/opt/msodbcsql/odbcinst.ini

set -euo pipefail

# CI only controls how much diagnostic context is printed on failure; every
# failure mode below is fatal locally too.
IS_CI=0
if [ -n "${TF_BUILD:-}" ] || [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ]; then
    IS_CI=1
fi

# Dump the logs that explain a configure/build/test failure. CI has no working
# copy to inspect afterwards, so the evidence has to be in the build log.
print_failure_diagnostics() {
    [ "$IS_CI" -eq 1 ] || return 0
    local context="$1"
    echo ""
    echo "=== CI diagnostics: $context ==="
    local log
    for log in "$BUILD_DIR/CMakeFiles/CMakeOutput.log" \
               "$BUILD_DIR/CMakeFiles/CMakeError.log" \
               "$BUILD_DIR/Testing/Temporary/LastTest.log"; do
        if [ -f "$log" ]; then
            echo ""
            echo "--- tail of $log ---"
            tail -n 100 "$log"
        fi
    done
    echo ""
    echo "--- test executables in $BUILD_DIR ---"
    # `find` exits 0 even when it matches nothing, so `find || echo` never prints
    # the fallback. Capture the output and test it explicitly instead.
    local exes
    exes="$(find "$BUILD_DIR" -type f -name '*_test' 2>/dev/null)"
    if [ -n "$exes" ]; then echo "$exes"; else echo "(none found)"; fi
}

# A ctest run that executed nothing still exits 0 and prints "No tests were
# found!!!" — historically that turned a broken CMake configure into a green
# build. Treat an empty JUnit as a hard failure.
assert_tests_executed() {
    local junit="$1" label="$2" count=0
    if [ -f "$junit" ]; then
        # -o counts occurrences rather than matching lines, so a JUnit file that
        # puts several <testcase> elements on one line is still counted correctly.
        count=$(grep -o '<testcase' "$junit" 2>/dev/null | wc -l) || count=0
    fi
    if [ "$count" -eq 0 ]; then
        print_failure_diagnostics "no tests executed for '$label'"
        echo "Error: no tests were executed for '$label'." >&2
        echo "  The CMake project produced no ctest entries, or ctest failed to run them." >&2
        exit 1
    fi
    echo "$label leg executed $count test(s)."
}

# ----------------------------------------------------------------------------
# Globals
# ----------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"
MSODBCSQL_DRIVER_SECTION="ODBC Driver 18 for SQL Server"
RUST_DRIVER_SECTION="ODBC Driver 18 for SQL Server (Rust)"

BUILD_TYPE="debug"
VERBOSE=0
COMPARE=0
RETRIES=0
COVERAGE=0
# Default Cobertura output lives in the workspace target/ dir so the CI mount
# (-v $PWD:/workspace) exposes it to the host agent for artifact publishing.
COVERAGE_OUTPUT="${COVERAGE_OUTPUT:-$ODBC_CRATE_DIR/../target/cobertura-odbc-e2e.xml}"
MSODBCSQL_INI="/etc/odbcinst.ini"
SKIP_BUILD=0
DRIVER_OVERRIDE=""

CTEST_ARGS=(--output-on-failure)
RUST_INI_DIR=""    # tempdir holding our generated odbcinst.ini
RUST_DRIVER_PATH=""

# ----------------------------------------------------------------------------
# CLI parsing / help
# ----------------------------------------------------------------------------
usage() {
    # Print the leading comment block (line 2 to the first non-comment line),
    # stripping the leading "# ".
    awk 'NR==1{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "$0"
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
            --coverage) COVERAGE=1 ;;
            --coverage=*) COVERAGE=1; COVERAGE_OUTPUT="${arg#--coverage=}" ;;
            --msodbcsql-ini=*) MSODBCSQL_INI="${arg#--msodbcsql-ini=}" ;;
            --skip-build) SKIP_BUILD=1 ;;
            --driver=*) DRIVER_OVERRIDE="${arg#--driver=}" ;;
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
# Coverage: enable LLVM source-based instrumentation for the driver build
# ----------------------------------------------------------------------------
# `cargo llvm-cov show-env` exports RUSTFLAGS (-C instrument-coverage), the
# llvm-cov target dir and an LLVM_PROFILE_FILE pattern (with %p/%m so distinct
# gtest processes and ctest retries never clobber each other's .profraw). The
# subsequent `cargo build` then produces an instrumented mssql-odbc.so, and
# every ctest child process inherits LLVM_PROFILE_FILE from this environment.
setup_coverage_env() {
    echo "=== Enabling coverage instrumentation for the Rust driver ==="
    cargo llvm-cov clean --workspace
    # Warm the tooling first so any one-time rustup component install output
    # (e.g. downloading llvm-tools) is not sourced into the shell below.
    cargo llvm-cov show-env --export-prefix >/dev/null 2>&1 || true
    # Source only the export statements; discard rustup/info chatter.
    eval "$(cargo llvm-cov show-env --export-prefix 2>/dev/null | grep '^export ')"
    echo "LLVM_PROFILE_FILE=${LLVM_PROFILE_FILE:-<unset>}"
}

# ----------------------------------------------------------------------------
# Step 1: Configure tracing for verbose mode
# ----------------------------------------------------------------------------
setup_tracing() {
    if [ "$VERBOSE" -eq 1 ]; then
        export MSSQL_TDS_TRACE="${MSSQL_TDS_TRACE:-true}"
        export MSSQL_TDS_TRACE_LEVEL="${MSSQL_TDS_TRACE_LEVEL:-warn,mssqlodbc=debug}"
        echo "Verbose: MSSQL_TDS_TRACE=$MSSQL_TDS_TRACE MSSQL_TDS_TRACE_LEVEL=$MSSQL_TDS_TRACE_LEVEL"
    fi
}

# ----------------------------------------------------------------------------
# Step 2: Build the Rust driver and resolve its shared library path
# ----------------------------------------------------------------------------
build_rust_driver() {
    # Resolve the shipped shared-library filename for this platform.
    local libname
    if [[ "$(uname -s)" == "Darwin" ]]; then
        libname="mssql-odbc.dylib"
    else
        libname="mssql-odbc.so"
    fi

    # Prebuilt mode: an explicit --driver=PATH wins; otherwise --skip-build
    # resolves the path from the target dir without invoking cargo.
    if [ -n "$DRIVER_OVERRIDE" ]; then
        RUST_DRIVER_PATH="$DRIVER_OVERRIDE"
        echo "=== Using prebuilt driver: $RUST_DRIVER_PATH ==="
        if [ ! -f "$RUST_DRIVER_PATH" ]; then
            echo "Error: Rust driver not found at $RUST_DRIVER_PATH" >&2
            exit 1
        fi
        return
    fi

    if [ "$SKIP_BUILD" -eq 0 ]; then
        echo "=== Building mssql-odbc ($BUILD_TYPE) ==="
        (
            cd "$ODBC_CRATE_DIR"
            if [ "$BUILD_TYPE" = "release" ]; then
                cargo build --release
            else
                cargo build
            fi
        )
        RUST_DRIVER_PATH="$(bash "$ODBC_CRATE_DIR/scripts/finalize-artifact.sh" "$BUILD_TYPE")"
    else
        echo "=== Skipping driver build (--skip-build) ==="
        # build_e2e.sh stages the driver inside the build tree, so prefer that
        # copy when it exists before falling back to the cargo target dir.
        if [ -f "$BUILD_DIR/$libname" ]; then
            RUST_DRIVER_PATH="$BUILD_DIR/$libname"
            echo "Using staged driver: $RUST_DRIVER_PATH"
            return
        fi
        local target_dir
        target_dir="$(cd "$ODBC_CRATE_DIR" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
            | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null \
            || echo "$ODBC_CRATE_DIR/target")"
        RUST_DRIVER_PATH="$target_dir/$BUILD_TYPE/$libname"
    fi

    if [ ! -f "$RUST_DRIVER_PATH" ]; then
        echo "Error: Rust driver not found at $RUST_DRIVER_PATH" >&2
        exit 1
    fi
    echo "Rust driver: $RUST_DRIVER_PATH"

    # Under coverage, surface the resolved paths so a broken instrumentation
    # setup is obvious in the log. RUSTFLAGS=-C instrument-coverage was exported
    # before this build, so this .so is the instrumented one ctest will load;
    # the post-run .profraw check in generate_coverage_report proves it fired.
    if [ "$COVERAGE" -eq 1 ]; then
        echo "Coverage: LLVM_PROFILE_FILE=${LLVM_PROFILE_FILE:-<unset>}"
        echo "Coverage: instrumented driver=$RUST_DRIVER_PATH"
    fi
}

# ----------------------------------------------------------------------------
# Step 3: Register the Rust driver in a temporary odbcinst.ini under its own
# section name, so it never collides with an installed msodbcsql18.
# ----------------------------------------------------------------------------
register_rust_driver() {
    RUST_INI_DIR="$(mktemp -d)"
    cat > "$RUST_INI_DIR/odbcinst.ini" <<EOF
[$RUST_DRIVER_SECTION]
Description=Microsoft ODBC Driver 18 for SQL Server (Rust)
Driver=$RUST_DRIVER_PATH
UsageCount=1
EOF
    echo "Rust driver registered at: $RUST_INI_DIR/odbcinst.ini"
    echo "Rust driver name: $RUST_DRIVER_SECTION"
}

# ----------------------------------------------------------------------------
# Step 4: Validate comparison-mode preconditions
# ----------------------------------------------------------------------------
validate_compare_preconditions() {
    # A full connection-string override ignores ODBC_TEST_DRIVER, which would
    # silently run both legs against whichever driver it names.
    if [ -n "${ODBC_TEST_CONNSTR:-}" ]; then
        echo "Error: ODBC_TEST_CONNSTR is set; it overrides the driver name and would" >&2
        echo "  pin both comparison legs to the same driver. Unset it to compare." >&2
        exit 1
    fi
    # A DSN pins the connection to one driver just like a full connection string,
    # so both legs would resolve to the same driver.
    if [ -n "${ODBC_TEST_DSN:-}" ]; then
        echo "Error: ODBC_TEST_DSN is set; a DSN pins the connection to one driver, so" >&2
        echo "  both comparison legs would use the same driver. Unset it to compare." >&2
        exit 1
    fi
    if [ ! -f "$MSODBCSQL_INI" ]; then
        echo "Error: msodbcsql odbcinst.ini not found: $MSODBCSQL_INI" >&2
        echo "  Pass --msodbcsql-ini=PATH or install the C++ driver." >&2
        exit 1
    fi
    if ! grep -qE "^\[$MSODBCSQL_DRIVER_SECTION\]" "$MSODBCSQL_INI"; then
        echo "Error: $MSODBCSQL_INI does not contain a [$MSODBCSQL_DRIVER_SECTION] section." >&2
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
    if [ "$SKIP_BUILD" -eq 1 ]; then
        echo ""
        echo "=== Skipping e2e test build (--skip-build); reusing $BUILD_DIR ==="
        if [ ! -f "$BUILD_DIR/CTestTestfile.cmake" ]; then
            echo "Error: prebuilt CMake build not found at $BUILD_DIR" >&2
            echo "  Expected $BUILD_DIR/CTestTestfile.cmake (produced by build_e2e.sh)." >&2
            exit 1
        fi
        # Publishing/downloading the build tree as a pipeline artifact drops the
        # Unix execute bit, so the restored ctest binaries come back as 0644 and
        # ctest fails to exec them ("permission denied"). Restore +x here.
        find "$BUILD_DIR" -type f -name '*_test' -exec chmod +x {} +
        return
    fi

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
#   $4 = odbcinst.ini section name the tests should connect through
# Returns ctest's exit code (does not abort the script).
# ----------------------------------------------------------------------------
run_tests() {
    local label="$1" ini_dir="$2" junit_out="$3" driver_name="$4"
    echo ""
    echo "=== Running e2e tests against $label ==="
    echo "ODBCSYSINI=$ini_dir"
    echo "ODBC_TEST_DRIVER=$driver_name"

    local rc=0
    (
        cd "$BUILD_DIR"
        # ODBC_TEST_TARGET tells tests which driver implementation this leg runs
        # against ("mssql-odbc" or "msodbcsql") so mssql-odbc-specific tests can
        # SKIP_IF_COMPARING_MSODBCSQL() on the reference-driver leg.
        # ODBC_TEST_DRIVER selects the driver by name in the connection string.
        ODBC_TEST_TARGET="$label" ODBC_TEST_DRIVER="$driver_name" \
            ODBCSYSINI="$ini_dir" ctest "${CTEST_ARGS[@]}" --output-junit "$junit_out"
    ) || rc=$?
    return $rc
}

# ----------------------------------------------------------------------------
# Step 8: Parse two JUnit XMLs and print a parity table.
#   $1 = mssql-odbc JUnit XML
#   $2 = msodbcsql  JUnit XML
# Returns non-zero when any test reached a different verdict in the two legs.
# ----------------------------------------------------------------------------
print_parity_report() {
    local rust_xml="$1" ms_xml="$2"
    python3 "$SCRIPT_DIR/parity_report.py" "$rust_xml" "$ms_xml"
}

# ----------------------------------------------------------------------------
# Coverage: turn the .profraw written by the ctest processes into Cobertura.
# `cargo llvm-cov report` wraps llvm-profdata merge + llvm-cov export, so the
# LLVM tooling matches the rustc that produced the instrumented .so. Includes
# both mssql-tds and mssql-odbc because the cdylib statically links mssql-tds,
# so driver execution also covers mssql-tds source. Best-effort: the suite has
# already run, so a coverage tooling hiccup must not change the pass/fail result.
# Run from the workspace root (the script's cwd) so Cobertura filenames are
# repo-root-relative and union with the other per-OS reports in the merge.
# ----------------------------------------------------------------------------
generate_coverage_report() {
    echo ""
    echo "=== Generating ODBC e2e coverage report ==="
    # Prove the instrumented driver actually ran under the Driver Manager: each
    # gtest process writes a .profraw keyed by LLVM_PROFILE_FILE. Zero .profraw
    # means coverage silently captured nothing (e.g. an uninstrumented .so was
    # loaded), so warn loudly. Non-fatal: the functional result already stands.
    local profraw_dir
    profraw_dir="$(dirname "${LLVM_PROFILE_FILE:-}")"
    if [ -n "$profraw_dir" ] && [ -d "$profraw_dir" ]; then
        local n
        n="$(find "$profraw_dir" -name '*.profraw' 2>/dev/null | wc -l | tr -d ' ')"
        echo "Coverage: found $n .profraw file(s) under $profraw_dir"
        if [ "$n" -eq 0 ]; then
            echo "WARNING: no .profraw produced; the driver ctest loaded may not be instrumented" >&2
        fi
    fi
    if ! mkdir -p "$(dirname "$COVERAGE_OUTPUT")"; then
        echo "WARNING: failed to create ODBC e2e coverage output directory" >&2
        return 0
    fi
    if cargo llvm-cov report --package mssql-tds --package mssql-odbc \
        --cobertura --output-path "$COVERAGE_OUTPUT"; then
        echo "Coverage report written to $COVERAGE_OUTPUT"
    else
        echo "WARNING: failed to generate ODBC e2e coverage report" >&2
    fi
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
    if [ "$COVERAGE" -eq 1 ]; then
        setup_coverage_env
    fi
    setup_dev_sql_env
    build_rust_driver
    register_rust_driver
    if [ "$COMPARE" -eq 1 ]; then
        validate_compare_preconditions
    fi
    configure_and_build_tests

    local rust_junit="$BUILD_DIR/junit-mssql-odbc.xml"
    local ms_junit="$BUILD_DIR/junit-msodbcsql.xml"
    local rust_rc=0 ms_rc=0

    run_tests "mssql-odbc" "$RUST_INI_DIR" "$rust_junit" "$RUST_DRIVER_SECTION" || rust_rc=$?
    assert_tests_executed "$rust_junit" "mssql-odbc"

    # Report on the instrumented mssql-odbc leg before the (uninstrumented)
    # msodbcsql reference leg runs, so the profraw reflects our driver only.
    if [ "$COVERAGE" -eq 1 ]; then
        generate_coverage_report
    fi

    if [ "$COMPARE" -eq 0 ]; then
        if [ "$rust_rc" -ne 0 ]; then
            print_failure_diagnostics "e2e tests failed"
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
    run_tests "msodbcsql"  "$ms_ini_dir"  "$ms_junit" "$MSODBCSQL_DRIVER_SECTION" || ms_rc=$?
    assert_tests_executed "$ms_junit" "msodbcsql"

    local parity_rc=0
    print_parity_report "$rust_junit" "$ms_junit" || parity_rc=$?

    if [ "$rust_rc" -eq 0 ] && [ "$ms_rc" -eq 0 ] && [ "$parity_rc" -eq 0 ]; then
        echo "=== Both runs passed with full parity ==="
        exit 0
    fi
    print_failure_diagnostics "parity check failed"
    echo "=== Parity check FAILED (mssql-odbc rc=$rust_rc, msodbcsql rc=$ms_rc, parity rc=$parity_rc) ==="
    exit 1
}

main "$@"
