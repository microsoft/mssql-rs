#!/bin/sh

# Use apk for Alpine Linux
set -e

apk update
apk add --no-cache \
    ca-certificates \
    zstd \
    tar \
    gcc \
    g++ \
    make \
    pkgconfig \
    openssl-dev \
    openssl-libs-static \
    musl-dev \
    krb5-dev \
    curl \
    openssl

# Install Rust
curl https://sh.rustup.rs -sSf | sh -s -- -y

export PATH="$HOME/.cargo/bin:$PATH"

cargo install cargo-nextest --version 0.9.99 --locked

cd /workspace

# Setup the certificate
cp ca.crt /usr/local/share/ca-certificates

update-ca-certificates

# Verify the certificate
openssl verify -CAfile /etc/ssl/certs/ca-certificates.crt mssql.crt

# Compile fixtures run in the build container.
cargo nextest run -E 'not (test(connectivity) | (package(mssql-rs) & binary(data_api_compile)))' --workspace-remap /workspace --archive-file "$ARCHIVE_NAME" --no-fail-fast --profile ci --success-output immediate