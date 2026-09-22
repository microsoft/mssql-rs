#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Check out the approved microsoft/mssql-python revision for cross-repo tests.
# MSSQL_PYTHON_CLONE_DIR sets the destination (default: ../mssql-python).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PIN_FILE="$SCRIPT_DIR/../mssql-python-revision.txt"
CLONE_DIR="${MSSQL_PYTHON_CLONE_DIR:-../mssql-python}"

if [ ! -f "$PIN_FILE" ]; then
  echo "##[error]Missing mssql-python pin: $PIN_FILE" >&2
  exit 1
fi
REVISION="$(cat "$PIN_FILE")"
if [[ ! "$REVISION" =~ ^[0-9a-f]{40}$ ]]; then
  echo "##[error]mssql-python pin must contain one full lowercase 40-character commit SHA: $PIN_FILE" >&2
  exit 1
fi

echo "##[section]mssql-python requested pin: $REVISION"
# Refuse to reuse a checkout or overwrite a developer's existing work.
mkdir "$CLONE_DIR"
git init --quiet "$CLONE_DIR"
git -C "$CLONE_DIR" remote add origin https://github.com/microsoft/mssql-python.git
if ! git -C "$CLONE_DIR" fetch --depth 1 origin "$REVISION"; then
  echo "##[error]Cannot fetch mssql-python pin $REVISION; no branch fallback is allowed" >&2
  exit 1
fi
git -C "$CLONE_DIR" checkout --detach "$REVISION"
HEAD="$(git -C "$CLONE_DIR" rev-parse HEAD)"
echo "##[section]mssql-python HEAD: $HEAD"
if [ "$HEAD" != "$REVISION" ]; then
  echo "##[error]mssql-python HEAD does not match requested pin $REVISION" >&2
  exit 1
fi
