#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Test entrypoint for the mssql-odbc C++ gtest e2e suite on Azure Linux 3
# (tdnf). Installs only the RUNTIME deps to rerun the prebuilt driver + ctest
# binaries: ctest (from cmake), the unixODBC driver manager runtime, OpenSSL
# libs, and the Kerberos runtime. The prebuilt build/ tree (with
# libmsodbcsql18.so staged inside) is restored to mssql-odbc/tests/e2e/build by
# the calling template.
#
# Connection details arrive via ODBC_TEST_* env vars; ODBC_E2E_RETRIES controls
# the ctest until-pass retry count (default 3).

set -e

tdnf install -y \
    cmake \
    unixODBC \
    openssl-libs \
    krb5 \
    ca-certificates
tdnf clean all || true

cd /workspace
exec /workspace/mssql-odbc/tests/e2e/run_e2e.sh --skip-build --retries="${ODBC_E2E_RETRIES:-3}"
