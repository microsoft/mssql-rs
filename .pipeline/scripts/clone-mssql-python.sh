#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Check out the approved microsoft/mssql-python revision for cross-repo tests.
# MSSQL_PYTHON_CLONE_DIR sets the destination (default: ../mssql-python).
# PR and local runs use the SHA pin; non-PR pipeline runs follow main.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PIN_FILE="$SCRIPT_DIR/../mssql-python-revision.txt"
CLONE_DIR="${MSSQL_PYTHON_CLONE_DIR:-../mssql-python}"

REVISION="main"
if [ "${BUILD_REASON:-PullRequest}" = "PullRequest" ]; then
  if [ ! -f "$PIN_FILE" ]; then
    echo "##[error]Missing mssql-python pin: $PIN_FILE" >&2
    exit 1
  fi
  REVISION="$(cat "$PIN_FILE")"
  if [[ ! "$REVISION" =~ ^[0-9a-f]{40}$ ]]; then
    echo "##[error]mssql-python pin must contain one full lowercase 40-character commit SHA: $PIN_FILE" >&2
    exit 1
  fi
fi

echo "##[section]mssql-python requested pin: $REVISION"
FETCH_REF="$REVISION"
if [[ ! "$REVISION" =~ ^[0-9a-f]{40}$ ]]; then
  FETCH_REF="refs/heads/$REVISION"
fi
# Refuse to reuse a checkout or overwrite a developer's existing work.
mkdir "$CLONE_DIR"
git init --quiet "$CLONE_DIR"
git -C "$CLONE_DIR" remote add origin https://github.com/microsoft/mssql-python.git
if ! git -C "$CLONE_DIR" fetch --depth 1 origin "$FETCH_REF"; then
  echo "##[error]Cannot fetch mssql-python pin $REVISION; no branch fallback is allowed" >&2
  exit 1
fi
git -C "$CLONE_DIR" checkout --detach FETCH_HEAD
HEAD="$(git -C "$CLONE_DIR" rev-parse HEAD)"
echo "##[section]mssql-python HEAD: $HEAD"
if [[ "$REVISION" =~ ^[0-9a-f]{40}$ ]] && [ "$HEAD" != "$REVISION" ]; then
  echo "##[error]mssql-python HEAD does not match requested pin $REVISION" >&2
  exit 1
fi
