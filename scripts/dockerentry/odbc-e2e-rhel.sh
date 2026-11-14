#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Test entrypoint for the mssql-odbc C++ gtest e2e suite on the glibc-2.28
# RHEL-family (dnf) track: RHEL 8 / UBI 8 (glibc 2.28, OpenSSL 1.1). Installs
# only the RUNTIME deps to rerun the prebuilt driver + ctest binaries: ctest
# (from cmake), the unixODBC driver manager runtime, OpenSSL libs, the
# Kerberos runtime, and glibc-gconv-extra (the base image converts CP1252 but
# not the CP932 and CP1251 that client_code_page_test uses). The prebuilt
# build/ tree (with mssqlodbc.so staged inside) is restored to
# mssql-odbc/tests/e2e/build by the calling template.
#
# Connection details arrive via ODBC_TEST_* env vars; ODBC_E2E_RETRIES controls
# the ctest until-pass retry count (default 3).

set -e

dnf install -y \
    cmake \
    unixODBC \
    openssl-libs \
    krb5-libs \
    glibc-gconv-extra \
    ca-certificates
dnf clean all || true

# client_code_page_test skips a code page the host cannot convert; fail here so
# this leg cannot silently lose that coverage.
for code_page in CP932 CP1251; do
    iconv -f UTF-8 -t "$code_page" </dev/null
done

cd /workspace
exec /workspace/mssql-odbc/tests/e2e/run_e2e.sh --skip-build --retries="${ODBC_E2E_RETRIES:-3}"
