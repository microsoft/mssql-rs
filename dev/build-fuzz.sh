#!/bin/bash
set -e

# Build all fuzz targets for mssql-tds and mssql-odbc.
# Requires: nightly toolchain and cargo-fuzz.

if ! rustup toolchain list | grep -q "nightly"; then
    echo "Installing nightly toolchain..."
    rustup toolchain install nightly
fi

if ! cargo +nightly fuzz --version &> /dev/null 2>&1; then
    echo "Installing cargo-fuzz..."
    cargo +nightly install cargo-fuzz
fi

repo_root="$(dirname "$0")/.."

echo "Building mssql-tds fuzz targets..."
(cd "$repo_root/mssql-tds" && cargo +nightly fuzz build)

echo "Building mssql-odbc fuzz targets..."
(cd "$repo_root/mssql-odbc" && cargo +nightly fuzz build)
