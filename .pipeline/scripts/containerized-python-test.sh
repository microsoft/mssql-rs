#!/bin/bash
set -e

# Update CA certificates in container
update-ca-certificates

# Source cargo environment
source ~/.cargo/env

# Set Python path
export PATH="/usr/local/bin:$PATH"

# Some ARM64 build images have rustup and Cargo but omit rustc from the
# repository-selected toolchain.
cd /workspace
rustup component add rustc

# Run Python tests using the dev script
# Pass through any arguments (e.g., --skip-integration)
echo "Running Python tests for mssql-py-core..."
./dev/test-python.sh "$@"

echo "Python tests completed successfully"
