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
  *-apple-darwin*) export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-10.15}" ;;
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

# On Linux the library loads OpenSSL itself when it is first used
# (mssql-sqlcmd/src/openssl_runtime.rs), so consumers must not link libssl or
# libcrypto: linked, sqlcmd would need the OpenSSL major version of this build
# machine. dlopen is in libdl before glibc 2.34 (libc on musl and later glibc).
case "$target" in
  *-linux-*)
    native_libs="$(printf '%s\n' "$native_libs" | tr ' ' '\n' |
      grep -v -x -e '-lssl' -e '-lcrypto' | tr '\n' ' ' | sed 's/ *$//')"
    case "$target" in
      *-gnu*) case " $native_libs " in *" -ldl "*) ;; *) native_libs="$native_libs -ldl" ;; esac ;;
    esac
    ;;
esac

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

# Prove the staged archive links the way native sqlcmd links it: a program that
# uses every C ABI function, against exactly native-static-libs.txt. On Linux it
# must also need no libssl or libcrypto, so every OpenSSL function the archive
# calls is one openssl_runtime defines.
case "$target" in
  *-linux-*)
    check="$(mktemp -d "${TMPDIR:-/tmp}/mssql-sqlcmd-link.XXXXXX")"
    trap 'rm -f "$log"; rm -rf "$check"' EXIT
    if ! api="$(nm -g --defined-only "$dest/libmssql_sqlcmd.a")"; then
      echo "ERROR: nm could not read the symbols of $dest/libmssql_sqlcmd.a." >&2
      exit 1
    fi
    api="$(printf '%s\n' "$api" | awk '$2 == "T" && $3 ~ /^mssql_sqlcmd_/ { print $3 }' | sort -u)"
    if [ -z "$api" ]; then
      echo "ERROR: $dest/libmssql_sqlcmd.a defines no mssql_sqlcmd_* functions." >&2
      exit 1
    fi
    {
      printf '%s\n' "$api" | sed 's/.*/extern void &(void);/'
      echo 'void *volatile mssql_sqlcmd_link_check[] = {'
      printf '%s\n' "$api" | sed 's/.*/  (void *)\&&,/'
      echo '};'
      echo 'int main(void) { return mssql_sqlcmd_link_check[0] == 0; }'
    } > "$check/check.c"
    # shellcheck disable=SC2086 # the library list is word-split on purpose
    if ! "${CC:-cc}" -o "$check/check" "$check/check.c" "$dest/libmssql_sqlcmd.a" $native_libs; then
      echo "ERROR: $rid does not link with only its native-static-libs ($native_libs)." >&2
      echo "       An undefined OpenSSL function must be added to mssql-sqlcmd/src/openssl_runtime.rs." >&2
      exit 1
    fi
    if ! deps="$(readelf -d "$check/check")"; then
      echo "ERROR: readelf could not read the dependencies of the $rid link check." >&2
      exit 1
    fi
    if printf '%s\n' "$deps" | grep -E 'NEEDED.*lib(ssl|crypto)\.so' >&2; then
      echo "ERROR: $rid links OpenSSL; it must load it at run time." >&2
      exit 1
    fi
    ;;
esac

echo "Staged $rid ($target): $(wc -c < "$dest/libmssql_sqlcmd.a") bytes; native libs: $native_libs"
