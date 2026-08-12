#!/bin/bash
# Installs the pinned cargo-llvm-cov and cargo-nextest used by the coverage test
# steps.
#
# Both projects publish prebuilt binaries, so download those instead of letting
# `cargo install` compile them from source. The from-source build took up to
# ~10 minutes on the hosted macOS agent and was a recurring cause of job
# timeouts. Falls back to `cargo install` when no prebuilt asset applies (musl
# hosts) or the download fails, so the worst case is the previous behaviour
# rather than a broken build.
set -euo pipefail

LLVM_COV_VERSION="${LLVM_COV_VERSION:-0.6.16}"
NEXTEST_VERSION="${NEXTEST_VERSION:-0.9.99}"

CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$CARGO_BIN"

# Resolve the release asset for this host rather than assuming one: the hosted
# macOS image and the 1ES Linux pools differ in architecture.
llvm_cov_target=""
nextest_slug=""
case "$(uname -s)" in
  Darwin)
    llvm_cov_target="universal-apple-darwin"
    nextest_slug="mac"
    ;;
  Linux)
    if ! ldd --version 2>&1 | grep -qi musl; then
      case "$(uname -m)" in
        aarch64 | arm64)
          llvm_cov_target="aarch64-unknown-linux-gnu"
          nextest_slug="linux-arm"
          ;;
        x86_64)
          llvm_cov_target="x86_64-unknown-linux-gnu"
          nextest_slug="linux"
          ;;
      esac
    fi
    ;;
esac

# Both archives contain the bare binary at their root.
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

  if [ -n "$url" ] && fetch_binary "$url" "$binary"; then
    echo "Installed prebuilt $binary $version"
    return
  fi

  echo "No prebuilt $binary available for this host, building from source"
  cargo install "$binary" --version "$version" --locked
}

llvm_cov_url=""
if [ -n "$llvm_cov_target" ]; then
  llvm_cov_url="https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${LLVM_COV_VERSION}/cargo-llvm-cov-${llvm_cov_target}.tar.gz"
fi

nextest_url=""
if [ -n "$nextest_slug" ]; then
  nextest_url="https://get.nexte.st/${NEXTEST_VERSION}/${nextest_slug}"
fi

install_tool llvm-cov cargo-llvm-cov "$LLVM_COV_VERSION" "$llvm_cov_url"
install_tool nextest cargo-nextest "$NEXTEST_VERSION" "$nextest_url"
