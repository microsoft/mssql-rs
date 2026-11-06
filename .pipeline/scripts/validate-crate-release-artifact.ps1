# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Validates a Rust crate artifact before an ESRP release.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ArtifactDirectory,
    [ValidateSet('True', 'False')][string]$RequireMssqlRs = 'False'
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
if ($entries.Count -eq 3) {
    $expectedNames += 'mssql-rs'
}
elseif ($RequireMssqlRs -eq 'True') {
    throw 'The selected build artifact must contain mssql-rs.'
}
if ($entries.Count -ne $expectedNames.Count) {
    throw "Expected $($expectedNames.Count) crates, found $($entries.Count)"
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
    if ($entry.version -ne $entries[0].version) {
        throw "Crate versions must match: $($entries[0].version) != $($entry.version)"
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

    if ($expectedName -ne 'mssql-tds') {
        $packagedManifestPath = "$expectedName-$($entry.version)/Cargo.toml"
        $packagedManifest = (& tar -xOf $cratePath $packagedManifestPath) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            throw "Could not read $packagedManifestPath from $expectedFileName"
        }

        $dependencyKinds = @('dependencies')
        if ($expectedName -eq 'mssql-rs') {
            $dependencyKinds += 'dev-dependencies'
        }
        foreach ($kind in $dependencyKinds) {
            $pattern = '(?ms)^\[{0}\.mssql-tds\]\s*$(?<body>.*?)(?=^\[|\z)' -f $kind
            $dependency = [regex]::Match($packagedManifest, $pattern)
            if (-not $dependency.Success) {
                throw "$expectedFileName has no mssql-tds $kind entry"
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
                throw "$expectedFileName records mssql-tds $kind version '$actualVersion', expected '$($entry.version)'"
            }
            if ($dependency.Groups['body'].Value -match '(?m)^\s*(path|registry|registry-index)\s*=') {
                throw "$expectedFileName does not resolve mssql-tds from crates.io"
            }
        }
    }

    $variableName = switch ($expectedName) {
        'mssql-tds' { 'mssqlTdsCrateVersion' }
        'mssql-mock-tds' { 'mssqlMockTdsCrateVersion' }
        'mssql-rs' { 'mssqlRsCrateVersion' }
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
