#!/bin/bash
# Installs the pinned cargo-llvm-cov and cargo-nextest on the hosted macOS agent.
#
# `cargo install` builds both tools from source, which took up to ~10 minutes
# there and was a recurring cause of Test MacOS timeouts. Both projects publish
# prebuilt macOS binaries, so download those instead. Falls back to the source
# build if a download fails, so a GitHub outage degrades to the previous
# behaviour rather than breaking the job.
set -euo pipefail

LLVM_COV_VERSION="${LLVM_COV_VERSION:-0.6.16}"
NEXTEST_VERSION="${NEXTEST_VERSION:-0.9.99}"

# rustup installs to $HOME/.cargo and install-rustup.sh prepends its bin to PATH.
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$CARGO_BIN"

# Both downloads are universal binaries, so an x86_64 or arm64 pool image works
# alike, and both archives contain the bare binary at their root.
fetch_binary() {
  local url="$1" binary="$2" tmp rc=0
  tmp="$(mktemp -d)"
  if curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$url" -o "$tmp/tool.tar.gz" &&
    tar -xzf "$tmp/tool.tar.gz" -C "$tmp" "$binary"; then
    install -m 755 "$tmp/$binary" "$CARGO_BIN/$binary"
  else
    rc=1
  fi
  rm -rf "$tmp"
  return "$rc"
}

install_tool() {
  local subcommand="$1" binary="$2" version="$3" url="$4"

  if cargo "$subcommand" --version 2>/dev/null | grep -qF "$version"; then
    echo "$binary $version already present, skipping"
    return
  fi

  if fetch_binary "$url" "$binary"; then
    echo "Installed prebuilt $binary $version"
    return
  fi

  echo "Could not download prebuilt $binary, building from source"
  cargo install "$binary" --version "$version" --locked
}

install_tool llvm-cov cargo-llvm-cov "$LLVM_COV_VERSION" \
  "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${LLVM_COV_VERSION}/cargo-llvm-cov-universal-apple-darwin.tar.gz"

install_tool nextest cargo-nextest "$NEXTEST_VERSION" \
  "https://get.nexte.st/${NEXTEST_VERSION}/mac"
