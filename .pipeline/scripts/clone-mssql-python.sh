#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Clone microsoft/mssql-python for the cross-repo validation jobs.
#
# Branch selection defaults to main. A PR can retarget the clone by adding
#   mssql-python-branch: feature/my-branch
# to its description; the description is read back through the Azure DevOps CLI.
# An unknown branch falls back to main so the job still produces signal instead
# of failing on a typo.
#
# Env:
#   MSSQL_PYTHON_CLONE_DIR   Clone destination (default: ../mssql-python).
#   AZURE_DEVOPS_EXT_PAT     Token consumed by `az repos pr show`.

set -euo pipefail

CLONE_DIR="${MSSQL_PYTHON_CLONE_DIR:-../mssql-python}"
MSSQL_PYTHON_BRANCH="main"

if [ -n "${SYSTEM_PULLREQUEST_PULLREQUESTID:-}" ]; then
  echo "PR build detected (PR #${SYSTEM_PULLREQUEST_PULLREQUESTID})"

  echo "Installing azure-devops CLI extension..."
  az extension add --name azure-devops --yes --allow-preview true 2>/dev/null || echo "Extension may already be installed"

  az devops configure --defaults organization="${SYSTEM_COLLECTIONURI}" project="${SYSTEM_TEAMPROJECT}"

  PR_DESC=$(az repos pr show --id "${SYSTEM_PULLREQUEST_PULLREQUESTID}" --query description -o tsv 2>&1) || {
    echo "##[warning]Failed to get PR description: $PR_DESC"
    PR_DESC=""
  }

  if [ -n "$PR_DESC" ]; then
    echo "PR description found (length: ${#PR_DESC} chars)"
    # Match word chars, slashes, hyphens, dots only (no HTML tags or special chars)
    BRANCH_OVERRIDE=$(echo "$PR_DESC" | grep -oP 'mssql-python-branch:\s*\K[a-zA-Z0-9_/.-]+' | head -1 | tr -d '[:space:]' || true)
    if [ -n "$BRANCH_OVERRIDE" ]; then
      MSSQL_PYTHON_BRANCH="$BRANCH_OVERRIDE"
      echo "##[section]Using mssql-python branch override: $MSSQL_PYTHON_BRANCH"
    else
      echo "No branch override found in PR description"
    fi
  else
    echo "PR description is empty or could not be retrieved"
  fi
else
  echo "Not a PR build (SYSTEM_PULLREQUEST_PULLREQUESTID not set)"
fi

echo "Cloning microsoft/mssql-python (branch: $MSSQL_PYTHON_BRANCH) into $CLONE_DIR..."
if ! git clone --depth 1 -b "$MSSQL_PYTHON_BRANCH" https://github.com/microsoft/mssql-python.git "$CLONE_DIR" 2>/dev/null; then
  echo "##[warning]Branch '$MSSQL_PYTHON_BRANCH' not found, falling back to main"
  git clone --depth 1 -b main https://github.com/microsoft/mssql-python.git "$CLONE_DIR"
fi
