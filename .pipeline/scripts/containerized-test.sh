#!/bin/bash
set -e
source ~/.cargo/env

update-ca-certificates

# Generate test certificates for mock TDS server TLS tests (if not already present)
echo '==> Generating test certificates for mock TDS server...'
/workspace/scripts/generate_mock_tds_server_certs.sh

echo '==> Running tests...'
mkdir -p /workspace/target/nextest/ci
cargo llvm-cov nextest "$@" --frozen --no-report --all-targets --package mssql-tds --no-fail-fast --profile ci --success-output immediate

echo '==> Generating coverage report...'
cargo llvm-cov report --package mssql-tds --lcov --output-path /workspace/target/lcov.info
cargo llvm-cov report --package mssql-tds --cobertura --output-path /workspace/target/cobertura.xml
