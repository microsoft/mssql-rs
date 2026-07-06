# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Stamps a prerelease version into the crate manifests.

.DESCRIPTION
  Replaces ONLY the first ^version line (the [package].version) of mssql-tds and
  mssql-mock-tds. We do NOT reuse the shared stamp-crate-versions.ps1 because its
  global `^version = "..."` regex also rewrites table-style dependency versions
  such as [dependencies.uuid] version = "1.19.0", which corrupts the dependency
  requirement and makes cargo publish fail.

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

$date = Get-Date -Format 'yyyyMMdd'
$rx = [regex]'(?m)^(version\s*=\s*)"[^"]+"'
$baseVer = ([regex]'(?m)^version\s*=\s*"([^"]+)"').Match((Get-Content 'mssql-tds/Cargo.toml' -Raw)).Groups[1].Value
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
    $c = Get-Content $f -Raw
    $c = $rx.Replace($c, ('${1}"' + $ver + '"'), 1)
    Set-Content $f $c -NoNewline
    Write-Host "Stamped $f -> version = `"$ver`""
}

Write-Host "##vso[task.setvariable variable=crateVersion]$ver"
