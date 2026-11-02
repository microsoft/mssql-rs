#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Test entrypoint for the mssql-odbc C++ gtest e2e suite on Alpine (musl, apk)
# containers. Installs only the RUNTIME dependencies needed to rerun the
# prebuilt musl driver + ctest binaries (no compiler, no cargo): bash (run_e2e.sh
# is a bash script), ctest (from cmake), the unixODBC driver manager runtime,
# libssl, libstdc++/libgcc, and the Kerberos runtime the driver may link. The
# prebuilt build/ tree (with mssqlodbc.so staged inside) is restored to
# mssql-odbc/tests/e2e/build by the calling template.
#
# Connection details arrive via ODBC_TEST_* env vars; ODBC_E2E_RETRIES controls
# the ctest until-pass retry count (default 3). ODBC_E2E_MT_PROFILE selects
# run_e2e.sh --mt-profile (default pr); the stress profiles also install gdb so
# a crash or watchdog abort in a <name>_mt entry prints every thread's stack.

set -e

MT_PROFILE="${ODBC_E2E_MT_PROFILE:-pr}"
extra=""
if [ "$MT_PROFILE" != "pr" ]; then
    extra="gdb"
fi

# shellcheck disable=SC2086  # $extra is empty or one package name
apk add --no-cache \
    bash \
    cmake \
    unixodbc \
    libssl3 \
    libstdc++ \
    libgcc \
    krb5-libs \
    ca-certificates \
    $extra

cd /workspace
exec bash /workspace/mssql-odbc/tests/e2e/run_e2e.sh --skip-build --retries="${ODBC_E2E_RETRIES:-3}" \
    --mt-profile="$MT_PROFILE"
