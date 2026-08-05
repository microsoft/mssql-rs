# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# New-SqlPassword.ps1 — Windows counterpart of derive-sql-password.sh.
#
# Must produce byte-identical output to the bash version: the SQL host job runs
# on a Linux x64 agent and derives the SA password there, while this script runs
# on the Windows agent that connects to it. Any divergence in salt, digest
# encoding, or truncation silently turns into login failure 18456.
#
# Required env:
#   BUILD_BUILDID         Current build id (System.BuildId).
#   SYSTEM_COLLECTIONID   ADO collection (organization) GUID.
#
# Sets pipeline variables:
#   SQL_PASSWORD            secret-masked SA password
#   SQL_PASSWORD_GENERATED  marker so generate-sql-password-template.yml skips
#                           re-generation if also pulled into the same job.

$ErrorActionPreference = 'Stop'

if ($env:SQL_PASSWORD_GENERATED -eq '1') {
    Write-Host 'SQL_PASSWORD already established for this job; skipping derivation'
    exit 0
}

if (-not $env:BUILD_BUILDID) { throw 'BUILD_BUILDID is required' }
if (-not $env:SYSTEM_COLLECTIONID) { throw 'SYSTEM_COLLECTIONID is required' }

# Keep in sync with derive-sql-password.sh.
$salt = 'mssql-tds-arm-cross-pool-sa'
$material = "$($env:BUILD_BUILDID)-$($env:SYSTEM_COLLECTIONID)-$salt"

$sha256 = [System.Security.Cryptography.SHA256]::Create()
try {
    $bytes = $sha256.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($material))
} finally {
    $sha256.Dispose()
}

# sha256sum emits lowercase hex; -f 'x2' matches it.
$digest = -join ($bytes | ForEach-Object { $_.ToString('x2') })
$password = 'Aa1!' + $digest.Substring(0, 22)

Write-Host "##vso[task.setvariable variable=SQL_PASSWORD;issecret=true]$password"
Write-Host '##vso[task.setvariable variable=SQL_PASSWORD_GENERATED]1'
Write-Host "Derived SQL_PASSWORD (length=$($password.Length)) for build $($env:BUILD_BUILDID)"
