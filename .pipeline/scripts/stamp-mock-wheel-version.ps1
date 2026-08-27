# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Stamps the run's version into the mock wheel manifests.

.DESCRIPTION
  Writes the run's version into mssql-mock-tds-py/pyproject.toml and
  mssql-mock-tds-py/Cargo.toml. maturin reads the wheel version from pyproject.toml's
  [project].version, so pyproject gets the PEP 440 spelling (0.1.0.dev123) while
  Cargo.toml gets the SemVer one (0.1.0-dev.123) that `cargo metadata` will accept.
  Used by the Windows build jobs (the Linux/macOS jobs use stamp-mock-wheel-version.sh).

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

# Cargo rejects PEP 440's `.devN` suffix ("unexpected character '.' after patch
# version number") because SemVer spells a prerelease with a hyphen. Release
# versions carry no dev segment and are already valid SemVer.
$cargoVer = if ($ver -match '^(.*)\.dev(\d+)$') { "$($Matches[1])-dev.$($Matches[2])" } else { $ver }

Write-Host "Sandbox wheel version: $ver (Cargo manifest: $cargoVer)"

function Set-ManifestVersion {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Version)
    (Get-Content $Path -Raw) -replace '(?m)^(version\s*=\s*)"[^"]+"', "`$1`"$Version`"" |
        Set-Content $Path -NoNewline
}

Set-ManifestVersion -Path $pyproject -Version $ver
Set-ManifestVersion -Path $cargo -Version $cargoVer

Write-Host "##vso[task.setvariable variable=mockWheelVersion]$ver"
