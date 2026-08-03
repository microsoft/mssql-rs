# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Computes the mock wheel version ONCE for the run.

.DESCRIPTION
  Reads the base version from mssql-mock-tds-py/pyproject.toml and resolves the
  version the whole run should stamp, then emits it as the `mockWheelVersion`
  pipeline OUTPUT variable. Every build job maps this one value and hands it to the
  per-job stamper, so the seven parallel wheels always share an identical version
  even when a run straddles UTC midnight (the dev date is computed here, once).

  Dev date uses UTC to match the sandbox convention.

.PARAMETER ReleaseVersion
  'True' publishes the base version as-is (e.g. 1.0.0). Anything else appends a
  .dev<date><BuildId> segment.

.PARAMETER BuildId
  Azure DevOps build id, used in the dev segment.
#>
[CmdletBinding()]
param(
    [string]$ReleaseVersion = 'False',
    [Parameter(Mandatory = $true)][string]$BuildId
)

$ErrorActionPreference = 'Stop'

$pyproject = 'mssql-mock-tds-py/pyproject.toml'
$py = Get-Content $pyproject -Raw
if ($py -match '(?m)^version\s*=\s*"([^"]+)"') { $base = $Matches[1] }
else { Write-Error "Could not read version from $pyproject"; exit 1 }

if ($ReleaseVersion -eq 'True') {
    $ver = $base   # release: publish base version as-is (e.g. 1.0.0)
}
else {
    $dev = "$([DateTime]::UtcNow.ToString('yyyyMMdd'))$BuildId"
    $ver = "$base.dev$dev"   # PEP 440 dev release segment (.devN)
}

Write-Host "Resolved sandbox wheel version (computed once): $ver"
Write-Host "##vso[task.setvariable variable=mockWheelVersion;isOutput=true]$ver"
