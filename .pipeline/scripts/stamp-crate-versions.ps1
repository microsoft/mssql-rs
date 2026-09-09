# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Stamps publishable crate manifests with the version for the current build.

.DESCRIPTION
  Official builds retain the source version. Non-official scheduled builds use a
  nightly version, while other non-official builds use a unique dev version.
  mssql-mock-tds is kept aligned with mssql-tds, including its dependency version.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BuildReason,

    [Parameter(Mandatory = $true)]
    [string]$BuildId,

    [string]$IsOfficial = 'False',

    [string[]]$Crates = @('mssql-tds', 'mssql-mock-tds'),

    [switch]$WhatIf,

    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..'))
)

$ErrorActionPreference = 'Stop'

function Get-PackageVersion {
    param([Parameter(Mandatory = $true)][string]$Content, [Parameter(Mandatory = $true)][string]$Path)

    $packageSection = [regex]::Match($Content, '(?ms)^\[package\].*?(?=^\[|\z)')
    if (-not $packageSection.Success) {
        throw "No [package] section found in $Path"
    }

    $version = [regex]::Match($packageSection.Value, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $version.Success) {
        throw "No package version found in $Path"
    }

    return $version.Groups[1].Value
}

function Set-PackageVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Content,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $packageSection = [regex]::Match($Content, '(?ms)^\[package\].*?(?=^\[|\z)')
    if (-not $packageSection.Success) {
        throw "No [package] section found in $Path"
    }

    $versionMatches = [regex]::Matches($packageSection.Value, '(?m)^version\s*=\s*"[^"]+"')
    if ($versionMatches.Count -ne 1) {
        throw "Expected one package version in ${Path}, found $($versionMatches.Count)"
    }

    $updatedSection = [regex]::Replace(
        $packageSection.Value,
        '(?m)^(version\s*=\s*)"[^"]+"',
        "`${1}`"$Version`"",
        1
    )

    return $Content.Substring(0, $packageSection.Index) +
        $updatedSection +
        $Content.Substring($packageSection.Index + $packageSection.Length)
}

function Set-MssqlTdsDependencyVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Content,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $pattern = [regex]'(?m)^(?<prefix>\s*mssql-tds\s*=\s*\{[^\r\n}]*?\bversion\s*=\s*)"[^"]+"(?<suffix>[^\r\n}]*\}\s*)$'
    $depMatches = $pattern.Matches($Content)
    if ($depMatches.Count -ne 1) {
        throw "Expected one versioned mssql-tds dependency in ${Path}, found $($depMatches.Count)"
    }

    $match = $depMatches[0]
    $replacement = $match.Groups['prefix'].Value + "`"$Version`"" + $match.Groups['suffix'].Value
    return $Content.Substring(0, $match.Index) +
        $replacement +
        $Content.Substring($match.Index + $match.Length)
}

$isOfficialBuild = $false
if (-not [bool]::TryParse($IsOfficial, [ref]$isOfficialBuild)) {
    throw "IsOfficial must be True or False, got '$IsOfficial'"
}

$cargoTomlPath = Join-Path $RepositoryRoot "$($Crates[0])/Cargo.toml"
$baseVersion = Get-PackageVersion -Content (Get-Content $cargoTomlPath -Raw) -Path $cargoTomlPath
if ($baseVersion -match '-(?:dev|nightly)\.\d{8}(?:\.|$)') {
    throw "Source manifest is already stamped with build version $baseVersion"
}
$dateStamp = Get-Date -Format 'yyyyMMdd'

if ($isOfficialBuild) {
    $crateVersion = $baseVersion
    Write-Host 'Official build detected'
}
elseif ($BuildReason -eq 'Schedule') {
    $crateVersion = "$baseVersion-nightly.$dateStamp"
    Write-Host 'Non-official nightly build detected'
}
else {
    $crateVersion = "$baseVersion-dev.$dateStamp.$BuildId"
    Write-Host "Non-official build detected (reason: $BuildReason)"
}

Write-Host "Base version:  $baseVersion"
Write-Host "Crate version: $crateVersion"

foreach ($crate in $Crates) {
    $path = Join-Path $RepositoryRoot "$crate/Cargo.toml"
    $content = Get-Content $path -Raw
    $updated = Set-PackageVersion -Content $content -Version $crateVersion -Path $path
    if ($crate -eq 'mssql-mock-tds') {
        $updated = Set-MssqlTdsDependencyVersion -Content $updated -Version $crateVersion -Path $path
    }

    if ($WhatIf) {
        Write-Host "[WhatIf] Would patch $path -> version = `"$crateVersion`""
    }
    elseif ($content -ne $updated) {
        Set-Content $path $updated -NoNewline
        Write-Host "Patched $path -> version = `"$crateVersion`""
    }
    else {
        Write-Host "$path already uses version `"$crateVersion`""
    }
}

if (-not $WhatIf) {
    Write-Host "##vso[task.setvariable variable=crateVersion]$crateVersion"
    Write-Host "##vso[task.setvariable variable=crateVersion;isOutput=true]$crateVersion"
}

Write-Host "Done. Crate version: $crateVersion"
