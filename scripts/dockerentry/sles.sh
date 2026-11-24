#!/bin/bash

# Use zypper for SLES 15
set -e

zypper refresh
zypper update -y

zypper install -y \
    ca-certificates \
    zstd \
    tar \
    gcc \
    gcc-c++ \
    make \
    pkg-config \
    libopenssl-devel \
    krb5-devel \
    curl

# Install Rust
curl https://sh.rustup.rs -sSf | sh -s -- -y

export PATH="$HOME/.cargo/bin:$PATH"

cargo install cargo-nextest --version 0.9.99 --locked

cd /workspace

cp ca.crt /etc/pki/trust/anchors

update-ca-certificates

# Compile fixtures run in the build container.
cargo nextest run -E 'not (test(connectivity) | (package(mssql-rs) & binary(data_api_compile)))' --workspace-remap /workspace --archive-file "$ARCHIVE_NAME" --no-fail-fast --profile ci --success-output immediate