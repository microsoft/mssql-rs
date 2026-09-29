# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Validates a Rust crate artifact before an ESRP release.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ArtifactDirectory
)

$ErrorActionPreference = 'Stop'
$manifestPath = Join-Path $ArtifactDirectory 'release-manifest.json'
if (-not (Test-Path $manifestPath -PathType Leaf)) {
    throw "Crate release manifest not found: $manifestPath"
}

$manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
if ($manifest.schemaVersion -ne 1) {
    throw "Unsupported crate release manifest schema: $($manifest.schemaVersion)"
}

$entries = @($manifest.crates)
$expectedNames = @('mssql-tds', 'mssql-mock-tds')
if ($entries.Count -ne $expectedNames.Count) {
    throw "Expected $($expectedNames.Count) crates, found $($entries.Count)"
}
if ($entries[0].version -ne $entries[1].version) {
    throw "Crate versions must match: $($entries[0].version) != $($entries[1].version)"
}

$artifactRoot = [System.IO.Path]::GetFullPath($ArtifactDirectory).TrimEnd(
    [System.IO.Path]::DirectorySeparatorChar,
    [System.IO.Path]::AltDirectorySeparatorChar
)
$expectedFiles = @()

for ($index = 0; $index -lt $expectedNames.Count; $index++) {
    $entry = $entries[$index]
    $expectedName = $expectedNames[$index]
    if ($entry.name -ne $expectedName) {
        throw "Expected crate $expectedName at release position $index, found $($entry.name)"
    }
    if ([string]::IsNullOrWhiteSpace($entry.version)) {
        throw "No version recorded for $expectedName"
    }

    $cratePath = [System.IO.Path]::GetFullPath(
        (Join-Path $artifactRoot ($entry.file -replace '/', [System.IO.Path]::DirectorySeparatorChar))
    )
    if (-not $cratePath.StartsWith("$artifactRoot$([System.IO.Path]::DirectorySeparatorChar)")) {
        throw "Crate path escapes the artifact directory: $($entry.file)"
    }
    if (-not (Test-Path $cratePath -PathType Leaf)) {
        throw "Crate file not found: $cratePath"
    }

    $expectedFileName = "$expectedName-$($entry.version).crate"
    if ([System.IO.Path]::GetFileName($cratePath) -ne $expectedFileName) {
        throw "Expected crate file $expectedFileName, found $([System.IO.Path]::GetFileName($cratePath))"
    }

    $actualHash = (Get-FileHash $cratePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $entry.sha256) {
        throw "SHA-256 mismatch for $expectedFileName"
    }
    $expectedFiles += $cratePath

    if ($expectedName -eq 'mssql-mock-tds') {
        $packagedManifestPath = "$expectedName-$($entry.version)/Cargo.toml"
        $packagedManifest = (& tar -xOf $cratePath $packagedManifestPath) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            throw "Could not read $packagedManifestPath from $expectedFileName"
        }

        $dependency = [regex]::Match(
            $packagedManifest,
            '(?ms)^\[dependencies\.mssql-tds\]\s*$(?<body>.*?)(?=^\[|\z)'
        )
        if (-not $dependency.Success) {
            throw "$expectedFileName has no mssql-tds dependency"
        }

        $dependencyVersion = [regex]::Match(
            $dependency.Groups['body'].Value,
            '(?m)^\s*version\s*=\s*"([^"]+)"'
        )
        if (-not $dependencyVersion.Success -or $dependencyVersion.Groups[1].Value -ne $entry.version) {
            $actualVersion = if ($dependencyVersion.Success) {
                $dependencyVersion.Groups[1].Value
            }
            else {
                '<missing>'
            }
            throw "$expectedFileName records mssql-tds dependency version '$actualVersion', expected '$($entry.version)'"
        }
        if ($dependency.Groups['body'].Value -match '(?m)^\s*(path|registry|registry-index)\s*=') {
            throw "$expectedFileName does not resolve mssql-tds from crates.io"
        }
    }

    $variableName = if ($expectedName -eq 'mssql-tds') {
        'mssqlTdsCrateVersion'
    }
    else {
        'mssqlMockTdsCrateVersion'
    }
    Write-Host "##vso[task.setvariable variable=$variableName]$($entry.version)"
}

$actualFiles = @(Get-ChildItem $artifactRoot -Recurse -Filter *.crate -File).FullName
if ($actualFiles.Count -ne $expectedFiles.Count) {
    throw "Expected $($expectedFiles.Count) .crate files, found $($actualFiles.Count)"
}
foreach ($actualFile in $actualFiles) {
    if ($actualFile -notin $expectedFiles) {
        throw "Unexpected crate file in release artifact: $actualFile"
    }
}

Write-Host 'Rust crate release artifact is valid.'
