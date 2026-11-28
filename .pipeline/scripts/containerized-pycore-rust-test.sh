#!/bin/bash
set -e
source ~/.cargo/env

echo '==> Preparing PyArrow for embedded Python tests...'
python3 -m venv /tmp/pycore-rust-venv
export PYO3_PYTHON=/tmp/pycore-rust-venv/bin/python
"$PYO3_PYTHON" -m pip install --quiet 'pyarrow>=19.0.1'
# Embedded PyO3 initializes the base interpreter, not the venv's package paths.
PYTHONPATH="$("$PYO3_PYTHON" -c 'import sysconfig; print(sysconfig.get_path("purelib"))')${PYTHONPATH:+:$PYTHONPATH}"
export PYTHONPATH

# mssql-py-core is excluded from the Cargo workspace, so its Rust unit tests
# need a dedicated run here.
echo '==> Running mssql-py-core Rust unit tests with coverage...'
cd /workspace/mssql-py-core
cargo llvm-cov clean --workspace
cargo llvm-cov nextest --frozen --profile ci --no-fail-fast --no-report

echo '==> Running live SQL pool tests...'
cargo llvm-cov nextest --frozen --test pool_sql -E 'test(live_)' \
  --run-ignored only --profile pool-sql --no-fail-fast --no-report

echo '==> Generating mssql-py-core Rust coverage report...'
# Scope to mssql-py-core's own sources: the mssql-tds path dependency is covered
# comprehensively by the mssql-tds suite and would otherwise leak into this report.
mkdir -p /workspace/target
cargo llvm-cov report --ignore-filename-regex '(^|/)mssql-tds/' \
  --cobertura --output-path /workspace/target/pycore-rust-cobertura.xml
