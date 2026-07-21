# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Pins the mssql-tds path dependency to a version.

.DESCRIPTION
  The mssql-tds path dependency in mssql-mock-tds/Cargo.toml has no version, which
  cargo publish rejects ("all dependencies must have a version"). This adds the
  stamped version while keeping the path (cargo uses the version on publish and the
  path for local verify builds).

.PARAMETER Version
  The stamped crate version to pin (typically $(crateVersion)).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Version)) { Write-Error 'Version not set'; exit 1 }

$path = 'mssql-mock-tds/Cargo.toml'
$c = Get-Content $path -Raw
$c = $c -replace 'mssql-tds\s*=\s*\{\s*path\s*=\s*"\.\./mssql-tds"',
                 "mssql-tds = { path = `"../mssql-tds`", version = `"$Version`""
Set-Content $path $c -NoNewline

Write-Host "Pinned mssql-tds dependency to version $Version"
Select-String -Path $path -Pattern 'mssql-tds\s*=' | ForEach-Object { Write-Host $_.Line }
