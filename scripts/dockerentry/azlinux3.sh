#!/bin/bash

tdnf upgrade -y
tdnf install ca-certificates curl pkg-config build-essential krb5-devel -y


curl https://sh.rustup.rs -sSf | sh -s -- -y

export PATH="$HOME/.cargo/bin:$PATH"

cargo install cargo-nextest --version 0.9.99 --locked


cd /workspace

cp ca.crt /etc/pki/ca-trust/source/anchors/

update-ca-trust extract

openssl verify -CAfile /etc/pki/tls/certs/ca-bundle.trust.crt mssql.crt

# Compile fixtures run in the build container.
cargo nextest run -E 'not (test(connectivity) | (package(mssql-rs) & binary(data_api_compile)))' --workspace-remap /workspace --archive-file "$ARCHIVE_NAME" --no-fail-fast --profile ci --success-output immediate