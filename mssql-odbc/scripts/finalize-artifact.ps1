# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

param(
    [ValidateSet("debug", "release")]
    [string]$BuildProfile = "debug"
)

$ErrorActionPreference = "Stop"
$OdbcCrateDir = Split-Path -Parent $PSScriptRoot

Push-Location $OdbcCrateDir
try {
    try {
        $Metadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
    } catch {
        throw "could not resolve Cargo target directory (is cargo on PATH?): $_"
    }
    if (-not $Metadata.target_directory) {
        throw "cargo metadata did not return a target directory"
    }
} finally {
    Pop-Location
}

# On Windows the Cargo cdylib output has no `lib` prefix, so it already carries
# the shipped basename `mssqlodbc.dll`. No copy is needed; just resolve it.
$ArtifactPath = Join-Path $Metadata.target_directory "$BuildProfile\mssqlodbc.dll"
if (-not (Test-Path $ArtifactPath -PathType Leaf)) {
    throw "Cargo artifact not found at $ArtifactPath"
}

(Resolve-Path $ArtifactPath).Path
