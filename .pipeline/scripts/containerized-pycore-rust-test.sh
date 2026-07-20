#!/bin/bash
set -e
source ~/.cargo/env

# mssql-py-core is excluded from the Cargo workspace, so its Rust unit tests
# need a dedicated run here.
echo '==> Running mssql-py-core Rust unit tests with coverage...'
cd /workspace/mssql-py-core
cargo llvm-cov clean --workspace
cargo llvm-cov nextest --frozen --profile ci --no-fail-fast --no-report

echo '==> Generating mssql-py-core Rust coverage report...'
# Scope to mssql-py-core's own sources: the mssql-tds path dependency is covered
# comprehensively by the mssql-tds suite and would otherwise leak into this report.
mkdir -p /workspace/target
cargo llvm-cov report --ignore-filename-regex '(^|/)mssql-tds/' \
  --cobertura --output-path /workspace/target/pycore-rust-cobertura.xml
