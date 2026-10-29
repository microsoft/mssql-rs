#!/bin/bash
# Ensures rustc is actually usable for the toolchain pinned by
# rust-toolchain.toml, before any maturin/cargo invocation that depends on it.
#
# A fresh build container can contain the pinned toolchain metadata without
# rustc itself, which maturin needs to detect the host target: rust-toolchain.toml
# pins the channel and rustup materialises it lazily on first use, deep inside
# `maturin develop`/`maturin build`. When that fetch lands incomplete, the
# toolchain stays registered but without rustc, and the only symptom several
# layers up is maturin reporting "Failed to run rustc to get the host target"
# with no indication of which download failed. Materialise it here instead,
# with retries, and dump enough state to explain a failure that survives them.
#
# Usage: ensure-rust-toolchain.sh [workspace-root]
# workspace-root (default: current directory) must contain rust-toolchain.toml.
set -e

WORKSPACE_ROOT="${1:-$PWD}"

# Try the currently active (default) toolchain first -- cheap, and covers the
# same "registered but rustc missing" state for whatever toolchain the image
# defaults to outside a rust-toolchain.toml-pinned directory.
rustup component add rustc || true

RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$WORKSPACE_ROOT/rust-toolchain.toml" | head -1)"
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
