#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Runs the mssql-odbc C++ gtest e2e suite inside the Ubuntu build container,
# against the SQL Server the surrounding Build_Linux / Build_Linux_ARM job has
# already brought up (reachable on the shared docker "testnet"). The build image
# (containers/Dockerfile.Ubuntu.Build) ships gcc/g++/make/git/cargo but not cmake
# or the unixODBC dev headers the C++ Driver Manager links against, so install
# those here before delegating to run_e2e.sh.
#
# Connection details are passed via ODBC_TEST_* env vars by the caller.
# ODBC_E2E_RETRIES controls the ctest until-pass retry count (default 3).
# ODBC_E2E_COVERAGE=1 builds the driver instrumented and emits a Cobertura
# report (x64 Linux PR builds only).
# ODBC_E2E_COMPARE=1 additionally installs the Microsoft ODBC Driver 18 and
# reruns the same suite against it, failing on any parity divergence.

set -euo pipefail

source ~/.cargo/env

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends cmake unixodbc-dev

# The reference driver for comparison mode. Pinned so a new upstream release
# can't silently change what the parity table is comparing against.
MSODBCSQL_VERSION="18.6.2.1-1"

compare_args=()
case "$(printf '%s' "${ODBC_E2E_COMPARE:-0}" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes)
        apt-get install -y --no-install-recommends curl gnupg ca-certificates
        curl -fsSL https://packages.microsoft.com/keys/microsoft.asc \
            -o /etc/apt/trusted.gpg.d/microsoft.asc
        curl -fsSL "https://packages.microsoft.com/config/ubuntu/$(. /etc/os-release && echo "$VERSION_ID")/prod.list" \
            -o /etc/apt/sources.list.d/mssql-release.list
        apt-get update
        ACCEPT_EULA=Y apt-get install -y --no-install-recommends "msodbcsql18=$MSODBCSQL_VERSION"
        # msodbcsql18 registers itself as [ODBC Driver 18 for SQL Server] in
        # /etc/odbcinst.ini, which is run_e2e.sh's default --msodbcsql-ini.
        compare_args+=(--compare-with-msodbcsql)
        ;;
esac

rm -rf /var/lib/apt/lists/*

# When ODBC_E2E_COVERAGE=1 (x64 Linux PR builds), build the driver with LLVM
# instrumentation and emit a Cobertura report to the mounted workspace target/
# dir so the host agent can publish it as CoberturaCoverageOdbcE2E_Linux. The
# cargo-llvm-cov + llvm-tools this needs already ship in the build image.
coverage_args=()
case "$(printf '%s' "${ODBC_E2E_COVERAGE:-0}" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes)
        coverage_args+=(--coverage=/workspace/target/cobertura-odbc-e2e.xml)
        ;;
esac

exec /workspace/mssql-odbc/tests/e2e/run_e2e.sh --retries="${ODBC_E2E_RETRIES:-3}" \
    "${coverage_args[@]}" "${compare_args[@]}"
