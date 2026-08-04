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

.PARAMETER Version
  Precomputed version from the run's single compute step. When supplied it is
  stamped verbatim so every build job shares one version. When empty, the version
  is computed here as a fallback.

.PARAMETER ReleaseVersion
  'True' publishes the base version as-is (e.g. 1.0.0). Anything else appends a
  .dev<date><BuildId> segment. Ignored when -Version is supplied.

.PARAMETER BuildId
  Azure DevOps build id, used in the dev segment. Ignored when -Version is supplied.
#>
[CmdletBinding()]
param(
    [string]$Version = '',
    [string]$ReleaseVersion = 'False',
    [string]$BuildId = ''
)

$ErrorActionPreference = 'Stop'

$pyproject = 'mssql-mock-tds-py/pyproject.toml'
$cargo = 'mssql-mock-tds-py/Cargo.toml'

if (-not [string]::IsNullOrWhiteSpace($Version)) {
    $ver = $Version   # shared, computed once upstream
}
else {
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
}

Write-Host "Sandbox wheel version: $ver"

foreach ($f in $pyproject, $cargo) {
    (Get-Content $f -Raw) -replace '(?m)^(version\s*=\s*)"[^"]+"', "`$1`"$ver`"" |
        Set-Content $f -NoNewline
}

Write-Host "##vso[task.setvariable variable=mockWheelVersion]$ver"
