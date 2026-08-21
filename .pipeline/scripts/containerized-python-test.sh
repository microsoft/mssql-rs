#!/bin/bash
set -e

# Update CA certificates in container
update-ca-certificates

# Source cargo environment
source ~/.cargo/env

# A fresh ARM64 build container can contain the pinned toolchain metadata
# without rustc, which maturin needs to detect the host target.
rustup component add rustc

# Set Python path
export PATH="/usr/local/bin:$PATH"

# Run Python tests using the dev script
# Pass through any arguments (e.g., --skip-integration)
echo "Running Python tests for mssql-py-core..."
cd /workspace

# rust-toolchain.toml pins the channel and rustup materialises it on first use,
# deep inside `maturin develop`. When that fetch lands incomplete the toolchain
# stays registered but without rustc, and the only symptom is maturin reporting
# "Failed to run rustc to get the host target" with no indication of which
# download failed. Materialise it here instead, with retries, and dump enough
# state to explain a failure that survives them.
RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml | head -1)"
echo "=== Rust toolchain state (rust-toolchain.toml channel: ${RUST_CHANNEL:-unknown}) ==="
rustup show || true
rustup toolchain list --verbose || true

for attempt in 1 2 3; do
    if rustc --version >/dev/null 2>&1; then
        break
    fi
    echo "rustc not usable (attempt ${attempt}/3); installing toolchain ${RUST_CHANNEL}..."
    # --force because the failure mode is a half-installed toolchain: without it
    # rustup considers the channel present and skips the repair.
    rustup toolchain install "${RUST_CHANNEL}" --force --no-self-update || true
done

if ! rustc --version; then
    echo "ERROR: rustc still unavailable after 3 attempts."
    echo "=== toolchain CDN reachability ==="
    curl -sS -o /dev/null \
        -w '  channel manifest: http=%{http_code} dns=%{time_namelookup}s connect=%{time_connect}s total=%{time_total}s bytes=%{size_download} speed=%{speed_download}B/s\n' \
        "https://static.rust-lang.org/dist/channel-rust-${RUST_CHANNEL}.toml" \
        || echo "  probe failed to complete"
    echo "=== toolchains after retries ==="
    rustup toolchain list --verbose || true
    exit 1
fi

./dev/test-python.sh "$@"

echo "Python tests completed successfully"
