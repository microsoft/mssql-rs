#!/usr/bin/env bash
# derive-sql-password.sh — produce a per-build SQL Server SA password that is
# identical in the SQL host job and the dependent test jobs without any
# cross-job secret transport.
#
# The cross-pool design boots SQL Server on an x64 1ES agent and runs the
# tests on a separate ARM 1ES agent. They cannot share a job-scoped random
# password, and shipping a generated password through pipeline artifacts
# would expose it to anyone with read access to the build. Instead, both
# sides derive the same password from build context: BUILD_BUILDID +
# SYSTEM_COLLECTIONID + a static salt. The result is unique per build,
# unpredictable to outsiders (CollectionId is not public), and requires no
# Key Vault round-trip.
#
# Required env:
#   BUILD_BUILDID         Current build id (System.BuildId).
#   SYSTEM_COLLECTIONID   ADO collection (organization) GUID.
#
# Sets pipeline variables (idempotent within a job; safe to source twice):
#   SQL_PASSWORD            secret-masked SA password
#   SQL_PASSWORD_GENERATED  marker so generate-sql-password-template.yml
#                           skips re-generation if also pulled in.

set -euo pipefail

if [ "${SQL_PASSWORD_GENERATED:-}" = "1" ]; then
    echo "SQL_PASSWORD already established for this job; skipping derivation"
    exit 0
fi

: "${BUILD_BUILDID:?BUILD_BUILDID is required}"
: "${SYSTEM_COLLECTIONID:?SYSTEM_COLLECTIONID is required}"

# Static salt distinguishes this password from any other build-id-derived
# secret that might exist in the same project, and forces an attacker who
# knows the public BuildId to also know the project layout.
SALT="mssql-tds-arm-cross-pool-sa"

digest="$(printf '%s' "${BUILD_BUILDID}-${SYSTEM_COLLECTIONID}-${SALT}" \
            | sha256sum | awk '{print $1}')"

# 22 hex chars + 4-char policy-class prefix (upper, lower, digit, symbol)
# = 26 chars, well above the 8-char SQL Server SA minimum and includes all
# four character classes the policy requires.
SQL_PASSWORD="Aa1!${digest:0:22}"

echo "##vso[task.setvariable variable=SQL_PASSWORD;issecret=true]${SQL_PASSWORD}"
echo "##vso[task.setvariable variable=SQL_PASSWORD_GENERATED]1"
echo "Derived SQL_PASSWORD (length=${#SQL_PASSWORD}) for build ${BUILD_BUILDID}"
