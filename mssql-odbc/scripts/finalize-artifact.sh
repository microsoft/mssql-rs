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
TARGET_DIR="$(cd "$ODBC_CRATE_DIR" \
    && cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | grep -o '"target_directory":"[^"]*"' | head -n1 \
    | sed 's/^"target_directory":"//; s/"$//')"

case "$(uname -s)" in
    Darwin)
        CARGO_ARTIFACT="libmssqlodbc.dylib"
        PRODUCT_ARTIFACT="mssql-odbc.dylib"
        ;;
    Linux)
        CARGO_ARTIFACT="libmssqlodbc.so"
        PRODUCT_ARTIFACT="mssql-odbc.so"
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