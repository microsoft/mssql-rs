#!/bin/bash
set -e
source ~/.cargo/env

# Update CA certificates in container
update-ca-certificates

# Verify certificate
openssl verify -CAfile /etc/ssl/certs/ca-certificates.crt /workspace/mssql.crt || true

# Generate test certificates for mock TDS server TLS tests
echo '==> Generating test certificates for mock TDS server...'
/workspace/scripts/generate_mock_tds_server_certs.sh

# Fetch dependencies
echo '==> Fetching crates...'
cargo fetch

# Fetch mssql-py-core dependencies (it's outside workspace)
echo '==> Fetching mssql-py-core crates...'
cd mssql-py-core
cargo fetch
cd ..

# Build based on BUILD_TYPE
if [ "$BUILD_TYPE" = "Debug" ] || [ "$BUILD_TYPE" = "Both" ]; then
  echo '==> Building debug...'
  cargo build --frozen
  bash mssql-odbc/scripts/finalize-artifact.sh debug
fi

if [ "$BUILD_TYPE" = "Release" ] || [ "$BUILD_TYPE" = "Both" ]; then
  echo '==> Building release...'
  cargo build --frozen --release
  bash mssql-odbc/scripts/finalize-artifact.sh release
fi

# Archive nextest (used by later test stages)
echo '==> Creating nextest archive...'
cd mssql-tds
cargo nextest archive --archive-file tdslib-nextest.tar.zst
mv tdslib-nextest.tar.zst /workspace/
cd ..

# Verify fuzz targets compile (PR builds only)
if [ "$IS_PR_BUILD" = "true" ]; then
  echo '==> Installing nightly toolchain for fuzz build check...'
  rustup toolchain install nightly --profile minimal
  echo '==> Checking fuzz targets compile...'
  RUSTFLAGS="--cfg fuzzing" cargo +nightly check --manifest-path mssql-tds/fuzz/Cargo.toml
  echo '==> Fuzz build check passed.'
fi
