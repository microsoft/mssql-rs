#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Test entrypoint for the mssql-odbc C++ gtest e2e suite on Debian/Ubuntu
# (apt) containers. Installs only the RUNTIME dependencies needed to rerun the
# prebuilt driver + ctest binaries (no compiler, no cargo): ctest (from cmake),
# the unixODBC driver manager runtime, libssl, and the Kerberos runtime the
# driver may link. The prebuilt build/ tree (with libmsodbcsql18.so staged
# inside) is restored to mssql-odbc/tests/e2e/build by the calling template.
#
# Connection details arrive via ODBC_TEST_* env vars; ODBC_E2E_RETRIES controls
# the ctest until-pass retry count (default 3).

set -e

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    cmake \
    unixodbc \
    libssl3 \
    libgssapi-krb5-2 \
    ca-certificates
rm -rf /var/lib/apt/lists/*

cd /workspace
exec /workspace/mssql-odbc/tests/e2e/run_e2e.sh --skip-build --retries="${ODBC_E2E_RETRIES:-3}"
