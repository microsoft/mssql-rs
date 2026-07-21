# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Stamps a PEP 440 version into the mock wheel manifests.

.DESCRIPTION
  Writes the same version into mssql-mock-tds-py/pyproject.toml and
  mssql-mock-tds-py/Cargo.toml so maturin bakes it into the wheel. Used by the
  Windows build jobs (the Linux/macOS jobs use stamp-mock-wheel-version.sh).

  Emits the resolved version as the `mockWheelVersion` pipeline variable.

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
$cargo = 'mssql-mock-tds-py/Cargo.toml'

$py = Get-Content $pyproject -Raw
if ($py -match '(?m)^version\s*=\s*"([^"]+)"') { $base = $Matches[1] }
else { Write-Error "Could not read version from $pyproject"; exit 1 }

if ($ReleaseVersion -eq 'True') {
    $ver = $base   # release: publish base version as-is (e.g. 1.0.0)
}
else {
    $dev = "$(Get-Date -Format 'yyyyMMdd')$BuildId"
    $ver = "$base.dev$dev"   # PEP 440 dev release segment (.devN) for the wheel
}

Write-Host "Sandbox wheel version: $ver"

foreach ($f in $pyproject, $cargo) {
    (Get-Content $f -Raw) -replace '(?m)^(version\s*=\s*)"[^"]+"', "`$1`"$ver`"" |
        Set-Content $f -NoNewline
}

Write-Host "##vso[task.setvariable variable=mockWheelVersion]$ver"
