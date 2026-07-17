#!/bin/bash
set -e
source ~/.cargo/env

# mssql-py-core is outside the workspace, so its Rust unit tests are not covered
# by the mssql-tds nextest run. Run them here with the crate-local `ci` profile
# so JUnit is written to mssql-py-core/target/nextest/ci/junit.xml for the ADO
# test-results tab.
echo '==> Running mssql-py-core Rust unit tests...'
cd /workspace/mssql-py-core
cargo nextest run --frozen --profile ci --no-fail-fast
