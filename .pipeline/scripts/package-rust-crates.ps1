# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Creates the Rust crate artifacts consumed by the official release pipeline.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$TargetDirectory = (Join-Path ([System.IO.Path]::GetTempPath()) 'mssql-rs-crate-target'),
    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..'))
)

$ErrorActionPreference = 'Stop'
$crateNames = @('mssql-tds', 'mssql-mock-tds')

if (Test-Path $OutputDirectory) {
    $existing = @(Get-ChildItem $OutputDirectory -Force)
    if ($existing.Count -ne 0) {
        throw "Crate output directory is not empty: $OutputDirectory"
    }
}
else {
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
}

New-Item -ItemType Directory -Path $TargetDirectory -Force | Out-Null
$manifestEntries = @()

Push-Location $RepositoryRoot
try {
    foreach ($crateName in $crateNames) {
        $arguments = @(
            'package'
            '--package', $crateName
            '--allow-dirty'
            '--exclude-lockfile'
            '--target-dir', $TargetDirectory
        )
        if ($crateName -eq 'mssql-mock-tds') {
            # Verification requires its mssql-tds version to exist on crates.io.
            $arguments += '--no-verify'
        }

        & cargo @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "cargo package failed for $crateName with exit code $LASTEXITCODE"
        }

        $fileName = "$crateName-$Version.crate"
        $source = Join-Path $TargetDirectory "package/$fileName"
        if (-not (Test-Path $source -PathType Leaf)) {
            throw "cargo package did not create the expected file: $source"
        }

        $crateDirectory = Join-Path $OutputDirectory $crateName
        New-Item -ItemType Directory -Path $crateDirectory -Force | Out-Null
        $destination = Join-Path $crateDirectory $fileName
        Copy-Item $source $destination

        $manifestEntries += [ordered]@{
            name = $crateName
            version = $Version
            file = "$crateName/$fileName"
            sha256 = (Get-FileHash $destination -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
}
finally {
    Pop-Location
}

$releaseManifest = [ordered]@{
    schemaVersion = 1
    crates = $manifestEntries
}
$manifestPath = Join-Path $OutputDirectory 'release-manifest.json'
$releaseManifest | ConvertTo-Json -Depth 4 | Set-Content $manifestPath

Write-Host 'Created Rust crate artifacts:'
Get-ChildItem $OutputDirectory -Recurse -File | ForEach-Object {
    Write-Host "  $($_.FullName)"
}
