#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

PROFILE="${1:-debug}"
case "$PROFILE" in
    debug|release) ;;
    *) echo "Usage: $0 [debug|release]" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
if ! METADATA="$(cd "$ODBC_CRATE_DIR" && cargo metadata --format-version 1 --no-deps)"; then
    echo "Error: could not resolve Cargo target directory (is cargo on PATH?)" >&2
    exit 1
fi
if ! TARGET_DIR="$(printf '%s\n' "$METADATA" \
    | grep -o '"target_directory":"[^"]*"' | head -n1 \
    | sed 's/^"target_directory":"//; s/"$//')" || [ -z "$TARGET_DIR" ]; then
    echo "Error: cargo metadata did not return a target directory" >&2
    exit 1
fi

case "$(uname -s)" in
    Darwin)
        CARGO_ARTIFACT="libmssqlodbc.dylib"
        PRODUCT_ARTIFACT="mssqlodbc.dylib"
        ;;
    Linux)
        CARGO_ARTIFACT="libmssqlodbc.so"
        PRODUCT_ARTIFACT="mssqlodbc.so"
        ;;
    *)
        echo "Error: use finalize-artifact.ps1 on Windows" >&2
        exit 1
        ;;
esac

SOURCE_PATH="$TARGET_DIR/$PROFILE/$CARGO_ARTIFACT"
PRODUCT_PATH="$TARGET_DIR/$PROFILE/$PRODUCT_ARTIFACT"
if [ ! -f "$SOURCE_PATH" ]; then
    echo "Error: Cargo artifact not found at $SOURCE_PATH" >&2
    exit 1
fi

cp -f "$SOURCE_PATH" "$PRODUCT_PATH"
printf '%s\n' "$PRODUCT_PATH"
