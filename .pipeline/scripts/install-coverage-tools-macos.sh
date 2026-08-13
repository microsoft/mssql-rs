#!/bin/bash
# Installs the pinned cargo-llvm-cov and cargo-nextest on the hosted macOS agent.
#
# `cargo install` builds both tools from source, which took up to ~10 minutes
# there and was a recurring cause of Test MacOS timeouts. Both projects publish
# prebuilt macOS binaries, so download those instead. Falls back to the source
# build if a download fails, so a GitHub outage degrades to the previous
# behaviour rather than breaking the job.
set -euo pipefail

# Keep in sync with the non-macOS `cargo install` step in
# .pipeline/templates/build-template.yml, which pins the same two versions for
# Windows and Linux. Bumping only one side gives macOS a different toolchain.
LLVM_COV_VERSION=0.6.16
NEXTEST_VERSION=0.9.99

# HTTPS authenticates the host but not the bytes, so pin each archive's digest
# too. A mismatch is treated as a download failure and falls back to
# `cargo install`, which verifies crates.io checksums itself. Update these
# whenever the versions above change.
LLVM_COV_SHA256=1cc7eee103ab8d4ee56c18c51dd9e05c2139e63583e85af5e9861d3d01242c9b
NEXTEST_SHA256=fb1e9fb9a6da22972182d96e62f6664d325db3788775c96a07dacaf04cfed244

# rustup installs to $HOME/.cargo and install-rustup.sh prepends its bin to PATH.
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$CARGO_BIN"

verify_sha256() {
  local file="$1" expected="$2" actual
  actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  if [ "$actual" = "$expected" ]; then
    return 0
  fi
  echo "Checksum mismatch: expected $expected, got $actual" >&2
  return 1
}

# Both downloads are universal binaries, so an x86_64 or arm64 pool image works
# alike, and both archives contain the bare binary at their root.
#
# Every step stays in the && chain so a failure anywhere - including the final
# install - is reported to the caller and falls back to the source build.
fetch_binary() {
  local url="$1" binary="$2" expected="$3" tmp rc=0
  tmp="$(mktemp -d)"
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$url" -o "$tmp/tool.tar.gz" &&
    verify_sha256 "$tmp/tool.tar.gz" "$expected" &&
    tar -xzf "$tmp/tool.tar.gz" -C "$tmp" "$binary" &&
    install -m 755 "$tmp/$binary" "$CARGO_BIN/$binary" || rc=1
  rm -rf "$tmp"
  return "$rc"
}

install_tool() {
  local subcommand="$1" binary="$2" version="$3" expected="$4" url="$5"

  # Require a word boundary on both sides: a bare substring match would treat an
  # installed 0.6.161 as satisfying a 0.6.16 pin. `cargo nextest --version`
  # prints a commit suffix after the version, so anchoring on end-of-line alone
  # is not enough either.
  if cargo "$subcommand" --version 2>/dev/null |
    grep -qE "(^|[[:space:]])${version//./\\.}([[:space:]]|\$)"; then
    echo "$binary $version already present, skipping"
    return
  fi

  if fetch_binary "$url" "$binary" "$expected"; then
    echo "Installed prebuilt $binary $version"
    return
  fi

  # Surface the slow path on the build summary. This step exists to avoid the
  # ~10 minute source build, so a silent fallback would hide the regression.
  echo "##vso[task.logissue type=warning]Prebuilt $binary $version unavailable or failed verification; falling back to cargo install (slow)"
  cargo install "$binary" --version "$version" --locked
}

install_tool llvm-cov cargo-llvm-cov "$LLVM_COV_VERSION" "$LLVM_COV_SHA256" \
  "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${LLVM_COV_VERSION}/cargo-llvm-cov-universal-apple-darwin.tar.gz"

install_tool nextest cargo-nextest "$NEXTEST_VERSION" "$NEXTEST_SHA256" \
  "https://get.nexte.st/${NEXTEST_VERSION}/mac"
