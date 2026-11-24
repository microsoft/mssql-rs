#!/bin/bash

apt-get update && \
apt-get install -y \
    curl \
    ca-certificates \
    zstd \
    tar \
    build-essential \
    pkg-config \
    openssl \
    libkrb5-dev


curl https://sh.rustup.rs -sSf | sh -s -- -y

export PATH="$HOME/.cargo/bin:$PATH"

cargo install cargo-nextest --version 0.9.99 --locked

cd /workspace

# Setup the certificate 

cp ca.crt /usr/local/share/ca-certificates
update-ca-certificates
# Verify the certificate
openssl verify -CAfile /etc/ssl/certs/ca-certificates.crt mssql.pem

# Compile fixtures run in the build container.
cargo nextest run -E 'not (test(connectivity) | (package(mssql-rs) & binary(data_api_compile)))' --workspace-remap /workspace --archive-file "$ARCHIVE_NAME" --no-fail-fast --profile ci --success-output immediate