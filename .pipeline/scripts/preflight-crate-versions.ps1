# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Preflight guard for a release crate publish.

.DESCRIPTION
  Reads the base [package].version from mssql-tds/Cargo.toml and delegates to
  check-crate-version-not-published.ps1 so a release run fails in seconds if the
  version is already on the feed, instead of at cargo publish time.

.PARAMETER IndexBaseUrl
  Cargo sparse index base URL (typically $(cargoSparseIndex)).

.PARAMETER Crates
  Crate names to check (e.g. mssql-tds, mssql-mock-tds).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$IndexBaseUrl,
    [Parameter(Mandatory = $true)][string[]]$Crates
)

$ErrorActionPreference = 'Stop'

$baseVer = ([regex]'(?m)^version\s*=\s*"([^"]+)"').Match((Get-Content 'mssql-tds/Cargo.toml' -Raw)).Groups[1].Value
if ([string]::IsNullOrWhiteSpace($baseVer)) {
    Write-Error 'Could not read base version from mssql-tds/Cargo.toml'; exit 1
}

& "$PSScriptRoot/check-crate-version-not-published.ps1" `
    -IndexBaseUrl $IndexBaseUrl -Version $baseVer -Crates $Crates
