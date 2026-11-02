#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# ctest launcher for the <name>_mt entries (the ODBC_TEST_F concurrent
# variants) on Unix. See CMakeLists.txt.
#
# Usage: run_mt_test.sh <test-executable> [gtest arguments...]
#
# By default it just runs the executable. With MSSQL_ODBC_MT_GDB=1 it runs it
# under gdb in batch mode instead, so a crash, or the hang watchdog's abort(),
# prints every thread's stack into the ctest log, which CI keeps even when a
# container cannot write a core file. Set MSSQL_ODBC_MT_TERMINATE_UNDER_DEBUGGER=1
# as well, or the watchdog leaves a traced process running. The exit code is the
# test's own, or 128 + signal when it died on a signal. Without gdb installed it
# warns and runs the executable directly.
#
# Invoked as "sh run_mt_test.sh", so the script needs no execute bit after a
# pipeline artifact round trip.

set -u

if [ "$#" -lt 1 ]; then
    echo "usage: run_mt_test.sh <test-executable> [args...]" >&2
    exit 2
fi

if [ "${MSSQL_ODBC_MT_GDB:-0}" != "1" ]; then
    exec "$@"
fi

if ! command -v gdb >/dev/null 2>&1; then
    echo "run_mt_test.sh: MSSQL_ODBC_MT_GDB=1 but gdb is not installed; running without it." >&2
    exec "$@"
fi

script_dir=$(cd "$(dirname "$0")" && pwd)
exec gdb -q -batch -nx -x "$script_dir/run_mt_test.gdb" --args "$@"
