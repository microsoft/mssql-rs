#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Check out microsoft/mssql-python main or a pinned revision for cross-repo tests.
# MSSQL_PYTHON_CLONE_DIR sets the destination (default: ../mssql-python).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REVISION_FILE="$SCRIPT_DIR/../mssql-python-revision.txt"
CLONE_DIR="${MSSQL_PYTHON_CLONE_DIR:-../mssql-python}"

if [ ! -f "$REVISION_FILE" ]; then
  echo "##[error]Missing mssql-python revision: $REVISION_FILE" >&2
  exit 1
fi
REVISION="$(cat "$REVISION_FILE")"
if [[ "$REVISION" != "main" && ! "$REVISION" =~ ^[0-9a-f]{40}$ ]]; then
  echo "##[error]mssql-python revision must contain main or one full lowercase 40-character commit SHA: $REVISION_FILE" >&2
  exit 1
fi

echo "##[section]mssql-python requested revision: $REVISION"
FETCH_REF="$REVISION"
if [ "$REVISION" = "main" ]; then
  FETCH_REF="refs/heads/main"
fi
# Refuse to reuse a checkout or overwrite a developer's existing work.
mkdir "$CLONE_DIR"
git init --quiet "$CLONE_DIR"
git -C "$CLONE_DIR" remote add origin https://github.com/microsoft/mssql-python.git
if ! git -C "$CLONE_DIR" fetch --depth 1 origin "$FETCH_REF"; then
  echo "##[error]Cannot fetch mssql-python revision $REVISION; no fallback is allowed" >&2
  exit 1
fi
git -C "$CLONE_DIR" checkout --detach FETCH_HEAD
HEAD="$(git -C "$CLONE_DIR" rev-parse HEAD)"
echo "##[section]mssql-python HEAD: $HEAD"
if [ "$REVISION" != "main" ] && [ "$HEAD" != "$REVISION" ]; then
  echo "##[error]mssql-python HEAD does not match requested pin $REVISION" >&2
  exit 1
fi
