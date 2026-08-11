#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Run the microsoft/mssql-python pytest suite inside the test container after the
# bundled Microsoft ODBC Driver 18 binary has been replaced by the Rust
# mssql-odbc driver (see swap-mssql-python-odbc-driver.sh).
#
# The Rust driver is still under construction, so this suite is expected to fail
# and to hard-crash the interpreter (segfault / abort) on some files. Each test
# file therefore gets its own pytest process wrapped in `timeout`, so a crash or
# a hang costs one file instead of the whole run:
#
#   - A crash kills only that file's process; the loop moves to the next file.
#   - `timeout` bounds every file, so a wedged driver call can never stall the job.
#   - pytest only writes its JUnit XML at the end of a run, so a file that dies
#     leaves no report. A synthetic one-error report is emitted in its place,
#     keeping the crash visible in the pipeline's Tests tab.
#
# `--cov` is deliberately dropped from the upstream invocation: coverage.py
# cannot combine data files across the per-file processes, and a native crash
# leaves the data file truncated anyway.
#
# Env:
#   MSSQL_PYTHON_DIR         mssql-python checkout to test (default: /workspace).
#   TEST_RESULTS_DIR         JUnit XML output directory (default: $MSSQL_PYTHON_DIR/test-results).
#   PYTEST_FILE_TIMEOUT      Per-file wall-clock budget (default: 15m).
#
# Exits non-zero when any file failed, crashed, or timed out. The calling
# pipeline step runs with continueOnError so that surfaces as a warning.

# No `set -e`: a failing or crashing test file must not abort the loop.
set -uo pipefail

MSSQL_PYTHON_DIR="${MSSQL_PYTHON_DIR:-/workspace}"
RESULTS_DIR="${TEST_RESULTS_DIR:-$MSSQL_PYTHON_DIR/test-results}"
PYTEST_FILE_TIMEOUT="${PYTEST_FILE_TIMEOUT:-15m}"

cd "$MSSQL_PYTHON_DIR" || exit 1
mkdir -p "$RESULTS_DIR"
rm -f "$RESULTS_DIR"/*.xml

# XML-escape for embedding a message in an attribute value.
xml_escape() {
    printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g'
}

write_report_stub() {
    local name="$1" kind="$2" reason="$3" out="$4"
    local safe_reason
    safe_reason="$(xml_escape "$reason")"
    if [ "$kind" = "skipped" ]; then
        cat > "$out" <<XML
<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="$name" tests="1" errors="0" failures="0" skipped="1" time="0">
    <testcase classname="mssql_odbc_swap.$name" name="pytest_process" time="0">
      <skipped type="pytest.skip" message="$safe_reason"/>
    </testcase>
  </testsuite>
</testsuites>
XML
    else
        cat > "$out" <<XML
<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="$name" tests="1" errors="1" failures="0" skipped="0" time="0">
    <testcase classname="mssql_odbc_swap.$name" name="pytest_process" time="0">
      <error type="ProcessTerminated" message="$safe_reason">The pytest process for $name terminated abnormally under the mssql-odbc driver, so no per-test results were produced for this file.</error>
    </testcase>
  </testsuite>
</testsuites>
XML
    fi
}

mapfile -t TEST_FILES < <(find tests -maxdepth 1 -name 'test_*.py' -type f | sort)

if [ "${#TEST_FILES[@]}" -eq 0 ]; then
    echo "##[error]No test files found under $MSSQL_PYTHON_DIR/tests"
    exit 1
fi

echo "Running ${#TEST_FILES[@]} mssql-python test files against the mssql-odbc driver"
echo "Per-file timeout: $PYTEST_FILE_TIMEOUT"

declare -a SUMMARY=()
passed=0 failed=0 crashed=0 timedout=0 empty=0

for test_file in "${TEST_FILES[@]}"; do
    name="$(basename "$test_file" .py)"
    report="$RESULTS_DIR/results-$name.xml"

    echo ""
    echo "##[group]$test_file"
    # SIGKILL 60s after SIGTERM in case the driver wedges in an uninterruptible call.
    timeout --kill-after=60s "$PYTEST_FILE_TIMEOUT" \
        python -m pytest "$test_file" -v \
        --junitxml="$report" \
        --capture=tee-sys \
        --cache-clear
    rc=$?
    echo "##[endgroup]"

    case "$rc" in
        0)   status="PASSED";              kind="ok";      passed=$((passed + 1)) ;;
        1)   status="FAILED";              kind="error";   failed=$((failed + 1)) ;;
        2)   status="INTERRUPTED";         kind="error";   failed=$((failed + 1)) ;;
        3)   status="INTERNAL ERROR";      kind="error";   failed=$((failed + 1)) ;;
        4)   status="USAGE ERROR";         kind="error";   failed=$((failed + 1)) ;;
        # Every test in the file was deselected by pytest.ini's `-m "not stress"`.
        5)   status="NO TESTS COLLECTED";  kind="skipped"; empty=$((empty + 1)) ;;
        124|137) status="TIMED OUT after $PYTEST_FILE_TIMEOUT"; kind="error"; timedout=$((timedout + 1)) ;;
        *)
            if [ "$rc" -gt 128 ]; then
                status="CRASHED (signal $((rc - 128)))"
            else
                status="CRASHED (exit $rc)"
            fi
            kind="error"
            crashed=$((crashed + 1))
            ;;
    esac

    if [ ! -s "$report" ]; then
        write_report_stub "$name" "$kind" "$status" "$report"
        if [ "$kind" = "skipped" ]; then
            echo "$name: $status"
        else
            echo "##[warning]$name: $status - no pytest report produced, wrote synthetic error report"
        fi
    else
        echo "$name: $status"
    fi

    SUMMARY+=("$(printf '%-52s %s' "$name" "$status")")
done

echo ""
echo "==============================================================="
echo "mssql-python suite against mssql-odbc - per-file results"
echo "==============================================================="
printf '%s\n' "${SUMMARY[@]}"
echo "==============================================================="
echo "files: ${#TEST_FILES[@]} | passed: $passed | failed: $failed | crashed: $crashed | timed out: $timedout | no tests: $empty"
echo "==============================================================="

if [ "$failed" -gt 0 ] || [ "$crashed" -gt 0 ] || [ "$timedout" -gt 0 ]; then
    echo "##[warning]mssql-python tests did not fully pass against the mssql-odbc driver (expected while the driver is in development)"
    exit 1
fi
