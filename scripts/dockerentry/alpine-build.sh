#!/bin/sh


set -e

export PATH="$HOME/.cargo/bin:$PATH"

cd /workspace

# Detect architecture and set the appropriate musl target
ARCH=$(uname -m)
if [ "$ARCH" = "aarch64" ]; then
    RUST_TARGET="aarch64-unknown-linux-musl"
elif [ "$ARCH" = "x86_64" ]; then
    RUST_TARGET="x86_64-unknown-linux-musl"
else
    echo "Unsupported architecture: $ARCH"
    exit 1
fi

echo "==> Building for target: $RUST_TARGET"

# Ensure the musl target is installed
echo "==> Installing musl target: $RUST_TARGET"
rustup target add "$RUST_TARGET"

cd mssql-tds

# Build tests with GSSAPI feature enabled.
# With the new dlopen-based implementation, there's no compile-time dependency on krb5.
# GSSAPI support is loaded at runtime via dlopen - if libgssapi_krb5.so is not installed,
# is_gssapi_available() returns false and Kerberos auth gracefully fails.
# This matches ODBC's approach: no compile-time linking, runtime detection.
#
# IMPORTANT: Must use -C target-feature=-crt-static to enable dynamic linking.
# Rust defaults to static linking on musl, but GSSAPI uses dlopen() to load
# the Kerberos library at runtime. Static binaries cannot use dlopen().
echo "==> Building tests with GSSAPI (dlopen-based, dynamic linking for dlopen support)"
RUSTFLAGS="-C target-feature=-crt-static" cargo nextest archive --target "$RUST_TARGET" --features mssql-tds/gssapi,mssql-rs/arrow,mssql-rs/jiff,mssql-rs/serde,mssql-rs/time --archive-file ../tdslib-nextest-musl.tar.zst --package mssql-tds --package mssql-rs
