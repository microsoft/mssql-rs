#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Test entrypoint for the mssql-odbc C++ gtest e2e suite on Debian/Ubuntu
# (apt) containers. Installs only the RUNTIME dependencies needed to rerun the
# prebuilt driver + ctest binaries (no compiler, no cargo): ctest (from cmake),
# the unixODBC driver manager runtime, libssl, and the Kerberos runtime the
# driver may link. The prebuilt build/ tree (with mssqlodbc.so staged
# inside) is restored to mssql-odbc/tests/e2e/build by the calling template.
#
# Connection details arrive via ODBC_TEST_* env vars; ODBC_E2E_RETRIES controls
# the ctest until-pass retry count (default 3). ODBC_E2E_MT_PROFILE selects
# run_e2e.sh --mt-profile (default pr); the stress profiles also install gdb so
# a crash or watchdog abort in a <name>_mt entry prints every thread's stack.

set -e

MT_PROFILE="${ODBC_E2E_MT_PROFILE:-pr}"
extra=()
if [ "$MT_PROFILE" != "pr" ]; then
    extra+=(gdb)
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    cmake \
    unixodbc \
    libssl3 \
    libgssapi-krb5-2 \
    ca-certificates \
    "${extra[@]}"
rm -rf /var/lib/apt/lists/*

cd /workspace
exec /workspace/mssql-odbc/tests/e2e/run_e2e.sh --skip-build --retries="${ODBC_E2E_RETRIES:-3}" \
    --mt-profile="$MT_PROFILE"
