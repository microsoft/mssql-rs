#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Import only the initialization output, never a job-local fallback.
set +x
set -euo pipefail

if [[ ! "${SHARED_SQL_PASSWORD:-}" =~ ^Aa1![0-9a-f]{48}$ ]] \
    || [[ ! "${SQL_CREDENTIAL_GENERATION:-}" =~ ^[0-9a-f]{32}$ ]]; then
    echo "##vso[task.logissue type=error]Shared SQL credential output is missing or invalid. Queue a fresh validation run."
    exit 1
fi

echo "##vso[task.setvariable variable=SQL_PASSWORD;issecret=true]${SHARED_SQL_PASSWORD}"
echo "##vso[task.setvariable variable=SQL_PASSWORD_GENERATED]1"
