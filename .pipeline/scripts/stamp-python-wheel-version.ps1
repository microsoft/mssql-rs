# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Stamps the run's version into the mssql-py-core wheel manifests (Windows jobs).

.DESCRIPTION
  maturin reads the wheel version from pyproject.toml's [project].version, so
  pyproject gets the PEP 440 spelling (0.1.0.dev123) while Cargo.toml gets the
  SemVer one (0.1.0-dev.123) that `cargo metadata` accepts. Linux/macOS jobs use
  stamp-python-wheel-version.sh.

.PARAMETER Version
  The version resolved once by compute-python-wheel-version.ps1. Empty is a no-op
  (the base version in the manifests is used unchanged).

.PARAMETER PyprojectPath
  Path to pyproject.toml (default mssql-py-core/pyproject.toml).

.PARAMETER CargoPath
  Path to Cargo.toml (default mssql-py-core/Cargo.toml).
#>
[CmdletBinding()]
param(
    [string]$Version = '',
    [string]$PyprojectPath = 'mssql-py-core/pyproject.toml',
    [string]$CargoPath = 'mssql-py-core/Cargo.toml'
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Version)) {
    Write-Host 'No version supplied; leaving manifests at their base version.'
    exit 0
}

# Cargo rejects PEP 440's `.devN` suffix ("unexpected character '.' after patch
# version number") because SemVer spells a prerelease with a hyphen.
$cargoVer = if ($Version -match '^(.*)\.dev(\d+)$') { "$($Matches[1])-dev.$($Matches[2])" } else { $Version }

Write-Host "Wheel version: $Version (Cargo manifest: $cargoVer)"

function Set-ManifestVersion {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Version)
    (Get-Content $Path -Raw) -replace '(?m)^(version\s*=\s*)"[^"]+"', "`$1`"$Version`"" |
        Set-Content $Path -NoNewline
}

Set-ManifestVersion -Path $PyprojectPath -Version $Version
Set-ManifestVersion -Path $CargoPath -Version $cargoVer
