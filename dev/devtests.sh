#!/bin/bash

set -e

# Check if cargo nextest is installed
if ! command -v cargo-nextest &> /dev/null && ! cargo nextest --version &> /dev/null; then
    echo "Error: cargo-nextest is not installed."
    echo "Please install it with: cargo install cargo-nextest"
    exit 1
fi

# Generate certificates required for tests
./scripts/generate_mock_tds_server_certs.sh

# Run workspace Rust tests.
echo "Running workspace tests..."
workspace_exit_code=0
cargo nextest run \
    --workspace \
    --no-fail-fast \
    --profile ci \
    --success-output immediate || workspace_exit_code=$?

# Run mssql-py-core independently because it is outside the workspace.
echo "Running tests for mssql-py-core..."
pycore_exit_code=0
pushd mssql-py-core > /dev/null
cargo nextest run \
    --all-targets \
    --no-fail-fast \
    --profile ci \
    --success-output immediate || pycore_exit_code=$?
popd > /dev/null

# Run the mssql-tds integration tests with connectivity excluded.
echo "Running mssql-tds integration tests..."
mssql_tds_exit_code=0
cargo nextest run \
    -E "not (test(connectivity))" \
    --all-targets \
    -p mssql-tds \
    --no-fail-fast \
    --profile ci \
    --success-output immediate || mssql_tds_exit_code=$?

if [ "$workspace_exit_code" -ne 0 ]; then
    echo "Workspace tests failed"
fi

if [ "$pycore_exit_code" -ne 0 ]; then
    echo "mssql-py-core tests failed"
fi

if [ "$mssql_tds_exit_code" -ne 0 ]; then
    echo "mssql-tds integration tests failed"
fi

if [ "$workspace_exit_code" -ne 0 ]; then
    exit "$workspace_exit_code"
fi

if [ "$pycore_exit_code" -ne 0 ]; then
    exit "$pycore_exit_code"
fi

exit "$mssql_tds_exit_code"
