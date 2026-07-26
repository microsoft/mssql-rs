# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Stamps a prerelease version into the crate manifests.

.DESCRIPTION
  Replaces ONLY the [package].version of mssql-tds and mssql-mock-tds. The edit is
  scoped to the [package] table (header up to the next ^[table] or EOF), so it never
  touches table-style dependency versions such as [dependencies.uuid] version =
  "1.19.0" regardless of table ordering in the manifest.

  Emits the resolved version as the `crateVersion` pipeline variable.

.PARAMETER ReleaseVersion
  'True' publishes the base version as-is (e.g. 1.0.0). Anything else appends a
  -dev.<date>.<BuildId> segment.

.PARAMETER BuildId
  Azure DevOps build id, used in the dev segment.
#>
[CmdletBinding()]
param(
    [string]$ReleaseVersion = 'False',
    [Parameter(Mandatory = $true)][string]$BuildId
)

$ErrorActionPreference = 'Stop'

# Rewrites the version line inside the [package] table only. Isolating the section
# first (up to the next ^[ header or EOF) keeps the substitution independent of
# where [package] sits relative to other tables.
function Set-PackageVersion {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Version)
    $content = Get-Content $Path -Raw
    $section = [regex]'(?ms)^\[package\].*?(?=^\[|\z)'
    $m = $section.Match($content)
    if (-not $m.Success) { Write-Error "No [package] section in $Path"; exit 1 }
    $patched = [regex]::Replace($m.Value, '(?m)^(version\s*=\s*)"[^"]+"', "`${1}`"$Version`"", 1)
    $content = $content.Substring(0, $m.Index) + $patched + $content.Substring($m.Index + $m.Length)
    Set-Content $Path $content -NoNewline
    Write-Host "Stamped $Path -> version = `"$Version`""
}

$date = Get-Date -Format 'yyyyMMdd'
$baseVer = ([regex]'(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"').Match((Get-Content 'mssql-tds/Cargo.toml' -Raw)).Groups[1].Value
if ([string]::IsNullOrWhiteSpace($baseVer)) {
    Write-Error 'Could not read base version from mssql-tds/Cargo.toml'; exit 1
}

if ($ReleaseVersion -eq 'True') {
    $ver = $baseVer   # release: publish base version as-is (e.g. 1.0.0)
}
else {
    $ver = "$baseVer-dev.$date.$BuildId"
}

Write-Host "Sandbox crate version: $ver"
foreach ($f in 'mssql-tds/Cargo.toml', 'mssql-mock-tds/Cargo.toml') {
    Set-PackageVersion -Path $f -Version $ver
}

Write-Host "##vso[task.setvariable variable=crateVersion]$ver"
