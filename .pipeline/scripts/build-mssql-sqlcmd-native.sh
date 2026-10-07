#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the mssql-sqlcmd static library for one Rust target and stage it in the
# layout of the mssql-sqlcmd NuGet package (see mssql-sqlcmd/README.md):
#
#   <out-dir>/runtimes/<rid>/native/libmssql_sqlcmd.a
#   <out-dir>/runtimes/<rid>/native/native-static-libs.txt
#
# native-static-libs.txt holds the system libraries the archive needs, exactly
# as rustc reports them, so consumers link what this build requires instead of
# a hard-coded list.
#
# Usage: build-mssql-sqlcmd-native.sh <rust-target> <rid> <out-dir>
#   e.g. build-mssql-sqlcmd-native.sh x86_64-unknown-linux-gnu linux-x64 /tmp/sqlcmd-native

set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <rust-target> <rid> <out-dir>" >&2
  exit 2
fi
target="$1"
rid="$2"
out_dir="$3"

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

if command -v rustup >/dev/null 2>&1; then
  rustup target add "$target" >/dev/null
fi

# musl targets default to a static C runtime, which leaves the archive needing
# an external libunwind. The consumer (Alpine sqlcmd) links musl dynamically,
# so build against the dynamic runtime: it then needs only libgcc_s and libc,
# as on glibc.
case "$target" in
  *-musl*) export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static" ;;
esac

log="$(mktemp "${TMPDIR:-/tmp}/mssql-sqlcmd.XXXXXX")"
trap 'rm -f "$log"' EXIT
if ! cargo rustc -p mssql-sqlcmd --release --lib --target "$target" \
    --crate-type staticlib -- --print native-static-libs 2>"$log"; then
  cat "$log" >&2
  exit 1
fi

native_libs="$(sed -n 's/^note: native-static-libs: //p' "$log" | tail -n 1)"
if [ -z "$native_libs" ]; then
  cat "$log" >&2
  echo "ERROR: rustc printed no native-static-libs line for $target" >&2
  exit 1
fi

# Cargo's resolved target directory honors CARGO_TARGET_DIR and [build] target-dir.
target_dir="$(cargo metadata --no-deps --format-version 1 |
  sed -n 's/.*"target_directory"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
if [ -z "$target_dir" ]; then
  echo "ERROR: cargo metadata reports no target_directory" >&2
  exit 1
fi
archive="$target_dir/$target/release/libmssql_sqlcmd.a"
dest="$out_dir/runtimes/$rid/native"
mkdir -p "$dest"
cp -f "$archive" "$dest/libmssql_sqlcmd.a"
printf '%s\n' "$native_libs" > "$dest/native-static-libs.txt"

echo "Staged $rid ($target): $(wc -c < "$dest/libmssql_sqlcmd.a") bytes; native libs: $native_libs"
